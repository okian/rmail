//! Integration test: the OAuth2 broker RPCs
//! (`AccountService.BeginOAuth/CompleteOAuth/RefreshToken`) driven end to end
//! against an in-process tonic server over a Unix domain socket — including
//! the full loopback+PKCE flow, with a mock token endpoint standing in for the
//! provider and a browser simulated by a plain `GET` to the redirect URI.
//!
//! Nothing here touches the network, and nothing here touches the Keychain:
//! the daemon's broker is installed over an in-memory store before the server
//! starts, because there is no Keychain in the container the suite runs in.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use rmail_core::oauth::{
    MemoryTokenStore, OAuthBroker, Provider, StoreKey, StoredTokens, TokenStore,
};
use rmail_core::Secret;
use rmail_proto::v1::account_service_client::AccountServiceClient;
use rmail_proto::v1::{
    BeginOAuthRequest, CompleteOAuthRequest, CreateAccountRequest, GetAccountRequest,
    RefreshTokenRequest,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tonic::transport::Channel;
use tonic::Code;

static COUNTER: AtomicU32 = AtomicU32::new(0);

// ---------------------------------------------------------------------------
// The process's broker, over a memory store and a mock token endpoint
// ---------------------------------------------------------------------------

/// The mock token endpoint every test in this binary shares.
///
/// One endpoint rather than one per test because the broker is process-wide
/// (see `rmail_core::oauth`'s module docs) and is installed exactly once; its
/// token endpoint is fixed at construction. Each test uses its own account,
/// so they do not interfere.
struct Fixtures {
    store: Arc<MemoryTokenStore>,
}

fn fixtures() -> &'static Fixtures {
    static FIXTURES: OnceLock<Fixtures> = OnceLock::new();
    FIXTURES.get_or_init(|| {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let store = Arc::new(MemoryTokenStore::new());
        let endpoint = format!("http://{addr}/token");

        let broker = OAuthBroker::new(Arc::clone(&store) as Arc<dyn TokenStore>)
            .unwrap()
            .with_token_endpoint(&endpoint)
            .unwrap();
        assert!(
            rmail_core::oauth::install_broker(Arc::new(broker)),
            "this binary must own the process broker"
        );

        // Served by a task started lazily on the first runtime that asks; the
        // listener is bound synchronously above so the endpoint URL is known
        // before any server starts.
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let listener = TcpListener::from_std(listener).unwrap();
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        return;
                    };
                    tokio::spawn(serve_token(stream));
                }
            });
        });

        Fixtures { store }
    })
}

/// The refresh token a test seeds when it wants the provider to say the grant
/// has been revoked. Shaped like a real Google one, `/` and all.
const REVOKED_REFRESH: &str = "1//revoked-by-the-user";

/// The part of [`REVOKED_REFRESH`] the mock matches on.
///
/// The request body is `application/x-www-form-urlencoded`, so the token's
/// `//` arrives as `%2F%2F` and a substring search for the whole token finds
/// nothing — which silently turned this test into an assertion that a *valid*
/// grant refreshes. Matching on the unreserved tail is what actually reaches
/// the revoked branch, and it is equally what a leak assertion must look for.
const REVOKED_MARKER: &str = "revoked-by-the-user";

/// Answer with a usable grant, unless the request presents [`REVOKED_REFRESH`]
/// — in which case answer the way a provider answers a grant the user has
/// revoked, so the whole re-consent path is exercised through gRPC.
async fn serve_token(mut stream: TcpStream) {
    let mut buf = [0u8; 4096];
    // One read is enough: the form is a few hundred bytes and arrives with the
    // head in the same segment.
    let read = stream.read(&mut buf).await.unwrap_or(0);
    let request = String::from_utf8_lossy(&buf[..read]).into_owned();

    let (status, body) = if request.contains(REVOKED_MARKER) {
        (
            "400 Bad Request",
            serde_json::json!({
                "error": "invalid_grant",
                "error_description": format!("Token {REVOKED_REFRESH} has been revoked."),
            })
            .to_string(),
        )
    } else {
        (
            "200 OK",
            serde_json::json!({
                "access_token": "ya29.integration-access",
                "refresh_token": "1//integration-refresh",
                "expires_in": 3600,
                "scope": "https://mail.google.com/",
            })
            .to_string(),
        )
    };
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}

