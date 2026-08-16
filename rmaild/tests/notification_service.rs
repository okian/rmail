//! Integration test: drive `NotificationService` end-to-end against an
//! in-process tonic server over a Unix domain socket.
//!
//! Covers the `Status` paths as well as the happy ones — an unknown message,
//! a negative cursor, and a daemon with `notify.enabled = false` (which must
//! refuse to enqueue a scoring pass rather than quietly spending money the
//! operator switched off) — plus the two claims that only a real server can
//! make: that `StreamAlerts` replays the durable backlog from a cursor (and
//! deliberately replays *nothing* for an absent cursor, so `mail notify watch`
//! means "from now on"), and that a suppressed notification never appears in
//! that stream at all.
//!
//! The live-tail half of `StreamAlerts` is covered in `rmail-core`
//! (`notify::tests::a_delivered_notification_is_published_and_readable_from_the_cursor`)
//! rather than here: publishing happens inside `NotifyEngine::deliver`, and
//! reaching it through a *daemon* would need a delivery channel that actually
//! succeeds inside a container, which by design there is not (see
//! `rmail_core::notify::channel`'s module docs on why the only delivering
//! channel is the local desktop one).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use rmail_core::config::{AccountConfig, AccountNotifyConfig, NotifyChannel, NotifyConfig};
use rmail_core::notify::{repo, NotifyScore, Tier};
use rmail_core::{repo as core_repo, Config, Database};
use rmail_proto::v1::notification_service_client::NotificationServiceClient;
use rmail_proto::v1::{
    NotificationState, NotificationTier, ScoreMessageRequest, StreamAlertsRequest,
};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tonic::transport::Channel;
use tonic::Code;

static COUNTER: AtomicU32 = AtomicU32::new(0);

const ACCOUNT: &str = "Personal";

fn unique_path(label: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    std::env::temp_dir().join(format!("rmail-notifysvc-{label}-{pid}-{n}"))
}

/// A provider that refuses every call without touching the network.
///
/// Injected so the daemon's AI subsystem is genuinely *active* — the worker
/// pool is built, the dispatch loop runs — while no request can ever leave the
/// container. Refusing rather than answering is deliberate: a scoring job this
/// suite enqueued must stay enqueued, so a test asserting "the RPC put a job in
/// the queue" is not racing a background worker that would drain it.
#[derive(Debug)]
struct RefusingProvider;

