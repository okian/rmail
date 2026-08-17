//! What task 68 owes, proved against a real socket rather than a mocked
//! client — the same discipline `ai::provider`, `embed::voyage` and task 75's
//! sinks use, and the reason no test here can reach the network: every
//! endpoint is a `TcpListener` bound to `127.0.0.1:0` inside the test process.
//!
//! - **Nothing leaves without an opt-in**: a destination must exist, be
//!   enabled, and subscribe to the event.
//! - **The default payload is a notification, not the mail**: no body, no
//!   attachments, no recipients — until that destination is registered with
//!   `include_body`.
//! - **Redaction happens before the wire**, over the fields derived from
//!   message content.
//! - **HMAC**: the header verifies over `<timestamp>.<body>`, and an unsigned
//!   destination sends no signature rather than a fake one.
//! - **The URL policy bites**: https required, loopback exempt, userinfo
//!   refused, and a lookalike host is not loopback.
//! - **Redirects are not followed**, and the second server is never contacted.
//! - **The error paths**: a 500 retries, a hang times out and retries, a 404
//!   is terminal at once, and the attempt cap ends in `failed`.
//! - **Idempotency**: one row and one POST per `(destination, event)`.
//! - **Replay** is the only way out of `failed`, and it resends the frozen
//!   bytes.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::config::{AiPrivacy, HumanDuration, WebhooksConfig};
use crate::credential::CredentialSource;
use crate::events::{EventKind, EventLog, NewEvent, Retention};
use crate::repo;

// ---------------------------------------------------------------------------
// A loopback HTTP endpoint
// ---------------------------------------------------------------------------

/// What the mock answers with.
#[derive(Debug, Clone)]
enum Reply {
    /// A status and a tiny body.
    Status(u16),
    /// A redirect the client must not follow.
    Redirect { status: u16, location: String },
    /// Accept the request, record it, and never answer — the "destination
    /// that hangs" case.
    Hang,
}

/// One request the endpoint saw.
#[derive(Debug, Clone)]
struct Seen {
    headers: Vec<(String, String)>,
    body: String,
}

impl Seen {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or(serde_json::Value::Null)
    }
}

/// A recording HTTP endpoint on loopback.
struct Endpoint {
    url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Endpoint {
    /// Always answer `status`.
    async fn always(status: u16) -> Self {
        Self::queued(vec![Reply::Status(status)]).await
    }

    /// Answer each reply in turn, repeating the last once exhausted.
    async fn queued(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen: Arc<Mutex<Vec<Seen>>> = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        let replies = Arc::new(Mutex::new(VecDeque::from(replies)));
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let recorder = Arc::clone(&recorder);
                let reply = {
                    let mut queue = replies.lock().unwrap_or_else(PoisonError::into_inner);
                    if queue.len() > 1 {
                        queue.pop_front().unwrap_or(Reply::Status(500))
                    } else {
                        queue.front().cloned().unwrap_or(Reply::Status(500))
                    }
                };
                tokio::spawn(serve(stream, recorder, reply));
            }
        });
        Self {
            url: format!("http://{addr}/hook"),
            seen,
            task,
        }
    }

    fn requests(&self) -> Vec<Seen> {
        self.seen
            .lock()
            .map(|log| log.clone())
            .unwrap_or_else(|e| e.into_inner().clone())
    }

    fn count(&self) -> usize {
        self.requests().len()
    }
}

async fn serve(mut stream: TcpStream, recorder: Arc<Mutex<Vec<Seen>>>, reply: Reply) {
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    // Read the head.
    let head_end = loop {
        if let Some(idx) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break idx + 4;
        }
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
        }
    };
    let head = String::from_utf8_lossy(&raw[..head_end]).to_string();
    let mut headers = Vec::new();
    let mut length = 0usize;
    for line in head.lines().skip(1) {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let (name, value) = (name.trim().to_owned(), value.trim().to_owned());
        if name.eq_ignore_ascii_case("content-length") {
            length = value.parse().unwrap_or(0);
        }
        headers.push((name, value));
    }
    while raw.len() < head_end + length {
        match stream.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => raw.extend_from_slice(&buf[..n]),
        }
    }
    let body =
        String::from_utf8_lossy(&raw[head_end..(head_end + length).min(raw.len())]).to_string();
    if let Ok(mut log) = recorder.lock() {
        log.push(Seen { headers, body });
    }

    match reply {
        Reply::Status(status) => {
            let response = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                 Content-Length: 2\r\nConnection: close\r\n\r\n{{}}"
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.flush().await;
        }
        Reply::Redirect { status, location } => {
            let response = format!(
                "HTTP/1.1 {status} X\r\nLocation: {location}\r\n\
                 Content-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.flush().await;
        }
        Reply::Hang => {
            // Hold the connection open, answering nothing, until the test
            // process tears the task down.
            std::future::pending::<()>().await;
        }
    }
}

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    db: Database,
    events: EventLog,
    path: PathBuf,
    account_id: i64,
    inbox_id: i64,
    next_uid: AtomicI64,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.path.display())));
        }
    }
}