// ---------------------------------------------------------------------------
// The server harness
// ---------------------------------------------------------------------------

struct TestServer {
    socket: PathBuf,
    db_path: PathBuf,
    shutdown: oneshot::Sender<()>,
    handle: JoinHandle<Result<(), rmaild::ServeError>>,
}

impl TestServer {
    async fn start() -> Self {
        // Installed before the daemon builds its `AccountApi`, which takes the
        // process broker.
        let _ = fixtures();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let socket = PathBuf::from("/tmp").join(format!("rmail-oauth-{pid}-{n}.sock"));
        let db_path = std::env::temp_dir().join(format!("rmail-oauth-{pid}-{n}.db"));
        let db = rmail_core::Database::open(&db_path).unwrap();

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let server_socket = socket.clone();
        let handle = tokio::spawn(async move {
            rmaild::serve_uds(&server_socket, db, async move {
                let _ = shutdown_rx.await;
            })
            .await
        });

        let mut ready = false;
        for _ in 0..200 {
            if rmail_core::connect_uds(&socket).await.is_ok() {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(ready, "server never became ready");

        Self {
            socket,
            db_path,
            shutdown: shutdown_tx,
            handle,
        }
    }

    async fn client(&self) -> AccountServiceClient<Channel> {
        let channel = rmail_core::connect_uds(&self.socket).await.unwrap();
        AccountServiceClient::new(channel)
    }

    async fn shutdown(self) {
        self.shutdown.send(()).unwrap();
        self.handle.await.unwrap().unwrap();
        for suffix in ["", "-wal", "-shm"] {
            let _ =
                std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.db_path.display())));
        }
    }
}

async fn make_account(client: &mut AccountServiceClient<Channel>, name: &str) -> i64 {
    client
        .create(CreateAccountRequest {
            name: name.to_owned(),
            username: Some(format!("{name}@example.com")),
            imap_server: Some("imap.gmail.com".to_owned()),
            imap_port: Some(993),
            ..Default::default()
        })
        .await
        .expect("create")
        .into_inner()
        .id
}

