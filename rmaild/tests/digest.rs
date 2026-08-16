//! Integration test: drive `AnalyticsService.GenerateDigest` end-to-end
//! against an in-process tonic server booted through the **real** daemon
//! wiring (`rmaild::serve_uds_injected`), with only the Anthropic client
//! faked.
//!
//! # Why this goes through `serve_uds_injected`
//!
//! The digest is not one component: it is `AnalyticsApi` deciding a window,
//! `rmail_core::digest` selecting and clustering it, `ai::rag::context::pack`
//! applying the `ai.policy` gate and the fence, `ai::redact` guarding the
//! request, `ai::budget` gating the spend, and `digest::briefing` resolving
//! the citations. A hand-built handler would test the last two and none of the
//! wiring between them, which is exactly where a mistake would live — most
//! sharply the one that matters here, that the daemon actually attaches a
//! digest engine to the service it serves.
//!
//! # What each test proves — the `verify` line's own "sectioned briefing,
//! every line cites a message-id", plus the failure paths
//!
//! - [`digest_returns_a_sectioned_briefing_over_the_windows_mail`] — the five
//!   prd.md sections, in order, over mail seeded through the real indexing
//!   pipeline.
//! - [`digest_every_briefing_line_cites_a_message_id`] — the acceptance
//!   criterion, checked over the rendered markdown *and* the structured
//!   sections, against the ids the daemon actually retrieved.
//! - [`digest_drops_a_line_that_cites_nothing`] and
//!   [`digest_drops_a_fabricated_citation`] — a line the model did not source,
//!   and a label no source has, never reach a client.
//! - [`digest_does_not_brief_the_same_window_twice`] — one window, one model
//!   call; the second request is served from the stored row.
//! - [`digest_force_regenerates_the_same_window`] — and the one way to get a
//!   second briefing is to ask for it.
//! - [`digest_an_empty_window_produces_a_briefing_without_a_model_call`] — the
//!   quiet-week path, which must not become an empty prompt on a timer.
//! - [`digest_never_sends_a_forbidden_folder_to_the_provider`] — the P0 shape,
//!   checked over the literal bytes of every request the provider was handed.
//! - [`digest_rejects_an_inverted_window`] — the boundary maps a domain error
//!   to `INVALID_ARGUMENT`.
//! - [`digest_declines_when_the_ai_subsystem_is_off`] — `FAILED_PRECONDITION`,
//!   with no window scan and no provider call.
//!
//! Every name starts with `digest_` so a bare `cargo nextest run -p rmaild
//! digest` selects them: nextest matches a positional filter against a test's
//! *name*, not its binary id.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use rmail_core::ai::provider::{ChatResponse, StopReason, Usage as CoreUsage};
use rmail_core::ai::{ChatRequest, Provider, ProviderStream};
use rmail_core::config::{AiPolicyMode, AiPolicyRule};
use rmail_core::index::fts::FtsIndex;
use rmail_core::index::{extract_message, IndexQueue, QueueOptions, PRIORITY_NORMAL};
use rmail_core::repo;
use rmail_core::Error as CoreError;
use rmail_core::{Config, Database};
use rmail_proto::v1::analytics_service_client::AnalyticsServiceClient;
use rmail_proto::v1::GenerateDigestRequest;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tonic::transport::Channel;
use tonic::Code;

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// The window every test briefs, and the instant its mail is dated at. Fixed
/// so the seeded mail and the requested bounds cannot drift apart.
const WINDOW_START: i64 = 1_700_000_000;
const WINDOW_END: i64 = WINDOW_START + 86_400;

/// A well-formed briefing citing both sources. Written the way the prompt asks
/// for it — five headings, `- ` bullets, bracketed labels.
const GOOD_ANSWER: &str = "## Needs reply\n\
     - AWS wants the October invoice settled by the 30th [1]\n\n\
     ## FYI\n\
     - the deploy finished cleanly [2]\n\n\
     ## Waiting on\n_none_\n\n\
     ## Auto-handled\n_none_\n\n\
     ## Skipped\n_none_\n";

// ---------------------------------------------------------------------------
// A provider that answers the digest and records everything
// ---------------------------------------------------------------------------

/// Stands in for `ClaudeProvider`.
///
/// The digest is a non-streaming call, so only `complete()` is ever reached;
/// `stream()` failing loudly is what would catch a change that silently routed
/// the briefing through the streaming path (where nothing here would observe
/// it).
///
/// `transmitted()` is the assertion surface the policy test needs: every
/// character of every request this provider was handed.
#[derive(Debug, Default)]
struct MockProvider {
    answers: Mutex<Vec<String>>,
    seen: Mutex<Vec<ChatRequest>>,
    calls: AtomicUsize,
}

