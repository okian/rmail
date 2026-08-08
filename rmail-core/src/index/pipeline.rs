//! The thing that actually runs index jobs.
//!
//! Tasks 16-21 built a durable queue and four stages that know how to index one
//! message each. Nothing called them. [`IndexPipeline`] is the missing middle:
//! it leases jobs, dispatches each to the stage that owns it, and reports the
//! outcome back to the queue. [`IndexLoop`] is the schedule that keeps it
//! running, and the only thing in the process that turns "a message synced"
//! into "a message is searchable."
//!
//! # A disabled stage retires its jobs, it does not fake them
//!
//! [`crate::index::extract_message`] enqueues the lexical, entity and semantic
//! stages unconditionally — it cannot know which of them the operator switched
//! off. A daemon with `[index.semantic] enabled = false` therefore accumulates
//! semantic jobs that no worker will ever run. Leaving them pending grows the
//! queue without bound; completing them writes an `index_state` row claiming
//! the message was embedded, and `mail index status` then reports 100%
//! semantic coverage for a stage that has never once run. Neither is
//! acceptable, so a disabled stage's job is *discarded*
//! ([`crate::index::IndexQueue::discard`]): out of the queue, no state row, and
//! coverage keeps telling the truth.
//!
//! # Cancellation returns leases rather than stranding them
//!
//! A drain that stops mid-batch — because the client streaming its progress
//! disconnected, or the daemon is shutting down — hands its unrun leases back
//! with [`crate::index::IndexQueue::release`] instead of letting them lapse.
//! Lapsing works (the reaper exists precisely for the crash case) but it takes
//! a lease's worth of minutes, during which the next drain sees a queue that
//! looks busy and is not. Releasing also rolls the attempt back, because a job
//! that was never run has not failed and must not be charged for it.
//!
//! # Why the loop polls the durable log rather than subscribing
//!
//! Same reasoning [`crate::ai::AiDispatchLoop`] gives for the identical choice:
//! this is a background scheduler with nobody waiting synchronously on it, so
//! re-reading [`crate::events::EventLog::since`] from an in-memory cursor
//! trades a few seconds of worst-case latency for not having to hold a
//! broadcast subscription open across restarts and lag recovery. Re-scanning
//! the retention window after a restart is free: every message already indexed
//! dedups away inside [`crate::index::IndexQueue::enqueue`].

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::config::IndexConfig;
use crate::error::{Error, ErrorReason};
use crate::events::{EventKind, EventLog};
use crate::index::fts::FtsIndex;
use crate::index::semantic::SemanticIndex;
use crate::index::{entities, extract, IndexKind, IndexQueue, Lease, NewJob, PRIORITY_RECENT};
use crate::storage::Database;

/// How many durable-log events one [`IndexLoop::drain_new_mail`] page reads.
const DRAIN_PAGE: i64 = 500;

/// Default interval between worker ticks.
///
/// The PRD's target is "new mail lexically indexed < 2 s after sync", and the
/// tick is the dominant term in that budget — a message enqueued just after a
/// tick waits a whole interval before anything looks at it. Two seconds leaves
/// room for the extract itself while keeping an idle mailbox from polling the
/// database several times a second for nothing.
pub const DEFAULT_TICK_INTERVAL: Duration = Duration::from_secs(2);

/// Default number of jobs leased per batch.
///
/// A batch is the unit of cancellation granularity and of progress reporting,
/// so it is deliberately small: a client that drops a `Reindex` stream should
/// see the work stop within one batch, not one page.
pub const DEFAULT_LEASE_LIMIT: i64 = 16;

// ---------------------------------------------------------------------------
// Pause flag
// ---------------------------------------------------------------------------

/// A shared, cheap-to-clone on/off switch for the background indexer.
///
/// In-memory only, matching [`crate::ai::AiPauseFlag`] and
/// [`crate::sync::SyncEngine`]'s own per-account flag: a pause is an operator's
/// "stop working right now," not a durable policy, and a restarted daemon
/// resuming un-paused is the convention this codebase already established
/// rather than a new one invented here.
#[derive(Clone)]
pub struct IndexPauseFlag(Arc<AtomicBool>);

impl std::fmt::Debug for IndexPauseFlag {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("IndexPauseFlag").field(&self.get()).finish()
    }
}

