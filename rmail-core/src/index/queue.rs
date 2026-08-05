//! The durable index work queue.
//!
//! Indexing a message is four independent stages — extract, lexical, entities,
//! semantic — and they fail independently. An embeddings provider being down
//! must not stop lexical search from being built, so each stage is its own job
//! rather than a step in one job's state machine.
//!
//! # Why the queue is in SQLite and not in memory
//!
//! A mailbox is indexed once and then incrementally forever. The expensive part
//! is the first pass — hundreds of thousands of messages, hours of work — and
//! the machine doing it is a laptop that will be closed halfway through. An
//! in-memory queue would restart that from nothing. Durability here is not
//! about surviving crashes so much as surviving *ordinary use*.
//!
//! # Three rules the queue exists to enforce
//!
//! **A re-run over unchanged mail is free.** [`IndexQueue::enqueue`] compares
//! what is being asked for against [`index_state`](self) — the record of what
//! has been done, and against which content and model — and drops the job if
//! nothing has changed. This is the common case on every restart, so it has to
//! cost a query rather than a re-index. Content changing, or the embedding
//! model changing, re-runs exactly what it must and nothing else.
//!
//! **A crash returns work to the queue, not to nobody.** A leased job carries
//! an expiry. A worker that dies leaves it in the past, and
//! [`IndexQueue::reap_expired`] puts the job back. No coordinator, no
//! heartbeat protocol: the lease *is* the liveness claim, and it expires on its
//! own.
//!
//! **A poison job never blocks the queue behind it.** A job that keeps failing
//! backs off, and after [`QueueOptions::max_attempts`] it is quarantined as
//! `dead` — visible for diagnosis, invisible to workers. The ready-set index
//! is ordered so both dead and backing-off jobs are skipped without being read.
//! One message with an unparsable attachment must not stop a mailbox from
//! being indexed.

use std::time::Duration;

use rusqlite::{Connection, OptionalExtension};

use crate::error::Error;
use crate::storage::Database;

/// How long a lease is good for before the reaper may take it back.
///
/// Long enough that a slow stage — OCR over a large scan — finishes inside it;
/// short enough that a crashed worker's jobs are not stranded for an hour.
pub const DEFAULT_LEASE: Duration = Duration::from_secs(5 * 60);

/// How many times a job is retried before it is quarantined.
pub const DEFAULT_MAX_ATTEMPTS: i64 = 5;

/// The first retry delay; doubles per attempt.
pub const DEFAULT_BACKOFF: Duration = Duration::from_secs(30);

/// Ceiling on the retry delay.
pub const DEFAULT_MAX_BACKOFF: Duration = Duration::from_secs(30 * 60);

/// Default priority. Lower runs first.
pub const PRIORITY_NORMAL: i64 = 100;

/// Priority for mail a user is likely looking at right now.
pub const PRIORITY_RECENT: i64 = 10;

/// Priority for the backlog walk.
pub const PRIORITY_BACKFILL: i64 = 500;

/// A stage of indexing.
///
/// The wire strings are stored in the queue and in `index_state`, so they are
/// spelled out rather than derived.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum IndexKind {
    /// Normalize text out of the message and its parts.
    Extract,
    /// Feed the full-text index.
    Lexical,
    /// Pull out entities.
    Entities,
    /// Chunk and embed.
    Semantic,
    /// Roll a thread's index entry up.
    Thread,
}

impl IndexKind {
    /// Every stage, in the order they naturally run.
    pub const ALL: [Self; 5] = [
        Self::Extract,
        Self::Lexical,
        Self::Entities,
        Self::Semantic,
        Self::Thread,
    ];

    /// The stable wire string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Extract => "extract",
            Self::Lexical => "lexical",
            Self::Entities => "entities",
            Self::Semantic => "semantic",
            Self::Thread => "thread",
        }
    }

    /// Whether this stage's output depends on the configured embedding model.
    ///
    /// Only the embedding stage does. Comparing a model against a stage that
    /// has none is how a queue churns forever: a lexical worker naturally
    /// completes with no model, and a sweep that then passes one would find
    /// every lexical row stale on every restart, for every message.
    #[must_use]
    pub fn uses_model(self) -> bool {
        matches!(self, Self::Semantic)
    }

    /// Parse a wire string.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] for a string no version of this code wrote — a queue
    /// written by a newer build, which is a deployment problem.
    pub fn parse(value: &str) -> Result<Self, Error> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str() == value)
            .ok_or_else(|| Error::internal(format!("unknown index kind in queue: {value}")))
    }
}