impl MockProvider {
    /// Queue one answer. Answers are consumed in order; the last one repeats
    /// so a test that only cares about the first call need not queue for
    /// calls it does not expect.
    fn queue(&self, answer: &str) {
        self.answers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(answer.to_owned());
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn transmitted(&self) -> String {
        let seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        let mut out = String::new();
        for request in seen.iter() {
            out.push_str(request.system.as_deref().unwrap_or_default());
            for message in &request.messages {
                out.push_str(&message.content);
            }
        }
        out
    }
}

#[async_trait]
impl Provider for MockProvider {
    async fn complete(
        &self,
        request: &ChatRequest,
        _cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<ChatResponse, CoreError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(request.clone());
        let answers = self.answers.lock().unwrap_or_else(PoisonError::into_inner);
        let text = answers
            .get(n)
            .or_else(|| answers.last())
            .cloned()
            .unwrap_or_default();
        Ok(ChatResponse {
            id: "msg_mock".to_owned(),
            model: request.model.clone(),
            stop_reason: StopReason::EndTurn,
            text,
            usage: CoreUsage::default(),
        })
    }

    async fn stream(
        &self,
        _request: &ChatRequest,
        _cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<ProviderStream, CoreError> {
        Err(CoreError::internal(
            "the digest must not reach the streaming path",
        ))
    }
}

// ---------------------------------------------------------------------------
// Test server
// ---------------------------------------------------------------------------

struct TestServer {
    socket: PathBuf,
    db_path: PathBuf,
    db: Database,
    fts: FtsIndex,
    queue: IndexQueue,
    account_id: i64,
    inbox_id: i64,
    next_uid: std::cell::Cell<i64>,
    provider: Arc<MockProvider>,
    shutdown: oneshot::Sender<()>,
    handle: JoinHandle<Result<(), rmaild::ServeError>>,
}

/// The base config every test here starts from.
///
/// Semantic indexing off, for the reason `rmaild/tests/ask_mailbox.rs` gives:
/// the hash fallback keeps these tests from loading — or, on a cold cache,
/// downloading — an ONNX model none of them needs. `digest.enabled` is left at
/// its default (`false`) on purpose: the *scheduler* must stay off in a test
/// process, and the whole point of the switch is that `GenerateDigest` still
/// answers without it. One test flips it, to prove the daemon spawns the loop.
fn base_config() -> Config {
    let mut config = Config::default();
    config.index.semantic.enabled = false;
    config.ai.limits.requests_per_minute = 1_000_000;
    config.ai.batching.enabled = false;
    config
}

impl TestServer {
    async fn start() -> Self {
        Self::with_config(base_config(), true).await
    }

    async fn with_config(config: Config, ai: bool) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let socket = PathBuf::from("/tmp").join(format!("rmail-digest-{pid}-{n}.sock"));
        let db_path = std::env::temp_dir().join(format!("rmail-digest-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", db_path.display())));
        }
        let _ = std::fs::remove_file(&socket);