impl Fixture {
    async fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-webhooks-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", path.display())));
        }
        let db = Database::open(&path).unwrap();
        let (account_id, inbox_id) = db
            .write(|c| {
                let account_id = repo::insert_account(
                    c,
                    &repo::NewAccount {
                        name: "Personal".to_owned(),
                        ..Default::default()
                    },
                )?;
                let inbox_id = repo::insert_mailbox(
                    c,
                    &repo::NewMailbox {
                        account_id,
                        name: "INBOX".to_owned(),
                        ..Default::default()
                    },
                )?;
                Ok((account_id, inbox_id))
            })
            .await
            .unwrap();
        let events = EventLog::new(db.clone(), Retention::default());
        Self {
            db,
            events,
            path,
            account_id,
            inbox_id,
            next_uid: AtomicI64::new(1),
        }
    }

    async fn message(&self, subject: &str, body: &str) -> i64 {
        let uid = self.next_uid.fetch_add(1, Ordering::Relaxed);
        let (account_id, mailbox_id) = (self.account_id, self.inbox_id);
        let subject = subject.to_owned();
        let body = body.to_owned();
        self.db
            .write(move |c| {
                repo::insert_message(
                    c,
                    &repo::NewMessage {
                        account_id,
                        mailbox_id,
                        uid,
                        uidvalidity: 1,
                        subject: Some(subject),
                        from_addr: Some("ada@example.com".to_owned()),
                        from_name: Some("Ada Lovelace".to_owned()),
                        body_text: Some(body),
                        message_id: Some(format!("<msg-{uid}@example.com>")),
                        date: Some(1_700_000_000),
                        ..Default::default()
                    },
                )
            })
            .await
            .unwrap()
    }

    /// A stored AI artifact, so the enrichment tests exercise the *read* path
    /// rather than a provider call (there is no provider in these tests, on
    /// purpose — see `payload`'s module docs).
    async fn summary(&self, message_id: i64, tl_dr: &str, todos: &[&str]) {
        let account_id = self.account_id;
        let tl_dr = tl_dr.to_owned();
        let todos = serde_json::to_string(todos).unwrap();
        self.db
            .write(move |c| {
                c.execute(
                    "INSERT INTO ai_summaries
                       (message_id, account_id, model, pass, schema_version, tl_dr, todos)
                     VALUES (?1, ?2, 'claude-haiku-4-5', 'triage', 1, ?3, ?4)",
                    rusqlite::params![message_id, account_id, tl_dr, todos],
                )?;
                Ok(())
            })
            .await
            .unwrap();
    }

    async fn register(&self, new: NewDestination) -> Destination {
        store::register(&self.db, new).await.unwrap()
    }

    /// A destination pointed at `endpoint`, subscribed to new mail.
    fn destination(name: &str, endpoint: &Endpoint) -> NewDestination {
        NewDestination {
            name: name.to_owned(),
            url: endpoint.url.clone(),
            events: vec![HookEvent::OnNewMessage],
            ..NewDestination::default()
        }
    }

    /// A dispatcher whose backoff is zero, so consecutive `tick`s each make
    /// one attempt without any test having to wait out a real delay or
    /// rewrite `next_attempt_at` behind the code's back.
    fn dispatcher(&self, timeout: Duration) -> WebhookDispatcher {
        WebhookDispatcher::new(
            self.db.clone(),
            self.events.clone(),
            &WebhooksConfig {
                enabled: true,
                max_concurrency: 4,
                tick_interval: HumanDuration::new(Duration::from_millis(10)),
                delivery_timeout: HumanDuration::new(timeout),
                backoff_base: HumanDuration::new(Duration::ZERO),
                backoff_max: HumanDuration::new(Duration::ZERO),
                max_batch: 100,
            },
            AiPrivacy::default(),
        )
        .unwrap()
    }

    async fn delivery(&self, id: i64) -> Delivery {
        store::get_delivery(&self.db, id).await.unwrap()
    }

    async fn deliveries(&self) -> Vec<Delivery> {
        store::list_deliveries(&self.db, None, 1_000).await.unwrap()
    }

    async fn log_new_mail(&self, message_id: i64) -> i64 {
        self.events
            .append(
                NewEvent::new(EventKind::NewMail)
                    .account(self.account_id)
                    .mailbox(self.inbox_id)
                    .message(message_id),
            )
            .await
            .unwrap()
            .seq
    }
}

fn cancel() -> CancellationToken {
    CancellationToken::new()
}

