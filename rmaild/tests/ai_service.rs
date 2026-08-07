//! Integration test: drive `AiService` end-to-end against an in-process tonic
//! server over a Unix domain socket, backed by a hand-rolled [`MockProvider`]
//! rather than the real Anthropic endpoint (`ClaudeProvider`'s endpoint is
//! not configurable at the `Config` level — see `rmail_core::ai::provider`'s
//! own docs — so no path through the real daemon-boot wiring is hermetic;
//! this test builds `AiApi` directly instead, the same "fake the one
//! network-facing dependency, wire everything else for real" discipline
//! `mail_service.rs`'s own tests use for `ImapMutator`).
//!
//! Covers the three behaviors named in this task's `verify` line — a cached
//! `GetSummary`, a forced `AnalyzeMessage` stream, and `StreamEnrichments`'
//! resume-by-message_id — plus `SuggestReply`, `GetUsage`, and `SetPaused`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use rmail_core::ai::provider::{ChatResponse, StopReason, StreamFrame, Usage as CoreUsage};
use rmail_core::ai::queue::QueueOptions;
use rmail_core::ai::{
    AiPauseFlag, AiQueue, ChatRequest, DeepPassHandler, NewAiJob, PolicyEngine, Provider,
    ProviderStream,
};
use rmail_core::config::{AiDeepPass, AiLimits, AiPolicyMode, AiPolicyRule, AiPrivacy, OnCap};
use rmail_core::events::{EventKind as CoreKind, EventLog, NewEvent, Retention};
use rmail_core::index::{IndexQueue, QueueOptions as IndexQueueOptions};
use rmail_core::repo;
use rmail_core::sync::{SyncEngine, SyncOptions};
use rmail_core::Config;
use rmail_core::Database;
use rmail_core::Error as AiError;
use rmail_proto::v1::ai_service_client::AiServiceClient;
use rmail_proto::v1::{
    analyze_event, AnalyzeMessageRequest, GetSummaryRequest, GetUsageRequest, SetPausedRequest,
    StreamEnrichmentsRequest, SuggestReplyRequest, SummaryStatus,
};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::UnixListenerStream;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;
use tonic::transport::{Channel, Server};
use tonic::Code;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// How long a stream assertion waits before failing — generous, since these
/// are liveness checks on spawned tasks, not latency measurements.
const STREAM_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// MockProvider: scriptable completions and streams, with cancellation
// observability
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum ScriptedComplete {
    Ok(String),
    Unavailable,
}

/// A hand-rolled [`Provider`] whose `complete`/`stream` answers are queued up
/// front by each test, mirroring `rmail-core::ai::queue::tests`' own
/// `MockProvider` — duplicated rather than shared (that one is private to a
/// crate this one only depends on, not exported for reuse).
#[derive(Debug, Default)]
struct MockProvider {
    completions: Mutex<VecDeque<ScriptedComplete>>,
    /// Frames for the next `stream()` call, sent with a small delay between
    /// each so a test has a real window to disconnect mid-stream.
    stream_scripts: Mutex<VecDeque<Vec<StreamFrame>>>,
    complete_calls: AtomicU32,
    stream_calls: AtomicU32,
    /// Set by the spawned frame-sender if it observed cancellation before
    /// finishing its script — the proof that "abort upstream on cancel"
    /// actually reaches the provider, not just the local relay.
    stream_cancelled: Arc<AtomicBool>,
}

impl MockProvider {
    fn queue_complete(&self, reply: ScriptedComplete) {
        self.completions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_back(reply);
    }

    fn queue_stream(&self, frames: Vec<StreamFrame>) {
        self.stream_scripts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_back(frames);
    }

    fn complete_calls(&self) -> u32 {
        self.complete_calls.load(Ordering::SeqCst)
    }

    fn stream_calls(&self) -> u32 {
        self.stream_calls.load(Ordering::SeqCst)
    }

