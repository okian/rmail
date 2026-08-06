//! What task 48 owes: a triage request is structured JSON (never regex over
//! prose), a successful response lands as a complete `ai_summaries` row with
//! `ai_fts` kept in sync, a schema-invalid response is a hard error the
//! queue can dead-letter rather than a partial row, the same
//! [`TriagePassHandler`] works through both [`AiWorkerPool`] (live) and
//! [`BatchCoordinator`] (backlog), and the `ai:` search operators
//! (`retrieve::filtermask`) select exactly the right messages against real
//! `ai_summaries` rows — not merely that the SQL compiles.
//!
//! Driven against a real HTTP server on loopback, the same "test against a
//! socket, not a mocked client" discipline `ai::provider`'s own tests and
//! `ai::queue`'s batch tests use — never the real Anthropic API.

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
use crate::ai::queue::{
    AiQueue, AiWorkerPool, BatchClient, BatchCoordinator, BatchPollOutcome, NewAiJob, PassHandler,
    QueueOptions,
};
use crate::config::{AiBatching, AiConfig, AiLimits, AiPolicyMode, AiPrivacy, AiRetry};
use crate::embed::hash::HashEmbedder;
use crate::query::QueryPlanner;
use crate::retrieve::StructuredRetriever;
use crate::storage::Database;
use crate::{config::ExpansionConfig, repo};

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    db: Database,
    queue: AiQueue,
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
        let path = std::env::temp_dir().join(format!("rmail-triage-{pid}-{n}.db"));
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
        let queue = AiQueue::new(db.clone(), opts);
        Self {
            db,
            queue,
            path,
            account_id,
            inbox_id,
            next_uid: AtomicI64::new(1),
        }
    }

    async fn message(&self, body: &str) -> i64 {
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

    /// Insert an `ai_summaries` row directly — for the retrieval-wiring
    /// tests, which are proving `retrieve::filtermask` reads this table
    /// correctly, not re-proving the AI pipeline writes it (that is what
    /// the `AiWorkerPool`/`BatchCoordinator` tests below already cover).
    #[allow(clippy::too_many_arguments)]
    async fn insert_summary(
        &self,
        message_id: i64,
        needs_reply: Option<bool>,
        category: Option<&str>,
        priority: Option<&str>,
        sentiment: Option<&str>,
    ) {
        let account_id = self.account_id;
        let category = category.map(str::to_owned);
        let priority = priority.map(str::to_owned);
        let sentiment = sentiment.map(str::to_owned);
        self.db
            .write(move |c| {
                c.execute(
                    "INSERT INTO ai_summaries (
                         message_id, account_id, model, pass, schema_version,
                         tl_dr, needs_reply, category, priority, sentiment, created_at
                     ) VALUES (?1, ?2, 'claude-haiku-4-5', 'triage', 1, 'tl;dr', ?3, ?4, ?5, ?6, unixepoch())",
                    rusqlite::params![message_id, account_id, needs_reply, category, priority, sentiment],
                )
            })
            .await
            .unwrap();
    }

    fn queue_options_max_attempts_1() -> QueueOptions {
        QueueOptions {
            max_attempts: 1,
            ..QueueOptions::default()
        }
    }

    /// Insert an `ai_summaries` row for a specific `pass`, with `tl_dr` as
    /// the only field set — for the V21 FTS-trigger test below, which needs
    /// several distinct rows (and re-inserts hitting the upsert) per
    /// message and does not care about the triage-specific columns.
    async fn insert_pass(&self, message_id: i64, pass: &str, tl_dr: &str) {
        let account_id = self.account_id;
        let pass = pass.to_owned();
        let tl_dr = tl_dr.to_owned();
        self.db
            .write(move |c| {
                c.execute(
                    "INSERT INTO ai_summaries (message_id, account_id, model, pass, schema_version, tl_dr, created_at)
                     VALUES (?1, ?2, 'claude-haiku-4-5', ?3, 1, ?4, unixepoch())
                     ON CONFLICT(message_id, pass, model) DO UPDATE SET tl_dr = excluded.tl_dr",
                    rusqlite::params![message_id, account_id, pass, tl_dr],
                )
            })
            .await
            .unwrap();
    }

    async fn delete_pass(&self, message_id: i64, pass: &str) {
        let pass = pass.to_owned();
        self.db
            .write(move |c| {
                c.execute(
                    "DELETE FROM ai_summaries WHERE message_id = ?1 AND pass = ?2",
                    rusqlite::params![message_id, pass],
                )
            })
            .await
            .unwrap();
    }

    async fn delete_message(&self, message_id: i64) {
        self.db
            .write(move |c| c.execute("DELETE FROM messages WHERE id = ?1", [message_id]))
            .await
            .unwrap();
    }

    async fn ai_summaries_count(&self, message_id: i64) -> i64 {
        self.db
            .with_read(move |conn| {
                conn.query_row(
                    "SELECT count(*) FROM ai_summaries WHERE message_id = ?1",
                    [message_id],
                    |row| row.get(0),
                )
            })
            .unwrap()
    }

    async fn ai_fts_row_count(&self, message_id: i64) -> i64 {
        self.db
            .with_read(move |conn| {
                conn.query_row(
                    "SELECT count(*) FROM ai_fts WHERE rowid = ?1",
                    [message_id],
                    |row| row.get(0),
                )
            })
            .unwrap()
    }

    async fn ai_fts_matches(&self, message_id: i64, term: &str) -> bool {
        let term = term.to_owned();
        self.db
            .with_read(move |conn| {
                conn.query_row(
                    "SELECT count(*) FROM ai_fts WHERE rowid = ?1 AND ai_fts MATCH ?2",
                    rusqlite::params![message_id, term],
                    |row| row.get::<_, i64>(0),
                )
            })
            .map(|count| count > 0)
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

/// A policy engine that allows everything — policy resolution (task 46) is
/// not what this module's tests are about.
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

fn handler(db: &Database) -> Arc<TriagePassHandler> {
    Arc::new(TriagePassHandler::new(db.clone(), "claude-haiku-4-5"))
}

// ---------------------------------------------------------------------------
// A hand-rolled mock Anthropic server (real HTTP on loopback), the same
// discipline `ai::provider`'s own tests and `ai::queue`'s batch tests use.
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
    async fn json(status: u16, body: impl Into<String>) -> Self {
        Self::queued(vec![(status, body.into())]).await
    }

    /// Answers each connection from `replies` in turn, repeating the last
    /// one once exhausted — enough to express "fail once, then succeed"
    /// across several requests.
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
                    if queue.len() > 1 {
                        queue.pop_front().unwrap_or(fallback)
                    } else {
                        queue.front().cloned().unwrap_or(fallback)
                    }
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
        "input_tokens": 20,
        "output_tokens": 30,
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": 0,
    })
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

