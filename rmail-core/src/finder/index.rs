//! The only place the finder touches SQLite: building `finder_index`,
//! draining `finder_dirty` into the in-memory store, and seeding
//! `finder_commands`.
//!
//! # Everything slow lives here, on its own timer
//!
//! [`super::Finder`] answers a keystroke from memory and holds no
//! `Database`. This type holds the `Database` and holds no query path. The
//! split is the design: the expensive half (folding text, six derivation
//! queries, a write transaction) runs every `finder.refresh_interval_ms`
//! on a background task, and the cheap half runs on every character typed.
//! Neither can accidentally become the other, because neither has the other's
//! inputs.
//!
//! # The drain is coalesced, capped, and idempotent
//!
//! A full mailbox resync writes one `finder_dirty` row per message touched,
//! several times over. [`FinderIndex::drain`] reads at most
//! `finder.max_drain_batch` rows, coalesces them by `(kind, ref_id)` so a row
//! touched forty times costs one re-fold, applies the result, and deletes
//! everything up to the highest sequence it read. Applying the same batch
//! twice is harmless — upserts are upserts and a delete of an absent row is a
//! no-op — which matters because a crash between "apply" and "delete the feed
//! rows" must leave the index correct, not merely recoverable.
//!
//! # ...and it reconciles, because the feed cannot be relied on to be
//! exhaustive
//!
//! SQLite documents that foreign-key cascade actions fire triggers only when
//! `recursive_triggers` is on, and this database does not set it (see
//! `storage::configure_writer`). Deleting an account removes its
//! `finder_index` rows — the cascade itself always happens — but on that
//! reading nothing would be written to `finder_dirty`, and the in-memory
//! store would keep serving entries for mail that no longer exists. Observed
//! behavior on the SQLite this build links is more generous than the
//! documentation promises, which is exactly the kind of thing not to depend
//! on.
//!
//! So the drain does not depend on it: every [`RECONCILE_EVERY_PASSES`]
//! passes it checks whether the store still *is* the set of rows a fresh load
//! would produce, and reloads when it is not. That also self-heals every
//! *other* way the two could drift — a future task writing `finder_index`
//! directly, a bug in this file — which a fix aimed only at cascades would
//! not. See [`FinderIndex::reconcile_needed`] for why the check compares
//! identity rather than a row count, and what went wrong when it did not.
//! It runs once every thirty seconds at the default interval, not per tick.

use std::collections::BTreeMap;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use chrono::Utc;
use rusqlite::{Connection, OptionalExtension};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::config::FinderConfig;
use crate::error::Error;
use crate::keymap::Action;
use crate::storage::Database;

use super::fold;
use super::rank::Signals;
use super::score::MAX_MATCH_CHARS;
use super::store::{Entry, FinderStore, Limits};
use super::ItemKind;

/// How many drain passes run between full store/table reconciliations. At the
/// default 250 ms interval this is once every 30 seconds — cheap enough to be
/// invisible, frequent enough that a cascade-deleted account stops being
/// findable long before anyone notices it is gone.
pub const RECONCILE_EVERY_PASSES: u64 = 120;

/// `finder_dirty.op`.
const OP_UPSERT: i64 = 0;
const OP_DELETE: i64 = 1;

/// What one drain pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DrainReport {
    /// Feed rows read (before coalescing).
    pub rows: usize,
    /// Entries upserted into the store.
    pub upserted: usize,
    /// Entries removed from the store.
    pub deleted: usize,
    /// Whether this pass reloaded the whole store.
    pub reloaded: bool,
}

/// A snapshot of the index's health, for `IndexStatus`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IndexStatus {
    /// Entries resident in memory.
    pub entries: usize,
    /// Measured heap bytes those entries occupy.
    pub bytes: usize,
    /// Feed rows waiting to be drained.
    pub pending: i64,
    /// Admissions a cap has refused since the last full load — non-zero
    /// means the index is deliberately incomplete.
    pub rejected: u64,
    /// Unix seconds of the last successful refresh, or 0 if never.
    pub refreshed_at: i64,
}