impl Default for IndexPauseFlag {
    fn default() -> Self {
        Self::new(false)
    }
}

impl IndexPauseFlag {
    /// A flag starting in the given state.
    #[must_use]
    pub fn new(paused: bool) -> Self {
        Self(Arc::new(AtomicBool::new(paused)))
    }

    /// Whether the background worker is stopped.
    #[must_use]
    pub fn get(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    /// Set the paused state. Every clone observes it — that sharing is the
    /// whole point, since `rmaild`'s `IndexService.SetPaused` handler and
    /// [`IndexLoop`] hold independent clones of the same flag.
    pub fn set(&self, paused: bool) {
        self.0.store(paused, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------
// Stage switches
// ---------------------------------------------------------------------------

/// Which stages are switched on in config.
///
/// Extraction has no switch of its own and is always on: it is the substrate
/// every other stage reads, and `[index] enabled = false` means "do not run the
/// background worker," not "extraction is meaningless." An operator who asks
/// for a drain explicitly gets one either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StageSwitches {
    /// `[index.lexical] enabled`.
    pub lexical: bool,
    /// `[index.entities] enabled`.
    pub entities: bool,
    /// `[index.semantic] enabled`.
    pub semantic: bool,
}

impl Default for StageSwitches {
    fn default() -> Self {
        Self {
            lexical: true,
            entities: true,
            semantic: true,
        }
    }
}

impl StageSwitches {
    /// Read the three per-stage switches out of `[index]`.
    #[must_use]
    pub fn from_config(config: &IndexConfig) -> Self {
        Self {
            lexical: config.lexical.enabled,
            entities: config.entities.enabled,
            semantic: config.semantic.enabled,
        }
    }

    /// Whether `kind` will actually do work.
    ///
    /// [`IndexKind::Thread`] is reported off because this build has no thread
    /// rollup stage — see [`IndexKind::PER_MESSAGE`]'s own docs for why the
    /// PRD's `thread_index` is already covered by the threading subsystem.
    #[must_use]
    pub fn enabled(self, kind: IndexKind) -> bool {
        match kind {
            IndexKind::Extract => true,
            IndexKind::Lexical => self.lexical,
            IndexKind::Entities => self.entities,
            IndexKind::Semantic => self.semantic,
            IndexKind::Thread => false,
        }
    }
}

// ---------------------------------------------------------------------------
// Drain
// ---------------------------------------------------------------------------

/// What one batch of leased work did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DrainReport {
    /// Jobs leased this pass.
    pub leased: u64,
    /// Jobs that ran and recorded state.
    pub completed: u64,
    /// Jobs retired without recording state — a disabled stage, or a message
    /// that disappeared between enqueue and lease.
    pub discarded: u64,
    /// Jobs that failed and were backed off or quarantined.
    pub failed: u64,
    /// Leases handed back unrun because the pass was cancelled.
    pub released: u64,
    /// Lapsed leases the reaper returned to the queue before this pass.
    pub reclaimed: u64,
}

impl DrainReport {
    /// Jobs this pass took off the queue for good.
    #[must_use]
    pub fn retired(&self) -> u64 {
        self.completed + self.discarded
    }

    fn merge(&mut self, other: Self) {
        self.leased += other.leased;
        self.completed += other.completed;
        self.discarded += other.discarded;
        self.failed += other.failed;
        self.released += other.released;
        self.reclaimed += other.reclaimed;
    }
}

/// What one stage did with a message.
enum StageOutcome {
    /// Work happened; record it in `index_state`.
    Indexed,
    /// Nothing to do, and nothing to record. The job leaves the queue without
    /// claiming the message was indexed.
    Retire,
}

/// Leases index jobs and runs the stage each one names.
///
/// Cheap to clone: every field is a handle over one database or an `Arc`.
#[derive(Clone)]
pub struct IndexPipeline {
    db: Database,
    queue: IndexQueue,
    fts: FtsIndex,
    semantic: SemanticIndex,
    switches: StageSwitches,
    paused: IndexPauseFlag,
    worker: String,
    /// Jobs retired since this pipeline was built. Exposed so a caller — and a
    /// test asserting that a cancelled drain actually stopped — can tell "the
    /// worker is idle because there is nothing to do" from "the worker is idle
    /// because it stopped."
    ran: Arc<AtomicU64>,
}

impl std::fmt::Debug for IndexPipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexPipeline")
            .field("switches", &self.switches)
            .field("worker", &self.worker)
            .field("paused", &self.paused.get())
            .field("ran", &self.jobs_run())
            .finish_non_exhaustive()
    }
}

