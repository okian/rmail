//! The AI queue and worker pool: the chokepoint every outbound Claude call
//! passes through.
//!
//! # The pipeline, and why the order is load-bearing
//!
//! ```text
//! policy → assemble → redact → provider → audit
//! ```
//!
//! 1. **Policy** ([`crate::ai::policy::PolicyEngine::resolve`]) runs first.
//!    A folder the user has marked `forbidden` or `local_only` must never
//!    reach the steps that follow — building a request, redacting it, or
//!    auditing it all imply "this mail was looked at for an AI feature,"
//!    which is exactly what a forbidden classification promises will not
//!    happen. This is checked before any content is even read off disk.
//! 2. **Assemble** ([`assemble_content`]) builds the bounded, policy-safe
//!    content a request will carry: the message body, truncated to
//!    `ai.privacy.max_body_chars`, with attachment text included or omitted
//!    per `ai.privacy.strip_attachments`. [`crate::ai::redact`]'s module
//!    docs are explicit that this is *this* module's job, not theirs — the
//!    firewall's job starts once the text already exists, not deciding what
//!    earns a place in the request.
//! 3. **Redact** ([`crate::ai::redact::guard`]) is the mandatory PII
//!    firewall, run unconditionally over whatever [`PassHandler::build_request`]
//!    produced from that content. [`GuardedRequest::RedactedSkip`] means the
//!    provider is **never called** — the job is recorded as such and
//!    terminated, not retried. Running redaction *before* this point (over
//!    unassembled content) would miss text the assembly step adds; running
//!    it after the provider call would defeat the entire firewall.
//! 4. **Provider** ([`crate::ai::provider::Provider::complete`]) is the one
//!    step that leaves the machine. Concurrency ([`tokio::sync::Semaphore`]),
//!    pacing ([`RateLimiter`]), and the cost gate ([`CostGate`]) all act
//!    *before* this step — see "The cost gate blocks before dispatch" below.
//! 5. **Audit** ([`crate::ai::audit::record_call`]) is called with the
//!    **redacted** payload — never the raw one — immediately around the
//!    provider call, whether it succeeded or failed. Auditing the
//!    pre-redaction body would record proof that raw content left the
//!    machine even when it did not; auditing before the provider call ran
//!    at all would record calls that never happened.
//!
//! Swapping steps 3 and 5 — auditing the unredacted body, or redacting after
//! the provider already saw the raw text — defeats the purpose of both
//! systems simultaneously, which is why this module composes them in this
//! fixed order rather than leaving the order to each caller.
//!
//! # What this module owns vs. what it composes
//!
//! Every step above except "assemble" and "the queue itself" is a call into
//! a task that landed before this one: [`crate::ai::provider`] (43),
//! [`crate::ai::redact`] (44), [`crate::ai::audit`] (45),
//! [`crate::ai::policy`] (46). This module does not reimplement retry,
//! backoff, streaming, PII detection, or precedence rules — it sequences
//! calls into those modules in the order above and adds the two things none
//! of them can: a durable, leased, dedup'd queue of *what* to run that
//! pipeline over, and the concurrency/rate/cost controls that decide *how
//! fast*.
//!
//! # Why `ai_queue` needs a fifth state `index_queue` does not
//!
//! [`crate::index::queue`] — this module's structural sibling, and the
//! pattern this one follows rather than redesigns — has four states:
//! pending, leased, done, dead. An AI job needs a fifth,
//! [`JobState::Error`], because three outcomes here are *definitely never
//! going to succeed on retry*, which is a different fact from "retries were
//! attempted and exhausted" (`dead`):
//!
//! - [`crate::ai::redact::GuardedRequest::RedactedSkip`] — there was nothing
//!   left to send once PII was tokenized out.
//! - A `Forbidden`/`LocalOnly` policy resolution reached at dispatch time
//!   (the ordinary path is to never enqueue such a job in the first place,
//!   but a policy rule can change *after* enqueue, and this is the
//!   fail-closed backstop).
//! - A model refusal (`stop_reason: "refusal"`,
//!   [`crate::error::Error::FailedPrecondition`] per `provider.rs`'s own
//!   docs) — a deliberate decision from a reachable provider, not a
//!   transient fault.
//!
//! Backing these off and eventually quarantining them to `dead` — as a
//! transient 429 legitimately should — would burn `max_attempts` retrying
//! something no retry can fix, and would make `mail ai retry --failed`
//! (which targets `dead`, the *transient*-failure quarantine) requeue work
//! that is not going to change its answer. [`AiQueue::terminate`] is the
//! one-way door into `Error`; [`AiQueue::fail`] is the backoff-then-`dead`
//! path for everything else, matching `index_queue::fail` exactly.
//!
//! # The cost gate blocks *before* dispatch, not after
//!
//! [`CostGate::decide`] is consulted once per [`AiWorkerPool::dispatch_pending`]
//! call, before anything is leased. Under [`crate::config::OnCap::Pause`] or
//! [`crate::config::OnCap::TriageOnly`], the jobs `on_cap` should hold back
//! are simply never leased that cycle — `lease`'s own `WHERE` clause filters
//! them out, so there is no code path by which a held-back job's provider
//! call could ever be reached. Under [`crate::config::OnCap::Drop`], the
//! jobs *are* leased (so they can be terminated with the same fenced
//! ownership check every other terminal transition uses) but the pipeline
//! stops immediately after leasing — [`AiQueue::terminate`] is called
//! instead of [`crate::ai::provider::Provider::complete`], never both. In
//! every branch, the provider is either never reached or explicitly skipped;
//! it is never called and then discarded after the fact.
//!
//! [`crate::ai::budget::BudgetEnforcer`] (task 76) is the second half of the
//! same seam, one level finer. [`CostGate`] answers a question about the
//! whole cycle — "has this daemon's global daily spend run out?" — which is
//! all it can answer, because it is consulted once, before anything is
//! leased, when no account or model is yet known. The per-account caps, the
//! bulk sub-budget, and the soft-cap model downgrade all need a *specific*
//! job: they are therefore evaluated inside [`AiWorkerPool::process_one`],
//! after the pass handler has chosen a model and before redaction and the
//! provider call. Both act before dispatch; they differ only in what they can
//! see. A job the enforcer blocks is released back to `pending` rather than
//! terminated — a daily cap rolls over at midnight, so the work is still
//! wanted, and [`AiQueue::release`] hands back the attempt `lease` charged it
//! so a week of capped-out cycles cannot quietly exhaust `max_attempts` and
//! quarantine work that was never actually tried.
//!
//! # Batch mode and why its bookkeeping does not survive a restart
//!
//! [`BatchCoordinator::maybe_submit`] flips a pass from the live per-request
//! path to the Message Batches API once its pending depth reaches
//! `ai.batching.threshold`, submitting up to `ai.batching.max_batch` jobs
//! with `custom_id` set to the message id (unique within one submission
//! because a submission only ever covers one `pass`, and `(message_id,
//! pass)` is the queue's own dedup key). Batches run at 50% of the live
//! per-token price.
//!
//! The redacted [`crate::ai::provider::ChatRequest`] and the
//! [`crate::ai::redact::TokenMap`] needed to audit and rehydrate each item's
//! eventual result are kept in the coordinator's process memory
//! ([`BatchCoordinator`]'s `pending` field), *not* persisted — this is not
//! an oversight, it is the same constraint [`crate::ai::redact`]'s module
//! docs describe for why a `TokenMap` is in-memory-only, applied
//! consistently: writing the redacted request to disk would be safe (it has
//! no raw PII in it by construction), but writing the token map that
//! reverses it back to real values would turn "the API never saw raw PII"
//! into "the API never saw raw PII, but a database row now lets anyone
//! reconstruct it" — exactly what that firewall exists to prevent. The
//! practical consequence: if the daemon restarts while a batch is still
//! processing, [`BatchCoordinator::poll`] on a fresh coordinator instance
//! cannot audit or rehydrate that batch's results, and returns
//! [`crate::error::Error::FailedPrecondition`] rather than silently doing
//! nothing. The jobs are not lost, though — they stay `leased` under the
//! long batch-lease TTL, and once that lease lapses
//! [`AiQueue::reap_expired`] returns them to `pending` exactly as it would
//! for a crashed live worker, where the next `dispatch_pending`/
//! `maybe_submit` cycle picks them up fresh (live or in a new batch,
//! whichever applies) with a brand new redaction pass. This is slower than
//! resuming the original batch, but it is the same crash-recovery story
//! `index_queue` already tells for every other kind of failure, rather than
//! a special case invented for this one.
//!
//! # Offline is not a special code path
//!
//! "Offline rows stay `pending` and drain on reconnect" is not something
//! this module implements — it is a restatement of durability.
//! [`AiQueue::enqueue`] writes to SQLite regardless of network state, and
//! nothing here deletes a `pending` row for any reason other than it being
//! leased and completed. A daemon that is closed, asleep, or has no route to
//! `api.anthropic.com` simply is not calling
//! [`AiWorkerPool::dispatch_pending`]; the moment something does call it
//! again — reconnect, restart, a scheduler tick — every `pending` row is
//! still exactly where it was.

