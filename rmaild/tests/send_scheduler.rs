//! Integration test: drive `SendSchedulerService` end-to-end against an in-process
//! tonic server over a Unix domain socket, backed by a real
//! `rmail_core::outbox::OutboxStore` over a real (temp-file) database — the
//! same "build the handler directly, no fake transport" discipline
//! `compose_service.rs` uses.
//!
//! The scheduler loop and the SMTP classification are covered where they live
//! (`rmail-core`'s `outbox::scheduler` and `outbox::smtp` tests, against a real
//! in-process SMTP server). What this file owes is the *boundary*: that the
//! twelve RPCs translate faithfully, that every documented error path returns
//! the status code prd.md's contract promises, and — most of all — that the
//! safety property survives the trip through proto.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use rmail_core::config::{HumanDuration, SendConfig};
use rmail_core::outbox::{FollowupStore, OutboxStore};
use rmail_core::repo;
use rmail_core::Database;
use rmail_proto::v1::send_scheduler_service_client::SendSchedulerServiceClient;
use rmail_proto::v1::{
    CancelRequest, CreateDraftRequest, CreateFollowupRequest, DraftAddress, FollowupState,
    IdRequest, ListFollowupsRequest, ListOutboxRequest, NewDraftAttachment, OutboxEntry,
    OutboxState, RescheduleRequest, ScheduleSendRequest, SendOrigin, SuggestSendTimeRequest,
    UpdateBodyRequest, WatchOutboxRequest,
};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::UnixListenerStream;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;
use tonic::transport::{Channel, Server};
use tonic::Code;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct TestServer {
    socket: PathBuf,
    db_path: PathBuf,
    db: Database,
    account_id: i64,
    store: OutboxStore,
    shutdown: oneshot::Sender<()>,
    cancel: CancellationToken,
    handle: JoinHandle<()>,
}

impl TestServer {
    async fn start() -> Self {
        Self::start_with(SendConfig::default()).await
    }

    async fn start_with(config: SendConfig) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let socket = PathBuf::from("/tmp").join(format!("rmail-sched-{pid}-{n}.sock"));
        let db_path = std::env::temp_dir().join(format!("rmail-sched-svc-{pid}-{n}.db"));
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

