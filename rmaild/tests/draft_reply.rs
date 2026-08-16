//! Integration test: AI reply drafting and the tone/length rewrite (task 62)
//! driven end-to-end through `ComposeService` over an in-process tonic server
//! on a Unix domain socket, backed by a real `rmail_core::compose::DraftStore`
//! over a real (temp-file) database.
//!
//! The judgement itself is covered where it lives — `rmail-core`'s
//! `compose::reply` tests. What this file owes is the *boundary*, and
//! specifically the four things that are only true once the whole path is
//! assembled:
//!
//! * a streamed reply arrives as typed frames, in the contractual order, and
//!   the tokens a client concatenated are the body the draft was staged with;
//! * the staged draft carries the reply headers derived from the parent —
//!   `To`, `Re:` subject, `In-Reply-To`, `References` — and **nothing reaches
//!   the outbox**, before or after;
//! * a rewrite produces a revision a client can list, cycle and revert
//!   through the RPCs, without a second one for "revert";
//! * a daemon with no AI provider answers `FAILED_PRECONDITION` to the two
//!   model-calling RPCs rather than pretending — while the revision RPCs,
//!   which call no model, keep working.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use futures::Stream;
use rmail_core::ai::provider::{
    ChatRequest, ChatResponse, Provider, ProviderStream, StopReason, StreamFrame, Usage,
};
use rmail_core::ai::queue::RateLimiter;
use rmail_core::ai::PolicyEngine;
use rmail_core::compose::reply::ReplyDrafter;
use rmail_core::compose::DraftStore;
use rmail_core::config::{Config, SendReply};
use rmail_core::{repo, Database, Error};
use rmail_proto::v1::compose_service_client::ComposeServiceClient;
use rmail_proto::v1::{
    draft_reply_event, CreateDraftRequest, Draft, DraftAddress, DraftReplyEvent, DraftReplyRequest,
    ListDraftRevisionsRequest, RewriteDraftRequest, RewriteLength, RewriteTone,
    SelectDraftRevisionRequest,
};
use tokio::sync::{oneshot, Semaphore};
use tokio::task::JoinHandle;
use tokio_stream::wrappers::UnixListenerStream;
use tokio_stream::StreamExt as _;
use tokio_util::sync::CancellationToken;
use tonic::transport::{Channel, Server};
use tonic::Code;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

// ---------------------------------------------------------------------------
// A scripted provider
// ---------------------------------------------------------------------------

/// Answers from a script and fails once it runs out — which is exactly how an
/// unreachable provider behaves.
#[derive(Debug, Default)]
struct MockProvider {
    script: Mutex<Vec<Vec<String>>>,
    calls: AtomicUsize,
}

impl MockProvider {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Queue one answer, split into several tokens so a streaming assertion
    /// proves the relay concatenates rather than passing one blob through.
    fn queue(&self, text: &str) {
        self.script
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(text.split_inclusive(' ').map(str::to_owned).collect());
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn next(&self) -> Option<Vec<String>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let mut script = self.script.lock().unwrap_or_else(PoisonError::into_inner);
        if script.is_empty() {
            None
        } else {
            Some(script.remove(0))
        }
    }
}

#[async_trait]
impl Provider for MockProvider {
    async fn complete(
        &self,
        request: &ChatRequest,
        _cancel: &CancellationToken,
    ) -> Result<ChatResponse, Error> {
        match self.next() {
            Some(tokens) => Ok(ChatResponse {
                id: "msg_mock".to_owned(),
                model: request.model.clone(),
                stop_reason: StopReason::EndTurn,
                text: tokens.concat(),
                usage: Usage::default(),
            }),
            None => Err(Error::unavailable(
                "mock provider: the network is down".to_owned(),
            )),
        }
    }

