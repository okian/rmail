//! What task 49 owes: a deep pass only ever runs for a message triage's own
//! verdict qualifies (gating), a thread's prior state is folded into each
//! new request without ever resending an earlier message's body
//! (incrementality), and the resulting summary/key-points/todos become
//! part of what a message is findable by (index feed) — the three things
//! `tasks.md`'s `verify` line names explicitly. Structured-output parsing,
//! entity persistence and the `suggest_reply` operator toggle are covered
//! alongside them.
//!
//! Driven against a real HTTP server on loopback for the end-to-end tests,
//! the same "test against a socket, not a mocked client" discipline
//! `ai::provider`'s and `ai::triage`'s own tests use — never the real
//! Anthropic API.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::ai::policy::PolicyEngine;
use crate::ai::provider::{ClaudeProvider, Provider};
use crate::ai::queue::{AiQueue, AiWorkerPool, NewAiJob, QueueOptions};
use crate::ai::triage::TriagePassHandler;
use crate::config::{AiConfig, AiLimits, AiPolicyMode, AiPrivacy, AiRetry, Bm25Weights};
use crate::index::fts::FtsIndex;
use crate::repo;
use crate::storage::Database;

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    db: Database,
    ai_queue: AiQueue,
    index_queue: IndexQueue,
    path: PathBuf,
    account_id: i64,
    inbox_id: i64,
    next_uid: AtomicI64,
}

impl Fixture {
    async fn open() -> Self {
        Self::with_options(QueueOptions::default()).await
    }

    async fn with_options(opts: QueueOptions) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-deep-{pid}-{n}.db"));
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
        let ai_queue = AiQueue::new(db.clone(), opts);
        let index_queue = IndexQueue::new(db.clone(), crate::index::QueueOptions::default());
        Self {
            db,
            ai_queue,
            index_queue,
            path,
            account_id,
            inbox_id,
            next_uid: AtomicI64::new(1),
        }
    }

    /// A new thread row, returning its id.
    async fn thread(&self) -> i64 {
        let account_id = self.account_id;
        self.db
            .write(move |c| {
                repo::insert_thread(
                    c,
                    &repo::NewThread {
                        account_id,
                        ..Default::default()
                    },
                )
            })
            .await
            .unwrap()
    }

    async fn message(&self, body: &str) -> i64 {
        self.message_in_thread(None, body).await
    }

    /// Move a message to a different thread's *live* `messages.thread_id` —
    /// what `thread::merge_threads` does to every message in a merged-away
    /// thread, without ever touching that message's already-written
    /// `ai_summaries.thread_id` snapshot. Lets a test simulate the merge's
    /// effect precisely, without depending on `thread::merge_threads`
    /// itself (a different module's implementation detail).
    async fn set_message_thread(&self, message_id: i64, thread_id: i64) {
        self.db
            .write(move |c| {
                c.execute(
                    "UPDATE messages SET thread_id = ?1 WHERE id = ?2",
                    rusqlite::params![thread_id, message_id],
                )
            })
            .await
            .unwrap();
    }

    async fn message_in_thread(&self, thread_id: Option<i64>, body: &str) -> i64 {
        let uid = self.next_uid.fetch_add(1, Ordering::Relaxed);
        let (account_id, mailbox_id) = (self.account_id, self.inbox_id);
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
                        thread_id,
                        subject: Some("Q3 roadmap".to_owned()),
                        from_addr: Some("pm@example.com".to_owned()),
                        from_name: Some("Priya".to_owned()),
                        body_text: Some(body),
                        ..Default::default()
                    },
                )
            })
            .await
            .unwrap()
    }

    /// Insert a deep-pass row directly, for the thread-fold unit tests —
    /// proving `build_request` reads a prior row correctly without first
    /// having to run a whole call to produce one.
    async fn insert_deep(
        &self,
        message_id: i64,
        thread_id: i64,
        summary: &str,
        thread_summary: Option<&str>,
    ) {
        let account_id = self.account_id;
        let summary = summary.to_owned();
        let thread_summary = thread_summary.map(str::to_owned);
        self.db
            .write(move |c| {
                c.execute(
                    "INSERT INTO ai_summaries (
                         message_id, account_id, thread_id, model, pass, schema_version,
                         summary, thread_summary, created_at
                     ) VALUES (?1, ?2, ?3, 'claude-opus-4-8', 'deep', 1, ?4, ?5, unixepoch())",
                    rusqlite::params![message_id, account_id, thread_id, summary, thread_summary],
                )
            })
            .await
            .unwrap();
    }

    /// A minimal, valid `ai_ledger` row, returning its id — `ai_summaries.
    /// ledger_entry_id` is a real foreign key (V21/V18), so any test calling
    /// `on_success` directly (rather than through a full dispatch cycle,
    /// which creates this row for real via `audit::record_call_priced`)
    /// needs one to exist first.
    async fn ledger_entry(&self) -> i64 {
        self.db
            .write(|c| {
                c.execute(
                    "INSERT INTO ai_ledger (
                         created_at, model, pass, input_tokens, output_tokens,
                         cache_creation_input_tokens, cache_read_input_tokens,
                         cost_usd, redaction_level, latency_ms, payload_sha256, status
                     ) VALUES (unixepoch(), 'claude-opus-4-8', 'deep', 100, 100, 0, 0, \
                     0.01, 'none', 500, X'00', 'ok')",
                    [],
                )?;
                Ok(c.last_insert_rowid())
            })
            .await
            .unwrap()
    }

    async fn ai_summaries_row(
        &self,
        message_id: i64,
    ) -> Option<(String, Option<String>, Option<String>)> {
        self.db
            .with_read(move |conn| {
                conn.query_row(
                    "SELECT summary, thread_summary, suggested_reply FROM ai_summaries
                     WHERE message_id = ?1 AND pass = 'deep'",
                    [message_id],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, Option<String>>(1)?,
                            row.get::<_, Option<String>>(2)?,
                        ))
                    },
                )
                .optional()
            })
            .unwrap()
    }

    async fn entity_count(&self, message_id: i64) -> i64 {
        self.db
            .with_read(move |conn| {
                conn.query_row(
                    "SELECT count(*) FROM ai_entities WHERE message_id = ?1",
                    [message_id],
                    |row| row.get(0),
                )
            })
            .unwrap()
    }

    async fn index_content_text(&self, message_id: i64) -> Option<String> {
        self.db
            .with_read(move |conn| {
                conn.query_row(
                    "SELECT text FROM index_content WHERE message_id = ?1 AND part = 'summary'",
                    [message_id],
                    |row| row.get(0),
                )
                .optional()
            })
            .unwrap()
    }

    async fn index_queue_pending(&self, message_id: i64, kind: &str) -> i64 {
        let kind = kind.to_owned();
        self.db
            .with_read(move |conn| {
                conn.query_row(
                    "SELECT count(*) FROM index_queue
                     WHERE message_id = ?1 AND kind = ?2 AND state = 'pending'",
                    rusqlite::params![message_id, kind],
                    |row| row.get(0),
                )
            })
            .unwrap()
    }

    async fn index_queue_content_hash(&self, message_id: i64, kind: &str) -> Option<Vec<u8>> {
        let kind = kind.to_owned();
        self.db
            .with_read(move |conn| {
                conn.query_row(
                    "SELECT content_hash FROM index_queue WHERE message_id = ?1 AND kind = ?2",
                    rusqlite::params![message_id, kind],
                    |row| row.get(0),
                )
                .optional()
            })
            .unwrap()
    }

    /// Every `(part, content_hash)` currently stored for a message — the
    /// same read `ai::deep::feed_index` and `index::extract::store` each do
    /// before computing [`crate::index::extract::message_hash`], so a test
    /// can compute the identical hash independently and compare.
    async fn stored_index_content(&self, message_id: i64) -> Vec<(String, Vec<u8>)> {
        self.db
            .with_read(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT part, content_hash FROM index_content WHERE message_id = ?1",
                )?;
                let rows = stmt
                    .query_map([message_id], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .unwrap()
    }

    async fn deep_job_state(&self, message_id: i64) -> Option<String> {
        self.db
            .with_read(move |conn| {
                conn.query_row(
                    "SELECT state FROM ai_queue WHERE message_id = ?1 AND pass = 'deep'",
                    [message_id],
                    |row| row.get(0),
                )
                .optional()
            })
            .unwrap()
    }

    /// Whether a *triage* `ai_summaries` row exists for `message_id` — used
    /// by the atomicity-under-failure test to prove `triage::write_summary`
    /// really is "both or neither": a `ledger_entry_id` that violates the
    /// foreign key must leave neither this row nor the deep job behind.
    async fn triage_row_exists(&self, message_id: i64) -> bool {
        self.db
            .with_read(move |conn| {
                conn.query_row(
                    "SELECT count(*) FROM ai_summaries WHERE message_id = ?1 AND pass = 'triage'",
                    [message_id],
                    |row| row.get::<_, i64>(0),
                )
            })
            .map(|n| n > 0)
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.path.display())));
        }
    }
}

