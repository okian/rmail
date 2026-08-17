//! The operator surface over the index: what it knows, what it owes, and the
//! four verbs that change that.
//!
//! An index is a *derived artifact* — safe to drop and rebuild from the message
//! store — and every operation here is one of two things: enqueue work, or drop
//! work already done. Nothing in this module indexes anything itself. That
//! discipline is what keeps `mail index` from becoming a second, subtly
//! different indexer alongside [`crate::index::pipeline`].
//!
//! # The four verbs are not interchangeable
//!
//! [`IndexAdmin::reindex`] re-enqueues what is **stale**. Content already
//! indexed against the same hash and the same model dedups to nothing inside
//! [`IndexQueue::enqueue`], so running it over a current index costs a query
//! and changes nothing. It is the repair path for whatever
//! [`IndexAdmin::verify`] reported.
//!
//! [`IndexAdmin::rebuild`] **destroys** the derived data for the stages it is
//! given and enqueues the work to recompute it. Search over those stages
//! returns nothing until the drain catches up. It is not a stronger `reindex`;
//! it is the answer to a question `reindex` cannot address — "the extractor
//! itself changed, so everything recorded is stale but nothing *looks* stale."
//!
//! [`IndexAdmin::verify`] is read-only. It never repairs, enqueues, or deletes,
//! so it can be run against a live daemon without changing what the next search
//! returns. [`IndexAdmin::gc`] mutates, but only ever removes rows whose parent
//! is already gone.
//!
//! # Why coverage is measured against every message
//!
//! The denominator for every stage is the whole message store, not the messages
//! that already reached that stage. The tempting alternative — dividing by the
//! rows that made it to the stage's input — is always 100% by construction, and
//! a coverage meter that is structurally incapable of reporting a problem is
//! worse than none: it is a green light wired to nothing.

use std::collections::{BTreeMap, BTreeSet};

use crate::config::IndexConfig;
use crate::error::Error;
use crate::index::extract::{message_hash, Part};
use crate::index::pipeline::{IndexPauseFlag, StageSwitches};
use crate::index::semantic::{Drift as SemanticDrift, SemanticIndex};
use crate::index::{
    entities, IndexKind, IndexQueue, NewJob, QueueStats, PRIORITY_BACKFILL, PRIORITY_RECENT,
};
use crate::storage::Database;

/// How many messages one selection page reads and enqueues at a time.
///
/// Bounded so a `reindex` over a million-message mailbox is a sequence of small
/// transactions rather than one enormous one holding the single writer
/// connection every other subsystem writes through.
const SELECT_PAGE: i64 = 500;

/// Largest page [`IndexAdmin::list_entities`] will return.
pub const MAX_ENTITY_LIMIT: i64 = 500;

/// How many messages one embedding backfill pass schedules.
///
/// A cap rather than the whole backlog: re-embedding is the most expensive work
/// in the indexer, and an operator who runs `mail index embed --backfill` twice
/// gets the next slice rather than a duplicate of the first.
pub const BACKFILL_BATCH: i64 = 5_000;

// ---------------------------------------------------------------------------
// Reports
// ---------------------------------------------------------------------------

/// One stage's standing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KindStatus {
    /// Which stage.
    pub kind: IndexKind,
    /// Whether the stage is switched on in config.
    pub enabled: bool,
    /// Messages the stage could index — every message in the store.
    pub eligible: i64,
    /// Messages the stage has indexed.
    pub indexed: i64,
    /// Outstanding queue jobs for the stage.
    pub pending: i64,
    /// Quarantined jobs for the stage.
    pub quarantined: i64,
    /// Seconds between the newest message in the store and the newest message
    /// this stage has indexed. `None` when the stage has indexed nothing.
    pub lag_seconds: Option<i64>,
}

impl KindStatus {
    /// `indexed / eligible`, in `0.0..=1.0`.
    ///
    /// Zero, not one, when nothing is eligible. An empty mailbox is not fully
    /// indexed; it is unindexed and empty, and reporting 100% would make the
    /// number meaningless exactly when a first sync most needs it.
    #[must_use]
    pub fn coverage(&self) -> f64 {
        if self.eligible <= 0 {
            return 0.0;
        }
        #[allow(clippy::cast_precision_loss)]
        {
            (self.indexed as f64 / self.eligible as f64).clamp(0.0, 1.0)
        }
    }
}

/// What the index currently knows.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexStatus {
    /// One row per per-message stage, in pipeline order.
    pub kinds: Vec<KindStatus>,
    /// Messages in the store.
    pub messages: i64,
    /// Queue counts across every stage.
    pub queue: QueueStats,
    /// The configured embedding model and its width.
    pub model: String,
    /// Width of the configured embedding model.
    pub dim: i64,
    /// Chunks stored.
    pub chunks: i64,
    /// Chunk vectors stored.
    pub vectors: i64,
    /// Whether the background worker is stopped.
    pub paused: bool,
    /// Whether semantic indexing is switched on.
    pub semantic_enabled: bool,
}