    async fn stream(
        &self,
        _request: &ChatRequest,
        _cancel: &CancellationToken,
    ) -> Result<ProviderStream, Error> {
        let Some(tokens) = self.next() else {
            return Err(Error::unavailable(
                "mock provider: the network is down".to_owned(),
            ));
        };
        let mut frames: Vec<Result<StreamFrame, Error>> = tokens
            .into_iter()
            .map(|token| Ok(StreamFrame::Token(token)))
            .collect();
        frames.push(Ok(StreamFrame::Usage(Usage::default())));
        frames.push(Ok(StreamFrame::Done {
            stop_reason: StopReason::EndTurn,
        }));
        Ok(Box::pin(tokio_stream::iter(frames)))
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct TestServer {
    socket: PathBuf,
    db_path: PathBuf,
    db: Database,
    account_id: i64,
    mailbox_id: i64,
    next_uid: AtomicI64,
    provider: Arc<MockProvider>,
    shutdown: oneshot::Sender<()>,
    handle: JoinHandle<()>,
}

impl TestServer {
    async fn start() -> Self {
        Self::start_with(true).await
    }

    /// `with_ai = false` stands in for a daemon whose AI subsystem never came
    /// up: `rmaild::serve` leaves the drafter unset there, and the RPCs have
    /// to behave.
    async fn start_with(with_ai: bool) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let socket = PathBuf::from("/tmp").join(format!("rmail-draft-reply-{pid}-{n}.sock"));
        let db_path = std::env::temp_dir().join(format!("rmail-draft-reply-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", db_path.display())));
        }
        let _ = std::fs::remove_file(&socket);

        let db = Database::open(&db_path).unwrap();
        let (account_id, mailbox_id) = db
            .write(|c| {
                let account_id = repo::insert_account(
                    c,
                    &repo::NewAccount {
                        name: "Personal".to_owned(),
                        username: Some("alice@example.com".to_owned()),
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

        let provider = MockProvider::new();
        let mut api = rmaild::ComposeApi::new(
            DraftStore::new(db.clone()),
            rmail_core::idempotency::IdempotencyStore::new(
                db.clone(),
                Duration::from_secs(3600),
                Duration::from_secs(60),
            ),
            db.clone(),
            CancellationToken::new(),
        );
        if with_ai {
            let policy = Arc::new(PolicyEngine::from_config(&Config::default()).unwrap());
            api = api.with_drafter(ReplyDrafter::new(
                db.clone(),
                Arc::clone(&provider) as Arc<dyn Provider>,
                policy,
                Config::default().ai.privacy.clone(),
                Config::default().ai.limits.clone(),
                SendReply::default(),
                Arc::new(Semaphore::new(4)),
                // Not `0` — see `RateLimiter`: zero means one free token and
                // then an effectively infinite wait, so a second call hangs.
                Arc::new(RateLimiter::new(1_000_000)),
            ));
        }

        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let incoming = UnixListenerStream::new(listener);
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            let _ = Server::builder()
                .add_service(
                    rmail_proto::v1::compose_service_server::ComposeServiceServer::new(api),
                )
                .serve_with_incoming_shutdown(incoming, async move {
                    let _ = shutdown_rx.await;
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
            account_id,
            mailbox_id,
            next_uid: AtomicI64::new(1),
            provider,
            shutdown: shutdown_tx,
            handle,
        }
    }

    async fn client(&self) -> ComposeServiceClient<Channel> {
        ComposeServiceClient::new(rmail_core::connect_uds(&self.socket).await.unwrap())
    }

    /// One incoming message from Bob, addressed to Alice.
    async fn incoming(&self) -> i64 {
        let uid = self.next_uid.fetch_add(1, Ordering::Relaxed);
        let (account_id, mailbox_id) = (self.account_id, self.mailbox_id);
        self.db
            .write(move |c| {
                c.execute(
                    "INSERT INTO messages
                         (account_id, mailbox_id, uid, uidvalidity, message_id, subject,
                          from_addr, from_name, to_addrs, cc_addrs, body_text, date)
                     VALUES (?1, ?2, ?3, 1, 'parent@example.net', 'Quarterly numbers',
                             'bob@example.net', 'Bob Stone', 'alice@example.com',
                             'carol@example.org', 'Can you confirm the Q3 figure?', 1700000000)",
                    rusqlite::params![account_id, mailbox_id, uid],
                )?;
                Ok(c.last_insert_rowid())
            })
            .await
            .unwrap()
    }

    /// A plain hand-written draft, through the same RPC a composer uses.
    async fn draft(&self, body: &str) -> Draft {
        self.client()
            .await
            .create_draft(CreateDraftRequest {
                idempotency_key: String::new(),
                account_id: self.account_id,
                from: Some(DraftAddress {
                    address: "alice@example.com".to_owned(),
                    display_name: "Alice".to_owned(),
                }),
                to: vec![DraftAddress {
                    address: "bob@example.net".to_owned(),
                    display_name: String::new(),
                }],
                cc: Vec::new(),
                bcc: Vec::new(),
                subject: "Re: numbers".to_owned(),
                body_text: body.to_owned(),
                body_html: None,
                attachments: Vec::new(),
                in_reply_to_message_id: None,
            })
            .await
            .unwrap()
            .into_inner()
    }

    async fn outbox_rows(&self) -> i64 {
        self.db
            .read(|conn| conn.query_row("SELECT COUNT(*) FROM outbox", [], |row| row.get(0)))
            .await
            .unwrap()
    }

    async fn stop(self) {
        let _ = self.shutdown.send(());
        let _ = self.handle.await;
        let _ = std::fs::remove_file(&self.socket);
        for suffix in ["", "-wal", "-shm"] {
            let _ =
                std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.db_path.display())));
        }
    }
}

/// Drain a `DraftReply` stream into its frames.
async fn drain(
    mut stream: impl Stream<Item = Result<DraftReplyEvent, tonic::Status>> + Unpin,
) -> Vec<Result<DraftReplyEvent, tonic::Status>> {
    let mut out = Vec::new();
    while let Some(frame) = stream.next().await {
        out.push(frame);
    }
    out
}

fn events(frames: &[Result<DraftReplyEvent, tonic::Status>]) -> Vec<draft_reply_event::Event> {
    frames
        .iter()
        .filter_map(|frame| frame.as_ref().ok()?.event.clone())
        .collect()
}

fn body(events: &[draft_reply_event::Event]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            draft_reply_event::Event::Token(token) => Some(token.as_str()),
            _ => None,
        })
        .collect()
}