use std::time::{Duration, Instant};

use rusqlite::{Connection, OptionalExtension};

use crate::ai::audit;
use crate::ai::provider::ChatRequest;
use crate::config::{AiLimits, OnCap};
use crate::error::Error;
use crate::storage::Database;

mod batch;
mod content;
mod worker;

pub use batch::{
    BatchClient, BatchCoordinator, BatchHandle, BatchOutcome, BatchPollOutcome, BatchRequestCounts,
    BatchRequestItem, BatchResult, BatchStatus,
};
pub use content::{assemble_content, MessageContent};
pub use worker::{AiWorkerPool, DispatchSummary, PassHandler};

/// How long a live (non-batch) lease is good for before the reaper may take
/// it back. Short enough that a crashed worker's jobs are not stranded long;
/// long enough that a deep-pass call — the slower of the two passes — still
/// finishes inside it.
pub const DEFAULT_LEASE: Duration = Duration::from_secs(5 * 60);

/// How long a lease taken out for a Message Batches submission is good for.
/// Anthropic's batches can legitimately take up to 24 hours; a lease shorter
/// than that would have the reaper reclaim jobs the batch is still
/// processing, out from under a coordinator that is working exactly as
/// intended.
pub const BATCH_LEASE: Duration = Duration::from_secs(26 * 60 * 60);

/// How many times a job is retried before it is quarantined to `dead`.
pub const DEFAULT_MAX_ATTEMPTS: i64 = 5;

/// The first retry delay; doubles per attempt.
pub const DEFAULT_BACKOFF: Duration = Duration::from_secs(30);

/// Ceiling on the retry delay.
pub const DEFAULT_MAX_BACKOFF: Duration = Duration::from_secs(30 * 60);

/// Default priority. Lower runs first.
pub const PRIORITY_NORMAL: i64 = 100;

/// Priority for mail a user is looking at right now (`mail ai process`, an
/// interactive re-analyze).
pub const PRIORITY_RECENT: i64 = 10;

/// Priority for a backlog walk.
pub const PRIORITY_BACKFILL: i64 = 500;

/// The fixed worker identity a [`BatchCoordinator`] leases jobs under. A
/// single daemon process runs at most one coordinator, so — unlike a live
/// worker pool, which mints a distinct name per instance for diagnosis —
/// there is nothing this needs to disambiguate.
const BATCH_WORKER: &str = "ai-batch-coordinator";

// ---------------------------------------------------------------------------
// Vocabulary
// ---------------------------------------------------------------------------

/// Where an AI job stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    /// Waiting for a worker.
    Pending,
    /// Held by a worker (live or batch) until its lease expires.
    Leased,
    /// Finished; an artifact was persisted.
    Done,
    /// Terminated as unrecoverable — see the module docs on why this is
    /// distinct from `dead`. Never leased again; kept so the reason is
    /// visible rather than silently dropped.
    Error,
    /// Quarantined after too many transient failures. Never leased again
    /// until [`AiQueue::revive`]/[`AiQueue::revive_all_dead`].
    Dead,
}

impl JobState {
    /// The stable wire string stored in `ai_queue.state`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Leased => "leased",
            Self::Done => "done",
            Self::Error => "error",
            Self::Dead => "dead",
        }
    }

    /// Parse a wire string.
    ///
    /// # Errors
    /// [`Error::Internal`] for a value no version of this code wrote.
    pub fn parse(value: &str) -> Result<Self, Error> {
        match value {
            "pending" => Ok(Self::Pending),
            "leased" => Ok(Self::Leased),
            "done" => Ok(Self::Done),
            "error" => Ok(Self::Error),
            "dead" => Ok(Self::Dead),
            other => Err(Error::internal(format!("unknown ai_queue state: {other}"))),
        }
    }
}

/// A unit of AI work to queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAiJob {
    /// The message to analyze.
    pub message_id: i64,
    /// The account it belongs to (denormalized onto the queue row so a
    /// worker never needs a join just to resolve the policy target).
    pub account_id: i64,
    /// Which pass, e.g. `"triage"` or `"deep"`. Half of the dedup key.
    pub pass: String,
    /// Lower runs first.
    pub priority: i64,
}

impl NewAiJob {
    /// A job at [`PRIORITY_NORMAL`].
    #[must_use]
    pub fn new(message_id: i64, account_id: i64, pass: impl Into<String>) -> Self {
        Self {
            message_id,
            account_id,
            pass: pass.into(),
            priority: PRIORITY_NORMAL,
        }
    }

    /// Set the priority.
    #[must_use]
    pub fn priority(mut self, priority: i64) -> Self {
        self.priority = priority;
        self
    }
}

/// A leased AI job: work a worker (live or batch) now owns until its lease
/// expires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AiLease {
    /// Queue row id, used to complete, fail, or terminate the job.
    pub job_id: i64,
    /// The message to analyze.
    pub message_id: i64,
    /// The owning account.
    pub account_id: i64,
    /// Which pass this lease is for.
    pub pass: String,
    /// The queue priority this job was enqueued at. Carried onto the lease
    /// (rather than left behind on the row) because it is what
    /// [`crate::ai::budget::WorkClass::for_priority`] classifies a job as
    /// bulk or interactive by, and the budget check happens per job on the
    /// dispatch path — re-reading `ai_queue` for one integer that was already
    /// in hand at lease time would be a second round trip per call.
    pub priority: i64,
    /// How many times this job has been attempted, including this one.
    pub attempts: i64,
    /// When the lease lapses (unix seconds).
    pub lease_expires_at: i64,
    /// Who holds this lease. [`AiQueue::complete`]/[`AiQueue::fail`]/
    /// [`AiQueue::terminate`] all refuse to act on a job whose lease has
    /// since been reaped and handed to someone else — the same fencing
    /// [`crate::index::queue`] uses.
    pub worker: String,
}