fn valid_triage_json() -> serde_json::Value {
    json!({
        "category": "work",
        "priority": "high",
        "needs_reply": true,
        "sentiment": "neutral",
        "suggested_tags": ["roadmap", "follow-up"],
        "tl_dr": "Priya wants roadmap feedback before Friday.",
    })
}

fn no_cancel() -> CancellationToken {
    CancellationToken::new()
}

// ---------------------------------------------------------------------------
// build_request: structured output, never regex
// ---------------------------------------------------------------------------

#[tokio::test]
async fn build_request_uses_the_configured_model_and_a_json_schema_output_format() {
    let fx = Fixture::open().await;
    let h = TriagePassHandler::new(fx.db.clone(), "claude-haiku-4-5");
    let content = MessageContent {
        message_id: 1,
        account_id: 1,
        subject: Some("Hello".to_owned()),
        from_name: Some("Alice".to_owned()),
        from_addr: Some("alice@example.com".to_owned()),
        body: "Can you review this by Friday?".to_owned(),
        truncated: false,
        attachments_included: false,
    };

    let request = h.build_request(&content).unwrap();

    assert_eq!(request.model, "claude-haiku-4-5");
    assert_eq!(request.messages.len(), 1);
    assert!(request.messages[0].content.contains("alice@example.com"));
    let format = request
        .output_format
        .expect("triage must constrain output via output_config.format");
    let schema = format.schema;
    assert_eq!(schema["type"], "object");
    assert_eq!(schema["additionalProperties"], false);
    for field in [
        "category",
        "priority",
        "needs_reply",
        "sentiment",
        "suggested_tags",
        "tl_dr",
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
}

#[tokio::test]
async fn build_request_notes_a_truncated_body_and_honors_a_custom_token_ceiling() {
    let fx = Fixture::open().await;
    let h = TriagePassHandler::new(fx.db.clone(), "claude-haiku-4-5").with_max_tokens(256);
    let content = MessageContent {
        message_id: 1,
        account_id: 1,
        subject: None,
        from_name: None,
        from_addr: None,
        body: "the body was cut here".to_owned(),
        truncated: true,
        attachments_included: false,
    };

    let request = h.build_request(&content).unwrap();

    assert_eq!(
        request.max_tokens, 256,
        "with_max_tokens must override DEFAULT_MAX_TOKENS"
    );
    assert!(
        request.messages[0].content.contains("[body truncated]"),
        "a truncated body must tell the model not to draw conclusions from a \
         body that just stops mid-sentence: {:?}",
        request.messages[0].content
    );
    assert!(
        request.messages[0].content.contains("(no subject)"),
        "an absent subject/sender must render as a placeholder, not panic or vanish"
    );
}

// ---------------------------------------------------------------------------
// TriageResult::parse: schema-invalid is a hard error, never a partial value
// ---------------------------------------------------------------------------

#[test]
fn a_response_that_is_not_json_fails_to_parse() {
    let err = TriageResult::parse("not json at all").unwrap_err();
    assert_eq!(err.reason(), crate::ErrorReason::Internal);
}

#[test]
fn an_out_of_vocabulary_category_fails_to_parse() {
    let mut value = valid_triage_json();
    value["category"] = json!("not-a-real-category");
    let err = TriageResult::parse(&value.to_string()).unwrap_err();
    assert_eq!(err.reason(), crate::ErrorReason::Internal);
}

#[test]
fn an_out_of_vocabulary_priority_fails_to_parse() {
    let mut value = valid_triage_json();
    value["priority"] = json!("urgent");
    let err = TriageResult::parse(&value.to_string()).unwrap_err();
    assert_eq!(err.reason(), crate::ErrorReason::Internal);
}

#[test]
fn a_well_formed_response_parses_completely() {
    let value = valid_triage_json();
    let result = TriageResult::parse(&value.to_string()).unwrap();
    assert_eq!(result.category, "work");
    assert_eq!(result.priority, "high");
    assert!(result.needs_reply);
    assert_eq!(result.sentiment, "neutral");
    assert_eq!(result.suggested_tags, vec!["roadmap", "follow-up"]);
    assert!(result.tl_dr.contains("roadmap"));
}

#[test]
fn more_than_five_suggested_tags_is_truncated_not_rejected() {
    // The prompt says "zero to five"; nothing in the schema itself can
    // enforce `maxItems` (Anthropic's `output_config.format` subset has no
    // array-length constraint), so `parse` is where the ceiling is applied.
    // A model that over-generates tags is not a broken structured-output
    // contract worth dead-lettering the whole message over.
    let mut value = valid_triage_json();
    value["suggested_tags"] = json!(["a", "b", "c", "d", "e", "f", "g"]);
    let result = TriageResult::parse(&value.to_string()).unwrap();
    assert_eq!(result.suggested_tags, vec!["a", "b", "c", "d", "e"]);
}

// ---------------------------------------------------------------------------
// End-to-end via AiWorkerPool: the real pipeline, a real HTTP mock
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dispatch_writes_a_complete_ai_summaries_row_and_populates_ai_fts() {
    let fx = Fixture::open().await;
    let id = fx
        .message("Can we push the roadmap review to next Friday? Let me know.")
        .await;
    fx.queue
        .enqueue(vec![NewAiJob::new(id, fx.account_id, PASS)])
        .await
        .unwrap();

    let server = Server::json(200, triage_message_body(&valid_triage_json())).await;
    let pool = AiWorkerPool::new(
        fx.db.clone(),
        fx.queue.clone(),
        provider(&server),
        Arc::new(open_policy()),
        high_rpm_limits(),
        AiPrivacy::default(),
        vec![handler(&fx.db) as Arc<dyn PassHandler>],
        "test-worker",
    );

    let summary = pool.dispatch_pending(10, &no_cancel()).await.unwrap();
    assert_eq!(summary.completed, 1, "{summary:?}");

    // The wire request carried a JSON-schema output constraint, never a
    // prompt asking the model to "reply in JSON" that a caller would then
    // have to regex out of prose.
    let seen = server.requests();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0].body["output_config"]["format"]["type"],
        "json_schema"
    );
    assert_eq!(
        seen[0].body["output_config"]["format"]["schema"]["additionalProperties"],
        false
    );

    let row = fx
        .db
        .with_read(move |conn| {
            conn.query_row(
                "SELECT category, priority, needs_reply, sentiment, tl_dr, suggested_tags, \
                        ledger_entry_id, schema_version
                 FROM ai_summaries WHERE message_id = ?1 AND pass = 'triage'",
                [id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, bool>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, Option<i64>>(6)?,
                        row.get::<_, i64>(7)?,
                    ))
                },
            )
        })
        .unwrap();
    let (category, priority, needs_reply, sentiment, tl_dr, tags, ledger_entry_id, schema_version) =
        row;
    assert_eq!(category, "work");
    assert_eq!(priority, "high");
    assert!(needs_reply);
    assert_eq!(sentiment, "neutral");
    assert!(tl_dr.contains("roadmap"));
    assert!(tags.contains("follow-up"));
    assert!(
        ledger_entry_id.is_some(),
        "every ai_summaries row must trace back to the ledger entry that produced it"
    );
    assert_eq!(schema_version, 1);

    // Traces to the audit ledger: the same call that wrote this row.
    let ledger_model: String = fx
        .db
        .with_read(move |conn| {
            conn.query_row(
                "SELECT model FROM ai_ledger WHERE id = ?1",
                [ledger_entry_id.unwrap()],
                |row| row.get(0),
            )
        })
        .unwrap();
    assert_eq!(ledger_model, "claude-haiku-4-5");

    // ai_fts is populated (kept in sync by V21's triggers, not application
    // code) and actually matches the tl_dr text.
    let fts_hits: i64 = fx
        .db
        .with_read(move |conn| {
            conn.query_row(
                "SELECT count(*) FROM ai_fts WHERE rowid = ?1 AND ai_fts MATCH 'roadmap'",
                [id],
                |row| row.get(0),
            )
        })
        .unwrap();
    assert_eq!(fts_hits, 1, "ai_fts must contain and match the tl_dr text");
}

