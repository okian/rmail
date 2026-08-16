//! Integration test: the pre-send guardian and the waiting-on tracker
//! (task 63) driven end-to-end through `SendSchedulerService` over an
//! in-process tonic server on a Unix domain socket, backed by a real
//! `rmail_core` store over a real (temp-file) database.
//!
//! The judgement logic itself is covered where it lives — `rmail-core`'s
//! `send::preflight` and `outbox::followup::track` tests. What this file owes
//! is the *boundary*, and specifically the three things that are only true if
//! the whole path is assembled:
//!
//! * a message the guardian blocks is refused with `FAILED_PRECONDITION` and
//!   never reaches the outbox, while the same message with `skip_preflight`
//!   does;
//! * a daemon whose model is unreachable still sends, and says over the wire
//!   that the review did not happen;
//! * a daemon with no provider at all answers `FAILED_PRECONDITION` to the
//!   explicit RPCs rather than pretending to have checked something.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use rmail_core::ai::provider::{
    ChatRequest, ChatResponse, Provider, ProviderStream, StopReason, Usage,
};
use rmail_core::ai::queue::RateLimiter;
use rmail_core::ai::PolicyEngine;
use rmail_core::config::{Config, SendConfig};
use rmail_core::outbox::followup::track::FollowupTracker;
use rmail_core::outbox::{FollowupStore, OutboxStore};
use rmail_core::send::preflight::PreflightGuardian;
use rmail_core::{repo, Database, Error};
use rmail_proto::v1::send_scheduler_service_client::SendSchedulerServiceClient;
use rmail_proto::v1::{
    CreateDraftRequest, CreateFollowupRequest, DraftAddress, DraftNudgeRequest, IdRequest,
    ListOutboxRequest, ListWaitingOnRequest, NewDraftAttachment, OutboxState,
    PreflightCheckRequest, PreflightDegradation, PreflightFindingKind, PreflightSeverity,
    ScheduleSendRequest, SendOrigin, TrackFollowupRequest,
};
use tokio::sync::oneshot;
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::UnixListenerStream;
use tokio_util::sync::CancellationToken;
use tonic::transport::{Channel, Server};
use tonic::Code;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// A provider that answers from a script and fails once the script runs out —
/// which is exactly how an unreachable one behaves.
#[derive(Debug, Default)]
struct MockProvider {
    replies: Mutex<Vec<String>>,
    calls: AtomicUsize,
}

impl MockProvider {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn queue(&self, body: serde_json::Value) {
        self.replies
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(body.to_string());
    }

