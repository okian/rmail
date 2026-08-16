//! Driven against a real HTTP server on loopback, the way
//! `crate::ai::provider`'s tests are: the request asserted on is the one
//! `reqwest` would really send, and the token endpoint's replies are bytes off
//! a socket rather than a mocked client. Nothing here touches the network.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::ErrorReason;

// ---------------------------------------------------------------------------
// A mock token endpoint
// ---------------------------------------------------------------------------

/// One request, as the token endpoint saw it: the decoded form body.
#[derive(Debug, Clone, Default)]
struct SeenRequest {
    form: Vec<(String, String)>,
}

impl SeenRequest {
    fn get(&self, key: &str) -> Option<&str> {
        self.form
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

/// A token endpoint that answers from a queue of canned replies (repeating the
/// last once exhausted) and records every form it was posted.
struct TokenServer {
    endpoint: String,
    seen: Arc<Mutex<Vec<SeenRequest>>>,
    hits: Arc<AtomicUsize>,
    /// Most requests the server ever had open at the same moment. The direct
    /// observation of whether two callers were serialized, which a wall-clock
    /// threshold only approximates.
    peak: Arc<AtomicUsize>,
    in_flight: Arc<AtomicUsize>,
    task: tokio::task::JoinHandle<()>,
}

/// How the server picks its answer.
///
/// A queue is enough when one caller is in play. It is *not* enough when two
/// different grant types race, because then the reply a caller gets depends on
/// the order the requests happened to arrive in — which is exactly the thing
/// under test. `ByGrantType` decides after parsing the form, so the answer is
/// a property of the request rather than of the interleaving.
enum Replies {
    Queue(Mutex<VecDeque<(u16, String)>>),
    ByGrantType(HashMap<String, (u16, String, Duration)>),
}

impl Replies {
    /// The reply for one request, and how long to sit on it.
    fn pick(&self, grant_type: Option<&str>, default_delay: Duration) -> (u16, String, Duration) {
        match self {
            Self::Queue(queue) => {
                let mut queue = queue
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let fallback = (500, String::new());
                let (status, body) = if queue.len() > 1 {
                    queue.pop_front().unwrap_or(fallback)
                } else {
                    queue.front().cloned().unwrap_or(fallback)
                };
                (status, body, default_delay)
            }
            Self::ByGrantType(routes) => grant_type
                .and_then(|g| routes.get(g))
                .cloned()
                .unwrap_or((500, String::new(), Duration::ZERO)),
        }
    }
}

impl Drop for TokenServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl TokenServer {
    async fn always(status: u16, body: impl Into<String>) -> Self {
        Self::queued(vec![(status, body.into())], Duration::ZERO).await
    }

    async fn queued(replies: Vec<(u16, String)>, delay: Duration) -> Self {
        Self::serving(Replies::Queue(Mutex::new(VecDeque::from(replies))), delay).await
    }

    /// Answer by `grant_type`, each route with its own delay.
    async fn by_grant_type(routes: Vec<(&str, u16, String, Duration)>) -> Self {
        Self::serving(
            Replies::ByGrantType(
                routes
                    .into_iter()
                    .map(|(grant, status, body, delay)| (grant.to_owned(), (status, body, delay)))
                    .collect(),
            ),
            Duration::ZERO,
        )
        .await
    }

    async fn serving(replies: Replies, delay: Duration) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let hits = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let recorder = Arc::clone(&seen);
        let counter = Arc::clone(&hits);
        let peak_counter = Arc::clone(&peak);
        let in_flight_counter = Arc::clone(&in_flight);
        let replies = Arc::new(replies);
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let recorder = Arc::clone(&recorder);
                let counter = Arc::clone(&counter);
                let peak_counter = Arc::clone(&peak_counter);
                let in_flight_counter = Arc::clone(&in_flight_counter);
                let replies = Arc::clone(&replies);
                tokio::spawn(serve_token(
                    stream,
                    recorder,
                    counter,
                    peak_counter,
                    in_flight_counter,
                    replies,
                    delay,
                ));
            }
        });
        Self {
            endpoint: format!("http://{addr}/token"),
            seen,
            hits,
            peak,
            in_flight,
            task,
        }
    }

    fn requests(&self) -> Vec<SeenRequest> {
        self.seen.lock().map(|log| log.clone()).unwrap_or_default()
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    fn peak_concurrency(&self) -> usize {
        let _ = &self.in_flight;
        self.peak.load(Ordering::SeqCst)
    }
}

#[allow(clippy::too_many_arguments)]
async fn serve_token(
    mut stream: TcpStream,
    recorder: Arc<Mutex<Vec<SeenRequest>>>,
    counter: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    in_flight: Arc<AtomicUsize>,
    replies: Arc<Replies>,
    default_delay: Duration,
) {
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    let mut content_length = 0usize;
    let mut head_end = None;
    loop {
        let Ok(read) = stream.read(&mut buf).await else {
            return;
        };
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&buf[..read]);
        if head_end.is_none() {
            if let Some(at) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                head_end = Some(at + 4);
                let head = String::from_utf8_lossy(&raw[..at]).to_ascii_lowercase();
                content_length = head
                    .lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .and_then(|v| v.trim().parse().ok())
                    .unwrap_or(0);
            }
        }
        if let Some(at) = head_end {
            if raw.len() >= at + content_length {
                break;
            }
        }
    }
    let Some(at) = head_end else { return };
    let form_body = String::from_utf8_lossy(&raw[at..]).to_string();
    let form = super::url::parse_query(&form_body);
    let seen = SeenRequest { form };
    // Chosen from the *parsed request*, so a routed server answers each caller
    // on its own merits rather than by arrival order.
    let (status, body, delay) = replies.pick(seen.get("grant_type"), default_delay);
    counter.fetch_add(1, Ordering::SeqCst);
    let concurrent = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
    peak.fetch_max(concurrent, Ordering::SeqCst);
    if let Ok(mut log) = recorder.lock() {
        log.push(seen);
    }
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
    in_flight.fetch_sub(1, Ordering::SeqCst);
    let response = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const REFRESH: &str = "1//refresh-token-do-not-leak";
const ACCESS: &str = "ya29.access-token-do-not-leak";

fn key() -> StoreKey {
    StoreKey::new("rmail-oauth-google", "user@example.com")
}

fn stored(expires_at: i64) -> StoredTokens {
    StoredTokens {
        provider: Provider::Google,
        client_id: "client-abc.apps.googleusercontent.com".to_owned(),
        client_secret: None,
        refresh_token: Secret::new(REFRESH),
        access_token: Some(Secret::new("stale-access-token")),
        expires_at,
        scopes: vec!["https://mail.google.com/".to_owned()],
    }
}

/// A broker over a memory store seeded with `tokens`, pointed at `server`.
fn broker_with(server: &TokenServer, tokens: StoredTokens) -> (OAuthBroker, Arc<MemoryTokenStore>) {
    let store = Arc::new(MemoryTokenStore::new());
    store.save(&key(), &tokens).unwrap();
    let broker = OAuthBroker::new(Arc::clone(&store) as Arc<dyn TokenStore>)
        .unwrap()
        .with_token_endpoint(&server.endpoint)
        .unwrap();
    (broker, store)
}