        let db = Database::open(&db_path).unwrap();
        let (account_id, inbox_id) = db
            .with_write(move |c| {
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
            .unwrap();

        let fts = FtsIndex::new(db.clone(), config.search.bm25_weights.clone());
        let queue = IndexQueue::new(db.clone(), QueueOptions::default());
        let provider = Arc::new(MockProvider::default());

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let server_socket = socket.clone();
        let server_db = db.clone();
        let server_provider: Option<Arc<dyn Provider>> =
            ai.then(|| Arc::clone(&provider) as Arc<dyn Provider>);
        let handle = tokio::spawn(async move {
            // The real boot path: only the Anthropic client is substituted.
            let events = rmail_core::events::EventLog::new(
                server_db.clone(),
                rmail_core::events::Retention::unlimited(),
            );
            let engine = rmail_core::sync::SyncEngine::new(
                server_db.clone(),
                events,
                rmail_core::sync::SyncOptions::default(),
            );
            let mail_store = rmail_core::mail::MailStore::new(
                server_db.clone(),
                engine.events().clone(),
                Arc::new(rmail_core::imap::mutate::LiveImapMutator::new(
                    server_db.clone(),
                )),
            );
            let tag_store = rmail_core::tags::TagStore::new(
                server_db.clone(),
                Arc::new(rmail_core::imap::mutate::LiveImapMutator::new(
                    server_db.clone(),
                )),
                config.tags.clone(),
            );
            rmaild::serve_uds_injected(
                &server_socket,
                server_db,
                engine,
                mail_store,
                tag_store,
                &config,
                rmaild::Injected {
                    ai_provider: server_provider,
                    reranker: None,
                    ..Default::default()
                },
                async move {
                    let _ = shutdown_rx.await;
                },
            )
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
            db,
            fts,
            queue,
            account_id,
            inbox_id,
            next_uid: std::cell::Cell::new(1),
            provider,
            shutdown: shutdown_tx,
            handle,
        }
    }

    async fn client(&self) -> AnalyticsServiceClient<Channel> {
        AnalyticsServiceClient::new(rmail_core::connect_uds(&self.socket).await.unwrap())
    }

    async fn mailbox(&self, name: &str) -> i64 {
        let account_id = self.account_id;
        let name = name.to_owned();
        self.db
            .write(move |c| {
                repo::insert_mailbox(
                    c,
                    &repo::NewMailbox {
                        account_id,
                        name,
                        ..Default::default()
                    },
                )
            })
            .await
            .unwrap()
    }

    /// Insert, extract and lexically index a message dated inside the window
    /// — the real pipeline. `extract_message` is what writes the
    /// `index_content` row the digest's packer reads, so a message seeded any
    /// other way would be selected and then packed with an empty body.
    async fn index(&self, mailbox_id: i64, from: &str, subject: &str, body: &str) -> i64 {
        let uid = self.next_uid.get();
        self.next_uid.set(uid + 1);
        let new = repo::NewMessage {
            account_id: self.account_id,
            mailbox_id,
            uid,
            uidvalidity: 1,
            subject: Some(subject.to_owned()),
            from_addr: Some(from.to_owned()),
            body_text: Some(body.to_owned()),
            date: Some(WINDOW_START + uid),
            ..Default::default()
        };
        let message_id = self
            .db
            .with_write(move |c| repo::insert_message(c, &new))
            .unwrap();
        extract_message(&self.db, &self.queue, message_id, PRIORITY_NORMAL)
            .await
            .unwrap();
        self.fts.index_message(message_id).await.unwrap();
        message_id
    }

    /// The two messages every "happy path" test briefs: one that wants an
    /// answer, one that does not.
    async fn seed(&self) -> (i64, i64) {
        let invoice = self
            .index(
                self.inbox_id,
                "billing@aws.example",
                "Invoice for October",
                "Your October invoice of 412 dollars is due on the 30th. Please confirm.",
            )
            .await;
        let deploy = self
            .index(
                self.inbox_id,
                "ci@example.com",
                "Deploy 4821 succeeded",
                "The production deploy finished cleanly in 4 minutes. No action needed.",
            )
            .await;
        (invoice, deploy)
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

/// A request for the fixed window every test uses.
fn window_request() -> GenerateDigestRequest {
    GenerateDigestRequest {
        account_id: 0,
        since: WINDOW_START,
        until: WINDOW_END,
        force: false,
    }
}

/// Every `- ` bullet in a rendered briefing.
fn bullets(markdown: &str) -> Vec<&str> {
    markdown
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("- "))
        .collect()
}

// ---------------------------------------------------------------------------
// The briefing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn digest_returns_a_sectioned_briefing_over_the_windows_mail() {
    let server = TestServer::start().await;
    server.seed().await;
    server.provider.queue(GOOD_ANSWER);

    let digest = server
        .client()
        .await
        .generate_digest(window_request())
        .await
        .expect("GenerateDigest")
        .into_inner();

    assert_eq!(server.provider.calls(), 1);
    assert!(!digest.cached);
    assert!(!digest.empty);
    assert_eq!(digest.since, WINDOW_START);
    assert_eq!(digest.until, WINDOW_END);
    assert_eq!(digest.packed, 2, "both seeded messages entered the prompt");
    assert!(digest.digest_id > 0, "the briefing was stored");

    // The five prd.md sections, in prd.md's order, all present.
    let ids: Vec<&str> = digest
        .sections
        .iter()
        .map(|section| section.id.as_str())
        .collect();
    assert_eq!(
        ids,
        vec![
            "needs_reply",
            "fyi",
            "waiting_on",
            "auto_handled",
            "skipped"
        ]
    );
    for section in &digest.sections {
        assert!(
            digest.markdown.contains(&format!("## {}", section.heading)),
            "the markdown is missing the {} heading",
            section.heading
        );
    }
    // ... and the ones the model filled in are filled in, in the right place.
    let needs_reply = digest
        .sections
        .iter()
        .find(|s| s.id == "needs_reply")
        .expect("a needs_reply section");
    assert_eq!(needs_reply.lines.len(), 1);
    assert!(needs_reply.lines[0].text.contains("October invoice"));
    let waiting = digest
        .sections
        .iter()
        .find(|s| s.id == "waiting_on")
        .expect("a waiting_on section");
    assert!(waiting.lines.is_empty());
    assert!(digest.markdown.contains("## Waiting on\n_none_"));

    server.stop().await;
}

#[tokio::test]
async fn digest_every_briefing_line_cites_a_message_id() {
    // The acceptance criterion, checked twice over: on the rendered markdown a
    // human reads, and on the structured lines a client renders. Both have to
    // name ids the daemon actually retrieved — not ids the model invented.
    let server = TestServer::start().await;
    let (invoice, deploy) = server.seed().await;
    server.provider.queue(GOOD_ANSWER);

    let digest = server
        .client()
        .await
        .generate_digest(window_request())
        .await
        .expect("GenerateDigest")
        .into_inner();

    let retrieved: Vec<i64> = digest.sources.iter().map(|s| s.message_id).collect();
    assert!(retrieved.contains(&invoice) && retrieved.contains(&deploy));

    let lines: Vec<_> = digest
        .sections
        .iter()
        .flat_map(|section| section.lines.iter())
        .collect();
    assert_eq!(lines.len(), 2, "both bullets survived");
    for line in &lines {
        assert!(
            !line.message_ids.is_empty(),
            "line {:?} cites no message",
            line.text
        );
        for id in &line.message_ids {
            assert!(
                retrieved.contains(id),
                "line {:?} cites message {id}, which was never retrieved",
                line.text
            );
        }
        assert!(
            line.text
                .contains(&format!("[msg:{}]", line.message_ids[0])),
            "line {:?} does not carry its citation inline",
            line.text
        );
    }

    let rendered = bullets(&digest.markdown);
    assert_eq!(rendered.len(), 2);
    for bullet in rendered {
        assert!(
            bullet.contains("[msg:"),
            "rendered bullet {bullet:?} cites no message-id"
        );
    }
    // The sources the briefing actually used are marked as such.
    assert_eq!(digest.sources.iter().filter(|s| s.cited).count(), 2);

    server.stop().await;
}

#[tokio::test]
async fn digest_drops_a_line_that_cites_nothing() {
    let server = TestServer::start().await;
    server.seed().await;
    server.provider.queue(
        "## Needs reply\n\
         - the invoice is due [1]\n\
         - something else happened, trust me\n",
    );

    let digest = server
        .client()
        .await
        .generate_digest(window_request())
        .await
        .expect("GenerateDigest")
        .into_inner();

    let lines: Vec<_> = digest
        .sections
        .iter()
        .flat_map(|section| section.lines.iter())
        .collect();
    assert_eq!(lines.len(), 1);
    assert!(!digest.markdown.contains("trust me"));

    server.stop().await;
}

#[tokio::test]
async fn digest_drops_a_fabricated_citation() {
    // `[97]` names no source this daemon retrieved, so it resolves to nothing
    // and is deleted rather than rendered as if it were a citation.
    let server = TestServer::start().await;
    server.seed().await;
    server
        .provider
        .queue("## FYI\n- two unrelated things [1, 97]\n- entirely invented [98]\n");

    let digest = server
        .client()
        .await
        .generate_digest(window_request())
        .await
        .expect("GenerateDigest")
        .into_inner();

    let lines: Vec<_> = digest
        .sections
        .iter()
        .flat_map(|section| section.lines.iter())
        .collect();
    assert_eq!(lines.len(), 1, "the wholly-invented bullet was dropped");
    assert_eq!(lines[0].message_ids.len(), 1);
    assert!(!digest.markdown.contains("[97]"));
    assert!(!digest.markdown.contains("msg:97"));
    assert!(!digest.markdown.contains("entirely invented"));

    server.stop().await;
}

// ---------------------------------------------------------------------------
// One window, one briefing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn digest_does_not_brief_the_same_window_twice() {
    let server = TestServer::start().await;
    server.seed().await;
    server.provider.queue(GOOD_ANSWER);

    let mut client = server.client().await;
    let first = client
        .generate_digest(window_request())
        .await
        .expect("first GenerateDigest")
        .into_inner();
    let second = client
        .generate_digest(window_request())
        .await
        .expect("second GenerateDigest")
        .into_inner();

    assert_eq!(
        server.provider.calls(),
        1,
        "the second request paid for a second briefing"
    );
    assert!(!first.cached);
    assert!(second.cached);
    assert_eq!(second.digest_id, first.digest_id);
    assert_eq!(second.markdown, first.markdown);
    assert_eq!(second.generated_at, first.generated_at);
    // The cached response is a briefing, not a blob: its sections and its
    // sources come back too.
    assert_eq!(second.sections.len(), 5);
    assert_eq!(second.sources.len(), first.sources.len());
    let lines: usize = second.sections.iter().map(|s| s.lines.len()).sum();
    assert_eq!(lines, 2);

    server.stop().await;
}

#[tokio::test]
async fn digest_force_regenerates_the_same_window() {
    let server = TestServer::start().await;
    server.seed().await;
    server.provider.queue(GOOD_ANSWER);
    server
        .provider
        .queue("## Skipped\n- on reflection, this was all noise [1, 2]\n");

    let mut client = server.client().await;
    let first = client
        .generate_digest(window_request())
        .await
        .expect("first GenerateDigest")
        .into_inner();
    let forced = client
        .generate_digest(GenerateDigestRequest {
            force: true,
            ..window_request()
        })
        .await
        .expect("forced GenerateDigest")
        .into_inner();

    assert_eq!(server.provider.calls(), 2);
    assert!(!forced.cached);
    assert!(forced.markdown.contains("all noise"));
    assert_ne!(forced.markdown, first.markdown);

    // Replaced, not accumulated.
    let rows: i64 = server
        .db
        .read(|c| c.query_row("SELECT COUNT(*) FROM digests", [], |r| r.get(0)))
        .await
        .unwrap();
    assert_eq!(rows, 1);

    server.stop().await;
}

#[tokio::test]
async fn digest_an_empty_window_produces_a_briefing_without_a_model_call() {
    // A quiet period. On a timer, this is the recurring cost that would
    // otherwise be paid forever for an answer that cannot contain anything.
    let server = TestServer::start().await;
    server.provider.queue(GOOD_ANSWER);

    let digest = server
        .client()
        .await
        .generate_digest(window_request())
        .await
        .expect("GenerateDigest")
        .into_inner();

    assert_eq!(server.provider.calls(), 0);
    assert!(digest.empty);
    assert!(digest.model.is_empty());
    assert_eq!(digest.packed, 0);
    assert!(digest.sources.is_empty());
    // Still the same five-section document, not a special-cased string.
    assert_eq!(digest.sections.len(), 5);
    assert!(digest.markdown.contains("## Needs reply\n_none_"));
    assert!(digest.digest_id > 0, "the empty period was recorded");

    server.stop().await;
}

// ---------------------------------------------------------------------------
// The policy gate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn digest_never_sends_a_forbidden_folder_to_the_provider() {
    // The P0 shape: asserted over the literal bytes of every request the
    // provider was handed, not over the briefing.
    let mut config = base_config();
    config.ai.policy.rules = vec![AiPolicyRule {
        account: None,
        folder: Some("Legal".to_owned()),
        mode: AiPolicyMode::Forbidden,
        residency: None,
        reason: None,
    }];
    let server = TestServer::with_config(config, true).await;
    let legal = server.mailbox("Legal").await;
    server
        .index(
            server.inbox_id,
            "billing@aws.example",
            "Invoice for October",
            "Your October invoice of 412 dollars is due on the 30th.",
        )
        .await;
    server
        .index(
            legal,
            "counsel@example.com",
            "Privileged settlement terms",
            "The confidential settlement figure is four million dollars.",
        )
        .await;
    server.provider.queue("## FYI\n- an invoice arrived [1]\n");