fn open_policy() -> PolicyEngine {
    PolicyEngine::new(Vec::new(), AiPolicyMode::Allowed, "unspecified").unwrap()
}

fn high_rpm_limits() -> AiLimits {
    AiLimits {
        max_concurrency: 4,
        requests_per_minute: 1_000_000,
        ..AiLimits::default()
    }
}

fn handler(fx: &Fixture) -> Arc<DeepPassHandler> {
    Arc::new(DeepPassHandler::new(
        fx.db.clone(),
        fx.index_queue.clone(),
        "claude-opus-4-8",
        AiDeepPass::default(),
    ))
}

fn content_for(message_id: i64, account_id: i64, body: &str) -> MessageContent {
    MessageContent {
        message_id,
        account_id,
        subject: Some("Q3 roadmap".to_owned()),
        from_name: Some("Priya".to_owned()),
        from_addr: Some("pm@example.com".to_owned()),
        body: body.to_owned(),
        truncated: false,
        attachments_included: false,
    }
}

fn no_cancel() -> CancellationToken {
    CancellationToken::new()
}

fn lease_for(message_id: i64, account_id: i64) -> AiLease {
    AiLease {
        job_id: 1,
        message_id,
        account_id,
        pass: PASS.to_owned(),
        attempts: 1,
        lease_expires_at: 0,
        worker: "test-worker".to_owned(),
    }
}

// ---------------------------------------------------------------------------
// A hand-rolled mock Anthropic server (real HTTP on loopback), the same
// discipline `ai::provider`'s and `ai::triage`'s own tests use.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Seen {
    body: serde_json::Value,
}

struct Server {
    endpoint: String,
    seen: Arc<Mutex<Vec<Seen>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Server {
    async fn queued(replies: Vec<(u16, String)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
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
                    let fallback = (500u16, String::new());
                    queue.pop_front().unwrap_or(fallback)
                };
                tokio::spawn(handle_connection(stream, recorder, reply));
            }
        });
        Self {
            endpoint: format!("http://{addr}/v1/messages"),
            seen,
            task,
        }
    }

    fn requests(&self) -> Vec<Seen> {
        self.seen.lock().map(|log| log.clone()).unwrap_or_default()
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    recorder: Arc<Mutex<Vec<Seen>>>,
    (status, body): (u16, String),
) {
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    let Some((head_end, length)) = read_request_head(&mut stream, &mut raw, &mut buf).await else {
        return;
    };
    let seen = Seen {
        body: serde_json::from_str(&String::from_utf8_lossy(&raw[head_end..head_end + length]))
            .unwrap_or(serde_json::Value::Null),
    };
    if let Ok(mut log) = recorder.lock() {
        log.push(seen);
    }
    let response = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.flush().await;
}

async fn read_request_head(
    stream: &mut TcpStream,
    raw: &mut Vec<u8>,
    buf: &mut [u8; 4096],
) -> Option<(usize, usize)> {
    loop {
        let n = stream.read(buf).await.unwrap_or(0);
        if n == 0 {
            return None;
        }
        raw.extend_from_slice(&buf[..n]);
        let text = String::from_utf8_lossy(raw).to_string();
        if let Some(at) = text.find("\r\n\r\n") {
            let length = text
                .lines()
                .find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.trim()
                        .eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().to_owned())
                })
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(0);
            if raw.len() >= at + 4 + length {
                return Some((at + 4, length));
            }
        }
    }
}