/// Where a job stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    /// Waiting for a worker.
    Pending,
    /// Held by a worker until its lease expires.
    Leased,
    /// Finished.
    Done,
    /// Quarantined after too many failures. Never leased again; kept so the
    /// failure is visible rather than silently dropped.
    Dead,
}

impl JobState {
    /// The stable wire string.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Leased => "leased",
            Self::Done => "done",
            Self::Dead => "dead",
        }
    }

    /// Parse a wire string.
    ///
    /// # Errors
    /// [`Error::Internal`] for an unrecognized state.
    pub fn parse(value: &str) -> Result<Self, Error> {
        match value {
            "pending" => Ok(Self::Pending),
            "leased" => Ok(Self::Leased),
            "done" => Ok(Self::Done),
            "dead" => Ok(Self::Dead),
            other => Err(Error::internal(format!(
                "unknown job state in queue: {other}"
            ))),
        }
    }
}

/// A unit of indexing work to queue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewJob {
    /// The message to index.
    pub message_id: i64,
    /// Which stage.
    pub kind: IndexKind,
    /// What is being indexed. Compared against `index_state` to decide whether
    /// the work is needed at all.
    pub content_hash: Option<Vec<u8>>,
    /// Lower runs first.
    pub priority: i64,
}

impl NewJob {
    /// A job at [`PRIORITY_NORMAL`] with no content hash.
    #[must_use]
    pub fn new(message_id: i64, kind: IndexKind) -> Self {
        Self {
            message_id,
            kind,
            content_hash: None,
            priority: PRIORITY_NORMAL,
        }
    }

    /// Set the content hash the re-index decision compares.
    #[must_use]
    pub fn content_hash(mut self, hash: impl Into<Vec<u8>>) -> Self {
        self.content_hash = Some(hash.into());
        self
    }

    /// Set the priority.
    #[must_use]
    pub fn priority(mut self, priority: i64) -> Self {
        self.priority = priority;
        self
    }
}

/// A leased job: work a worker now owns until its lease expires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lease {
    /// Queue row id, used to complete or fail the job.
    pub job_id: i64,
    /// The message to index.
    pub message_id: i64,
    /// Which stage.
    pub kind: IndexKind,
    /// What is being indexed.
    pub content_hash: Option<Vec<u8>>,
    /// How many times this job has been attempted, including this one.
    pub attempts: i64,
    /// When the lease lapses (unix seconds).
    pub lease_expires_at: i64,
    /// Who holds this lease. Carried so [`IndexQueue::complete`] and
    /// [`IndexQueue::fail`] can refuse to act on a job that has since been
    /// reaped and handed to someone else.
    pub worker: String,
}

/// What happened to a failed job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failure {
    /// It will be retried after a backoff.
    Retrying {
        /// When it becomes eligible again (unix seconds).
        next_attempt_at: i64,
        /// How many attempts it has now had.
        attempts: i64,
    },
    /// It exhausted its attempts and was quarantined.
    Quarantined {
        /// How many attempts it had.
        attempts: i64,
    },
}

/// A quarantined job, for diagnosis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeadLetter {
    /// Queue row id, for [`IndexQueue::revive`].
    pub job_id: i64,
    /// The message that could not be indexed.
    pub message_id: i64,
    /// Which stage failed.
    pub kind: IndexKind,
    /// The last failure. `None` means it was quarantined without one, which
    /// only a lapsed final lease does.
    pub last_error: Option<String>,
}

/// A count of jobs by state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct QueueStats {
    /// Waiting for a worker, and eligible now.
    pub ready: i64,
    /// Waiting, but backing off after a failure.
    pub backing_off: i64,
    /// Held by a worker.
    pub leased: i64,
    /// Finished.
    pub done: i64,
    /// Quarantined.
    pub dead: i64,
}

impl QueueStats {
    /// Jobs still to do: ready plus backing off plus leased.
    #[must_use]
    pub fn outstanding(&self) -> i64 {
        self.ready + self.backing_off + self.leased
    }
}