/// What happened to a job passed to [`AiQueue::fail`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// It will be retried after a backoff.
    Retrying {
        /// When it becomes eligible again (unix seconds).
        next_attempt_at: i64,
        /// How many attempts it has now had.
        attempts: i64,
    },
    /// It exhausted its attempts and was quarantined to `dead`.
    Quarantined {
        /// How many attempts it had.
        attempts: i64,
    },
}

/// A quarantined (`dead`) job, for diagnosis and for `mail ai retry --failed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeadLetter {
    /// Queue row id, for [`AiQueue::revive`].
    pub job_id: i64,
    /// The message that could not be analyzed.
    pub message_id: i64,
    /// Which pass failed.
    pub pass: String,
    /// The last failure recorded against it.
    pub last_error: Option<String>,
}

/// A count of AI jobs by state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct QueueStats {
    /// Waiting for a worker, and eligible now.
    pub ready: i64,
    /// Waiting, but backing off after a transient failure.
    pub backing_off: i64,
    /// Held by a worker (live or batch).
    pub leased: i64,
    /// Finished.
    pub done: i64,
    /// Terminated as unrecoverable — see [`JobState::Error`].
    pub error: i64,
    /// Quarantined after exhausting retries.
    pub dead: i64,
}

impl QueueStats {
    /// Jobs still to do: ready, backing off, or leased.
    #[must_use]
    pub fn outstanding(&self) -> i64 {
        self.ready + self.backing_off + self.leased
    }
}

/// Tuning for an [`AiQueue`].
#[derive(Debug, Clone, Copy)]
pub struct QueueOptions {
    /// How long a live lease is good for.
    pub lease: Duration,
    /// Attempts before quarantine to `dead`.
    pub max_attempts: i64,
    /// First retry delay.
    pub backoff: Duration,
    /// Ceiling on the retry delay.
    pub max_backoff: Duration,
}

impl Default for QueueOptions {
    fn default() -> Self {
        Self {
            lease: DEFAULT_LEASE,
            max_attempts: DEFAULT_MAX_ATTEMPTS,
            backoff: DEFAULT_BACKOFF,
            max_backoff: DEFAULT_MAX_BACKOFF,
        }
    }
}

impl QueueOptions {
    /// The delay before attempt `attempts`, doubling and capped — identical
    /// in shape to `index::queue::QueueOptions::backoff_for`.
    fn backoff_for(&self, attempts: i64) -> Duration {
        let shift = u32::try_from(attempts.max(1) - 1)
            .unwrap_or(u32::MAX)
            .min(32);
        self.backoff
            .saturating_mul(1u32.checked_shl(shift).unwrap_or(u32::MAX))
            .min(self.max_backoff)
    }
}

// ---------------------------------------------------------------------------
// The queue
// ---------------------------------------------------------------------------

/// The durable AI work queue.
///
/// Cheap to clone: every clone shares one database handle.
#[derive(Debug, Clone)]
pub struct AiQueue {
    db: Database,
    opts: QueueOptions,
}

impl AiQueue {
    /// Open a queue over `db`.
    #[must_use]
    pub fn new(db: Database, opts: QueueOptions) -> Self {
        Self { db, opts }
    }

    /// The database this queue is backed by, for callers ([`BatchCoordinator`],
    /// [`AiWorkerPool`]) that also need it directly.
    #[must_use]
    pub fn database(&self) -> &Database {
        &self.db
    }

    /// Queue work, skipping any `(message_id, pass)` already queued in any
    /// state — the acceptance criterion's dedup, applied literally: a
    /// message queued twice for the same pass is not queued twice, whatever
    /// state the existing row is in (including `dead`, matching
    /// `index_queue`'s "a quarantined job stays quarantined" rule; use
    /// [`AiQueue::revive`]/[`AiQueue::requeue`] to intentionally reset one).
    ///
    /// Returns how many jobs were actually queued.
    ///
    /// # Errors
    /// A mapped storage error.
    #[tracing::instrument(skip(self, jobs), fields(count = jobs.len(), queued))]
    pub async fn enqueue(&self, jobs: Vec<NewAiJob>) -> Result<u64, Error> {
        if jobs.is_empty() {
            return Ok(0);
        }
        let queued = self
            .db
            .write(move |conn| {
                let tx = conn.transaction()?;
                let mut queued = 0u64;
                for job in jobs {
                    if enqueue_one(&tx, &job)? {
                        queued += 1;
                    }
                }
                tx.commit()?;
                Ok(queued)
            })
            .await?;
        tracing::Span::current().record("queued", queued);
        tracing::debug!(queued, "ai jobs enqueued");
        Ok(queued)
    }