#[tokio::test]
async fn a_schema_invalid_response_is_dead_lettered_not_a_partial_row() {
    let fx = Fixture::with_options(Fixture::queue_options_max_attempts_1()).await;
    let id = fx
        .message("Whatever the model says, this must not partially write.")
        .await;
    fx.queue
        .enqueue(vec![NewAiJob::new(id, fx.account_id, PASS)])
        .await
        .unwrap();

    // The wrapper message is well-formed (a real 200, a real stop_reason) --
    // only the *content* fails to match the triage schema, exactly the
    // "provider succeeded, the answer itself was bad" case this module's
    // docs describe.
    let server = Server::json(200, triage_message_body(&json!("not an object at all"))).await;
    let pool = AiWorkerPool::new(
        fx.db.clone(),
        fx.queue.clone(),
        provider(&server),
        Arc::new(open_policy()),
        high_rpm_limits(),
        AiPrivacy::default(),
        vec![handler(&fx.db) as Arc<dyn PassHandler>],
        "test-worker",
    );

    let summary = pool.dispatch_pending(10, &no_cancel()).await.unwrap();
    assert_eq!(
        summary.dead, 1,
        "a schema-invalid answer must be dead-letterable, not silently dropped: {summary:?}"
    );

    let rows: i64 = fx
        .db
        .with_read(move |conn| {
            conn.query_row(
                "SELECT count(*) FROM ai_summaries WHERE message_id = ?1",
                [id],
                |row| row.get(0),
            )
        })
        .unwrap();
    assert_eq!(
        rows, 0,
        "a schema-invalid response must never leave a partially-populated row"
    );

    let stats = fx.queue.stats().await.unwrap();
    assert_eq!(stats.dead, 1);
    let dead = fx.queue.dead_letters(10).await.unwrap();
    assert_eq!(dead.len(), 1);
    assert!(
        dead[0]
            .last_error
            .as_deref()
            .unwrap_or_default()
            .contains("schema"),
        "the dead-letter reason should say the structured output was the problem: {:?}",
        dead[0].last_error
    );
}

