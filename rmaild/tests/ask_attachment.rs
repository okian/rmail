//! Integration test: drive `AttachmentService.AskAttachment` end-to-end
//! against an in-process tonic server booted through the **real** daemon
//! wiring (`rmaild::serve_uds_injected`), with only the Anthropic client
//! faked.
//!
//! # What each test proves
//!
//! - [`ask_attachment_streams_a_page_cited_answer`] — the `verify` line's
//!   "page-cited answer": a question about one attachment streams a trace,
//!   tokens and a citation naming the part *and* the page the clause is on.
//! - [`ask_attachment_refuses_an_answer_the_document_does_not_support`] — the
//!   `verify` line's "unsupported refusal": prose that cites nothing is
//!   reported ungrounded, so no client can present it as sourced.
//! - [`ask_attachment_never_sends_a_forbidden_folder_to_the_provider`] — the
//!   P0 shape, checked over the literal bytes of every request the provider
//!   was handed.
//! - [`ask_attachment_drops_a_fabricated_citation`] — a label no passage has
//!   yields no citation.
//! - [`ask_attachment_answers_over_a_searched_result_set`] — the second scope:
//!   no attachment named, retrieval by the question itself.
//! - [`ask_attachment_rejects_half_a_scope`] /
//!   [`ask_attachment_rejects_an_empty_question`] — the boundary maps domain
//!   errors to the right codes.
//!
//! Every name starts with `ask_attachment_` so a bare positional nextest
//! filter selects them: nextest matches such a filter against a test's *name*,
//! not against its binary id.
//!
//! Semantic indexing is off, as in every other `rmaild` test — `true` would
//! make each test load (and on a cold cache download) an ONNX model. The
//! passages these tests pack therefore come from the lexical arm and from the
//! question's own words, both deterministic.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use rmail_core::ai::provider::{ChatResponse, StopReason, StreamFrame, Usage as CoreUsage};
use rmail_core::ai::{ChatRequest, Provider, ProviderStream};
use rmail_core::attach::extract_attachments;
use rmail_core::config::{AiPolicyMode, AiPolicyRule, IndexExtractConfig};
use rmail_core::repo;
use rmail_core::Error as CoreError;
use rmail_core::{Config, Database};
use rmail_proto::v1::attachment_service_client::AttachmentServiceClient;
use rmail_proto::v1::{
    ask_attachment_chunk, AskAttachmentChunk, AskAttachmentRequest, AttachmentCitation,
    AttachmentRetrievalTrace,
};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;
use tonic::transport::Channel;
use tonic::Code;

mod attach_fixture;
use attach_fixture::{message_with, pdf_bytes};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// How long a stream assertion waits before failing — generous, since these
/// are liveness checks on spawned tasks, not latency measurements.
const STREAM_TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// A provider that records everything it is handed
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct MockProvider {
    answer: Mutex<Vec<String>>,
    seen: Mutex<Vec<ChatRequest>>,
    stream_calls: AtomicUsize,
}

