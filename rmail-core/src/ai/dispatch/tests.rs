//! Proof of the gap this module closes: nothing enqueued a triage job when a
//! message synced. Every test here drives [`AiDispatchLoop`] against a real
//! [`EventLog`]/[`AiQueue`] over a temp SQLite database — the same "test the
//! real pipeline, not a mock of it" discipline `ai::queue`'s own tests use.

use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use rusqlite::OptionalExtension;
use tokio_util::sync::CancellationToken;

use super::{AiDispatchLoop, AiPauseFlag, PRIORITY_BACKFILL};
use crate::ai::policy::PolicyEngine;
use crate::ai::provider::{ChatRequest, ChatResponse, Provider, ProviderStream};
use crate::ai::queue::{AiWorkerPool, QueueOptions};
use crate::ai::triage;
use crate::ai::AiQueue;
use crate::config::{AiLimits, AiPolicyMode, AiPrivacy};
use crate::error::Error;
use crate::events::{EventKind, EventLog, NewEvent, Retention};
use crate::repo;
use crate::storage::Database;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    db: Database,
    path: PathBuf,
    events: EventLog,
    queue: AiQueue,
    account_id: i64,
    mailbox_id: i64,
    next_uid: AtomicI64,
}

impl Fixture {
    async fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-aidispatch-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", path.display())));
        }
        let db = Database::open(&path).unwrap();
        let events = EventLog::new(db.clone(), Retention::unlimited());
        let queue = AiQueue::new(db.clone(), QueueOptions::default());
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
        Self {
            db,
            path,
            events,
            queue,
            account_id,
            mailbox_id,
            next_uid: AtomicI64::new(1),
        }
    }

    /// Insert a message and return its id.
    async fn message(&self) -> i64 {
        let uid = self.next_uid.fetch_add(1, Ordering::Relaxed);
        let account_id = self.account_id;
        let mailbox_id = self.mailbox_id;
        self.db
            .write(move |c| {
                repo::insert_message(
                    c,
                    &repo::NewMessage {
                        account_id,
                        mailbox_id,
                        uid,
                        uidvalidity: 1,
                        subject: Some("Test".to_owned()),
                        body_text: Some("hello".to_owned()),
                        ..Default::default()
                    },
                )
            })
            .await
            .unwrap()
    }

    /// Insert a message and append a `NewMail` event for it — standing in
    /// for what `sync::engine::LogSink` does the moment a message lands.
    async fn sync_new_message(&self) -> i64 {
        let message_id = self.message().await;
        self.events
            .append(
                NewEvent::new(EventKind::NewMail)
                    .account(self.account_id)
                    .mailbox(self.mailbox_id)
                    .message(message_id),
            )
            .await
            .unwrap();
        message_id
    }

    async fn queue_state(&self, message_id: i64, pass: &str) -> Option<String> {
        let pass = pass.to_owned();
        self.db
            .read(move |c| {
                c.query_row(
                    "SELECT state FROM ai_queue WHERE message_id = ?1 AND pass = ?2",
                    rusqlite::params![message_id, pass],
                    |row| row.get::<_, String>(0),
                )
                .optional()
            })
            .await
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

// ---------------------------------------------------------------------------
// A provider that must never be called
// ---------------------------------------------------------------------------

/// Proves `tick`'s pipeline reaches the queue/lease stage without needing a
/// working model backend: with no [`crate::ai::queue::PassHandler`]
/// registered, `AiWorkerPool::process_one` terminates a leased job the
/// instant it fails to find a handler for its pass — *before* policy,
/// assembly, or the provider are ever touched (see `queue/worker.rs`'s own
/// `process_one`). A call here would mean that ordering broke.
#[derive(Debug)]
struct NeverCalledProvider;

#[async_trait]
impl Provider for NeverCalledProvider {
    async fn complete(
        &self,
        _request: &ChatRequest,
        _cancel: &CancellationToken,
    ) -> Result<ChatResponse, Error> {
        unreachable!("no PassHandler is registered; the provider must never be reached")
    }

    async fn stream(
        &self,
        _request: &ChatRequest,
        _cancel: &CancellationToken,
    ) -> Result<ProviderStream, Error> {
        unreachable!("no PassHandler is registered; the provider must never be reached")
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

fn no_handler_pool(fx: &Fixture) -> AiWorkerPool {
    let provider: std::sync::Arc<dyn Provider> = std::sync::Arc::new(NeverCalledProvider);
    AiWorkerPool::new(
        fx.db.clone(),
        fx.queue.clone(),
        provider,
        std::sync::Arc::new(open_policy()),
        high_rpm_limits(),
        AiPrivacy::default(),
        Vec::new(),
        "test-dispatch-worker",
        fx.events.clone(),
    )
}

// ---------------------------------------------------------------------------
// The gap 1 proof
// ---------------------------------------------------------------------------

#[tokio::test]
async fn syncing_a_message_makes_a_triage_job_appear() {
    // This is the acceptance criterion, proven directly: nothing before this
    // module ever turned a `NewMail` event into an `ai_queue` row. Before
    // task 50, this assertion would fail — `queue_state` would return `None`
    // forever, no matter how long the test waited, because nothing was
    // watching the event log at all.
    let fx = Fixture::open().await;
    let message_id = fx.sync_new_message().await;

    let loop_ = AiDispatchLoop::new(fx.events.clone(), fx.queue.clone(), no_handler_pool(&fx));
    let (enqueued, _cursor) = loop_.drain_new_mail(0).await.unwrap();

    assert_eq!(enqueued, 1, "exactly one job should have been enqueued");
    assert_eq!(
        fx.queue_state(message_id, "triage").await.as_deref(),
        Some("pending"),
        "the synced message must now have a pending triage job"
    );
}

/// Task 57's acceptance criterion opens with "New mail → low-priority
/// `suggest_tags` job", and both halves of that clause are asserted here: the
/// job exists at all, and it is enqueued *behind* triage rather than beside
/// it. The priority is not cosmetic — `budget::WorkClass::for_priority`
/// classifies at `PRIORITY_BACKFILL` as `Bulk`, which is what keeps auto-
/// tagging drawing on the bulk sub-budget instead of competing with
/// user-facing calls.
#[tokio::test]
async fn syncing_a_message_makes_a_low_priority_suggest_tags_job_appear() {
    let fx = Fixture::open().await;
    let message_id = fx.sync_new_message().await;

    let loop_ = AiDispatchLoop::new(fx.events.clone(), fx.queue.clone(), no_handler_pool(&fx))
        .with_suggest_tags_pass(true);
    let (enqueued, _cursor) = loop_.drain_new_mail(0).await.unwrap();

    assert_eq!(enqueued, 2, "triage and auto-tagging, one message");
    assert_eq!(
        fx.queue_state(message_id, crate::tags::ai::PASS)
            .await
            .as_deref(),
        Some("pending")
    );
    let priorities = fx
        .db
        .read(move |c| {
            let mut stmt =
                c.prepare("SELECT pass, priority FROM ai_queue WHERE message_id = ?1")?;
            let rows = stmt.query_map([message_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
        .await
        .unwrap();
    let suggest = priorities
        .iter()
        .find(|(pass, _)| pass == crate::tags::ai::PASS)
        .expect("the suggest_tags row");
    let triage_row = priorities
        .iter()
        .find(|(pass, _)| pass == triage::PASS)
        .expect("the triage row");
    assert_eq!(suggest.1, PRIORITY_BACKFILL);
    assert!(
        suggest.1 > triage_row.1,
        "auto-tagging must sort behind triage, got {suggest:?} vs {triage_row:?}"
    );
}

/// The switch lives at the enqueue site precisely so a disabled feature
/// queues nothing: a job enqueued and then declined would still occupy
/// `ai_queue`, and `AiQueue::enqueue`'s `(message_id, pass)` dedup would make
/// that permanent.
#[tokio::test]
async fn the_suggest_tags_pass_enqueues_nothing_when_it_is_switched_off() {
    let fx = Fixture::open().await;
    let message_id = fx.sync_new_message().await;

    let loop_ = AiDispatchLoop::new(fx.events.clone(), fx.queue.clone(), no_handler_pool(&fx));
    let (enqueued, _) = loop_.drain_new_mail(0).await.unwrap();

    assert_eq!(enqueued, 1, "triage only");
    assert_eq!(
        fx.queue_state(message_id, crate::tags::ai::PASS).await,
        None,
        "no auto-tagging job may be created while the feature is off"
    );
}

#[tokio::test]
async fn draining_twice_does_not_double_enqueue() {
    // `AiQueue::enqueue`'s own `(message_id, pass)` dedup makes a re-drain
    // (e.g. after a restart, which always resumes from cursor 0) a no-op for
    // work already queued — proven here at the dispatch-loop level, not just
    // inside `ai::queue`'s own tests.
    let fx = Fixture::open().await;
    let message_id = fx.sync_new_message().await;
    let loop_ = AiDispatchLoop::new(fx.events.clone(), fx.queue.clone(), no_handler_pool(&fx));

    let (first, cursor) = loop_.drain_new_mail(0).await.unwrap();
    assert_eq!(first, 1);
    let (second, _) = loop_.drain_new_mail(0).await.unwrap();
    assert_eq!(second, 0, "the same NewMail event must not enqueue twice");

    let stats = fx.queue.stats().await.unwrap();
    assert_eq!(stats.ready, 1, "still exactly one job in the queue");
    assert!(cursor > 0, "the cursor must have advanced past the event");
    assert_eq!(
        fx.queue_state(message_id, "triage").await.as_deref(),
        Some("pending")
    );
}

#[tokio::test]
async fn non_new_mail_events_are_ignored() {
    let fx = Fixture::open().await;
    fx.events
        .append(
            NewEvent::new(EventKind::FlagChanged)
                .account(fx.account_id)
                .mailbox(fx.mailbox_id)
                .message(999),
        )
        .await
        .unwrap();

    let loop_ = AiDispatchLoop::new(fx.events.clone(), fx.queue.clone(), no_handler_pool(&fx));
    let (enqueued, cursor) = loop_.drain_new_mail(0).await.unwrap();

    assert_eq!(enqueued, 0);
    assert!(
        cursor > 0,
        "the cursor still advances past the event it scanned but did not act on, \
         so a quiet stretch of non-NewMail events cannot wedge it"
    );
    assert_eq!(fx.queue.stats().await.unwrap().outstanding(), 0);
}

#[tokio::test]
async fn a_multi_page_backlog_of_synced_messages_is_fully_drained() {
    // DRAIN_PAGE is 500; this exercises the paging loop's own seam the same
    // way `sync_service`'s watch-events test exercises `REPLAY_PAGE`.
    let fx = Fixture::open().await;
    let mut ids = Vec::new();
    for _ in 0..650 {
        let message_id = fx.message().await;
        fx.events
            .append(
                NewEvent::new(EventKind::NewMail)
                    .account(fx.account_id)
                    .mailbox(fx.mailbox_id)
                    .message(message_id),
            )
            .await
            .unwrap();
        ids.push(message_id);
    }

    let loop_ = AiDispatchLoop::new(fx.events.clone(), fx.queue.clone(), no_handler_pool(&fx));
    let (enqueued, _cursor) = loop_.drain_new_mail(0).await.unwrap();

    assert_eq!(enqueued, 650);
    for id in ids {
        assert_eq!(
            fx.queue_state(id, "triage").await.as_deref(),
            Some("pending")
        );
    }
}

// ---------------------------------------------------------------------------
// Recovering from a cursor the event log can no longer serve
// ---------------------------------------------------------------------------

#[tokio::test]
async fn drain_new_mail_resets_and_recovers_from_a_cursor_past_the_log() {
    // Before this fix, any cursor `EventLog::since` rejects with
    // `OutOfRange` (every value except exactly 0 — see that method's own
    // gap contract) propagated straight out of `drain_new_mail` and, since
    // nothing ever changes a rejected cursor, would wedge every later call
    // identically forever. A cursor ahead of the log (the branch exercised
    // here) and a cursor a quiet mailbox's retention has pruned past both
    // hit the same `OutOfRange` — this proves the recovery, not just one of
    // the two ways to trigger it.
    let fx = Fixture::open().await;
    let message_id = fx.sync_new_message().await;
    let loop_ = AiDispatchLoop::new(fx.events.clone(), fx.queue.clone(), no_handler_pool(&fx));

    let (enqueued, cursor) = loop_.drain_new_mail(999_999).await.unwrap();

    assert_eq!(
        enqueued, 1,
        "recovery must reset to cursor 0 and actually find the message that synced, \
         not just avoid erroring"
    );
    assert!(
        cursor > 0 && cursor < 999_999,
        "the returned cursor must land on the log's real position, not stay at the \
         unserviceable one: {cursor}"
    );
    assert_eq!(
        fx.queue_state(message_id, "triage").await.as_deref(),
        Some("pending")
    );
}

#[tokio::test]
async fn a_tick_with_an_unserviceable_cursor_still_dispatches_rather_than_wedging() {
    // The end-to-end proof: a whole `tick()` — not just `drain_new_mail` in
    // isolation — recovers and keeps running, which is what actually
    // matters for a long-lived daemon (this loop's own cursor is never
    // persisted, so every restart after a retention prune hits exactly this
    // path — see the module docs).
    let fx = Fixture::open().await;
    let message_id = fx.sync_new_message().await;
    let loop_ = AiDispatchLoop::new(fx.events.clone(), fx.queue.clone(), no_handler_pool(&fx));
    // Force the loop's internal cursor into the unserviceable range a
    // restart-after-prune would leave it in, without a public setter: two
    // ticks starting from 0 would just find the message normally, so this
    // reaches into the same atomic `tick()` itself uses.
    loop_
        .cursor
        .store(999_999, std::sync::atomic::Ordering::SeqCst);
    let cancel = CancellationToken::new();

    let report = loop_.tick(&cancel).await.unwrap();

    assert_eq!(
        report.new_mail_enqueued, 1,
        "the drain recovered within this same tick"
    );
    let dispatch = report
        .dispatch
        .expect("dispatch must still run in the same tick the drain recovered in");
    assert_eq!(
        dispatch.terminated, 1,
        "and it reached and processed the recovered job"
    );
    assert_eq!(
        fx.queue_state(message_id, "triage").await.as_deref(),
        Some("error"),
        "the specific message that synced is the one that got processed"
    );
}

// ---------------------------------------------------------------------------
// tick(): the full cycle, and the pause switch
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tick_drains_and_dispatches_in_one_call() {
    // With no handler registered the leased job is terminated immediately
    // (see `NeverCalledProvider`'s docs) -- this proves `tick` really does
    // reach the lease/dispatch stage after draining, not just that
    // `drain_new_mail` in isolation works.
    let fx = Fixture::open().await;
    let message_id = fx.sync_new_message().await;
    let loop_ = AiDispatchLoop::new(fx.events.clone(), fx.queue.clone(), no_handler_pool(&fx));
    let cancel = CancellationToken::new();

    let report = loop_.tick(&cancel).await.unwrap();

    assert_eq!(report.new_mail_enqueued, 1);
    let dispatch = report
        .dispatch
        .expect("a non-paused tick always dispatches");
    assert_eq!(
        dispatch.terminated, 1,
        "the job was leased and terminated for lacking a handler"
    );
    assert_eq!(
        fx.queue_state(message_id, "triage").await.as_deref(),
        Some("error"),
        "terminated jobs land in the `error` state, not `pending` or `dead`"
    );
}

#[tokio::test]
async fn a_paused_loop_does_nothing_at_all() {
    let fx = Fixture::open().await;
    let message_id = fx.sync_new_message().await;
    let loop_ = AiDispatchLoop::new(fx.events.clone(), fx.queue.clone(), no_handler_pool(&fx));
    loop_.pause_flag().set(true);
    let cancel = CancellationToken::new();

    let report = loop_.tick(&cancel).await.unwrap();

    assert_eq!(report.new_mail_enqueued, 0);
    assert!(report.dispatch.is_none());
    assert_eq!(
        fx.queue_state(message_id, "triage").await,
        None,
        "a paused loop must not even enqueue new work, let alone dispatch it"
    );
}

#[tokio::test]
async fn unpausing_lets_the_next_tick_catch_up() {
    let fx = Fixture::open().await;
    let message_id = fx.sync_new_message().await;
    let loop_ = AiDispatchLoop::new(fx.events.clone(), fx.queue.clone(), no_handler_pool(&fx));
    let flag = loop_.pause_flag();
    flag.set(true);
    let cancel = CancellationToken::new();

    assert!(loop_.tick(&cancel).await.unwrap().dispatch.is_none());
    flag.set(false);
    let report = loop_.tick(&cancel).await.unwrap();

    assert_eq!(report.new_mail_enqueued, 1);
    assert_eq!(
        fx.queue_state(message_id, "triage").await.as_deref(),
        Some("error"),
        "once unpaused, the previously-missed sync is picked up and dispatched"
    );
}

#[tokio::test]
async fn a_shared_pause_flag_is_observed_by_every_clone() {
    // What lets `rmaild::AiApi::SetPaused` and the spawned tick loop agree on
    // one switch: `with_pause_flag` hands the loop the *same* flag rather
    // than minting its own.
    let flag = AiPauseFlag::new(false);
    let fx = Fixture::open().await;
    let loop_ = AiDispatchLoop::new(fx.events.clone(), fx.queue.clone(), no_handler_pool(&fx))
        .with_pause_flag(flag.clone());

    assert!(!loop_.pause_flag().get());
    flag.set(true);
    assert!(
        loop_.pause_flag().get(),
        "the loop's own flag is the same shared handle, not a copy taken at construction"
    );
}

#[tokio::test]
async fn tick_never_blocks_longer_than_a_bounded_lease_limit_call() {
    // A smoke test that `tick` actually returns promptly against a real
    // (if tiny) database -- guards against an accidental infinite loop in
    // the drain's paging logic regressing silently.
    let fx = Fixture::open().await;
    let loop_ = AiDispatchLoop::new(fx.events.clone(), fx.queue.clone(), no_handler_pool(&fx));
    let cancel = CancellationToken::new();

    let result = tokio::time::timeout(Duration::from_secs(10), loop_.tick(&cancel)).await;
    assert!(result.is_ok(), "tick must not hang on an empty log/queue");
}