/// Owns the durable index and the in-memory store it feeds.
///
/// # One index per store, and why `refresh` is on it
///
/// Clone this rather than building a second one over the same store: the
/// `refresh` mutex is what keeps two refreshes from interleaving, and a
/// second [`FinderIndex::new`] would mint a second mutex that serializes
/// nothing. The daemon builds exactly one (`rmaild::serve`) and clones it
/// into the service and the drain loop.
#[derive(Clone)]
pub struct FinderIndex {
    db: Database,
    store: Arc<RwLock<FinderStore>>,
    config: FinderConfig,
    limits: Limits,
    /// Held across a whole refresh — the database read *and* the store swap.
    ///
    /// [`FinderIndex::load`] is a read followed by a write with no
    /// transaction spanning them, so without this the two halves of two
    /// refreshes could interleave and the *older* snapshot could land last,
    /// `clear()`ing a correct store and repopulating it from a table state
    /// that no longer existed. That is a silent loss of index entries, not a
    /// stale read: the mail stays unfindable until something else triggers a
    /// reload. See `a_stale_load_cannot_clobber_a_newer_one`.
    refresh: Arc<tokio::sync::Mutex<()>>,
    /// Test-only: released between a load's read and its store swap, so a
    /// test can hold a snapshot open across another refresh. There is no
    /// other way to construct that interleaving from outside — it is decided
    /// by which blocking task happens to acquire the store lock first.
    #[cfg(test)]
    pause_after_read: Option<Arc<tokio::sync::Notify>>,
}

impl FinderIndex {
    /// Build the index side over `db`, writing into `store`.
    #[must_use]
    pub fn new(db: Database, store: Arc<RwLock<FinderStore>>, config: &FinderConfig) -> Self {
        Self {
            db,
            store,
            config: config.clone(),
            limits: Limits::from_config(config),
            refresh: Arc::new(tokio::sync::Mutex::new(())),
            #[cfg(test)]
            pause_after_read: None,
        }
    }

    /// Test-only: pause this handle's next load between its read and its swap.
    #[cfg(test)]
    fn pausing_after_read(mut self, gate: Arc<tokio::sync::Notify>) -> Self {
        self.pause_after_read = Some(gate);
        self
    }

    /// The store this index maintains.
    #[must_use]
    pub fn store(&self) -> &Arc<RwLock<FinderStore>> {
        &self.store
    }