    fn stream_was_cancelled(&self) -> bool {
        self.stream_cancelled.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Provider for MockProvider {
    async fn complete(
        &self,
        _request: &ChatRequest,
        _cancel: &CancellationToken,
    ) -> Result<ChatResponse, AiError> {
        self.complete_calls.fetch_add(1, Ordering::SeqCst);
        let next = self
            .completions
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front();
        match next {
            Some(ScriptedComplete::Ok(text)) => Ok(ChatResponse {
                id: "msg_mock".to_owned(),
                model: "mock-deep-model".to_owned(),
                stop_reason: StopReason::EndTurn,
                text,
                usage: CoreUsage::default(),
            }),
            Some(ScriptedComplete::Unavailable) | None => Err(AiError::unavailable(
                "mock provider: no scripted reply".to_owned(),
            )),
        }
    }

    async fn stream(
        &self,
        _request: &ChatRequest,
        cancel: &CancellationToken,
    ) -> Result<ProviderStream, AiError> {
        self.stream_calls.fetch_add(1, Ordering::SeqCst);
        let frames = self
            .stream_scripts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .pop_front()
            .unwrap_or_default();
        let cancelled_flag = Arc::clone(&self.stream_cancelled);
        let cancel = cancel.clone();
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<StreamFrame, AiError>>(8);
        tokio::spawn(async move {
            for frame in frames {
                tokio::select! {
                    () = cancel.cancelled() => {
                        cancelled_flag.store(true, Ordering::SeqCst);
                        return;
                    }
                    sent = tx.send(Ok(frame)) => {
                        if sent.is_err() {
                            return;
                        }
                    }
                }
                // A real network stream does not deliver every frame in the
                // same tick; this gap is what gives a test a real window to
                // disconnect mid-stream rather than racing a completed one.
                tokio::select! {
                    () = cancel.cancelled() => {
                        cancelled_flag.store(true, Ordering::SeqCst);
                        return;
                    }
                    () = tokio::time::sleep(Duration::from_millis(60)) => {}
                }
            }
        });
        Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }
}

fn valid_deep_json(summary: &str) -> String {
    serde_json::json!({
        "summary": summary,
        "key_points": ["Point A"],
        "todos": [],
        "entities": [],
        "suggested_reply": null,
        "thread_summary": summary,
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// Test server
// ---------------------------------------------------------------------------

struct TestServer {
    socket: PathBuf,
    db_path: PathBuf,
    db: Database,
    events: EventLog,
    queue: AiQueue,
    pause: AiPauseFlag,
    provider: Arc<MockProvider>,
    shutdown: oneshot::Sender<()>,
    handle: JoinHandle<()>,
}

impl TestServer {
    async fn start() -> Self {
        Self::start_with_policy(Vec::new()).await
    }

    /// A server whose `ai.policy` rules are exactly `rules` (default mode
    /// `Allowed`) — for the policy-forbidden acceptance case.
    async fn start_with_policy(rules: Vec<AiPolicyRule>) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let socket = PathBuf::from("/tmp").join(format!("rmail-ai-{pid}-{n}.sock"));
        let db_path = std::env::temp_dir().join(format!("rmail-ai-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", db_path.display())));
        }
        let _ = std::fs::remove_file(&socket);

        let db = Database::open(&db_path).unwrap();
        let events = EventLog::new(db.clone(), Retention::unlimited());
        let queue = AiQueue::new(db.clone(), QueueOptions::default());
        let index_queue = IndexQueue::new(db.clone(), IndexQueueOptions::default());
        let deep = Arc::new(DeepPassHandler::new(
            db.clone(),
            index_queue,
            "mock-deep-model",
            AiDeepPass::default(),
        ));
        // `PolicyEngine::new` is `#[cfg(test)]` *inside* `rmail-core` — a
        // dependency's `#[cfg(test)]` items are never visible from another
        // crate's tests, cfg-gated or not, so an integration test here must
        // go through the real `from_config` path, the same one production
        // code uses.
        let mut policy_config = rmail_core::Config::default();
        policy_config.ai.policy.rules = rules;
        let policy = Arc::new(PolicyEngine::from_config(&policy_config).unwrap());
        let provider = Arc::new(MockProvider::default());
        let pause = AiPauseFlag::default();
        let limits = AiLimits {
            max_concurrency: 4,
            requests_per_minute: 1_000_000,
            daily_token_cap: 1_000_000_000,
            daily_cost_cap_usd: 1_000.0,
            monthly_cost_cap_usd: 1_000.0,
            on_cap: OnCap::Pause,
        };
        let shutdown_cancel = CancellationToken::new();

        let provider_dyn: Arc<dyn Provider> = provider.clone();
        // A fresh semaphore/rate-limiter, sized off the same `limits` --
        // mirroring what a real `AiWorkerPool::new` builds internally, since
        // this harness never constructs one (see this file's own module
        // docs on why `AiApi` is built directly here instead).
        let semaphore = Arc::new(tokio::sync::Semaphore::new(limits.max_concurrency as usize));
        let rate_limiter = Arc::new(rmail_core::ai::RateLimiter::new(limits.requests_per_minute));
        let api = rmaild::AiApi::new(
            db.clone(),
            queue.clone(),
            events.clone(),
            deep,
            provider_dyn,
            policy,
            AiPrivacy::default(),
            limits,
            pause.clone(),
            true,
            semaphore,
            rate_limiter,
            shutdown_cancel.clone(),
        );

        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let incoming = UnixListenerStream::new(listener);
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            let _ = Server::builder()
                .add_service(rmail_proto::v1::ai_service_server::AiServiceServer::new(
                    api,
                ))
                .serve_with_incoming_shutdown(incoming, async move {
                    let _ = shutdown_rx.await;
                    shutdown_cancel.cancel();
                })
                .await;
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
            db,
            events,
            queue,
            pause,
            provider,
            shutdown: shutdown_tx,
            handle,
        }
    }

    async fn client(&self) -> AiServiceClient<Channel> {
        AiServiceClient::new(rmail_core::connect_uds(&self.socket).await.unwrap())
    }

    /// An account with one mailbox named `mailbox_name`.
    async fn account(&self, mailbox_name: &str) -> (i64, i64) {
        let mailbox_name = mailbox_name.to_owned();
        self.db
            .write(move |c| {
                let account_id = repo::insert_account(
                    c,
                    &repo::NewAccount {
                        name: "Personal".to_owned(),
                        ..Default::default()
                    },
                )?;
                let mailbox_id = repo::insert_mailbox(
                    c,
                    &repo::NewMailbox {
                        account_id,
                        name: mailbox_name,
                        ..Default::default()
                    },
                )?;
                Ok((account_id, mailbox_id))
            })
            .await
            .unwrap()
    }

    async fn message(&self, account_id: i64, mailbox_id: i64, uid: i64, body: &str) -> i64 {
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
                        subject: Some("Test message".to_owned()),
                        body_text: Some(body),
                        ..Default::default()
                    },
                )
            })
            .await
            .unwrap()
    }

    /// Write a triage `ai_summaries` row directly — standing in for what
    /// `TriagePassHandler::on_success` would have persisted, without
    /// running the whole redact/provider/audit pipeline for a fixture.
    async fn write_triage_row(&self, message_id: i64, account_id: i64, tl_dr: &str) {
        let tl_dr = tl_dr.to_owned();
        self.db
            .write(move |c| {
                c.execute(
                    "INSERT INTO ai_summaries
                         (message_id, account_id, model, pass, schema_version, tl_dr, sentiment,
                          category, priority, needs_reply, suggested_tags)
                     VALUES (?1, ?2, 'mock-triage-model', 'triage', 1, ?3, 'neutral', 'work', \
                              'normal', 0, '[]')",
                    rusqlite::params![message_id, account_id, tl_dr],
                )
            })
            .await
            .unwrap();
    }

    /// Write a deep `ai_summaries` row directly, and publish the matching
    /// `AiSummary` event — the two things `ai_service::announce`/
    /// `ai::queue::worker::finish_call` do together in production, mirrored
    /// here so `StreamEnrichments` has something real to observe.
    async fn write_deep_row_and_announce(&self, message_id: i64, account_id: i64, summary: &str) {
        let summary_owned = summary.to_owned();
        self.db
            .write(move |c| {
                c.execute(
                    "INSERT INTO ai_summaries
                         (message_id, account_id, model, pass, schema_version, summary,
                          thread_summary, key_points, todos)
                     VALUES (?1, ?2, 'mock-deep-model', 'deep', 1, ?3, ?3, '[]', '[]')",
                    rusqlite::params![message_id, account_id, summary_owned],
                )
            })
            .await
            .unwrap();
        self.events
            .append(
                NewEvent::new(CoreKind::AiSummary)
                    .account(account_id)
                    .message(message_id)
                    .payload(serde_json::json!({ "pass": "deep" })),
            )
            .await
            .unwrap();
    }

    async fn stop(self) {
        let _ = self.shutdown.send(());
        let _ = tokio::time::timeout(Duration::from_secs(10), self.handle).await;
        for suffix in ["", "-wal", "-shm"] {
            let _ =
                std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.db_path.display())));
        }
        let _ = std::fs::remove_file(&self.socket);
    }
}