fn provider(server: &Server) -> Arc<dyn Provider> {
    let config = AiConfig {
        api_key_command: "printf secret-key".to_owned(),
        retry: AiRetry {
            max_attempts: 1,
            base_delay_ms: 1,
            max_delay_ms: 2,
        },
        ..AiConfig::default()
    };
    Arc::new(
        ClaudeProvider::new(&config)
            .unwrap()
            .with_endpoint(&server.endpoint),
    )
}

fn usage_json() -> serde_json::Value {
    json!({
        "input_tokens": 200,
        "output_tokens": 150,
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": 0,
    })
}

fn deep_message_body(deep_json: &serde_json::Value) -> String {
    json!({
        "id": "msg_deep",
        "model": "claude-opus-4-8",
        "content": [{"type": "text", "text": deep_json.to_string()}],
        "stop_reason": "end_turn",
        "usage": usage_json(),
    })
    .to_string()
}

fn triage_message_body(triage_json: &serde_json::Value) -> String {
    json!({
        "id": "msg_triage",
        "model": "claude-haiku-4-5",
        "content": [{"type": "text", "text": triage_json.to_string()}],
        "stop_reason": "end_turn",
        "usage": usage_json(),
    })
    .to_string()
}

fn valid_triage_json(priority: &str, needs_reply: bool, category: &str) -> serde_json::Value {
    json!({
        "category": category,
        "priority": priority,
        "needs_reply": needs_reply,
        "sentiment": "neutral",
        "suggested_tags": [],
        "tl_dr": "A test message.",
    })
}

fn valid_deep_json(thread_summary: &str) -> serde_json::Value {
    json!({
        "summary": "Priya asks for roadmap feedback before Friday and flags a staffing risk.",
        "key_points": [
            "Wants feedback on the Q3 roadmap draft",
            "Flags a staffing risk on the API migration workstream",
        ],
        "todos": [
            {"text": "Review the roadmap draft", "due": "Friday", "owner": "recipient"},
        ],
        "entities": [
            {"kind": "date", "value": "Friday", "iso": null, "amount": null, "currency": null},
            {"kind": "person", "value": "Priya", "iso": null, "amount": null, "currency": null},
        ],
        "suggested_reply": "Thanks Priya -- I'll review the roadmap and get back to you by Friday.",
        "thread_summary": thread_summary,
    })
}

// ---------------------------------------------------------------------------
// build_request: structured output, schema shape
// ---------------------------------------------------------------------------

#[tokio::test]
async fn build_request_uses_the_configured_model_and_a_json_schema_output_format() {
    let fx = Fixture::open().await;
    let h = handler(&fx);
    let id = fx.message("Can you review the roadmap by Friday?").await;
    let content = content_for(id, fx.account_id, "Can you review the roadmap by Friday?");

    let request = h.build_request(&content).await.unwrap();

    assert_eq!(request.model, "claude-opus-4-8");
    assert_eq!(request.messages.len(), 1);
    let format = request
        .output_format
        .expect("deep pass must constrain output via output_config.format");
    let schema = format.schema;
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["additionalProperties"], false);
    for field in [
        "summary",
        "key_points",
        "todos",
        "entities",
        "suggested_reply",
        "thread_summary",
    ] {
        assert!(
            schema["properties"].get(field).is_some(),
            "schema missing {field}"
        );
        assert!(
            schema["required"]
                .as_array()
                .unwrap()
                .contains(&serde_json::Value::String(field.to_owned())),
            "{field} must be required"
        );
    }
    let entity_kinds = schema["properties"]["entities"]["items"]["properties"]["kind"]["enum"]
        .as_array()
        .unwrap()
        .clone();
    for kind in ["date", "amount", "person", "organization", "other"] {
        assert!(
            entity_kinds.contains(&serde_json::Value::String(kind.to_owned())),
            "entity kind enum missing {kind}"
        );
    }
}

#[tokio::test]
async fn build_request_with_no_thread_carries_no_prior_synopsis_section() {
    let fx = Fixture::open().await;
    let h = handler(&fx);
    let id = fx.message("First contact, no thread yet.").await;
    let content = content_for(id, fx.account_id, "First contact, no thread yet.");

    let request = h.build_request(&content).await.unwrap();

    assert!(
        !request.messages[0]
            .content
            .contains("Prior thread synopsis"),
        "a message with no thread must not claim to have prior thread context"
    );
}

// ---------------------------------------------------------------------------
// Thread rollup incrementality: the acceptance criterion's own words
// ---------------------------------------------------------------------------

#[tokio::test]
async fn build_request_folds_the_prior_threads_state_without_resending_the_body() {
    let fx = Fixture::open().await;
    let thread_id = fx.thread().await;
    let first = fx
        .message_in_thread(Some(thread_id), "MARKER_ONLY_IN_FIRST_MESSAGE_BODY")
        .await;
    let second = fx
        .message_in_thread(Some(thread_id), "Second message body")
        .await;
    fx.insert_deep(
        first,
        thread_id,
        "Priya opened the thread about the roadmap.",
        Some("Priya asked for roadmap feedback; staffing risk noted."),
    )
    .await;

    let h = handler(&fx);
    let content = content_for(second, fx.account_id, "Second message body");
    let request = h.build_request(&content).await.unwrap();
    let rendered = &request.messages[0].content;

    assert!(
        rendered.contains("Priya asked for roadmap feedback; staffing risk noted."),
        "the prior thread's rollup must be folded into the next message's request: {rendered:?}"
    );
    assert!(
        !rendered.contains("MARKER_ONLY_IN_FIRST_MESSAGE_BODY"),
        "folding the prior state must never resend an earlier message's own body: {rendered:?}"
    );
}