/// Tuning for a queue.
#[derive(Debug, Clone, Copy)]
pub struct QueueOptions {
    /// How long a lease is good for.
    pub lease: Duration,
    /// Attempts before quarantine.
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
    /// The delay before attempt `attempts`, doubling and capped.
    fn backoff_for(&self, attempts: i64) -> Duration {
        let shift = u32::try_from(attempts.max(1) - 1)
            .unwrap_or(u32::MAX)
            .min(32);
        self.backoff
            .saturating_mul(1u32.checked_shl(shift).unwrap_or(u32::MAX))
            .min(self.max_backoff)
    }
}

/// The durable index work queue.
///
/// Cheap to clone: every clone shares one database handle.
#[derive(Debug, Clone)]
pub struct IndexQueue {
    db: Database,
    opts: QueueOptions,
}

impl IndexQueue {
    /// Open a queue over `db`.
    #[must_use]
    pub fn new(db: Database, opts: QueueOptions) -> Self {
        Self { db, opts }
    }

    /// Queue work, skipping anything already done against the same content and
    /// model.
    ///
    /// Returns how many jobs were actually queued. `model` is the embedding
    /// model now configured; a job whose recorded state names a different one
    /// is stale however unchanged its content, which is what makes a model
    /// switch re-embed exactly the affected stages.
    ///
    /// # Errors
    ///
    /// A mapped storage error. A job naming a message that does not exist is
    /// skipped rather than failing the batch — sync and indexing race, and a
    /// message deleted between the two is not an error.
    #[tracing::instrument(skip(self, jobs), fields(count = jobs.len(), queued))]
    pub async fn enqueue(&self, jobs: Vec<NewJob>, model: Option<&str>) -> Result<u64, Error> {
        if jobs.is_empty() {
            return Ok(0);
        }
        let model = model.map(str::to_owned);
        let queued = self
            .db
            .write(move |conn| {
                let tx = conn.transaction()?;
                let mut seen = std::collections::HashSet::new();
                let mut queued = 0u64;
                for job in jobs {
                    if enqueue_one(&tx, &job, model.as_deref())?
                        && seen.insert((job.message_id, job.kind))
                    {
                        queued += 1;
                    }
                }
                tx.commit()?;
                Ok(queued)
            })
            .await?;
        tracing::Span::current().record("queued", queued);
        tracing::debug!(queued, "index jobs enqueued");
        Ok(queued)
    }