    let digest = server
        .client()
        .await
        .generate_digest(window_request())
        .await
        .expect("GenerateDigest")
        .into_inner();

    assert_eq!(digest.withheld_by_policy, 1);
    assert_eq!(digest.packed, 1);
    let transmitted = server.provider.transmitted();
    assert!(
        !transmitted.contains("Privileged settlement")
            && !transmitted.contains("confidential settlement")
            && !transmitted.contains("four million"),
        "a forbidden folder's text reached the provider"
    );
    assert!(
        transmitted.contains("Invoice for October"),
        "the allowed folder never reached the provider either, so this proves nothing"
    );
    // And the withheld message is not among the briefing's sources.
    assert_eq!(digest.sources.len(), 1);

    server.stop().await;
}

// ---------------------------------------------------------------------------
// Error paths
// ---------------------------------------------------------------------------

#[tokio::test]
async fn digest_rejects_an_inverted_window() {
    let server = TestServer::start().await;
    let status = server
        .client()
        .await
        .generate_digest(GenerateDigestRequest {
            account_id: 0,
            since: WINDOW_END,
            until: WINDOW_START,
            force: false,
        })
        .await
        .expect_err("an inverted window");
    assert_eq!(status.code(), Code::InvalidArgument);
    assert_eq!(server.provider.calls(), 0);
    server.stop().await;
}