fn token_body(access: &str, expires_in: i64) -> String {
    serde_json::json!({
        "access_token": access,
        "expires_in": expires_in,
        "token_type": "Bearer",
        "scope": "https://mail.google.com/",
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// Secrets stay secret
// ---------------------------------------------------------------------------

/// The non-negotiable: no token material survives a `Debug` of anything this
/// module hands around. A plain `String` token field added to any of these
/// types fails here rather than in a log file six months later.
#[test]
fn token_material_never_survives_debug() {
    let tokens = stored(now() + 3600);
    let rendered = format!("{tokens:?}");
    assert!(
        !rendered.contains(REFRESH),
        "the refresh token leaked into Debug: {rendered}"
    );
    assert!(
        !rendered.contains("stale-access-token"),
        "the access token leaked into Debug: {rendered}"
    );
    assert!(rendered.contains("Secret(***)"), "expected redaction");

    // The same for the flow's own secrets.
    let pkce = Pkce::generate();
    let verifier = pkce.verifier().expose().to_owned();
    assert!(!format!("{pkce:?}").contains(&verifier));
    assert!(!format!("{:?}", pkce.verifier()).contains(&verifier));
}

#[tokio::test]
async fn broker_debug_names_no_account() {
    let server = TokenServer::always(200, token_body(ACCESS, 3600)).await;
    let (broker, _store) = broker_with(&server, stored(now() + 3600));
    let rendered = format!("{broker:?}");
    assert!(!rendered.contains("user@example.com"), "{rendered}");
    assert!(!rendered.contains(REFRESH), "{rendered}");
}

#[tokio::test]
async fn a_refreshed_token_is_not_in_any_error_or_debug() {
    let server = TokenServer::always(200, token_body(ACCESS, 3600)).await;
    let (broker, _store) = broker_with(&server, stored(now() - 10));
    let token = broker.access_token(&key()).await.unwrap();
    assert_eq!(token.expose(), ACCESS, "the fresh token is usable");
    assert!(!format!("{token:?}").contains(ACCESS));

    let status = broker.status(&key()).await.unwrap();
    assert!(!format!("{status:?}").contains(ACCESS));
    assert!(!format!("{status:?}").contains(REFRESH));
}

// ---------------------------------------------------------------------------
// PKCE
// ---------------------------------------------------------------------------

#[test]
fn pkce_challenge_is_the_rfc7636_s256_of_the_verifier() {
    // RFC 7636 appendix B's worked example, which pins the encoding (base64url,
    // unpadded) as well as the hash.
    let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    assert_eq!(
        super::pkce::challenge_for(verifier),
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
}

#[test]
fn pkce_pairs_are_unique_and_well_formed() {
    let a = Pkce::generate();
    let b = Pkce::generate();
    assert_ne!(a.verifier().expose(), b.verifier().expose());
    // RFC 7636 §4.1: 43..=128 characters from the unreserved set.
    let verifier = a.verifier().expose();
    assert_eq!(verifier.len(), 43, "32 random octets, base64url unpadded");
    assert!(
        verifier
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')),
        "verifier must be unreserved characters only: {verifier}"
    );
    assert!(!a.challenge().contains('='), "challenge must be unpadded");
    assert_eq!(a.challenge(), super::pkce::challenge_for(verifier));
}

// ---------------------------------------------------------------------------
// Authorization URL
// ---------------------------------------------------------------------------

#[test]
fn authorization_url_carries_pkce_and_asks_for_offline_access() {
    let url = Provider::Google.authorization_url(
        "client-abc",
        "http://127.0.0.1:54321/rmail/oauth/callback",
        &["https://mail.google.com/".to_owned()],
        "state-xyz",
        "challenge-123",
    );
    assert!(url.starts_with("https://accounts.google.com/o/oauth2/v2/auth?"));
    assert!(url.contains("code_challenge=challenge-123"));
    assert!(url.contains("code_challenge_method=S256"));
    assert!(url.contains("response_type=code"));
    assert!(url.contains("state=state-xyz"));
    // Without these two Google returns no refresh token on a re-consent, and
    // the account silently becomes browser-only.
    assert!(url.contains("access_type=offline"));
    assert!(url.contains("prompt=consent"));
    // The redirect URI and the scope must be escaped or the endpoint 400s.
    assert!(url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A54321%2Frmail%2Foauth%2Fcallback"));
    assert!(url.contains("scope=https%3A%2F%2Fmail.google.com%2F"));
}

#[test]
fn microsoft_requests_offline_access_as_a_scope_and_not_as_a_parameter() {
    let scopes = Provider::Microsoft.default_scopes();
    assert!(
        scopes.iter().any(|s| s == "offline_access"),
        "Microsoft issues no refresh token without it: {scopes:?}"
    );
    let url = Provider::Microsoft.authorization_url(
        "client-abc",
        "http://127.0.0.1:1/cb",
        &scopes,
        "s",
        "c",
    );
    assert!(url.contains("offline_access"));
    assert!(
        !url.contains("access_type=offline"),
        "that parameter is Google-specific"
    );
}

#[test]
fn provider_names_round_trip_from_what_a_user_would_type() {
    for (input, expected) in [
        ("gmail", Provider::Google),
        ("Google", Provider::Google),
        (" outlook ", Provider::Microsoft),
        ("microsoft", Provider::Microsoft),
        ("o365", Provider::Microsoft),
    ] {
        assert_eq!(Provider::parse(input).unwrap(), expected, "input {input:?}");
    }
    let err = Provider::parse("yahoo").expect_err("unknown provider must be rejected");
    assert_eq!(err.reason(), ErrorReason::InvalidArgument);
    for provider in Provider::ALL {
        assert_eq!(Provider::parse(provider.as_str()).unwrap(), *provider);
    }
}

// ---------------------------------------------------------------------------
// XOAUTH2
// ---------------------------------------------------------------------------

#[test]
fn xoauth2_matches_the_documented_sasl_string() {
    // The exact example from Google's XOAUTH2 documentation.
    let raw = xoauth2(
        "someuser@example.com",
        "vF9dft4qmTc2Nvb3RlckBhdHRhdmlzdGEuY29tCg==",
    );
    assert_eq!(
        raw.expose(),
        "user=someuser@example.com\x01auth=Bearer \
         vF9dft4qmTc2Nvb3RlckBhdHRhdmlzdGEuY29tCg==\x01\x01"
    );
    assert_eq!(
        xoauth2_b64("someuser@example.com", "vF9dft4qmTc2Nvb3RlckBhdHRhdmlzdGEuY29tCg==").expose(),
        "dXNlcj1zb21ldXNlckBleGFtcGxlLmNvbQFhdXRoPUJlYXJlciB2RjlkZnQ0cW1UYzJOdmIzUmxja0JoZEhSaGRtbHpkR0V1WTI5dENnPT0BAQ=="
    );
}

#[test]
fn the_xoauth2_string_is_a_secret() {
    let raw = xoauth2("u@example.com", ACCESS);
    assert!(
        !format!("{raw:?}").contains(ACCESS),
        "the bearer token is embedded in it verbatim"
    );
}

// ---------------------------------------------------------------------------
// Expiry, refresh, and skew
// ---------------------------------------------------------------------------

#[test]
fn spent_covers_expiry_the_skew_window_and_an_impossible_clock() {
    let now = 1_000_000;
    // Comfortably valid.
    assert!(!is_spent(&stored(now + 3600), now));
    // Already expired.
    assert!(is_spent(&stored(now - 1), now));
    // Inside the refresh-before-expiry window: still "valid" by the letter of
    // `expires_at`, but not for long enough to be worth handing out.
    assert!(
        is_spent(&stored(now + 30), now),
        "a token expiring in 30s must be refreshed before use"
    );
    // Exactly at the skew boundary counts as spent.
    let skew = i64::try_from(REFRESH_SKEW.as_secs()).unwrap();
    assert!(is_spent(&stored(now + skew), now));
    assert!(!is_spent(&stored(now + skew + 1), now));
    // Clock skew the other way: an expiry further out than any provider issues
    // means the clock that wrote it cannot be trusted.
    let day = i64::try_from(MAX_TOKEN_LIFETIME.as_secs()).unwrap();
    assert!(
        is_spent(&stored(now + day + 1), now),
        "an impossible lifetime is a bad clock, not a long-lived token"
    );
    assert!(!is_spent(&stored(now + day - 1), now));
    // No access token at all (a fresh store, or a restarted daemon).
    let mut none = stored(now + 3600);
    none.access_token = None;
    assert!(is_spent(&none, now));
}

#[tokio::test]
async fn a_valid_token_is_returned_without_touching_the_network() {
    let server = TokenServer::always(200, token_body(ACCESS, 3600)).await;
    let (broker, _store) = broker_with(&server, stored(now() + 3600));

    let token = broker.access_token(&key()).await.unwrap();
    assert_eq!(token.expose(), "stale-access-token", "the cached one");
    assert_eq!(server.hits(), 0, "no refresh was needed");

    let status = broker.refresh(&key(), false).await.unwrap();
    assert!(!status.refreshed);
    assert_eq!(server.hits(), 0);
}

#[tokio::test]
async fn an_expired_token_is_refreshed_and_the_new_one_is_persisted() {
    let server = TokenServer::always(200, token_body(ACCESS, 3600)).await;
    let (broker, store) = broker_with(&server, stored(now() - 5));

    let token = broker.access_token(&key()).await.unwrap();
    assert_eq!(token.expose(), ACCESS);
    assert_eq!(server.hits(), 1);

    let seen = server.requests();
    let form = seen.first().expect("one request");
    assert_eq!(form.get("grant_type"), Some("refresh_token"));
    assert_eq!(form.get("refresh_token"), Some(REFRESH));
    assert_eq!(
        form.get("client_id"),
        Some("client-abc.apps.googleusercontent.com")
    );

    // Written through to the store, not merely cached: a restarted daemon must
    // find the new access token rather than refresh again.
    let persisted = store.load(&key()).unwrap().unwrap();
    assert_eq!(
        persisted.access_token.as_ref().map(Secret::expose),
        Some(ACCESS)
    );
    assert!(persisted.expires_in() > 3500);
    assert_eq!(
        persisted.refresh_token.expose(),
        REFRESH,
        "Google returns no new refresh token; the old one must survive"
    );

    // And a second call is served from what the first stored.
    let again = broker.access_token(&key()).await.unwrap();
    assert_eq!(again.expose(), ACCESS);
    assert_eq!(server.hits(), 1, "no second refresh");
}

#[tokio::test]
async fn a_rotated_refresh_token_replaces_the_stored_one() {
    // Microsoft rotates on every use; keeping the old one would work exactly
    // once and then start failing with `invalid_grant`.
    let body = serde_json::json!({
        "access_token": ACCESS,
        "refresh_token": "rotated-refresh-token",
        "expires_in": 3599,
    })
    .to_string();
    let server = TokenServer::always(200, body).await;
    let (broker, store) = broker_with(&server, stored(now() - 5));

    broker.access_token(&key()).await.unwrap();
    let persisted = store.load(&key()).unwrap().unwrap();
    assert_eq!(persisted.refresh_token.expose(), "rotated-refresh-token");
    assert_eq!(
        persisted.scopes,
        vec!["https://mail.google.com/".to_owned()],
        "a refresh that returns no scope must not erase the granted ones"
    );
}

#[tokio::test]
async fn force_refreshes_a_token_that_has_not_expired() {
    let server = TokenServer::always(200, token_body(ACCESS, 3600)).await;
    let (broker, _store) = broker_with(&server, stored(now() + 3600));

    let status = broker.refresh(&key(), true).await.unwrap();
    assert!(status.refreshed);
    assert_eq!(server.hits(), 1);
    assert_eq!(status.provider, Provider::Google);
    assert!(status.expires_at > now());
}

#[tokio::test]
async fn a_missing_expires_in_does_not_mean_forever() {
    let body = serde_json::json!({ "access_token": ACCESS }).to_string();
    let server = TokenServer::always(200, body).await;
    let (broker, store) = broker_with(&server, stored(now() - 5));

    broker.access_token(&key()).await.unwrap();
    let persisted = store.load(&key()).unwrap().unwrap();
    let remaining = persisted.expires_in();
    assert!(
        remaining > 0 && remaining <= DEFAULT_EXPIRES_IN,
        "an absent expires_in must fall back to a bounded lifetime, got {remaining}"
    );
}

#[tokio::test]
async fn a_non_positive_expires_in_is_treated_as_already_expired() {
    let server = TokenServer::always(200, token_body(ACCESS, 0)).await;
    let (broker, store) = broker_with(&server, stored(now() - 5));

    broker.access_token(&key()).await.unwrap();
    let persisted = store.load(&key()).unwrap().unwrap();
    assert!(
        is_spent(&persisted, now()),
        "a token the provider says is already dead must not be trusted"
    );
}

/// A store that will not accept a write, to exercise the one place a refresh
/// deliberately survives a store failure.
#[derive(Debug)]
struct ReadOnlyStore(Arc<MemoryTokenStore>);

impl TokenStore for ReadOnlyStore {
    fn load(&self, key: &StoreKey) -> Result<Option<StoredTokens>, Error> {
        self.0.load(key)
    }
    fn save(&self, _key: &StoreKey, _tokens: &StoredTokens) -> Result<(), Error> {
        Err(Error::unauthenticated("the keychain is locked"))
    }
    fn delete(&self, key: &StoreKey) -> Result<(), Error> {
        self.0.delete(key)
    }
}

/// A refresh that cannot be written down must not fail the caller: the token
/// exists, the provider has already rotated, and failing would leave a sync
/// retrying with a credential the provider has retired.
#[tokio::test]
async fn a_refresh_survives_a_store_that_will_not_accept_the_write() {
    let body = serde_json::json!({
        "access_token": ACCESS,
        "refresh_token": "rotated",
        "expires_in": 3600,
    })
    .to_string();
    let server = TokenServer::always(200, body).await;
    let backing = Arc::new(MemoryTokenStore::new());
    backing.save(&key(), &stored(now() - 5)).unwrap();
    let broker = OAuthBroker::new(Arc::new(ReadOnlyStore(Arc::clone(&backing))))
        .unwrap()
        .with_token_endpoint(&server.endpoint)
        .unwrap();

    let token = broker
        .access_token(&key())
        .await
        .expect("a usable token must still be handed back");
    assert_eq!(token.expose(), ACCESS);

    // And the process keeps working from its cache rather than refreshing
    // again off the stale stored token.
    assert_eq!(broker.access_token(&key()).await.unwrap().expose(), ACCESS);
    assert_eq!(server.hits(), 1);
    assert_eq!(
        backing
            .load(&key())
            .unwrap()
            .unwrap()
            .refresh_token
            .expose(),
        REFRESH,
        "the durable copy really is stale — that is what the error log is for"
    );
}

// ---------------------------------------------------------------------------
// Concurrency
// ---------------------------------------------------------------------------

/// Sixteen callers notice the same expired token at the same moment. Exactly
/// one refresh may reach the provider, and all sixteen must come back with the
/// token that refresh produced — not fifteen of them with a token the provider
/// has already retired.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_callers_burn_exactly_one_refresh() {
    // Each reply rotates the refresh token, so a second refresh would be
    // visible in the store as well as in the hit count.
    let replies = vec![
        (
            200,
            serde_json::json!({
                "access_token": "first-refresh-result",
                "refresh_token": "rotation-1",
                "expires_in": 3600,
            })
            .to_string(),
        ),
        (
            200,
            serde_json::json!({
                "access_token": "second-refresh-result",
                "refresh_token": "rotation-2",
                "expires_in": 3600,
            })
            .to_string(),
        ),
    ];
    // A delay wide enough that every task is inside `access_token` before the
    // first refresh returns; without serialization they would all fire.
    let server = TokenServer::queued(replies, Duration::from_millis(150)).await;
    let (broker, store) = broker_with(&server, stored(now() - 5));
    let broker = Arc::new(broker);

    let mut tasks = Vec::new();
    for _ in 0..16 {
        let broker = Arc::clone(&broker);
        tasks.push(tokio::spawn(
            async move { broker.access_token(&key()).await },
        ));
    }
    let mut tokens = Vec::new();
    for task in tasks {
        tokens.push(task.await.unwrap().unwrap());
    }

    assert_eq!(server.hits(), 1, "only one refresh may reach the provider");
    for token in &tokens {
        assert_eq!(
            token.expose(),
            "first-refresh-result",
            "every caller must get the token the single refresh produced"
        );
    }
    let persisted = store.load(&key()).unwrap().unwrap();
    assert_eq!(
        persisted.refresh_token.expose(),
        "rotation-1",
        "a second refresh would have overwritten this with rotation-2"
    );
}

/// Different accounts must not queue behind each other: the lock is per
/// credential, not per broker.
///
/// Asserted by observing that the token endpoint really had two requests open
/// at once, rather than by timing the pair — a wall-clock threshold on a
/// shared CI container is a flake waiting to happen.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn separate_accounts_refresh_in_parallel() {
    let server = TokenServer::queued(
        vec![(200, token_body(ACCESS, 3600))],
        Duration::from_millis(200),
    )
    .await;
    let store = Arc::new(MemoryTokenStore::new());
    let a = StoreKey::new("svc", "a@example.com");
    let b = StoreKey::new("svc", "b@example.com");
    store.save(&a, &stored(now() - 5)).unwrap();
    store.save(&b, &stored(now() - 5)).unwrap();
    let broker = Arc::new(
        OAuthBroker::new(Arc::clone(&store) as Arc<dyn TokenStore>)
            .unwrap()
            .with_token_endpoint(&server.endpoint)
            .unwrap(),
    );

    let one = {
        let broker = Arc::clone(&broker);
        tokio::spawn(async move { broker.access_token(&a).await })
    };
    let two = {
        let broker = Arc::clone(&broker);
        tokio::spawn(async move { broker.access_token(&b).await })
    };
    one.await.unwrap().unwrap();
    two.await.unwrap().unwrap();

    assert_eq!(server.hits(), 2, "each account refreshes for itself");
    assert_eq!(
        server.peak_concurrency(),
        2,
        "the two refreshes serialized behind one another"
    );
}

/// `complete` must take the same per-account lock `access_token` does.
///
/// Without it: a refresh loads the old tokens, goes to the wire, and — while
/// it is out there — a re-consent stores a brand-new grant. The refresh then
/// writes back the tokens it loaded *before* that, silently undoing the
/// re-consent and leaving the account on a refresh token the provider has
/// already retired.
///
/// The refresh is made the *slow* one so that, unlocked, its write lands last
/// and clobbers. An unlocked run genuinely fails this; a locked run makes the
/// consent wait for the refresh to finish and then write after it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_re_consent_is_not_clobbered_by_a_refresh_that_was_already_in_flight() {
    let server = TokenServer::by_grant_type(vec![
        (
            "refresh_token",
            200,
            serde_json::json!({
                "access_token": "from-the-refresh",
                "refresh_token": "refresh-rotation",
                "expires_in": 3600,
            })
            .to_string(),
            Duration::from_millis(600),
        ),
        (
            "authorization_code",
            200,
            serde_json::json!({
                "access_token": "from-the-consent",
                "refresh_token": "the-users-new-grant",
                "expires_in": 3600,
            })
            .to_string(),
            Duration::ZERO,
        ),
    ])
    .await;
    let (broker, store) = broker_with(&server, stored(now() - 5));
    let broker = Arc::new(broker);

    let refreshing = {
        let broker = Arc::clone(&broker);
        tokio::spawn(async move { broker.access_token(&key()).await })
    };
    // Let the refresh get as far as the wire before the consent starts.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let pending = broker
        .begin(Provider::Google, "client-abc", None, None)
        .await
        .unwrap();
    let uri = pending.redirect_uri().to_owned();
    let state = pending
        .authorization_url()
        .split('&')
        .find_map(|p| p.strip_prefix("state="))
        .unwrap()
        .to_owned();
    let completing = {
        let broker = Arc::clone(&broker);
        tokio::spawn(async move {
            broker
                .complete(&key(), pending, CancellationToken::new())
                .await
        })
    };
    // The browser comes back immediately, so the consent is ready to exchange
    // long before the refresh's reply arrives.
    let page = get(&format!("{uri}?state={state}&code=consent-code")).await;
    assert!(page.contains("rmail is authorized"), "{page}");

    refreshing.await.unwrap().unwrap();
    tokio::time::timeout(Duration::from_secs(10), completing)
        .await
        .expect("the consent must complete")
        .unwrap()
        .unwrap();

    let persisted = store.load(&key()).unwrap().unwrap();
    assert_eq!(
        persisted.refresh_token.expose(),
        "the-users-new-grant",
        "the re-consent was overwritten by an older in-flight refresh"
    );
    assert_eq!(
        persisted.access_token.as_ref().map(Secret::expose),
        Some("from-the-consent"),
        "the re-consent's access token was overwritten too"
    );
    assert_eq!(
        broker.access_token(&key()).await.unwrap().expose(),
        "from-the-consent",
        "the cache kept the clobbered value"
    );
    assert_eq!(
        server.peak_concurrency(),
        1,
        "the exchange and the refresh must not have been at the endpoint together"
    );
}

#[test]
fn a_token_endpoint_override_must_be_https_or_loopback() {
    // Public API on a library crate: an override that accepted any `http://`
    // URL would be a supported way to ship every refresh token elsewhere in
    // cleartext.
    let broker = || OAuthBroker::new(Arc::new(MemoryTokenStore::new())).unwrap();
    for rejected in [
        "http://evil.example.com/token",
        "http://10.0.0.1/token",
        "ftp://example.com/token",
        "http://127.0.0.1.evil.example.com/token",
    ] {
        let err = broker()
            .with_token_endpoint(rejected)
            .expect_err("must be refused");
        assert_eq!(
            err.reason(),
            ErrorReason::InvalidArgument,
            "endpoint {rejected}"
        );
    }
    for accepted in [
        "https://oauth2.googleapis.com/token",
        "http://127.0.0.1:8080/token",
        "http://[::1]:8080/token",
    ] {
        assert!(
            broker().with_token_endpoint(accepted).is_ok(),
            "{accepted} must be accepted"
        );
    }
}

// ---------------------------------------------------------------------------
// Error paths
// ---------------------------------------------------------------------------

/// The revoked-grant path: `invalid_grant` means consent is gone, and the only
/// fix is a browser. It must not look like something worth retrying.
#[tokio::test]
async fn a_revoked_refresh_token_is_unauthenticated_and_says_to_reauthorize() {
    let body = serde_json::json!({
        "error": "invalid_grant",
        "error_description": "Token has been expired or revoked.",
    })
    .to_string();
    let server = TokenServer::always(400, body).await;
    let (broker, _store) = broker_with(&server, stored(now() - 5));

    let err = broker
        .access_token(&key())
        .await
        .expect_err("a revoked grant must fail");
    assert_eq!(err.reason(), ErrorReason::Unauthenticated);
    let message = err.to_string();
    assert!(
        message.contains("re-authorize") && message.contains("mail account login --oauth google"),
        "the error must tell the user how to fix it: {message}"
    );
    assert_eq!(
        server.hits(),
        1,
        "a revoked grant must not be retried in a loop"
    );

    // And every subsequent caller is answered from the recorded verdict rather
    // than by asking again. Without this, one revoked account turns every IMAP
    // connection and every SMTP send into a request to a provider that has
    // already said no — which *is* the retry loop, just spread across callers.
    for _ in 0..5 {
        let err = broker
            .access_token(&key())
            .await
            .expect_err("still revoked");
        assert_eq!(err.reason(), ErrorReason::Unauthenticated);
        assert!(err.to_string().contains("re-authorize"), "{err}");
    }
    assert_eq!(
        server.hits(),
        1,
        "the provider was asked once, not once per caller"
    );

    // `force` is the deliberate escape hatch for someone who has just fixed
    // things at the provider's end.
    let _ = broker.refresh(&key(), true).await;
    assert_eq!(server.hits(), 2, "--force must still reach the provider");
}

/// Re-consent clears the revoked verdict: the short-circuit is keyed to the
/// refresh token that was rejected, so a new grant is never held back by the
/// old one's failure.
#[tokio::test]
async fn re_consent_clears_a_revoked_verdict() {
    let revoked = serde_json::json!({ "error": "invalid_grant" }).to_string();
    let granted = serde_json::json!({
        "access_token": "fresh-access",
        "refresh_token": "fresh-refresh",
        "expires_in": 3600,
    })
    .to_string();
    let server = TokenServer::queued(
        vec![(400, revoked), (200, granted.clone()), (200, granted)],
        Duration::ZERO,
    )
    .await;
    let (broker, store) = broker_with(&server, stored(now() - 5));

    // Revoked, and recorded as such.
    assert_eq!(
        broker
            .access_token(&key())
            .await
            .expect_err("revoked")
            .reason(),
        ErrorReason::Unauthenticated
    );
    let _ = broker.access_token(&key()).await;
    assert_eq!(server.hits(), 1, "the second call short-circuited");

    // The user re-authorizes; a new refresh token lands in the store.
    let mut fresh = stored(now() - 5);
    fresh.refresh_token = Secret::new("fresh-refresh");
    store.save(&key(), &fresh).unwrap();
    broker.forget(&key()).await.unwrap();
    store.save(&key(), &fresh).unwrap();

    let token = broker
        .access_token(&key())
        .await
        .expect("a new grant must not inherit the old one's verdict");
    assert_eq!(token.expose(), "fresh-access");
}

#[tokio::test]
async fn a_revoked_grant_never_echoes_the_provider_description_or_the_token() {
    // A provider that quotes the request back — which Microsoft's
    // `error_description` has been observed to do.
    let body = serde_json::json!({
        "error": "invalid_grant",
        "error_description": format!("The token {REFRESH} is invalid"),
    })
    .to_string();
    let server = TokenServer::always(400, body).await;
    let (broker, _store) = broker_with(&server, stored(now() - 5));

    let err = broker.access_token(&key()).await.expect_err("must fail");
    let message = err.to_string();
    assert!(
        !message.contains(REFRESH),
        "the refresh token leaked through error_description: {message}"
    );
    assert!(
        !message.contains("error_description"),
        "provider free text must not be repeated: {message}"
    );
}

#[tokio::test]
async fn a_malformed_token_response_is_unavailable_and_quotes_nothing() {
    for body in [
        "not json at all".to_owned(),
        // Valid JSON, no access token — the shape a proxy's error page or a
        // half-written response has.
        serde_json::json!({ "expires_in": 3600, "token_type": "Bearer" }).to_string(),
        // A 200 that happens to carry a token next to a field that breaks the
        // parse: the body must still never be echoed.
        format!("{{\"access_token\": {{\"nested\": \"{ACCESS}\"}}}}"),
    ] {
        let server = TokenServer::always(200, body.clone()).await;
        let (broker, _store) = broker_with(&server, stored(now() - 5));
        let err = broker
            .access_token(&key())
            .await
            .expect_err("a malformed response must fail");
        assert_eq!(
            err.reason(),
            ErrorReason::Unavailable,
            "body {body:?} produced {err}"
        );
        let message = err.to_string();
        assert!(!message.contains(ACCESS), "leaked a token: {message}");
        assert!(!message.contains("not json at all"), "echoed the body");
    }
}

#[tokio::test]
async fn a_string_expires_in_is_accepted() {
    // Some providers ship it as a string; refusing the whole response over it
    // would break refresh entirely.
    let body = serde_json::json!({ "access_token": ACCESS, "expires_in": "3600" }).to_string();
    let server = TokenServer::always(200, body).await;
    let (broker, store) = broker_with(&server, stored(now() - 5));
    broker.access_token(&key()).await.unwrap();
    assert!(store.load(&key()).unwrap().unwrap().expires_in() > 3500);
}

#[tokio::test]
async fn transient_and_client_errors_are_classified_apart() {
    for (status, body, expected) in [
        (429, "{}".to_owned(), ErrorReason::Unavailable),
        (503, String::new(), ErrorReason::Unavailable),
        (
            401,
            serde_json::json!({ "error": "invalid_client" }).to_string(),
            ErrorReason::Unauthenticated,
        ),
        (401, "{}".to_owned(), ErrorReason::Unauthenticated),
        // An unrecognized 4xx is the OAuth *client configuration* being
        // wrong, not the caller's argument.
        (
            400,
            serde_json::json!({ "error": "invalid_request" }).to_string(),
            ErrorReason::FailedPrecondition,
        ),
    ] {
        let server = TokenServer::always(status, body.clone()).await;
        let (broker, _store) = broker_with(&server, stored(now() - 5));
        let err = broker.access_token(&key()).await.expect_err("must fail");
        assert_eq!(err.reason(), expected, "status {status} body {body:?}");
    }
}

#[tokio::test]
async fn an_account_that_was_never_authorized_is_a_failed_precondition() {
    let server = TokenServer::always(200, token_body(ACCESS, 3600)).await;
    let store = Arc::new(MemoryTokenStore::new());
    let broker = OAuthBroker::new(store as Arc<dyn TokenStore>)
        .unwrap()
        .with_token_endpoint(&server.endpoint)
        .unwrap();

    let err = broker.access_token(&key()).await.expect_err("no tokens");
    assert_eq!(err.reason(), ErrorReason::FailedPrecondition);
    assert!(
        err.to_string().contains("mail account login --oauth"),
        "{err}"
    );
    assert_eq!(server.hits(), 0, "nothing to refresh, so nothing was sent");
}

#[tokio::test]
async fn forget_removes_the_grant_from_the_store_and_the_cache() {
    let server = TokenServer::always(200, token_body(ACCESS, 3600)).await;
    let (broker, store) = broker_with(&server, stored(now() + 3600));
    broker.access_token(&key()).await.unwrap();

    broker.forget(&key()).await.unwrap();
    assert!(store.load(&key()).unwrap().is_none());
    let err = broker
        .access_token(&key())
        .await
        .expect_err("the cache must not outlive the stored grant");
    assert_eq!(err.reason(), ErrorReason::FailedPrecondition);
}

// ---------------------------------------------------------------------------
// The store
// ---------------------------------------------------------------------------

#[test]
fn stored_tokens_round_trip_through_the_stored_blob() {
    let store = MemoryTokenStore::new();
    let mut tokens = stored(1_700_000_000);
    tokens.client_secret = Some(Secret::new("desktop-client-secret"));
    store.save(&key(), &tokens).unwrap();

    let back = store.load(&key()).unwrap().unwrap();
    assert_eq!(back.provider, Provider::Google);
    assert_eq!(back.client_id, tokens.client_id);
    assert_eq!(
        back.client_secret.as_ref().map(Secret::expose),
        Some("desktop-client-secret")
    );
    assert_eq!(back.refresh_token.expose(), REFRESH);
    assert_eq!(back.expires_at, 1_700_000_000);
    assert_eq!(back.scopes, tokens.scopes);

    store.delete(&key()).unwrap();
    assert!(store.load(&key()).unwrap().is_none());
    store.delete(&key()).unwrap();
}

#[test]
fn a_corrupt_stored_blob_is_a_failed_precondition_that_quotes_nothing() {
    #[derive(Debug)]
    struct Corrupt;
    impl TokenStore for Corrupt {
        fn load(&self, _key: &StoreKey) -> Result<Option<StoredTokens>, Error> {
            StoredTokens::from_json(&format!("{{\"refresh_token\": \"{REFRESH}\"")).map(Some)
        }
        fn save(&self, _key: &StoreKey, _tokens: &StoredTokens) -> Result<(), Error> {
            Ok(())
        }
        fn delete(&self, _key: &StoreKey) -> Result<(), Error> {
            Ok(())
        }
    }
    let err = Corrupt
        .load(&key())
        .expect_err("a truncated blob must fail");
    assert_eq!(err.reason(), ErrorReason::FailedPrecondition);
    assert!(
        !err.to_string().contains(REFRESH),
        "the blob is the secret: {err}"
    );
}

#[test]
fn an_unknown_provider_in_the_store_is_rejected() {
    let raw = serde_json::json!({
        "provider": "yahoo",
        "client_id": "x",
        "refresh_token": REFRESH,
        "expires_at": 0,
    })
    .to_string();
    let err = StoredTokens::from_json(&raw).expect_err("unknown provider");
    assert_eq!(
        err.reason(),
        ErrorReason::FailedPrecondition,
        "a bad provider name out of the *store* is corruption, not a bad argument"
    );
    assert!(!err.to_string().contains(REFRESH), "{err}");
}

// ---------------------------------------------------------------------------
// Account -> store key
// ---------------------------------------------------------------------------

#[test]
fn key_for_addresses_the_grant_by_service_and_login() {
    let account = |credential, username: Option<&str>| crate::account::Account {
        id: 7,
        name: "Personal".to_owned(),
        imap_server: None,
        imap_port: None,
        username: username.map(str::to_owned),
        smtp_server: None,
        smtp_port: None,
        credential,
        created_at: 0,
        updated_at: 0,
    };

    let key = key_for(&account(
        crate::CredentialSource::OAuth("rmail-oauth-google-7".to_owned()),
        Some("user@example.com"),
    ))
    .unwrap();
    assert_eq!(
        key,
        StoreKey::new("rmail-oauth-google-7", "user@example.com")
    );
    assert!(key.describe().contains("user@example.com"));

    // Not an OAuth account.
    let err = key_for(&account(
        crate::CredentialSource::Keychain("svc".to_owned()),
        Some("u"),
    ))
    .expect_err("a password account has no grant");
    assert_eq!(err.reason(), ErrorReason::FailedPrecondition);

    // No username: the grant is filed per login, so there is nothing to look
    // up — and nothing to put in the XOAUTH2 `user=` either.
    for username in [None, Some("")] {
        let err = key_for(&account(
            crate::CredentialSource::OAuth("svc".to_owned()),
            username,
        ))
        .expect_err("no username");
        assert_eq!(err.reason(), ErrorReason::FailedPrecondition);
    }
}

/// A credential source that has no password must refuse to produce one rather
/// than return something an IMAP `LOGIN` would send.
#[test]
fn an_oauth_credential_source_has_no_password_to_resolve() {
    let err = crate::CredentialSource::OAuth("svc".to_owned())
        .resolve(Some("user@example.com"))
        .expect_err("OAuth resolves through the broker, not here");
    assert_eq!(err.reason(), ErrorReason::FailedPrecondition);
    assert!(err.to_string().contains("XOAUTH2"), "{err}");
}

// ---------------------------------------------------------------------------
// The loopback redirect
// ---------------------------------------------------------------------------

/// Pretend to be the browser: GET the redirect URI the flow published.
async fn get(uri: &str) -> String {
    let rest = uri.strip_prefix("http://").unwrap_or(uri);
    let (authority, target) = rest.split_once('/').unwrap_or((rest, ""));
    let mut stream = TcpStream::connect(authority).await.unwrap();
    stream
        .write_all(format!("GET /{target} HTTP/1.1\r\nHost: {authority}\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut body = Vec::new();
    stream.read_to_end(&mut body).await.unwrap();
    String::from_utf8_lossy(&body).into_owned()
}

#[tokio::test]
async fn the_redirect_delivers_a_percent_encoded_code_and_checks_state() {
    let redirect = LoopbackRedirect::bind().await.unwrap();
    let uri = redirect.redirect_uri().to_owned();
    assert!(
        uri.starts_with("http://127.0.0.1:"),
        "never `localhost`: {uri}"
    );
    let state = redirect.state().expose().to_owned();

    let waiter = tokio::spawn(async move {
        let code = redirect
            .wait_for_code(CancellationToken::new())
            .await
            .unwrap();
        code.expose().to_owned()
    });
    // Junk first: a browser prefetching a favicon must not end the flow.
    let authority = uri
        .strip_prefix("http://")
        .and_then(|r| r.split('/').next())
        .unwrap()
        .to_owned();
    let not_found = get(&format!("http://{authority}/favicon.ico")).await;
    assert!(not_found.contains("404"), "{not_found}");

    // Google's codes contain `/`, which arrives percent-encoded.
    let page = get(&format!(
        "{uri}?state={state}&code=4%2F0AY0e-g7SOME%2Bcode&scope=https%3A%2F%2Fmail.google.com%2F"
    ))
    .await;
    assert!(page.contains("200 OK"), "{page}");
    assert!(page.contains("rmail is authorized"));

    let code = tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .expect("the flow must complete")
        .unwrap();
    assert_eq!(code, "4/0AY0e-g7SOME+code", "percent-decoded verbatim");
}

/// A redirect carrying a `state` this flow never issued is refused — and the
/// flow keeps waiting, so a local process cannot kill an authorization it
/// cannot otherwise touch.
#[tokio::test]
async fn a_foreign_state_is_refused_without_ending_the_flow() {
    let redirect = LoopbackRedirect::bind().await.unwrap();
    let uri = redirect.redirect_uri().to_owned();
    let state = redirect.state().expose().to_owned();
    let waiter = tokio::spawn(async move {
        let code = redirect
            .wait_for_code(CancellationToken::new())
            .await
            .unwrap();
        code.expose().to_owned()
    });

    // Three planted redirects, including one carrying a code.
    for planted in [
        "state=planted-by-someone-else&code=stolen",
        "code=no-state-at-all",
        "state=&code=empty-state",
    ] {
        let page = get(&format!("{uri}?{planted}")).await;
        assert!(page.contains("400"), "{planted} produced {page}");
        assert!(
            !page.contains("rmail is authorized"),
            "{planted} must not be accepted"
        );
    }

    // The genuine redirect still lands.
    let page = get(&format!("{uri}?state={state}&code=the-real-one")).await;
    assert!(page.contains("rmail is authorized"), "{page}");
    let code = tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .expect("the flow must survive the planted redirects")
        .unwrap();
    assert_eq!(code, "the-real-one");
}

#[tokio::test]
async fn a_declined_authorization_is_permission_denied() {
    let redirect = LoopbackRedirect::bind().await.unwrap();
    let uri = redirect.redirect_uri().to_owned();
    let state = redirect.state().expose().to_owned();
    let waiter =
        tokio::spawn(async move { redirect.wait_for_code(CancellationToken::new()).await });

    let _ = get(&format!(
        "{uri}?state={state}&error=access_denied&error_description=The+user+said+no"
    ))
    .await;

    let err = tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .expect("must not hang")
        .unwrap()
        .expect_err("a decline is not a success");
    assert_eq!(err.reason(), ErrorReason::PermissionDenied);
    assert!(
        !err.to_string().contains("The user said no"),
        "provider free text must not be repeated: {err}"
    );
}

#[tokio::test]
async fn a_redirect_with_no_code_is_unauthenticated() {
    let redirect = LoopbackRedirect::bind().await.unwrap();
    let uri = redirect.redirect_uri().to_owned();
    let state = redirect.state().expose().to_owned();
    let waiter =
        tokio::spawn(async move { redirect.wait_for_code(CancellationToken::new()).await });

    let _ = get(&format!("{uri}?state={state}")).await;
    let err = tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .expect("must not hang")
        .unwrap()
        .expect_err("no code is not a success");
    assert_eq!(err.reason(), ErrorReason::Unauthenticated);
}

#[tokio::test]
async fn cancelling_the_flow_stops_waiting() {
    let redirect = LoopbackRedirect::bind().await.unwrap();
    let cancel = CancellationToken::new();
    let token = cancel.clone();
    let waiter = tokio::spawn(async move { redirect.wait_for_code(token).await });
    cancel.cancel();

    let err = tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .expect("cancellation must be honored promptly")
        .unwrap()
        .expect_err("a cancelled flow yields no code");
    assert_eq!(err.reason(), ErrorReason::Cancelled);
}

#[tokio::test]
async fn begin_publishes_a_bound_redirect_and_a_usable_url() {
    let store = Arc::new(MemoryTokenStore::new());
    let broker = OAuthBroker::new(store as Arc<dyn TokenStore>).unwrap();
    let pending = broker
        .begin(Provider::Microsoft, "  client-abc  ", None, None)
        .await
        .unwrap();

    assert_eq!(pending.provider(), Provider::Microsoft);
    // The port is bound *now*, so nothing else can claim it between publishing
    // the URL and the browser arriving.
    let authority = pending
        .redirect_uri()
        .strip_prefix("http://")
        .and_then(|r| r.split('/').next())
        .unwrap();
    assert!(
        TcpListener::bind(authority).await.is_err(),
        "the redirect port must already be held by this flow"
    );

    let url = pending.authorization_url();
    assert!(url.starts_with(Provider::Microsoft.authorize_endpoint()));
    assert!(url.contains("client_id=client-abc"), "trimmed: {url}");
    assert!(url.contains("code_challenge_method=S256"));

    let err = broker
        .begin(Provider::Google, "   ", None, None)
        .await
        .expect_err("an empty client id must be rejected");
    assert_eq!(err.reason(), ErrorReason::InvalidArgument);
}

/// The whole flow, end to end: begin, browser redirect, code exchange, storage,
/// and then a refresh off the stored grant.
#[tokio::test]
async fn a_complete_pkce_flow_stores_a_grant_that_can_then_refresh() {
    let exchange = serde_json::json!({
        "access_token": ACCESS,
        "refresh_token": REFRESH,
        "expires_in": 3600,
        "scope": "https://mail.google.com/",
        "token_type": "Bearer",
    })
    .to_string();
    let refreshed = serde_json::json!({
        "access_token": "second-access-token",
        "expires_in": 3600,
    })
    .to_string();
    let server = TokenServer::queued(
        vec![(200, exchange), (200, refreshed)],
        Duration::from_millis(0),
    )
    .await;
    let store = Arc::new(MemoryTokenStore::new());
    let broker = OAuthBroker::new(Arc::clone(&store) as Arc<dyn TokenStore>)
        .unwrap()
        .with_token_endpoint(&server.endpoint)
        .unwrap();

    let pending = broker
        .begin(
            Provider::Google,
            "client-abc",
            Some(Secret::new("desktop-secret")),
            None,
        )
        .await
        .unwrap();
    let url = pending.authorization_url();
    let uri = pending.redirect_uri().to_owned();
    // The `state` the flow issued, read out of the URL exactly as the provider
    // would.
    let state = url
        .split('&')
        .find_map(|p| p.strip_prefix("state="))
        .unwrap()
        .to_owned();
    let challenge = url
        .split('&')
        .find_map(|p| p.strip_prefix("code_challenge="))
        .unwrap()
        .to_owned();

    let completed = tokio::spawn({
        let store = Arc::clone(&store);
        async move {
            let broker = broker;
            let status = broker
                .complete(&key(), pending, CancellationToken::new())
                .await;
            (broker, store, status)
        }
    });
    let page = get(&format!("{uri}?state={state}&code=auth-code-123")).await;
    assert!(page.contains("200 OK"), "{page}");

    let (broker, store, status) = tokio::time::timeout(Duration::from_secs(5), completed)
        .await
        .expect("the flow must complete")
        .unwrap();
    let status = status.unwrap();
    assert!(status.refreshed);
    assert_eq!(status.provider, Provider::Google);
    assert_eq!(status.scopes, vec!["https://mail.google.com/".to_owned()]);

    let seen = server.requests();
    let exchange = seen.first().expect("the code exchange");
    assert_eq!(exchange.get("grant_type"), Some("authorization_code"));
    assert_eq!(exchange.get("code"), Some("auth-code-123"));
    assert_eq!(exchange.get("redirect_uri"), Some(uri.as_str()));
    assert_eq!(exchange.get("client_secret"), Some("desktop-secret"));
    // The verifier posted here must be the pre-image of the challenge that was
    // published in the URL — that is the whole of PKCE.
    let verifier = exchange.get("code_verifier").expect("a verifier");
    assert_eq!(
        super::pkce::challenge_for(verifier),
        challenge,
        "the posted verifier does not hash to the published challenge"
    );

    // The grant is stored, and refreshing off it works.
    let persisted = store.load(&key()).unwrap().unwrap();
    assert_eq!(persisted.refresh_token.expose(), REFRESH);
    let status = broker.refresh(&key(), true).await.unwrap();
    assert!(status.refreshed);
    assert_eq!(
        broker.access_token(&key()).await.unwrap().expose(),
        "second-access-token"
    );
}

#[tokio::test]
async fn an_exchange_with_no_refresh_token_is_refused_rather_than_stored() {
    // Google does this on a re-consent without `prompt=consent`: an access
    // token and nothing durable. Storing it would produce an account that
    // works for an hour and then needs a browser forever after.
    let body = serde_json::json!({ "access_token": ACCESS, "expires_in": 3600 }).to_string();
    let server = TokenServer::always(200, body).await;
    let store = Arc::new(MemoryTokenStore::new());
    let broker = OAuthBroker::new(Arc::clone(&store) as Arc<dyn TokenStore>)
        .unwrap()
        .with_token_endpoint(&server.endpoint)
        .unwrap();

    let pending = broker
        .begin(Provider::Google, "client-abc", None, None)
        .await
        .unwrap();
    let uri = pending.redirect_uri().to_owned();
    let state = pending
        .authorization_url()
        .split('&')
        .find_map(|p| p.strip_prefix("state="))
        .unwrap()
        .to_owned();

    let completed = tokio::spawn(async move {
        broker
            .complete(&key(), pending, CancellationToken::new())
            .await
    });
    let _ = get(&format!("{uri}?state={state}&code=abc")).await;

    let err = tokio::time::timeout(Duration::from_secs(5), completed)
        .await
        .expect("must not hang")
        .unwrap()
        .expect_err("no refresh token is a failed authorization");
    assert_eq!(err.reason(), ErrorReason::FailedPrecondition);
    assert!(
        store.load(&key()).unwrap().is_none(),
        "nothing may be stored for a grant that cannot be refreshed"
    );
}

// ---------------------------------------------------------------------------
// URL helpers
// ---------------------------------------------------------------------------

#[test]
fn query_values_round_trip_through_encode_and_parse() {
    let value = "a b+c/d?e&f=g%h~i-j_k.l";
    let encoded = super::url::encode_value(value);
    assert!(!encoded.contains(' '));
    assert!(!encoded.contains('+'), "a bare + would decode as a space");
    let parsed = super::url::parse_query(&format!("v={encoded}"));
    assert_eq!(parsed, vec![("v".to_owned(), value.to_owned())]);
}

#[test]
fn parse_query_tolerates_junk() {
    assert_eq!(super::url::parse_query(""), Vec::new());
    assert_eq!(
        super::url::parse_query("a=1&&b&c=%zz&d=%"),
        vec![
            ("a".to_owned(), "1".to_owned()),
            ("b".to_owned(), String::new()),
            ("c".to_owned(), "%zz".to_owned()),
            ("d".to_owned(), "%".to_owned()),
        ]
    );
}

/// A `%` followed by a multi-byte character must not panic.
///
/// `read_request_line` builds its string with `from_utf8_lossy`, so any local
/// process can put a three-byte U+FFFD after a `%` just by sending an invalid
/// byte — and slicing the `&str` at `i+1..i+3` there lands inside a character.
/// The panic would unwind out of the redirect listener and take the gRPC
/// connection with it.
#[test]
fn a_percent_before_a_multibyte_character_does_not_panic() {
    for input in [
        "a=%\u{FFFD}",
        "a=%€",
        "a=%\u{FFFD}\u{FFFD}&b=1",
        "%€=%€",
        "a=%f\u{FFFD}",
        // The exact shape a lossy-decoded `?a=%<0xFF>` takes.
        &String::from_utf8_lossy(b"a=%\xff"),
    ] {
        let parsed = super::url::parse_query(input);
        assert!(!parsed.is_empty(), "input {input:?} decoded to nothing");
    }
}