#[tonic::async_trait]
impl rmail_core::ai::Provider for RefusingProvider {
    async fn complete(
        &self,
        _request: &rmail_core::ai::provider::ChatRequest,
        _cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<rmail_core::ai::provider::ChatResponse, rmail_core::Error> {
        Err(rmail_core::Error::unavailable(
            "this test provider never calls out",
        ))
    }

    async fn stream(
        &self,
        _request: &rmail_core::ai::provider::ChatRequest,
        _cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<rmail_core::ai::provider::ProviderStream, rmail_core::Error> {
        Err(rmail_core::Error::unavailable(
            "this test provider never calls out",
        ))
    }
}

struct TestServer {
    socket: PathBuf,
    db_path: PathBuf,
    db: Database,
    account_id: i64,
    inbox_id: i64,
    shutdown: oneshot::Sender<()>,
    handle: JoinHandle<Result<(), rmaild::ServeError>>,
}

impl TestServer {
    /// A daemon whose AI subsystem is active behind [`RefusingProvider`].
    async fn start(notify: NotifyConfig) -> Self {
        Self::start_inner(notify, true, AccountNotifyConfig::default()).await
    }

    /// A daemon with `ai.enabled = false` and no injected provider — so
    /// `ai_active` is genuinely false and nothing would ever run a queued
    /// scoring job.
    async fn start_without_ai(notify: NotifyConfig) -> Self {
        Self::start_inner(notify, false, AccountNotifyConfig::default()).await
    }

    /// A daemon whose one account carries its own `[[accounts]] notify`
    /// overrides.
    async fn start_with_account(notify: NotifyConfig, account: AccountNotifyConfig) -> Self {
        Self::start_inner(notify, true, account).await
    }

    async fn start_inner(
        notify: NotifyConfig,
        ai_active: bool,
        account_notify: AccountNotifyConfig,
    ) -> Self {
        let socket = unique_path("sock");
        let db_path = unique_path("db");
        let db = Database::open(&db_path).expect("open db");
        let (account_id, inbox_id) = db
            .write(|c| {
                let account_id = core_repo::insert_account(
                    c,
                    &core_repo::NewAccount {
                        name: ACCOUNT.to_owned(),
                        ..Default::default()
                    },
                )?;
                let inbox_id = core_repo::insert_mailbox(
                    c,
                    &core_repo::NewMailbox {
                        account_id,
                        name: "INBOX".to_owned(),
                        ..Default::default()
                    },
                )?;
                Ok((account_id, inbox_id))
            })
            .await
            .unwrap();

        let mut config = Config::default();
        config.index.semantic.enabled = false;
        // The AI subsystem is *active* (an injected provider, so nothing ever
        // dials out — see `RefusingProvider`) rather than switched off,
        // because `ScoreMessage`'s enqueue path is gated on `ai_active` as
        // well as on `notify.enabled`: a daemon that answered QUEUED with no
        // worker pool behind it would be promising something it structurally
        // cannot deliver. Testing against `ai.enabled = false` would therefore
        // only ever exercise the refusal.
        config.ai.enabled = ai_active;
        config.notify = notify;
        config.accounts = vec![AccountConfig {
            name: ACCOUNT.to_owned(),
            imap_server: None,
            port: 993,
            username: None,
            password_command: None,
            password_env: None,
            keychain: None,
            smtp_server: None,
            smtp_port: 587,
            ai: rmail_core::config::AccountAiConfig::default(),
            notify: account_notify,
        }];

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let server_socket = socket.clone();
        let server_db = db.clone();
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
                std::sync::Arc::new(rmail_core::imap::mutate::LiveImapMutator::new(
                    server_db.clone(),
                )),
            );
            let tag_store = rmail_core::tags::TagStore::new(
                server_db.clone(),
                std::sync::Arc::new(rmail_core::imap::mutate::LiveImapMutator::new(
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
                    ai_provider: ai_active.then(|| {
                        std::sync::Arc::new(RefusingProvider)
                            as std::sync::Arc<dyn rmail_core::ai::Provider>
                    }),
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
            account_id,
            inbox_id,
            shutdown: shutdown_tx,
            handle,
        }
    }

    async fn client(&self) -> NotificationServiceClient<Channel> {
        NotificationServiceClient::new(rmail_core::connect_uds(&self.socket).await.unwrap())
    }

    async fn message(&self, uid: i64, subject: &str) -> i64 {
        let (account_id, mailbox_id) = (self.account_id, self.inbox_id);
        let subject = subject.to_owned();
        self.db
            .write(move |c| {
                core_repo::insert_message(
                    c,
                    &core_repo::NewMessage {
                        account_id,
                        mailbox_id,
                        uid,
                        uidvalidity: 1,
                        subject: Some(subject),
                        from_addr: Some("ada@example.com".to_owned()),
                        from_name: Some("Ada".to_owned()),
                        body_text: Some("body".to_owned()),
                        ..Default::default()
                    },
                )
            })
            .await
            .unwrap()
    }

    async fn score(&self, message_id: i64, tier: Tier, reason: &str) {
        repo::record_score(
            &self.db,
            message_id,
            self.account_id,
            &NotifyScore {
                tier,
                reason: reason.to_owned(),
            },
            "claude-haiku-4-5",
            None,
        )
        .await
        .unwrap();
    }

    async fn shutdown(self) {
        let _ = self.shutdown.send(());
        let _ = tokio::time::timeout(Duration::from_secs(10), self.handle).await;
        for suffix in ["", "-wal", "-shm"] {
            let _ =
                std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.db_path.display())));
        }
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// `notify.enabled = true`, but with the desktop channel off — every test
/// here is about the RPC surface, and none of them wants a real notifier.
fn enabled_config() -> NotifyConfig {
    NotifyConfig {
        enabled: true,
        channel: NotifyChannel::None,
        // Long enough that the daemon's own delivery loop never races a test
        // that is asserting about a `pending` row.
        tick_interval: rmail_core::config::HumanDuration::new(Duration::from_secs(3600)),
        ..NotifyConfig::default()
    }
}

// ---------------------------------------------------------------------------
// ScoreMessage
// ---------------------------------------------------------------------------

#[tokio::test]
async fn score_message_reports_an_existing_decision_with_its_effective_threshold() {
    let server = TestServer::start(enabled_config()).await;
    let id = server.message(1, "Production is down").await;
    server
        .score(id, Tier::Critical, "the API is returning 500s")
        .await;

    let response = server
        .client()
        .await
        .score_message(ScoreMessageRequest { message_id: id })
        .await
        .unwrap()
        .into_inner();

    assert_eq!(response.state, NotificationState::Pending as i32);
    assert_eq!(response.tier, Some(NotificationTier::Critical as i32));
    assert_eq!(
        response.reason.as_deref(),
        Some("the API is returning 500s")
    );
    assert_eq!(response.effective_threshold, "high");
    assert!(response.account_enabled);
    assert!(response.would_notify);

    server.shutdown().await;
}

#[tokio::test]
async fn score_message_reports_would_notify_false_below_the_threshold() {
    let server = TestServer::start(enabled_config()).await;
    let id = server.message(1, "This week in widgets").await;
    server.score(id, Tier::Low, "a newsletter").await;

    let response = server
        .client()
        .await
        .score_message(ScoreMessageRequest { message_id: id })
        .await
        .unwrap()
        .into_inner();

    assert_eq!(response.tier, Some(NotificationTier::Low as i32));
    assert!(
        !response.would_notify,
        "a newsletter must not report that it would ping"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn score_message_queues_a_pass_for_an_unscored_message() {
    let server = TestServer::start(enabled_config()).await;
    let id = server.message(1, "Production is down").await;

    let response = server
        .client()
        .await
        .score_message(ScoreMessageRequest { message_id: id })
        .await
        .unwrap()
        .into_inner();

    assert_eq!(response.state, NotificationState::Queued as i32);
    assert_eq!(response.tier, None);

    let queued: i64 = server
        .db
        .with_read(move |conn| {
            conn.query_row(
                "SELECT count(*) FROM ai_queue WHERE message_id = ?1 AND pass = 'notify'",
                [id],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(queued, 1, "the RPC must actually enqueue the scoring pass");

    server.shutdown().await;
}

/// The switch is not advisory: an RPC must not be a side door around
/// `notify.enabled`.
#[tokio::test]
async fn score_message_refuses_to_queue_on_a_daemon_with_notifications_disabled() {
    let server = TestServer::start(NotifyConfig {
        enabled: false,
        ..enabled_config()
    })
    .await;
    let id = server.message(1, "Production is down").await;

    let status = server
        .client()
        .await
        .score_message(ScoreMessageRequest { message_id: id })
        .await
        .expect_err("a disabled daemon must not enqueue a paid scoring pass");
    assert_eq!(status.code(), Code::FailedPrecondition);

    let queued: i64 = server
        .db
        .with_read(move |conn| {
            conn.query_row(
                "SELECT count(*) FROM ai_queue WHERE message_id = ?1 AND pass = 'notify'",
                [id],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(queued, 0);

    server.shutdown().await;
}

/// The other half of the same gate: notifications are on, but the AI
/// subsystem is not, so there is no worker pool to run a queued scoring job.
/// Answering `QUEUED` there would be a promise the daemon cannot keep — and
/// `AiQueue::enqueue`'s `(message_id, pass)` dedup would make the orphaned row
/// permanent.
#[tokio::test]
async fn score_message_refuses_to_queue_when_the_ai_subsystem_is_inactive() {
    let server = TestServer::start_without_ai(enabled_config()).await;
    let id = server.message(1, "Production is down").await;

    let status = server
        .client()
        .await
        .score_message(ScoreMessageRequest { message_id: id })
        .await
        .expect_err("nothing would ever run the job, so it must not be queued");
    assert_eq!(status.code(), Code::FailedPrecondition);

    let queued: i64 = server
        .db
        .with_read(move |conn| {
            conn.query_row(
                "SELECT count(*) FROM ai_queue WHERE message_id = ?1 AND pass = 'notify'",
                [id],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(queued, 0);

    server.shutdown().await;
}

/// A per-account opt-out is a *cost* gate, not only a silence: the RPC must
/// not queue a paid scoring pass for an account that can never produce a
/// notification. (`NotifyPassHandler` would decline the job anyway — that is
/// what protects the background enqueue path — but answering QUEUED here would
/// still be a promise about work that will be thrown away.)
#[tokio::test]
async fn score_message_refuses_to_queue_for_an_account_with_notifications_off() {
    let server = TestServer::start_with_account(
        enabled_config(),
        AccountNotifyConfig {
            enabled: Some(false),
            threshold: None,
        },
    )
    .await;
    let id = server.message(1, "Production is down").await;

    let status = server
        .client()
        .await
        .score_message(ScoreMessageRequest { message_id: id })
        .await
        .expect_err("an opted-out account must not have a paid pass queued for it");
    assert_eq!(status.code(), Code::FailedPrecondition);

    let queued: i64 = server
        .db
        .with_read(move |conn| {
            conn.query_row(
                "SELECT count(*) FROM ai_queue WHERE message_id = ?1 AND pass = 'notify'",
                [id],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(queued, 0);

    server.shutdown().await;
}

/// …but a decision already on record is still reported on a disabled daemon:
/// turning the feature off must not make history unreadable.
#[tokio::test]
async fn score_message_still_reports_a_recorded_decision_when_disabled() {
    let server = TestServer::start(NotifyConfig {
        enabled: false,
        ..enabled_config()
    })
    .await;
    let id = server.message(1, "Production is down").await;
    server.score(id, Tier::Critical, "outage").await;

    let response = server
        .client()
        .await
        .score_message(ScoreMessageRequest { message_id: id })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(response.tier, Some(NotificationTier::Critical as i32));

    server.shutdown().await;
}

#[tokio::test]
async fn score_message_rejects_an_unknown_message_and_a_nonpositive_id() {
    let server = TestServer::start(enabled_config()).await;
    let mut client = server.client().await;

    let unknown = client
        .score_message(ScoreMessageRequest {
            message_id: 999_999,
        })
        .await
        .expect_err("an unknown message must not be reported as unscored");
    assert_eq!(unknown.code(), Code::NotFound);

    let bad = client
        .score_message(ScoreMessageRequest { message_id: 0 })
        .await
        .expect_err("a nonpositive id is invalid input");
    assert_eq!(bad.code(), Code::InvalidArgument);

    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// StreamAlerts
// ---------------------------------------------------------------------------

/// A cursor of `1` replays everything after id 1 from the durable table
/// before the live tail resumes — the resumability the CLI's `--since`
/// depends on.
#[tokio::test]
async fn stream_alerts_replays_the_backlog_from_a_cursor() {
    let server = TestServer::start(enabled_config()).await;
    let first = server.message(1, "Outage one").await;
    let second = server.message(2, "Outage two").await;
    server.score(first, Tier::Critical, "one").await;
    server.score(second, Tier::Critical, "two").await;
    // Mark both delivered directly: this test is about the stream's cursor,
    // not about re-proving the delivery loop (rmail-core's own suite does).
    for id in [first, second] {
        let row = repo::state_of(&server.db, id).await.unwrap().unwrap();
        assert!(repo::mark_delivered(&server.db, row.1).await.unwrap());
    }

    let mut stream = server
        .client()
        .await
        .stream_alerts(StreamAlertsRequest { since_id: Some(1) })
        .await
        .unwrap()
        .into_inner();

    let alert = tokio::time::timeout(Duration::from_secs(10), stream.message())
        .await
        .expect("the backlog must arrive promptly")
        .unwrap()
        .expect("one alert is after the cursor");
    assert_eq!(alert.message_id, second);
    assert_eq!(alert.tier, NotificationTier::Critical as i32);
    assert_eq!(alert.subject.as_deref(), Some("Outage two"));
    assert_eq!(alert.from.as_deref(), Some("Ada <ada@example.com>"));
    assert_eq!(alert.account, ACCOUNT);

    server.shutdown().await;
}

/// A suppressed decision is not an alert. This is the RPC-level restatement
/// of prd.md #62's "newsletters never ping".
#[tokio::test]
async fn stream_alerts_never_reports_a_suppressed_notification() {
    let server = TestServer::start(enabled_config()).await;
    let low = server.message(1, "This week in widgets").await;
    let high = server.message(2, "Outage").await;
    server.score(low, Tier::Low, "a newsletter").await;
    server.score(high, Tier::Critical, "outage").await;

    let low_row = repo::state_of(&server.db, low).await.unwrap().unwrap();
    assert!(
        repo::mark_suppressed(&server.db, low_row.1, "below_threshold")
            .await
            .unwrap()
    );
    let high_row = repo::state_of(&server.db, high).await.unwrap().unwrap();
    assert!(repo::mark_delivered(&server.db, high_row.1).await.unwrap());

    let mut stream = server
        .client()
        .await
        // A present cursor of 0 replays the whole history — ids start at 1.
        .stream_alerts(StreamAlertsRequest { since_id: Some(0) })
        .await
        .unwrap()
        .into_inner();

    let alert = tokio::time::timeout(Duration::from_secs(10), stream.message())
        .await
        .expect("the backlog must arrive promptly")
        .unwrap()
        .expect("the delivered alert is in the replay");
    assert_eq!(
        alert.message_id, high,
        "the suppressed newsletter must not be in the stream"
    );

    // And nothing else follows: the suppressed row is not merely ordered
    // after the delivered one, it is absent.
    let nothing = tokio::time::timeout(Duration::from_millis(300), stream.message()).await;
    assert!(
        nothing.is_err(),
        "only delivered notifications are alerts: {nothing:?}"
    );

    server.shutdown().await;
}

/// An *absent* cursor means "from now on", not "replay everything" —
/// otherwise every `mail notify watch` would dump a week of history into a
/// terminal. The presence of the field is the whole distinction; see
/// `StreamAlertsRequest`'s proto comment.
#[tokio::test]
async fn stream_alerts_with_an_absent_cursor_starts_at_the_current_head() {
    let server = TestServer::start(enabled_config()).await;
    let id = server.message(1, "Outage").await;
    server.score(id, Tier::Critical, "outage").await;
    let row = repo::state_of(&server.db, id).await.unwrap().unwrap();
    assert!(repo::mark_delivered(&server.db, row.1).await.unwrap());

    let mut stream = server
        .client()
        .await
        .stream_alerts(StreamAlertsRequest { since_id: None })
        .await
        .unwrap()
        .into_inner();

    let nothing = tokio::time::timeout(Duration::from_millis(300), stream.message()).await;
    assert!(
        nothing.is_err(),
        "an already-delivered alert must not be replayed to an absent cursor: {nothing:?}"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn stream_alerts_rejects_a_negative_cursor() {
    let server = TestServer::start(enabled_config()).await;

    let status = server
        .client()
        .await
        .stream_alerts(StreamAlertsRequest { since_id: Some(-1) })
        .await
        .expect_err("a negative cursor is invalid input");
    assert_eq!(status.code(), Code::InvalidArgument);

    server.shutdown().await;
}