#[tokio::test]
async fn digest_rejects_a_negative_bound() {
    let server = TestServer::start().await;
    let status = server
        .client()
        .await
        .generate_digest(GenerateDigestRequest {
            account_id: 0,
            since: -1,
            until: WINDOW_END,
            force: false,
        })
        .await
        .expect_err("a negative bound");
    assert_eq!(status.code(), Code::InvalidArgument);
    server.stop().await;
}

#[tokio::test]
async fn digest_declines_when_the_ai_subsystem_is_off() {
    // `ai.enabled = false` and no injected provider: the RPC is still
    // registered (reflection and the fail-closed scope table must see it), and
    // it declines up front rather than scanning a window it can never brief.
    let mut config = base_config();
    config.ai.enabled = false;
    let server = TestServer::with_config(config, false).await;
    server.seed().await;

    let status = server
        .client()
        .await
        .generate_digest(window_request())
        .await
        .expect_err("a daemon with no AI subsystem");
    assert_eq!(status.code(), Code::FailedPrecondition);
    assert_eq!(server.provider.calls(), 0);

    let rows: i64 = server
        .db
        .read(|c| c.query_row("SELECT COUNT(*) FROM digests", [], |r| r.get(0)))
        .await
        .unwrap();
    assert_eq!(rows, 0, "a declined request must not store anything");

    server.stop().await;
}