impl IndexPipeline {
    /// Build a pipeline over the stages it drives.
    ///
    /// `fts` and `semantic` are passed in rather than constructed here because
    /// the daemon already builds both for `SearchService` — a second
    /// [`SemanticIndex`] in particular would mean a second `Arc<dyn Embedder>`
    /// and, for a real ONNX backend, a second copy of the weights.
    #[must_use]
    pub fn new(
        db: Database,
        queue: IndexQueue,
        fts: FtsIndex,
        semantic: SemanticIndex,
        config: &IndexConfig,
    ) -> Self {
        Self {
            db,
            queue,
            fts,
            semantic,
            switches: StageSwitches::from_config(config),
            paused: IndexPauseFlag::default(),
            worker: format!("rmaild-index-{}", std::process::id()),
            ran: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Share an existing pause flag rather than this pipeline's own.
    #[must_use]
    pub fn with_pause_flag(mut self, flag: IndexPauseFlag) -> Self {
        self.paused = flag;
        self
    }

    /// Override the worker name recorded on leases. Tests use it to prove a
    /// lease belongs to the worker that took it.
    #[must_use]
    pub fn with_worker(mut self, worker: impl Into<String>) -> Self {
        self.worker = worker.into();
        self
    }

    /// This pipeline's pause flag, for a caller that flips it from another
    /// task.
    #[must_use]
    pub fn pause_flag(&self) -> IndexPauseFlag {
        self.paused.clone()
    }

    /// The queue this pipeline drains.
    #[must_use]
    pub fn queue(&self) -> &IndexQueue {
        &self.queue
    }

    /// Which stages are switched on.
    #[must_use]
    pub fn switches(&self) -> StageSwitches {
        self.switches
    }

    /// How many jobs this pipeline has retired since it was built.
    #[must_use]
    pub fn jobs_run(&self) -> u64 {
        self.ran.load(Ordering::SeqCst)
    }

    /// Lease up to `limit` jobs and run them.
    ///
    /// Returns an empty report when the queue has nothing ready, which is how a
    /// caller knows a drain is finished. Reaps lapsed leases first, so a
    /// daemon restarted mid-index picks its own abandoned work back up on the
    /// first pass rather than waiting for a separate reaper schedule.
    ///
    /// Cancellation is honored between jobs: the remaining leases are handed
    /// straight back rather than left to lapse.
    ///
    /// # Errors
    ///
    /// A mapped storage error from the queue itself. A *stage* failing is not
    /// an error here — that is what the queue's backoff and quarantine are for,
    /// and one poison message must not stop the batch around it.
    #[tracing::instrument(skip(self, cancel), fields(leased, completed, failed))]
    pub async fn run_once(
        &self,
        limit: i64,
        cancel: &CancellationToken,
    ) -> Result<DrainReport, Error> {
        let mut report = DrainReport::default();
        if cancel.is_cancelled() {
            return Ok(report);
        }
        report.reclaimed = self.queue.reap_expired().await?;

        let leases = self.queue.lease(&self.worker, limit).await?;
        report.leased = leases.len() as u64;

        for (at, lease) in leases.iter().enumerate() {
            if cancel.is_cancelled() {
                self.release_all(&leases[at..], &mut report).await;
                break;
            }
            // A stage failing is not an error here — that is what the queue's
            // backoff is for. Reaching this arm means the *queue itself* failed,
            // and the leases this pass has not run yet must go back with the
            // same care cancellation gives them: without this they sit leased
            // for the full expiry, and the next drain sees a queue that looks
            // busy and is not.
            if let Err(error) = self.run_lease(lease, &mut report).await {
                self.release_all(&leases[at + 1..], &mut report).await;
                return Err(error);
            }
        }

        let span = tracing::Span::current();
        span.record("leased", report.leased);
        span.record("completed", report.completed);
        span.record("failed", report.failed);
        Ok(report)
    }

    /// Drain until the queue is empty, `max_jobs` have run, or `cancel` fires,
    /// awaiting `on_batch` between batches with the running total and how much
    /// is still outstanding.
    ///
    /// `max_jobs` of 0 means "until the queue is empty". `on_batch` is where a
    /// streaming RPC turns batches into progress frames, and returning
    /// [`std::ops::ControlFlow::Break`] from it stops the drain — which is what
    /// a disconnected client looks like from in here.
    ///
    /// It returns a future rather than a plain value on purpose. The one caller
    /// that matters sends on a bounded channel, and a synchronous callback
    /// could only ever try-send: a client that stopped reading would then have
    /// its frames silently dropped while the indexing it walked away from ran
    /// to completion. Awaiting makes "the client stopped reading" and "stop the
    /// work" the same event, which is the contract `Reindex` advertises.
    ///
    /// The outstanding count is handed over rather than left for the callback
    /// to fetch because this method has already paid for the read, and a
    /// progress frame with a fabricated "remaining" is worse than none.
    ///
    /// # Errors
    ///
    /// As [`Self::run_once`].
    pub async fn drain<F, Fut>(
        &self,
        limit: i64,
        max_jobs: u64,
        cancel: &CancellationToken,
        mut on_batch: F,
    ) -> Result<DrainReport, Error>
    where
        F: FnMut(DrainReport, i64) -> Fut,
        Fut: std::future::Future<Output = std::ops::ControlFlow<()>>,
    {
        let mut total = DrainReport::default();
        loop {
            let budget = if max_jobs == 0 {
                limit
            } else {
                let left = max_jobs.saturating_sub(total.retired());
                if left == 0 {
                    return Ok(total);
                }
                limit.min(i64::try_from(left).unwrap_or(i64::MAX))
            };
            let batch = self.run_once(budget, cancel).await?;
            let leased = batch.leased;
            total.merge(batch);
            let outstanding = self.queue.outstanding().await?;
            if on_batch(total, outstanding).await.is_break() {
                return Ok(total);
            }
            // An empty lease is the only reliable "nothing left": the queue can
            // hold jobs that are backing off or held by another worker, and
            // neither is this drain's to wait for.
            if leased == 0 || cancel.is_cancelled() {
                return Ok(total);
            }
        }
    }

    /// Hand back leases this pass will not run.
    ///
    /// Best effort: a release that fails leaves the lease to lapse, which is
    /// the crash path and already correct. It must not turn a clean stop — or
    /// an unrelated error on its way out — into a second failure.
    async fn release_all(&self, unrun: &[Lease], report: &mut DrainReport) {
        for lease in unrun {
            match self.queue.release(lease).await {
                Ok(_) => report.released += 1,
                Err(error) => tracing::warn!(
                    %error,
                    job_id = lease.job_id,
                    "could not return a lease; it will lapse instead"
                ),
            }
        }
    }

    /// Run one leased job and tell the queue what happened.
    async fn run_lease(&self, lease: &Lease, report: &mut DrainReport) -> Result<(), Error> {
        // Only the semantic stage records a model; `complete` filters it by
        // `IndexKind::uses_model`, but passing the configured one for every
        // stage keeps the decision in exactly one place.
        let model = self.semantic.model().to_owned();
        match self.run_stage(lease).await {
            Ok(StageOutcome::Indexed) => {
                if self.queue.complete(lease, Some(&model)).await? {
                    report.completed += 1;
                    self.ran.fetch_add(1, Ordering::SeqCst);
                }
            }
            Ok(StageOutcome::Retire) => {
                if self.queue.discard(lease).await? {
                    report.discarded += 1;
                    self.ran.fetch_add(1, Ordering::SeqCst);
                }
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    job_id = lease.job_id,
                    message_id = lease.message_id,
                    kind = lease.kind.as_str(),
                    "index stage failed"
                );
                self.queue.fail(lease, &error.to_string()).await?;
                report.failed += 1;
                self.ran.fetch_add(1, Ordering::SeqCst);
            }
        }
        Ok(())
    }

    /// Dispatch one job to the stage that owns it.
    async fn run_stage(&self, lease: &Lease) -> Result<StageOutcome, Error> {
        if !self.switches.enabled(lease.kind) {
            tracing::debug!(
                kind = lease.kind.as_str(),
                message_id = lease.message_id,
                "retiring a job for a stage that is switched off; no index state is recorded, \
                 so coverage keeps reporting this stage as unindexed"
            );
            return Ok(StageOutcome::Retire);
        }
        let message_id = lease.message_id;
        let outcome = match lease.kind {
            IndexKind::Extract => extract::run_job(&self.db, &self.queue, lease)
                .await
                .map(|_| StageOutcome::Indexed),
            IndexKind::Lexical => self
                .fts
                .index_message(message_id)
                .await
                .map(|_| StageOutcome::Indexed),
            IndexKind::Entities => entities::extract_entities(&self.db, message_id)
                .await
                .map(|_| StageOutcome::Indexed),
            IndexKind::Semantic => self
                .semantic
                .index_message(message_id)
                .await
                .map(|_| StageOutcome::Indexed),
            // Unreachable while `StageSwitches::enabled` reports Thread off,
            // and kept as a match arm rather than a catch-all so adding a stage
            // is a compile error here rather than a silently retired job.
            IndexKind::Thread => Ok(StageOutcome::Retire),
        };
        match outcome {
            // Sync and indexing race: a message deleted between the enqueue and
            // the lease is not a failure, and retrying it four more times
            // before quarantining it would turn an ordinary deletion into a
            // dead letter an operator has to look at.
            Err(error) if error.reason() == ErrorReason::NotFound => {
                tracing::debug!(
                    message_id,
                    kind = lease.kind.as_str(),
                    "the message this job names is gone; retiring the job"
                );
                Ok(StageOutcome::Retire)
            }
            other => other,
        }
    }
}

// ---------------------------------------------------------------------------
// The background loop
// ---------------------------------------------------------------------------

/// What one [`IndexLoop::tick`] did.
#[derive(Debug, Clone, Copy, Default)]
pub struct TickReport {
    /// Extract jobs newly enqueued from freshly drained `NewMail` events.
    pub enqueued: u64,
    /// What the drain did. `None` when the tick was skipped because the worker
    /// is paused.
    pub drain: Option<DrainReport>,
}

/// Turns "a message synced" into "a message is indexed", on a schedule.
///
/// One instance per daemon process. Cheap to clone.
#[derive(Clone)]
pub struct IndexLoop {
    events: EventLog,
    pipeline: IndexPipeline,
    tick_interval: Duration,
    lease_limit: i64,
    /// Position in the durable event log. In memory only, for the reasons
    /// [`crate::ai::AiDispatchLoop`] documents at length: a persisted cursor
    /// would need a migration, and a full retention-window rescan after a
    /// restart is free through the queue's own dedup.
    cursor: Arc<AtomicI64>,
}

impl std::fmt::Debug for IndexLoop {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexLoop")
            .field("tick_interval", &self.tick_interval)
            .field("lease_limit", &self.lease_limit)
            .field("cursor", &self.cursor.load(Ordering::SeqCst))
            .finish_non_exhaustive()
    }
}