    /// Take up to `limit` jobs, best first, leasing them to `worker`.
    ///
    /// Ordering is priority then arrival, so recent mail is indexed before the
    /// archive and a user searching straight after a sync finds today's mail.
    /// Dead and backing-off jobs are skipped by the index rather than read and
    /// discarded — which is what stops a poison job blocking the queue behind
    /// it.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    #[tracing::instrument(skip(self), fields(leased))]
    pub async fn lease(&self, worker: &str, limit: i64) -> Result<Vec<Lease>, Error> {
        if limit <= 0 {
            return Ok(Vec::new());
        }
        let worker = worker.to_owned();
        let lease_secs = i64::try_from(self.opts.lease.as_secs()).unwrap_or(i64::MAX);
        let leased = self
            .db
            .write(move |conn| {
                let now = chrono::Utc::now().timestamp();
                let tx = conn.transaction()?;
                // Select and claim in one transaction: two workers polling at
                // the same moment must not both take the same job, and the
                // single writer connection makes that free.
                let candidates: Vec<i64> = {
                    let mut stmt = tx.prepare(
                        "SELECT job_id FROM index_queue
                         WHERE state = 'pending' AND next_attempt_at <= ?1
                         ORDER BY priority, enqueued_at, job_id
                         LIMIT ?2",
                    )?;
                    let rows = stmt
                        .query_map(rusqlite::params![now, limit], |row| row.get(0))?
                        .collect::<rusqlite::Result<Vec<i64>>>()?;
                    rows
                };

                let mut leased = Vec::with_capacity(candidates.len());
                {
                    let mut claim = tx.prepare(
                        "UPDATE index_queue
                         SET state = 'leased',
                             attempts = attempts + 1,
                             lease_expires_at = ?2,
                             leased_by = ?3,
                             updated_at = unixepoch()
                         WHERE job_id = ?1
                         RETURNING job_id, message_id, kind, content_hash, attempts,
                                   lease_expires_at, leased_by",
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
        // The row carries a wire string; a value this build cannot parse is a
        // corrupt queue and surfaces as such.
        let leased: Result<Vec<Lease>, Error> = leased.into_iter().map(TryInto::try_into).collect();
        let leased = leased?;
        tracing::Span::current().record("leased", leased.len());
        Ok(leased)
    }

    /// Mark a leased job done and record what it indexed.
    ///
    /// Returns whether the lease still held. A worker whose lease was reaped —
    /// because it stalled past the expiry — no longer owns the job, and letting
    /// it write `index_state` anyway would record work under whoever holds the
    /// job now, and mark indexed a version of the content nobody indexed.
    ///
    /// What is recorded is the hash from the *lease*, not from the queue row.
    /// The row can have been rewritten by a sync sweep while the worker was
    /// running; reading it back would record content the worker never saw and
    /// make the next enqueue of that content dedup to nothing — the message
    /// silently unindexable from then on.
    ///
    /// The `index_state` write shares the completion's transaction: a crash
    /// between them would leave a job reporting success and re-running forever.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    #[tracing::instrument(skip(self, lease), fields(job_id = lease.job_id, kind = lease.kind.as_str()))]
    pub async fn complete(&self, lease: &Lease, model: Option<&str>) -> Result<bool, Error> {
        // Only stages that use one record a model; storing the configured
        // embedding model against a lexical row would make the next sweep find
        // it stale for a reason that has nothing to do with it.
        let model = lease
            .kind
            .uses_model()
            .then(|| model.map(str::to_owned))
            .flatten();
        let job_id = lease.job_id;
        let worker = lease.worker.clone();
        let message_id = lease.message_id;
        let kind = lease.kind.as_str();
        let content_hash = lease.content_hash.clone();
        let held = self
            .db
            .write(move |conn| {
                let tx = conn.transaction()?;
                let changed = tx.execute(
                    "UPDATE index_queue
                     SET state = 'done', lease_expires_at = NULL, leased_by = NULL,
                         last_error = NULL, updated_at = unixepoch()
                     WHERE job_id = ?1 AND state = 'leased' AND leased_by = ?2",
                    rusqlite::params![job_id, worker],
                )?;
                if changed == 0 {
                    tx.commit()?;
                    return Ok(false);
                }
                tx.execute(
                    "INSERT INTO index_state (message_id, kind, content_hash, model, indexed_at)
                     VALUES (?1, ?2, ?3, ?4, unixepoch())
                     ON CONFLICT(message_id, kind) DO UPDATE SET
                         content_hash = excluded.content_hash,
                         model = excluded.model,
                         indexed_at = excluded.indexed_at",
                    rusqlite::params![message_id, kind, content_hash, model],
                )?;
                tx.commit()?;
                Ok(true)
            })
            .await?;
        if !held {
            tracing::warn!(
                job_id,
                "completed a job this worker no longer holds; the lease was \
                 reaped and the work will be redone by its new owner"
            );
        }
        Ok(held)
    }

    /// Record a failure on a leased job, backing it off or quarantining it.
    ///
    /// Returns `None` if the lease no longer held. Fenced for the same reason
    /// as [`Self::complete`], plus one of its own: an unfenced failure applies
    /// a backoff and an attempt to whatever occupies that row now, which for a
    /// job re-enqueued at [`PRIORITY_RECENT`] means the message the user is
    /// looking at sits out a backoff it never earned.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    #[tracing::instrument(skip(self, lease, error), fields(job_id = lease.job_id, kind = lease.kind.as_str()))]
    pub async fn fail(&self, lease: &Lease, error: &str) -> Result<Option<Failure>, Error> {
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
                        "SELECT attempts FROM index_queue
                         WHERE job_id = ?1 AND state = 'leased' AND leased_by = ?2",
                        rusqlite::params![job_id, worker],
                        |row| row.get(0),
                    )
                    .optional()?;
                let Some(attempts) = attempts else {
                    return Ok(None);
                };

                let outcome = if attempts >= opts.max_attempts {
                    tx.execute(
                        "UPDATE index_queue
                         SET state = 'dead', lease_expires_at = NULL, leased_by = NULL,
                             last_error = ?2, updated_at = unixepoch()
                         WHERE job_id = ?1",
                        rusqlite::params![job_id, error],
                    )?;
                    Failure::Quarantined { attempts }
                } else {
                    let delay =
                        i64::try_from(opts.backoff_for(attempts).as_secs()).unwrap_or(i64::MAX);
                    let next_attempt_at = now.saturating_add(delay);
                    tx.execute(
                        "UPDATE index_queue
                         SET state = 'pending', lease_expires_at = NULL, leased_by = NULL,
                             next_attempt_at = ?2, last_error = ?3, updated_at = unixepoch()
                         WHERE job_id = ?1",
                        rusqlite::params![job_id, next_attempt_at, error],
                    )?;
                    Failure::Retrying {
                        next_attempt_at,
                        attempts,
                    }
                };
                tx.commit()?;
                Ok(Some(outcome))
            })
            .await?;
        match &outcome {
            Some(Failure::Quarantined { attempts }) => tracing::warn!(
                job_id,
                message_id = lease.message_id,
                kind = lease.kind.as_str(),
                attempts,
                error = %logged,
                "index job quarantined after repeated failures"
            ),
            Some(Failure::Retrying { attempts, .. }) => {
                tracing::debug!(job_id, attempts, error = %logged, "index job will be retried");
            }
            None => tracing::warn!(job_id, "failed a job this worker no longer holds"),
        }
        Ok(outcome)
    }

    /// Return jobs whose lease has lapsed to the queue.
    ///
    /// Called on startup and periodically. A worker that died mid-job left its
    /// lease in the past; this is what makes that job someone else's problem
    /// rather than nobody's. The attempt count is *not* rolled back — a job
    /// that repeatedly kills its worker is exactly the kind that should
    /// eventually be quarantined.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    #[tracing::instrument(skip(self))]
    pub async fn reap_expired(&self) -> Result<u64, Error> {
        let opts = self.opts;
        let reclaimed = self
            .db
            .write(move |conn| {
                let now = chrono::Utc::now().timestamp();
                let tx = conn.transaction()?;
                // A lease that lapsed while the job was already at its limit
                // goes straight to quarantine rather than round the loop again.
                let quarantined = tx.execute(
                    "UPDATE index_queue
                     SET state = 'dead', lease_expires_at = NULL, leased_by = NULL,
                         last_error = 'lease expired after the final attempt',
                         updated_at = unixepoch()
                     WHERE state = 'leased' AND lease_expires_at <= ?1 AND attempts >= ?2",
                    rusqlite::params![now, opts.max_attempts],
                )?;
                let returned = tx.execute(
                    "UPDATE index_queue
                     SET state = 'pending', lease_expires_at = NULL, leased_by = NULL,
                         last_error = 'lease expired', updated_at = unixepoch()
                     WHERE state = 'leased' AND lease_expires_at <= ?1",
                    [now],
                )?;
                tx.commit()?;
                Ok(quarantined + returned)
            })
            .await?;
        if reclaimed > 0 {
            tracing::info!(reclaimed, "reclaimed index jobs from expired leases");
        }
        Ok(reclaimed as u64)
    }

    /// Move a quarantined job back to the queue, clearing its attempts.
    ///
    /// The manual escape hatch: a job quarantined by a bug that has since been
    /// fixed should not need the message re-synced to be retried.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    pub async fn revive(&self, job_id: i64) -> Result<bool, Error> {
        let changed = self
            .db
            .write(move |conn| {
                conn.execute(
                    "UPDATE index_queue
                     SET state = 'pending', attempts = 0, next_attempt_at = 0,
                         last_error = NULL, updated_at = unixepoch()
                     WHERE job_id = ?1 AND state = 'dead'",
                    [job_id],
                )
            })
            .await?;
        Ok(changed > 0)
    }

