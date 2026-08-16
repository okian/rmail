//! The daemon-side scheduler that makes the AI pipeline actually run.
//!
//! # The gap this module closes
//!
//! Tasks 47-49 built a durable queue ([`super::AiQueue`]), a live worker pool
//! ([`super::AiWorkerPool`]), a batch coordinator ([`super::BatchCoordinator`]),
//! and the two [`super::PassHandler`]s that answer to them — but nothing
//! called [`super::AiQueue::enqueue`] when a message actually synced, and
//! nothing called [`super::AiWorkerPool::dispatch_pending`] on any schedule.
//! Every unit test in `ai::queue`/`ai::triage`/`ai::deep` drives the pipeline
//! by hand, which is exactly how a production gap like this one hides: the
//! pipeline works perfectly whenever something remembers to call it, and
//! nothing did. [`AiDispatchLoop`] is that "something."
//!
//! # Why polling the durable log, not a live broadcast subscription
//!
//! [`crate::events::EventLog::catch_up`]/`subscribe` exist for exactly this
//! kind of consumer, and `rmaild::sync_service::SyncApi::watch_events` uses
//! them precisely because an interactive client is waiting on the other end
//! and every millisecond of latency is felt. This module's consumer is a
//! background scheduler with nobody waiting synchronously on it — the PRD's
//! own "AI runs off the sync/UI critical path" — so [`AiDispatchLoop::tick`]
//! instead re-reads [`crate::events::EventLog::since`] from its own cursor on
//! every tick. That trades a few seconds of worst-case latency (bounded by
//! [`DEFAULT_TICK_INTERVAL`]) for not having to hold a broadcast subscription
//! open across restarts, reconnects, and the lag-recovery machinery
//! `watch_events` needs to get right — the durability guarantee is identical
//! either way: [`crate::events::EventLog::since`]'s cursor always advances by
//! what it *scanned*, not merely what matched, so a quiet stretch of
//! non-`NewMail` events can never wedge the cursor.
//!
//! # Why the cursor starts at zero on every restart, not a persisted one
//!
//! [`AiDispatchLoop::cursor`] lives in memory only. A persisted cursor would
//! need its own durable slot (a migration this task does not otherwise need —
//! see the crate's own module docs on why a bare table/column earns its
//! keep). Starting from `0` instead means a restart re-scans the event log's
//! full retention window (`ai.batching`'s default keeps 7 days / 1M rows —
//! see `events::Retention`), which is bounded and, thanks to
//! [`super::AiQueue::enqueue`]'s own `(message_id, pass)` dedup, free for
//! every message that was already triaged: the re-scan produces the same
//! `INSERT ... ON CONFLICT DO NOTHING` no-op it would for a message this
//! process handled five minutes ago. The one real cost is a message that
//! synced, aged out of the event log's retention window, and *never* got
//! triaged (the daemon was down the whole time it was live in the log) —
//! that message needs an explicit `mail ai process` to catch up. Given the
//! default retention is measured in days, this is an acceptable trade for
//! not adding a migration this task does not otherwise require.
//!
//! # Batch bookkeeping lives here, not in `BatchCoordinator`
//!
//! [`super::BatchCoordinator::maybe_submit`] returns the id of whatever batch
//! it just submitted and then forgets about it — see that module's own docs
//! for why a coordinator's in-memory bookkeeping is scoped to redacted
//! payloads it needs to audit/rehydrate results, not to "which ids are still
//! outstanding." Tracking *that* is this module's job: [`AiDispatchLoop::tick`]
//! remembers every id [`super::BatchCoordinator::maybe_submit`] hands back and
//! polls each one on every subsequent tick until it reports
//! [`super::BatchPollOutcome::Completed`].

use std::fmt;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::error::{Error, ErrorReason};
use crate::events::{EventKind, EventLog};
use crate::notify;
use crate::tags;

use super::queue::{
    AiWorkerPool, BatchCoordinator, BatchPollOutcome, DispatchSummary, PRIORITY_BACKFILL,
};
use super::triage;
use super::{AiQueue, NewAiJob};

/// How many durable-log events one [`AiDispatchLoop::drain_new_mail`] page
/// reads at a time. Small enough that a single page never holds an initial
/// sync's worth of events in memory at once; large enough that catching up
/// after a restart does not cost one round trip per event.
const DRAIN_PAGE: i64 = 500;

/// Default interval between dispatch ticks. Short enough that a freshly
/// synced message is triaged within a few seconds (well inside "AI runs off
/// the critical path, TUI reads local results" — nothing here blocks on it);
/// long enough that an idle mailbox is not polling the database several
/// times a second for nothing.
pub const DEFAULT_TICK_INTERVAL: Duration = Duration::from_secs(5);