impl IndexLoop {
    /// A loop over `pipeline`, fed by `events`.
    #[must_use]
    pub fn new(events: EventLog, pipeline: IndexPipeline) -> Self {
        Self {
            events,
            pipeline,
            tick_interval: DEFAULT_TICK_INTERVAL,
            lease_limit: DEFAULT_LEASE_LIMIT,
            cursor: Arc::new(AtomicI64::new(0)),
        }
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

    /// The pipeline this loop drives.
    #[must_use]
    pub fn pipeline(&self) -> &IndexPipeline {
        &self.pipeline
    }

    /// Enqueue an extract job for every `NewMail` event logged after `since`
    /// (exclusive).
    ///
    /// Returns how many jobs were newly enqueued and the cursor to resume from
    /// — always the *scanned* position, so a stretch of events with no
    /// `NewMail` in it advances the cursor rather than being re-read forever.
    ///
    /// Freshly synced mail is enqueued at [`PRIORITY_RECENT`]: it is, by
    /// definition, the mail a user is most likely about to search for, and the
    /// PRD's "recent/inbox mail is indexed first so results are useful early"
    /// is exactly this ordering. The backlog walk that `mail index reindex`
    /// drives enqueues at a lower priority and therefore runs behind it.
    ///
    /// A cursor that has fallen behind the log's retention window resets to
    /// `0` rather than wedging the loop, for the reasons
    /// [`crate::ai::AiDispatchLoop::drain_new_mail`] documents: `0` is the one
    /// cursor the log can never reject, and re-scanning costs nothing because
    /// every already-indexed message dedups away.
    ///
    /// # Errors
    ///
    /// A mapped storage error from reading the log or writing the queue.
    #[tracing::instrument(skip(self), fields(enqueued, next_cursor))]
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
                        "index cursor fell behind the event log's retention window; resuming \
                         from 0 rather than wedging (already-indexed messages dedup away)"
                    );
                    cursor = 0;
                    continue;
                }
                Err(error) => return Err(error),
            };
            let got = i64::try_from(page.events.len()).unwrap_or(i64::MAX);
            let jobs: Vec<NewJob> = page
                .events
                .iter()
                .filter(|event| event.kind == EventKind::NewMail)
                .filter_map(|event| {
                    event
                        .message_id
                        .map(|id| NewJob::new(id, IndexKind::Extract).priority(PRIORITY_RECENT))
                })
                .collect();
            if !jobs.is_empty() {
                // One transaction for the page, not one per event: a restart
                // replays a retention window's worth of history, and stalling
                // every other writer behind thousands of commits is the failure
                // this batching exists to avoid.
                match self.pipeline.queue().enqueue(jobs, None).await {
                    Ok(n) => enqueued += n,
                    Err(error) => tracing::warn!(
                        %error,
                        cursor,
                        next_seq = page.next_seq,
                        "failed to enqueue this page's extract jobs; the cursor still advances \
                         past it, so `mail index reindex` is the recovery path for a persistent \
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

    /// One cycle: drain newly synced mail into the queue, then run a batch.
    ///
    /// A paused worker skips both. `mail index stop` means "stop indexing," and
    /// a stopped worker that kept enqueueing would spend the moment it was
    /// started again catching up on a backlog nobody watched it build.
    ///
    /// # Errors
    ///
    /// A mapped storage error from the drain. A failed event-log read is logged
    /// and the cycle still runs whatever was already queued — an unrelated read
    /// failure must not cost a whole tick's indexing.
    #[tracing::instrument(skip(self, cancel))]
    pub async fn tick(&self, cancel: &CancellationToken) -> Result<TickReport, Error> {
        if self.pipeline.paused.get() {
            return Ok(TickReport::default());
        }
        let since = self.cursor.load(Ordering::SeqCst);
        let enqueued = match self.drain_new_mail(since).await {
            Ok((enqueued, next)) => {
                self.cursor.store(next, Ordering::SeqCst);
                enqueued
            }
            Err(error) => {
                tracing::warn!(%error, since, "index loop: draining new mail failed this tick");
                0
            }
        };
        let drain = self.pipeline.run_once(self.lease_limit, cancel).await?;
        Ok(TickReport {
            enqueued,
            drain: Some(drain),
        })
    }

    /// Spawn the loop, running once immediately — so a daemon restarted more
    /// often than the tick interval still makes progress — and then until
    /// `cancel` fires.
    ///
    /// # The interval is a latency floor, not a throughput ceiling
    ///
    /// A full batch means there was more work than this pass could take, so the
    /// next one starts straight away. Sleeping between full batches instead
    /// would turn `tick_interval` into a rate limit: at two seconds and sixteen
    /// jobs it caps the indexer at eight jobs a second, and a first index of a
    /// hundred thousand messages — four jobs each — would spend fourteen hours
    /// almost entirely inside `sleep`. The interval exists to keep an *idle*
    /// mailbox from polling the database several times a second, which is
    /// exactly the case where the batch comes back short.
    pub fn spawn(self, cancel: CancellationToken) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                let saturated = match self.tick(&cancel).await {
                    Ok(report) => {
                        tracing::trace!(?report, "index tick");
                        report
                            .drain
                            .is_some_and(|drain| drain.leased >= self.lease_limit.max(0) as u64)
                    }
                    Err(error) => {
                        tracing::warn!(%error, "index tick failed");
                        false
                    }
                };
                if cancel.is_cancelled() {
                    return;
                }
                if saturated {
                    // Yield rather than sleep: there is a backlog, and every
                    // other task in the process — including the RPCs a user is
                    // waiting on — still needs the runtime between batches.
                    tokio::task::yield_now().await;
                    continue;
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