#[tokio::test]
async fn a_runaway_prior_thread_state_is_truncated_before_it_reaches_the_request() {
    // The fold is unbounded by construction: every deep pass writes a rollup
    // that the next one reads back and folds forward again. Across a long
    // thread that grows without limit, and it grows on the *input* side of a
    // metered API call. Worse, `redact`'s truncation runs after this text is
    // already prepended and does not set the `[body truncated]` marker, so an
    // oversized synopsis would silently start eating the real message body's
    // tail — the model would be told the body was complete when it was not.
    //
    // MAX_PRIOR_STATE_CHARS existed but was never applied; this pins it.
    let fx = Fixture::open().await;
    let thread_id = fx.thread().await;
    let first = fx.message_in_thread(Some(thread_id), "first").await;
    let second = fx
        .message_in_thread(Some(thread_id), "BODY_MARKER_MUST_SURVIVE")
        .await;

    // Comfortably past the cap, and multi-byte so a byte-wise cut would
    // panic or produce invalid UTF-8 rather than merely over-truncating.
    let runaway = "é".repeat(MAX_PRIOR_STATE_CHARS + 500);
    fx.insert_deep(first, thread_id, "synopsis", Some(&runaway))
        .await;

    let h = handler(&fx);
    let content = content_for(second, fx.account_id, "BODY_MARKER_MUST_SURVIVE");
    let request = h.build_request(&content).await.unwrap();
    let rendered = &request.messages[0].content;

    assert!(
        !rendered.contains(&runaway),
        "the full runaway synopsis reached the request unbounded"
    );
    assert!(
        rendered.contains("[prior thread state truncated]"),
        "truncation must be visible to the model, not silent: {rendered:?}"
    );
    assert!(
        rendered.contains("BODY_MARKER_MUST_SURVIVE"),
        "capping the prior state must not cost the message its own body"
    );
    // The cap binds on the folded text specifically, not on the whole
    // rendered request, so measure the part that came from the fold.
    let folded_chars = rendered.matches('é').count();
    assert!(
        folded_chars <= MAX_PRIOR_STATE_CHARS,
        "kept {folded_chars} chars of prior state, cap is {MAX_PRIOR_STATE_CHARS}"
    );
}

#[tokio::test]
async fn build_request_prefers_the_prior_rows_thread_summary_over_its_bare_summary() {
    // Once a thread has a rollup, later messages fold *that* forward rather
    // than the contributing message's own short synopsis -- otherwise a
    // third message would only ever see the second message's own summary
    // and the first message's contribution would quietly drop out.
    let fx = Fixture::open().await;
    let thread_id = fx.thread().await;
    let first = fx.message_in_thread(Some(thread_id), "first").await;
    let third = fx.message_in_thread(Some(thread_id), "third").await;
    fx.insert_deep(
        first,
        thread_id,
        "This message's own bare summary.",
        Some("The thread's accumulated rollup so far."),
    )
    .await;

    let h = handler(&fx);
    let content = content_for(third, fx.account_id, "third");
    let request = h.build_request(&content).await.unwrap();

    assert!(request.messages[0]
        .content
        .contains("The thread's accumulated rollup so far."));
    assert!(!request.messages[0]
        .content
        .contains("This message's own bare summary."));
}

#[tokio::test]
async fn a_stale_ai_summaries_thread_id_snapshot_is_not_trusted_after_a_merge() {
    // Simulates `thread::merge_threads`: a message's *live* `messages.
    // thread_id` moves to a different thread, but its already-written
    // `ai_summaries.thread_id` snapshot is never touched (V21's own docs
    // are explicit that column has no foreign key and is exactly a
    // snapshot). The fold must follow the live column, not the stale one,
    // or a merged-away message's rollup keeps leaking into a thread it no
    // longer belongs to.
    let fx = Fixture::open().await;
    let thread_a = fx.thread().await;
    let thread_b = fx.thread().await;

    let moved = fx
        .message_in_thread(Some(thread_a), "the message that gets merged away")
        .await;
    fx.insert_deep(
        moved,
        thread_a,
        "The merged-away message's own summary.",
        Some("Thread A's rollup, from before the merge."),
    )
    .await;

    // The merge: `moved`'s live thread changes to B; nothing updates its
    // already-written `ai_summaries.thread_id` (still `thread_a`).
    fx.set_message_thread(moved, thread_b).await;

    // A fresh message actually in thread A today must not see the
    // merged-away message's rollup — it is not part of thread A's live
    // membership, whatever its `ai_summaries.thread_id` snapshot still says.
    let second = fx
        .message_in_thread(Some(thread_a), "a genuinely new thread A message")
        .await;
    let h = handler(&fx);
    let content = content_for(second, fx.account_id, "a genuinely new thread A message");
    let request = h.build_request(&content).await.unwrap();

    assert!(
        !request.messages[0]
            .content
            .contains("Thread A's rollup, from before the merge."),
        "a message whose live thread_id moved away must not still be folded in: {:?}",
        request.messages[0].content
    );
}

#[tokio::test]
async fn build_request_excludes_the_messages_own_prior_deep_row_even_when_newest() {
    // `prior_thread_state`'s `s.message_id != ?2` clause matters only when
    // the target message's own prior row would otherwise win the
    // `ORDER BY created_at DESC, id DESC LIMIT 1` race -- so this inserts
    // the *other* message's rollup first and the target's own prior row
    // last, making the target's own row the newest of the two. Drop the
    // self-exclusion clause and this test folds a message's own past
    // output back into its own next request instead of the other
    // participant's.
    let fx = Fixture::open().await;
    let thread_id = fx.thread().await;
    let target = fx.message_in_thread(Some(thread_id), "target body").await;
    let other = fx.message_in_thread(Some(thread_id), "other body").await;

    fx.insert_deep(
        other,
        thread_id,
        "Other's own bare summary.",
        Some("Other's rollup, contributed first."),
    )
    .await;
    fx.insert_deep(
        target,
        thread_id,
        "Target's own bare summary.",
        Some("Target's own rollup -- must never fold into its own request."),
    )
    .await;

    let h = handler(&fx);
    let content = content_for(target, fx.account_id, "target body");
    let request = h.build_request(&content).await.unwrap();
    let rendered = &request.messages[0].content;

    assert!(
        rendered.contains("Other's rollup, contributed first."),
        "the fold must find the *other* participant's rollup: {rendered:?}"
    );
    assert!(
        !rendered.contains("Target's own rollup"),
        "a message's own prior deep row must never be folded into its own next request, even \
         when it is the newest row for the thread: {rendered:?}"
    );
}