impl MockProvider {
    fn set_answer(&self, frames: &[&str]) {
        *self.answer.lock().unwrap_or_else(PoisonError::into_inner) =
            frames.iter().map(|s| (*s).to_owned()).collect();
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
}

#[async_trait]
impl Provider for MockProvider {
    async fn complete(
        &self,
        _request: &ChatRequest,
        _cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<ChatResponse, CoreError> {
        // `AskAttachment` never completes; the L2 reranker would, but nothing
        // in these tests runs a message search.
        Ok(ChatResponse {
            id: "msg_mock".to_owned(),
            model: "mock".to_owned(),
            stop_reason: StopReason::EndTurn,
            text: "{\"results\": []}".to_owned(),
            usage: CoreUsage::default(),
        })
    }

    async fn stream(
        &self,
        request: &ChatRequest,
        _cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<ProviderStream, CoreError> {
        self.stream_calls.fetch_add(1, Ordering::SeqCst);
        self.seen
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(request.clone());
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
    account_id: i64,
    inbox_id: i64,
    next_uid: std::cell::Cell<i64>,
    provider: Arc<MockProvider>,
    shutdown: oneshot::Sender<()>,
    handle: JoinHandle<Result<(), rmaild::ServeError>>,
}

fn base_config() -> Config {
    let mut config = Config::default();
    config.index.semantic.enabled = false;
    // One question is one provider call, but the shipped 60/minute would
    // still pace a suite of them into a test's own patience.
    config.ai.limits.requests_per_minute = 1_000_000;
    config.ai.batching.enabled = false;
    config
}

impl TestServer {
    async fn start() -> Self {
        Self::with_config(base_config()).await
    }

    async fn with_config(config: Config) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let socket = PathBuf::from("/tmp").join(format!("rmail-atask-{pid}-{n}.sock"));
        let db_path = std::env::temp_dir().join(format!("rmail-atask-{pid}-{n}.db"));
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

        let provider = Arc::new(MockProvider::default());
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let server_socket = socket.clone();
        let server_db = db.clone();
        let server_provider: Arc<dyn Provider> = Arc::clone(&provider) as Arc<dyn Provider>;
        let handle = tokio::spawn(async move {
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
            account_id,
            inbox_id,
            next_uid: std::cell::Cell::new(1),
            provider,
            shutdown: shutdown_tx,
            handle,
        }
    }

    async fn client(&self) -> AttachmentServiceClient<Channel> {
        AttachmentServiceClient::new(rmail_core::connect_uds(&self.socket).await.unwrap())
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

    /// Seed a message carrying `attachments` and run the real extraction
    /// pipeline over it — the same call the indexer makes.
    async fn index(&self, mailbox_id: i64, attachments: &[(&str, &str, &[u8])]) -> i64 {
        let raw = message_with(attachments);
        let uid = self.next_uid.get();
        self.next_uid.set(uid + 1);
        let (account_id, mailbox_id) = (self.account_id, mailbox_id);
        let message_id = self
            .db
            .with_write(move |c| {
                repo::insert_message(
                    c,
                    &repo::NewMessage {
                        account_id,
                        mailbox_id,
                        uid,
                        uidvalidity: 1,
                        subject: Some("With attachments".to_owned()),
                        from_addr: Some("ada@example.com".to_owned()),
                        raw: Some(raw),
                        date: Some(1_700_000_000 + uid),
                        ..Default::default()
                    },
                )
            })
            .unwrap();
        let meta: Vec<(String, String, String, i64)> = attachments
            .iter()
            .enumerate()
            .map(|(index, (filename, content_type, bytes))| {
                (
                    index.to_string(),
                    (*filename).to_owned(),
                    (*content_type).to_owned(),
                    bytes.len() as i64,
                )
            })
            .collect();
        self.db
            .write(move |c| {
                for (part_id, filename, content_type, size) in &meta {
                    c.execute(
                        "INSERT INTO attachments
                             (message_id, part_id, filename, content_type, size)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        rusqlite::params![message_id, part_id, filename, content_type, size],
                    )?;
                }
                Ok(())
            })
            .await
            .unwrap();
        extract_attachments(&self.db, &IndexExtractConfig::default(), message_id)
            .await
            .unwrap();
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

/// A three-page contract whose operative clause is on page two.
fn contract() -> Vec<u8> {
    pdf_bytes(&[
        "Recitals and definitions for the parties to this agreement",
        "Either party may terminate this agreement for convenience on thirty days notice",
        "Signatures and counterparts executed by the parties hereto",
    ])
}

// ---------------------------------------------------------------------------
// Stream helpers
// ---------------------------------------------------------------------------

async fn drain(mut stream: tonic::Streaming<AskAttachmentChunk>) -> Vec<AskAttachmentChunk> {
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

fn kinds(chunks: &[AskAttachmentChunk]) -> Vec<&'static str> {
    chunks
        .iter()
        .map(|chunk| match &chunk.body {
            Some(ask_attachment_chunk::Body::Trace(_)) => "trace",
            Some(ask_attachment_chunk::Body::Token(_)) => "token",
            Some(ask_attachment_chunk::Body::Citation(_)) => "citation",
            Some(ask_attachment_chunk::Body::Usage(_)) => "usage",
            Some(ask_attachment_chunk::Body::Done(_)) => "done",
            None => "empty",
        })
        .collect()
}

fn answer_text(chunks: &[AskAttachmentChunk]) -> String {
    chunks
        .iter()
        .filter_map(|chunk| match &chunk.body {
            Some(ask_attachment_chunk::Body::Token(token)) => Some(token.as_str()),
            _ => None,
        })
        .collect()
}

fn citations(chunks: &[AskAttachmentChunk]) -> Vec<AttachmentCitation> {
    chunks
        .iter()
        .filter_map(|chunk| match &chunk.body {
            Some(ask_attachment_chunk::Body::Citation(citation)) => Some(citation.clone()),
            _ => None,
        })
        .collect()
}

fn trace(chunks: &[AskAttachmentChunk]) -> AttachmentRetrievalTrace {
    chunks
        .iter()
        .find_map(|chunk| match &chunk.body {
            Some(ask_attachment_chunk::Body::Trace(trace)) => Some(trace.clone()),
            _ => None,
        })
        .expect("every answer opens with a trace")
}

fn grounded(chunks: &[AskAttachmentChunk]) -> bool {
    chunks
        .iter()
        .find_map(|chunk| match &chunk.body {
            Some(ask_attachment_chunk::Body::Done(done)) => Some(done.grounded),
            _ => None,
        })
        .expect("every answer ends with a done frame")
}

fn refusal(chunks: &[AskAttachmentChunk]) -> String {
    chunks
        .iter()
        .find_map(|chunk| match &chunk.body {
            Some(ask_attachment_chunk::Body::Done(done)) => Some(done.refusal.clone()),
            _ => None,
        })
        .expect("every answer ends with a done frame")
}

fn about(message_id: i64, part_id: &str, question: &str) -> AskAttachmentRequest {
    AskAttachmentRequest {
        question: question.to_owned(),
        message_id,
        part_id: part_id.to_owned(),
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ask_attachment_streams_a_page_cited_answer() {
    let server = TestServer::start().await;
    let message_id = server
        .index(
            server.inbox_id,
            &[("contract.pdf", "application/pdf", &contract())],
        )
        .await;
    server
        .provider
        .set_answer(&["Either party may terminate ", "for convenience [1]."]);

    let chunks = drain(
        server
            .client()
            .await
            .ask_attachment(about(
                message_id,
                "0",
                "terminate this agreement for convenience",
            ))
            .await
            .expect("AskAttachment RPC")
            .into_inner(),
    )
    .await;

    let shape = kinds(&chunks);
    assert_eq!(shape.first(), Some(&"trace"), "frames were {shape:?}");
    assert_eq!(shape.last(), Some(&"done"), "frames were {shape:?}");
    let last_token = shape
        .iter()
        .rposition(|kind| *kind == "token")
        .unwrap_or_else(|| panic!("no token frame; frames {shape:?}"));
    let first_citation = shape
        .iter()
        .position(|kind| *kind == "citation")
        .unwrap_or_else(|| panic!("no citation frame; frames {shape:?}"));
    assert!(
        last_token < first_citation,
        "citations must follow the prose: {shape:?}"
    );

    assert_eq!(
        answer_text(&chunks),
        "Either party may terminate for convenience [1]."
    );
    let cited = citations(&chunks);
    assert_eq!(cited.len(), 1, "{cited:?}");
    assert_eq!(cited[0].label, 1);
    assert_eq!(cited[0].message_id, message_id);
    assert_eq!(cited[0].part_id, "0");
    assert_eq!(cited[0].filename, "contract.pdf");
    assert_eq!(cited[0].mailbox, "INBOX");
    assert_eq!(cited[0].account_id, server.account_id);
    // The acceptance criterion: a page citation, naming the page the clause
    // is actually on.
    assert_eq!(
        cited[0].page,
        Some(2),
        "the clause is on page two; citation was {:?}",
        cited[0]
    );
    // The "section" half, which is what exists for an unpaginated format.
    assert!(cited[0].span_end > cited[0].span_start);
    assert!(
        cited[0].quote.to_lowercase().contains("convenience"),
        "the quote should come from the cited passage: {:?}",
        cited[0].quote
    );
    // And the passage is text from that page and no other. Without this the
    // page assertion above passes for a window spanning the whole document —
    // which is what a three-page contract shorter than one passage produces,
    // and the citation then names a page most of its own text is not on.
    let quote = cited[0].quote.replace('…', "");
    assert!(
        !quote.contains("Recitals") && !quote.contains("Signatures"),
        "the cited passage spans more than the page it names: {quote:?}"
    );
    assert!(grounded(&chunks));
    assert!(refusal(&chunks).is_empty());

    let t = trace(&chunks);
    assert_eq!(t.retrieved, 1);
    assert_eq!(t.attachments, 1);
    assert!(t.passages >= 1);
    assert_eq!(t.withheld_by_policy, 0);
    assert!(t.context_tokens > 0);
    assert_eq!(t.model, Config::default().ai.ask.model);
    assert_eq!(server.provider.stream_calls(), 1);
    server.stop().await;
}

/// The `verify` line's refusal: prose the passages do not support cites
/// nothing and is reported ungrounded.
#[tokio::test]
async fn ask_attachment_refuses_an_answer_the_document_does_not_support() {
    let server = TestServer::start().await;
    let message_id = server
        .index(
            server.inbox_id,
            &[("contract.pdf", "application/pdf", &contract())],
        )
        .await;
    server
        .provider
        .set_answer(&["The attachment text you were shown does not say."]);

    let chunks = drain(
        server
            .client()
            .await
            .ask_attachment(about(message_id, "0", "what is the governing law"))
            .await
            .expect("AskAttachment RPC")
            .into_inner(),
    )
    .await;

    assert!(citations(&chunks).is_empty());
    assert!(!grounded(&chunks));
    assert!(
        refusal(&chunks).contains("cited no attachment passage"),
        "refusal was {:?}",
        refusal(&chunks)
    );
    // The prose still reaches the client — suppressing it would hide the
    // model correctly saying it could not find anything.
    assert!(answer_text(&chunks).contains("does not say"));
    server.stop().await;
}

/// An attachment with no extracted text refuses before a provider call.
#[tokio::test]
async fn ask_attachment_refuses_without_a_provider_call_when_there_is_no_text() {
    let server = TestServer::start().await;
    let message_id = server
        .index(
            server.inbox_id,
            &[("blob.bin", "application/octet-stream", b"\x00\x01\x02\x03")],
        )
        .await;
    server.provider.set_answer(&["this should never be sent"]);

    let chunks = drain(
        server
            .client()
            .await
            .ask_attachment(about(message_id, "0", "what does it say"))
            .await
            .expect("AskAttachment RPC")
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
        refusal(&chunks).contains("no extracted attachment text"),
        "refusal was {:?}",
        refusal(&chunks)
    );
    let t = trace(&chunks);
    assert_eq!(t.passages, 0);
    assert!(t.model.is_empty(), "no model was called, so none is named");
    server.stop().await;
}

#[tokio::test]
async fn ask_attachment_drops_a_fabricated_citation() {
    let server = TestServer::start().await;
    let message_id = server
        .index(
            server.inbox_id,
            &[("contract.pdf", "application/pdf", &contract())],
        )
        .await;
    server
        .provider
        .set_answer(&["Delaware law governs [9], see also [42] and [0]."]);

    let chunks = drain(
        server
            .client()
            .await
            .ask_attachment(about(message_id, "0", "governing law"))
            .await
            .expect("AskAttachment RPC")
            .into_inner(),
    )
    .await;

    assert!(
        citations(&chunks).is_empty(),
        "a label no passage has must produce no citation"
    );
    assert!(!grounded(&chunks));
    assert!(answer_text(&chunks).contains("Delaware"));
    server.stop().await;
}

/// The P0 shape, checked over the literal bytes of every request the provider
/// was handed.
#[tokio::test]
async fn ask_attachment_never_sends_a_forbidden_folder_to_the_provider() {
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
    let message_id = server
        .index(
            legal,
            &[(
                "settlement.txt",
                "text/plain",
                "The settlement figure agreed today is nine million dollars, privileged and \
                 confidential."
                    .as_bytes(),
            )],
        )
        .await;
    server.provider.set_answer(&["this must never be sent"]);

    let chunks = drain(
        server
            .client()
            .await
            .ask_attachment(about(message_id, "0", "settlement figure"))
            .await
            .expect("AskAttachment RPC")
            .into_inner(),
    )
    .await;

    assert_eq!(
        server.provider.stream_calls(),
        0,
        "a withheld attachment must not cost a provider call"
    );
    let transmitted = server.provider.transmitted();
    for forbidden in ["nine million", "privileged and", "settlement.txt"] {
        assert!(
            !transmitted.contains(forbidden),
            "a forbidden folder's attachment text ({forbidden:?}) reached the provider"
        );
    }
    let t = trace(&chunks);
    assert_eq!(t.withheld_by_policy, 1);
    assert_eq!(t.passages, 0);
    assert!(!grounded(&chunks));
    server.stop().await;
}

/// The second scope: no attachment named, so retrieval runs over the question
/// itself and a forbidden folder is still never packed.
#[tokio::test]
async fn ask_attachment_answers_over_a_searched_result_set() {
    let mut config = base_config();
    config.ai.policy.rules = vec![AiPolicyRule {
        account: None,
        folder: Some("Legal".to_owned()),
        mode: AiPolicyMode::Forbidden,
        residency: None,
        reason: None,
    }];
    let server = TestServer::with_config(config).await;
    let legal = server.mailbox("Legal").await;
    server
        .index(
            legal,
            &[(
                "privileged.txt",
                "text/plain",
                b"Termination for convenience: the privileged nine million figure applies."
                    as &[u8],
            )],
        )
        .await;
    let public = server
        .index(
            server.inbox_id,
            &[(
                "public.txt",
                "text/plain",
                b"Termination for convenience: the published figure is four dollars." as &[u8],
            )],
        )
        .await;
    server
        .provider
        .set_answer(&["The published figure is four dollars [1]."]);

    let chunks = drain(
        server
            .client()
            .await
            .ask_attachment(AskAttachmentRequest {
                question: "termination for convenience".to_owned(),
                ..Default::default()
            })
            .await
            .expect("AskAttachment RPC")
            .into_inner(),
    )
    .await;

    let transmitted = server.provider.transmitted();
    assert!(
        !transmitted.contains("nine million"),
        "a forbidden folder's attachment reached the provider"
    );
    assert!(transmitted.contains("published figure"));

    let t = trace(&chunks);
    assert_eq!(t.retrieved, 2, "both attachments were retrieved: {t:?}");
    assert_eq!(t.withheld_by_policy, 1);
    assert_eq!(t.attachments, 1);
    let cited = citations(&chunks);
    assert_eq!(cited.len(), 1);
    assert_eq!(cited[0].message_id, public);
    assert!(grounded(&chunks));
    server.stop().await;
}

#[tokio::test]
async fn ask_attachment_rejects_an_empty_question() {
    let server = TestServer::start().await;
    let status = server
        .client()
        .await
        .ask_attachment(about(1, "0", "   "))
        .await
        .expect_err("an empty question is not a question");
    assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");
    assert_eq!(server.provider.stream_calls(), 0);
    server.stop().await;
}

/// A part id names an attachment *of a message*, so half the pair is a
/// mistake rather than a third scope.
#[tokio::test]
async fn ask_attachment_rejects_half_a_scope() {
    let server = TestServer::start().await;
    for request in [
        about(7, "", "what does it say"),
        about(0, "0", "what does it say"),
    ] {
        let status = server
            .client()
            .await
            .ask_attachment(request)
            .await
            .expect_err("half a scope is not a scope");
        assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");
    }
    assert_eq!(server.provider.stream_calls(), 0);
    server.stop().await;
}