/// What [`IndexAdmin::verify`] found. Every field counts rows that are wrong.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IndexDrift {
    /// `index_state` rows for a downstream stage whose recorded content hash
    /// no longer matches the text in `index_content`.
    ///
    /// This is the drift the acceptance criterion names. It is also, briefly,
    /// what an index with queued work looks like: a note added a minute ago
    /// changes the hash and the lexical job to catch up is still pending. The
    /// pending count in [`IndexStatus`] is what tells those two apart.
    pub content_hash_drift: i64,
    /// Messages with extracted content but no extract state row — nothing
    /// records that the text now stored was ever produced by a run.
    pub extract_missing: i64,
    /// Messages the lexical stage claims to have indexed that have no FTS row,
    /// despite having text to index. They are silently unfindable.
    pub lexical_missing: i64,
    /// FTS rows for messages that no longer exist.
    pub lexical_orphaned: i64,
    /// Entities with no remaining mention.
    pub entity_orphaned: i64,
    /// What the semantic index's own reconciliation found.
    pub semantic: SemanticDrift,
    /// Quarantined queue jobs. Not drift in the index itself, but the reason a
    /// stage's coverage stops climbing.
    pub quarantined: i64,
}

impl IndexDrift {
    /// Whether the index is consistent.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        *self == Self::default()
    }
}

/// What [`IndexAdmin::gc`] deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GcReport {
    /// Entities with no remaining mention.
    pub entities: u64,
    /// `vec_chunks`/`vec_messages` rows whose chunk or message is gone.
    pub vectors: u64,
    /// FTS rows for messages that no longer exist.
    pub lexical_rows: u64,
    /// `index_content` rows for messages that no longer exist.
    pub content_rows: u64,
}

impl GcReport {
    /// Total rows removed.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.entities + self.vectors + self.lexical_rows + self.content_rows
    }
}

/// What [`IndexAdmin::rebuild`] destroyed and scheduled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RebuildReport {
    /// Rows deleted by the wipe.
    pub dropped: u64,
    /// Jobs enqueued to recompute them.
    pub enqueued: u64,
}

/// Which messages a [`IndexAdmin::reindex`] pass covers.
///
/// Every field narrows; the default selects the whole store.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Selection {
    /// Stages to enqueue. Empty means [`IndexKind::PER_MESSAGE`].
    pub kinds: Vec<IndexKind>,
    /// Restrict to one account.
    pub account_id: Option<i64>,
    /// Restrict to one mailbox.
    pub mailbox_id: Option<i64>,
    /// Restrict to one message.
    pub message_id: Option<i64>,
    /// Restrict to mail no older than this (unix seconds, against the
    /// message's arrival time).
    pub since: Option<i64>,
}

impl Selection {
    /// The stages this selection asks for.
    fn kinds(&self) -> Vec<IndexKind> {
        if self.kinds.is_empty() {
            IndexKind::PER_MESSAGE.to_vec()
        } else {
            let mut kinds = self.kinds.clone();
            kinds.sort_unstable();
            kinds.dedup();
            kinds
        }
    }

    /// Mail a user is plausibly looking at right now goes to the front of the
    /// queue; a backlog walk goes behind whatever the live path is doing.
    fn priority(&self) -> i64 {
        if self.message_id.is_some() {
            PRIORITY_RECENT
        } else {
            PRIORITY_BACKFILL
        }
    }
}

/// One extracted entity, with how widely it is mentioned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityRow {
    /// Row id.
    pub entity_id: i64,
    /// Entity kind.
    pub kind: String,
    /// As written, for display.
    pub value: String,
    /// Canonical form.
    pub norm: String,
    /// Kind-specific detail as stored, JSON-encoded.
    pub meta: Option<String>,
    /// Mentions across the mailbox.
    pub mentions: i64,
    /// Distinct messages mentioning it.
    pub messages: i64,
}

// ---------------------------------------------------------------------------
// The admin surface
// ---------------------------------------------------------------------------

/// Status, verification, garbage collection and (re)build scheduling over one
/// index.
///
/// Cheap to clone: every field is a handle over one database, an `Arc`, or a
/// small copy.
#[derive(Clone)]
pub struct IndexAdmin {
    db: Database,
    queue: IndexQueue,
    semantic: SemanticIndex,
    switches: StageSwitches,
    paused: IndexPauseFlag,
}

impl std::fmt::Debug for IndexAdmin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IndexAdmin")
            .field("switches", &self.switches)
            .field("paused", &self.paused.get())
            .finish_non_exhaustive()
    }
}

impl IndexAdmin {
    /// Build the admin surface over an index.
    ///
    /// `paused` is the *same* flag the background worker reads, not a copy of
    /// its value: `status` reporting a stale pause state would make
    /// `mail index stop` look like it had failed.
    #[must_use]
    pub fn new(
        db: Database,
        queue: IndexQueue,
        semantic: SemanticIndex,
        config: &IndexConfig,
        paused: IndexPauseFlag,
    ) -> Self {
        Self {
            db,
            queue,
            semantic,
            switches: StageSwitches::from_config(config),
            paused,
        }
    }