// ---------------------------------------------------------------------------
// V21's ai_fts triggers: the migration's own claims, proven directly.
// dispatch_writes_a_complete_ai_summaries_row_and_populates_ai_fts above only
// exercises the single-insert case; this drives every scenario the
// migration's comments describe.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ai_fts_stays_in_sync_across_multiple_passes_upserts_deletes_and_message_cascade() {
    let fx = Fixture::open().await;
    let id = fx
        .message("whatever the wire body is, irrelevant here")
        .await;

    // 1. First row (triage): ai_fts gains exactly one row, matching its text.
    fx.insert_pass(id, "triage", "roadmap tldr").await;
    assert_eq!(fx.ai_fts_row_count(id).await, 1);
    assert!(fx.ai_fts_matches(id, "roadmap").await);

    // 2. A second row (a future deep pass) for the SAME message folds into
    //    the same rowid rather than creating a second one.
    fx.insert_pass(id, "deep", "quarterly numbers").await;
    assert_eq!(
        fx.ai_fts_row_count(id).await,
        1,
        "one ai_fts row per message, not per ai_summaries row"
    );
    assert!(
        fx.ai_fts_matches(id, "roadmap").await,
        "the triage row's tokens survive the deep row's insert"
    );
    assert!(
        fx.ai_fts_matches(id, "quarterly").await,
        "the deep row's tokens are folded in"
    );

    // 3. Upsert the triage row (same message_id/pass/model) with different
    //    text -- the superseded tl_dr's tokens must be gone, not merely
    //    supplemented (delete-then-reinsert, not update-in-place).
    fx.insert_pass(id, "triage", "budget review").await;
    assert!(
        !fx.ai_fts_matches(id, "roadmap").await,
        "the superseded tl_dr must not still match"
    );
    assert!(fx.ai_fts_matches(id, "budget").await);
    assert!(
        fx.ai_fts_matches(id, "quarterly").await,
        "the sibling deep row is untouched by the triage row's upsert"
    );

    // 4. Delete one of the two rows -- the survivor's tokens remain indexed.
    fx.delete_pass(id, "deep").await;
    assert_eq!(fx.ai_fts_row_count(id).await, 1);
    assert!(fx.ai_fts_matches(id, "budget").await);
    assert!(!fx.ai_fts_matches(id, "quarterly").await);

    // 5. Delete the last remaining row -- ai_fts must go to zero rows, not
    //    leave a phantom always-absent-but-present row (the `HAVING count(*)
    //    > 0` guard in the delete trigger).
    fx.delete_pass(id, "triage").await;
    assert_eq!(fx.ai_summaries_count(id).await, 0);
    assert_eq!(
        fx.ai_fts_row_count(id).await,
        0,
        "no phantom ai_fts row once every pass for this message is gone"
    );

    // 6. `DELETE FROM messages` cascades to `ai_summaries` (ON DELETE
    //    CASCADE), which must in turn clean up `ai_fts` via the same delete
    //    trigger.
    fx.insert_pass(id, "triage", "final tldr before delete")
        .await;
    assert_eq!(fx.ai_fts_row_count(id).await, 1);
    fx.delete_message(id).await;
    assert_eq!(
        fx.ai_summaries_count(id).await,
        0,
        "ON DELETE CASCADE must remove ai_summaries rows when the message is deleted"
    );
    assert_eq!(
        fx.ai_fts_row_count(id).await,
        0,
        "the cascaded ai_summaries delete must reach ai_fts too"
    );
}