fn staged(events: &[draft_reply_event::Event]) -> Draft {
    events
        .iter()
        .find_map(|event| match event {
            draft_reply_event::Event::Draft(draft) => Some(draft.clone()),
            _ => None,
        })
        .expect("a Draft frame")
}

// ---------------------------------------------------------------------------
// DraftReply
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_streamed_reply_arrives_as_typed_frames_and_stages_an_editable_draft() {
    let server = TestServer::start().await;
    let message_id = server.incoming().await;
    server
        .provider
        .queue("Confirmed: Q3 revenue was 4.1M, final.");

    let stream = server
        .client()
        .await
        .draft_reply(DraftReplyRequest {
            message_id,
            intent: "confirm the figure".to_owned(),
            reply_all: false,
        })
        .await
        .unwrap()
        .into_inner();
    let frames = drain(stream).await;
    let events = events(&frames);

    // The contractual order: context, tokens, draft, usage, done. `draft`
    // before `done` is what lets a client that saw `done` know the draft is
    // durable.
    let names: Vec<&str> = events
        .iter()
        .map(|event| match event {
            draft_reply_event::Event::Context(_) => "context",
            draft_reply_event::Event::Token(_) => "token",
            draft_reply_event::Event::Draft(_) => "draft",
            draft_reply_event::Event::Usage(_) => "usage",
            draft_reply_event::Event::Done(_) => "done",
        })
        .collect();
    assert_eq!(names.first(), Some(&"context"), "{names:?}");
    assert_eq!(names.last(), Some(&"done"), "{names:?}");
    let draft_at = names.iter().position(|n| *n == "draft").unwrap();
    let done_at = names.iter().position(|n| *n == "done").unwrap();
    let last_token = names.iter().rposition(|n| *n == "token").unwrap();
    assert!(last_token < draft_at && draft_at < done_at, "{names:?}");
    assert!(
        names.iter().filter(|n| **n == "token").count() > 1,
        "the relay should pass the provider's tokens through, not buffer them into one"
    );

    let streamed = body(&events);
    assert_eq!(streamed, "Confirmed: Q3 revenue was 4.1M, final.");

    // The headers are derived from the parent, not generated by the model.
    let draft = staged(&events);
    assert_eq!(draft.subject, "Re: Quarterly numbers");
    assert_eq!(draft.in_reply_to.as_deref(), Some("parent@example.net"));
    assert_eq!(draft.references, vec!["parent@example.net".to_owned()]);
    assert_eq!(draft.in_reply_to_message_id, Some(message_id));
    assert_eq!(
        draft
            .to
            .iter()
            .map(|a| a.address.clone())
            .collect::<Vec<_>>(),
        vec!["bob@example.net".to_owned()]
    );
    assert!(
        draft.cc.is_empty(),
        "a plain reply addresses only the author"
    );
    assert_eq!(draft.from.unwrap().address, "alice@example.com");
    assert!(
        draft.body_text.starts_with(&streamed),
        "the draft holds the body the client watched arrive: {:?}",
        draft.body_text
    );

    // The property the whole task rests on.
    assert_eq!(
        server.outbox_rows().await,
        0,
        "drafting a reply must never enqueue anything for submission"
    );

    // And it is a real, fetchable draft rather than a frame that vanished.
    let fetched = server
        .client()
        .await
        .get_draft(rmail_proto::v1::GetDraftRequest { draft_id: draft.id })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(fetched.body_text, draft.body_text);
    assert_eq!(server.outbox_rows().await, 0);
    server.stop().await;
}