    /// Queue (or force back to `pending`) one `(message_id, pass)` job,
    /// regardless of its current state — the escape hatch `enqueue`
    /// deliberately does not provide, for `mail ai process --force` and
    /// similar forced re-analysis. Refuses to disturb a job a worker
    /// currently holds (`state = 'leased'`): a force-reprocess request that
    /// arrives mid-call should not have that call's own eventual
    /// `complete`/`fail` land on a row that has since been reset out from
    /// under it.
    ///
    /// Returns whether the row was inserted or reset (`false` only when the
    /// existing row is currently `leased`).
    ///
    /// # Errors
    /// A mapped storage error.
    pub async fn requeue(
        &self,
        message_id: i64,
        account_id: i64,
        pass: impl Into<String>,
        priority: i64,
    ) -> Result<bool, Error> {
        let pass = pass.into();
        self.db
            .write(move |conn| {
                let exists: bool = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM messages WHERE id = ?1)",
                    [message_id],
                    |row| row.get(0),
                )?;
                if !exists {
                    return Ok(false);
                }
                let changed = conn.execute(
                    "INSERT INTO ai_queue (message_id, account_id, pass, priority, state, attempts, next_attempt_at)
                     VALUES (?1, ?2, ?3, ?4, 'pending', 0, 0)
                     ON CONFLICT(message_id, pass) DO UPDATE SET
                         account_id = excluded.account_id,
                         state = 'pending',
                         attempts = 0,
                         next_attempt_at = 0,
                         lease_expires_at = NULL,
                         leased_by = NULL,
                         last_error = NULL,
                         batch_id = NULL,
                         priority = MIN(ai_queue.priority, excluded.priority),
                         updated_at = unixepoch()
                     WHERE ai_queue.state <> 'leased'",
                    rusqlite::params![message_id, account_id, pass, priority],
                )?;
                Ok(changed > 0)
            })
            .await
            .map_err(Error::from)
    }

    /// How many `pending` **and currently leasable** jobs exist for `pass` —
    /// the batch-flip threshold input. Only `pending`, not
    /// `pending + leased`: a job already claimed by a live worker is
    /// already being handled and should not count toward "there is a
    /// backlog worth batching." And only jobs whose backoff has actually
    /// elapsed (`next_attempt_at <= now`) — the same filter
    /// [`AiQueue::lease_with_ttl`] applies — so this never reports a depth
    /// that a same-moment `lease_with_ttl` call could not actually satisfy;
    /// counting backing-off jobs here would let a batch "flip" fire on a
    /// backlog `lease_with_ttl` then hands back nothing for.
    ///
    /// # Errors
    /// A mapped storage error.
    pub async fn depth_for_pass(&self, pass: &str) -> Result<i64, Error> {
        let pass = pass.to_owned();
        self.db
            .read(move |conn| {
                let now = chrono::Utc::now().timestamp();
                conn.query_row(
                    "SELECT COUNT(*) FROM ai_queue
                     WHERE state = 'pending' AND pass = ?1 AND next_attempt_at <= ?2",
                    rusqlite::params![pass, now],
                    |row| row.get(0),
                )
            })
            .await
            .map_err(Error::from)
    }

    /// Take up to `limit` `pending` jobs, best first, leasing them to
    /// `worker` for [`QueueOptions::lease`]. `pass`, if set, restricts the
    /// selection to that pass — how [`crate::config::OnCap::TriageOnly`]
    /// admits only cheap work while a spend cap holds.
    ///
    /// # Errors
    /// A mapped storage error.
    #[tracing::instrument(skip(self), fields(leased))]
    pub async fn lease(
        &self,
        worker: &str,
        limit: i64,
        pass: Option<&str>,
    ) -> Result<Vec<AiLease>, Error> {
        let leased = self
            .lease_with_ttl(worker, limit, pass, self.opts.lease)
            .await?;
        tracing::Span::current().record("leased", leased.len());
        Ok(leased)
    }

    /// As [`AiQueue::lease`], but with an explicit lease TTL — what
    /// [`BatchCoordinator`] uses to take out [`BATCH_LEASE`]-long leases
    /// instead of the ordinary live-worker duration.
    ///
    /// # Per-thread serialization of `"deep"` leases — carried over from
    /// task 49
    ///
    /// [`crate::ai::deep::DeepPassHandler::build_request`] reads a thread's
    /// prior rollup once, before the semaphore permit and before the
    /// provider call — see that module's own docs for the full "known,
    /// accepted race" it describes and why the fix has to live here, in the
    /// query that decides what gets leased, rather than in the handler. Two
    /// `"deep"` jobs for the *same* thread leased out of the same call would
    /// both read that identical prior state and race to overwrite each
    /// other's contribution to `ai_summaries.thread_summary`; the candidate
    /// query below closes that off from two directions at once:
    ///
    /// - The `NOT EXISTS` guard excludes a thread that already has a
    ///   `"deep"` job `leased` — i.e. still being worked by a *previous*
    ///   lease call, live or batch — so this call cannot double-lease a
    ///   thread whose earlier deep pass has not finished yet.
    /// - A `ROW_NUMBER() OVER (PARTITION BY ...)` window, evaluated over
    ///   *every* eligible candidate before `LIMIT` is ever applied, admits
    ///   only the single best (by priority/enqueued_at/job_id) `"deep"`
    ///   candidate per thread within this call — see the query's own inline
    ///   comment for why this has to rank-then-limit rather than the
    ///   simpler-looking limit-then-filter-in-Rust an earlier version of
    ///   this method did, and why that earlier shape quietly collapsed
    ///   throughput on exactly the backlog case this fix targets.
    ///
    /// Together these mean at most one `"deep"` job per thread is ever
    /// leased (by anyone) at a time — the acceptance criterion's "cap
    /// concurrent deep leases to one per thread per cycle," extended to
    /// cover overlap *across* cycles too, since a deep-pass provider call can
    /// easily outlive one cycle's own interval. A thread with several
    /// `"deep"` jobs queued at once (the batch path's own worst case — see
    /// `ai::deep`'s docs on why backlog/initial-sync is exactly where this
    /// happens) simply drains one lease at a time, across as many calls as
    /// it has messages, rather than all at once. Jobs of any other pass
    /// (`"triage"`) are never restricted by this — the race is specific to
    /// `"deep"`'s thread-rollup fold, which no other pass performs.
    ///
    /// A message with no thread (`messages.thread_id IS NULL`) is exempt:
    /// there is no shared rollup for a solo message to race against itself
    /// over.
    ///
    /// # Errors
    /// A mapped storage error.
    pub async fn lease_with_ttl(
        &self,
        worker: &str,
        limit: i64,
        pass: Option<&str>,
        ttl: Duration,
    ) -> Result<Vec<AiLease>, Error> {
        if limit <= 0 {
            return Ok(Vec::new());
        }
        let worker = worker.to_owned();
        let pass = pass.map(str::to_owned);
        let lease_secs = i64::try_from(ttl.as_secs()).unwrap_or(i64::MAX);
        let leased = self
            .db
            .write(move |conn| {
                let now = chrono::Utc::now().timestamp();
                let tx = conn.transaction()?;
                // The per-thread "deep" dedup is done entirely in SQL, via
                // `ROW_NUMBER() OVER (PARTITION BY ...)`, and *before* the
                // final `LIMIT` — not a `LIMIT`-then-filter-in-Rust pass, an
                // earlier version of this query's own mistake. `LIMIT`-then-
                // filter means "how many distinct threads appear in the
                // first `limit` rows by priority order," which is not the
                // same question as "the `limit` best distinct-thread
                // candidates" — a single thread with more backlog than
                // `limit` (exactly the batch/initial-sync case this fix
                // targets, per the module docs) would fill the whole
                // candidate window and starve every other thread's "deep"
                // job out of this call entirely, and the batch path would
                // observe a large `depth_for_pass` but keep submitting
                // near-empty batches under a 26h `BATCH_LEASE` each. Ranking
                // first and limiting after admits the true `limit` best
                // distinct-thread candidates instead.
                //
                // The partition key is `(pass, 'job:'||job_id)` for every
                // non-"deep" row and every threadless "deep" row — each
                // alone in its own partition, so `thread_rank` is always 1
                // and neither is ever restricted by this — and
                // `(pass, 'thread:'||thread_id)` only for a "deep" row with
                // a real thread, which is the one case
                // `ORDER BY q.priority, q.enqueued_at, q.job_id` inside the
                // window actually caps at one admitted row. The `'job:'`/
                // `'thread:'` text prefixes are load-bearing, not
                // decoration: `threads.id` and `ai_queue.job_id` are
                // independent autoincrement sequences over separate tables
                // and routinely share numeric values, especially early in a
                // fresh mailbox's life (both start at 1). Partitioning on
                // the bare integer would silently put a threadless "deep"
                // job (keyed by its own job_id) in the same window
                // partition as an unrelated thread whose thread_id happens
                // to equal that number, capping the threadless job as if it
                // belonged to that thread — the text prefixes make the two
                // id spaces disjoint at the partition-key level, not merely
                // usually-disjoint by chance. `'deep'` is a literal, not
                // `crate::ai::deep::PASS`, deliberately — see this method's
                // own doc comment for why this module does not otherwise
                // know pass names, and treats this one as a narrow,
                // documented exception rather than an excuse to import a
                // sibling pass module into the queue.
                let candidates: Vec<i64> = {
                    let mut stmt = tx.prepare(
                        "WITH candidates AS (
                             SELECT
                                 q.job_id,
                                 q.priority,
                                 q.enqueued_at,
                                 ROW_NUMBER() OVER (
                                     PARTITION BY
                                         q.pass,
                                         CASE WHEN q.pass = 'deep' AND m.thread_id IS NOT NULL
                                              THEN 'thread:' || m.thread_id
                                              ELSE 'job:' || q.job_id
                                         END
                                     ORDER BY q.priority, q.enqueued_at, q.job_id
                                 ) AS thread_rank
                             FROM ai_queue q
                             LEFT JOIN messages m ON m.id = q.message_id
                             WHERE q.state = 'pending' AND q.next_attempt_at <= ?1
                               AND (?2 IS NULL OR q.pass = ?2)
                               AND (
                                 q.pass <> 'deep'
                                 OR m.thread_id IS NULL
                                 OR NOT EXISTS (
                                   SELECT 1 FROM ai_queue lq
                                   JOIN messages lm ON lm.id = lq.message_id
                                   WHERE lq.pass = 'deep' AND lq.state = 'leased'
                                     AND lm.thread_id = m.thread_id
                                 )
                               )
                         )
                         SELECT job_id FROM candidates
                         WHERE thread_rank = 1
                         ORDER BY priority, enqueued_at, job_id
                         LIMIT ?3",
                    )?;
                    let rows = stmt
                        .query_map(rusqlite::params![now, pass, limit], |row| row.get(0))?
                        .collect::<rusqlite::Result<Vec<i64>>>()?;
                    rows
                };

                let mut leased = Vec::with_capacity(candidates.len());
                {
                    let mut claim = tx.prepare(
                        "UPDATE ai_queue
                         SET state = 'leased',
                             attempts = attempts + 1,
                             lease_expires_at = ?2,
                             leased_by = ?3,
                             updated_at = unixepoch()
                         WHERE job_id = ?1
                         RETURNING job_id, message_id, account_id, pass, attempts,
                                   lease_expires_at, leased_by, priority",
                    )?;
                    for job_id in candidates {
                        let row = claim.query_row(
                            rusqlite::params![job_id, now.saturating_add(lease_secs), worker],
                            lease_from_row,
                        )?;
                        leased.push(row);
                    }
                }
                tx.commit()?;
                Ok(leased)
            })
            .await?;
        Ok(leased)
    }

    /// Stamp `batch_id` onto jobs already leased for a Message Batches
    /// submission — called once [`BatchClient::submit`] returns an id, so
    /// `ai_queue.batch_id` is queryable even though the in-memory bookkeeping
    /// that can actually complete those jobs lives only in the coordinator
    /// that submitted them (see the module docs).
    ///
    /// # Errors
    /// A mapped storage error.
    pub async fn mark_batched(&self, job_ids: &[i64], batch_id: &str) -> Result<u64, Error> {
        if job_ids.is_empty() {
            return Ok(0);
        }
        let job_ids = job_ids.to_vec();
        let batch_id = batch_id.to_owned();
        self.db
            .write(move |conn| {
                let tx = conn.transaction()?;
                let mut changed = 0u64;
                {
                    let mut stmt = tx.prepare(
                        "UPDATE ai_queue SET batch_id = ?2, updated_at = unixepoch()
                         WHERE job_id = ?1 AND state = 'leased'",
                    )?;
                    for job_id in job_ids {
                        changed +=
                            u64::try_from(stmt.execute(rusqlite::params![job_id, batch_id])?)
                                .unwrap_or(0);
                    }
                }
                tx.commit()?;
                Ok(changed)
            })
            .await
            .map_err(Error::from)
    }

    /// Mark a leased job done, recording the ledger entry its provider call
    /// produced. Returns whether the lease still held — `false` means the
    /// lease was reaped and handed to someone else, exactly as
    /// `index_queue::complete` documents.
    ///
    /// # Errors
    /// A mapped storage error.
    #[tracing::instrument(skip(self, lease), fields(job_id = lease.job_id, pass = %lease.pass))]
    pub async fn complete(
        &self,
        lease: &AiLease,
        ledger_entry_id: Option<i64>,
    ) -> Result<bool, Error> {
        let job_id = lease.job_id;
        let worker = lease.worker.clone();
        let held = self
            .db
            .write(move |conn| {
                let changed = conn.execute(
                    "UPDATE ai_queue
                     SET state = 'done', lease_expires_at = NULL, leased_by = NULL,
                         last_error = NULL, ledger_entry_id = ?3, updated_at = unixepoch()
                     WHERE job_id = ?1 AND state = 'leased' AND leased_by = ?2",
                    rusqlite::params![job_id, worker, ledger_entry_id],
                )?;
                Ok(changed > 0)
            })
            .await?;
        if !held {
            tracing::warn!(
                job_id,
                "completed an ai job this worker no longer holds; its lease was reaped"
            );
        }
        Ok(held)
    }

    /// Record a transient failure on a leased job, backing it off or
    /// quarantining it to `dead` after [`QueueOptions::max_attempts`] —
    /// identical in shape to `index_queue::fail`. Use [`AiQueue::terminate`]
    /// instead for a failure that is never going to succeed on retry (see
    /// the module docs).
    ///
    /// Returns `None` if the lease no longer held.
    ///
    /// # Errors
    /// A mapped storage error.
    #[tracing::instrument(skip(self, lease, error), fields(job_id = lease.job_id, pass = %lease.pass))]
    pub async fn fail(&self, lease: &AiLease, error: &str) -> Result<Option<Failure>, Error> {
        let error = error.to_owned();
        let logged = error.clone();
        let opts = self.opts;
        let job_id = lease.job_id;
        let worker = lease.worker.clone();
        let outcome = self
            .db
            .write(move |conn| {
                let now = chrono::Utc::now().timestamp();
                let tx = conn.transaction()?;
                let attempts: Option<i64> = tx
                    .query_row(
                        "SELECT attempts FROM ai_queue WHERE job_id = ?1 AND state = 'leased' AND leased_by = ?2",
                        rusqlite::params![job_id, worker],
                        |row| row.get(0),
                    )
                    .optional()?;
                let Some(attempts) = attempts else {
                    return Ok(None);
                };

                let outcome = if attempts >= opts.max_attempts {
                    tx.execute(
                        "UPDATE ai_queue
                         SET state = 'dead', lease_expires_at = NULL, leased_by = NULL,
                             last_error = ?2, updated_at = unixepoch()
                         WHERE job_id = ?1",
                        rusqlite::params![job_id, error],
                    )?;
                    Failure::Quarantined { attempts }
                } else {
                    let delay = i64::try_from(opts.backoff_for(attempts).as_secs()).unwrap_or(i64::MAX);
                    let next_attempt_at = now.saturating_add(delay);
                    tx.execute(
                        "UPDATE ai_queue
                         SET state = 'pending', lease_expires_at = NULL, leased_by = NULL,
                             next_attempt_at = ?2, last_error = ?3, updated_at = unixepoch()
                         WHERE job_id = ?1",
                        rusqlite::params![job_id, next_attempt_at, error],
                    )?;
                    Failure::Retrying { next_attempt_at, attempts }
                };
                tx.commit()?;
                Ok(Some(outcome))
            })
            .await?;
        match &outcome {
            Some(Failure::Quarantined { attempts }) => tracing::warn!(
                job_id,
                message_id = lease.message_id,
                pass = %lease.pass,
                attempts,
                error = %logged,
                "ai job quarantined after repeated failures"
            ),
            Some(Failure::Retrying { attempts, .. }) => {
                tracing::debug!(job_id, attempts, error = %logged, "ai job will be retried");
            }
            None => tracing::warn!(job_id, "failed an ai job this worker no longer holds"),
        }
        Ok(outcome)
    }

    /// Terminate a leased job as unrecoverable — see the module docs'
    /// "Why `ai_queue` needs a fifth state" for exactly which outcomes this
    /// is for. Unlike [`AiQueue::fail`], attempts are not incremented and no
    /// backoff is scheduled: this is a one-way door, not a retry.
    ///
    /// Returns whether the lease still held.
    ///
    /// # Errors
    /// A mapped storage error.
    #[tracing::instrument(skip(self, lease, reason), fields(job_id = lease.job_id, pass = %lease.pass))]
    pub async fn terminate(&self, lease: &AiLease, reason: &str) -> Result<bool, Error> {
        let reason = reason.to_owned();
        let logged = reason.clone();
        let job_id = lease.job_id;
        let worker = lease.worker.clone();
        let held = self
            .db
            .write(move |conn| {
                let changed = conn.execute(
                    "UPDATE ai_queue
                     SET state = 'error', lease_expires_at = NULL, leased_by = NULL,
                         last_error = ?3, updated_at = unixepoch()
                     WHERE job_id = ?1 AND state = 'leased' AND leased_by = ?2",
                    rusqlite::params![job_id, worker, reason],
                )?;
                Ok(changed > 0)
            })
            .await?;
        if held {
            tracing::debug!(job_id, reason = %logged, "ai job terminated as unrecoverable");
        } else {
            tracing::warn!(job_id, "terminated an ai job this worker no longer holds");
        }
        Ok(held)
    }

    /// Hand a leased job back to `pending` without charging it an attempt —
    /// for a job cancelled (graceful shutdown, most often) before it ever
    /// reached the provider. `lease` already incremented `attempts` when it
    /// claimed the row; this undoes exactly that increment, so a shutdown
    /// racing a worker never costs a message part of its
    /// [`QueueOptions::max_attempts`] budget for work nothing actually
    /// attempted. Distinct from [`AiQueue::fail`], which *does* charge an
    /// attempt, because `fail` is for a call that was actually made and
    /// failed.
    ///
    /// Returns whether the lease still held.
    ///
    /// # Errors
    /// A mapped storage error.
    pub async fn release(&self, lease: &AiLease) -> Result<bool, Error> {
        let job_id = lease.job_id;
        let worker = lease.worker.clone();
        let held = self
            .db
            .write(move |conn| {
                let changed = conn.execute(
                    "UPDATE ai_queue
                     SET state = 'pending', attempts = MAX(attempts - 1, 0),
                         lease_expires_at = NULL, leased_by = NULL, batch_id = NULL,
                         updated_at = unixepoch()
                     WHERE job_id = ?1 AND state = 'leased' AND leased_by = ?2",
                    rusqlite::params![job_id, worker],
                )?;
                Ok(changed > 0)
            })
            .await?;
        if held {
            tracing::debug!(job_id, "ai job released back to pending, uncharged");
        } else {
            tracing::warn!(job_id, "released an ai job this worker no longer holds");
        }
        Ok(held)
    }

    /// Return a leased job to `pending` without charging it an attempt, and
    /// hold it out of the candidate set until `next_attempt_at`.
    ///
    /// [`Self::release`] with a deadline, and the difference matters. A job
    /// the budget enforcer withheld ([`crate::ai::budget`]) cannot run until
    /// its blocking window rolls over, but `release` leaves
    /// `next_attempt_at` alone — so `lease`, which orders by
    /// `(priority, enqueued_at, job_id)`, would hand back the same jobs on
    /// every tick for as long as the cap held. That is a fixed cost per tick
    /// in leases, spend scans, and write transactions, and worse, it is
    /// head-of-line starvation: a capped account's oldest jobs would sit at
    /// the front of the candidate set and keep an *uncapped* account's work
    /// from ever being leased. Setting the deadline drops the job out of the
    /// candidate set entirely (`lease`'s own `next_attempt_at <= now`
    /// filter) until it can actually run.
    ///
    /// The attempt refund is [`Self::release`]'s, for [`Self::release`]'s
    /// reason: `lease` already incremented `attempts`, and a job that was
    /// never dispatched must not lose a retry to a cap it never got to spend
    /// against.
    ///
    /// Returns whether this worker still held the lease — a job reaped and
    /// re-leased elsewhere is left alone, the same fencing every other
    /// transition here uses.
    ///
    /// # Errors
    /// A mapped storage error.
    pub async fn defer(&self, lease: &AiLease, next_attempt_at: i64) -> Result<bool, Error> {
        let job_id = lease.job_id;
        let worker = lease.worker.clone();
        let held = self
            .db
            .write(move |conn| {
                let changed = conn.execute(
                    "UPDATE ai_queue
                     SET state = 'pending', attempts = MAX(attempts - 1, 0),
                         next_attempt_at = ?3,
                         lease_expires_at = NULL, leased_by = NULL, batch_id = NULL,
                         updated_at = unixepoch()
                     WHERE job_id = ?1 AND state = 'leased' AND leased_by = ?2",
                    rusqlite::params![job_id, worker, next_attempt_at],
                )?;
                Ok(changed > 0)
            })
            .await?;
        if held {
            tracing::debug!(
                job_id,
                next_attempt_at,
                "ai job deferred to pending, uncharged"
            );
        } else {
            tracing::warn!(job_id, "deferred an ai job this worker no longer holds");
        }
        Ok(held)
    }

    /// Return jobs whose lease has lapsed to the queue — called at the start
    /// of every [`AiWorkerPool::dispatch_pending`] cycle, and should also be
    /// called periodically even when nothing is actively dispatching, so a
    /// batch whose coordinator died still eventually comes back.
    ///
    /// # Errors
    /// A mapped storage error.
    #[tracing::instrument(skip(self))]
    pub async fn reap_expired(&self) -> Result<u64, Error> {
        let opts = self.opts;
        let reclaimed = self
            .db
            .write(move |conn| {
                let now = chrono::Utc::now().timestamp();
                let tx = conn.transaction()?;
                let quarantined = tx.execute(
                    "UPDATE ai_queue
                     SET state = 'dead', lease_expires_at = NULL, leased_by = NULL,
                         batch_id = NULL,
                         last_error = 'lease expired after the final attempt',
                         updated_at = unixepoch()
                     WHERE state = 'leased' AND lease_expires_at <= ?1 AND attempts >= ?2",
                    rusqlite::params![now, opts.max_attempts],
                )?;
                let returned = tx.execute(
                    "UPDATE ai_queue
                     SET state = 'pending', lease_expires_at = NULL, leased_by = NULL,
                         batch_id = NULL, last_error = 'lease expired', updated_at = unixepoch()
                     WHERE state = 'leased' AND lease_expires_at <= ?1",
                    [now],
                )?;
                tx.commit()?;
                Ok(quarantined + returned)
            })
            .await?;
        if reclaimed > 0 {
            tracing::info!(reclaimed, "reclaimed ai jobs from expired leases");
        }
        Ok(reclaimed as u64)
    }

    /// Move one quarantined (`dead`) job back to `pending`, clearing its
    /// attempts.
    ///
    /// # Errors
    /// A mapped storage error.
    pub async fn revive(&self, job_id: i64) -> Result<bool, Error> {
        let changed = self
            .db
            .write(move |conn| {
                conn.execute(
                    "UPDATE ai_queue
                     SET state = 'pending', attempts = 0, next_attempt_at = 0,
                         last_error = NULL, updated_at = unixepoch()
                     WHERE job_id = ?1 AND state = 'dead'",
                    [job_id],
                )
            })
            .await?;
        Ok(changed > 0)
    }

    /// Move every quarantined (`dead`) job back to `pending` — what
    /// `mail ai retry --failed` calls.
    ///
    /// # Errors
    /// A mapped storage error.
    pub async fn revive_all_dead(&self) -> Result<u64, Error> {
        let changed = self
            .db
            .write(|conn| {
                conn.execute(
                    "UPDATE ai_queue
                     SET state = 'pending', attempts = 0, next_attempt_at = 0,
                         last_error = NULL, updated_at = unixepoch()
                     WHERE state = 'dead'",
                    [],
                )
            })
            .await?;
        Ok(changed.try_into().unwrap_or(0))
    }

    /// Count jobs by state.
    ///
    /// # Errors
    /// A mapped storage error.
    pub async fn stats(&self) -> Result<QueueStats, Error> {
        let stats = self
            .db
            .read(|conn| {
                let now = chrono::Utc::now().timestamp();
                let mut stats = QueueStats::default();
                let mut stmt = conn.prepare(
                    "SELECT state, next_attempt_at <= ?1, count(*)
                     FROM ai_queue GROUP BY state, next_attempt_at <= ?1",
                )?;
                let rows = stmt.query_map([now], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, bool>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                })?;
                let mut unknown: Option<String> = None;
                for row in rows {
                    let (state, ready, count) = row?;
                    match state.as_str() {
                        "pending" if ready => stats.ready += count,
                        "pending" => stats.backing_off += count,
                        "leased" => stats.leased += count,
                        "done" => stats.done += count,
                        "error" => stats.error += count,
                        "dead" => stats.dead += count,
                        other => unknown = unknown.or_else(|| Some(other.to_owned())),
                    }
                }
                Ok((stats, unknown))
            })
            .await?;
        let (stats, unknown) = stats;
        if let Some(state) = unknown {
            return Err(Error::internal(format!("unknown ai_queue state: {state}")));
        }
        Ok(stats)
    }

    /// Quarantined (`dead`) jobs, newest failure first, for diagnosis and for
    /// `mail ai retry --failed`'s preview.
    ///
    /// # Errors
    /// A mapped storage error.
    pub async fn dead_letters(&self, limit: i64) -> Result<Vec<DeadLetter>, Error> {
        let rows: Vec<(i64, i64, String, Option<String>)> = self
            .db
            .read(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT job_id, message_id, pass, last_error FROM ai_queue
                     WHERE state = 'dead' ORDER BY updated_at DESC LIMIT ?1",
                )?;
                let rows = stmt
                    .query_map([limit.max(0)], |row| {
                        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?;
        Ok(rows
            .into_iter()
            .map(|(job_id, message_id, pass, last_error)| DeadLetter {
                job_id,
                message_id,
                pass,
                last_error,
            })
            .collect())
    }
}

/// Queue one job unless `(message_id, pass)` is already present in any
/// state. Returns whether it was queued.
///
/// `pub(crate)`, not private: `ai::triage::write_summary` (task 49) calls
/// this directly, inside the very transaction that persists a triage
/// verdict, to enqueue a qualifying deep pass atomically with that write —
/// see that function's own docs for why "durable verdict" and "a qualifying
/// verdict earns a deep job" must not be two separately-failable steps. That
/// caller does not go through [`AiQueue::enqueue`] because it does not have
/// (and must not need) an `AiQueue` handle to run inside someone else's
/// transaction.
pub(crate) fn enqueue_one(conn: &Connection, job: &NewAiJob) -> rusqlite::Result<bool> {
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM messages WHERE id = ?1)",
        [job.message_id],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(false);
    }
    let changed = conn.execute(
        "INSERT INTO ai_queue (message_id, account_id, pass, priority, state, attempts, next_attempt_at)
         VALUES (?1, ?2, ?3, ?4, 'pending', 0, 0)
         ON CONFLICT(message_id, pass) DO NOTHING",
        rusqlite::params![job.message_id, job.account_id, job.pass, job.priority],
    )?;
    Ok(changed > 0)
}