        // No scheduler loop is spawned: this file tests the RPC surface, and a
        // background sender would race every assertion about a scheduled row
        // (and dial a port nothing is listening on).
        let store = OutboxStore::new(db.clone());
        let cancel = CancellationToken::new();
        let api = rmaild::SendSchedulerApi::new(
            store.clone(),
            FollowupStore::new(db.clone()),
            db.clone(),
            config,
            rmail_core::idempotency::IdempotencyStore::new(
                db.clone(),
                std::time::Duration::from_secs(3600),
                std::time::Duration::from_secs(300),
            ),
            cancel.clone(),
        );

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
            store,
            shutdown: shutdown_tx,
            cancel,
            handle,
        }
    }

    async fn client(&self) -> SendSchedulerServiceClient<Channel> {
        SendSchedulerServiceClient::new(rmail_core::connect_uds(&self.socket).await.unwrap())
    }

    fn request(&self) -> ScheduleSendRequest {
        ScheduleSendRequest {
            account_id: self.account_id,
            draft_id: None,
            to: vec!["bob@example.net".to_owned()],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: Some("Lunch".to_owned()),
            body: Some("Shall we say noon?".to_owned()),
            in_reply_to: None,
            send_at: Some(chrono::Utc::now().timestamp() + 3_600),
            send_at_nl: None,
            optimal: None,
            tz: String::new(),
            undo_window_secs: None,
            origin: SendOrigin::User as i32,
            idempotency_key: String::new(),
        }
    }

    /// Create a draft through the same `DraftStore` the handler renders from.
    async fn draft(&self, body: &str, attachment: bool) -> i64 {
        let api = rmaild::ComposeApi::new(self.store.drafts().clone());
        use rmail_proto::v1::compose_service_server::ComposeService as _;
        api.create_draft(tonic::Request::new(CreateDraftRequest {
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
                    filename: "notes.txt".to_owned(),
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

    async fn stop(self) {
        self.cancel.cancel();
        let _ = self.shutdown.send(());
        let _ = self.handle.await;
        let _ = std::fs::remove_file(&self.socket);
        for suffix in ["", "-wal", "-shm"] {
            let _ =
                std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.db_path.display())));
        }
    }
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

// ---------------------------------------------------------------------------
// Scheduling
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_inline_send_is_rendered_scheduled_and_readable() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let entry: OutboxEntry = client
        .schedule_send(ScheduleSendRequest {
            cc: vec!["carol@example.org".to_owned()],
            bcc: vec!["blind@example.org".to_owned()],
            ..server.request()
        })
        .await
        .unwrap()
        .into_inner();

    assert_eq!(entry.state, OutboxState::Scheduled as i32);
    assert_eq!(entry.origin, SendOrigin::User as i32);
    // The account's own identity, since an inline send names no From.
    assert_eq!(entry.from_addr, "alice@example.com");
    assert_eq!(entry.to, ["bob@example.net"]);
    assert_eq!(entry.bcc, ["blind@example.org"]);
    assert!(entry.smtp_message_id.is_none(), "nothing has been sent yet");

    // The frozen octets carry no Bcc; the blind recipient exists only for the
    // envelope.
    let raw = String::from_utf8(server.store.raw_mime(entry.id).await.unwrap()).unwrap();
    assert!(!raw.to_ascii_lowercase().contains("bcc:"), "{raw}");
    assert!(!raw.contains("blind@example.org"), "{raw}");
    assert!(
        raw.contains("carol@example.org"),
        "Cc is a real header:\n{raw}"
    );

    let listed = client
        .list_outbox(ListOutboxRequest {
            account_id: Some(server.account_id),
            state: OutboxState::Unspecified as i32,
            page_size: 0,
            page_token: String::new(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(listed.entries.len(), 1);
    assert_eq!(listed.entries[0].id, entry.id);

    server.stop().await;
}

#[tokio::test]
async fn a_draft_send_carries_the_draft_identity_and_its_attachments() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let draft_id = server.draft("from the draft", true).await;

    let entry = client
        .schedule_send(ScheduleSendRequest {
            draft_id: Some(draft_id),
            to: Vec::new(),
            subject: None,
            body: None,
            ..server.request()
        })
        .await
        .unwrap()
        .into_inner();

    assert_eq!(entry.draft_id, Some(draft_id));
    assert_eq!(entry.subject, "From a draft");
    let raw = String::from_utf8(server.store.raw_mime(entry.id).await.unwrap()).unwrap();
    assert!(
        raw.contains("notes.txt"),
        "the draft's attachment must survive into the frozen octets:\n{raw}"
    );
    server.stop().await;
}

#[tokio::test]
async fn a_natural_language_time_is_resolved_server_side() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let entry = client
        .schedule_send(ScheduleSendRequest {
            send_at: None,
            send_at_nl: Some("in 2h".to_owned()),
            tz: "UTC".to_owned(),
            ..server.request()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(entry.tz, "UTC");
    let delta = entry.send_at - now();
    assert!(
        (7_100..=7_300).contains(&delta),
        "expected roughly two hours out, got {delta}s"
    );

    // An expression the deterministic grammar does not cover is refused, not
    // guessed at.
    let status = client
        .schedule_send(ScheduleSendRequest {
            send_at: None,
            send_at_nl: Some("sometime next quarter".to_owned()),
            ..server.request()
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);

    server.stop().await;
}

#[tokio::test]
async fn an_immediate_send_gets_an_undo_window() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let entry = client
        .schedule_send(ScheduleSendRequest {
            send_at: None,
            ..server.request()
        })
        .await
        .unwrap()
        .into_inner();

    // prd.md: every send is really "schedule at now + undo_window", which is
    // what makes undo a cancel rather than a recall.
    assert!(entry.send_at > now(), "an immediate send is not immediate");
    assert_eq!(entry.undo_deadline, Some(entry.send_at));

    let canceled = client
        .cancel_scheduled(CancelRequest {
            id: None,
            account_id: Some(server.account_id),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(canceled.id, entry.id, "a bare undo takes the newest window");
    assert_eq!(canceled.state, OutboxState::Canceled as i32);

    server.stop().await;
}

// ---------------------------------------------------------------------------
// The safety property, through the wire
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_ai_send_gets_an_undo_window_no_configuration_can_remove() {
    // The end-to-end statement of `rmail_core::outbox::policy`'s floor: a
    // daemon configured to give humans no undo window at all, with AI
    // confirmation explicitly turned off, still cannot be talked into
    // transmitting a model's message with nobody able to stop it.
    let server = TestServer::start_with(SendConfig {
        undo_window: HumanDuration::new(Duration::ZERO),
        ai_requires_confirmation: false,
        ..SendConfig::default()
    })
    .await;
    let mut client = server.client().await;

    // A human send in this configuration is genuinely immediate.
    let human = client
        .schedule_send(ScheduleSendRequest {
            send_at: None,
            undo_window_secs: Some(0),
            ..server.request()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(human.undo_deadline, None);

    // The same request from a model is not, however it is phrased.
    for (label, request) in [
        (
            "no send_at, zero window",
            ScheduleSendRequest {
                send_at: None,
                undo_window_secs: Some(0),
                origin: SendOrigin::Ai as i32,
                ..server.request()
            },
        ),
        (
            "send_at = now",
            ScheduleSendRequest {
                send_at: Some(now()),
                undo_window_secs: Some(0),
                origin: SendOrigin::Ai as i32,
                ..server.request()
            },
        ),
        (
            "send_at in the past",
            ScheduleSendRequest {
                send_at: Some(now() - 86_400),
                origin: SendOrigin::Ai as i32,
                ..server.request()
            },
        ),
    ] {
        let entry = client.schedule_send(request).await.unwrap().into_inner();
        assert_eq!(entry.origin, SendOrigin::Ai as i32, "{label}");
        assert!(
            entry.send_at > now(),
            "{label}: an AI send must not become due immediately"
        );
        assert!(entry.undo_deadline.is_some(), "{label}");
        // And a human really can stop it.
        assert_eq!(
            client
                .cancel_scheduled(CancelRequest {
                    id: Some(entry.id),
                    account_id: None,
                })
                .await
                .unwrap()
                .into_inner()
                .state,
            OutboxState::Canceled as i32,
            "{label}"
        );
    }

    server.stop().await;
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cancel_reschedule_send_now_and_retry_report_the_right_codes() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let entry = client
        .schedule_send(server.request())
        .await
        .unwrap()
        .into_inner();

    // Reschedule to an expression, in a named zone.
    let moved = client
        .reschedule_send(RescheduleRequest {
            id: entry.id,
            send_at: None,
            send_at_nl: Some("in 3d".to_owned()),
            tz: "Europe/Berlin".to_owned(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(moved.tz, "Europe/Berlin");
    assert!(moved.send_at > entry.send_at);

    // Rescheduling with no time at all is a bad request, not a silent no-op.
    let status = client
        .reschedule_send(RescheduleRequest {
            id: entry.id,
            send_at: None,
            send_at_nl: None,
            tz: String::new(),
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);

    // Send now makes it due.
    let due = client
        .send_now(IdRequest { id: entry.id })
        .await
        .unwrap()
        .into_inner();
    assert!(due.send_at <= now());
    assert_eq!(due.undo_deadline, None);

    // Retrying something that has not failed is a precondition failure.
    let status = client
        .retry_failed(IdRequest { id: entry.id })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::FailedPrecondition);

    // Unknown ids are NOT_FOUND on every verb that takes one.
    for status in [
        client.send_now(IdRequest { id: 9_999 }).await.unwrap_err(),
        client
            .retry_failed(IdRequest { id: 9_999 })
            .await
            .unwrap_err(),
        client
            .cancel_scheduled(CancelRequest {
                id: Some(9_999),
                account_id: None,
            })
            .await
            .unwrap_err(),
    ] {
        assert_eq!(status.code(), Code::NotFound);
    }

    server.stop().await;
}

#[tokio::test]
async fn a_send_already_claimed_can_no_longer_be_cancelled() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let entry = client
        .schedule_send(ScheduleSendRequest {
            send_at: Some(now() - 1),
            ..server.request()
        })
        .await
        .unwrap()
        .into_inner();

    // Claim it the way the scheduler would.
    let claimed = server
        .store
        .claim_due("test-worker", 1, now(), Duration::from_secs(60))
        .await
        .unwrap();
    assert_eq!(claimed.len(), 1);

    let status = client
        .cancel_scheduled(CancelRequest {
            id: Some(entry.id),
            account_id: None,
        })
        .await
        .unwrap_err();
    assert_eq!(
        status.code(),
        Code::AlreadyExists,
        "prd.md: after the deadline, cancel reports already-sent rather than racing"
    );

    server.stop().await;
}

#[tokio::test]
async fn editing_a_body_needs_a_draft_behind_it() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let inline = client
        .schedule_send(server.request())
        .await
        .unwrap()
        .into_inner();
    let status = client
        .update_scheduled_body(UpdateBodyRequest {
            id: inline.id,
            body: "second thoughts".to_owned(),
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::FailedPrecondition);

    let draft_id = server.draft("first", false).await;
    let from_draft = client
        .schedule_send(ScheduleSendRequest {
            draft_id: Some(draft_id),
            to: Vec::new(),
            subject: None,
            body: None,
            ..server.request()
        })
        .await
        .unwrap()
        .into_inner();
    let edited = client
        .update_scheduled_body(UpdateBodyRequest {
            id: from_draft.id,
            body: "second".to_owned(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(edited.body_preview, "second");
    let raw = String::from_utf8(server.store.raw_mime(from_draft.id).await.unwrap()).unwrap();
    assert!(raw.contains("second") && !raw.contains("first"), "{raw}");

    server.stop().await;
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bad_requests_report_the_status_their_contract_promises() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // No recipient at all.
    let status = client
        .schedule_send(ScheduleSendRequest {
            to: Vec::new(),
            ..server.request()
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);

    // An address the renderer cannot use.
    let status = client
        .schedule_send(ScheduleSendRequest {
            to: vec!["not an address".to_owned()],
            ..server.request()
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);

    // An unknown zone.
    let status = client
        .schedule_send(ScheduleSendRequest {
            tz: "Middle/Earth".to_owned(),
            ..server.request()
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);

    // An unknown account has no sending identity to derive.
    let status = client
        .schedule_send(ScheduleSendRequest {
            account_id: 9_999,
            ..server.request()
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::NotFound);

    // A negative page size is nonsense rather than a request for zero.
    let status = client
        .list_outbox(ListOutboxRequest {
            account_id: None,
            state: OutboxState::Unspecified as i32,
            page_size: -1,
            page_token: String::new(),
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);

    server.stop().await;
}

#[tokio::test]
async fn an_account_with_no_sending_address_is_a_precondition_failure() {
    let server = TestServer::start().await;
    server
        .db
        .write({
            let account_id = server.account_id;
            move |c| {
                c.execute(
                    "UPDATE accounts SET username = NULL WHERE id = ?1",
                    [account_id],
                )
            }
        })
        .await
        .unwrap();
    let mut client = server.client().await;

    let status = client.schedule_send(server.request()).await.unwrap_err();
    assert_eq!(
        status.code(),
        Code::FailedPrecondition,
        "guessing a sender is not a recoverable mistake once the message is delivered"
    );
    server.stop().await;
}

// ---------------------------------------------------------------------------
// Streaming
// ---------------------------------------------------------------------------

#[tokio::test]
async fn watch_outbox_streams_every_transition() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let mut stream = client
        .watch_outbox(WatchOutboxRequest {
            account_id: Some(server.account_id),
        })
        .await
        .unwrap()
        .into_inner();

    let entry = client
        .schedule_send(server.request())
        .await
        .unwrap()
        .into_inner();
    let scheduled = stream.next().await.unwrap().unwrap().entry.unwrap();
    assert_eq!(scheduled.id, entry.id);
    assert_eq!(scheduled.state, OutboxState::Scheduled as i32);

    client
        .cancel_scheduled(CancelRequest {
            id: Some(entry.id),
            account_id: None,
        })
        .await
        .unwrap();
    let canceled = stream.next().await.unwrap().unwrap().entry.unwrap();
    assert_eq!(canceled.state, OutboxState::Canceled as i32);

    server.stop().await;
}

#[tokio::test]
async fn watch_outbox_honours_its_account_filter() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let mut stream = client
        .watch_outbox(WatchOutboxRequest {
            account_id: Some(server.account_id + 1),
        })
        .await
        .unwrap()
        .into_inner();

    client
        .schedule_send(server.request())
        .await
        .unwrap()
        .into_inner();

    // Nothing for the other account: the filter drops it rather than the
    // subscriber having to.
    let idle = tokio::time::timeout(Duration::from_millis(200), stream.next()).await;
    assert!(idle.is_err(), "a filtered stream must stay quiet");

    server.stop().await;
}

// ---------------------------------------------------------------------------
// Suggestions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_suggestion_lands_inside_the_configured_guardrails() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // 2026-01-05 is a Monday; 03:00 UTC is 19:00 the previous day in Los
    // Angeles, past the 18:00 guardrail, so the answer is the next morning's
    // 08:00 — 16:00 UTC on the 5th.
    let response = client
        .suggest_send_time(SuggestSendTimeRequest {
            account_id: server.account_id,
            tz: "America/Los_Angeles".to_owned(),
            not_before: Some(1_767_582_000),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(response.tz, "America/Los_Angeles");
    assert_eq!(response.display, "2026-01-05T08:00:00-08:00");
    assert!(!response.rationale.is_empty());

    // And `--at optimal` schedules exactly that instant rather than a second,
    // independently-computed one.
    let entry = client
        .schedule_send(ScheduleSendRequest {
            send_at: None,
            optimal: Some(true),
            tz: "America/Los_Angeles".to_owned(),
            ..server.request()
        })
        .await
        .unwrap()
        .into_inner();
    let again = client
        .suggest_send_time(SuggestSendTimeRequest {
            account_id: server.account_id,
            tz: "America/Los_Angeles".to_owned(),
            not_before: None,
        })
        .await
        .unwrap()
        .into_inner();
    assert!(
        (entry.send_at - again.send_at).abs() <= 2,
        "an optimal send should schedule the suggested instant"
    );

    server.stop().await;
}

// ---------------------------------------------------------------------------
// Follow-ups
// ---------------------------------------------------------------------------

#[tokio::test]
async fn follow_ups_are_armed_listed_and_dismissed() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let followup = client
        .create_followup(CreateFollowupRequest {
            account_id: server.account_id,
            // Angle brackets are what a user copies out of a header; they are
            // stripped so the reply that dismisses this can match.
            message_id: "<asked@example.com>".to_owned(),
            thread_id: None,
            remind_at: None,
            remind_in: Some("3d".to_owned()),
            tz: "UTC".to_owned(),
            note: Some("chase the quote".to_owned()),
            cancel_on_reply: None,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(followup.message_id, "asked@example.com");
    assert_eq!(followup.state, FollowupState::Armed as i32);
    assert!(followup.cancel_on_reply);
    let delta = followup.remind_at - now();
    assert!(
        (259_000..=259_400).contains(&delta),
        "expected roughly three days out, got {delta}s"
    );

    // A default delay when nothing is named.
    let defaulted = client
        .create_followup(CreateFollowupRequest {
            account_id: server.account_id,
            message_id: "other@example.com".to_owned(),
            thread_id: None,
            remind_at: None,
            remind_in: None,
            tz: String::new(),
            note: None,
            cancel_on_reply: Some(false),
        })
        .await
        .unwrap()
        .into_inner();
    assert!(!defaulted.cancel_on_reply);
    assert!(defaulted.remind_at > now());

    let listed = client
        .list_followups(ListFollowupsRequest {
            account_id: Some(server.account_id),
            state: FollowupState::Armed as i32,
            page_size: 0,
            page_token: String::new(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(listed.followups.len(), 2);

    let dismissed = client
        .dismiss_followup(IdRequest { id: followup.id })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(dismissed.state, FollowupState::Dismissed as i32);
    // Idempotent.
    assert_eq!(
        client
            .dismiss_followup(IdRequest { id: followup.id })
            .await
            .unwrap()
            .into_inner()
            .state,
        FollowupState::Dismissed as i32
    );
    assert_eq!(
        client
            .dismiss_followup(IdRequest { id: 9_999 })
            .await
            .unwrap_err()
            .code(),
        Code::NotFound
    );

    server.stop().await;
}

#[tokio::test]
async fn a_followup_on_an_unknown_account_or_with_no_message_id_is_refused() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let status = client
        .create_followup(CreateFollowupRequest {
            account_id: 9_999,
            message_id: "abc@example.com".to_owned(),
            thread_id: None,
            remind_at: Some(now() + 60),
            remind_in: None,
            tz: String::new(),
            note: None,
            cancel_on_reply: None,
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::NotFound);

    let status = client
        .create_followup(CreateFollowupRequest {
            account_id: server.account_id,
            message_id: "   ".to_owned(),
            thread_id: None,
            remind_at: Some(now() + 60),
            remind_in: None,
            tz: String::new(),
            note: None,
            cancel_on_reply: None,
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);

    server.stop().await;
}

// ---------------------------------------------------------------------------
// Idempotency (task 40)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_retried_schedule_send_under_one_key_queues_one_message() {
    // The case the outbox's own `smtp_message_id` fence cannot cover: two
    // enqueues are two genuinely different messages, each with its own
    // Message-ID, so nothing downstream can tell they were meant to be one.
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let request = ScheduleSendRequest {
        idempotency_key: "send-key-1".to_owned(),
        ..server.request()
    };
    let first = client
        .schedule_send(request.clone())
        .await
        .unwrap()
        .into_inner();
    let replayed = client
        .schedule_send(request)
        .await
        .expect("a retry under the same key must replay")
        .into_inner();

    assert_eq!(
        replayed.id, first.id,
        "the retry must replay the first entry, not create a second"
    );
    assert_eq!(replayed.subject, first.subject);

    let queued: i64 = server
        .db
        .with_read(|conn| conn.query_row("SELECT count(*) FROM outbox", [], |row| row.get(0)))
        .unwrap();
    assert_eq!(queued, 1, "the message was queued twice");

    server.stop().await;
}

#[tokio::test]
async fn list_outbox_pages_through_the_queue_exactly_once() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let mut expected = Vec::new();
    for n in 0..5 {
        expected.push(
            client
                .schedule_send(ScheduleSendRequest {
                    subject: Some(format!("Note {n}")),
                    ..server.request()
                })
                .await
                .unwrap()
                .into_inner()
                .id,
        );
    }
    expected.sort_unstable();

    let mut seen = Vec::new();
    let mut token = String::new();
    for _ in 0..10 {
        let page = client
            .list_outbox(ListOutboxRequest {
                account_id: Some(server.account_id),
                state: OutboxState::Unspecified as i32,
                page_size: 2,
                page_token: token.clone(),
            })
            .await
            .unwrap()
            .into_inner();
        assert!(page.entries.len() <= 2);
        seen.extend(page.entries.iter().map(|e| e.id));
        if page.next_page_token.is_empty() {
            break;
        }
        token = page.next_page_token;
    }
    seen.sort_unstable();
    assert_eq!(seen, expected, "paging repeated or skipped an entry");

    server.stop().await;
}

#[tokio::test]
async fn an_outbox_page_token_cannot_be_re_aimed_at_another_state() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    for n in 0..3 {
        client
            .schedule_send(ScheduleSendRequest {
                subject: Some(format!("Note {n}")),
                ..server.request()
            })
            .await
            .unwrap();
    }

    let token = client
        .list_outbox(ListOutboxRequest {
            account_id: Some(server.account_id),
            state: OutboxState::Unspecified as i32,
            page_size: 1,
            page_token: String::new(),
        })
        .await
        .unwrap()
        .into_inner()
        .next_page_token;
    assert!(!token.is_empty(), "a full page should carry a token");

    let status = client
        .list_outbox(ListOutboxRequest {
            account_id: Some(server.account_id),
            state: OutboxState::Scheduled as i32,
            page_size: 1,
            page_token: token,
        })
        .await
        .expect_err("a token from an unfiltered listing must not resume a filtered one");
    assert_eq!(status.code(), Code::InvalidArgument, "{status:?}");

    server.stop().await;
}

#[tokio::test]
async fn reusing_a_send_key_with_a_different_message_is_already_exists() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    client
        .schedule_send(ScheduleSendRequest {
            idempotency_key: "send-key-2".to_owned(),
            ..server.request()
        })
        .await
        .unwrap();

    let status = client
        .schedule_send(ScheduleSendRequest {
            idempotency_key: "send-key-2".to_owned(),
            subject: Some("Dinner, actually".to_owned()),
            ..server.request()
        })
        .await
        .expect_err("a key names one message; a changed one must not replay it");
    assert_eq!(status.code(), Code::AlreadyExists, "{status:?}");

    server.stop().await;
}