    /// Per-stage coverage, queue depth, model, and lag.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    #[tracing::instrument(skip(self))]
    pub async fn status(&self) -> Result<IndexStatus, Error> {
        // One scan, not two. `stats` and `stats_by_kind` are the same full-table
        // group — the `next_attempt_at <= ?` expression means no index can serve
        // either — and after a first index nearly every row in `index_queue` is
        // `done`. The whole-queue totals are the sum over the stages, so asking
        // for both would read the same million rows twice to derive one from the
        // other.
        let by_kind = self.queue.stats_by_kind().await?;
        let queue = by_kind
            .values()
            .fold(QueueStats::default(), |mut total, stats| {
                total.ready += stats.ready;
                total.backing_off += stats.backing_off;
                total.leased += stats.leased;
                total.done += stats.done;
                total.dead += stats.dead;
                total
            });

        let (messages, newest, indexed, chunks, vectors) = self
            .db
            .read(|conn| {
                let messages: i64 =
                    conn.query_row("SELECT count(*) FROM messages", [], |row| row.get(0))?;
                // `created_at` is the last resort rather than a NULL: a message
                // whose server gave neither an INTERNALDATE nor a Date header
                // still arrived at a moment this daemon knows, and dropping it
                // from the maximum would make lag jump backwards whenever such
                // a message was the newest thing in the mailbox.
                let newest: Option<i64> = conn.query_row(
                    "SELECT max(coalesce(internaldate, date, created_at)) FROM messages",
                    [],
                    |row| row.get(0),
                )?;
                let mut stmt = conn.prepare(
                    "SELECT s.kind, count(*),
                            max(coalesce(m.internaldate, m.date, m.created_at))
                     FROM index_state s
                     JOIN messages m ON m.id = s.message_id
                     GROUP BY s.kind",
                )?;
                let indexed = stmt
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, Option<i64>>(2)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let chunks: i64 =
                    conn.query_row("SELECT count(*) FROM chunks", [], |row| row.get(0))?;
                let vectors: i64 =
                    conn.query_row("SELECT count(*) FROM vec_chunks", [], |row| row.get(0))?;
                Ok((messages, newest, indexed, chunks, vectors))
            })
            .await?;

        let mut per_kind: BTreeMap<IndexKind, (i64, Option<i64>)> = BTreeMap::new();
        for (kind, count, newest_indexed) in indexed {
            per_kind.insert(IndexKind::parse(&kind)?, (count, newest_indexed));
        }

        let kinds = IndexKind::PER_MESSAGE
            .into_iter()
            .map(|kind| {
                let (indexed, newest_indexed) = per_kind.get(&kind).copied().unwrap_or((0, None));
                let stats = by_kind.get(&kind).copied().unwrap_or_default();
                KindStatus {
                    kind,
                    enabled: self.switches.enabled(kind),
                    eligible: messages,
                    indexed,
                    pending: stats.outstanding(),
                    quarantined: stats.dead,
                    // Only a stage that has indexed *something* has a lag; one
                    // that has indexed nothing is not "infinitely behind," it
                    // has simply not started, which coverage already says.
                    lag_seconds: match (newest, newest_indexed) {
                        (Some(newest), Some(indexed_at)) => Some((newest - indexed_at).max(0)),
                        _ => None,
                    },
                }
            })
            .collect();

        Ok(IndexStatus {
            kinds,
            messages,
            queue,
            model: self.semantic.model().to_owned(),
            dim: i64::try_from(self.semantic.dim()).unwrap_or(i64::MAX),
            chunks,
            vectors,
            paused: self.paused.get(),
            semantic_enabled: self.switches.semantic,
        })
    }