#[tokio::test]
async fn reply_all_addresses_the_thread_and_never_the_user() {
    let server = TestServer::start().await;
    let message_id = server.incoming().await;
    server.provider.queue("We are go.");

    let stream = server
        .client()
        .await
        .draft_reply(DraftReplyRequest {
            message_id,
            intent: String::new(),
            reply_all: true,
        })
        .await
        .unwrap()
        .into_inner();
    let draft = staged(&events(&drain(stream).await));

    assert_eq!(
        draft
            .to
            .iter()
            .map(|a| a.address.clone())
            .collect::<Vec<_>>(),
        vec!["bob@example.net".to_owned()]
    );
    assert_eq!(
        draft
            .cc
            .iter()
            .map(|a| a.address.clone())
            .collect::<Vec<_>>(),
        vec!["carol@example.org".to_owned()],
        "reply-all keeps the other recipients and drops the user themselves"
    );
    server.stop().await;
}

#[tokio::test]
async fn a_reply_to_a_message_that_does_not_exist_is_not_found() {
    let server = TestServer::start().await;
    let status = server
        .client()
        .await
        .draft_reply(DraftReplyRequest {
            message_id: 9_999,
            intent: String::new(),
            reply_all: false,
        })
        .await
        .expect_err("no such message");
    assert_eq!(status.code(), Code::NotFound);
    assert_eq!(
        server.provider.calls(),
        0,
        "nothing decidable locally should reach the provider"
    );
    server.stop().await;
}

