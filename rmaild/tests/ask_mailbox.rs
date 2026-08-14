//! Integration test: drive `AiService.AskMailbox` end-to-end against an
//! in-process tonic server booted through the **real** daemon wiring
//! (`rmaild::serve_uds_injected`), with only the Anthropic client faked.
//!
//! # Why this goes through `serve_uds_injected` rather than building `AiApi`
//!
//! `rmaild/tests/ai_service.rs` assembles its handler by hand, which is right
//! for RPCs whose only dependency is a provider. `AskMailbox` is not one of
//! those: it is *retrieval* plus a model call, and the retrieval is
//! `SearchApi` — planner, fan-out, fusion, features, L1, **L2 rerank**,
//! presenter — assembled once, in `serve_uds*`. A hand-built handler would
//! test neither the real pipeline nor the real wiring between the two halves,
//! which is exactly where a mistake would live.
//!
//! Task 51 left that seam deliberately unbuilt and named task 52 as the task
//! that would need it; `rmaild::Injected` is it. Everything below runs the
//! daemon's own boot path — the same `SearchApi`, the same `L2Stage`, the same
//! `PolicyEngine`, the same auth layer — with `Injected::ai_provider` standing
//! in for the one thing that would otherwise reach the network.
//!
//! # Why the questions here read like search queries
//!
//! prd.md's own example is `mail ask "how much did AWS bill me in Q2?"`, and
//! that sentence retrieves **nothing** in this harness. That is not a defect in
//! `AskMailbox`; it is which retrieval arm answers a question, and it is worth
//! stating plainly because the alternative is rediscovering it:
//!
//! - `retrieve::lexical` joins a query's terms with `AND` (see its
//!   `MatchExpr::build`), so a sentence only matches a message containing
//!   *every* word of it — "how", "much", "did" and "me" included.
//! - `fuse::drop_prior_only_candidates` then drops any candidate supported
//!   only by the recency/prefix/structured priors, so "the two most recent
//!   messages" is not a fallback for a free-text query. That rule is
//!   deliberate and task 33's own tests pin it.
//! - What is left to answer a *sentence* is the dense (embedding) arm, which
//!   is exactly the arm prd.md assigns the job ("fusing FTS5 recall with
//!   embeddings"). It is on by default in production and off here, because
//!   `index.semantic.enabled = true` would make every test in this file load —
//!   and on a cold cache download — an ONNX model, and the deterministic hash
//!   fallback that replaces it produces no meaningful similarity.
//!
//! So these tests ask questions whose recall does not depend on a model file.
//! Everything this file is actually about — the policy gate, packing,
//! streaming, citation resolution, the grounding verdict — is independent of
//! which retrieval arm produced the candidates, and `rmail_core::ai::rag`'s own
//! tests drive those against a stub retriever where the ids are given outright.
//!
//! # What each test proves
//!
//! Every name here starts with `ask_mailbox_` so this task's `verify` line —
//! `cargo nextest run -p rmaild ask_mailbox` — actually selects them: nextest
//! matches a bare positional filter against a test's *name*, not against its
//! binary id, so a suite in `tests/ask_mailbox.rs` whose tests are named
//! anything else is selected by that command not at all.
//!
//! - [`ask_mailbox_streams_a_trace_then_tokens_then_citations`] — the `verify`
//!   line's "streamed tokens+citations", over the real pipeline, including
//!   that `SearchKind::Deep` genuinely routed to the Claude listwise reranker
//!   (`complete()` was called before `stream()` was).
//! - [`ask_mailbox_refuses_when_nothing_is_retrieved`] — the `verify` line's
//!   "grounded-refusal path": no context, no provider call, `grounded=false`.
//! - [`ask_mailbox_drops_a_fabricated_citation`] — an answer citing a
//!   label no source has yields no `Citation` frame and is reported ungrounded.
//! - [`ask_mailbox_never_sends_a_forbidden_folder_to_the_provider`] — the P0 shape, checked
//!   over the literal bytes of every request the provider was handed.
//! - [`ask_mailbox_rejects_an_empty_question`] — the boundary maps a domain
//!   error to the right code.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use rmail_core::ai::provider::{ChatResponse, StopReason, StreamFrame, Usage as CoreUsage};
use rmail_core::ai::{ChatRequest, Provider, ProviderStream};
use rmail_core::config::{AiPolicyMode, AiPolicyRule, RetrieversConfig};
use rmail_core::index::fts::FtsIndex;
use rmail_core::index::{extract_message, IndexQueue, QueueOptions, PRIORITY_NORMAL};
use rmail_core::repo;
use rmail_core::Error as CoreError;
use rmail_core::{Config, Database};
use rmail_proto::v1::ai_service_client::AiServiceClient;
use rmail_proto::v1::{ask_chunk, AskChunk, AskRequest, Citation, RetrievalTrace};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;
use tonic::transport::Channel;
use tonic::Code;

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// How long a stream assertion waits before failing — generous, since these
/// are liveness checks on spawned tasks, not latency measurements.
const STREAM_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// A provider that answers both halves of the pipeline and records everything
// ---------------------------------------------------------------------------