#[tokio::test]
async fn digest_refuses_a_briefing_that_cites_nothing_and_stores_nothing() {
    // The window had mail, so "nothing to report" is not a true statement
    // about it — and storing it would consume the window's one briefing.
    let server = TestServer::start().await;
    server.seed().await;
    server
        .provider
        .queue("Here is your week! Everything looks fine. No sections, no labels.");

    let status = server
        .client()
        .await
        .generate_digest(window_request())
        .await
        .expect_err("an uncited briefing");
    assert_eq!(status.code(), Code::Internal);

    let rows: i64 = server
        .db
        .read(|c| c.query_row("SELECT COUNT(*) FROM digests", [], |r| r.get(0)))
        .await
        .unwrap();
    assert_eq!(rows, 0, "the window must stay unbriefed and be retried");

    server.stop().await;
}

// ---------------------------------------------------------------------------
// The scheduled job
// ---------------------------------------------------------------------------

#[tokio::test]
async fn digest_the_scheduled_job_briefs_the_last_completed_period() {
    // `digest.enabled = true` is the only switch that starts the timer, and
    // this is the one test that flips it: it proves the daemon actually spawns
    // the loop and that the loop reaches the same engine the RPC does.
    //
    // The window it briefs is the last *completed* period, which is not the
    // fixed one the rest of this file uses — so the mail is dated relative to
    // now rather than to `WINDOW_START`, and the assertion is on the row the
    // loop stored, not on its contents.
    let mut config = base_config();
    config.digest.enabled = true;
    config.digest.interval =
        rmail_core::config::HumanDuration::new(std::time::Duration::from_secs(3_600));
    config.digest.tick_interval =
        rmail_core::config::HumanDuration::new(std::time::Duration::from_secs(1));
    let server = TestServer::with_config(config, true).await;
    server.provider.queue("## FYI\n- something happened [1]\n");

    // The loop runs once immediately on spawn, so the row appears without
    // waiting a tick interval. Polled rather than slept on: a fixed sleep is
    // either flaky or slow.
    let mut stored = 0i64;
    for _ in 0..100 {
        stored = server
            .db
            .read(|c| c.query_row("SELECT COUNT(*) FROM digests", [], |r| r.get(0)))
            .await
            .unwrap();
        if stored > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        stored, 1,
        "the scheduled job never briefed the last completed period"
    );
    // Nothing was in that window, so it cost nothing — the point of the empty
    // path, on the surface that actually runs on a timer.
    assert_eq!(server.provider.calls(), 0);

    server.stop().await;
}