    /// Register every keymap action as a palette command.
    ///
    /// Seeded from `keymap::Action::ALL` rather than a list kept here, so a
    /// command the palette can run is by construction an action a key can be
    /// bound to and `mail keys` can print — prd.md's "action ids shared by
    /// palette/gRPC/MCP", enforced by there being one list rather than two.
    /// Actions that have disappeared between releases are removed, so a
    /// renamed binding does not leave an unrunnable palette entry behind.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    pub async fn seed_commands(&self) -> Result<usize, Error> {
        let commands: Vec<(&'static str, &'static str)> = Action::ALL
            .iter()
            .map(|action| (action.id(), action.describe()))
            .collect();
        let written = self
            .db
            .write(move |conn| {
                let tx = conn.transaction()?;
                {
                    let mut upsert = tx.prepare(
                        "INSERT INTO finder_commands (name, keywords, action) VALUES (?1, ?2, ?3) \
                         ON CONFLICT(action) DO UPDATE SET name = excluded.name, \
                         keywords = excluded.keywords",
                    )?;
                    for (id, describe) in &commands {
                        // The action id doubles as keywords: a user who
                        // half-remembers `message.archive` should find it by
                        // typing `archive`, and a user who remembers the help
                        // text should find it by typing that.
                        upsert.execute(rusqlite::params![describe, id, id])?;
                    }
                    let keep: Vec<String> =
                        commands.iter().map(|(id, _)| (*id).to_owned()).collect();
                    // `NOT IN ()` is a syntax error, and an empty action
                    // registry would otherwise mean "delete every command" —
                    // exactly the wrong reading of a registry that failed to
                    // populate. `Action::ALL` is never empty today; this is
                    // what keeps that from being load-bearing.
                    if !keep.is_empty() {
                        let placeholders = vec!["?"; keep.len()].join(",");
                        let mut stale = tx.prepare(&format!(
                            "DELETE FROM finder_commands WHERE action NOT IN ({placeholders})"
                        ))?;
                        let bound: Vec<&dyn rusqlite::ToSql> =
                            keep.iter().map(|k| k as &dyn rusqlite::ToSql).collect();
                        stale.execute(bound.as_slice())?;
                    }
                }
                tx.commit()?;
                Ok(commands.len())
            })
            .await?;
        Ok(written)
    }

    /// Rebuild `finder_index` from scratch, then reload the store.
    ///
    /// Truncates the change feed as part of the same transaction. That is
    /// not merely an optimization: clearing `finder_index` fires its delete
    /// trigger once per row, so a rebuild of a large mailbox would otherwise
    /// hand the drain a backlog exactly as large as the index it just
    /// rebuilt, describing changes that are already applied.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    #[tracing::instrument(skip(self), fields(entries))]
    pub async fn rebuild(&self) -> Result<usize, Error> {
        // Held across the table write *and* the reload, so a refresh that
        // began earlier cannot apply its older snapshot on top of this one.
        let _refresh = self.refresh.lock().await;
        self.seed_commands().await?;
        let snippet_max = self.config.snippet_max_bytes as usize;
        let written = self
            .db
            .write(move |conn| {
                let tx = conn.transaction()?;
                tx.execute("DELETE FROM finder_index", [])?;
                let mut written = 0usize;
                for kind in ItemKind::ALL {
                    written += insert_kind(&tx, kind, snippet_max, None)?;
                }
                tx.execute("DELETE FROM finder_dirty", [])?;
                tx.commit()?;
                Ok(written)
            })
            .await?;
        let loaded = self.load_locked().await?;
        tracing::Span::current().record("entries", loaded);
        tracing::info!(written, loaded, "rebuilt the finder index");
        Ok(loaded)
    }

    /// Load `finder_index` into a fresh in-memory store, newest first.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    pub async fn load(&self) -> Result<usize, Error> {
        let _refresh = self.refresh.lock().await;
        self.load_locked().await
    }

    /// [`FinderIndex::load`]'s body, for callers already holding `refresh`.
    ///
    /// `tokio::sync::Mutex` is not reentrant, so `rebuild` — which holds the
    /// lock across its table write and its reload — must call this rather
    /// than `load`.
    async fn load_locked(&self) -> Result<usize, Error> {
        let limits = self.limits;
        let entries = self
            .db
            .read(move |conn| read_entries(conn, limits.max_entries))
            .await?;
        #[cfg(test)]
        if let Some(gate) = self.pause_after_read.clone() {
            gate.notified().await;
        }
        let store = Arc::clone(&self.store);
        let now = Utc::now().timestamp();
        let loaded = tokio::task::spawn_blocking(move || {
            let mut guard = store.write().unwrap_or_else(PoisonError::into_inner);
            guard.clear();
            for entry in entries {
                if !guard.upsert(entry, &limits) {
                    // The caps bind newest-first, so everything after the
                    // first refusal is older still: stopping is the same
                    // outcome as continuing, minus the work.
                    break;
                }
            }
            guard.mark_refreshed(now);
            guard.len()
        })
        .await
        .map_err(|error| Error::internal(format!("the finder store load task failed: {error}")))?;
        Ok(loaded)
    }

    /// Apply one bounded batch of the change feed.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    pub async fn drain(&self, pass: u64) -> Result<DrainReport, Error> {
        // A drain consumes feed rows and then applies them to the store. If a
        // reload landed between those two steps it would either lose the
        // batch or double-apply it, so a drain is one refresh like any other.
        let _refresh = self.refresh.lock().await;
        let batch = self.config.max_drain_batch.max(1) as i64;
        let snippet_max = self.config.snippet_max_bytes as usize;
        let changed = self
            .db
            .write(move |conn| apply_feed(conn, batch, snippet_max))
            .await?;

        let mut report = DrainReport {
            rows: changed.rows,
            ..DrainReport::default()
        };

        if !changed.upserts.is_empty() || !changed.deletes.is_empty() {
            let store = Arc::clone(&self.store);
            let limits = self.limits;
            let now = Utc::now().timestamp();
            let upserts = changed.upserts;
            let deletes = changed.deletes;
            let (upserted, deleted) = tokio::task::spawn_blocking(move || {
                let mut guard = store.write().unwrap_or_else(PoisonError::into_inner);
                let mut upserted = 0usize;
                for entry in upserts {
                    if guard.upsert(entry, &limits) {
                        upserted += 1;
                    }
                }
                for (kind, ref_id) in &deletes {
                    guard.remove(*kind, *ref_id);
                }
                guard.mark_refreshed(now);
                (upserted, deletes.len())
            })
            .await
            .map_err(|error| {
                Error::internal(format!("the finder store drain task failed: {error}"))
            })?;
            report.upserted = upserted;
            report.deleted = deleted;
        }

        if pass > 0 && pass % RECONCILE_EVERY_PASSES == 0 && self.reconcile_needed().await? {
            self.load_locked().await?;
            report.reloaded = true;
        }
        Ok(report)
    }

    /// Whether the store still describes what the table holds.
    ///
    /// # A count is not enough, and nearly shipped as one
    ///
    /// The obvious check — compare `COUNT(*)` against `FinderStore::len` — is
    /// wrong in both directions on a *capped* store, which is precisely the
    /// large mailbox this safety net was written for. The store's length is
    /// pinned at the cap whatever the table holds, so a store full of entries
    /// for deleted mail has exactly the same count as a correct one; and the
    /// table legitimately holds more rows than the store, so any inequality
    /// is ambiguous. An earlier draft "solved" that by giving up whenever
    /// anything had ever been turned away — which disabled reconciliation
    /// *permanently*, because `rejected` is cleared only by
    /// [`FinderIndex::load`], which is what this gates.
    ///
    /// So the comparison is over *identity*, not size. The store is by
    /// construction a prefix of the table in load order (newest first,
    /// stopping at the first refusal), so this reads the first
    /// `FinderStore::len` rows in that exact order and compares their
    /// `item_id` sum against [`FinderStore::checksum`]. Equal means the store
    /// is exactly the prefix it should be; unequal means something removed,
    /// replaced or reordered rows without the feed saying so. When nothing
    /// was capped away, the total row count is compared as well, so a table
    /// that grew behind the feed's back is caught too.
    ///
    /// The read is bounded by the resident entry count — the same number of
    /// rows the store already holds — and runs once every
    /// [`RECONCILE_EVERY_PASSES`] passes, i.e. once every 30 s at the default
    /// interval.
    async fn reconcile_needed(&self) -> Result<bool, Error> {
        // On the blocking pool for the reason `status` gives.
        let store = Arc::clone(&self.store);
        let (resident, checksum, capped) = tokio::task::spawn_blocking(move || {
            let guard = store.read().unwrap_or_else(PoisonError::into_inner);
            (guard.len(), guard.checksum(), guard.rejected() > 0)
        })
        .await
        .map_err(|error| Error::internal(format!("the finder reconcile task failed: {error}")))?;

        let limit = i64::try_from(resident).unwrap_or(i64::MAX);
        let (prefix_rows, prefix_sum, total) = self
            .db
            .read(move |conn| {
                let (rows, sum) = conn.query_row(
                    "SELECT COUNT(*), COALESCE(SUM(item_id), 0) FROM \
                     (SELECT item_id FROM finder_index \
                      ORDER BY COALESCE(last_activity, 0) DESC, item_id DESC LIMIT ?1)",
                    [limit],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
                )?;
                let total: i64 =
                    conn.query_row("SELECT COUNT(*) FROM finder_index", [], |row| row.get(0))?;
                Ok((rows, sum, total))
            })
            .await?;

        let prefix_rows = usize::try_from(prefix_rows).unwrap_or(usize::MAX);
        let total = usize::try_from(total).unwrap_or(usize::MAX);
        Ok(prefix_rows != resident || prefix_sum != checksum || (!capped && total != resident))
    }

    /// A snapshot for `IndexStatus`.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    pub async fn status(&self) -> Result<IndexStatus, Error> {
        let pending: i64 = self
            .db
            .read(|conn| conn.query_row("SELECT COUNT(*) FROM finder_dirty", [], |row| row.get(0)))
            .await?;
        // On the blocking pool, like every other place this lock is taken.
        // `std::sync::RwLock::read` parks the *thread*, and `load()`'s write
        // guard spans a clear plus up to `max_entries` upserts — so taking
        // this on a runtime worker would let a reload stall an unrelated
        // task for as long as a full reindex takes.
        let store = Arc::clone(&self.store);
        tokio::task::spawn_blocking(move || {
            let guard = store.read().unwrap_or_else(PoisonError::into_inner);
            IndexStatus {
                entries: guard.len(),
                bytes: guard.footprint(),
                pending,
                rejected: guard.rejected(),
                refreshed_at: guard.refreshed_at(),
            }
        })
        .await
        .map_err(|error| Error::internal(format!("the finder status task failed: {error}")))
    }

    /// Run the drain loop until `cancel` fires.
    ///
    /// The first thing it does is a full load — not a drain — because a cold
    /// daemon has a populated `finder_index` and an empty store, and a feed
    /// that describes only what has changed since cannot bridge that.
    #[must_use]
    pub fn spawn(self, cancel: CancellationToken) -> JoinHandle<()> {
        let span = tracing::info_span!("finder_drain");
        tokio::spawn(
            async move {
                self.run(cancel).await;
            }
            .instrument(span),
        )
    }

    /// Load the store, building `finder_index` first if it is empty.
    ///
    /// A migration establishes the table but cannot populate it — the triggers
    /// only see changes made *after* they exist — so the first daemon start
    /// after V38 has a mailbox full of mail and an index full of nothing.
    /// Rebuilding on an empty index is what closes that, and it costs nothing
    /// on a genuinely empty mailbox.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    pub async fn ensure_built(&self) -> Result<usize, Error> {
        let loaded = self.load().await?;
        if loaded > 0 {
            return Ok(loaded);
        }
        self.rebuild().await
    }

    async fn run(self, cancel: CancellationToken) {
        let interval = Duration::from_millis(self.config.refresh_interval_ms.max(1));
        match self.ensure_built().await {
            Ok(entries) => tracing::info!(entries, "loaded the finder index"),
            Err(error) => {
                tracing::warn!(%error, "the finder index could not be loaded; it will fill in from the change feed");
            }
        }
        let mut pass: u64 = 0;
        loop {
            tokio::select! {
                () = cancel.cancelled() => break,
                () = tokio::time::sleep(interval) => {}
            }
            pass = pass.wrapping_add(1);
            match self.drain(pass).await {
                Ok(report) if report.rows > 0 || report.reloaded => {
                    tracing::debug!(
                        rows = report.rows,
                        upserted = report.upserted,
                        deleted = report.deleted,
                        reloaded = report.reloaded,
                        "drained the finder change feed"
                    );
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(%error, "a finder drain pass failed; retrying next tick");
                }
            }
        }
        tracing::info!("the finder drain loop stopped");
    }
}