    fn queue_clean_review(&self) {
        self.queue(serde_json::json!({"findings": []}));
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Provider for MockProvider {
    async fn complete(
        &self,
        request: &ChatRequest,
        _cancel: &CancellationToken,
    ) -> Result<ChatResponse, Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let next = {
            let mut replies = self.replies.lock().unwrap_or_else(PoisonError::into_inner);
            if replies.is_empty() {
                None
            } else {
                Some(replies.remove(0))
            }
        };
        match next {
            Some(text) => Ok(ChatResponse {
                id: "msg_mock".to_owned(),
                model: request.model.clone(),
                stop_reason: StopReason::EndTurn,
                text,
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
        Err(Error::internal("mock provider: stream is not scripted"))
    }
}

struct TestServer {
    socket: PathBuf,
    db_path: PathBuf,
    db: Database,
    account_id: i64,
    provider: Arc<MockProvider>,
    shutdown: oneshot::Sender<()>,
    handle: JoinHandle<()>,
}

impl TestServer {
    async fn start() -> Self {
        Self::start_with(true).await
    }

    /// `with_ai = false` stands in for a daemon whose AI subsystem never came
    /// up — the wiring in `rmaild::serve` leaves the guardian and the tracker
    /// unset there, and the RPCs have to behave.
    async fn start_with(with_ai: bool) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let socket = PathBuf::from("/tmp").join(format!("rmail-followup-{pid}-{n}.sock"));
        let db_path = std::env::temp_dir().join(format!("rmail-followup-svc-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", db_path.display())));
        }
        let _ = std::fs::remove_file(&socket);

        let db = Database::open(&db_path).unwrap();
        let account_id = db
            .write(|c| {
                repo::insert_account(
                    c,
                    &repo::NewAccount {
                        name: "Personal".to_owned(),
                        username: Some("alice@example.com".to_owned()),
                        smtp_server: Some("127.0.0.1".to_owned()),
                        smtp_port: Some(2525),
                        ..Default::default()
                    },
                )
            })
            .await
            .unwrap();

        // No scheduler loop: this file tests the RPC surface, and a background
        // sender would race every assertion and dial a port nothing serves.
        let provider = MockProvider::new();
        let cancel = CancellationToken::new();
        let config = SendConfig::default();
        let mut api = rmaild::SendSchedulerApi::new(
            OutboxStore::new(db.clone()),
            FollowupStore::new(db.clone()),
            db.clone(),
            config.clone(),
            rmail_core::idempotency::IdempotencyStore::new(
                db.clone(),
                Duration::from_secs(3600),
                Duration::from_secs(300),
            ),
            cancel.clone(),
        );
        if with_ai {
            let policy = Arc::new(PolicyEngine::from_config(&Config::default()).unwrap());
            let semaphore = Arc::new(Semaphore::new(4));
            // Not `0` — see `RateLimiter`: zero means one free token and then
            // an effectively infinite wait, so a second call would hang.
            let limiter = Arc::new(RateLimiter::new(1_000_000));
            api = api
                .with_guardian(PreflightGuardian::new(
                    db.clone(),
                    Arc::clone(&provider) as Arc<dyn Provider>,
                    Arc::clone(&policy),
                    Config::default().ai.privacy.clone(),
                    Config::default().ai.limits.clone(),
                    config.preflight.clone(),
                    Arc::clone(&semaphore),
                    Arc::clone(&limiter),
                ))
                .with_tracker(FollowupTracker::new(
                    db.clone(),
                    FollowupStore::new(db.clone()),
                    Arc::clone(&provider) as Arc<dyn Provider>,
                    policy,
                    Config::default().ai.privacy.clone(),
                    Config::default().ai.limits.clone(),
                    config.followup.clone(),
                    semaphore,
                    limiter,
                ));
        }

        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let incoming = UnixListenerStream::new(listener);
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            let _ = Server::builder()
                .add_service(
                    rmail_proto::v1::send_scheduler_service_server::SendSchedulerServiceServer::new(
                        api,
                    ),
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
            provider,
            shutdown: shutdown_tx,
            handle,
        }
    }

    async fn client(&self) -> SendSchedulerServiceClient<Channel> {
        SendSchedulerServiceClient::new(rmail_core::connect_uds(&self.socket).await.unwrap())
    }

    fn send_request(&self, body: &str) -> ScheduleSendRequest {
        ScheduleSendRequest {
            account_id: self.account_id,
            draft_id: None,
            to: vec!["bob@example.net".to_owned()],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: Some("Lunch".to_owned()),
            body: Some(body.to_owned()),
            in_reply_to: None,
            send_at: Some(chrono::Utc::now().timestamp() + 3_600),
            send_at_nl: None,
            optimal: None,
            tz: String::new(),
            undo_window_secs: None,
            origin: SendOrigin::User as i32,
            skip_preflight: false,
            idempotency_key: String::new(),
        }
    }

    fn check_request(&self, body: &str) -> PreflightCheckRequest {
        PreflightCheckRequest {
            account_id: self.account_id,
            draft_id: None,
            to: vec!["bob@example.net".to_owned()],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: Some("Lunch".to_owned()),
            body: Some(body.to_owned()),
            attachment_names: Vec::new(),
            in_reply_to: None,
        }
    }

    /// Create a draft through the same `DraftStore` the handler reads from.
    async fn draft(&self, body: &str, attachment: bool) -> i64 {
        let api = rmaild::ComposeApi::new(
            rmail_core::compose::DraftStore::new(self.db.clone()),
            rmail_core::idempotency::IdempotencyStore::new(
                self.db.clone(),
                Duration::from_secs(3600),
                Duration::from_secs(60),
            ),
        );
        use rmail_proto::v1::compose_service_server::ComposeService as _;
        api.create_draft(tonic::Request::new(CreateDraftRequest {
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
            subject: "From a draft".to_owned(),
            body_text: body.to_owned(),
            body_html: None,
            attachments: if attachment {
                vec![NewDraftAttachment {
                    filename: "numbers.xlsx".to_owned(),
                    content_type: "text/plain".to_owned(),
                    content: b"attached".to_vec(),
                }]
            } else {
                Vec::new()
            },
            in_reply_to_message_id: None,
        }))
        .await
        .unwrap()
        .into_inner()
        .id
    }

    async fn outbox_len(&self) -> usize {
        self.client()
            .await
            .list_outbox(ListOutboxRequest {
                account_id: Some(self.account_id),
                state: OutboxState::Unspecified as i32,
                page_size: 100,
                page_token: String::new(),
            })
            .await
            .unwrap()
            .into_inner()
            .entries
            .len()
    }

    async fn shutdown(self) {
        let _ = self.shutdown.send(());
        let _ = self.handle.await;
        let _ = std::fs::remove_file(&self.socket);
        for suffix in ["", "-wal", "-shm"] {
            let _ =
                std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.db_path.display())));
        }
    }
}