/// Default number of jobs [`AiWorkerPool::dispatch_pending`] is asked to
/// lease per tick.
pub const DEFAULT_LEASE_LIMIT: i64 = 32;

// ---------------------------------------------------------------------------
// Pause flag
// ---------------------------------------------------------------------------

/// A shared, cheap-to-clone on/off switch for the dispatch loop.
///
/// In-memory only — matches [`crate::sync::SyncEngine`]'s own per-account
/// pause flag (`AccountState.paused`, `rmail-core/src/sync/engine.rs`), which
/// is likewise never persisted: a pause is an operator's "stop spending right
/// now," not a durable policy, and a restarted daemon resuming un-paused is
/// the same convention sync already established rather than a new one
/// invented for AI.
#[derive(Clone)]
pub struct AiPauseFlag(Arc<std::sync::atomic::AtomicBool>);

impl fmt::Debug for AiPauseFlag {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("AiPauseFlag").field(&self.get()).finish()
    }
}

impl Default for AiPauseFlag {
    fn default() -> Self {
        Self::new(false)
    }
}

impl AiPauseFlag {
    /// A flag starting in the given state.
    #[must_use]
    pub fn new(paused: bool) -> Self {
        Self(Arc::new(std::sync::atomic::AtomicBool::new(paused)))
    }