/// One coalesced batch of the feed, already re-derived.
struct FeedBatch {
    rows: usize,
    upserts: Vec<Entry>,
    deletes: Vec<(ItemKind, i64)>,
}

/// Read, coalesce and apply up to `batch` feed rows. Runs on the writer.
fn apply_feed(
    conn: &mut Connection,
    batch: i64,
    snippet_max: usize,
) -> rusqlite::Result<FeedBatch> {
    let tx = conn.transaction()?;
    // Coalesced by (kind, ref_id) with the *last* op winning: a message
    // created and deleted inside one drain window is a delete, and one
    // touched forty times is one re-fold.
    let mut pending: BTreeMap<(i64, i64), i64> = BTreeMap::new();
    let mut rows = 0usize;
    let mut high_seq = 0i64;
    {
        let mut stmt =
            tx.prepare("SELECT seq, kind, ref_id, op FROM finder_dirty ORDER BY seq LIMIT ?1")?;
        let mut cursor = stmt.query([batch])?;
        while let Some(row) = cursor.next()? {
            let seq: i64 = row.get(0)?;
            let kind: i64 = row.get(1)?;
            let ref_id: i64 = row.get(2)?;
            let op: i64 = row.get(3)?;
            pending.insert((kind, ref_id), op);
            high_seq = high_seq.max(seq);
            rows += 1;
        }
    }
    if rows == 0 {
        return Ok(FeedBatch {
            rows: 0,
            upserts: Vec::new(),
            deletes: Vec::new(),
        });
    }

    let mut upserts = Vec::new();
    let mut deletes = Vec::new();
    for ((kind_code, ref_id), op) in pending {
        // A kind this build does not know is a row from a newer schema:
        // skipped, not guessed at.
        let Some(kind) = ItemKind::from_code(kind_code) else {
            continue;
        };
        // Anything that is not an explicit upsert is treated as a delete. The
        // column only ever holds 0 or 1 (the triggers are the only writers),
        // and deleting on an unrecognized op is the fail-safe direction: it
        // drops an entry that will be re-derived on the source row's next
        // touch, rather than resurrecting one from a row that may be gone.
        if op != OP_UPSERT {
            // No `debug_assert` that `op == OP_DELETE`: the comment above
            // says an unrecognized op fails *safe*, and an assertion would
            // make it panic instead — in exactly the builds (debug, test)
            // where a corrupt feed row is most likely to be encountered.
            tx.execute(
                "DELETE FROM finder_index WHERE kind = ?1 AND ref_id = ?2",
                rusqlite::params![kind_code, ref_id],
            )?;
            deletes.push((kind, ref_id));
            continue;
        }
        let written = insert_kind(&tx, kind, snippet_max, Some(ref_id))?;
        if written == 0 {
            // The source row is gone (deleted between the trigger firing and
            // this drain, or never existed). Removing here rather than
            // waiting for a delete event is what prd.md's "stale ref ...
            // entry pruned next drain" describes.
            tx.execute(
                "DELETE FROM finder_index WHERE kind = ?1 AND ref_id = ?2",
                rusqlite::params![kind_code, ref_id],
            )?;
            deletes.push((kind, ref_id));
            continue;
        }
        if let Some(entry) = read_entry(&tx, kind, ref_id)? {
            upserts.push(entry);
        }
    }

    // Everything up to `high_seq` has been applied.
    tx.execute("DELETE FROM finder_dirty WHERE seq <= ?1", [high_seq])?;

    // ...and so are the echoes this pass's own `DELETE FROM finder_index`
    // statements just produced through `finder_dirty_index_delete`. Leaving
    // them for the next pass is not merely wasteful, it is wrong: they carry
    // a `seq` *higher* than any feed row this pass left unread past its
    // `LIMIT`, and the next pass coalesces by "last seq wins". So an upsert
    // that was cut off by the batch cap would be overridden by this pass's
    // echo of a delete for the same `(kind, ref_id)` — and because
    // `messages.id` is an `INTEGER PRIMARY KEY` without `AUTOINCREMENT`,
    // SQLite reuses row ids, which makes "the same ref_id, now a different
    // message" reachable rather than theoretical. The result would be a live
    // message that is silently unindexed until something touches its source
    // row again. Deleting the echo inside the same transaction that caused it
    // removes the whole class.
    //
    // Deleting a *genuine* concurrent delete event for the same ref along
    // with it is harmless: the row is already gone from `finder_index`, so
    // re-applying it would be a no-op.
    if !deletes.is_empty() {
        let mut echo = tx.prepare(
            "DELETE FROM finder_dirty \
             WHERE seq > ?1 AND op = ?2 AND kind = ?3 AND ref_id = ?4",
        )?;
        for (kind, ref_id) in &deletes {
            echo.execute(rusqlite::params![high_seq, OP_DELETE, kind.code(), ref_id])?;
        }
    }
    tx.commit()?;
    Ok(FeedBatch {
        rows,
        upserts,
        deletes,
    })
}