/// A leased row before its wire strings are validated.
struct RawLease {
    job_id: i64,
    message_id: i64,
    account_id: i64,
    pass: String,
    attempts: i64,
    lease_expires_at: i64,
    worker: String,
    priority: i64,
}

fn lease_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<AiLease> {
    let raw = RawLease {
        job_id: row.get(0)?,
        message_id: row.get(1)?,
        account_id: row.get(2)?,
        pass: row.get(3)?,
        attempts: row.get(4)?,
        lease_expires_at: row.get(5)?,
        worker: row.get(6)?,
        priority: row.get(7)?,
    };
    Ok(AiLease {
        job_id: raw.job_id,
        message_id: raw.message_id,
        account_id: raw.account_id,
        pass: raw.pass,
        priority: raw.priority,
        attempts: raw.attempts,
        lease_expires_at: raw.lease_expires_at,
        worker: raw.worker,
    })
}

// ---------------------------------------------------------------------------
// Rate limiting
// ---------------------------------------------------------------------------

/// A token-bucket limiter pacing calls to at most `requests_per_minute`,
/// with **no burst allowance** — the acceptance criterion is "paces rather
/// than bursts," so this bucket holds at most one token at a time rather
/// than accumulating a minute's worth upfront. A limiter that let a fresh
/// process spend a whole minute's budget in the first second would be
/// correct on average over a minute and exactly the traffic pattern an RPM
/// cap exists to prevent — Anthropic's own rate limiter sees a burst of N
/// requests in one second identically whether or not this bucket's average
/// over 60 seconds was within budget.
#[derive(Debug)]
pub struct RateLimiter {
    refill_per_sec: f64,
    state: tokio::sync::Mutex<RateLimiterState>,
}