// ---------------------------------------------------------------------------
// Batch path: the same handler, via BatchCoordinator
// ---------------------------------------------------------------------------

fn batch_coordinator(
    fx: &Fixture,
    endpoint: &str,
    handler: Arc<TriagePassHandler>,
) -> BatchCoordinator {
    let client = BatchClient::new().unwrap().with_endpoint(endpoint);
    BatchCoordinator::new(
        fx.db.clone(),
        fx.queue.clone(),
        client,
        "printf secret-key",
        Arc::new(open_policy()),
        high_rpm_limits(),
        AiPrivacy::default(),
        AiBatching {
            enabled: true,
            threshold: 1,
            max_batch: 10,
        },
        vec![handler as Arc<dyn PassHandler>],
    )
    .unwrap()
}

#[tokio::test]
async fn batch_submission_and_poll_write_the_same_ai_summaries_row() {
    let fx = Fixture::open().await;
    let id = fx
        .message("Quarterly numbers are attached, no action needed.")
        .await;
    fx.queue
        .enqueue(vec![NewAiJob::new(id, fx.account_id, PASS)])
        .await
        .unwrap();

    let submit_body = json!({"id": "batch_triage", "processing_status": "in_progress"}).to_string();
    let status_body = json!({
        "id": "batch_triage",
        "processing_status": "ended",
        "request_counts": {"processing": 0, "succeeded": 1, "errored": 0, "canceled": 0, "expired": 0},
    })
    .to_string();
    let batch_triage_json = json!({
        "category": "notification",
        "priority": "low",
        "needs_reply": false,
        "sentiment": "neutral",
        "suggested_tags": [],
        "tl_dr": "FYI quarterly numbers, no reply needed.",
    });
    let results_body = format!(
        "{}\n",
        json!({
            "custom_id": id.to_string(),
            "result": {
                "type": "succeeded",
                "message": {
                    "id": "msg_batch",
                    "model": "claude-haiku-4-5",
                    "content": [{"type": "text", "text": batch_triage_json.to_string()}],
                    "stop_reason": "end_turn",
                    "usage": usage_json(),
                },
            },
        })
    );
    let http = Server::queued(vec![
        (200, submit_body),
        (200, status_body),
        (200, results_body),
    ])
    .await;

    let coord = batch_coordinator(&fx, &http.endpoint, handler(&fx.db));
    let batch_id = coord.maybe_submit(PASS).await.unwrap().unwrap();
    let outcome = coord.poll(&batch_id).await.unwrap();
    let BatchPollOutcome::Completed(summary) = outcome else {
        unreachable!("expected the batch to have ended, got {outcome:?}");
    };
    assert_eq!(summary.completed, 1, "{summary:?}");

    let category: String = fx
        .db
        .with_read(move |conn| {
            conn.query_row(
                "SELECT category FROM ai_summaries WHERE message_id = ?1 AND pass = 'triage'",
                [id],
                |row| row.get(0),
            )
        })
        .unwrap();
    assert_eq!(category, "notification");
}