/// Play the browser: `GET` the redirect URI with a code and the flow's own
/// `state`, read back out of the authorization URL exactly as the provider
/// would.
async fn follow_redirect(authorization_url: &str, redirect_uri: &str, code: &str) -> String {
    let state = authorization_url
        .split('&')
        .find_map(|p| p.strip_prefix("state="))
        .expect("the authorization URL carries a state");
    let rest = redirect_uri.strip_prefix("http://").unwrap();
    let (authority, path) = rest.split_once('/').unwrap();
    let mut stream = TcpStream::connect(authority).await.unwrap();
    stream
        .write_all(
            format!("GET /{path}?state={state}&code={code} HTTP/1.1\r\nHost: {authority}\r\n\r\n")
                .as_bytes(),
        )
        .await
        .unwrap();
    let mut page = Vec::new();
    stream.read_to_end(&mut page).await.unwrap();
    String::from_utf8_lossy(&page).into_owned()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The whole flow over gRPC: begin, browser redirect, code exchange, and the
/// account switched onto its new OAuth credential — with no token material in
/// any response.
#[tokio::test]
async fn begin_and_complete_authorize_an_account() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let id = make_account(&mut client, "gmail").await;

    let begun = client
        .begin_o_auth(BeginOAuthRequest {
            account_id: id,
            provider: "gmail".to_owned(),
            client_id: "client-abc.apps.googleusercontent.com".to_owned(),
            client_secret_command: Some("printf desktop-secret".to_owned()),
            scopes: Vec::new(),
        })
        .await
        .expect("begin")
        .into_inner();

    assert!(!begun.flow_id.is_empty());
    assert!(begun
        .authorization_url
        .starts_with("https://accounts.google.com/o/oauth2/v2/auth?"));
    assert!(begun
        .authorization_url
        .contains("code_challenge_method=S256"));
    assert!(begun.redirect_uri.starts_with("http://127.0.0.1:"));
    assert!(begun.expires_at > 0);

    // Complete blocks on the redirect, so drive both at once.
    let completing = {
        let mut client = server.client().await;
        let flow_id = begun.flow_id.clone();
        tokio::spawn(async move {
            client
                .complete_o_auth(CompleteOAuthRequest { flow_id })
                .await
        })
    };
    let page = follow_redirect(&begun.authorization_url, &begun.redirect_uri, "auth-code-1").await;
    assert!(page.contains("rmail is authorized"), "{page}");

    let done = tokio::time::timeout(Duration::from_secs(10), completing)
        .await
        .expect("the flow must complete")
        .unwrap()
        .expect("complete")
        .into_inner();
    assert_eq!(done.account_id, id);
    assert_eq!(done.provider, "google");
    assert_eq!(done.scopes, vec!["https://mail.google.com/".to_owned()]);
    assert!(done.expires_at > 0);

    // No token material anywhere in the responses.
    let rendered = format!("{begun:?}{done:?}");
    assert!(!rendered.contains("integration-refresh"), "{rendered}");
    assert!(!rendered.contains("integration-access"), "{rendered}");

    // The account now points at its grant, by reference only.
    let account = client
        .get(GetAccountRequest { id })
        .await
        .expect("get")
        .into_inner();
    assert_eq!(account.credential_kind, "oauth");
    let service = account.credential_ref.expect("a keychain service");
    assert!(service.starts_with("rmail-oauth-google-"), "{service}");

    // And the grant really is in the store, under that service.
    let stored = fixtures()
        .store
        .load(&StoreKey::new(service, "gmail@example.com"))
        .unwrap()
        .expect("the refresh token was persisted");
    assert_eq!(stored.refresh_token.expose(), "1//integration-refresh");
    assert_eq!(
        stored.client_secret.as_ref().map(Secret::expose),
        Some("desktop-secret"),
        "the client secret was resolved from its command, not carried over the wire"
    );

    // A second Complete on the same handle finds nothing: an authorization
    // code may be exchanged exactly once.
    let status = client
        .complete_o_auth(CompleteOAuthRequest {
            flow_id: begun.flow_id,
        })
        .await
        .expect_err("a flow may be completed once");
    assert_eq!(status.code(), Code::NotFound);

    server.shutdown().await;
}

#[tokio::test]
async fn begin_rejects_an_unknown_provider_a_missing_account_and_a_bad_client() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let id = make_account(&mut client, "picky").await;

    let bad_provider = client
        .begin_o_auth(BeginOAuthRequest {
            account_id: id,
            provider: "yahoo".to_owned(),
            client_id: "c".to_owned(),
            ..Default::default()
        })
        .await
        .expect_err("unknown provider");
    assert_eq!(bad_provider.code(), Code::InvalidArgument);

    let missing = client
        .begin_o_auth(BeginOAuthRequest {
            account_id: 9_999,
            provider: "google".to_owned(),
            client_id: "c".to_owned(),
            ..Default::default()
        })
        .await
        .expect_err("no such account");
    assert_eq!(missing.code(), Code::NotFound);

    let no_client = client
        .begin_o_auth(BeginOAuthRequest {
            account_id: id,
            provider: "google".to_owned(),
            client_id: "  ".to_owned(),
            ..Default::default()
        })
        .await
        .expect_err("an empty client id");
    assert_eq!(no_client.code(), Code::InvalidArgument);

    // A client-secret command that fails must stop the flow *before* a browser
    // is opened, not after the user has consented.
    let bad_secret = client
        .begin_o_auth(BeginOAuthRequest {
            account_id: id,
            provider: "google".to_owned(),
            client_id: "c".to_owned(),
            client_secret_command: Some("exit 1".to_owned()),
            scopes: Vec::new(),
        })
        .await
        .expect_err("a failing client-secret command");
    assert_eq!(bad_secret.code(), Code::Unauthenticated);

    server.shutdown().await;
}