/// Derive `finder_index` rows for one kind, either all of them (`only =
/// None`) or one (`only = Some(ref_id)`). Returns how many rows were written.
///
/// One statement per kind rather than a union: each kind reads different
/// columns from a different table with a different notion of "last activity",
/// and a union of six `SELECT`s padded to a common shape is harder to read
/// and no faster.
fn insert_kind(
    conn: &Connection,
    kind: ItemKind,
    snippet_max: usize,
    only: Option<i64>,
) -> rusqlite::Result<usize> {
    let (select, filter_col) = match kind {
        // `snippet_max` is interpolated rather than bound because it changes
        // the *statement*, and `prepare_cached` keys its cache on the SQL
        // text: a bound parameter would be fine here, but the cap comes from
        // config and never varies within a process, so baking it in costs one
        // cache entry and keeps the parameter numbering identical across all
        // six kinds. It is a `usize` from a typed config field, so there is
        // nothing to escape.
        ItemKind::Message => (message_select(snippet_max), "m.id"),
        ItemKind::Mailbox => (MAILBOX_SELECT.to_owned(), "b.id"),
        ItemKind::Contact => (CONTACT_SELECT.to_owned(), "c.id"),
        ItemKind::SavedSearch => (SAVED_SEARCH_SELECT.to_owned(), "s.id"),
        ItemKind::Tag => (TAG_SELECT.to_owned(), "t.id"),
        ItemKind::Command => (COMMAND_SELECT.to_owned(), "k.id"),
    };
    let sql = match only {
        Some(_) => format!("{select} WHERE {filter_col} = ?1"),
        None => select,
    };
    // `prepare_cached`, not `prepare`: `apply_feed` calls this once per
    // coalesced ref, so at the default `max_drain_batch` of 2000 a plain
    // `prepare` would recompile two statements up to two thousand times per
    // drain pass — every 250 ms, holding the single writer connection.
    let mut stmt = conn.prepare_cached(&sql)?;
    let mut cursor = match only {
        Some(ref_id) => stmt.query([ref_id])?,
        None => stmt.query([])?,
    };

    let mut upsert = conn.prepare_cached(
        "INSERT INTO finder_index \
         (kind, ref_id, account_id, mailbox_id, primary_text, secondary, snippet, match_blob, \
          last_activity, is_unread, importance, frequency, updated_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, unixepoch()) \
         ON CONFLICT(kind, ref_id) DO UPDATE SET \
           account_id = excluded.account_id, mailbox_id = excluded.mailbox_id, \
           primary_text = excluded.primary_text, secondary = excluded.secondary, \
           snippet = excluded.snippet, match_blob = excluded.match_blob, \
           last_activity = excluded.last_activity, is_unread = excluded.is_unread, \
           importance = excluded.importance, frequency = excluded.frequency, \
           updated_at = unixepoch()",
    )?;

    let mut written = 0usize;
    while let Some(row) = cursor.next()? {
        let ref_id: i64 = row.get(0)?;
        let account_id: Option<i64> = row.get(1)?;
        let mailbox_id: Option<i64> = row.get(2)?;
        let primary: String = row.get(3)?;
        let secondary: String = row.get(4)?;
        let snippet: String = row.get(5)?;
        let last_activity: Option<i64> = row.get(6)?;
        let unread: i64 = row.get(7)?;
        let importance: f64 = row.get(8)?;
        let frequency: i64 = row.get(9)?;
        upsert.execute(rusqlite::params![
            kind.code(),
            ref_id,
            account_id,
            mailbox_id,
            primary,
            secondary,
            truncate_chars(&snippet, snippet_max),
            build_blob(&primary, &secondary),
            last_activity,
            unread,
            importance,
            frequency,
        ])?;
        written += 1;
    }
    Ok(written)
}