/// Stands in for `ClaudeProvider`.
///
/// It serves *both* provider-calling halves of this RPC, which is what makes
/// the end-to-end shape observable:
///
/// - `complete()` — the L2 listwise rerank (`rank::l2::claude`). It parses the
///   labels out of the prompt it was given and echoes them back in the same
///   order, so the rerank genuinely succeeds and leaves the L1 order intact.
///   A fixed canned answer would fail `L2Stage`'s "judged a different
///   candidate set" check the moment a test changed how many messages it
///   seeds.
/// - `stream()` — the answer itself.
///
/// `transmitted()` is the assertion surface the policy test needs: every
/// character of every request this provider was handed, across both methods.
#[derive(Debug, Default)]
struct MockProvider {
    answer: Mutex<Vec<String>>,
    seen: Mutex<Vec<ChatRequest>>,
    complete_calls: AtomicUsize,
    stream_calls: AtomicUsize,
}

impl MockProvider {
    fn set_answer(&self, frames: &[&str]) {
        *self.answer.lock().unwrap_or_else(PoisonError::into_inner) =
            frames.iter().map(|s| (*s).to_owned()).collect();
    }

    fn complete_calls(&self) -> usize {
        self.complete_calls.load(Ordering::SeqCst)
    }

    fn stream_calls(&self) -> usize {
        self.stream_calls.load(Ordering::SeqCst)
    }

    /// Every character that was in every request this provider was handed —
    /// the text that actually would have left the host.
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

    fn record(&self, request: &ChatRequest) {
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(request.clone());
    }
}

/// The identity listwise ordering for whatever labels the rerank prompt
/// carried — see [`MockProvider`]'s own docs for why this is derived rather
/// than canned.
fn echo_listwise(request: &ChatRequest) -> String {
    let prompt: String = request.messages.iter().map(|m| m.content.clone()).collect();
    let mut labels: Vec<i64> = Vec::new();
    let bytes = prompt.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'[' {
            i += 1;
            continue;
        }
        let Some(close) = bytes[i + 1..].iter().position(|b| *b == b']') else {
            break;
        };
        if let Ok(label) = prompt[i + 1..i + 1 + close].parse::<i64>() {
            if !labels.contains(&label) {
                labels.push(label);
            }
        }
        i += close + 2;
    }
    let results: Vec<serde_json::Value> = labels
        .into_iter()
        .map(|label| serde_json::json!({ "label": label, "why": "matched" }))
        .collect();
    serde_json::json!({ "results": results }).to_string()
}