// ---------------------------------------------------------------------------
// The `ai:` search operators, end to end: real QueryPlanner + real
// StructuredRetriever against real ai_summaries rows.
// ---------------------------------------------------------------------------

async fn planner(db: &Database) -> QueryPlanner {
    QueryPlanner::new(
        db.clone(),
        Arc::new(HashEmbedder::new(64)),
        ExpansionConfig::default(),
    )
}

#[tokio::test]
async fn ai_needs_reply_selects_only_flagged_messages() {
    let fx = Fixture::open().await;
    let flagged = fx
        .message("Please confirm you can make the call tomorrow.")
        .await;
    let not_flagged = fx.message("Here is your receipt, no action needed.").await;
    let no_summary_yet = fx.message("Just synced, not triaged yet.").await;
    fx.insert_summary(
        flagged,
        Some(true),
        Some("work"),
        Some("high"),
        Some("neutral"),
    )
    .await;
    fx.insert_summary(
        not_flagged,
        Some(false),
        Some("receipt"),
        Some("low"),
        Some("neutral"),
    )
    .await;
    let _ = no_summary_yet;

    let planner = planner(&fx.db).await;
    let plan = planner.plan("ai:needs-reply").await.unwrap();
    let hits = StructuredRetriever::new(fx.db.clone())
        .retrieve(&plan, 100, &no_cancel())
        .await
        .unwrap();

    assert_eq!(
        hits.into_iter().map(|c| c.message_id).collect::<Vec<_>>(),
        vec![flagged],
        "only the message triage actually flagged needs_reply=1 must match"
    );
}

