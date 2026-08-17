//! Caching & incrementality for the search path (prd.md, "Caching &
//! Incrementality"; task 36).
//!
//! # The three caches, and which one each half of this module owns
//!
//! prd.md names three. They are at very different layers, so they are three
//! separate mechanisms rather than one generic store:
//!
//! 1. **Query-plan cache** — NL→plan compiles keyed by normalized query hash.
//!    Already built: `query_plan_cache` (migration V47), owned by
//!    [`crate::query::compile`]. Nothing here duplicates it; [`stats`] reports
//!    it and [`purge`] can clear it, because an operator asking "what is
//!    cached?" should not have to know which task built which table.
//! 2. **Embedding cache** — "query and document embeddings persisted;
//!    documents re-embedded only on `content_hash` change." The document half
//!    already existed too, as `chunk_embeddings.content_hash` in
//!    [`crate::index::semantic`]. [`embed`] adds the query half.
//! 3. **Result cache** — `(query, filter, corpus_version)` → ranked ids.
//!    [`result`], new here, along with the [`corpus`] version it keys on.
//!
//! # One invalidation rule, applied three times
//!
//! A cache that can return a stale answer is worse than no cache, and search
//! relevance is this product's first feature — so no cache in this module is
//! invalidated by deleting rows when the world changes. Deletion-based
//! invalidation is a line of code that every future write path has to
//! remember, and the path that forgets does not fail a test: it quietly serves
//! yesterday's results.
//!
//! Instead every entry is **content-addressed on everything that could change
//! its answer**:
//!
//! | cache | key includes | so it misses when |
//! |---|---|---|
//! | query plan | account, normalized query | the question changes |
//! | embedding | model, dim, hash of the truncated text | the model or the text changes |
//! | result | request, [`corpus::CorpusStamp::version`], [`result::RankerFingerprint`] | mail changes, or *any* `[search]` knob or the embedding model changes |
//!
//! Deletion is then only ever garbage collection — [`sweep`]'s TTL and LRU
//! bounds, and [`purge`]'s operator override — and neither is load-bearing for
//! correctness.
//!
//! # Incrementality is measured, not asserted
//!
//! The point of all three is work *not done*: a provider call not made, a
//! model not run, a pipeline not re-executed. Every test in this module's
//! `tests` counts that work with a counting fake or a raw row count, because a
//! test showing a second call returns the same answer proves only that the
//! code is correct twice.

use rusqlite::Connection;

use crate::config::CacheConfig;

pub mod corpus;
pub mod embed;
pub mod result;

#[cfg(test)]
mod tests;

pub use corpus::CorpusStamp;
pub use embed::{CachingEmbedder, EmbeddingCache};
pub use result::{
    BypassReason, Lease, Lookup, RankerFingerprint, ResultCache, ResultKey, ResultKeyParts,
};

/// What the search caches hold right now — the operator's read-only view.
///
/// Reported through `IndexService.Status`, because a cache nobody outside this
/// crate can inspect is a capability with no surface: an operator debugging
/// "why is search still returning the old ordering" needs to see the corpus
/// version and the entry counts without attaching a debugger to the daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CacheStats {
    /// The current corpus version — the number every result-cache entry is
    /// keyed on.
    pub corpus_version: i64,
    /// Unix seconds at the last corpus change.
    pub corpus_changed_at: i64,
    /// Rows in `query_plan_cache` (task 58's NL→plan compiles).
    pub query_plans: i64,
    /// Reads those rows served without a provider call.
    pub query_plan_uses: i64,
    /// Rows in `embedding_cache` (query vectors).
    pub embeddings: i64,
    /// Reads those rows served without an embedder call.
    pub embedding_uses: i64,
    /// Rows in `search_result_cache`.
    pub results: i64,
    /// Reads those rows served without running the pipeline.
    pub result_uses: i64,
    /// Result-cache rows stamped with a superseded corpus version — entries
    /// that can never be read again and are waiting for [`sweep`]. A number
    /// that only grows is the visible symptom of a sweep that is not running.
    pub stale_results: i64,
}

/// What [`sweep`] deleted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SweepReport {
    /// Query vectors dropped for being coldest past the LRU bound.
    pub embeddings: u64,
    /// Result pages dropped for being expired, superseded, or coldest past
    /// the LRU bound.
    pub results: u64,
}

impl SweepReport {
    /// Total rows removed.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.embeddings + self.results
    }
}