/// Poll `probe` until it yields a value, up to a generous ceiling.
///
/// For the one test that drives the *spawned* loop rather than calling `tick`
/// directly. A spawned dispatcher ticks once immediately and then on its
/// interval, so which tick does the work is a scheduling detail; the only
/// sound assertion is about the state the queue settles in. The ceiling is
/// several hundred times the 10 ms tick interval this fixture configures, so
/// it fails as a genuine bug rather than as flake on a loaded machine.
async fn wait_until<F, Fut, T>(mut probe: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(value) = probe().await {
            return value;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the dispatcher never reached the expected state"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// ---------------------------------------------------------------------------
// URL policy
// ---------------------------------------------------------------------------

#[test]
fn https_is_required_off_loopback() {
    assert!(validate_url("https://hooks.example.com/services/abc").is_ok());
    let err = validate_url("http://hooks.example.com/services/abc").unwrap_err();
    assert_eq!(err.reason(), ErrorReason::InvalidArgument);
    assert!(
        err.to_string().contains("https"),
        "the error must say what the rule is: {err}"
    );
}

#[test]
fn plaintext_is_allowed_only_for_a_genuine_loopback_host() {
    for ok in [
        "http://127.0.0.1:8080/hook",
        "http://localhost:9999/hook",
        "http://[::1]:9999/hook",
        "http://127.9.9.9/hook",
    ] {
        assert!(validate_url(ok).is_ok(), "{ok} should be allowed");
    }
    // The lookalikes. Each of these resolves wherever somebody else's DNS
    // says, and a suffix/substring test would have accepted all four.
    for bad in [
        "http://localhost.attacker.example/hook",
        "http://notlocalhost/hook",
        "http://127.0.0.1.attacker.example/hook",
        "http://evil.example/localhost",
    ] {
        assert!(
            validate_url(bad).is_err(),
            "{bad} must not be treated as loopback"
        );
    }
}

#[test]
fn a_url_carrying_userinfo_is_refused() {
    let err = validate_url("https://user:hunter2@hooks.example.com/x").unwrap_err();
    assert_eq!(err.reason(), ErrorReason::InvalidArgument);
    assert!(
        !err.to_string().contains("hunter2"),
        "the refusal must not echo the credential it refused: {err}"
    );
}

#[test]
fn a_non_http_scheme_is_refused() {
    for bad in [
        "ftp://files.example.com/x",
        "file:///etc/passwd",
        "javascript:alert(1)",
        "not a url",
    ] {
        assert!(validate_url(bad).is_err(), "{bad} must be refused");
    }
}

#[test]
fn a_logged_url_keeps_the_authority_and_drops_the_secret_path() {
    // A Slack incoming-webhook URL *is* the credential.
    let logged = log_url("https://hooks.slack.com/services/T0000/B0000/XXXXSECRETXXXX");
    assert_eq!(logged, "https://hooks.slack.com");
    assert!(!logged.contains("XXXXSECRETXXXX"));
    assert_eq!(
        log_url("http://127.0.0.1:8080/hook?token=abc"),
        "http://127.0.0.1:8080"
    );
}

// ---------------------------------------------------------------------------
// Payload shape, minimization and redaction
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_default_payload_carries_no_body_even_when_the_message_has_one() {
    let fx = Fixture::open().await;
    let id = fx
        .message("Q3 planning", "the entire confidential body text")
        .await;
    let endpoint = Endpoint::always(200).await;
    let destination = fx.register(Fixture::destination("alerts", &endpoint)).await;
    assert!(!destination.include_body, "off by default");

    let delivery_id = enqueue_for_message(
        &fx.db,
        &destination,
        "on_new_message",
        "event:1",
        id,
        &AiPrivacy::default(),
    )
    .await
    .unwrap()
    .unwrap();

    let body = fx.delivery(delivery_id).await.payload;
    assert!(
        !body.contains("confidential"),
        "a default destination must not receive the body: {body}"
    );
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert!(json["message"]["body"].is_null());
    // ...and it does carry what makes the notification useful.
    assert_eq!(json["message"]["subject"], "Q3 planning");
    assert_eq!(json["message"]["from"], "Ada Lovelace <ada@example.com>");
    assert_eq!(json["message"]["link"], format!("rmail://message/{id}"));
    assert_eq!(json["message"]["account"], "Personal");
    assert_eq!(json["message"]["mailbox"], "INBOX");
    assert_eq!(json["event"], "on_new_message");
    assert_eq!(json["delivery_id"], delivery_id);
}

/// The renderer's own `include_body` gate, exercised directly.
///
/// The end-to-end test above cannot cover it: `store::facts_for` gates the
/// *query*, so on that path `facts.body` is already `None` and the renderer
/// has nothing to leak whether or not it checks. That layering is deliberate
/// (see `store::facts_for`) but it means removing the check in
/// [`payload::build`] breaks nothing anybody would notice — until some future
/// caller assembles `MessageFacts` from somewhere other than that query. This
/// is the test that fails when the second gate goes.
#[test]
fn the_renderer_drops_a_body_it_was_handed_but_not_entitled_to() {
    let facts = MessageFacts {
        message_id: 7,
        account: "Personal".to_owned(),
        mailbox: "INBOX".to_owned(),
        from: "ada@example.com".to_owned(),
        subject: "Q3 planning".to_owned(),
        body: Some("the entire confidential body text".to_owned()),
        ..MessageFacts::default()
    };
    let withheld = payload::build(
        Template::Generic,
        "on_new_message",
        1,
        &facts,
        false,
        &AiPrivacy::default(),
    );
    assert!(withheld["message"]["body"].is_null());
    assert!(
        !withheld.to_string().contains("confidential"),
        "the renderer must not emit a body for a destination without include_body: {withheld}"
    );

    // ...and it does emit one when the destination is entitled to it, so the
    // assertion above is about the gate rather than about a field that never
    // renders.
    let allowed = payload::build(
        Template::Generic,
        "on_new_message",
        1,
        &facts,
        true,
        &AiPrivacy::default(),
    );
    assert_eq!(
        allowed["message"]["body"],
        "the entire confidential body text"
    );
}

#[tokio::test]
async fn a_destination_registered_for_bodies_gets_one() {
    let fx = Fixture::open().await;
    let id = fx.message("Q3 planning", "the entire body text").await;
    let endpoint = Endpoint::always(200).await;
    let destination = fx
        .register(NewDestination {
            include_body: true,
            ..Fixture::destination("alerts", &endpoint)
        })
        .await;

    let delivery_id = enqueue_for_message(
        &fx.db,
        &destination,
        "on_new_message",
        "event:1",
        id,
        &AiPrivacy::default(),
    )
    .await
    .unwrap()
    .unwrap();
    let json: serde_json::Value =
        serde_json::from_str(&fx.delivery(delivery_id).await.payload).unwrap();
    assert_eq!(json["message"]["body"], "the entire body text");
}

#[tokio::test]
async fn content_is_redacted_before_it_leaves_but_the_sender_is_not() {
    let fx = Fixture::open().await;
    // The realistic case: a one-time code in a subject line, on its way to a
    // chat channel.
    let id = fx
        .message(
            "Your verification code is 815234",
            "card 4111111111111111 is on file",
        )
        .await;
    let endpoint = Endpoint::always(200).await;
    let destination = fx
        .register(NewDestination {
            include_body: true,
            ..Fixture::destination("alerts", &endpoint)
        })
        .await;

    let delivery_id = enqueue_for_message(
        &fx.db,
        &destination,
        "on_new_message",
        "event:1",
        id,
        &AiPrivacy::default(),
    )
    .await
    .unwrap()
    .unwrap();
    let payload = fx.delivery(delivery_id).await.payload;
    assert!(
        !payload.contains("815234"),
        "an OTP must not reach a third party: {payload}"
    );
    assert!(
        !payload.contains("4111111111111111"),
        "a card number must not reach a third party: {payload}"
    );
    // The documented exemption: the sender is the fact the notification is
    // about, and `payload`'s module docs say so explicitly.
    assert!(
        payload.contains("ada@example.com"),
        "the sender is deliberately not redacted: {payload}"
    );
}

#[tokio::test]
async fn the_payload_carries_the_stored_summary_and_action_items_two_sentences_of_it() {
    let fx = Fixture::open().await;
    let id = fx.message("Launch", "body").await;
    fx.summary(
        id,
        "The launch slipped a week. Marketing needs new copy by Friday. A third \
         sentence nobody asked for.",
        &["send the copy", "book the review"],
    )
    .await;
    let endpoint = Endpoint::always(200).await;
    let destination = fx
        .register(NewDestination {
            template: Template::Slack,
            ..Fixture::destination("alerts", &endpoint)
        })
        .await;

    let delivery_id = enqueue_for_message(
        &fx.db,
        &destination,
        "on_new_message",
        "event:1",
        id,
        &AiPrivacy::default(),
    )
    .await
    .unwrap()
    .unwrap();
    let json: serde_json::Value =
        serde_json::from_str(&fx.delivery(delivery_id).await.payload).unwrap();
    let summary = json["message"]["summary"].as_str().unwrap();
    assert!(summary.starts_with("The launch slipped a week."));
    assert!(
        !summary.contains("third sentence"),
        "prd.md asks for two sentences: {summary}"
    );
    assert_eq!(
        json["message"]["action_items"],
        serde_json::json!(["send the copy", "book the review"])
    );
    // The Slack shape: a rendered `text` carrying the summary, the items and
    // the deep link.
    let text = json["text"].as_str().unwrap();
    assert!(text.contains("Ada Lovelace"));
    assert!(text.contains("send the copy"));
    assert!(text.contains(&format!("rmail://message/{id}")));
}

/// The sender's display name is exempt from *redaction* and emphatically not
/// from *sanitization*.
///
/// `From:` is attacker-authored and RFC 2047 lets it decode to anything,
/// newlines included. Without the `clean` in `payload::build`, a display name
/// carrying `\n• ...` renders in a Slack channel as extra lines the operator
/// reads as rmail's own output — a fake action item, a fake link line — and an
/// unbounded one becomes an unbounded request body and an unbounded stored row.
#[test]
fn a_sender_controlled_newline_cannot_forge_lines_in_the_rendered_text() {
    let facts = MessageFacts {
        message_id: 3,
        from: "Ada\n• Approved by finance\n• Wire to acct 4455 <ada@example.com>".to_owned(),
        subject: "Invoice".to_owned(),
        rfc_message_id: Some("<a\nb@example.com>".to_owned()),
        ..MessageFacts::default()
    };
    let built = payload::build(
        Template::Slack,
        "on_new_message",
        1,
        &facts,
        false,
        &AiPrivacy::default(),
    );
    let from = built["message"]["from"].as_str().unwrap();
    assert!(!from.contains('\n'), "from carries a raw newline: {from:?}");
    assert!(!built["message"]["rfc_message_id"]
        .as_str()
        .unwrap()
        .contains('\n'));

    let text = built["text"].as_str().unwrap();
    // The rendered text has exactly the lines this payload puts there: the
    // header line and the deep-link line. No summary and no action items were
    // supplied, so anything more came from the sender.
    assert_eq!(
        text.lines().count(),
        2,
        "a sender must not be able to add lines to the rendered message: {text:?}"
    );

    // ...and the bound bites.
    let long = MessageFacts {
        from: "x".repeat(10_000),
        ..MessageFacts::default()
    };
    let bounded = payload::build(
        Template::Generic,
        "on_new_message",
        1,
        &long,
        false,
        &AiPrivacy::default(),
    );
    assert_eq!(
        bounded["message"]["from"].as_str().unwrap().chars().count(),
        payload::MAX_FROM_CHARS
    );
}

#[test]
fn slack_control_characters_in_a_subject_cannot_become_markup() {
    let escaped = payload::slack_escape("<https://evil.example|click here> & <b>");
    assert!(!escaped.contains('<'), "{escaped}");
    assert!(!escaped.contains('>'), "{escaped}");
    assert_eq!(
        escaped,
        "&lt;https://evil.example|click here&gt; &amp; &lt;b&gt;"
    );
}

#[test]
fn two_sentences_stops_at_the_second_terminator() {
    assert_eq!(payload::two_sentences("One. Two. Three."), "One. Two.");
    assert_eq!(payload::two_sentences("Only one."), "Only one.");
    assert_eq!(payload::two_sentences("No terminator"), "No terminator");
    assert_eq!(payload::two_sentences("Q? A! More."), "Q? A!");
    // A multi-byte tail must not be sliced mid-character.
    assert_eq!(payload::two_sentences("Ünï. Töö. Drei."), "Ünï. Töö.");
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

#[tokio::test]
async fn register_list_and_remove_round_trip() {
    let fx = Fixture::open().await;
    let endpoint = Endpoint::always(200).await;
    let registered = fx
        .register(NewDestination {
            secret: CredentialSource::Env("RMAIL_TEST_KEY".to_owned()),
            events: vec![HookEvent::OnNewMessage, HookEvent::OnRuleMatch],
            template: Template::Slack,
            max_attempts: 3,
            ..Fixture::destination("alerts", &endpoint)
        })
        .await;
    assert_eq!(registered.name, "alerts");
    assert_eq!(registered.template, Template::Slack);
    assert_eq!(
        registered.events,
        vec![HookEvent::OnNewMessage, HookEvent::OnRuleMatch]
    );
    assert_eq!(
        registered.secret,
        CredentialSource::Env("RMAIL_TEST_KEY".to_owned())
    );

    let listed = store::list(&fx.db).await.unwrap();
    assert_eq!(listed, vec![registered.clone()]);
    assert_eq!(
        store::get_by_name(&fx.db, "alerts").await.unwrap(),
        registered
    );

    assert!(store::remove(&fx.db, "alerts").await.unwrap());
    assert!(store::list(&fx.db).await.unwrap().is_empty());
    // Idempotent: removing again is `false`, not an error.
    assert!(!store::remove(&fx.db, "alerts").await.unwrap());
    assert_eq!(
        store::get_by_name(&fx.db, "alerts")
            .await
            .unwrap_err()
            .reason(),
        ErrorReason::NotFound
    );
}

#[tokio::test]
async fn a_duplicate_name_is_refused_and_a_bad_url_never_reaches_the_table() {
    let fx = Fixture::open().await;
    let endpoint = Endpoint::always(200).await;
    fx.register(Fixture::destination("alerts", &endpoint)).await;

    let dup = store::register(&fx.db, Fixture::destination("alerts", &endpoint))
        .await
        .unwrap_err();
    assert_eq!(dup.reason(), ErrorReason::AlreadyExists);

    let bad = store::register(
        &fx.db,
        NewDestination {
            name: "plaintext".to_owned(),
            url: "http://hooks.example.com/x".to_owned(),
            ..NewDestination::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(bad.reason(), ErrorReason::InvalidArgument);
    assert_eq!(
        store::list(&fx.db).await.unwrap().len(),
        1,
        "a refused registration must not leave a row behind"
    );
}

#[tokio::test]
async fn an_oauth_credential_source_is_refused_as_a_signing_key() {
    let fx = Fixture::open().await;
    let endpoint = Endpoint::always(200).await;
    let err = store::register(
        &fx.db,
        NewDestination {
            secret: CredentialSource::OAuth("service".to_owned()),
            ..Fixture::destination("alerts", &endpoint)
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err.reason(), ErrorReason::InvalidArgument);
}

// ---------------------------------------------------------------------------
// The queue: idempotency, retries, the cap, replay
// ---------------------------------------------------------------------------

#[tokio::test]
async fn one_event_produces_one_row_however_many_times_it_is_offered() {
    let fx = Fixture::open().await;
    let id = fx.message("Hello", "body").await;
    let endpoint = Endpoint::always(200).await;
    let destination = fx.register(Fixture::destination("alerts", &endpoint)).await;

    let first = enqueue_for_message(
        &fx.db,
        &destination,
        "on_new_message",
        "event:7",
        id,
        &AiPrivacy::default(),
    )
    .await
    .unwrap();
    assert!(first.is_some());
    for _ in 0..5 {
        assert_eq!(
            enqueue_for_message(
                &fx.db,
                &destination,
                "on_new_message",
                "event:7",
                id,
                &AiPrivacy::default(),
            )
            .await
            .unwrap(),
            None,
            "the UNIQUE fence, not the process, decides who was first"
        );
    }
    assert_eq!(fx.deliveries().await.len(), 1);
}

#[tokio::test]
async fn a_delivery_is_signed_over_timestamp_dot_body_and_carries_its_dedupe_id() {
    // SAFETY: `set_var` is process-global. This test names its own variable
    // and no other test in this module reads it.
    unsafe { std::env::set_var("RMAIL_TEST_WEBHOOK_KEY", "test-signing-key-not-a-secret") };
    let fx = Fixture::open().await;
    let id = fx.message("Signed", "body").await;
    let endpoint = Endpoint::always(200).await;
    let destination = fx
        .register(NewDestination {
            secret: CredentialSource::Env("RMAIL_TEST_WEBHOOK_KEY".to_owned()),
            ..Fixture::destination("alerts", &endpoint)
        })
        .await;
    let delivery_id = enqueue_for_message(
        &fx.db,
        &destination,
        "on_new_message",
        "event:1",
        id,
        &AiPrivacy::default(),
    )
    .await
    .unwrap()
    .unwrap();

    let report = fx
        .dispatcher(Duration::from_secs(5))
        .tick(&cancel())
        .await
        .unwrap();
    assert_eq!(report.delivered, 1);
    assert_eq!(
        fx.delivery(delivery_id).await.state,
        DeliveryState::Delivered
    );

    let requests = endpoint.requests();
    assert_eq!(requests.len(), 1);
    let seen = &requests[0];
    assert_eq!(
        seen.header(sign::DELIVERY_HEADER),
        Some(delivery_id.to_string().as_str())
    );
    assert_eq!(seen.header(sign::EVENT_HEADER), Some("on_new_message"));
    let timestamp: i64 = seen
        .header(sign::TIMESTAMP_HEADER)
        .unwrap()
        .parse()
        .unwrap();
    let signature = seen.header(sign::SIGNATURE_HEADER).unwrap();
    assert!(
        sign::verify(
            &crate::credential::Secret::new("test-signing-key-not-a-secret"),
            timestamp,
            seen.body.as_bytes(),
            signature,
        ),
        "the receiver must be able to verify what it was sent"
    );
    // The key itself is never on the wire.
    assert!(!seen.body.contains("test-signing-key-not-a-secret"));
    for (_, value) in &seen.headers {
        assert!(!value.contains("test-signing-key-not-a-secret"));
    }
}

#[tokio::test]
async fn an_unsigned_destination_sends_no_signature_rather_than_a_fake_one() {
    let fx = Fixture::open().await;
    let id = fx.message("Unsigned", "body").await;
    let endpoint = Endpoint::always(200).await;
    let destination = fx.register(Fixture::destination("alerts", &endpoint)).await;
    enqueue_for_message(
        &fx.db,
        &destination,
        "on_new_message",
        "event:1",
        id,
        &AiPrivacy::default(),
    )
    .await
    .unwrap();
    fx.dispatcher(Duration::from_secs(5))
        .tick(&cancel())
        .await
        .unwrap();

    let requests = endpoint.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].header(sign::SIGNATURE_HEADER), None);
    assert!(requests[0].header(sign::TIMESTAMP_HEADER).is_some());
}

#[tokio::test]
async fn an_endpoint_that_500s_is_retried_until_the_cap_then_fails_terminally() {
    let fx = Fixture::open().await;
    let id = fx.message("Broken", "body").await;
    let endpoint = Endpoint::always(500).await;
    let destination = fx
        .register(NewDestination {
            max_attempts: 3,
            ..Fixture::destination("alerts", &endpoint)
        })
        .await;
    let delivery_id = enqueue_for_message(
        &fx.db,
        &destination,
        "on_new_message",
        "event:1",
        id,
        &AiPrivacy::default(),
    )
    .await
    .unwrap()
    .unwrap();
    let dispatcher = fx.dispatcher(Duration::from_secs(5));

    for attempt in 1..=2 {
        let report = dispatcher.tick(&cancel()).await.unwrap();
        assert_eq!(report.deferred, 1, "attempt {attempt} should defer");
        let row = fx.delivery(delivery_id).await;
        assert_eq!(row.state, DeliveryState::Pending);
        assert_eq!(row.attempts, attempt);
        assert_eq!(row.last_status, Some(500));
    }
    // The third attempt is the last one allowed.
    let report = dispatcher.tick(&cancel()).await.unwrap();
    assert_eq!(report.failed, 1);
    let row = fx.delivery(delivery_id).await;
    assert_eq!(row.state, DeliveryState::Failed);
    assert_eq!(row.attempts, 3);
    assert!(row.last_error.unwrap_or_default().contains("exhausted"));
    assert_eq!(endpoint.count(), 3, "exactly the cap, never more");

    // Terminal: a further tick does not touch it.
    let report = dispatcher.tick(&cancel()).await.unwrap();
    assert_eq!(report.attempted, 0);
    assert_eq!(endpoint.count(), 3);
}

#[tokio::test]
async fn an_endpoint_that_hangs_times_out_and_is_retried() {
    let fx = Fixture::open().await;
    let id = fx.message("Hangs", "body").await;
    let endpoint = Endpoint::queued(vec![Reply::Hang]).await;
    let destination = fx
        .register(NewDestination {
            max_attempts: 2,
            ..Fixture::destination("alerts", &endpoint)
        })
        .await;
    let delivery_id = enqueue_for_message(
        &fx.db,
        &destination,
        "on_new_message",
        "event:1",
        id,
        &AiPrivacy::default(),
    )
    .await
    .unwrap()
    .unwrap();
    let dispatcher = fx.dispatcher(Duration::from_millis(250));

    let report = dispatcher.tick(&cancel()).await.unwrap();
    assert_eq!(report.deferred, 1);
    let row = fx.delivery(delivery_id).await;
    assert_eq!(row.state, DeliveryState::Pending);
    assert_eq!(
        row.last_status, None,
        "a peer that never answered must not look like it returned a status"
    );
    assert!(row.last_error.unwrap_or_default().contains("timed out"));

    let report = dispatcher.tick(&cancel()).await.unwrap();
    assert_eq!(report.failed, 1);
    assert_eq!(fx.delivery(delivery_id).await.state, DeliveryState::Failed);
}

#[tokio::test]
async fn a_redirect_is_never_followed_and_is_terminal_at_once() {
    let fx = Fixture::open().await;
    let id = fx.message("Redirected", "body").await;
    // The endpoint an attacker would want the daemon walked to. It answers
    // 200, so following the redirect would look like success.
    let attacker = Endpoint::always(200).await;
    let endpoint = Endpoint::queued(vec![Reply::Redirect {
        status: 302,
        location: attacker.url.clone(),
    }])
    .await;
    let destination = fx
        .register(NewDestination {
            max_attempts: 5,
            ..Fixture::destination("alerts", &endpoint)
        })
        .await;
    let delivery_id = enqueue_for_message(
        &fx.db,
        &destination,
        "on_new_message",
        "event:1",
        id,
        &AiPrivacy::default(),
    )
    .await
    .unwrap()
    .unwrap();

    let report = fx
        .dispatcher(Duration::from_secs(5))
        .tick(&cancel())
        .await
        .unwrap();
    assert_eq!(report.failed, 1);
    let row = fx.delivery(delivery_id).await;
    assert_eq!(row.state, DeliveryState::Failed);
    assert_eq!(row.attempts, 1, "a redirect is not worth retrying");
    assert_eq!(row.last_status, Some(302));
    assert_eq!(
        attacker.count(),
        0,
        "the daemon must not be walked to a second host carrying mail content"
    );
}

#[tokio::test]
async fn a_4xx_is_terminal_without_burning_the_whole_cap() {
    let fx = Fixture::open().await;
    let id = fx.message("Gone", "body").await;
    let endpoint = Endpoint::always(404).await;
    let destination = fx
        .register(NewDestination {
            max_attempts: 5,
            ..Fixture::destination("alerts", &endpoint)
        })
        .await;
    let delivery_id = enqueue_for_message(
        &fx.db,
        &destination,
        "on_new_message",
        "event:1",
        id,
        &AiPrivacy::default(),
    )
    .await
    .unwrap()
    .unwrap();
    fx.dispatcher(Duration::from_secs(5))
        .tick(&cancel())
        .await
        .unwrap();
    let row = fx.delivery(delivery_id).await;
    assert_eq!(row.state, DeliveryState::Failed);
    assert_eq!(row.attempts, 1);
    assert_eq!(endpoint.count(), 1);
}

#[tokio::test]
async fn a_429_is_retried_rather_than_given_up_on() {
    let fx = Fixture::open().await;
    let id = fx.message("Slow down", "body").await;
    let endpoint = Endpoint::queued(vec![Reply::Status(429), Reply::Status(200)]).await;
    let destination = fx
        .register(NewDestination {
            max_attempts: 5,
            ..Fixture::destination("alerts", &endpoint)
        })
        .await;
    let delivery_id = enqueue_for_message(
        &fx.db,
        &destination,
        "on_new_message",
        "event:1",
        id,
        &AiPrivacy::default(),
    )
    .await
    .unwrap()
    .unwrap();
    let dispatcher = fx.dispatcher(Duration::from_secs(5));
    assert_eq!(dispatcher.tick(&cancel()).await.unwrap().deferred, 1);
    assert_eq!(dispatcher.tick(&cancel()).await.unwrap().delivered, 1);
    assert_eq!(
        fx.delivery(delivery_id).await.state,
        DeliveryState::Delivered
    );
}

#[tokio::test]
async fn a_retry_resends_the_bytes_the_first_attempt_sent() {
    let fx = Fixture::open().await;
    let id = fx.message("Frozen", "body").await;
    let endpoint = Endpoint::queued(vec![Reply::Status(503), Reply::Status(200)]).await;
    let destination = fx
        .register(NewDestination {
            max_attempts: 5,
            ..Fixture::destination("alerts", &endpoint)
        })
        .await;
    enqueue_for_message(
        &fx.db,
        &destination,
        "on_new_message",
        "event:1",
        id,
        &AiPrivacy::default(),
    )
    .await
    .unwrap();
    let dispatcher = fx.dispatcher(Duration::from_secs(5));
    dispatcher.tick(&cancel()).await.unwrap();
    // Change the message underneath the queue. A re-render would pick this
    // up; a frozen payload must not.
    fx.db
        .write(move |c| {
            c.execute(
                "UPDATE messages SET subject = 'CHANGED' WHERE id = ?1",
                [id],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    dispatcher.tick(&cancel()).await.unwrap();

    let requests = endpoint.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0].body, requests[1].body,
        "a retry must transmit what the first attempt transmitted"
    );
    assert!(!requests[1].body.contains("CHANGED"));
}

#[tokio::test]
async fn replay_is_the_way_out_of_failed_and_resends_the_frozen_body() {
    let fx = Fixture::open().await;
    let id = fx.message("Replayed", "body").await;
    let endpoint = Endpoint::queued(vec![Reply::Status(404), Reply::Status(200)]).await;
    let destination = fx.register(Fixture::destination("alerts", &endpoint)).await;
    let delivery_id = enqueue_for_message(
        &fx.db,
        &destination,
        "on_new_message",
        "event:1",
        id,
        &AiPrivacy::default(),
    )
    .await
    .unwrap()
    .unwrap();
    let dispatcher = fx.dispatcher(Duration::from_secs(5));
    dispatcher.tick(&cancel()).await.unwrap();
    assert_eq!(fx.delivery(delivery_id).await.state, DeliveryState::Failed);

    let replayed = store::replay(&fx.db, delivery_id).await.unwrap();
    assert_eq!(replayed.state, DeliveryState::Pending);
    assert_eq!(replayed.attempts, 0);

    assert_eq!(dispatcher.tick(&cancel()).await.unwrap().delivered, 1);
    let requests = endpoint.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].body, requests[1].body);
    assert_eq!(
        requests[0].header(sign::DELIVERY_HEADER),
        requests[1].header(sign::DELIVERY_HEADER),
        "a replay keeps the id a receiver dedupes on"
    );

    assert_eq!(
        store::replay(&fx.db, 99_999).await.unwrap_err().reason(),
        ErrorReason::NotFound
    );
}

// ---------------------------------------------------------------------------
// The dispatcher: who gets what
// ---------------------------------------------------------------------------

#[tokio::test]
async fn only_an_enabled_subscribed_destination_receives_an_event() {
    let fx = Fixture::open().await;
    let subscribed = Endpoint::always(200).await;
    let disabled = Endpoint::always(200).await;
    let unsubscribed = Endpoint::always(200).await;
    let forward_only = Endpoint::always(200).await;

    fx.register(Fixture::destination("subscribed", &subscribed))
        .await;
    fx.register(NewDestination {
        enabled: false,
        ..Fixture::destination("disabled", &disabled)
    })
    .await;
    fx.register(NewDestination {
        events: vec![HookEvent::OnRuleMatch],
        ..Fixture::destination("unsubscribed", &unsubscribed)
    })
    .await;
    fx.register(NewDestination {
        events: Vec::new(),
        ..Fixture::destination("forward-only", &forward_only)
    })
    .await;

    let dispatcher = fx.dispatcher(Duration::from_secs(5));
    // Seed the cursor at the current head, then log a new message.
    dispatcher.tick(&cancel()).await.unwrap();
    let id = fx.message("Arrived", "body").await;
    fx.log_new_mail(id).await;

    let report = dispatcher.tick(&cancel()).await.unwrap();
    assert_eq!(report.queued, 1);
    assert_eq!(report.delivered, 1);
    assert_eq!(subscribed.count(), 1);
    assert_eq!(
        disabled.count(),
        0,
        "a disabled destination receives nothing"
    );
    assert_eq!(unsubscribed.count(), 0);
    assert_eq!(
        forward_only.count(),
        0,
        "an empty subscription is a destination that only receives explicit forwards"
    );
}

#[tokio::test]
async fn disabling_a_destination_holds_its_queued_deliveries_and_enabling_resumes_them() {
    let fx = Fixture::open().await;
    let id = fx.message("Held", "body").await;
    let endpoint = Endpoint::always(200).await;
    let destination = fx.register(Fixture::destination("alerts", &endpoint)).await;
    let delivery_id = enqueue_for_message(
        &fx.db,
        &destination,
        "on_new_message",
        "event:1",
        id,
        &AiPrivacy::default(),
    )
    .await
    .unwrap()
    .unwrap();

    // Disabled after the delivery was already queued — the case the
    // enqueue-time filter alone cannot cover.
    let disabled = store::set_enabled(&fx.db, "alerts", false).await.unwrap();
    assert!(!disabled.enabled);

    let dispatcher = fx.dispatcher(Duration::from_secs(5));
    let report = dispatcher.tick(&cancel()).await.unwrap();
    assert_eq!(report.attempted, 0);
    assert_eq!(
        endpoint.count(),
        0,
        "a disabled destination receives nothing"
    );
    let row = fx.delivery(delivery_id).await;
    assert_eq!(
        row.state,
        DeliveryState::Pending,
        "held, not discarded and not failed"
    );
    assert_eq!(row.attempts, 0, "and no attempt was spent holding it");

    // Enabling resumes the same delivery rather than replaying anything.
    assert!(
        store::set_enabled(&fx.db, "alerts", true)
            .await
            .unwrap()
            .enabled
    );
    assert_eq!(dispatcher.tick(&cancel()).await.unwrap().delivered, 1);
    assert_eq!(endpoint.count(), 1);
    assert_eq!(
        fx.delivery(delivery_id).await.state,
        DeliveryState::Delivered
    );

    assert_eq!(
        store::set_enabled(&fx.db, "nope", true)
            .await
            .unwrap_err()
            .reason(),
        ErrorReason::NotFound
    );
}

#[tokio::test]
async fn with_no_destination_registered_nothing_leaves_the_machine() {
    let fx = Fixture::open().await;
    let dispatcher = fx.dispatcher(Duration::from_secs(5));
    dispatcher.tick(&cancel()).await.unwrap();
    let id = fx.message("Arrived", "body").await;
    fx.log_new_mail(id).await;
    let report = dispatcher.tick(&cancel()).await.unwrap();
    assert_eq!(report.queued, 0);
    assert_eq!(report.attempted, 0);
    assert!(fx.deliveries().await.is_empty());
}

#[tokio::test]
async fn history_before_a_dispatcher_started_is_not_replayed_at_a_new_endpoint() {
    let fx = Fixture::open().await;
    // Mail that arrived before anybody registered anything.
    for n in 0..3 {
        let id = fx.message(&format!("old {n}"), "body").await;
        fx.log_new_mail(id).await;
    }
    let endpoint = Endpoint::always(200).await;
    fx.register(Fixture::destination("alerts", &endpoint)).await;

    // Driven by hand rather than by the spawned loop, so the assertion is
    // about *which* events a cursor seeded at the head picks up rather than
    // about who won a race. `spawn`'s own eager seed is covered separately
    // below.
    let dispatcher = fx.dispatcher(Duration::from_secs(5));
    let first = dispatcher.tick(&cancel()).await.unwrap();
    assert_eq!(
        first.queued, 0,
        "a fresh cursor seeds at the log's head; the backlog is history"
    );
    assert_eq!(endpoint.count(), 0);

    let id = fx.message("new", "body").await;
    fx.log_new_mail(id).await;
    let second = dispatcher.tick(&cancel()).await.unwrap();
    assert_eq!(
        second.queued, 1,
        "only the event after the cursor was seeded"
    );
    let deliveries = fx.deliveries().await;
    assert_eq!(deliveries.len(), 1);
    assert_eq!(deliveries[0].message_id, Some(id));
}

#[tokio::test]
async fn a_spawned_dispatcher_seeds_its_cursor_before_spawn_returns() {
    let fx = Fixture::open().await;
    for n in 0..3 {
        let id = fx.message(&format!("old {n}"), "body").await;
        fx.log_new_mail(id).await;
    }
    let endpoint = Endpoint::always(200).await;
    fx.register(Fixture::destination("alerts", &endpoint)).await;

    let dispatcher = fx.dispatcher(Duration::from_secs(5));
    let token = cancel();
    let handle = dispatcher.spawn(token.clone()).await;
    // Appended strictly *after* `spawn` returned, which is the guarantee the
    // eager seed exists to make: everything from here forward is dispatched,
    // everything before it is history.
    let id = fx.message("new", "body").await;
    fx.log_new_mail(id).await;

    // Assert the state the queue settles in, never the return value of a call
    // racing the loop's own tick: the loop ticks once immediately on spawn and
    // then on its interval, so either tick may be the one that picks this up.
    let settled = wait_until(|| async {
        let deliveries = fx.deliveries().await;
        (!deliveries.is_empty()).then_some(deliveries)
    })
    .await;
    token.cancel();
    let _ = handle.await;

    assert_eq!(
        settled.len(),
        1,
        "the three events logged before spawn must never be delivered"
    );
    assert_eq!(settled[0].message_id, Some(id));
}

#[tokio::test]
async fn a_destination_registered_after_an_event_does_not_get_it() {
    let fx = Fixture::open().await;
    let dispatcher = fx.dispatcher(Duration::from_secs(5));
    dispatcher.tick(&cancel()).await.unwrap();
    let id = fx.message("before", "body").await;
    fx.log_new_mail(id).await;
    // The tick that passes the event by, with nothing registered.
    dispatcher.tick(&cancel()).await.unwrap();

    let endpoint = Endpoint::always(200).await;
    fx.register(Fixture::destination("late", &endpoint)).await;
    let report = dispatcher.tick(&cancel()).await.unwrap();
    assert_eq!(report.queued, 0);
    assert_eq!(endpoint.count(), 0);
}

// ---------------------------------------------------------------------------
// Forward
// ---------------------------------------------------------------------------

#[tokio::test]
async fn forward_queues_a_summary_action_items_and_a_deep_link() {
    let fx = Fixture::open().await;
    let id = fx.message("Contract review", "body").await;
    fx.summary(
        id,
        "Legal wants the redlines back. They will sign on Monday.",
        &["return redlines"],
    )
    .await;
    let endpoint = Endpoint::always(200).await;
    fx.register(NewDestination {
        template: Template::Slack,
        events: Vec::new(),
        ..Fixture::destination("eng-alerts", &endpoint)
    })
    .await;

    let delivery_id = forward(
        &fx.db,
        "eng-alerts",
        id,
        &AiPrivacy::default(),
        1_700_000_000,
    )
    .await
    .unwrap();
    let row = fx.delivery(delivery_id).await;
    assert_eq!(row.event, FORWARD_EVENT);
    assert_eq!(row.event_key, format!("forward:{id}:1700000000"));

    assert_eq!(
        fx.dispatcher(Duration::from_secs(5))
            .tick(&cancel())
            .await
            .unwrap()
            .delivered,
        1
    );
    let requests = endpoint.requests();
    assert_eq!(requests.len(), 1);
    let json = requests[0].json();
    let text = json["text"].as_str().unwrap();
    assert!(text.contains("Legal wants the redlines back."));
    assert!(text.contains("return redlines"));
    assert!(text.contains(&format!("rmail://message/{id}")));
    assert_eq!(json["event"], FORWARD_EVENT);
}

#[tokio::test]
async fn forward_refuses_an_unknown_or_disabled_destination() {
    let fx = Fixture::open().await;
    let id = fx.message("x", "body").await;
    let endpoint = Endpoint::always(200).await;

    assert_eq!(
        forward(&fx.db, "nope", id, &AiPrivacy::default(), 1)
            .await
            .unwrap_err()
            .reason(),
        ErrorReason::NotFound
    );

    fx.register(NewDestination {
        enabled: false,
        ..Fixture::destination("off", &endpoint)
    })
    .await;
    assert_eq!(
        forward(&fx.db, "off", id, &AiPrivacy::default(), 1)
            .await
            .unwrap_err()
            .reason(),
        ErrorReason::FailedPrecondition
    );
    assert!(fx.deliveries().await.is_empty());
    assert_eq!(endpoint.count(), 0);
}

#[tokio::test]
async fn a_double_clicked_forward_collapses_to_one_delivery() {
    let fx = Fixture::open().await;
    let id = fx.message("x", "body").await;
    let endpoint = Endpoint::always(200).await;
    fx.register(NewDestination {
        events: Vec::new(),
        ..Fixture::destination("alerts", &endpoint)
    })
    .await;

    let first = forward(&fx.db, "alerts", id, &AiPrivacy::default(), 1_700_000_000)
        .await
        .unwrap();
    let second = forward(&fx.db, "alerts", id, &AiPrivacy::default(), 1_700_000_000)
        .await
        .unwrap();
    assert_eq!(first, second);
    assert_eq!(fx.deliveries().await.len(), 1);

    // A deliberate second forward, later, is a second delivery.
    let third = forward(&fx.db, "alerts", id, &AiPrivacy::default(), 1_700_000_060)
        .await
        .unwrap();
    assert_ne!(third, first);
    assert_eq!(fx.deliveries().await.len(), 2);
}

// ---------------------------------------------------------------------------
// Subscriptions and secrets
// ---------------------------------------------------------------------------

#[test]
fn a_subscription_round_trips_and_an_unknown_entry_is_dropped_not_fatal() {
    let events = vec![HookEvent::OnNewMessage, HookEvent::OnSyncError];
    assert_eq!(split_events(&join_events(&events)), events);
    assert_eq!(split_events(""), Vec::new());
    assert_eq!(
        split_events("on_new_message\nfrom_the_future\non_move"),
        vec![HookEvent::OnNewMessage, HookEvent::OnMove]
    );
}

#[tokio::test]
async fn a_destination_never_stores_or_reports_the_key_itself() {
    // SAFETY: as above — this test owns this variable name.
    unsafe { std::env::set_var("RMAIL_TEST_SECRET_NEVER_STORED", "top-secret-value") };
    let fx = Fixture::open().await;
    let endpoint = Endpoint::always(200).await;
    let destination = fx
        .register(NewDestination {
            secret: CredentialSource::Env("RMAIL_TEST_SECRET_NEVER_STORED".to_owned()),
            ..Fixture::destination("alerts", &endpoint)
        })
        .await;
    assert!(!format!("{destination:?}").contains("top-secret-value"));

    // The raw row, not the parsed struct: the key must not be anywhere in the
    // database either.
    let dumped: String = fx
        .db
        .read(|c| {
            c.query_row(
                "SELECT secret_kind || '|' || COALESCE(secret_reference, '')
                 FROM webhook_destinations",
                [],
                |r| r.get(0),
            )
        })
        .await
        .unwrap();
    assert_eq!(dumped, "env|RMAIL_TEST_SECRET_NEVER_STORED");

    // ...and it resolves to the real value only through the credential
    // provider, on demand.
    let resolved = resolve_key(&destination.secret, &destination.name)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resolved.expose(), "top-secret-value");
}

#[tokio::test]
async fn an_unresolvable_signing_key_defers_rather_than_sending_unsigned() {
    let fx = Fixture::open().await;
    let id = fx.message("x", "body").await;
    let endpoint = Endpoint::always(200).await;
    let destination = fx
        .register(NewDestination {
            secret: CredentialSource::Env("RMAIL_TEST_ABSENT_KEY_VARIABLE".to_owned()),
            max_attempts: 2,
            ..Fixture::destination("alerts", &endpoint)
        })
        .await;
    enqueue_for_message(
        &fx.db,
        &destination,
        "on_new_message",
        "event:1",
        id,
        &AiPrivacy::default(),
    )
    .await
    .unwrap();

    let report = fx
        .dispatcher(Duration::from_secs(5))
        .tick(&cancel())
        .await
        .unwrap();
    assert_eq!(report.deferred, 1);
    assert_eq!(
        endpoint.count(),
        0,
        "a destination that asked to be signed must not be sent to unsigned"
    );
}

// ---------------------------------------------------------------------------
// Cancellation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_cancelled_tick_does_not_deliver_and_leaves_the_row_retryable() {
    let fx = Fixture::open().await;
    let id = fx.message("x", "body").await;
    let endpoint = Endpoint::queued(vec![Reply::Hang]).await;
    let destination = fx
        .register(NewDestination {
            max_attempts: 5,
            ..Fixture::destination("alerts", &endpoint)
        })
        .await;
    let delivery_id = enqueue_for_message(
        &fx.db,
        &destination,
        "on_new_message",
        "event:1",
        id,
        &AiPrivacy::default(),
    )
    .await
    .unwrap()
    .unwrap();

    let token = cancel();
    token.cancel();
    let report = fx
        .dispatcher(Duration::from_secs(30))
        .tick(&token)
        .await
        .unwrap();
    assert_eq!(report.delivered, 0);
    assert_eq!(report.failed, 0);
    let row = fx.delivery(delivery_id).await;
    assert_eq!(
        row.state,
        DeliveryState::Pending,
        "a shutdown mid-flight leaves the delivery to be retried, never lost"
    );
    assert_eq!(
        row.attempts, 0,
        "a shutdown is not the endpoint's fault; the attempt the claim charged is refunded"
    );
    assert_eq!(endpoint.count(), 0);
}

/// The reason [`Attempt::Cancelled`] refunds rather than charging: without it,
/// a daemon restarted `max_attempts` times while one delivery was in flight
/// would mark it `failed` having never had a single refusal from the endpoint.
#[tokio::test]
async fn repeated_shutdowns_cannot_exhaust_a_delivery_attempt_cap() {
    let fx = Fixture::open().await;
    let id = fx.message("x", "body").await;
    let endpoint = Endpoint::queued(vec![Reply::Hang]).await;
    let destination = fx
        .register(NewDestination {
            max_attempts: 2,
            ..Fixture::destination("alerts", &endpoint)
        })
        .await;
    let delivery_id = enqueue_for_message(
        &fx.db,
        &destination,
        "on_new_message",
        "event:1",
        id,
        &AiPrivacy::default(),
    )
    .await
    .unwrap()
    .unwrap();

    let dispatcher = fx.dispatcher(Duration::from_secs(30));
    for _ in 0..5 {
        let token = cancel();
        token.cancel();
        dispatcher.tick(&token).await.unwrap();
    }
    let row = fx.delivery(delivery_id).await;
    assert_eq!(row.state, DeliveryState::Pending);
    assert_eq!(row.attempts, 0);
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[test]
fn webhooks_are_off_until_an_operator_says_otherwise() {
    let config = crate::config::Config::default();
    assert!(
        !config.webhooks.enabled,
        "the one config default that decides whether mail can leave the machine"
    );
    assert!(!NewDestination::default().include_body);
}

#[test]
fn the_webhooks_table_parses_from_toml() {
    let config = crate::config::Config::from_toml_str(
        r#"
        [webhooks]
        enabled = true
        max_concurrency = 2
        tick_interval = "1s"
        delivery_timeout = "5s"
        backoff_base = "10s"
        backoff_max = "5m"
        max_batch = 25
        "#,
    )
    .unwrap();
    assert!(config.webhooks.enabled);
    assert_eq!(config.webhooks.max_concurrency, 2);
    assert_eq!(config.webhooks.max_batch, 25);
    assert_eq!(
        config.webhooks.backoff_max.as_duration(),
        Duration::from_secs(300)
    );
}