#[derive(Debug)]
struct RateLimiterState {
    /// In `[0, 1]` — this bucket's capacity is fixed at one token.
    tokens: f64,
    last_refill: Instant,
}

impl RateLimiter {
    /// A limiter pacing at most `requests_per_minute` calls per minute. A
    /// `requests_per_minute` of `0` is a `Provider` call rate of zero —
    /// `acquire` then waits forever, which is the correct (if unusual)
    /// reading of "zero requests per minute allowed" rather than a division
    /// by zero.
    #[must_use]
    pub fn new(requests_per_minute: u32) -> Self {
        Self {
            refill_per_sec: f64::from(requests_per_minute) / 60.0,
            state: tokio::sync::Mutex::new(RateLimiterState {
                tokens: 1.0,
                last_refill: Instant::now(),
            }),
        }
    }

    /// Wait until a token is available, then consume it. Concurrent callers
    /// are not guaranteed to be served in the order they started waiting —
    /// each re-locks and re-checks after its own sleep rather than holding
    /// a place in line — but that ordering was never the property this
    /// exists to provide. What it does guarantee is the *rate*: no matter
    /// how many callers are contending, at most one token is handed out per
    /// `60 / requests_per_minute` seconds, which is what keeps the acquires
    /// paced rather than bursty regardless of which particular caller gets
    /// each one.
    pub async fn acquire(&self) {
        loop {
            let wait = {
                let mut state = self.state.lock().await;
                if self.refill_per_sec <= 0.0 {
                    // Never refills; every `acquire` after the first (free)
                    // token waits forever rather than racing a refill that
                    // never happens.
                    if state.tokens >= 1.0 {
                        state.tokens = 0.0;
                        None
                    } else {
                        Some(Duration::from_secs(u64::MAX / 2))
                    }
                } else {
                    let now = Instant::now();
                    let elapsed = now
                        .saturating_duration_since(state.last_refill)
                        .as_secs_f64();
                    state.tokens = (state.tokens + elapsed * self.refill_per_sec).min(1.0);
                    state.last_refill = now;
                    if state.tokens >= 1.0 {
                        state.tokens -= 1.0;
                        None
                    } else {
                        let deficit = 1.0 - state.tokens;
                        Some(Duration::from_secs_f64(deficit / self.refill_per_sec))
                    }
                }
            };
            match wait {
                None => return,
                Some(delay) => tokio::time::sleep(delay.max(Duration::from_millis(1))).await,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The cost gate
// ---------------------------------------------------------------------------

/// What [`CostGate::decide`] resolved to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapDecision {
    /// Under every cap; dispatch normally.
    Open,
    /// Over a cap with `on_cap = "pause"`: lease nothing this cycle.
    Paused,
    /// Over a cap with `on_cap = "triage_only"`: lease only `pass = "triage"`.
    TriageOnly,
    /// Over a cap with `on_cap = "drop"`: lease jobs of any pass, but
    /// terminate them instead of dispatching.
    Dropping,
}

/// Reads today's (and this month's) [`crate::ai::audit`] usage and applies
/// `ai.limits`' caps and `on_cap`. Consulted once per
/// [`AiWorkerPool::dispatch_pending`] cycle, before anything is leased — see
/// the module docs' "The cost gate blocks before dispatch."
#[derive(Debug)]
pub struct CostGate<'a> {
    /// The database `ai_usage` lives in.
    pub db: &'a Database,
    /// The configured caps and `on_cap` behavior.
    pub limits: &'a AiLimits,
}

impl CostGate<'_> {
    /// Resolve today's and this month's usage against the configured caps.
    ///
    /// # Errors
    /// A mapped storage error.
    pub async fn decide(&self) -> Result<CapDecision, Error> {
        let now = chrono::Utc::now().timestamp();
        let day = day_key(now);
        let month_prefix = format!("{}%", &day[..7.min(day.len())]);

        let today = audit::usage_for_day(self.db, &day).await?;
        let (today_cost, today_tokens) = today
            .map(|u| {
                (
                    u.cost_usd,
                    u.input_tokens
                        + u.output_tokens
                        + u.cache_creation_input_tokens
                        + u.cache_read_input_tokens,
                )
            })
            .unwrap_or((0.0, 0));

        let month_cost = month_cost_usd(self.db, &month_prefix).await?;

        let over_daily_cost = today_cost >= self.limits.daily_cost_cap_usd;
        let over_daily_tokens =
            u64::try_from(today_tokens).unwrap_or(u64::MAX) >= self.limits.daily_token_cap;
        let over_monthly_cost = month_cost >= self.limits.monthly_cost_cap_usd;
        let over_cap = over_daily_cost || over_daily_tokens || over_monthly_cost;

        if !over_cap {
            return Ok(CapDecision::Open);
        }
        tracing::info!(
            today_cost,
            today_tokens,
            month_cost,
            on_cap = ?self.limits.on_cap,
            "ai spend cap reached"
        );
        Ok(match self.limits.on_cap {
            OnCap::Pause => CapDecision::Paused,
            OnCap::TriageOnly => CapDecision::TriageOnly,
            OnCap::Drop => CapDecision::Dropping,
        })
    }
}

/// Sum `ai_usage.cost_usd` over every day matching `day_prefix` (`"YYYY-MM%"`)
/// — the monthly-cap input [`crate::ai::audit`] does not itself expose,
/// since its own callers (`QueryAiCalls`/cost dashboards) only ever needed
/// a single day or an explicit range, not a calendar-month rollup.
async fn month_cost_usd(db: &Database, day_prefix: &str) -> Result<f64, Error> {
    let day_prefix = day_prefix.to_owned();
    db.read(move |conn| {
        conn.query_row(
            "SELECT COALESCE(SUM(cost_usd), 0) FROM ai_usage WHERE day LIKE ?1",
            [day_prefix],
            |row| row.get(0),
        )
    })
    .await
    .map_err(Error::from)
}

/// The UTC calendar day a unix timestamp falls on, as `"YYYY-MM-DD"` — the
/// same format (and the same fallback for an out-of-range timestamp no real
/// call can produce) as `audit::day_key`, duplicated rather than imported
/// since that function is private to a module this one only calls into, not
/// shares internals with.
fn day_key(unix_ts: i64) -> String {
    chrono::DateTime::from_timestamp(unix_ts, 0)
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "1970-01-01".to_owned())
}