#[tokio::test]
async fn ai_priority_greater_than_selects_by_ordinal_not_by_string() {
    let fx = Fixture::open().await;
    let critical = fx.message("Production is down, page on-call now.").await;
    let normal = fx.message("Weekly digest, nothing urgent.").await;
    let low = fx.message("Newsletter: this week in tech.").await;
    fx.insert_summary(
        critical,
        Some(false),
        Some("work"),
        Some("critical"),
        Some("urgent"),
    )
    .await;
    fx.insert_summary(
        normal,
        Some(false),
        Some("work"),
        Some("normal"),
        Some("neutral"),
    )
    .await;
    fx.insert_summary(
        low,
        Some(false),
        Some("newsletter"),
        Some("low"),
        Some("neutral"),
    )
    .await;

    let planner = planner(&fx.db).await;
    let plan = planner.plan("ai:priority>normal").await.unwrap();
    let hits = StructuredRetriever::new(fx.db.clone())
        .retrieve(&plan, 100, &no_cancel())
        .await
        .unwrap();

    assert_eq!(
        hits.into_iter().map(|c| c.message_id).collect::<Vec<_>>(),
        vec![critical],
        "'high' and 'critical' both rank above 'normal' as strings would sort wrong \
         ('critical' < 'normal' < 'low' lexicographically) -- only the ordinal comparison \
         gets this right"
    );
}

#[tokio::test]
async fn ai_category_equals_matches_exactly_case_insensitively() {
    let fx = Fixture::open().await;
    let invoice = fx.message("Invoice #4471 due on the 15th.").await;
    let receipt = fx
        .message("Thanks for your purchase, here is your receipt.")
        .await;
    fx.insert_summary(
        invoice,
        Some(true),
        Some("invoice"),
        Some("normal"),
        Some("neutral"),
    )
    .await;
    fx.insert_summary(
        receipt,
        Some(false),
        Some("receipt"),
        Some("low"),
        Some("neutral"),
    )
    .await;

    let planner = planner(&fx.db).await;
    let plan = planner.plan("ai:category:Invoice").await.unwrap();
    let hits = StructuredRetriever::new(fx.db.clone())
        .retrieve(&plan, 100, &no_cancel())
        .await
        .unwrap();

    assert_eq!(
        hits.into_iter().map(|c| c.message_id).collect::<Vec<_>>(),
        vec![invoice]
    );
}