#[tokio::test]
async fn a_prior_row_with_an_empty_thread_summary_is_not_folded_in() {
    // `COALESCE(s.thread_summary, s.summary)` picks `thread_summary`
    // whenever it is non-null -- including an empty string, which is not
    // the same as null. Without the `TRIM(...) != ''` guard, a prior row
    // whose `thread_summary` is `Some("")` would still be selected and
    // folded in as an empty synopsis, printing a "Prior thread synopsis"
    // preamble with nothing after it instead of correctly falling through
    // to no fold at all.
    let fx = Fixture::open().await;
    let thread_id = fx.thread().await;
    let other = fx.message_in_thread(Some(thread_id), "other body").await;
    fx.insert_deep(
        other,
        thread_id,
        "Other's bare summary, must not leak in either.",
        Some(""),
    )
    .await;

    let target = fx.message_in_thread(Some(thread_id), "target body").await;
    let h = handler(&fx);
    let content = content_for(target, fx.account_id, "target body");
    let request = h.build_request(&content).await.unwrap();
    let rendered = &request.messages[0].content;

    assert!(
        !rendered.contains("Prior thread synopsis"),
        "an empty thread_summary must not produce an empty fold preamble: {rendered:?}"
    );
    assert!(
        !rendered.contains("Other's bare summary, must not leak in either."),
        "an empty thread_summary must not fall back to the bare summary either -- COALESCE \
         already picked the empty string, so the TRIM guard is the only thing stopping it: \
         {rendered:?}"
    );
}

#[tokio::test]
async fn a_second_dispatched_message_builds_on_the_first_summary_rather_than_recomputing() {
    // End-to-end through the real pipeline: two live dispatch cycles, each
    // against the mock server, proving the *actual outbound request* (not
    // just `build_request`'s return value in isolation) carries the fold.
    let fx = Fixture::open().await;
    let thread_id = fx.thread().await;
    let first = fx
        .message_in_thread(Some(thread_id), "Kick off: what's our Q3 plan?")
        .await;
    let second = fx
        .message_in_thread(Some(thread_id), "Following up on the Q3 plan question.")
        .await;

    fx.ai_queue
        .enqueue(vec![NewAiJob::new(first, fx.account_id, PASS)])
        .await
        .unwrap();

    let server = Server::queued(vec![(
        200,
        deep_message_body(&valid_deep_json(
            "Team is scoping the Q3 plan; staffing still open.",
        )),
    )])
    .await;
    let pool = AiWorkerPool::new(
        fx.db.clone(),
        fx.ai_queue.clone(),
        provider(&server),
        Arc::new(open_policy()),
        high_rpm_limits(),
        AiPrivacy::default(),
        vec![handler(&fx) as Arc<dyn PassHandler>],
        "test-worker",
    );
    let summary = pool.dispatch_pending(10, &no_cancel()).await.unwrap();
    assert_eq!(summary.completed, 1, "{summary:?}");
    drop(server);

    fx.ai_queue
        .enqueue(vec![NewAiJob::new(second, fx.account_id, PASS)])
        .await
        .unwrap();
    let server = Server::queued(vec![(
        200,
        deep_message_body(&valid_deep_json("Q3 plan rollup, now including follow-up.")),
    )])
    .await;
    let pool = AiWorkerPool::new(
        fx.db.clone(),
        fx.ai_queue.clone(),
        provider(&server),
        Arc::new(open_policy()),
        high_rpm_limits(),
        AiPrivacy::default(),
        vec![handler(&fx) as Arc<dyn PassHandler>],
        "test-worker",
    );
    let summary = pool.dispatch_pending(10, &no_cancel()).await.unwrap();
    assert_eq!(summary.completed, 1, "{summary:?}");

    let seen = server.requests();
    assert_eq!(seen.len(), 1);
    let rendered = seen[0].body["messages"][0]["content"].as_str().unwrap();
    assert!(
        rendered.contains("Team is scoping the Q3 plan; staffing still open."),
        "the second call must build on the first message's rollup: {rendered:?}"
    );
    assert!(
        !rendered.contains("Kick off: what's our Q3 plan?"),
        "the second call must never resend the first message's own body -- that is the whole \
         point of folding a *summary* rather than the thread: {rendered:?}"
    );
}

// ---------------------------------------------------------------------------
// DeepResult::parse: structured, validated, never partial
// ---------------------------------------------------------------------------

#[test]
fn a_response_that_is_not_json_fails_to_parse() {
    let err = DeepResult::parse("not json at all").unwrap_err();
    assert_eq!(err.reason(), crate::ErrorReason::Internal);
}

#[test]
fn an_out_of_vocabulary_entity_kind_fails_to_parse() {
    let mut value = valid_deep_json("t");
    value["entities"][0]["kind"] = json!("phone_number");
    let err = DeepResult::parse(&value.to_string()).unwrap_err();
    assert_eq!(err.reason(), crate::ErrorReason::Internal);
}

#[test]
fn a_well_formed_response_parses_completely() {
    let value = valid_deep_json("Rollup.");
    let result = DeepResult::parse(&value.to_string()).unwrap();
    assert!(result.summary.contains("Priya"));
    assert_eq!(result.key_points.len(), 2);
    assert_eq!(result.todos.len(), 1);
    assert_eq!(result.todos[0].due.as_deref(), Some("Friday"));
    assert_eq!(result.entities.len(), 2);
    assert!(result.suggested_reply.is_some());
    assert_eq!(result.thread_summary, "Rollup.");
}

// ---------------------------------------------------------------------------
// Gating: conditional, not unconditional
//
// `DeepPassGate::qualifies` is pure and synchronous (no DB access — see its
// own docs on why), so these are plain `#[test]`s over the predicate itself,
// not `#[tokio::test]`s against a database fixture.
// ---------------------------------------------------------------------------

#[test]
fn a_high_priority_verdict_qualifies_for_a_deep_pass() {
    let gate = DeepPassGate::new(AiDeepPass::default());
    assert!(gate.qualifies("high", false, "other"));
}

#[test]
fn a_needs_reply_verdict_qualifies_regardless_of_priority() {
    let gate = DeepPassGate::new(AiDeepPass::default());
    assert!(gate.qualifies("low", true, "other"));
}

#[test]
fn an_allowlisted_category_qualifies_even_at_low_priority_and_no_reply_needed() {
    let gate = DeepPassGate::new(AiDeepPass::default());
    // "invoice" is in `AiDeepPass::default()`'s category allowlist.
    assert!(gate.qualifies("low", false, "invoice"));
}

#[test]
fn a_verdict_that_matches_nothing_does_not_qualify() {
    let gate = DeepPassGate::new(AiDeepPass::default());
    assert!(!gate.qualifies("low", false, "newsletter"));
}