#[async_trait]
impl Provider for MockProvider {
    async fn complete(
        &self,
        request: &ChatRequest,
        _cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<ChatResponse, CoreError> {
        self.complete_calls.fetch_add(1, Ordering::SeqCst);
        self.record(request);
        Ok(ChatResponse {
            id: "msg_mock".to_owned(),
            model: request.model.clone(),
            stop_reason: StopReason::EndTurn,
            text: echo_listwise(request),
            usage: CoreUsage::default(),
        })
    }

    async fn stream(
        &self,
        request: &ChatRequest,
        _cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<ProviderStream, CoreError> {
        self.stream_calls.fetch_add(1, Ordering::SeqCst);
        self.record(request);
        let frames = self
            .answer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let (tx, rx) = tokio::sync::mpsc::channel(32);
        tokio::spawn(async move {
            for frame in frames {
                if tx.send(Ok(StreamFrame::Token(frame))).await.is_err() {
                    return;
                }
            }
            let _ = tx.send(Ok(StreamFrame::Usage(CoreUsage::default()))).await;
            let _ = tx
                .send(Ok(StreamFrame::Done {
                    stop_reason: StopReason::EndTurn,
                }))
                .await;
        });
        Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)))
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
/// Semantic indexing off: the deterministic hash fallback keeps these tests
/// from loading — or, on a cold cache, downloading — an ONNX model none of
/// them needs (`rmaild/tests/search_service.rs` makes the identical call).
/// The rate limit is raised because one question makes two provider calls (a
/// rerank and the answer) and the shipped 60/minute would otherwise pace the
/// second one into a test's own patience.
fn base_config() -> Config {
    let mut config = Config::default();
    config.index.semantic.enabled = false;
    config.ai.limits.requests_per_minute = 1_000_000;
    // Nothing here exercises the Batch API, and a batch client built against
    // no API key just logs a warning; not building one keeps the log honest.
    config.ai.batching.enabled = false;
    config
}

/// A config whose lexical retriever is the *only* one — so "no message
/// matches" genuinely means no candidates.
///
/// `fuse::drop_prior_only_candidates` already discards recency/prefix-only
/// candidates for a free-text query, so those arms cannot manufacture a hit.
/// The fuzzy arm can: it is a subsequence matcher, and whether some nonsense
/// token happens to be a subsequence of a seeded body is a property of the
/// fixture, not of the code under test. Narrowing the fan-out makes the
/// refusal a fact rather than a coincidence.
fn lexical_only_config() -> Config {
    let mut config = base_config();
    config.search.retrievers = RetrieversConfig {
        dense: false,
        fuzzy: false,
        entity: false,
        structured: false,
        prefix: false,
        recency: false,
        ..RetrieversConfig::default()
    };
    config
}

impl TestServer {
    async fn start() -> Self {
        Self::with_config(base_config()).await
    }