// ---------------------------------------------------------------------------
// GetSummary — cached reads, never calling the provider
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_summary_merges_triage_and_deep_rows() {
    let server = TestServer::start().await;
    let (account_id, mailbox_id) = server.account("INBOX").await;
    let id = server.message(account_id, mailbox_id, 1, "hello").await;
    server
        .write_triage_row(id, account_id, "Quick heads up")
        .await;
    server
        .write_deep_row_and_announce(id, account_id, "A longer synopsis of the message.")
        .await;

    let summary = server
        .client()
        .await
        .get_summary(GetSummaryRequest { message_id: id })
        .await
        .unwrap()
        .into_inner();

    assert_eq!(summary.status(), SummaryStatus::Ok);
    assert_eq!(summary.tl_dr.as_deref(), Some("Quick heads up"));
    assert_eq!(
        summary.summary.as_deref(),
        Some("A longer synopsis of the message.")
    );
    assert_eq!(summary.triage_model.as_deref(), Some("mock-triage-model"));
    assert_eq!(summary.deep_model.as_deref(), Some("mock-deep-model"));
    assert_eq!(
        server.provider.complete_calls(),
        0,
        "a cache read never calls the model"
    );
    assert_eq!(server.provider.stream_calls(), 0);

    server.stop().await;
}