/// An account with no username cannot hold a grant — the username is both the
/// Keychain account field and the XOAUTH2 `user=`. Caught before a browser is
/// opened.
#[tokio::test]
async fn begin_refuses_an_account_with_no_username() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let id = client
        .create(CreateAccountRequest {
            name: "anonymous".to_owned(),
            ..Default::default()
        })
        .await
        .expect("create")
        .into_inner()
        .id;

    let status = client
        .begin_o_auth(BeginOAuthRequest {
            account_id: id,
            provider: "google".to_owned(),
            client_id: "c".to_owned(),
            ..Default::default()
        })
        .await
        .expect_err("no username");
    assert_eq!(status.code(), Code::FailedPrecondition);
    assert!(status.message().contains("username"), "{status:?}");

    server.shutdown().await;
}

/// A caller that starts authorizations and never finishes them must not be
/// able to hold loopback ports without limit — including when the calls are
/// concurrent, which is the case a check-then-await cap does not survive.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pending_authorizations_are_capped_even_under_concurrency() {
    let server = TestServer::start().await;
    let mut setup = server.client().await;
    let id = make_account(&mut setup, "hoarder").await;

    let mut tasks = Vec::new();
    for _ in 0..24 {
        let mut client = server.client().await;
        tasks.push(tokio::spawn(async move {
            client
                .begin_o_auth(BeginOAuthRequest {
                    account_id: id,
                    provider: "google".to_owned(),
                    client_id: "c".to_owned(),
                    ..Default::default()
                })
                .await
                .map(|_| ())
        }));
    }

    let mut admitted = 0usize;
    let mut refused = 0usize;
    for task in tasks {
        match task.await.unwrap() {
            Ok(()) => admitted += 1,
            Err(status) => {
                assert_eq!(status.code(), Code::ResourceExhausted, "{status:?}");
                refused += 1;
            }
        }
    }
    assert!(
        admitted <= 8,
        "the cap admitted {admitted} concurrent flows; check-then-await is not a cap"
    );
    assert!(
        refused > 0,
        "nothing was refused out of 24 concurrent begins"
    );
    assert_eq!(admitted + refused, 24);

    server.shutdown().await;
}

#[tokio::test]
async fn complete_with_an_unknown_flow_is_not_found() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let status = client
        .complete_o_auth(CompleteOAuthRequest {
            flow_id: "deadbeef".to_owned(),
        })
        .await
        .expect_err("no such flow");
    assert_eq!(status.code(), Code::NotFound);

    server.shutdown().await;
}

/// `RefreshToken` on a stored grant: valid tokens are reported as-is, `force`
/// goes to the provider, and nothing token-shaped comes back.
#[tokio::test]
async fn refresh_token_reports_expiry_without_returning_a_token() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let id = make_account(&mut client, "refreshable").await;

    let service = format!("rmail-oauth-test-{id}");
    let key = StoreKey::new(service.clone(), "refreshable@example.com");
    fixtures()
        .store
        .save(
            &key,
            &StoredTokens {
                provider: Provider::Google,
                client_id: "client-abc".to_owned(),
                client_secret: None,
                refresh_token: Secret::new("1//seeded-refresh"),
                access_token: Some(Secret::new("ya29.seeded-access")),
                expires_at: rmail_core::oauth::unix_now() + 3600,
                scopes: vec!["https://mail.google.com/".to_owned()],
            },
        )
        .unwrap();
    point_account_at(&server, id, &service).await;

    let still_good = client
        .refresh_token(RefreshTokenRequest {
            account_id: id,
            force: false,
        })
        .await
        .expect("refresh")
        .into_inner();
    assert!(!still_good.refreshed, "a valid token needs no round trip");
    assert_eq!(still_good.provider, "google");

    let forced = client
        .refresh_token(RefreshTokenRequest {
            account_id: id,
            force: true,
        })
        .await
        .expect("forced refresh")
        .into_inner();
    assert!(forced.refreshed);
    assert!(!format!("{forced:?}").contains("integration-access"));
    assert!(!format!("{forced:?}").contains("seeded-refresh"));

    // The refresh really replaced the stored access token.
    let stored = fixtures().store.load(&key).unwrap().unwrap();
    assert_eq!(
        stored.access_token.as_ref().map(Secret::expose),
        Some("ya29.integration-access")
    );

    server.shutdown().await;
}