    async fn with_config(config: Config) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let socket = PathBuf::from("/tmp").join(format!("rmail-ask-{pid}-{n}.sock"));
        let db_path = std::env::temp_dir().join(format!("rmail-ask-{pid}-{n}.db"));
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
        let server_provider: Arc<dyn Provider> = Arc::clone(&provider) as Arc<dyn Provider>;
        let handle = tokio::spawn(async move {
            // The real boot path: a real `SyncEngine`, a real `MailStore`, a
            // real `TagStore`, a real `SearchApi` — only the Anthropic client
            // is substituted. `serve_uds_with_config` builds the first three
            // the same way; they are constructed here only because
            // `serve_uds_injected` is the layer below it.
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
                    ai_provider: Some(server_provider),
                    reranker: None,
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

    async fn client(&self) -> AiServiceClient<Channel> {
        AiServiceClient::new(rmail_core::connect_uds(&self.socket).await.unwrap())
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

    /// Insert, extract, and lexically index a message — the real pipeline,
    /// mirroring `rmaild/tests/search_service.rs`'s own `index` helper.
    /// `extract_message` is what writes the `index_content` row the RAG
    /// packer reads, so a message seeded any other way would be retrievable
    /// and unpackable.
    async fn index(&self, mailbox_id: i64, subject: &str, body: &str) -> i64 {
        let uid = self.next_uid.get();
        self.next_uid.set(uid + 1);
        let new = repo::NewMessage {
            account_id: self.account_id,
            mailbox_id,
            uid,
            uidvalidity: 1,
            subject: Some(subject.to_owned()),
            from_addr: Some("billing@aws.example".to_owned()),
            body_text: Some(body.to_owned()),
            date: Some(1_700_000_000 + uid),
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
// Stream helpers
// ---------------------------------------------------------------------------

async fn drain(mut stream: tonic::Streaming<AskChunk>) -> Vec<AskChunk> {
    let mut out = Vec::new();
    loop {
        match tokio::time::timeout(STREAM_TIMEOUT, stream.next()).await {
            Ok(Some(Ok(chunk))) => out.push(chunk),
            Ok(Some(Err(status))) => panic!("ask stream item was an error: {status}"),
            Ok(None) => break,
            Err(_) => panic!("timed out draining the ask stream"),
        }
    }
    out
}

fn kinds(chunks: &[AskChunk]) -> Vec<&'static str> {
    chunks
        .iter()
        .map(|chunk| match &chunk.body {
            Some(ask_chunk::Body::Trace(_)) => "trace",
            Some(ask_chunk::Body::Token(_)) => "token",
            Some(ask_chunk::Body::Citation(_)) => "citation",
            Some(ask_chunk::Body::Usage(_)) => "usage",
            Some(ask_chunk::Body::Done(_)) => "done",
            None => "empty",
        })
        .collect()
}

fn answer_text(chunks: &[AskChunk]) -> String {
    chunks
        .iter()
        .filter_map(|chunk| match &chunk.body {
            Some(ask_chunk::Body::Token(token)) => Some(token.as_str()),
            _ => None,
        })
        .collect()
}

fn citations(chunks: &[AskChunk]) -> Vec<Citation> {
    chunks
        .iter()
        .filter_map(|chunk| match &chunk.body {
            Some(ask_chunk::Body::Citation(citation)) => Some(citation.clone()),
            _ => None,
        })
        .collect()
}

fn trace(chunks: &[AskChunk]) -> RetrievalTrace {
    chunks
        .iter()
        .find_map(|chunk| match &chunk.body {
            Some(ask_chunk::Body::Trace(trace)) => Some(trace.clone()),
            _ => None,
        })
        .expect("every answer opens with a trace")
}

fn grounded(chunks: &[AskChunk]) -> bool {
    chunks
        .iter()
        .find_map(|chunk| match &chunk.body {
            Some(ask_chunk::Body::Done(done)) => Some(done.grounded),
            _ => None,
        })
        .expect("every answer ends with a done frame")
}

fn refusal(chunks: &[AskChunk]) -> String {
    chunks
        .iter()
        .find_map(|chunk| match &chunk.body {
            Some(ask_chunk::Body::Done(done)) => Some(done.refusal.clone()),
            _ => None,
        })
        .expect("every answer ends with a done frame")
}

fn question(text: &str) -> AskRequest {
    AskRequest {
        question: text.to_owned(),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ask_mailbox_streams_a_trace_then_tokens_then_citations() {
    let server = TestServer::start().await;
    let invoice = server
        .index(
            server.inbox_id,
            "Your AWS invoice for Q2",
            "Your AWS invoice for the second quarter totals 4200 dollars, due on the fifteenth.",
        )
        .await;
    server
        .index(
            server.inbox_id,
            "AWS newsletter",
            "This quarter's AWS product newsletter, with no invoice or billing figures at all.",
        )
        .await;
    server
        .provider
        .set_answer(&["AWS billed ", "4200 dollars in Q2 [1]."]);

    let chunks = drain(
        server
            .client()
            .await
            .ask_mailbox(question("AWS invoice"))
            .await
            .expect("AskMailbox RPC")
            .into_inner(),
    )
    .await;

    let shape = kinds(&chunks);
    let t0 = trace(&chunks);
    assert_eq!(shape.first(), Some(&"trace"), "frames were {shape:?}");
    assert_eq!(shape.last(), Some(&"done"), "frames were {shape:?}");
    // Every citation comes after every token, and the usage frame after both.
    let last_token = shape
        .iter()
        .rposition(|k| *k == "token")
        .unwrap_or_else(|| panic!("no token frame; frames {shape:?}, trace {t0:?}"));
    let first_citation = shape
        .iter()
        .position(|k| *k == "citation")
        .unwrap_or_else(|| panic!("no citation frame; frames {shape:?}, trace {t0:?}"));
    assert!(
        last_token < first_citation,
        "citations must follow the prose: {shape:?}"
    );

    assert_eq!(answer_text(&chunks), "AWS billed 4200 dollars in Q2 [1].");
    let cited = citations(&chunks);
    assert_eq!(cited.len(), 1);
    assert_eq!(cited[0].message_id, invoice);
    assert_eq!(cited[0].label, 1);
    assert_eq!(cited[0].mailbox, "INBOX");
    assert_eq!(cited[0].account_id, server.account_id);
    assert!(
        cited[0].quote.contains("4200") || cited[0].quote.contains("invoice"),
        "the quote should be drawn from the cited message: {:?}",
        cited[0].quote
    );
    assert!(grounded(&chunks));
    assert!(refusal(&chunks).is_empty());

    let t = trace(&chunks);
    assert!(t.retrieved >= 2, "retrieved {}", t.retrieved);
    assert_eq!(t.packed, t.retrieved);
    assert_eq!(t.withheld_by_policy, 0);
    assert!(t.context_tokens > 0);
    assert_eq!(t.model, Config::default().ai.ask.model);

    // The deep-search seam task 51 built: under `search.rerank = auto`, a
    // question is a *deep* search, which is what routes to the Claude listwise
    // reranker. If `AskSearch` ever quietly used `SearchKind::Interactive`,
    // the local cross-encoder would run instead and this would be zero.
    assert_eq!(
        server.provider.complete_calls(),
        1,
        "a deep search must reach the Claude listwise reranker"
    );
    assert_eq!(server.provider.stream_calls(), 1);
    server.stop().await;
}

#[tokio::test]
async fn ask_mailbox_refuses_when_nothing_is_retrieved() {
    let server = TestServer::with_config(lexical_only_config()).await;
    server
        .index(
            server.inbox_id,
            "Lunch on Friday",
            "Are you free for lunch on Friday? The usual place.",
        )
        .await;
    server.provider.set_answer(&["this should never be sent"]);

    let chunks = drain(
        server
            .client()
            .await
            .ask_mailbox(question("zzzqqxjunobtainium"))
            .await
            .expect("AskMailbox RPC")
            .into_inner(),
    )
    .await;

    assert_eq!(
        server.provider.stream_calls(),
        0,
        "a refusal with no context must not cost a provider call"
    );
    assert!(answer_text(&chunks).is_empty());
    assert!(!grounded(&chunks));
    assert!(
        refusal(&chunks).contains("no message in your mailbox"),
        "refusal was {:?}",
        refusal(&chunks)
    );
    let t = trace(&chunks);
    assert_eq!(t.retrieved, 0);
    assert_eq!(t.packed, 0);
    assert!(
        t.model.is_empty(),
        "no model was called, so none should be named"
    );
    assert!(citations(&chunks).is_empty());
    server.stop().await;
}

#[tokio::test]
async fn ask_mailbox_drops_a_fabricated_citation() {
    let server = TestServer::start().await;
    server
        .index(
            server.inbox_id,
            "Your AWS invoice for Q2",
            "Your AWS invoice for the second quarter totals 4200 dollars.",
        )
        .await;
    server
        .index(
            server.inbox_id,
            "AWS newsletter",
            "This quarter's AWS product newsletter, no billing figures.",
        )
        .await;
    // Two dangling labels and a zero, none of which any source has.
    server
        .provider
        .set_answer(&["It was 40 dollars [9], see also [42] and [0]."]);

    let chunks = drain(
        server
            .client()
            .await
            .ask_mailbox(question("AWS invoice"))
            .await
            .expect("AskMailbox RPC")
            .into_inner(),
    )
    .await;

    assert!(
        citations(&chunks).is_empty(),
        "a label no source has must produce no citation"
    );
    assert!(
        !grounded(&chunks),
        "an answer whose every citation was fabricated is not grounded"
    );
    assert!(refusal(&chunks).contains("cited no message"));
    // The prose still reaches the client — suppressing it would hide the
    // model's own words — but nothing downstream can mistake it for sourced.
    assert!(answer_text(&chunks).contains("40 dollars"));
    server.stop().await;
}

#[tokio::test]
async fn ask_mailbox_never_sends_a_forbidden_folder_to_the_provider() {
    let mut config = base_config();
    config.ai.policy.rules = vec![AiPolicyRule {
        account: None,
        folder: Some("Legal".to_owned()),
        mode: AiPolicyMode::Forbidden,
        residency: None,
        reason: Some("privileged correspondence".to_owned()),
    }];
    let server = TestServer::with_config(config).await;
    let legal = server.mailbox("Legal").await;

    // The privileged message is the *better* lexical match for the question,
    // so a gate applied after packing — or not at all — would put it in the
    // prompt ahead of the permitted one.
    server
        .index(
            legal,
            "Settlement figure",
            "The settlement figure agreed today is nine million dollars, privileged and \
             confidential.",
        )
        .await;
    let public = server
        .index(
            server.inbox_id,
            "Published figure",
            "The published settlement figure in the press release is four dollars.",
        )
        .await;
    server
        .provider
        .set_answer(&["The published figure is four dollars [1]."]);

    let chunks = drain(
        server
            .client()
            .await
            .ask_mailbox(question("settlement figure"))
            .await
            .expect("AskMailbox RPC")
            .into_inner(),
    )
    .await;

    let transmitted = server.provider.transmitted();
    for forbidden in [
        "nine million",
        "privileged and",
        "Settlement figure",
        "agreed today",
    ] {
        assert!(
            !transmitted.contains(forbidden),
            "a forbidden folder's text ({forbidden:?}) reached the provider"
        );
    }
    assert!(
        transmitted.contains("published settlement figure"),
        "the permitted message should have been packed"
    );

    let t = trace(&chunks);
    assert!(t.withheld_by_policy >= 1, "trace was {t:?}");
    assert_eq!(t.packed, t.retrieved - t.withheld_by_policy);

    // And no citation names the withheld message.
    for citation in citations(&chunks) {
        assert_eq!(
            citation.message_id, public,
            "a citation named a message the policy withheld"
        );
    }
    server.stop().await;
}

#[tokio::test]
async fn ask_mailbox_rejects_an_empty_question() {
    let server = TestServer::start().await;
    let status = server
        .client()
        .await
        .ask_mailbox(question("   "))
        .await
        .expect_err("an empty question is not a question");
    assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");
    assert_eq!(server.provider.stream_calls(), 0);
    server.stop().await;
}

/// A retrieval failure must keep its reason across the `Status`→`Error` seam
/// between `SearchApi` and the RAG engine.
///
/// Before task 40 that seam wrapped every failure in `Error::Internal`, which
/// cost the caller twice: the code became `INTERNAL`, and the boundary then
/// scrubbed the message — so naming an account that does not exist reported
/// "internal error" and was indistinguishable from a daemon bug. A client
/// branches on the reason, and `INTERNAL` tells it to give up on a mistake it
/// could have fixed.
#[tokio::test]
async fn ask_mailbox_reports_an_unknown_account_as_not_found() {
    let server = TestServer::start().await;
    let status = server
        .client()
        .await
        .ask_mailbox(AskRequest {
            question: "how much did AWS bill me?".to_owned(),
            account_id: 999_999,
            ..Default::default()
        })
        .await
        .expect_err("an unknown account cannot be retrieved from");

    assert_eq!(status.code(), Code::NotFound, "{status:?}");
    assert!(
        status.message().contains("999999") || status.message().contains("account"),
        "the client-safe detail must survive the round trip: {status:?}"
    );
    assert_eq!(
        server.provider.stream_calls(),
        0,
        "a retrieval failure must not reach the provider"
    );
    server.stop().await;
}