    /// Count jobs by state.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    pub async fn stats(&self) -> Result<QueueStats, Error> {
        let stats = self
            .db
            .read(|conn| {
                let now = chrono::Utc::now().timestamp();
                let mut stats = QueueStats::default();
                let mut stmt = conn.prepare(
                    "SELECT state, next_attempt_at <= ?1, count(*)
                     FROM index_queue GROUP BY state, next_attempt_at <= ?1",
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
                        "dead" => stats.dead += count,
                        // Dropping this silently would under-report
                        // `outstanding()`, and a queue that looks drained while
                        // work sits in it is the worst answer available — worse
                        // than an error, because nobody goes looking.
                        other => unknown = unknown.or_else(|| Some(other.to_owned())),
                    }
                }
                Ok((stats, unknown))
            })
            .await?;
        let (stats, unknown) = stats;
        if let Some(state) = unknown {
            return Err(Error::internal(format!(
                "unknown job state in queue: {state}"
            )));
        }
        Ok(stats)
    }

    /// Quarantined jobs, newest failure first, for diagnosis.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    pub async fn dead_letters(&self, limit: i64) -> Result<Vec<DeadLetter>, Error> {
        let rows: Vec<(i64, i64, String, Option<String>)> = self
            .db
            .read(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT job_id, message_id, kind, last_error FROM index_queue
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
        rows.into_iter()
            .map(|(job_id, message_id, kind, last_error)| {
                Ok(DeadLetter {
                    job_id,
                    message_id,
                    kind: IndexKind::parse(&kind)?,
                    last_error,
                })
            })
            .collect()
    }
}