#[tokio::test]
async fn get_summary_on_an_unqueued_message_is_not_queued() {
    let server = TestServer::start().await;
    let (account_id, mailbox_id) = server.account("INBOX").await;
    let id = server.message(account_id, mailbox_id, 1, "hello").await;

    let summary = server
        .client()
        .await
        .get_summary(GetSummaryRequest { message_id: id })
        .await
        .unwrap()
        .into_inner();

    assert_eq!(summary.status(), SummaryStatus::NotQueued);
    assert!(summary.tl_dr.is_none());
    assert!(summary.summary.is_none());

    server.stop().await;
}

#[tokio::test]
async fn get_summary_on_a_pending_message_is_pending() {
    let server = TestServer::start().await;
    let (account_id, mailbox_id) = server.account("INBOX").await;
    let id = server.message(account_id, mailbox_id, 1, "hello").await;
    server
        .queue
        .enqueue(vec![NewAiJob::new(id, account_id, "triage")])
        .await
        .unwrap();

    let summary = server
        .client()
        .await
        .get_summary(GetSummaryRequest { message_id: id })
        .await
        .unwrap()
        .into_inner();

    assert_eq!(summary.status(), SummaryStatus::Pending);

    server.stop().await;
}

#[tokio::test]
async fn get_summary_on_an_unknown_message_is_not_found() {
    let server = TestServer::start().await;

    let status = server
        .client()
        .await
        .get_summary(GetSummaryRequest { message_id: 9_999 })
        .await
        .expect_err("no such message");

    assert_eq!(status.code(), Code::NotFound);
    server.stop().await;
}

// ---------------------------------------------------------------------------
// AnalyzeMessage — forced, streamed, token-by-token
// ---------------------------------------------------------------------------

#[tokio::test]
async fn analyze_message_streams_tokens_and_persists_the_deep_result() {
    let server = TestServer::start().await;
    let (account_id, mailbox_id) = server.account("INBOX").await;
    let id = server
        .message(account_id, mailbox_id, 1, "Quarterly planning notes")
        .await;

    let json = valid_deep_json("A quick note about Q3.");
    let (first, second) = json.split_at(json.len() / 2);
    server.provider.queue_stream(vec![
        StreamFrame::Token(first.to_owned()),
        StreamFrame::Token(second.to_owned()),
        StreamFrame::Usage(CoreUsage {
            input_tokens: 10,
            output_tokens: 20,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        }),
        StreamFrame::Done {
            stop_reason: StopReason::EndTurn,
        },
    ]);

    let mut stream = server
        .client()
        .await
        .analyze_message(AnalyzeMessageRequest { message_id: id })
        .await
        .unwrap()
        .into_inner();

    let mut reconstructed = String::new();
    let mut saw_usage = false;
    // Assigned exactly once, in the `Done` arm below, which is the loop's
    // only `break` — never read uninitialized.
    let done_summary;
    loop {
        let next = tokio::time::timeout(STREAM_TIMEOUT, stream.next())
            .await
            .expect("stream should produce an event")
            .expect("stream ended early")
            .expect("stream returned an error");
        match next.event.unwrap() {
            analyze_event::Event::Token(t) => reconstructed.push_str(&t),
            analyze_event::Event::Usage(_) => saw_usage = true,
            analyze_event::Event::ToolUseStart(_) => {}
            analyze_event::Event::Done(done) => {
                assert_eq!(done.stop_reason, "end_turn");
                done_summary = done.result;
                break;
            }
        }
    }

    assert_eq!(
        reconstructed, json,
        "the concatenated tokens reproduce the full answer"
    );
    assert!(saw_usage, "a Usage frame must precede Done");
    let summary = done_summary.expect("Done must carry the persisted summary");
    assert_eq!(summary.status(), SummaryStatus::Ok);
    assert_eq!(summary.summary.as_deref(), Some("A quick note about Q3."));

    // And it is now durably readable via GetSummary too.
    let cached = server
        .client()
        .await
        .get_summary(GetSummaryRequest { message_id: id })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(cached.summary.as_deref(), Some("A quick note about Q3."));

    server.stop().await;
}