/// The folded text stored in `finder_index.match_blob`: primary, then
/// secondary.
///
/// Capped at [`MAX_MATCH_CHARS`] here — at *write* time — so the aligner's
/// per-candidate cost has a fixed ceiling that no mailbox's contents can
/// raise, and so the cap is paid once per message rather than once per
/// keystroke. [`Entry::new`] applies the identical cap when it builds its own
/// in-memory copy; the column exists so a cold start does not have to re-fold
/// 100k subject lines to find out what it already knew.
fn build_blob(primary: &str, secondary: &str) -> String {
    let mut blob = fold::fold(primary);
    if !secondary.is_empty() {
        blob.push(' ');
        blob.push_str(&fold::fold(secondary));
    }
    if blob.chars().count() > MAX_MATCH_CHARS {
        blob = blob.chars().take(MAX_MATCH_CHARS).collect();
    }
    blob
}

/// Truncate to at most `max` bytes without splitting a character.
///
/// `finder.snippet_max_bytes` is stated in bytes (it is a memory budget), but
/// a byte cut through a multi-byte character produces a string SQLite will
/// store and every renderer downstream will have to defend against. Snapping
/// down to the nearest boundary spends at most three bytes to keep the value
/// valid UTF-8 by construction.
fn truncate_chars(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.get(..end).unwrap_or_default().to_owned()
}