    /// Reconcile the index against the message store and the configured model.
    ///
    /// Read-only. Nothing here repairs, enqueues, or deletes — the repair paths
    /// are [`Self::reindex`] (for drift), [`Self::gc`] (for orphans) and
    /// [`Self::rebuild`] (for everything else), and keeping them separate is
    /// what lets an operator look before they leap.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    #[tracing::instrument(skip(self))]
    pub async fn verify(&self) -> Result<IndexDrift, Error> {
        let semantic = self.semantic.verify().await?;
        let quarantined = self.queue.stats().await?.dead;

        let counts = self
            .db
            .read(|conn| {
                let one = |sql: &str| -> rusqlite::Result<i64> {
                    conn.query_row(sql, [], |row| row.get(0))
                };
                // Scoped to the parts the extract stage itself produces. A note,
                // an AI summary and an attachment's text are written by other
                // subsystems on their own schedules, and a message that has one
                // of those but has not been extracted yet is an ordinary state,
                // not drift — counting it would report a fault for every message
                // a user annotated before the indexer got to it.
                let extract_missing =
                    one("SELECT count(DISTINCT c.message_id) FROM index_content c
                     WHERE c.part IN ('subject', 'sender', 'recipients', 'body')
                       AND NOT EXISTS (SELECT 1 FROM index_state s
                                       WHERE s.message_id = c.message_id AND s.kind = 'extract')")?;
                // Scoped to messages that actually have text. `FtsIndex` removes
                // a message with no extracted text from the index rather than
                // inserting an empty document, so counting those would report
                // drift for the one case that is deliberately correct.
                let lexical_missing = one(
                    "SELECT count(*) FROM index_state s
                     WHERE s.kind = 'lexical'
                       AND EXISTS (SELECT 1 FROM index_content c
                                   WHERE c.message_id = s.message_id AND c.text <> '')
                       AND NOT EXISTS (SELECT 1 FROM fts_messages f WHERE f.rowid = s.message_id)",
                )?;
                let lexical_orphaned = one("SELECT count(*) FROM fts_messages f
                     WHERE NOT EXISTS (SELECT 1 FROM messages m WHERE m.id = f.rowid)")?;
                let entity_orphaned = one("SELECT count(*) FROM entities e
                     WHERE NOT EXISTS (SELECT 1 FROM entity_mentions m
                                       WHERE m.entity_id = e.entity_id)")?;
                let content_hash_drift = count_content_hash_drift(conn)?;
                Ok((
                    content_hash_drift,
                    extract_missing,
                    lexical_missing,
                    lexical_orphaned,
                    entity_orphaned,
                ))
            })
            .await?;

        let (
            content_hash_drift,
            extract_missing,
            lexical_missing,
            lexical_orphaned,
            entity_orphaned,
        ) = counts;
        let drift = IndexDrift {
            content_hash_drift,
            extract_missing,
            lexical_missing,
            lexical_orphaned,
            entity_orphaned,
            semantic,
            quarantined,
        };
        if !drift.is_clean() {
            tracing::info!(?drift, "index drift");
        }
        Ok(drift)
    }

    /// Delete index rows whose parent is gone.
    ///
    /// Only rows that are already unreachable: a vector whose chunk was
    /// removed, an entity nothing mentions, an FTS row for deleted mail. A row
    /// with a live parent is never touched, which is what makes this safe to
    /// run unattended — the failure mode of a garbage collector that gets this
    /// wrong is silent data loss that only shows up as search results quietly
    /// going missing.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    #[tracing::instrument(skip(self))]
    pub async fn gc(&self) -> Result<GcReport, Error> {
        let entities = entities::collect_orphans(&self.db).await?;
        let vectors = self.semantic.collect_orphans().await?;
        let (lexical_rows, content_rows) = self
            .db
            .write(|conn| {
                let tx = conn.transaction()?;
                let lexical = tx.execute(
                    "DELETE FROM fts_messages WHERE rowid IN (
                         SELECT f.rowid FROM fts_messages f
                         WHERE NOT EXISTS (SELECT 1 FROM messages m WHERE m.id = f.rowid)
                     )",
                    [],
                )?;
                // The foreign key already cascades this on a normal delete;
                // the sweep exists for a database that was written with
                // `foreign_keys` off, or restored from a partial copy.
                let content = tx.execute(
                    "DELETE FROM index_content WHERE message_id IN (
                         SELECT c.message_id FROM index_content c
                         WHERE NOT EXISTS (SELECT 1 FROM messages m WHERE m.id = c.message_id)
                     )",
                    [],
                )?;
                tx.commit()?;
                Ok((lexical as u64, content as u64))
            })
            .await?;

        let report = GcReport {
            entities,
            vectors,
            lexical_rows,
            content_rows,
        };
        if report.total() > 0 {
            tracing::info!(?report, "index garbage collected");
        }
        Ok(report)
    }

    /// What the search caches hold (task 36) — the read behind
    /// `IndexService.Status`'s `cache` field.
    ///
    /// Lives on the index admin surface rather than behind a cache-specific
    /// RPC because the corpus version answers both questions an operator has
    /// at once: how far the index has got, and which cached search results are
    /// still addressable.
    ///
    /// # Errors
    ///
    /// [`Error`] if the read fails.
    pub async fn cache_stats(&self) -> Result<crate::cache::CacheStats, Error> {
        self.db.read(crate::cache::stats).await.map_err(Error::from)
    }

    /// Garbage-collect the search caches: result pages that are expired or
    /// stranded by a corpus bump, then whatever is past each bound.
    ///
    /// Not invalidation — every row this removes is already unreachable or
    /// already a miss. It exists so the tables stay bounded without a search
    /// paying for eviction on the hot path.
    ///
    /// # Errors
    ///
    /// [`Error`] if the write fails.
    pub async fn sweep_caches(
        &self,
        config: crate::config::CacheConfig,
    ) -> Result<crate::cache::SweepReport, Error> {
        let now = chrono::Utc::now().timestamp();
        self.db
            .write(move |conn| crate::cache::sweep(conn, &config, now))
            .await
            .map_err(Error::from)
    }

    /// Drop every cached row, including compiled query plans.
    ///
    /// Destructive in the only way a cache can be: `query_plan_cache` rows
    /// each cost a paid provider call to rebuild. Nothing in normal operation
    /// calls this — see [`crate::cache::purge`].
    ///
    /// # Errors
    ///
    /// [`Error`] if the write fails.
    pub async fn purge_caches(&self) -> Result<crate::cache::PurgeReport, Error> {
        let report = self
            .db
            .write(crate::cache::purge)
            .await
            .map_err(Error::from)?;
        tracing::info!(?report, "search caches purged");
        Ok(report)
    }

    /// Enqueue the selected stages for the selected messages.
    ///
    /// Returns how many jobs were actually queued. Work already done against
    /// the same content and model dedups away inside
    /// [`IndexQueue::enqueue`], so this re-runs exactly what is stale — which
    /// over a current index is nothing at all.
    ///
    /// The extract stage is the exception, and deliberately so: nothing records
    /// a hash of a message's *source*, so "already extracted" is the only
    /// staleness this can see for it. `reindex` therefore extracts messages
    /// that have never been extracted — the catch-up path after a big sync —
    /// and [`Self::rebuild`] is the verb for "the extractor itself changed."
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    #[tracing::instrument(skip(self), fields(enqueued))]
    pub async fn reindex(&self, selection: &Selection) -> Result<u64, Error> {
        let kinds = selection.kinds();
        let priority = selection.priority();
        let model = self.semantic.model().to_owned();
        let mut enqueued = 0u64;
        let mut cursor = 0i64;

        loop {
            let page = self.select_page(selection, cursor).await?;
            let Some(&last) = page.last() else {
                break;
            };
            cursor = last;

            let hashes = self.page_hashes(&page).await?;
            let mut jobs: Vec<NewJob> = Vec::with_capacity(page.len() * kinds.len());
            for message_id in &page {
                for kind in &kinds {
                    let job = NewJob::new(*message_id, *kind).priority(priority);
                    match kind {
                        // Enqueued without a hash, matching what the queue
                        // already records for this stage. Handing it one it
                        // could not have produced would make the *next*
                        // enqueue think the message had changed, forever.
                        IndexKind::Extract | IndexKind::Thread => jobs.push(job),
                        // A message with no extracted text yet has nothing for a
                        // downstream stage to read, and no hash to dedup on. The
                        // extract job in this same batch enqueues it with the
                        // right hash once there is one.
                        _ => {
                            if let Some(hash) = hashes.get(message_id) {
                                jobs.push(job.content_hash(hash.clone()));
                            }
                        }
                    }
                }
            }
            enqueued += self.queue.enqueue(jobs, Some(&model)).await?;
        }

        tracing::Span::current().record("enqueued", enqueued);
        Ok(enqueued)
    }

    /// Schedule embedding work for messages that have been chunked and whose
    /// vectors are missing, stale, or from another model.
    ///
    /// Returns how many jobs were queued. This is the one path that has to
    /// *clear* recorded state before enqueuing, and the reason is the failure
    /// it exists to repair: a chunk whose row in `vec_chunks` has gone missing
    /// while `chunk_embeddings` still claims it was embedded is permanently
    /// dark — nothing joins to it, so it never appears in a result — and
    /// `index_state` for that message still says "semantic, this hash, this
    /// model," so an ordinary enqueue dedups the repair away. Deleting the
    /// state row is not destructive: the very next run rewrites it.
    ///
    /// # What this is not for
    ///
    /// A mailbox where `[index.semantic]` was off and has just been switched
    /// on has no `chunks` rows at all, and
    /// [`SemanticIndex::stale_messages`](crate::index::semantic::SemanticIndex::stale_messages)
    /// reads *from* `chunks` — so this returns zero for it, correctly. That
    /// case is [`Self::reindex`] with the semantic stage: those messages have
    /// no semantic state row either (the pipeline retired their jobs while the
    /// stage was off), so an ordinary enqueue schedules them.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    #[tracing::instrument(skip(self), fields(stale, enqueued))]
    pub async fn backfill_embeddings(&self) -> Result<u64, Error> {
        let stale = self.semantic.stale_messages(BACKFILL_BATCH).await?;
        if stale.is_empty() {
            return Ok(0);
        }
        // The hashes first, so the only state rows cleared are the ones that
        // actually get a job. Clearing the whole batch up front and then
        // dropping the ids with no extracted text would leave those messages
        // with neither a state row nor a job — silently *less* indexed than
        // before the repair ran.
        let hashes = self.page_hashes(&stale).await?;
        let schedulable: Vec<i64> = stale
            .iter()
            .copied()
            .filter(|id| hashes.contains_key(id))
            .collect();
        if schedulable.is_empty() {
            return Ok(0);
        }

        let ids = schedulable.clone();
        self.db
            .write(move |conn| {
                let tx = conn.transaction()?;
                {
                    let mut stmt = tx.prepare(
                        "DELETE FROM index_state WHERE message_id = ?1 AND kind = 'semantic'",
                    )?;
                    for id in &ids {
                        stmt.execute([id])?;
                    }
                }
                tx.commit()?;
                Ok(())
            })
            .await?;

        let model = self.semantic.model().to_owned();
        let jobs: Vec<NewJob> = schedulable
            .iter()
            .filter_map(|id| {
                hashes.get(id).map(|hash| {
                    NewJob::new(*id, IndexKind::Semantic)
                        .content_hash(hash.clone())
                        .priority(PRIORITY_BACKFILL)
                })
            })
            .collect();
        let enqueued = self.queue.enqueue(jobs, Some(&model)).await?;
        let span = tracing::Span::current();
        span.record("stale", stale.len());
        span.record("enqueued", enqueued);
        Ok(enqueued)
    }

    /// Drop the derived data for `kinds` and enqueue the work to recompute it.
    ///
    /// **Destructive.** Search over the affected stages returns nothing until
    /// the queue drains. An empty `kinds` means every stage.
    ///
    /// What is *not* dropped matters as much as what is: the extract stage owns
    /// four parts of `index_content` (subject, sender, recipients, body) and
    /// only those are wiped. Notes, AI summaries and attachment text are
    /// written by other subsystems on their own schedules, and a rebuild that
    /// deleted them would throw away a user's notes and minutes of OCR to
    /// re-derive text it could re-derive without them — the same scoping
    /// [`crate::index::extract`] applies to its own sweep.
    ///
    /// # The wipe and the re-enqueue are two transactions
    ///
    /// The delete commits, then the work to recompute it is queued in pages.
    /// A crash in that window leaves the derived data gone with nothing queued
    /// to bring it back, and the background worker will not notice on its own:
    /// it enqueues from `NewMail` events, and these messages synced long ago.
    /// The recovery is one command — `mail index reindex`, which finds no
    /// `index_state` rows (this deleted them) and therefore re-queues
    /// everything. Folding the enqueue into the wipe's transaction would close
    /// the window at the cost of holding the single writer connection for the
    /// whole of a million-message insert, which is the trade this declines.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    #[tracing::instrument(skip(self), fields(dropped, enqueued))]
    pub async fn rebuild(&self, kinds: &[IndexKind]) -> Result<RebuildReport, Error> {
        let kinds: Vec<IndexKind> = if kinds.is_empty() {
            IndexKind::PER_MESSAGE.to_vec()
        } else {
            let mut kinds = kinds.to_vec();
            kinds.sort_unstable();
            kinds.dedup();
            kinds
        };

        let wipe = kinds.clone();
        let mut dropped = self
            .db
            .write(move |conn| {
                let tx = conn.transaction()?;
                let mut dropped = 0u64;
                for kind in &wipe {
                    dropped += wipe_stage(&tx, *kind)?;
                    dropped += tx
                        .execute("DELETE FROM index_queue WHERE kind = ?1", [kind.as_str()])?
                        as u64;
                    dropped += tx
                        .execute("DELETE FROM index_state WHERE kind = ?1", [kind.as_str()])?
                        as u64;
                }
                tx.commit()?;
                Ok(dropped)
            })
            .await?;

        // The chunk rows are gone; their vectors live in a virtual table with
        // no foreign key, so the sweep is what actually removes them.
        if kinds.contains(&IndexKind::Semantic) {
            dropped += self.semantic.collect_orphans().await?;
        }

        // Extract cascades into every downstream stage as it stores each
        // message, so enqueuing those here as well would schedule them twice —
        // once now against text that has just been deleted, and once correctly
        // when extraction re-produces it.
        let selection = Selection {
            kinds: if kinds.contains(&IndexKind::Extract) {
                vec![IndexKind::Extract]
            } else {
                kinds.clone()
            },
            ..Selection::default()
        };
        let enqueued = self.reindex(&selection).await?;

        let span = tracing::Span::current();
        span.record("dropped", dropped);
        span.record("enqueued", enqueued);
        tracing::warn!(
            ?kinds,
            dropped,
            enqueued,
            "index rebuilt: the derived data for these stages was deleted and requeued"
        );
        Ok(RebuildReport { dropped, enqueued })
    }

    /// Entities of one kind, most widely mentioned first.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] for a kind no extractor produces — a typo'd
    /// kind and a kind with no entities are very different answers, and an
    /// empty result would report them identically. Otherwise a mapped storage
    /// error.
    #[tracing::instrument(skip(self), fields(hits))]
    pub async fn list_entities(
        &self,
        kind: &str,
        value: Option<&str>,
        limit: i64,
    ) -> Result<Vec<EntityRow>, Error> {
        // Validated against `EntityKind` rather than passed through: that enum
        // is the authority on which kinds an extractor can produce, and without
        // this an unknown kind would be answered with an empty list — reporting
        // "you asked for something that does not exist" and "nothing of that
        // kind has been found" identically. `EntityKind::parse` is not reused
        // here because it reports [`Error::Internal`], which is right for a
        // kind read back out of the database and wrong for one a user typed.
        let kind = entities::EntityKind::ALL
            .into_iter()
            .find(|known| known.as_str() == kind)
            .ok_or_else(|| {
                let known: Vec<&str> = entities::EntityKind::ALL
                    .iter()
                    .map(|k| k.as_str())
                    .collect();
                Error::invalid_argument(format!(
                    "unknown entity kind {kind:?}; known kinds are {}",
                    known.join(", ")
                ))
            })?;
        let limit = limit.clamp(1, MAX_ENTITY_LIMIT);
        // Lowercased on both sides, not left to `LIKE`'s own ASCII case
        // folding: that folding is switchable (`PRAGMA case_sensitive_like`),
        // and the norms this searches are not consistently cased — an email
        // normalizes to lower case, an invoice reference to upper. A filter
        // documented as case-insensitive should not depend on a pragma nobody
        // sets to stay that way.
        //
        // Escaped, too, and not merely parameterized. Binding stops injection;
        // it does not stop `%` and `_` from being read as wildcards, so a user
        // filtering entities for `100%` or `inv_2024` would silently get a
        // broader answer than the one they typed.
        let value = value.map(|v| {
            let escaped = v
                .to_lowercase()
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            format!("%{escaped}%")
        });

        let rows = self
            .db
            .read(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT e.entity_id, e.kind, e.value, e.norm, e.meta,
                            count(m.message_id), count(DISTINCT m.message_id)
                     FROM entities e
                     LEFT JOIN entity_mentions m ON m.entity_id = e.entity_id
                     WHERE e.kind = ?1 AND (?2 IS NULL OR lower(e.norm) LIKE ?2 ESCAPE '\\')
                     GROUP BY e.entity_id
                     ORDER BY count(m.message_id) DESC, e.entity_id
                     LIMIT ?3",
                )?;
                let rows = stmt
                    .query_map(rusqlite::params![kind.as_str(), value, limit], |row| {
                        Ok(EntityRow {
                            entity_id: row.get(0)?,
                            kind: row.get(1)?,
                            value: row.get(2)?,
                            norm: row.get(3)?,
                            meta: row.get(4)?,
                            mentions: row.get(5)?,
                            messages: row.get(6)?,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?;
        tracing::Span::current().record("hits", rows.len());
        Ok(rows)
    }

    /// One page of selected message ids, strictly after `cursor`.
    async fn select_page(&self, selection: &Selection, cursor: i64) -> Result<Vec<i64>, Error> {
        let account_id = selection.account_id;
        let mailbox_id = selection.mailbox_id;
        let message_id = selection.message_id;
        let since = selection.since;
        Ok(self
            .db
            .read(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id FROM messages
                     WHERE id > ?1
                       AND (?2 IS NULL OR account_id = ?2)
                       AND (?3 IS NULL OR mailbox_id = ?3)
                       AND (?4 IS NULL OR id = ?4)
                       AND (?5 IS NULL OR coalesce(internaldate, date, created_at) >= ?5)
                     ORDER BY id
                     LIMIT ?6",
                )?;
                let rows = stmt
                    .query_map(
                        rusqlite::params![
                            cursor,
                            account_id,
                            mailbox_id,
                            message_id,
                            since,
                            SELECT_PAGE
                        ],
                        |row| row.get(0),
                    )?
                    .collect::<rusqlite::Result<Vec<i64>>>()?;
                Ok(rows)
            })
            .await?)
    }

    /// The current content hash of each message in `page` that has extracted
    /// text.
    ///
    /// The same value [`crate::index::extract_message`] computes and the same
    /// one `NoteStore` re-derives when a note changes — one definition of "what
    /// this message currently is," which is the whole basis of the dedup.
    async fn page_hashes(&self, page: &[i64]) -> Result<BTreeMap<i64, Vec<u8>>, Error> {
        if page.is_empty() {
            return Ok(BTreeMap::new());
        }
        // The ids themselves, not the range they span. A page from
        // `reindex --account 2`, or the sparse set `backfill_embeddings` hands
        // over, can have a first and last id at opposite ends of the table —
        // and `WHERE message_id BETWEEN first AND last` then reads every part
        // of every message in between, only to throw nearly all of them away.
        // The page is bounded by `SELECT_PAGE`, so the placeholder list is too.
        let ids: Vec<i64> = page.to_vec();
        Ok(self
            .db
            .read(move |conn| {
                let placeholders = vec!["?"; ids.len()].join(",");
                let mut stmt = conn.prepare(&format!(
                    "SELECT message_id, part, content_hash FROM index_content
                     WHERE message_id IN ({placeholders})
                     ORDER BY message_id"
                ))?;
                let mut rows = stmt.query(rusqlite::params_from_iter(ids.iter()))?;
                let mut hashes: BTreeMap<i64, Vec<u8>> = BTreeMap::new();
                let mut current: Option<PartHashes> = None;
                while let Some(row) = rows.next()? {
                    let message_id: i64 = row.get(0)?;
                    let part: String = row.get(1)?;
                    let hash: Vec<u8> = row.get(2)?;
                    match &mut current {
                        Some((id, parts)) if *id == message_id => parts.push((part, hash)),
                        other => {
                            if let Some((id, parts)) = other.take() {
                                hashes.insert(id, message_hash(&parts));
                            }
                            *other = Some((message_id, vec![(part, hash)]));
                        }
                    }
                }
                if let Some((id, parts)) = current {
                    hashes.insert(id, message_hash(&parts));
                }
                Ok(hashes)
            })
            .await?)
    }
}

/// One message's part hashes, mid-fold.
///
/// A named type only because the tuple is nested enough that clippy's
/// complexity lint — rightly — asks for one.
type PartHashes = (i64, Vec<(String, Vec<u8>)>);

/// Count `index_state` rows whose recorded content hash disagrees with the text
/// now stored for that message.
///
/// Walked in one ordered pass rather than materialized: a mailbox has a row per
/// message per part, and holding all of them to compare a 32-byte hash per
/// message would make `verify` the most memory-hungry operation in the daemon.
fn count_content_hash_drift(conn: &rusqlite::Connection) -> rusqlite::Result<i64> {
    let mut stmt = conn.prepare(
        "SELECT s.message_id, s.kind, s.content_hash, c.part, c.content_hash
         FROM index_state s
         LEFT JOIN index_content c ON c.message_id = s.message_id
         WHERE s.kind IN ('lexical', 'entities', 'semantic')
         ORDER BY s.message_id",
    )?;
    let mut rows = stmt.query([])?;

    let mut drift = 0i64;
    let mut current: Option<i64> = None;
    // Both sets are keyed so the repeated rows a join produces collapse: the
    // query yields one row per (state, part) pair, and every stage sees the
    // same parts.
    let mut parts: BTreeSet<(String, Vec<u8>)> = BTreeSet::new();
    let mut states: BTreeMap<String, Option<Vec<u8>>> = BTreeMap::new();

    while let Some(row) = rows.next()? {
        let message_id: i64 = row.get(0)?;
        if current != Some(message_id) {
            drift += settle(&parts, &states);
            parts.clear();
            states.clear();
            current = Some(message_id);
        }
        states.insert(row.get(1)?, row.get(2)?);
        // NULL on both when the message has no `index_content` at all — the
        // LEFT JOIN's whole purpose, since a stage claiming to have indexed a
        // message with no text is exactly the drift being looked for.
        if let (Some(part), Some(hash)) = (
            row.get::<_, Option<String>>(3)?,
            row.get::<_, Option<Vec<u8>>>(4)?,
        ) {
            parts.insert((part, hash));
        }
    }
    drift += settle(&parts, &states);
    Ok(drift)
}

/// How many of one message's recorded stage hashes disagree with its text.
fn settle(parts: &BTreeSet<(String, Vec<u8>)>, states: &BTreeMap<String, Option<Vec<u8>>>) -> i64 {
    if states.is_empty() {
        return 0;
    }
    let owned: Vec<(String, Vec<u8>)> = parts.iter().cloned().collect();
    let expected = message_hash(&owned);
    states
        .values()
        .filter(|recorded| recorded.as_deref() != Some(expected.as_slice()))
        .count()
        .try_into()
        .unwrap_or(i64::MAX)
}

/// Delete the derived data one stage owns. Returns rows removed.
fn wipe_stage(tx: &rusqlite::Transaction<'_>, kind: IndexKind) -> rusqlite::Result<u64> {
    let dropped = match kind {
        IndexKind::Extract => {
            let mut dropped = 0u64;
            for part in Part::EXTRACTOR_OWNED {
                dropped += tx
                    .execute("DELETE FROM index_content WHERE part = ?1", [part.as_key()])?
                    as u64;
            }
            dropped
        }
        IndexKind::Lexical => {
            let rows: i64 =
                tx.query_row("SELECT count(*) FROM fts_messages", [], |row| row.get(0))?;
            // FTS5's own reset command. A `DELETE FROM` over a contentless
            // table has to walk every rowid to remove them one at a time;
            // 'delete-all' drops the index in one step, which for a full
            // rebuild of a large mailbox is the difference between seconds and
            // minutes.
            tx.execute(
                "INSERT INTO fts_messages(fts_messages) VALUES('delete-all')",
                [],
            )?;
            u64::try_from(rows).unwrap_or(0)
        }
        IndexKind::Entities => {
            let edges = tx.execute("DELETE FROM entity_edges", [])? as u64;
            let mentions = tx.execute("DELETE FROM entity_mentions", [])? as u64;
            let entities = tx.execute("DELETE FROM entities", [])? as u64;
            edges + mentions + entities
        }
        IndexKind::Semantic => {
            // The vectors first, while the rows naming them still exist: a
            // virtual table takes no foreign key, so deleting `chunks` first
            // would leave every vector unreachable by id and recoverable only
            // by the orphan sweep.
            let chunk_vectors = tx.execute(
                "DELETE FROM vec_chunks WHERE chunk_id IN (SELECT chunk_id FROM chunks)",
                [],
            )? as u64;
            // Unqualified, unlike the chunk vectors above. Scoping this to ids
            // present in `message_embeddings` would spare a centroid whose
            // bookkeeping row had already gone missing — and since that
            // message still exists, the orphan sweep would not take it either,
            // so a stale vector would survive a full rebuild of the very stage
            // that owns it.
            let message_vectors = tx.execute("DELETE FROM vec_messages", [])? as u64;
            let embeddings = tx.execute("DELETE FROM message_embeddings", [])? as u64;
            // `chunk_embeddings` cascades from `chunks`.
            let chunks = tx.execute("DELETE FROM chunks", [])? as u64;
            chunk_vectors + message_vectors + embeddings + chunks
        }
        // No derived data of its own in this schema — see
        // `IndexKind::PER_MESSAGE`.
        IndexKind::Thread => 0,
    };
    Ok(dropped)
}

#[cfg(test)]
mod tests;