#[tokio::test]
async fn analyze_message_on_an_unknown_message_is_not_found_before_any_stream_opens() {
    let server = TestServer::start().await;

    let status = server
        .client()
        .await
        .analyze_message(AnalyzeMessageRequest { message_id: 9_999 })
        .await
        .expect_err("no such message");

    assert_eq!(status.code(), Code::NotFound);
    assert_eq!(server.provider.stream_calls(), 0);
    server.stop().await;
}

#[tokio::test]
async fn analyze_message_is_denied_when_policy_forbids_the_folder() {
    let server = TestServer::start_with_policy(vec![AiPolicyRule {
        account: None,
        folder: Some("Legal".to_owned()),
        mode: AiPolicyMode::Forbidden,
        residency: None,
        reason: None,
    }])
    .await;
    let (account_id, mailbox_id) = server.account("Legal").await;
    let id = server
        .message(account_id, mailbox_id, 1, "privileged material")
        .await;

    let status = server
        .client()
        .await
        .analyze_message(AnalyzeMessageRequest { message_id: id })
        .await
        .expect_err("the Legal folder is policy-forbidden");

    assert_eq!(status.code(), Code::FailedPrecondition);
    assert_eq!(
        server.provider.stream_calls(),
        0,
        "the provider must never be reached"
    );
    server.stop().await;
}