/// A body the deterministic layer blocks on, and which no model has to agree
/// with: an unfilled mail-merge placeholder.
const TEMPLATED: &str = "Dear {{first_name}}, thanks for your interest.";

// ---------------------------------------------------------------------------
// PreflightCheck
// ---------------------------------------------------------------------------

#[tokio::test]
async fn preflight_reports_findings_and_says_whether_they_block() {
    let server = TestServer::start().await;
    server.provider.queue_clean_review();

    let response = server
        .client()
        .await
        .preflight_check(server.check_request(TEMPLATED))
        .await
        .unwrap()
        .into_inner();

    assert_eq!(response.findings.len(), 1, "{:?}", response.findings);
    assert_eq!(
        response.findings[0].kind,
        PreflightFindingKind::UnfilledPlaceholder as i32
    );
    assert_eq!(
        response.findings[0].severity,
        PreflightSeverity::Block as i32
    );
    assert!(!response.findings[0].from_model);
    assert_eq!(response.severity, PreflightSeverity::Block as i32);
    assert!(response.blocks);
    assert_eq!(
        response.degradation,
        PreflightDegradation::Unspecified as i32,
        "the full check ran; nothing should be reported as degraded"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn preflight_reports_a_tone_finding_from_the_model_that_does_not_block() {
    let server = TestServer::start().await;
    server.provider.queue(serde_json::json!({"findings": [
        {"kind": "tone_clash", "severity": "warn", "detail": "the closing line reads as sarcastic"}
    ]}));

    let response = server
        .client()
        .await
        .preflight_check(server.check_request("Fine, whatever you say."))
        .await
        .unwrap()
        .into_inner();

    assert_eq!(response.findings.len(), 1);
    assert_eq!(
        response.findings[0].kind,
        PreflightFindingKind::ToneClash as i32
    );
    assert!(response.findings[0].from_model);
    assert_eq!(
        response.findings[0].severity,
        PreflightSeverity::Warn as i32
    );
    assert!(!response.blocks, "a model finding must never refuse a send");
    server.shutdown().await;
}

#[tokio::test]
async fn preflight_says_over_the_wire_when_the_model_was_unavailable() {
    let server = TestServer::start().await;
    // Nothing queued: the provider fails the way an offline one does.
    let response = server
        .client()
        .await
        .preflight_check(server.check_request(TEMPLATED))
        .await
        .unwrap()
        .into_inner();

    assert_eq!(
        response.degradation,
        PreflightDegradation::Unavailable as i32,
        "a caller must be able to tell a partial review from a whole one"
    );
    assert!(response
        .degradation_detail
        .as_deref()
        .is_some_and(|d| !d.is_empty()));
    // The offline half still ran, and still blocks.
    assert!(response.blocks);
    server.shutdown().await;
}

#[tokio::test]
async fn preflight_flags_a_recipient_the_thread_has_not_involved() {
    let server = TestServer::start().await;
    server.provider.queue_clean_review();
    // A parent message this account already synced, with its own participants.
    let account_id = server.account_id;
    server
        .db
        .write(move |c| {
            let mailbox_id = repo::insert_mailbox(
                c,
                &repo::NewMailbox {
                    account_id,
                    name: "INBOX".to_owned(),
                    ..Default::default()
                },
            )?;
            c.execute(
                "INSERT INTO messages
                     (account_id, mailbox_id, uid, uidvalidity, message_id, from_addr, to_addrs)
                 VALUES (?1, ?2, 1, 1, 'parent@example.com', 'bob@example.net',
                         'alice@example.com')",
                rusqlite::params![account_id, mailbox_id],
            )
        })
        .await
        .unwrap();

    let mut request = server.check_request("Sounds good.");
    request.in_reply_to = Some("<parent@example.com>".to_owned());
    request.cc = vec!["legal@rival.example".to_owned()];
    let response = server
        .client()
        .await
        .preflight_check(request)
        .await
        .unwrap()
        .into_inner();

    assert_eq!(
        response.findings.iter().map(|f| f.kind).collect::<Vec<_>>(),
        [PreflightFindingKind::RecipientNotOnThread as i32]
    );
    assert!(response.findings[0].detail.contains("legal@rival.example"));
    assert!(
        !response.blocks,
        "an extra recipient warns, it does not block"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn preflight_over_a_stored_draft_sees_its_attachments() {
    // The `draft_id` path is the one the CLI's `mail draft check` will use,
    // and the whole point of reading the draft is that its attachment list
    // decides whether "see attached" is a finding.
    let server = TestServer::start().await;
    let with_file = server.draft("See attached for the numbers.", true).await;
    let without = server.draft("See attached for the numbers.", false).await;

    server.provider.queue_clean_review();
    let clean = server
        .client()
        .await
        .preflight_check(PreflightCheckRequest {
            account_id: server.account_id,
            draft_id: Some(with_file),
            ..server.check_request("")
        })
        .await
        .unwrap()
        .into_inner();
    assert!(
        clean.findings.is_empty(),
        "a draft that carries the file promises nothing it lacks: {:?}",
        clean.findings
    );

    server.provider.queue_clean_review();
    let flagged = server
        .client()
        .await
        .preflight_check(PreflightCheckRequest {
            account_id: server.account_id,
            draft_id: Some(without),
            ..server.check_request("")
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        flagged.findings.iter().map(|f| f.kind).collect::<Vec<_>>(),
        [PreflightFindingKind::MissingAttachment as i32]
    );
    server.shutdown().await;
}

#[tokio::test]
async fn an_inline_check_takes_the_callers_attachment_names() {
    // An inline check renders nothing, so the caller's stated filenames are
    // the only evidence about attachments there is.
    let server = TestServer::start().await;
    server.provider.queue_clean_review();
    let mut request = server.check_request("See attached for the numbers.");
    request.attachment_names = vec!["numbers.xlsx".to_owned()];
    let response = server
        .client()
        .await
        .preflight_check(request)
        .await
        .unwrap()
        .into_inner();
    assert!(
        response.findings.is_empty(),
        "attachment_names was ignored: {:?}",
        response.findings
    );
    server.shutdown().await;
}

#[tokio::test]
async fn a_preflight_over_another_accounts_draft_is_not_found() {
    let server = TestServer::start().await;
    let draft_id = server.draft("Hello.", false).await;
    let status = server
        .client()
        .await
        .preflight_check(PreflightCheckRequest {
            account_id: server.account_id + 1,
            draft_id: Some(draft_id),
            ..server.check_request("")
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::NotFound);
    server.shutdown().await;
}

#[tokio::test]
async fn preflight_is_failed_precondition_without_a_provider() {
    let server = TestServer::start_with(false).await;
    let status = server
        .client()
        .await
        .preflight_check(server.check_request(TEMPLATED))
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::FailedPrecondition);
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// The automatic check on ScheduleSend
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_blocked_message_is_refused_and_never_reaches_the_outbox() {
    let server = TestServer::start().await;
    server.provider.queue_clean_review();

    let status = server
        .client()
        .await
        .schedule_send(server.send_request(TEMPLATED))
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::FailedPrecondition);
    assert!(
        status.message().contains("unfilled_placeholder"),
        "the refusal must name what to fix: {}",
        status.message()
    );
    assert_eq!(
        server.outbox_len().await,
        0,
        "a refused send must not be queued"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn skip_preflight_sends_the_same_message_and_calls_no_model() {
    let server = TestServer::start().await;
    let mut request = server.send_request(TEMPLATED);
    request.skip_preflight = true;

    let entry = server
        .client()
        .await
        .schedule_send(request)
        .await
        .unwrap()
        .into_inner();
    assert_eq!(entry.state, OutboxState::Scheduled as i32);
    assert_eq!(server.outbox_len().await, 1);
    assert_eq!(
        server.provider.calls(),
        0,
        "an override must not spend a model call it is about to ignore"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn an_unreachable_model_never_stops_a_send() {
    let server = TestServer::start().await;
    // Nothing queued: every model call fails. An ordinary message must still
    // go out — the guardian degrades, it does not swallow.
    let entry = server
        .client()
        .await
        .schedule_send(server.send_request("Shall we say noon?"))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(entry.state, OutboxState::Scheduled as i32);
    assert!(server.provider.calls() >= 1, "the review was attempted");
    server.shutdown().await;
}

#[tokio::test]
async fn a_daemon_without_a_provider_still_sends() {
    let server = TestServer::start_with(false).await;
    let entry = server
        .client()
        .await
        .schedule_send(server.send_request(TEMPLATED))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        entry.state,
        OutboxState::Scheduled as i32,
        "an un-reviewable message is not an un-sendable one"
    );
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// The waiting-on tracker
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tracking_a_sent_message_arms_a_waiting_on_entry() {
    let server = TestServer::start().await;
    let sent_at = chrono::Utc::now().timestamp() - 3 * 86_400;
    server.provider.queue(serde_json::json!({
        "expects_reply": true,
        "ask": "confirm the Q3 numbers",
        "due_in_days": 2,
    }));

    let mut client = server.client().await;
    let tracked = client
        .track_followup(TrackFollowupRequest {
            account_id: server.account_id,
            message_id: "sent-1@example.com".to_owned(),
            subject: "Q3 numbers".to_owned(),
            body: "Could you confirm the Q3 numbers before Thursday?".to_owned(),
            recipients: vec!["bob@example.net".to_owned()],
            sent_at: Some(sent_at),
            thread_id: None,
            tz: "UTC".to_owned(),
        })
        .await
        .unwrap()
        .into_inner();

    assert!(tracked.expects_reply);
    assert_eq!(tracked.ask, "confirm the Q3 numbers");
    let followup = tracked.followup.expect("a reminder was armed");
    assert_eq!(followup.ask.as_deref(), Some("confirm the Q3 numbers"));
    assert_eq!(followup.waiting_on, ["bob@example.net"]);
    assert_eq!(followup.remind_at, sent_at + 2 * 86_400);

    // And it shows up on the aging list, with an age and an overdue flag.
    let waiting = client
        .list_waiting_on(ListWaitingOnRequest {
            account_id: Some(server.account_id),
            overdue_only: false,
            page_size: 50,
            page_token: String::new(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(waiting.followups.len(), 1);
    assert!(waiting.followups[0].age_secs >= 3 * 86_400);
    assert!(
        waiting.followups[0].overdue,
        "a reminder due a day ago is overdue"
    );
    assert_eq!(waiting.followups[0].subject.as_deref(), Some("Q3 numbers"));
    server.shutdown().await;
}

#[tokio::test]
async fn a_message_expecting_no_reply_arms_nothing_and_is_not_an_error() {
    let server = TestServer::start().await;
    server.provider.queue(serde_json::json!({
        "expects_reply": false,
        "ask": "",
        "due_in_days": 0,
    }));

    let mut client = server.client().await;
    let tracked = client
        .track_followup(TrackFollowupRequest {
            account_id: server.account_id,
            message_id: "sent-2@example.com".to_owned(),
            subject: "Thanks".to_owned(),
            body: "Thanks, that's perfect.".to_owned(),
            recipients: vec!["bob@example.net".to_owned()],
            sent_at: None,
            thread_id: None,
            tz: String::new(),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(!tracked.expects_reply);
    assert!(tracked.followup.is_none());

    let waiting = client
        .list_waiting_on(ListWaitingOnRequest {
            account_id: Some(server.account_id),
            overdue_only: false,
            page_size: 50,
            page_token: String::new(),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(waiting.followups.is_empty());
    server.shutdown().await;
}

#[tokio::test]
async fn a_judge_that_cannot_answer_arms_nothing_and_says_so() {
    let server = TestServer::start().await;
    // Nothing queued: the provider fails.
    let status = server
        .client()
        .await
        .track_followup(TrackFollowupRequest {
            account_id: server.account_id,
            message_id: "sent-3@example.com".to_owned(),
            subject: "Q3 numbers".to_owned(),
            body: "Could you confirm?".to_owned(),
            recipients: vec!["bob@example.net".to_owned()],
            sent_at: None,
            thread_id: None,
            tz: String::new(),
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::Unavailable);

    let waiting = server
        .client()
        .await
        .list_waiting_on(ListWaitingOnRequest {
            account_id: Some(server.account_id),
            overdue_only: false,
            page_size: 50,
            page_token: String::new(),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(
        waiting.followups.is_empty(),
        "a failed judgement must not fall back to a default reminder"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn tracking_needs_a_message_id() {
    let server = TestServer::start().await;
    let status = server
        .client()
        .await
        .track_followup(TrackFollowupRequest {
            account_id: server.account_id,
            message_id: "   ".to_owned(),
            subject: String::new(),
            body: String::new(),
            recipients: Vec::new(),
            sent_at: None,
            thread_id: None,
            tz: String::new(),
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);
    assert_eq!(
        server.provider.calls(),
        0,
        "the argument check must run before anything is sent to a model"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn an_answered_thread_drops_off_the_waiting_on_list() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    client
        .create_followup(CreateFollowupRequest {
            account_id: server.account_id,
            message_id: "asked@example.com".to_owned(),
            thread_id: None,
            remind_at: Some(chrono::Utc::now().timestamp() + 86_400),
            remind_in: None,
            tz: "UTC".to_owned(),
            note: Some("chase the quote".to_owned()),
            cancel_on_reply: Some(true),
        })
        .await
        .unwrap();

    let request = ListWaitingOnRequest {
        account_id: Some(server.account_id),
        overdue_only: false,
        page_size: 50,
        page_token: String::new(),
    };
    assert_eq!(
        client
            .list_waiting_on(request.clone())
            .await
            .unwrap()
            .into_inner()
            .followups
            .len(),
        1
    );

    // The reply syncs in. Nothing dismisses the reminder — the scheduler's
    // sweep does that at fire time — but the aging list must stop showing it.
    let account_id = server.account_id;
    server
        .db
        .write(move |c| {
            let mailbox_id = repo::insert_mailbox(
                c,
                &repo::NewMailbox {
                    account_id,
                    name: "INBOX".to_owned(),
                    ..Default::default()
                },
            )?;
            c.execute(
                "INSERT INTO messages
                     (account_id, mailbox_id, uid, uidvalidity, message_id, in_reply_to)
                 VALUES (?1, ?2, 7, 1, 'reply@example.com', 'asked@example.com')",
                rusqlite::params![account_id, mailbox_id],
            )
        })
        .await
        .unwrap();

    assert!(client
        .list_waiting_on(request)
        .await
        .unwrap()
        .into_inner()
        .followups
        .is_empty());
    server.shutdown().await;
}

#[tokio::test]
async fn a_waiting_on_token_from_the_wrong_listing_is_refused() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    for n in 0..3 {
        client
            .create_followup(CreateFollowupRequest {
                account_id: server.account_id,
                message_id: format!("m{n}@example.com"),
                thread_id: None,
                remind_at: Some(chrono::Utc::now().timestamp() + 86_400),
                remind_in: None,
                tz: "UTC".to_owned(),
                note: None,
                cancel_on_reply: Some(true),
            })
            .await
            .unwrap();
    }
    let followups = client
        .list_followups(rmail_proto::v1::ListFollowupsRequest {
            account_id: Some(server.account_id),
            state: rmail_proto::v1::FollowupState::Unspecified as i32,
            page_size: 1,
            page_token: String::new(),
        })
        .await
        .unwrap()
        .into_inner();
    let foreign = followups.next_page_token;
    assert!(!foreign.is_empty());

    let status = client
        .list_waiting_on(ListWaitingOnRequest {
            account_id: Some(server.account_id),
            overdue_only: false,
            page_size: 1,
            page_token: foreign,
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);
    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// DraftNudge
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_nudge_is_drafted_and_nothing_is_queued() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let followup = client
        .create_followup(CreateFollowupRequest {
            account_id: server.account_id,
            message_id: "asked@example.com".to_owned(),
            thread_id: None,
            remind_at: Some(chrono::Utc::now().timestamp() + 86_400),
            remind_in: None,
            tz: "UTC".to_owned(),
            note: None,
            cancel_on_reply: Some(true),
        })
        .await
        .unwrap()
        .into_inner();

    server.provider.queue(serde_json::json!({
        "subject": "Re: Q3 numbers",
        "body": "Floating this back up — no rush if it has moved down the list.",
    }));
    let nudge = client
        .draft_nudge(DraftNudgeRequest { id: followup.id })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(nudge.subject, "Re: Q3 numbers");
    assert!(nudge.body.contains("Floating this back up"));
    assert!(!nudge.model.is_empty());
    assert_eq!(
        server.outbox_len().await,
        0,
        "drafting a nudge must not queue one"
    );
    server.shutdown().await;
}

#[tokio::test]
async fn drafting_a_nudge_for_an_unknown_followup_is_not_found() {
    let server = TestServer::start().await;
    let status = server
        .client()
        .await
        .draft_nudge(DraftNudgeRequest { id: 9_999 })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::NotFound);
    server.shutdown().await;
}

#[tokio::test]
async fn draft_nudge_is_failed_precondition_without_a_provider() {
    let server = TestServer::start_with(false).await;
    let mut client = server.client().await;
    let followup = client
        .create_followup(CreateFollowupRequest {
            account_id: server.account_id,
            message_id: "asked@example.com".to_owned(),
            thread_id: None,
            remind_at: Some(chrono::Utc::now().timestamp() + 86_400),
            remind_in: None,
            tz: "UTC".to_owned(),
            note: None,
            cancel_on_reply: Some(true),
        })
        .await
        .unwrap()
        .into_inner();
    let status = client
        .draft_nudge(DraftNudgeRequest { id: followup.id })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::FailedPrecondition);
    // The dismiss path still works with no provider — a reminder is local.
    assert!(client
        .dismiss_followup(IdRequest { id: followup.id })
        .await
        .is_ok());
    server.shutdown().await;
}