/// Read every counter in [`CacheStats`].
///
/// # Errors
///
/// Propagates any `rusqlite` error.
pub fn stats(conn: &Connection) -> rusqlite::Result<CacheStats> {
    let stamp = corpus::read(conn)?;
    let (query_plans, query_plan_uses) = conn.query_row(
        "SELECT count(*), coalesce(sum(uses), 0) FROM query_plan_cache",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let (embeddings, embedding_uses) = conn.query_row(
        "SELECT count(*), coalesce(sum(uses), 0) FROM embedding_cache",
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let (results, result_uses, stale_results) = conn.query_row(
        "SELECT count(*), coalesce(sum(uses), 0),
                coalesce(sum(corpus_version != ?1), 0)
           FROM search_result_cache",
        rusqlite::params![stamp.version],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    Ok(CacheStats {
        corpus_version: stamp.version,
        corpus_changed_at: stamp.changed_at,
        query_plans,
        query_plan_uses,
        embeddings,
        embedding_uses,
        results,
        result_uses,
        stale_results,
    })
}

/// Garbage-collect both caches this module owns: expired and superseded
/// result pages, then whatever is past each LRU bound.
///
/// None of this is invalidation — every row it removes is already unreachable
/// or already a miss (see the module docs). It exists so the tables stay
/// bounded on disk without a search having to pay for eviction on the hot
/// path, which is why `IndexService.Gc` runs it.
///
/// # Errors
///
/// Propagates any `rusqlite` error.
pub fn sweep(
    conn: &mut Connection,
    config: &CacheConfig,
    now: i64,
) -> rusqlite::Result<SweepReport> {
    let tx = conn.transaction()?;
    let stamp = corpus::read(&tx)?;

    // Superseded: keyed on a corpus version that is no longer current, so no
    // lookup can ever address them again.
    let mut results = tx.execute(
        "DELETE FROM search_result_cache WHERE corpus_version != ?1",
        rusqlite::params![stamp.version],
    )?;
    // Expired by TTL.
    results += tx.execute(
        "DELETE FROM search_result_cache WHERE ?1 - created_at >= ?2",
        rusqlite::params![now, i64::from(config.result_ttl_secs)],
    )?;
    // Coldest past the bound.
    results += tx.execute(
        "DELETE FROM search_result_cache
          WHERE cache_key IN (
              SELECT cache_key FROM search_result_cache
               ORDER BY last_used_at DESC
               LIMIT -1 OFFSET ?1
          )",
        rusqlite::params![i64::from(config.max_results)],
    )?;

    let embeddings = tx.execute(
        "DELETE FROM embedding_cache
          WHERE (model, dim, text_hash) IN (
              SELECT model, dim, text_hash FROM embedding_cache
               ORDER BY last_used_at DESC
               LIMIT -1 OFFSET ?1
          )",
        rusqlite::params![i64::from(config.max_embeddings)],
    )?;

    tx.commit()?;
    Ok(SweepReport {
        embeddings: embeddings as u64,
        results: results as u64,
    })
}

/// What [`purge`] deleted, per cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PurgeReport {
    /// `query_plan_cache` rows. Reported separately from the rest because
    /// these are the expensive ones: each was a paid Claude call, and
    /// rebuilding them costs money, not milliseconds.
    pub query_plans: u64,
    /// `embedding_cache` rows.
    pub embeddings: u64,
    /// `search_result_cache` rows.
    pub results: u64,
}

impl PurgeReport {
    /// Total rows removed.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.query_plans + self.embeddings + self.results
    }
}

/// Drop every cached row, unconditionally — the operator's "I do not trust
/// what is in there" button, reachable as `mail index gc --purge-caches`.
///
/// Nothing in normal operation calls this, and nothing needs to: every cache
/// here invalidates structurally (see the module docs). It exists because
/// "you can inspect it but you cannot clear it" is not an operator surface,
/// and because a corrupted database restored from backup is a real reason to
/// want a clean slate.
///
/// # Errors
///
/// Propagates any `rusqlite` error.
pub fn purge(conn: &mut Connection) -> rusqlite::Result<PurgeReport> {
    let tx = conn.transaction()?;
    let query_plans = tx.execute("DELETE FROM query_plan_cache", [])?;
    let embeddings = tx.execute("DELETE FROM embedding_cache", [])?;
    let results = tx.execute("DELETE FROM search_result_cache", [])?;
    tx.commit()?;
    Ok(PurgeReport {
        query_plans: query_plans as u64,
        embeddings: embeddings as u64,
        results: results as u64,
    })
}