#[tokio::test]
async fn an_unreachable_provider_fails_the_rpc_and_stages_nothing() {
    let server = TestServer::start().await;
    let message_id = server.incoming().await;
    // Nothing queued: the mock behaves like a provider that cannot be reached.
    let stream = server
        .client()
        .await
        .draft_reply(DraftReplyRequest {
            message_id,
            intent: String::new(),
            reply_all: false,
        })
        .await
        .unwrap()
        .into_inner();
    let frames = drain(stream).await;
    assert!(
        frames.iter().any(std::result::Result::is_err),
        "a provider failure must surface, never end the stream as if it succeeded"
    );
    assert!(
        !events(&frames)
            .iter()
            .any(|e| matches!(e, draft_reply_event::Event::Draft(_))),
        "half a reply must not be staged as if it were whole"
    );

    let listed = server
        .client()
        .await
        .list_drafts(rmail_proto::v1::ListDraftsRequest {
            account_id: server.account_id,
            page_size: 0,
            page_token: String::new(),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(listed.drafts.is_empty());
    assert_eq!(server.outbox_rows().await, 0);
    server.stop().await;
}

// ---------------------------------------------------------------------------
// RewriteDraft and revisions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_rewrite_produces_a_revision_a_client_can_cycle_and_revert() {
    let server = TestServer::start().await;
    let draft = server.draft("hey can u send the numbers").await;
    let mut client = server.client().await;

    server
        .provider
        .queue("Dear Bob, could you please send the figures? Thank you.");
    let first = client
        .rewrite_draft(RewriteDraftRequest {
            draft_id: draft.id,
            tone: RewriteTone::Formal as i32,
            length: RewriteLength::AsIs as i32,
            instruction: String::new(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(first.seq, 1);
    assert_eq!(first.label, "formal");
    assert!(first.active);
    assert!(first.model.is_some());

    server.provider.queue("Bob — the figures, please.");
    let second = client
        .rewrite_draft(RewriteDraftRequest {
            draft_id: draft.id,
            tone: RewriteTone::Unspecified as i32,
            length: RewriteLength::Shorter as i32,
            instruction: String::new(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(second.seq, 2);
    assert_eq!(
        second.label, "shorter",
        "an unset tone is a length-only rewrite"
    );

    let revisions = client
        .list_draft_revisions(ListDraftRevisionsRequest { draft_id: draft.id })
        .await
        .unwrap()
        .into_inner()
        .revisions;
    assert_eq!(
        revisions
            .iter()
            .map(|r| r.label.clone())
            .collect::<Vec<_>>(),
        vec![
            "original".to_owned(),
            "formal".to_owned(),
            "shorter".to_owned()
        ],
        "the pre-rewrite text is captured as revision 0"
    );
    assert!(revisions[0].model.is_none(), "no model wrote the original");
    assert_eq!(revisions.iter().filter(|r| r.active).count(), 1);

    // Revert, then cycle forward again — one RPC for both.
    let reverted = client
        .select_draft_revision(SelectDraftRevisionRequest {
            draft_id: draft.id,
            seq: 0,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(reverted.body_text, "hey can u send the numbers");
    let cycled = client
        .select_draft_revision(SelectDraftRevisionRequest {
            draft_id: draft.id,
            seq: 1,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        cycled.body_text,
        "Dear Bob, could you please send the figures? Thank you."
    );

    assert_eq!(server.outbox_rows().await, 0, "a rewrite sends nothing");
    server.stop().await;
}

#[tokio::test]
async fn a_rewrite_that_asks_for_nothing_is_invalid_argument() {
    let server = TestServer::start().await;
    let draft = server.draft("some text").await;
    let status = server
        .client()
        .await
        .rewrite_draft(RewriteDraftRequest {
            draft_id: draft.id,
            tone: RewriteTone::AsIs as i32,
            length: RewriteLength::AsIs as i32,
            instruction: "   ".to_owned(),
        })
        .await
        .expect_err("nothing was asked for");
    assert_eq!(status.code(), Code::InvalidArgument);
    assert_eq!(server.provider.calls(), 0);
    server.stop().await;
}

#[tokio::test]
async fn an_unknown_tone_is_rejected_rather_than_silently_ignored() {
    let server = TestServer::start().await;
    let draft = server.draft("some text").await;
    let status = server
        .client()
        .await
        .rewrite_draft(RewriteDraftRequest {
            draft_id: draft.id,
            // A value no `RewriteTone` variant has: a newer client, or a bug.
            tone: 99,
            length: RewriteLength::AsIs as i32,
            instruction: String::new(),
        })
        .await
        .expect_err("unknown tone");
    assert_eq!(status.code(), Code::InvalidArgument);
    assert_eq!(server.provider.calls(), 0);
    server.stop().await;
}

#[tokio::test]
async fn revisions_of_a_missing_draft_are_not_found() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    assert_eq!(
        client
            .list_draft_revisions(ListDraftRevisionsRequest { draft_id: 4_242 })
            .await
            .expect_err("no such draft")
            .code(),
        Code::NotFound
    );
    assert_eq!(
        client
            .select_draft_revision(SelectDraftRevisionRequest {
                draft_id: 4_242,
                seq: 0,
            })
            .await
            .expect_err("no such draft")
            .code(),
        Code::NotFound
    );

    // An existing draft nobody has rewritten has no revisions — an empty list,
    // not an error.
    let draft = server.draft("untouched").await;
    assert!(client
        .list_draft_revisions(ListDraftRevisionsRequest { draft_id: draft.id })
        .await
        .unwrap()
        .into_inner()
        .revisions
        .is_empty());
    assert_eq!(
        client
            .select_draft_revision(SelectDraftRevisionRequest {
                draft_id: draft.id,
                seq: -1,
            })
            .await
            .expect_err("a negative sequence is nonsense")
            .code(),
        Code::InvalidArgument
    );
    server.stop().await;
}

// ---------------------------------------------------------------------------
// A daemon with no AI subsystem
// ---------------------------------------------------------------------------

#[tokio::test]
async fn without_a_provider_the_model_rpcs_refuse_and_the_rest_keep_working() {
    let server = TestServer::start_with(false).await;
    let message_id = server.incoming().await;
    let mut client = server.client().await;

    assert_eq!(
        client
            .draft_reply(DraftReplyRequest {
                message_id,
                intent: String::new(),
                reply_all: false,
            })
            .await
            .expect_err("no provider")
            .code(),
        Code::FailedPrecondition,
        "a daemon with no AI subsystem must say so rather than pretend to draft"
    );

    let draft = server.draft("some text").await;
    assert_eq!(
        client
            .rewrite_draft(RewriteDraftRequest {
                draft_id: draft.id,
                tone: RewriteTone::Formal as i32,
                length: RewriteLength::AsIs as i32,
                instruction: String::new(),
            })
            .await
            .expect_err("no provider")
            .code(),
        Code::FailedPrecondition
    );

    // The revision RPCs call no model, so turning AI off must not strand a
    // draft that was already rewritten while it was on.
    assert!(client
        .list_draft_revisions(ListDraftRevisionsRequest { draft_id: draft.id })
        .await
        .unwrap()
        .into_inner()
        .revisions
        .is_empty());
    server.stop().await;
}