/// The columns every `finder_index` read selects, in one place so
/// [`read_entries`] and [`read_entry`] cannot disagree about their order.
///
/// `snippet` is deliberately absent: see [`Entry`]'s own docs on why body
/// text stays on disk rather than in the store's 25 MB budget.
const ENTRY_COLS: &str = "item_id, kind, COALESCE(ref_id, 0), COALESCE(account_id, 0), \
     COALESCE(mailbox_id, 0), primary_text, COALESCE(secondary, ''), \
     COALESCE(last_activity, 0), is_unread, importance, frequency";

/// Load the store's worth of entries, newest first.
///
/// Reads `max_entries + 1` rows on purpose. The store enforces the cap itself
/// — that is what makes it a cap rather than a convention — but a `LIMIT` of
/// exactly `max_entries` would hand it precisely as many rows as it will
/// accept, so it would never *refuse* one and `IndexStatus.rejected` would
/// report a truncated index as a complete one. The extra row is the signal:
/// the store turns it away, bumps its counter, and [`FinderIndex::load`]
/// stops.
fn read_entries(conn: &Connection, max_entries: usize) -> rusqlite::Result<Vec<Entry>> {
    let limit = i64::try_from(max_entries)
        .unwrap_or(i64::MAX)
        .saturating_add(1);
    // `COALESCE` rather than `NULLS LAST`, which needs SQLite 3.30. Commands
    // have no activity time and sort last either way, which is right: they
    // are ranked by their kind prior, not by when they happened.
    let mut stmt = conn.prepare(&format!(
        "SELECT {ENTRY_COLS} FROM finder_index \
         ORDER BY COALESCE(last_activity, 0) DESC, item_id DESC LIMIT ?1"
    ))?;
    let mut cursor = stmt.query([limit])?;
    let mut out = Vec::new();
    while let Some(row) = cursor.next()? {
        if let Some(entry) = entry_from_row(row)? {
            out.push(entry);
        }
    }
    Ok(out)
}

/// Load one entry by identity.
fn read_entry(conn: &Connection, kind: ItemKind, ref_id: i64) -> rusqlite::Result<Option<Entry>> {
    conn.query_row(
        &format!("SELECT {ENTRY_COLS} FROM finder_index WHERE kind = ?1 AND ref_id = ?2"),
        rusqlite::params![kind.code(), ref_id],
        entry_from_row,
    )
    .optional()
    .map(Option::flatten)
}

/// Build an [`Entry`] from a `finder_index` row.
///
/// The folded blob is rebuilt by [`Entry::new`] rather than read from
/// `match_blob`, even though the column holds exactly the same string. That
/// is what lets an entry keep *one* buffer for ASCII rows (see [`Entry`]'s
/// docs): reading the column would hand this function a second `String` it
/// would then have to compare and usually throw away. `match_blob` earns its
/// place by being what a future consumer — a SQL-side prefilter, a
/// diagnostic, a `RebuildIndex` that wants to check its own work — can read
/// without this module, not by being on this path.
fn entry_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Option<Entry>> {
    let kind_code: i64 = row.get(1)?;
    let Some(kind) = ItemKind::from_code(kind_code) else {
        return Ok(None);
    };
    let primary_text: String = row.get(5)?;
    let secondary: String = row.get(6)?;
    let last_activity: i64 = row.get(7)?;
    let unread: i64 = row.get(8)?;
    Ok(Some(Entry::new(
        row.get(0)?,
        kind,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        &primary_text,
        &secondary,
        &Signals {
            last_activity: (last_activity != 0).then_some(last_activity),
            unread: unread != 0,
            importance: row.get(9)?,
            frequency: row.get(10)?,
        },
    )))
}