#[test]
fn critical_priority_qualifies_via_the_high_threshold_too() {
    // `priority_at_least` is a `>=`, not an `==` — regression coverage for
    // exactly that: a test that only ever checks the boundary value would
    // still pass if the comparison were quietly narrowed to `==`.
    let gate = DeepPassGate::new(AiDeepPass::default());
    assert!(gate.qualifies("critical", false, "other"));
}

#[test]
fn an_unrecognized_configured_threshold_fails_closed_not_open() {
    // Regression coverage for a real bug this task's own review caught: an
    // operator typo in `ai.deep_pass.on_priority` (e.g. "High", "none",
    // "off") must not silently make every message qualify by priority.
    let gate = DeepPassGate::new(AiDeepPass {
        on_priority: "not-a-real-priority".to_owned(),
        on_needs_reply: false,
        categories: Vec::new(),
        suggest_reply: true,
    });
    assert!(
        !gate.qualifies("critical", false, "other"),
        "an unrecognized on_priority threshold must reject every priority, not accept every one"
    );
}

#[test]
fn a_disabled_needs_reply_trigger_does_not_qualify_on_its_own() {
    let gate = DeepPassGate::new(AiDeepPass {
        on_needs_reply: false,
        ..AiDeepPass::default()
    });
    assert!(!gate.qualifies("low", true, "other"));
}

#[tokio::test]
async fn a_qualifying_triage_dispatch_enqueues_a_deep_job_atomically_through_on_success() {
    // The wiring, not just the pure gate predicate: `TriagePassHandler`
    // configured with a `DeepPassGate` must itself enqueue a deep job, in
    // the same transaction as its own triage row, the moment its live
    // dispatch succeeds — see `triage::write_summary`'s own docs on why
    // this is atomic rather than a separate best-effort step.
    let fx = Fixture::open().await;
    let id = fx
        .message("Can we push the roadmap review? This needs a reply.")
        .await;
    fx.ai_queue
        .enqueue(vec![NewAiJob::new(id, fx.account_id, "triage")])
        .await
        .unwrap();

    let server = Server::queued(vec![(
        200,
        triage_message_body(&valid_triage_json("high", true, "work")),
    )])
    .await;
    let gate = DeepPassGate::new(AiDeepPass::default());
    let triage_handler = Arc::new(
        TriagePassHandler::new(fx.db.clone(), "claude-haiku-4-5").with_deep_pass_gate(gate),
    );
    let pool = AiWorkerPool::new(
        fx.db.clone(),
        fx.ai_queue.clone(),
        provider(&server),
        Arc::new(open_policy()),
        high_rpm_limits(),
        AiPrivacy::default(),
        vec![triage_handler as Arc<dyn PassHandler>],
        "test-worker",
    );

    let summary = pool.dispatch_pending(10, &no_cancel()).await.unwrap();
    assert_eq!(summary.completed, 1, "{summary:?}");

    assert_eq!(
        fx.deep_job_state(id).await.as_deref(),
        Some("pending"),
        "a qualifying triage verdict must enqueue a deep job as part of on_success"
    );
}

#[tokio::test]
async fn a_non_qualifying_triage_dispatch_never_enqueues_a_deep_job() {
    let fx = Fixture::open().await;
    let id = fx
        .message("A routine newsletter, nothing actionable.")
        .await;
    fx.ai_queue
        .enqueue(vec![NewAiJob::new(id, fx.account_id, "triage")])
        .await
        .unwrap();

    let server = Server::queued(vec![(
        200,
        triage_message_body(&valid_triage_json("low", false, "newsletter")),
    )])
    .await;
    let gate = DeepPassGate::new(AiDeepPass::default());
    let triage_handler = Arc::new(
        TriagePassHandler::new(fx.db.clone(), "claude-haiku-4-5").with_deep_pass_gate(gate),
    );
    let pool = AiWorkerPool::new(
        fx.db.clone(),
        fx.ai_queue.clone(),
        provider(&server),
        Arc::new(open_policy()),
        high_rpm_limits(),
        AiPrivacy::default(),
        vec![triage_handler as Arc<dyn PassHandler>],
        "test-worker",
    );

    let summary = pool.dispatch_pending(10, &no_cancel()).await.unwrap();
    assert_eq!(summary.completed, 1, "{summary:?}");

    assert_eq!(
        fx.deep_job_state(id).await,
        None,
        "a non-qualifying verdict must never enqueue a deep job"
    );
}

#[tokio::test]
async fn a_failed_write_leaves_neither_the_triage_row_nor_the_deep_job_behind() {
    // The two tests above prove the happy path enqueues atomically; this one
    // proves the *transaction* part of that claim, not just the outcome —
    // reverting `triage::write_summary` to "write the triage row, then a
    // separate best-effort enqueue" would still pass both tests above (the
    // enqueue never gets a chance to fail there) but must fail this one: a
    // `ledger_entry_id` violating `ai_summaries.ledger_entry_id`'s foreign
    // key (`ai_ledger(id)`, PRAGMA foreign_keys = ON) makes the whole write
    // fail, and "both or neither" means the deep job -- which would have
    // qualified -- must never have been inserted either, even though
    // `enqueue_one` runs *before* the row insert's constraint would be
    // checked in a naive ordering.
    let fx = Fixture::open().await;
    let id = fx
        .message("Can we push the roadmap review? This needs a reply.")
        .await;
    let lease = lease_for(id, fx.account_id);
    let gate = DeepPassGate::new(AiDeepPass::default());
    let handler =
        TriagePassHandler::new(fx.db.clone(), "claude-haiku-4-5").with_deep_pass_gate(gate);

    let text = valid_triage_json("high", true, "work").to_string();
    let no_such_ledger_row = 999_999_999_i64;
    let result = handler.on_success(&lease, &text, no_such_ledger_row).await;

    assert!(
        result.is_err(),
        "a ledger_entry_id with no matching ai_ledger row must fail the write, not silently \
         succeed with a dangling reference"
    );
    assert!(
        !fx.triage_row_exists(id).await,
        "the triage row must not survive a write that failed"
    );
    assert_eq!(
        fx.deep_job_state(id).await,
        None,
        "a qualifying deep job must never be left behind when the triage write it was \
         supposed to be atomic with did not actually commit"
    );
}