    /// Whether the loop is currently paused.
    #[must_use]
    pub fn get(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// Set the paused state. Every clone of this flag observes the change —
    /// that sharing is the whole point, since `AiApi::SetPaused` (`rmaild`)
    /// and [`AiDispatchLoop`] hold independent clones of the same flag.
    pub fn set(&self, paused: bool) {
        self.0.store(paused, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------
// The loop
// ---------------------------------------------------------------------------

/// What one [`AiDispatchLoop::tick`] did — for logging and tests, not a
/// contract any caller needs to branch on.
#[derive(Debug, Clone, Default)]
pub struct TickReport {
    /// Triage jobs newly enqueued from freshly drained `NewMail` events.
    pub new_mail_enqueued: u64,
    /// The live worker pool's summary for this tick, `None` only when the
    /// tick was skipped entirely (paused).
    pub dispatch: Option<DispatchSummary>,
    /// New batch submissions started this tick.
    pub batches_submitted: usize,
    /// Previously-submitted batches that finished (and were resolved) this
    /// tick.
    pub batches_completed: usize,
}

/// Drives the whole AI pipeline on a schedule: enqueue triage jobs for
/// messages that synced, lease and run pending jobs, and keep the batch path
/// moving. One instance per daemon process.
///
/// Cheap to clone: every field is already `Clone` (an `EventLog`/`AiQueue`/
/// `AiWorkerPool` handle, or an `Arc`), the same "share by cloning" contract
/// every other long-lived handle in this crate follows.
#[derive(Clone)]
pub struct AiDispatchLoop {
    events: EventLog,
    queue: AiQueue,
    workers: AiWorkerPool,
    batch: Option<Arc<BatchCoordinator>>,
    /// Which passes [`BatchCoordinator::maybe_submit`] is polled for each
    /// tick. Not derived from the worker pool's own registered handlers —
    /// batching is opt-in per pass at the config level (`ai.batching`), and
    /// this loop does not reach into the pool's private handler map to guess.
    batch_passes: Vec<String>,
    paused: AiPauseFlag,
    tick_interval: Duration,
    lease_limit: i64,
    cursor: Arc<AtomicI64>,
    /// Batch ids [`BatchCoordinator::maybe_submit`] has handed back that have
    /// not yet been resolved by [`BatchCoordinator::poll`] — see the module
    /// docs' "Batch bookkeeping lives here."
    active_batches: Arc<Mutex<Vec<String>>>,
    /// Whether a `NewMail` event also enqueues a
    /// [`crate::notify::PASS`] job alongside triage's — `notify.enabled`.
    /// See [`Self::drain_new_mail`] for why the switch lives at the enqueue
    /// site rather than in the handler.
    notify_pass: bool,
    /// Whether a `NewMail` event also enqueues a
    /// [`crate::tags::ai::PASS`] job — `tags.ai.enabled` *and*
    /// `tags.ai.suggest_on_new_mail`. Same reasoning as `notify_pass`: the
    /// switch belongs at the enqueue site, because a job enqueued for a
    /// disabled feature would still occupy `ai_queue` and
    /// [`AiQueue::enqueue`]'s `(message_id, pass)` dedup would make that
    /// permanent.
    suggest_tags_pass: bool,
}

impl fmt::Debug for AiDispatchLoop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AiDispatchLoop")
            .field("batch_passes", &self.batch_passes)
            .field("tick_interval", &self.tick_interval)
            .field("lease_limit", &self.lease_limit)
            .field("paused", &self.paused.get())
            .finish_non_exhaustive()
    }
}

impl AiDispatchLoop {
    /// A loop over the live path only — no batch coordinator. Use
    /// [`Self::with_batch`] to also drive the batch path.
    #[must_use]
    pub fn new(events: EventLog, queue: AiQueue, workers: AiWorkerPool) -> Self {
        Self {
            events,
            queue,
            workers,
            batch: None,
            batch_passes: Vec::new(),
            paused: AiPauseFlag::default(),
            tick_interval: DEFAULT_TICK_INTERVAL,
            lease_limit: DEFAULT_LEASE_LIMIT,
            cursor: Arc::new(AtomicI64::new(0)),
            active_batches: Arc::new(Mutex::new(Vec::new())),
            notify_pass: false,
            suggest_tags_pass: false,
        }
    }

    /// Also enqueue a [`crate::notify`] scoring job for every `NewMail`
    /// event — what `notify.enabled` turns on.
    #[must_use]
    pub fn with_notify_pass(mut self, enabled: bool) -> Self {
        self.notify_pass = enabled;
        self
    }

    /// Also enqueue a [`crate::tags::ai`] auto-tagging job for every
    /// `NewMail` event — what `tags.ai.enabled && tags.ai.suggest_on_new_mail`
    /// turn on.
    #[must_use]
    pub fn with_suggest_tags_pass(mut self, enabled: bool) -> Self {
        self.suggest_tags_pass = enabled;
        self
    }

    /// Also drive the batch path: [`BatchCoordinator::maybe_submit`] for each
    /// of `passes`, and [`BatchCoordinator::poll`] every submission this loop
    /// has not yet resolved, on every tick.
    #[must_use]
    pub fn with_batch(mut self, batch: Arc<BatchCoordinator>, passes: Vec<String>) -> Self {
        self.batch = Some(batch);
        self.batch_passes = passes;
        self
    }

    /// Override the default tick interval.
    #[must_use]
    pub fn with_tick_interval(mut self, interval: Duration) -> Self {
        self.tick_interval = interval;
        self
    }

    /// Override the default per-tick lease limit.
    #[must_use]
    pub fn with_lease_limit(mut self, limit: i64) -> Self {
        self.lease_limit = limit;
        self
    }

    /// Share an existing pause flag rather than this loop's own fresh one —
    /// what `rmaild` uses so `AiApi::SetPaused` and this loop observe the
    /// same switch.
    #[must_use]
    pub fn with_pause_flag(mut self, flag: AiPauseFlag) -> Self {
        self.paused = flag;
        self
    }

    /// This loop's pause flag, for a caller (`rmaild::AiApi`) that needs to
    /// flip it from a different task than the one running [`Self::spawn`].
    #[must_use]
    pub fn pause_flag(&self) -> AiPauseFlag {
        self.paused.clone()
    }

    /// Enqueue a triage job for every `NewMail` event durably logged after
    /// `since` (exclusive) — the wiring the module docs describe: "the sync
    /// engine emits `NewMessage`... AI Queue" made real, in one direction
    /// only (this never removes or completes a job, only adds one).
    ///
    /// Returns how many jobs were newly enqueued (already-queued messages
    /// dedup for free via [`AiQueue::enqueue`]) and the cursor to resume
    /// from on the next call — always the *scanned* position, per
    /// [`crate::events::EventLog::since`]'s own contract, so a stretch of
    /// events with no `NewMail` in it still advances the cursor rather than
    /// being re-read forever.
    ///
    /// # Retention gaps self-heal rather than wedging the loop
    ///
    /// `since`/`self.cursor` is never persisted (see the module docs), which
    /// means it can legitimately fall behind what [`crate::events::EventLog`]
    /// still retains: a quiet mailbox lets age-based retention prune the
    /// *entire* log out from under a cursor that stopped advancing, or a
    /// restart resumes from `0` and only reaches, say, seq 500 before the
    /// next call finds retention's floor has moved past it. Either way
    /// [`crate::events::EventLog::since`] answers with
    /// [`crate::error::ErrorReason::OutOfRange`] — and *every* cursor value
    /// except exactly `0` can be rejected this way, per that method's own
    /// gap contract. Treating that as a fatal error here would wedge this
    /// method — and, through it, [`Self::tick`] and every dispatch/batch
    /// cycle after it, forever, since nothing ever changes `since` again —
    /// on the first ordinary quiet stretch a long-running daemon hits. `0`
    /// is documented as the one cursor `since` can never reject, so this
    /// resets to it and retries instead: safe, because every `NewMail`
    /// event this loop already turned into a queue row before the gap is a
    /// no-op through [`AiQueue::enqueue`]'s own dedup, and the module docs
    /// already accept the cost of a full retention-window rescan as the
    /// price of not persisting a cursor.
    ///
    /// # Errors
    /// A mapped storage error from reading the event log or writing the
    /// queue — never [`crate::error::ErrorReason::OutOfRange`], which this
    /// method recovers from internally rather than surfacing.
    #[tracing::instrument(skip(self), fields(since, enqueued, next_cursor))]
    pub async fn drain_new_mail(&self, since: i64) -> Result<(u64, i64), Error> {
        let mut cursor = since;
        let mut enqueued = 0u64;
        loop {
            let page = match self.events.since(cursor, DRAIN_PAGE).await {
                Ok(page) => page,
                Err(error) if error.reason() == ErrorReason::OutOfRange && cursor != 0 => {
                    tracing::warn!(
                        cursor,
                        %error,
                        "ai dispatch cursor fell behind the event log's retention window; \
                         resuming from 0 rather than wedging (already-triaged messages are a \
                         no-op through AiQueue::enqueue's own dedup)"
                    );
                    cursor = 0;
                    continue;
                }
                Err(error) => return Err(error),
            };
            let got = i64::try_from(page.events.len()).unwrap_or(i64::MAX);
            // One transaction for the whole page, not one per event: at
            // restart (or after the reset above) this loop may be replaying
            // a retention window's worth of history, and `AiQueue::enqueue`
            // already accepts a `Vec` precisely so a page costs one write
            // lock and one commit rather than stalling every other writer
            // (sync, indexing) behind thousands of them.
            let jobs: Vec<NewAiJob> = page
                .events
                .iter()
                .filter(|event| event.kind == EventKind::NewMail)
                .filter_map(|event| {
                    let (Some(message_id), Some(account_id)) = (event.message_id, event.account_id)
                    else {
                        // A malformed `NewMail` event with no message/account
                        // scope cannot happen from `sync::engine::LogSink`
                        // (it always sets both) — skipped rather than
                        // failing the whole drain over a row nothing in this
                        // codebase writes.
                        return None;
                    };
                    Some((message_id, account_id))
                })
                .flat_map(|(message_id, account_id)| {
                    // Triage always; notification scoring only when the
                    // operator turned it on. `notify.enabled` is off by
                    // default (see `config::NotifyConfig::enabled`) precisely
                    // because this is where its cost is incurred — one extra
                    // Haiku call per newly synced message, on top of triage's.
                    // Gating it here rather than inside the handler is what
                    // keeps a disabled engine from queueing work it will
                    // never run: a job enqueued and then declined would still
                    // occupy `ai_queue`, and `AiQueue::enqueue`'s
                    // `(message_id, pass)` dedup would make it permanent.
                    let mut jobs = vec![NewAiJob::new(message_id, account_id, triage::PASS)];
                    if self.notify_pass {
                        jobs.push(NewAiJob::new(message_id, account_id, notify::PASS));
                    }
                    // Auto-tagging (task 57) is enqueued at
                    // `PRIORITY_BACKFILL`, not `PRIORITY_NORMAL`, because
                    // prd.md asks for a *low-priority* `suggest_tags` job and
                    // that word carries two consequences here, not one: the
                    // queue orders it behind every triage and notification
                    // job (nobody is waiting on a tag chip the way they are
                    // on a triage verdict), and `budget::WorkClass::for_priority`
                    // classifies it as `Bulk`, so it draws on the bulk
                    // sub-budget that exists precisely to keep background
                    // walks from starving user-facing calls.
                    if self.suggest_tags_pass {
                        jobs.push(
                            NewAiJob::new(message_id, account_id, tags::ai::PASS)
                                .priority(PRIORITY_BACKFILL),
                        );
                    }
                    jobs
                })
                .collect();
            if !jobs.is_empty() {
                match self.queue.enqueue(jobs).await {
                    Ok(n) => enqueued += n,
                    Err(error) => tracing::warn!(
                        %error,
                        cursor,
                        next_seq = page.next_seq,
                        "failed to enqueue this page's triage jobs; the cursor still advances \
                         past it (see this method's own docs on why that is safe), so these \
                         particular NewMail events are not retried by this loop — a full \
                         `mail ai process` backfill is the recovery path for a persistent \
                         failure here"
                    ),
                }
            }
            cursor = page.next_seq;
            if got < DRAIN_PAGE {
                break;
            }
        }
        let span = tracing::Span::current();
        span.record("enqueued", enqueued);
        span.record("next_cursor", cursor);
        Ok((enqueued, cursor))
    }

    /// One dispatch cycle: drain newly synced mail into the queue, run the
    /// live worker pool over whatever is pending, then advance the batch
    /// path. Errors from any one stage are logged and do not stop the
    /// others — a batch-endpoint hiccup must not also stall live dispatch,
    /// and vice versa.
    ///
    /// A paused loop skips every stage, including the drain — `mail ai
    /// pause` means "stop doing AI work," and a paused daemon that kept
    /// silently enqueueing jobs behind the operator's back would spend the
    /// moment it was unpaused catching up on a backlog the operator never
    /// asked it to build.
    ///
    /// # Errors
    /// Only ever [`AiWorkerPool::dispatch_pending`]'s own error — the drain
    /// and every batch step are caught and logged internally (the drain's
    /// own [`Self::drain_new_mail`] already recovers from a retention gap
    /// rather than erroring at all; anything else it returns is logged here
    /// and this cycle still proceeds to dispatch/batch on whatever the
    /// cursor already was) so one bad stage never stops the others within
    /// the same tick, nor aborts the loop that would otherwise recover on
    /// the next one.
    #[tracing::instrument(skip(self, cancel))]
    pub async fn tick(&self, cancel: &CancellationToken) -> Result<TickReport, Error> {
        if self.paused.get() {
            return Ok(TickReport::default());
        }

        let since = self.cursor.load(Ordering::SeqCst);
        let new_mail_enqueued = match self.drain_new_mail(since).await {
            Ok((enqueued, next_cursor)) => {
                self.cursor.store(next_cursor, Ordering::SeqCst);
                enqueued
            }
            Err(error) => {
                // Reaching here at all means `drain_new_mail` hit something
                // other than the retention gap it already self-heals from
                // (a storage hiccup, most likely) — logged, cursor left
                // exactly where it was so the next tick simply retries the
                // same range, and this cycle still runs dispatch/batch on
                // whatever was already queued rather than skipping a whole
                // tick's worth of work over an unrelated read failure.
                tracing::warn!(%error, since, "ai dispatch: draining new mail failed this tick");
                0
            }
        };

        let dispatch = self
            .workers
            .dispatch_pending(self.lease_limit, cancel)
            .await?;

        let mut batches_submitted = 0usize;
        let mut batches_completed = 0usize;
        if let Some(batch) = &self.batch {
            for pass in &self.batch_passes {
                match batch.maybe_submit(pass).await {
                    Ok(Some(id)) => {
                        self.active_batches
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .push(id);
                        batches_submitted += 1;
                    }
                    Ok(None) => {}
                    Err(error) => {
                        tracing::warn!(%error, pass, "ai batch submission failed this tick");
                    }
                }
            }

            let ids: Vec<String> = self
                .active_batches
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            let mut still_running = Vec::with_capacity(ids.len());
            for id in ids {
                match batch.poll(&id).await {
                    Ok(BatchPollOutcome::Completed(summary)) => {
                        batches_completed += 1;
                        tracing::info!(batch_id = %id, ?summary, "ai batch completed");
                    }
                    Ok(BatchPollOutcome::StillRunning) => still_running.push(id),
                    Err(error) => {
                        tracing::warn!(
                            %error,
                            batch_id = %id,
                            "ai batch poll failed this tick; will retry next tick"
                        );
                        still_running.push(id);
                    }
                }
            }
            *self
                .active_batches
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = still_running;
        }

        Ok(TickReport {
            new_mail_enqueued,
            dispatch: Some(dispatch),
            batches_submitted,
            batches_completed,
        })
    }

    /// Spawn the periodic tick loop, running once immediately (so a daemon
    /// restarted more often than [`Self::tick_interval`] still ever makes
    /// progress — the same reasoning `rmaild`'s event-log pruner task
    /// applies to itself) and then on `tick_interval`, until `cancel` fires.
    pub fn spawn(self, cancel: CancellationToken) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                match self.tick(&cancel).await {
                    Ok(report) => tracing::debug!(?report, "ai dispatch tick"),
                    Err(error) => tracing::warn!(%error, "ai dispatch tick failed"),
                }
                tokio::select! {
                    () = cancel.cancelled() => return,
                    () = tokio::time::sleep(self.tick_interval) => {}
                }
            }
        })
    }
}

#[cfg(test)]
mod tests;