// ---------------------------------------------------------------------------
// per-kind derivations
// ---------------------------------------------------------------------------
//
// Every one of these selects the same twelve values in the same order:
// ref_id, account_id, mailbox_id, primary, secondary, snippet, last_activity,
// is_unread, importance, frequency.

/// Messages: the subject, with sender as the second line.
///
/// `importance` is `\Flagged` today — the one importance signal that exists
/// locally and needs no model. Task 81's priority tiers are a strictly
/// better input for this column and can replace the expression without
/// touching anything else here.
///
/// # `substr` on the body, not the body
///
/// The snippet is capped at `snippet_max` bytes, and the naive
/// `COALESCE(m.body_text, '')` materialized the *entire* body into a Rust
/// `String` before the cap was applied — for every message, inside the
/// transaction that holds the single writer connection. `repo` already
/// documents that anti-pattern twice ("would read the entire mail corpus off
/// disk to look at six strings"), and here it was paid on two paths that
/// matter: `ensure_built()`'s automatic first-run rebuild over the whole
/// mailbox, and every drain pass during a resync, contending with the sync
/// engine for the writer.
///
/// `substr(x, 1, n)` counts *characters*, and the cap is in bytes, so this
/// asks for `snippet_max` characters — at most `4 × snippet_max` bytes, still
/// a bounded read — and [`truncate_chars`] then applies the real byte cap on
/// a string that is already small.
fn message_select(snippet_max: usize) -> String {
    format!(
        "SELECT m.id, m.account_id, m.mailbox_id, \
         COALESCE(m.subject, ''), \
         TRIM(COALESCE(m.from_name, '') || ' ' || COALESCE(m.from_addr, '')), \
         COALESCE(substr(m.body_text, 1, {snippet_max}), ''), \
         COALESCE(m.date, m.internaldate), \
         CASE WHEN EXISTS (SELECT 1 FROM flags f WHERE f.message_id = m.id AND f.flag = '\\Seen') \
              THEN 0 ELSE 1 END, \
         CASE WHEN EXISTS (SELECT 1 FROM flags f WHERE f.message_id = m.id AND f.flag = '\\Flagged') \
              THEN 1.0 ELSE 0.0 END, \
         0 \
         FROM messages m"
    )
}

/// Mailboxes: the full path, with its message count as the frequency signal
/// and its newest message as its recency.
const MAILBOX_SELECT: &str = "SELECT b.id, b.account_id, NULL, b.name, '', '', \
     (SELECT MAX(COALESCE(m.date, m.internaldate)) FROM messages m WHERE m.mailbox_id = b.id), \
     0, 0.0, \
     (SELECT COUNT(*) FROM messages m WHERE m.mailbox_id = b.id) \
     FROM mailboxes b";

/// Contacts: display name first, address second. `contacts` is not per
/// account (one address is one person however many accounts saw them), so
/// `account_id` is NULL and the account filter lets them through — see
/// `super::matches_filters`.
const CONTACT_SELECT: &str = "SELECT c.id, NULL, NULL, \
     COALESCE(NULLIF(c.name, ''), c.address), c.address, '', \
     c.last_seen, 0, 0.0, c.message_count \
     FROM contacts c";

/// Saved searches: the name, with the query text as the second line.
const SAVED_SEARCH_SELECT: &str = "SELECT s.id, s.account_id, NULL, s.name, s.query, '', \
     s.updated_at, 0, 0.0, 0 \
     FROM saved_searches s";

/// Tags: the full hierarchical name.
const TAG_SELECT: &str = "SELECT t.id, t.account_id, NULL, t.name, '', '', \
     t.created_at, 0, 0.0, \
     (SELECT COUNT(*) FROM message_tags mt WHERE mt.tag_id = t.id) \
     FROM tags t";

/// Commands: the human title, with the action id as the second line so
/// typing either finds it.
const COMMAND_SELECT: &str = "SELECT k.id, NULL, NULL, k.name, k.action, \
     COALESCE(k.keywords, ''), NULL, 0, 0.0, 0 \
     FROM finder_commands k";

#[cfg(test)]
mod tests;