#[tokio::test]
async fn analyze_message_aborts_the_upstream_call_on_client_disconnect() {
    let server = TestServer::start().await;
    let (account_id, mailbox_id) = server.account("INBOX").await;
    let id = server
        .message(account_id, mailbox_id, 1, "Quarterly planning notes")
        .await;

    // Five frames at ~60ms apart gives ample window to disconnect mid-stream.
    server.provider.queue_stream(vec![
        StreamFrame::Token("one ".to_owned()),
        StreamFrame::Token("two ".to_owned()),
        StreamFrame::Token("three ".to_owned()),
        StreamFrame::Token("four ".to_owned()),
        StreamFrame::Token("five".to_owned()),
    ]);

    let mut stream = server
        .client()
        .await
        .analyze_message(AnalyzeMessageRequest { message_id: id })
        .await
        .unwrap()
        .into_inner();

    // Receive one token, then drop the stream — the client equivalent of
    // hanging up mid-analysis.
    let first = tokio::time::timeout(STREAM_TIMEOUT, stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(matches!(first.event, Some(analyze_event::Event::Token(_))));
    drop(stream);

    let mut cancelled = false;
    for _ in 0..100 {
        if server.provider.stream_was_cancelled() {
            cancelled = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        cancelled,
        "the mock provider's frame-sender must observe the cancellation token firing, \
         proving the upstream call is aborted rather than just the local relay stopping"
    );

    server.stop().await;
}

// ---------------------------------------------------------------------------
// StreamEnrichments — resume-by-message_id
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stream_enrichments_resumes_by_message_id_and_follows_the_live_tail() {
    let server = TestServer::start().await;
    let (account_id, mailbox_id) = server.account("INBOX").await;
    let first = server.message(account_id, mailbox_id, 1, "first").await;
    let second = server.message(account_id, mailbox_id, 2, "second").await;
    server
        .write_deep_row_and_announce(first, account_id, "first summary")
        .await;
    server
        .write_deep_row_and_announce(second, account_id, "second summary")
        .await;

    // Resuming after `first` must only replay `second` from the backlog.
    let mut stream = server
        .client()
        .await
        .stream_enrichments(StreamEnrichmentsRequest {
            account_id: 0,
            since_message_id: first,
        })
        .await
        .unwrap()
        .into_inner();

    let backlog_item = tokio::time::timeout(STREAM_TIMEOUT, stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(backlog_item.message_id, second);
    assert_eq!(backlog_item.pass, "deep");
    assert_eq!(
        backlog_item.summary.unwrap().summary.as_deref(),
        Some("second summary")
    );

    // Then a live enrichment for a brand-new message must also be delivered.
    let third = server.message(account_id, mailbox_id, 3, "third").await;
    server
        .write_deep_row_and_announce(third, account_id, "third summary")
        .await;

    let live_item = tokio::time::timeout(STREAM_TIMEOUT, stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(live_item.message_id, third);
    assert_eq!(
        live_item.summary.unwrap().summary.as_deref(),
        Some("third summary")
    );

    server.stop().await;
}

#[tokio::test]
async fn stream_enrichments_delivers_a_live_enrichment_for_an_older_message_id() {
    // The live tail must not drop an enrichment just because a *newer*
    // message's enrichment already advanced the cursor past it -- passes do
    // not complete in message_id order (a deep pass commonly finishes after
    // several newer messages' triage passes already have; per-thread deep
    // serialization only makes that more true), so gating live delivery on
    // "message_id >= cursor" would silently and permanently drop exactly
    // this case. This is the direct regression proof for that bug.
    let server = TestServer::start().await;
    let (account_id, mailbox_id) = server.account("INBOX").await;
    let older = server.message(account_id, mailbox_id, 1, "older").await;
    let newer = server.message(account_id, mailbox_id, 2, "newer").await;

    let mut stream = server
        .client()
        .await
        .stream_enrichments(StreamEnrichmentsRequest {
            account_id: 0,
            since_message_id: 0,
        })
        .await
        .unwrap()
        .into_inner();

    // The newer message's deep pass finishes first (its cursor is now 2),
    // then the older message's finishes second -- exactly the out-of-order
    // completion this test exists to prove is not dropped.
    server
        .write_deep_row_and_announce(newer, account_id, "newer summary")
        .await;
    let first = tokio::time::timeout(STREAM_TIMEOUT, stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(first.message_id, newer);

    server
        .write_deep_row_and_announce(older, account_id, "older summary")
        .await;
    let second = tokio::time::timeout(STREAM_TIMEOUT, stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        second.message_id, older,
        "an enrichment for a lower message_id must still be delivered live, \
         not silently dropped because a higher one already went out"
    );
    assert_eq!(
        second.summary.unwrap().summary.as_deref(),
        Some("older summary")
    );

    server.stop().await;
}

#[tokio::test]
async fn stream_enrichments_backlog_delivers_both_passes_for_one_message() {
    // A message with both a triage and a deep row must produce two backlog
    // `Enrichment`s, not one -- `backlog_page` groups by message id and
    // then emits per-pass, precisely so neither pass is silently dropped in
    // favor of the other (see that function's own docs on the page-boundary
    // bug this replaces).
    let server = TestServer::start().await;
    let (account_id, mailbox_id) = server.account("INBOX").await;
    let id = server.message(account_id, mailbox_id, 1, "hello").await;
    server.write_triage_row(id, account_id, "quick note").await;
    server
        .write_deep_row_and_announce(id, account_id, "full analysis")
        .await;

    let mut stream = server
        .client()
        .await
        .stream_enrichments(StreamEnrichmentsRequest {
            account_id: 0,
            since_message_id: 0,
        })
        .await
        .unwrap()
        .into_inner();

    let mut passes_seen = Vec::new();
    for _ in 0..2 {
        let item = tokio::time::timeout(STREAM_TIMEOUT, stream.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(item.message_id, id);
        // Both carry the merged view regardless of which pass triggered it.
        let summary = item.summary.unwrap();
        assert_eq!(summary.tl_dr.as_deref(), Some("quick note"));
        assert_eq!(summary.summary.as_deref(), Some("full analysis"));
        passes_seen.push(item.pass);
    }
    passes_seen.sort();
    assert_eq!(passes_seen, vec!["deep".to_owned(), "triage".to_owned()]);

    server.stop().await;
}

#[tokio::test]
async fn stream_enrichments_from_zero_replays_full_history() {
    let server = TestServer::start().await;
    let (account_id, mailbox_id) = server.account("INBOX").await;
    let id = server.message(account_id, mailbox_id, 1, "hello").await;
    server
        .write_deep_row_and_announce(id, account_id, "the only summary")
        .await;

    let mut stream = server
        .client()
        .await
        .stream_enrichments(StreamEnrichmentsRequest {
            account_id: 0,
            since_message_id: 0,
        })
        .await
        .unwrap()
        .into_inner();

    let item = tokio::time::timeout(STREAM_TIMEOUT, stream.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(item.message_id, id);

    server.stop().await;
}

// ---------------------------------------------------------------------------
// SuggestReply — cached vs. forced
// ---------------------------------------------------------------------------

#[tokio::test]
async fn suggest_reply_returns_the_cached_deep_row_without_calling_the_model() {
    let server = TestServer::start().await;
    let (account_id, mailbox_id) = server.account("INBOX").await;
    let id = server.message(account_id, mailbox_id, 1, "hello").await;
    server
        .write_deep_row_and_announce(id, account_id, "already analyzed")
        .await;

    let summary = server
        .client()
        .await
        .suggest_reply(SuggestReplyRequest { message_id: id })
        .await
        .unwrap()
        .into_inner();

    assert_eq!(summary.summary.as_deref(), Some("already analyzed"));
    assert_eq!(server.provider.complete_calls(), 0);
    server.stop().await;
}

#[tokio::test]
async fn suggest_reply_forces_a_deep_pass_when_nothing_is_cached() {
    let server = TestServer::start().await;
    let (account_id, mailbox_id) = server.account("INBOX").await;
    let id = server
        .message(
            account_id,
            mailbox_id,
            1,
            "Can we push the review to Friday?",
        )
        .await;
    server
        .provider
        .queue_complete(ScriptedComplete::Ok(valid_deep_json(
            "Asks to move the review to Friday.",
        )));

    let summary = server
        .client()
        .await
        .suggest_reply(SuggestReplyRequest { message_id: id })
        .await
        .unwrap()
        .into_inner();

    assert_eq!(
        summary.summary.as_deref(),
        Some("Asks to move the review to Friday.")
    );
    assert_eq!(server.provider.complete_calls(), 1);

    // And it is now cached — a second call must not call the model again.
    let second = server
        .client()
        .await
        .suggest_reply(SuggestReplyRequest { message_id: id })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(second.summary, summary.summary);
    assert_eq!(
        server.provider.complete_calls(),
        1,
        "the second call was answered from cache"
    );

    server.stop().await;
}

#[tokio::test]
async fn suggest_reply_surfaces_a_provider_failure_rather_than_silently_caching_nothing() {
    let server = TestServer::start().await;
    let (account_id, mailbox_id) = server.account("INBOX").await;
    let id = server.message(account_id, mailbox_id, 1, "hello").await;
    server
        .provider
        .queue_complete(ScriptedComplete::Unavailable);

    let status = server
        .client()
        .await
        .suggest_reply(SuggestReplyRequest { message_id: id })
        .await
        .expect_err("the mock provider was scripted to fail");

    assert_eq!(status.code(), Code::Unavailable);
    assert_eq!(server.provider.complete_calls(), 1);

    // The failed attempt is still audited (an error-status ledger row), not
    // silently dropped -- and the message stays uncached, so a retry is
    // still possible.
    let cached = server
        .client()
        .await
        .get_summary(GetSummaryRequest { message_id: id })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(cached.status(), SummaryStatus::NotQueued);

    server.stop().await;
}

// ---------------------------------------------------------------------------
// GetUsage / SetPaused
// ---------------------------------------------------------------------------

#[tokio::test]
async fn get_usage_reports_queue_depth_and_configured_caps() {
    let server = TestServer::start().await;
    let (account_id, mailbox_id) = server.account("INBOX").await;
    let id = server.message(account_id, mailbox_id, 1, "hello").await;
    server
        .queue
        .enqueue(vec![NewAiJob::new(id, account_id, "triage")])
        .await
        .unwrap();

    let usage = server
        .client()
        .await
        .get_usage(GetUsageRequest {})
        .await
        .unwrap()
        .into_inner();

    let queue = usage.queue.expect("queue stats must be present");
    assert_eq!(queue.ready, 1);
    assert_eq!(usage.daily_cost_cap_usd, 1_000.0);
    assert_eq!(usage.monthly_cost_cap_usd, 1_000.0);
    assert!(!usage.paused);
    assert!(usage.enabled, "this harness always builds an active AiApi");
    assert!(usage.today.is_some());
    assert!(usage.month.is_some());

    server.stop().await;
}

#[tokio::test]
async fn set_paused_round_trips_through_get_usage() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let response = client
        .set_paused(SetPausedRequest { paused: true })
        .await
        .unwrap()
        .into_inner();
    assert!(response.paused);

    let usage = client
        .get_usage(GetUsageRequest {})
        .await
        .unwrap()
        .into_inner();
    assert!(
        usage.paused,
        "the pause is visible to any caller, not just the one that set it"
    );

    let response = client
        .set_paused(SetPausedRequest { paused: false })
        .await
        .unwrap()
        .into_inner();
    assert!(!response.paused);
    assert!(!server.pause.get());

    server.stop().await;
}

// ---------------------------------------------------------------------------
// RetryFailed — `mail ai retry --failed`
// ---------------------------------------------------------------------------

#[tokio::test]
async fn retry_failed_revives_dead_jobs_and_reports_the_count() {
    let server = TestServer::start().await;
    let (account_id, mailbox_id) = server.account("INBOX").await;
    let first = server.message(account_id, mailbox_id, 1, "hello").await;
    let second = server.message(account_id, mailbox_id, 2, "world").await;
    server
        .queue
        .enqueue(vec![
            NewAiJob::new(first, account_id, "triage"),
            NewAiJob::new(second, account_id, "triage"),
        ])
        .await
        .unwrap();
    // Quarantined directly via SQL — cycling `lease`/`fail` enough times to
    // naturally exhaust `max_attempts` would also work, but is a slower and
    // less direct way to set up the same fixture (see `write_triage_row`'s
    // own precedent in this file for "assert against the RPC, set up state
    // directly").
    server
        .db
        .write(|c| {
            c.execute(
                "UPDATE ai_queue SET state = 'dead', attempts = 5, \
                 last_error = 'simulated failure'",
                [],
            )
        })
        .await
        .unwrap();
    let stats_before = server.queue.stats().await.unwrap();
    assert_eq!(stats_before.dead, 2);

    let response = server
        .client()
        .await
        .retry_failed(rmail_proto::v1::RetryFailedRequest {})
        .await
        .unwrap()
        .into_inner();

    assert_eq!(response.revived, 2);
    let stats_after = server.queue.stats().await.unwrap();
    assert_eq!(stats_after.dead, 0);
    assert_eq!(stats_after.ready, 2);

    server.stop().await;
}

// ---------------------------------------------------------------------------
// The daemon actually wires the dispatch loop — not just `AiDispatchLoop` in
// isolation. Every other test in this file constructs `AiApi`/`AiDispatchLoop`
// by hand against a `MockProvider`, because `ClaudeProvider`'s endpoint is not
// configurable at the `Config` level (see this file's own module docs) — but
// that also means none of them would notice if the `AiDispatchLoop::spawn`
// call were ever deleted from `rmaild::serve_uds_with_engine_and_mail_store`.
// This test boots the real daemon entry point instead, proving the wiring
// itself, exactly what task 50's acceptance criterion asks for: "cover it
// with a test that syncs a message and asserts a job appears."
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_real_daemon_boot_wires_the_dispatch_loop_end_to_end() {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let socket = PathBuf::from("/tmp").join(format!("rmail-ai-daemon-{pid}-{n}.sock"));
    let db_path = std::env::temp_dir().join(format!("rmail-ai-daemon-{pid}-{n}.db"));
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", db_path.display())));
    }
    let _ = std::fs::remove_file(&socket);

    let db = Database::open(&db_path).unwrap();
    let events = EventLog::new(db.clone(), Retention::unlimited());
    let engine = SyncEngine::new(db.clone(), events.clone(), SyncOptions::default());

    // Semantic indexing off (avoid loading an embedder). `ai.enabled` and
    // `ai.batching.enabled` are left at their real defaults (both `true`) —
    // the whole point of this test is to prove the *default* daemon boot
    // wires AI dispatch, not a specially-configured one. The credential
    // command (`ai.api_key_command`, "security find-generic-password...")
    // will fail fast inside this (Linux) test container -- fine, this test
    // only asserts the job was *enqueued*, never that it completed.
    let mut config = Config::default();
    config.index.semantic.enabled = false;

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server_socket = socket.clone();
    let server_db = db.clone();
    let handle = tokio::spawn(async move {
        rmaild::serve_uds_with_engine(&server_socket, server_db, engine, &config, async move {
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
    assert!(ready, "the real daemon never became ready");

    let (account_id, mailbox_id) = db
        .write(|c| {
            let account_id = repo::insert_account(
                c,
                &repo::NewAccount {
                    name: "Personal".to_owned(),
                    ..Default::default()
                },
            )?;
            let mailbox_id = repo::insert_mailbox(
                c,
                &repo::NewMailbox {
                    account_id,
                    name: "INBOX".to_owned(),
                    ..Default::default()
                },
            )?;
            Ok((account_id, mailbox_id))
        })
        .await
        .unwrap();
    let message_id = db
        .write(move |c| {
            repo::insert_message(
                c,
                &repo::NewMessage {
                    account_id,
                    mailbox_id,
                    uid: 1,
                    uidvalidity: 1,
                    subject: Some("Test".to_owned()),
                    body_text: Some("hello from the real daemon boot".to_owned()),
                    ..Default::default()
                },
            )
        })
        .await
        .unwrap();

    // The same event `sync::engine::LogSink` appends the moment a message
    // lands — this is "sync a message" for the purposes of this test.
    events
        .append(
            NewEvent::new(CoreKind::NewMail)
                .account(account_id)
                .mailbox(mailbox_id)
                .message(message_id),
        )
        .await
        .unwrap();

    // The dispatch loop ticks once immediately on spawn and then every
    // `DEFAULT_TICK_INTERVAL` (5s) after — poll well past that so a message
    // synced just after the first tick is still caught by the second.
    let mut found = false;
    for _ in 0..120 {
        let exists: bool = db
            .read(move |c| {
                c.query_row(
                    "SELECT EXISTS(SELECT 1 FROM ai_queue WHERE message_id = ?1 AND pass = 'triage')",
                    [message_id],
                    |row| row.get(0),
                )
            })
            .await
            .unwrap();
        if exists {
            found = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(
        found,
        "the real daemon boot must enqueue a triage job for a synced message — if this \
         fails, `AiDispatchLoop::spawn` was likely removed from \
         `serve_uds_with_engine_and_mail_store`"
    );

    let _ = shutdown_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(10), handle).await;
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", db_path.display())));
    }
    let _ = std::fs::remove_file(&socket);
}