/// Serialize the redacted content of `request` — model, system prompt, and
/// every message's role/content — into the bytes [`crate::ai::audit::record_call`]
/// hashes as proof of what was sent.
///
/// Deliberately *not* the literal wire body [`Provider`] transmits: that is
/// a transport detail (headers, a `stream` flag, `cache_control` placement)
/// this trait does not expose to callers by design, and the ledger's job is
/// to prove the *content* was redacted, not to reproduce one specific
/// provider's exact HTTP framing. This is the one canonical serialization
/// every path that audits a call hashes — the live dispatch path,
/// [`BatchCoordinator::maybe_submit`], and (task 50) `rmaild::AiApi`'s own
/// forced `AnalyzeMessage`/`SuggestReply` calls, which run outside this
/// queue entirely but must still produce a comparable audit record. `pub`,
/// not `pub(crate)`: that third caller lives in the `rmaild` crate, not this
/// one.
pub fn payload_bytes(request: &ChatRequest) -> Vec<u8> {
    let messages: Vec<serde_json::Value> = request
        .messages
        .iter()
        .map(|m| {
            serde_json::json!({
                "role": role_str(m.role),
                "content": m.content,
            })
        })
        .collect();
    let body = serde_json::json!({
        "model": request.model,
        "system": request.system,
        "messages": messages,
    });
    serde_json::to_vec(&body).unwrap_or_default()
}

/// `provider::Role::as_str` is a private inherent method — this is the same
/// two-armed mapping, kept here rather than widening that method's
/// visibility for one call site outside its module.
fn role_str(role: crate::ai::provider::Role) -> &'static str {
    match role {
        crate::ai::provider::Role::User => "user",
        crate::ai::provider::Role::Assistant => "assistant",
    }
}

#[cfg(test)]
mod tests;