#[tokio::test]
async fn refresh_token_on_a_non_oauth_account_is_a_failed_precondition() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let id = make_account(&mut client, "password-account").await;

    let status = client
        .refresh_token(RefreshTokenRequest {
            account_id: id,
            force: false,
        })
        .await
        .expect_err("not an OAuth account");
    assert_eq!(status.code(), Code::FailedPrecondition);

    let missing = client
        .refresh_token(RefreshTokenRequest {
            account_id: 9_999,
            force: false,
        })
        .await
        .expect_err("no such account");
    assert_eq!(missing.code(), Code::NotFound);

    server.shutdown().await;
}

/// An OAuth account that was never authorized: the error must name the verb
/// that fixes it rather than looking like something worth retrying.
#[tokio::test]
async fn refresh_token_without_a_stored_grant_says_how_to_authorize() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let id = make_account(&mut client, "never-authorized").await;
    point_account_at(&server, id, "rmail-oauth-nothing-here").await;

    let status = client
        .refresh_token(RefreshTokenRequest {
            account_id: id,
            force: false,
        })
        .await
        .expect_err("nothing stored");
    assert_eq!(status.code(), Code::FailedPrecondition);
    assert!(
        status.message().contains("mail account login --oauth"),
        "{status:?}"
    );

    server.shutdown().await;
}

/// The acceptance criterion "re-consent on revocation", asserted where a user
/// actually meets it: a `RefreshToken` RPC against a grant the provider has
/// revoked must come back `UNAUTHENTICATED` telling them to authorize again —
/// not `UNAVAILABLE`, which a client would retry.
#[tokio::test]
async fn refresh_token_on_a_revoked_grant_is_unauthenticated_at_the_boundary() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let id = make_account(&mut client, "revoked").await;

    let service = format!("rmail-oauth-revoked-{id}");
    let key = StoreKey::new(service.clone(), "revoked@example.com");
    fixtures()
        .store
        .save(
            &key,
            &StoredTokens {
                provider: Provider::Google,
                client_id: "client-abc".to_owned(),
                client_secret: None,
                refresh_token: Secret::new(REVOKED_REFRESH),
                access_token: None,
                expires_at: 0,
                scopes: Vec::new(),
            },
        )
        .unwrap();
    point_account_at(&server, id, &service).await;

    let status = client
        .refresh_token(RefreshTokenRequest {
            account_id: id,
            force: false,
        })
        .await
        .expect_err("a revoked grant must fail");
    assert_eq!(status.code(), Code::Unauthenticated);
    assert!(
        status.message().contains("re-authorize")
            && status
                .message()
                .contains("mail account login --oauth google"),
        "{status:?}"
    );
    assert!(
        !status.message().contains(REVOKED_MARKER),
        "the refresh token leaked through the provider's error_description: {status:?}"
    );

    server.shutdown().await;
}

/// Point an existing account at an OAuth credential by writing the row
/// directly.
///
/// `CompleteOAuth` is the only RPC that does this, and using it here would
/// mean running a whole browser flow to set up a `RefreshToken` test.
async fn point_account_at(server: &TestServer, id: i64, service: &str) {
    let db = rmail_core::Database::open(&server.db_path).unwrap();
    let service = service.to_owned();
    db.write(move |c| {
        c.execute(
            "UPDATE accounts SET secret_kind = 'oauth', secret_ref = ?2 WHERE id = ?1",
            rusqlite::params![id, service],
        )
    })
    .await
    .unwrap();
}