// ---------------------------------------------------------------------------
// Index feed: enrichments become part of what a message is findable by
// ---------------------------------------------------------------------------

#[tokio::test]
async fn on_success_feeds_a_findable_summary_into_the_lexical_index() {
    let fx = Fixture::open().await;
    let id = fx
        .message("A perfectly ordinary message with no unusual terms in its body.")
        .await;
    let lease = lease_for(id, fx.account_id);
    let h = handler(&fx);

    let deep_json = json!({
        "summary": "Discusses the zylophonic quarterly rebate schedule.",
        "key_points": ["Mentions the zylophonic rebate program"],
        "todos": [],
        "entities": [],
        "suggested_reply": null,
        "thread_summary": "Zylophonic rebate discussion.",
    });
    let ledger_id = fx.ledger_entry().await;
    h.on_success(&lease, &deep_json.to_string(), ledger_id)
        .await
        .unwrap();

    let indexed = fx
        .index_content_text(id)
        .await
        .expect("on_success must write a Part::Summary index_content row");
    assert!(indexed.contains("zylophonic"));

    // Fold it into the real lexical index and search — the acceptance
    // criterion's own words: "a message becomes findable by words that
    // appear only in its AI summary."
    let fts = FtsIndex::new(fx.db.clone(), Bm25Weights::default());
    assert!(fts.index_message(id).await.unwrap());
    let hits = fts.search("zylophonic", 10).await.unwrap();
    assert_eq!(hits.len(), 1, "the AI-summary term must be findable");
    assert_eq!(hits[0].message_id, id);

    // And the semantic index is fed too -- via the same queue-and-let-a-
    // worker-drain-it shape `extract_message` already uses for every other
    // part, not driven synchronously here.
    assert_eq!(fx.index_queue_pending(id, "lexical").await, 1);
    assert_eq!(fx.index_queue_pending(id, "semantic").await, 1);
}

#[tokio::test]
async fn a_search_for_a_term_only_in_the_original_body_does_not_match_via_the_ai_summary() {
    // Sanity check on the same fixture: the AI summary's terms are what
    // make the message findable, not an accidental blanket "everything
    // matches everything."
    let fx = Fixture::open().await;
    let id = fx.message("body").await;
    let lease = lease_for(id, fx.account_id);
    let h = handler(&fx);

    let deep_json = json!({
        "summary": "A short synopsis with no unusual terms.",
        "key_points": [],
        "todos": [],
        "entities": [],
        "suggested_reply": null,
        "thread_summary": "Synopsis.",
    });
    let ledger_id = fx.ledger_entry().await;
    h.on_success(&lease, &deep_json.to_string(), ledger_id)
        .await
        .unwrap();

    let fts = FtsIndex::new(fx.db.clone(), Bm25Weights::default());
    fts.index_message(id).await.unwrap();
    let hits = fts.search("gibberish_term_not_anywhere", 10).await.unwrap();
    assert!(hits.is_empty());
}

#[tokio::test]
async fn feed_index_enqueues_the_whole_message_hash_not_a_hash_of_the_summary_part_alone() {
    // `index_state.content_hash` (what `index::queue::enqueue_one`'s dedup
    // compares against) is always a whole-message hash, because that is
    // what a routine `extract_message` sweep writes. If `feed_index` handed
    // the follow-on jobs a hash of the `summary` part alone, the two would
    // never agree and every deep pass would force an unconditional
    // re-index regardless of whether anything actually changed.
    let fx = Fixture::open().await;
    let id = fx
        .message("Some body text that gets extracted as its own part too.")
        .await;

    // Extract the message's own base parts first, the way a routine sync
    // would -- so `index_content` already holds more than just this pass's
    // own `summary` part by the time `on_success` runs.
    crate::index::extract_message(&fx.db, &fx.index_queue, id, crate::index::PRIORITY_NORMAL)
        .await
        .unwrap();

    let lease = lease_for(id, fx.account_id);
    let h = handler(&fx);
    let deep_json = json!({
        "summary": "A distinctive AI-only synopsis.",
        "key_points": [], "todos": [], "entities": [],
        "suggested_reply": null, "thread_summary": "t",
    });
    let ledger_id = fx.ledger_entry().await;
    h.on_success(&lease, &deep_json.to_string(), ledger_id)
        .await
        .unwrap();

    let stored = fx.stored_index_content(id).await;
    let expected = crate::index::extract::message_hash(&stored);

    let lexical_hash = fx
        .index_queue_content_hash(id, "lexical")
        .await
        .expect("a lexical job must be enqueued");
    assert_eq!(
        lexical_hash, expected,
        "the enqueued content_hash must be the whole-message hash extract_message would also \
         compute (over subject/body/summary together), not a hash of the summary part alone"
    );
    let semantic_hash = fx
        .index_queue_content_hash(id, "semantic")
        .await
        .expect("a semantic job must be enqueued");
    assert_eq!(semantic_hash, expected);
}