#[tokio::test]
async fn ai_sentiment_equals_matches_exactly() {
    let fx = Fixture::open().await;
    let angry = fx
        .message("This is the third time my order was late.")
        .await;
    let fine = fx.message("Thanks, all good on my end.").await;
    fx.insert_summary(
        angry,
        Some(true),
        Some("notification"),
        Some("high"),
        Some("negative"),
    )
    .await;
    fx.insert_summary(
        fine,
        Some(false),
        Some("personal"),
        Some("low"),
        Some("positive"),
    )
    .await;

    let planner = planner(&fx.db).await;
    let plan = planner.plan("ai:sentiment:negative").await.unwrap();
    let hits = StructuredRetriever::new(fx.db.clone())
        .retrieve(&plan, 100, &no_cancel())
        .await
        .unwrap();

    assert_eq!(
        hits.into_iter().map(|c| c.message_id).collect::<Vec<_>>(),
        vec![angry]
    );
}

#[tokio::test]
async fn ai_filters_conjoin_with_ordinary_operators() {
    let fx = Fixture::open().await;
    let matches_both = fx.message("Can you approve the invoice today?").await;
    let wrong_sender = fx.message("Can you approve this invoice too?").await;
    fx.db
        .write(move |c| {
            c.execute(
                "UPDATE messages SET from_addr = 'billing@example.com' WHERE id = ?1",
                [wrong_sender],
            )
        })
        .await
        .unwrap();
    fx.insert_summary(
        matches_both,
        Some(true),
        Some("invoice"),
        Some("high"),
        Some("neutral"),
    )
    .await;
    fx.insert_summary(
        wrong_sender,
        Some(true),
        Some("invoice"),
        Some("high"),
        Some("neutral"),
    )
    .await;

    let planner = planner(&fx.db).await;
    let plan = planner.plan("from:pm ai:needs-reply").await.unwrap();
    let hits = StructuredRetriever::new(fx.db.clone())
        .retrieve(&plan, 100, &no_cancel())
        .await
        .unwrap();

    assert_eq!(
        hits.into_iter().map(|c| c.message_id).collect::<Vec<_>>(),
        vec![matches_both],
        "ai: must conjoin with other hard filters, not override them"
    );
}

#[tokio::test]
async fn negating_ai_needs_reply_includes_messages_with_no_triage_row_at_all() {
    // NULL-safe negation: a message never triaged yet has zero
    // `ai_summaries` rows, so the *positive* predicate's `EXISTS` is false
    // for it (never NULL, since EXISTS itself never yields NULL) — `-ai:
    // needs-reply` must include it, the same "don't drop what you can't
    // answer" rule every other negated hard filter in this codebase follows.
    let fx = Fixture::open().await;
    let flagged = fx
        .message("Please confirm you can make the call tomorrow.")
        .await;
    let not_flagged = fx.message("Here is your receipt, no action needed.").await;
    let never_triaged = fx
        .message("Just synced, nothing has looked at this yet.")
        .await;
    fx.insert_summary(
        flagged,
        Some(true),
        Some("work"),
        Some("high"),
        Some("neutral"),
    )
    .await;
    fx.insert_summary(
        not_flagged,
        Some(false),
        Some("receipt"),
        Some("low"),
        Some("neutral"),
    )
    .await;

    let planner = planner(&fx.db).await;
    let plan = planner.plan("-ai:needs-reply").await.unwrap();
    let mut hits = StructuredRetriever::new(fx.db.clone())
        .retrieve(&plan, 100, &no_cancel())
        .await
        .unwrap()
        .into_iter()
        .map(|c| c.message_id)
        .collect::<Vec<_>>();
    hits.sort_unstable();

    let mut expected = vec![not_flagged, never_triaged];
    expected.sort_unstable();
    assert_eq!(hits, expected);
}