/// Queue one job unless it is already done against the same content and model.
///
/// Returns whether it was queued.
fn enqueue_one(conn: &Connection, job: &NewJob, model: Option<&str>) -> rusqlite::Result<bool> {
    let kind = job.kind.as_str();

    // Sync and indexing race: a message deleted between the two is not an
    // error, and the foreign key would reject the insert anyway.
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM messages WHERE id = ?1)",
        [job.message_id],
        |row| row.get(0),
    )?;
    if !exists {
        return Ok(false);
    }

    let done: Option<(Option<Vec<u8>>, Option<String>)> = conn
        .query_row(
            "SELECT content_hash, model FROM index_state WHERE message_id = ?1 AND kind = ?2",
            rusqlite::params![job.message_id, kind],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;

    if let Some((indexed_hash, indexed_model)) = done {
        // The whole point of the state table. Unchanged content indexed by the
        // model now configured is work that does not need doing, which on every
        // restart is nearly all of it.
        let content_same = indexed_hash == job.content_hash;
        // Only for stages that actually depend on a model. Comparing one
        // against a lexical row — which naturally has none — would find every
        // such row stale on every restart.
        let model_same = !job.kind.uses_model() || indexed_model.as_deref() == model;
        if content_same && model_same {
            return Ok(false);
        }
    }

    let changed = conn.execute(
        "INSERT INTO index_queue
             (message_id, kind, priority, content_hash, state, attempts, next_attempt_at)
         VALUES (?1, ?2, ?3, ?4, 'pending', 0, 0)
         ON CONFLICT(message_id, kind) DO UPDATE SET
             -- Re-enqueuing an outstanding job updates what it is for rather
             -- than queueing it twice, and takes the more urgent priority: a
             -- message the user just opened outranks the backfill entry that
             -- happened to get there first.
             priority = MIN(index_queue.priority, excluded.priority),
             content_hash = excluded.content_hash,
             state = 'pending',
             attempts = 0,
             next_attempt_at = 0,
             lease_expires_at = NULL,
             leased_by = NULL,
             last_error = NULL,
             updated_at = unixepoch()
         WHERE
             -- A quarantined job stays quarantined for the content that
             -- poisoned it. Without this, every sync sweep un-quarantines every
             -- poison job — a dead row never wrote `index_state`, so the dedup
             -- above can never short-circuit it — and the queue burns
             -- `max_attempts` on the same broken message forever. New content
             -- is a different question and does earn a fresh attempt.
             index_queue.state <> 'dead'
             OR index_queue.content_hash IS NOT excluded.content_hash",
        rusqlite::params![job.message_id, kind, job.priority, job.content_hash],
    )?;
    Ok(changed > 0)
}

/// A leased row before its wire strings are parsed.
struct RawLease {
    job_id: i64,
    message_id: i64,
    kind: String,
    content_hash: Option<Vec<u8>>,
    attempts: i64,
    lease_expires_at: i64,
    worker: String,
}

impl TryFrom<RawLease> for Lease {
    type Error = Error;

    fn try_from(raw: RawLease) -> Result<Self, Error> {
        Ok(Self {
            job_id: raw.job_id,
            message_id: raw.message_id,
            kind: IndexKind::parse(&raw.kind)?,
            content_hash: raw.content_hash,
            attempts: raw.attempts,
            lease_expires_at: raw.lease_expires_at,
            worker: raw.worker,
        })
    }
}

fn lease_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawLease> {
    Ok(RawLease {
        job_id: row.get(0)?,
        message_id: row.get(1)?,
        kind: row.get(2)?,
        content_hash: row.get(3)?,
        attempts: row.get(4)?,
        lease_expires_at: row.get(5)?,
        worker: row.get(6)?,
    })
}

#[cfg(test)]
mod tests;