#[tokio::test]
async fn an_empty_ai_contribution_still_enqueues_reindex_after_removing_the_stale_part() {
    // A row-count assertion alone does not bite here: `index_queue` is
    // `UNIQUE(message_id, kind)`, so the *first* call's job row is still
    // sitting there (still `pending`, since nothing drains it in this test)
    // whether or not the second call re-enqueues anything. The real proof
    // is that the enqueued `content_hash` actually *moves* to reflect the
    // now-empty stored set -- if the removal path silently skipped
    // enqueueing (the bug this test exists to catch), the hash left behind
    // would still be the *first* call's (summary-included) hash.
    let fx = Fixture::open().await;
    let id = fx.message("body").await;
    let lease = lease_for(id, fx.account_id);
    let h = handler(&fx);

    // First: a real contribution, so there is something to remove.
    let first = json!({
        "summary": "Something with real content.",
        "key_points": [], "todos": [], "entities": [],
        "suggested_reply": null, "thread_summary": "t",
    });
    let ledger_1 = fx.ledger_entry().await;
    h.on_success(&lease, &first.to_string(), ledger_1)
        .await
        .unwrap();
    assert!(fx.index_content_text(id).await.is_some());
    let hash_with_summary = fx
        .index_queue_content_hash(id, "lexical")
        .await
        .expect("the first call must enqueue a lexical job");

    // Second: an all-empty contribution — summary/key_points/todos all
    // reduce to nothing after normalization.
    let second = json!({
        "summary": "",
        "key_points": [], "todos": [], "entities": [],
        "suggested_reply": null, "thread_summary": "t2",
    });
    let ledger_2 = fx.ledger_entry().await;
    h.on_success(&lease, &second.to_string(), ledger_2)
        .await
        .unwrap();

    assert!(
        fx.index_content_text(id).await.is_none(),
        "the stale Part::Summary row must be removed once the AI contribution is empty"
    );

    // Recompute independently from what's actually left in `index_content`
    // (empty in this fixture, since it never runs `extract_message` and
    // `summary` was the only part) rather than assuming `&[]` — the point is
    // that the enqueued jobs carry *that* value, not the stale
    // summary-included one.
    let stored = fx.stored_index_content(id).await;
    let expected_hash = crate::index::extract::message_hash(&stored);
    let lexical_hash = fx
        .index_queue_content_hash(id, "lexical")
        .await
        .expect("removal must still enqueue a lexical job, not skip enqueueing entirely");
    assert_ne!(
        lexical_hash, hash_with_summary,
        "the enqueued hash must move once the part is removed -- an unchanged hash means the \
         removal never actually reached the index queue, only `index_content`"
    );
    assert_eq!(
        lexical_hash, expected_hash,
        "the enqueued hash must reflect the now-empty stored set"
    );
    let semantic_hash = fx
        .index_queue_content_hash(id, "semantic")
        .await
        .expect("removal must still enqueue a semantic job, not skip enqueueing entirely");
    assert_eq!(semantic_hash, expected_hash);
}

// ---------------------------------------------------------------------------
// Entities: dates, amounts, people, organizations
// ---------------------------------------------------------------------------

#[tokio::test]
async fn on_success_writes_entities_dates_and_amounts() {
    let fx = Fixture::open().await;
    let id = fx.message("Invoice INV-9 for $450 due June 5.").await;
    let lease = lease_for(id, fx.account_id);
    let h = handler(&fx);

    let deep_json = json!({
        "summary": "An invoice is due.",
        "key_points": [],
        "todos": [],
        "entities": [
            {"kind": "date", "value": "June 5", "iso": "2026-06-05", "amount": null, "currency": null},
            {"kind": "amount", "value": "$450", "iso": null, "amount": 450.0, "currency": "USD"},
        ],
        "suggested_reply": null,
        "thread_summary": "Invoice thread.",
    });
    let ledger_id = fx.ledger_entry().await;
    h.on_success(&lease, &deep_json.to_string(), ledger_id)
        .await
        .unwrap();

    assert_eq!(fx.entity_count(id).await, 2);
    let (iso, amount, currency): (Option<String>, Option<f64>, Option<String>) = fx
        .db
        .with_read(move |conn| {
            conn.query_row(
                "SELECT iso, amount, currency FROM ai_entities WHERE message_id = ?1 AND kind = 'amount'",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
        })
        .unwrap();
    assert_eq!(iso, None);
    assert_eq!(amount, Some(450.0));
    assert_eq!(currency.as_deref(), Some("USD"));
}

#[tokio::test]
async fn re_running_the_deep_pass_replaces_rather_than_accumulates_entities() {
    let fx = Fixture::open().await;
    let id = fx.message("body").await;
    let lease = lease_for(id, fx.account_id);
    let h = handler(&fx);

    let first = json!({
        "summary": "s", "key_points": [], "todos": [],
        "entities": [{"kind": "person", "value": "Alice", "iso": null, "amount": null, "currency": null}],
        "suggested_reply": null, "thread_summary": "t",
    });
    let ledger_id_1 = fx.ledger_entry().await;
    h.on_success(&lease, &first.to_string(), ledger_id_1)
        .await
        .unwrap();
    assert_eq!(fx.entity_count(id).await, 1);

    let second = json!({
        "summary": "s2", "key_points": [], "todos": [],
        "entities": [
            {"kind": "person", "value": "Bob", "iso": null, "amount": null, "currency": null},
            {"kind": "person", "value": "Carol", "iso": null, "amount": null, "currency": null},
        ],
        "suggested_reply": null, "thread_summary": "t2",
    });
    let ledger_id_2 = fx.ledger_entry().await;
    h.on_success(&lease, &second.to_string(), ledger_id_2)
        .await
        .unwrap();
    assert_eq!(
        fx.entity_count(id).await,
        2,
        "a re-analysis must replace the model's entity set, not accumulate on top of it"
    );
}

// ---------------------------------------------------------------------------
// suggest_reply: an operator toggle, honored regardless of what the model drafted
// ---------------------------------------------------------------------------

#[tokio::test]
async fn suggested_reply_is_nulled_when_the_operator_disabled_it() {
    let fx = Fixture::open().await;
    let id = fx.message("body").await;
    let lease = lease_for(id, fx.account_id);
    let h = Arc::new(DeepPassHandler::new(
        fx.db.clone(),
        fx.index_queue.clone(),
        "claude-opus-4-8",
        AiDeepPass {
            suggest_reply: false,
            ..AiDeepPass::default()
        },
    ));

    let deep_json = valid_deep_json("rollup");
    let ledger_id = fx.ledger_entry().await;
    h.on_success(&lease, &deep_json.to_string(), ledger_id)
        .await
        .unwrap();

    let (_, _, suggested_reply) = fx.ai_summaries_row(id).await.unwrap();
    assert_eq!(
        suggested_reply, None,
        "suggest_reply = false must suppress the stored reply even though the model drafted one"
    );
}

#[tokio::test]
async fn suggested_reply_is_stored_when_the_operator_left_it_on() {
    let fx = Fixture::open().await;
    let id = fx.message("body").await;
    let lease = lease_for(id, fx.account_id);
    let h = handler(&fx); // default config: suggest_reply = true

    let deep_json = valid_deep_json("rollup");
    let ledger_id = fx.ledger_entry().await;
    h.on_success(&lease, &deep_json.to_string(), ledger_id)
        .await
        .unwrap();

    let (summary, thread_summary, suggested_reply) = fx.ai_summaries_row(id).await.unwrap();
    assert!(summary.contains("Priya"));
    assert_eq!(thread_summary.as_deref(), Some("rollup"));
    assert!(suggested_reply.is_some());
}
