//! The dense retriever: cosine kNN over `vec_chunks`, deduped chunk→message
//! keeping both max and mean chunk similarity (prd.md's "chunk-level dense
//! retrieval" note under Stage 1).
//!
//! # A fresh, cancellable kNN query rather than `SemanticIndex::search_vector`
//!
//! [`crate::index::semantic::SemanticIndex`] already has a kNN search
//! (`search_vector`), and this module does not call it. Two reasons, both
//! about what this task adds that task 21 did not need:
//!
//! - **Cancellation.** `SemanticIndex::search_vector` goes through
//!   [`crate::storage::Database::read`], which cannot be interrupted (see
//!   `retrieve::cancel`'s docs) — task 21 had no reason to make it
//!   interruptible, since indexing and "find similar messages" are not on a
//!   request path a newer query supersedes. This module's whole job this
//!   task is to be cancellable, so its kNN query goes through
//!   [`super::cancel::interruptible_read`] directly.
//! - **The hard-filter mask.** `search_vector` has no notion of `from:`/
//!   `is:`/... at all; this retriever needs the same gate every other source
//!   in this task applies.
//!
//! What is deliberately *not* reproduced from `SemanticIndex::knn` is its
//! unbounded widen-and-retry loop for a corpus mid-model-switch: that loop
//! trades latency for exhaustiveness without limit, which is the wrong trade
//! for a retriever against a ~25 ms Stage-1 budget (prd.md's "Candidate
//! generation (all retrievers, parallel) < 25 ms"). A single generously-
//! overfetched pass (see [`OVERFETCH`]) recovers the common unfiltered case.
//!
//! A hard-filter mask on top of the kNN reintroduces a version of the same
//! problem `SemanticIndex::knn`'s loop exists for, though: `k` chunk slots
//! are spent *before* [`filtermask`](super::filtermask) ever runs (the
//! `EXISTS` gate sits outside the `k =` CTE, same as the model/dim filters
//! `SemanticIndex::knn` widens for), so a selective filter (`from:alice`,
//! `in:Archive`) can consume the whole overfetched budget on chunks that
//! never pass it and return zero dense candidates while matching messages
//! exist — not a migration-window edge case, an ordinary `from:`/`is:`/`in:`
//! query. [`retrieve`](DenseRetriever::retrieve) allows exactly **one**
//! widen retry, only when a mask is present and the first pass both came up
//! short of `limit` *and* exhausted its `k` budget (`rows.len() == fetch`,
//! meaning more chunks might exist beyond what was fetched, as opposed to
//! the corpus itself simply not having `limit` matches) — bounded, unlike
//! `SemanticIndex::knn`'s loop, to keep the worst case one extra round trip
//! rather than open-ended.
//!
//! # Max ranks; mean rides along
//!
//! `Candidate::score` is a chunk-deduped message's **max** cosine similarity
//! — the value this retriever's own top-N cut and rank are computed from —
//! and `Candidate::mean_score` carries the **mean** alongside it, unused for
//! ranking here but preserved for task 30's `cos_mean_chunk` feature (prd.md,
//! Stage 3). Max, not mean, drives ranking: a long message with one chunk
//! that is a strong match and several that are not should still surface —
//! averaging them in would bury it under a short message that is uniformly,
//! mildly relevant throughout.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::types::Value;
use tokio_util::sync::CancellationToken;

use super::cancel::interruptible_read;
use super::filtermask::{self, FilterMask};
use super::{Candidate, Source};
use crate::error::Error;
use crate::index::semantic::{SemanticIndex, VECTOR_DIM};
use crate::query::QueryPlan;
use crate::storage::Database;

/// How far past the requested page this retriever overfetches chunk hits.
///
/// `k` in `vec_chunks MATCH ? AND k = ?` is chunk-granular, but the result
/// this retriever hands back is message-granular after dedup — a page of
/// `limit` messages needs more than `limit` chunk hits whenever a message
/// contributes more than one matching chunk, or a filtered-out chunk (wrong
/// model, moved text) consumes a `k` slot for nothing. `8` mirrors
/// `SemanticIndex`'s own single-pass overfetch factor for the equivalent
/// reason.
const OVERFETCH: i64 = 8;

/// Ceiling on the chunk fetch, however large `limit * OVERFETCH` would ask
/// for — `k` is work `vec_chunks` does before any filtering, so this bounds
/// the worst case rather than the typical one.
const MAX_FETCH: i64 = 4_000;

/// Largest number of message candidates this retriever returns, however
/// large a caller's `limit` requests — mirrors every other retriever's own
/// page ceiling ([`crate::index::fts::MAX_LIMIT`]).
const MAX_LIMIT: i64 = crate::index::fts::MAX_LIMIT;

/// Dense-vector kNN retrieval over chunk embeddings, deduped to messages.
#[derive(Debug, Clone)]
pub struct DenseRetriever {
    db: Database,
    model: String,
}

impl DenseRetriever {
    /// Build a retriever over `db`, matching against whatever model `semantic`
    /// is currently configured with (a stale vector from a previous model is
    /// excluded by the same `model`/`dim`/`content_hash` check
    /// [`SemanticIndex::knn`](crate::index::semantic) uses).
    #[must_use]
    pub fn new(db: Database, semantic: &SemanticIndex) -> Self {
        Self {
            db,
            model: semantic.model().to_owned(),
        }
    }

    /// Retrieve up to `limit` messages nearest `plan`'s query vector, each
    /// carrying max chunk similarity as `score` and mean as `mean_score`.
    ///
    /// Returns an empty list, not an error, when `plan` has no query vector
    /// (no free text to embed, or the embedder itself failed — prd.md's
    /// "Embeddings unavailable / no key → dense retriever silently drops")
    /// or a hard filter provably excludes every message.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    #[tracing::instrument(skip(self, plan, cancel), fields(hits))]
    pub async fn retrieve(
        &self,
        plan: &QueryPlan,
        limit: i64,
        cancel: &CancellationToken,
    ) -> Result<Vec<Candidate>, Error> {
        let Some(vector) = &plan.query_vector else {
            return Ok(Vec::new());
        };
        if vector.dim() != VECTOR_DIM {
            // Defensive: `QueryPlanner` embeds with the same configured
            // embedder this index expects, so this should not happen in
            // practice. Degrading rather than erroring keeps a
            // misconfiguration from failing the whole query when every other
            // source can still answer it.
            tracing::warn!(
                dim = vector.dim(),
                expected = VECTOR_DIM,
                "query vector width does not match the semantic index; skipping dense retrieval"
            );
            return Ok(Vec::new());
        }
        if vector.as_slice().iter().all(|v| *v == 0.0) {
            // Same guard as `SemanticIndex::search_vector`: nothing is near
            // a point with no direction, so searching would return the k
            // nearest to the origin — an arbitrary set — dressed up as an
            // answer.
            return Ok(Vec::new());
        }
        let mask = filtermask::compile(&plan.hard_filters);
        if matches!(mask, FilterMask::ExcludesEverything) {
            return Ok(Vec::new());
        }

        let page = clamp_limit(limit);
        let bytes = vector.to_bytes();
        let dim = i64::try_from(VECTOR_DIM).unwrap_or(i64::MAX);

        let mut sql = "WITH hits AS (
                 SELECT chunk_id, distance FROM vec_chunks WHERE embedding MATCH ?1 AND k = ?2
             )
             SELECT c.message_id, h.distance FROM hits h
             JOIN chunks c ON c.chunk_id = h.chunk_id
             JOIN chunk_embeddings e ON e.chunk_id = h.chunk_id
             WHERE e.model = ?3 AND e.dim = ?4 AND e.content_hash = c.content_hash"
            .to_owned();
        let mask_params = if let FilterMask::Sql(mask) = &mask {
            sql.push_str(&format!(" AND {}", mask.exists_clause("c.message_id")));
            mask.params.clone()
        } else {
            Vec::new()
        };

        let fetch = page.saturating_mul(OVERFETCH).min(MAX_FETCH);
        let Some(mut rows) = self
            .knn(cancel, &sql, &bytes, fetch, dim, &mask_params)
            .await?
        else {
            tracing::debug!("scan cancelled; superseded by a newer query");
            return Ok(Vec::new());
        };

        // See the module docs: a mask can starve `k` before it ever filters,
        // so one bounded widen retry covers the ordinary "selective filter
        // alongside dense recall" case the unfiltered path never hits.
        if !mask_params.is_empty() && fetch < MAX_FETCH {
            let distinct = rows
                .iter()
                .map(|(id, _)| *id)
                .collect::<BTreeSet<_>>()
                .len();
            let exhausted_k = rows.len() as i64 == fetch;
            if (distinct as i64) < page && exhausted_k {
                tracing::debug!(
                    fetch,
                    distinct,
                    page,
                    "dense kNN under a hard filter came up short with k exhausted; widening once"
                );
                if let Some(widened) = self
                    .knn(cancel, &sql, &bytes, MAX_FETCH, dim, &mask_params)
                    .await?
                {
                    rows = widened;
                }
            }
        }

        // `vec0`'s L2 distance over unit vectors relates to cosine by
        // `L2^2 = 2 - 2*cos` — the same conversion `SemanticIndex::knn` uses,
        // repeated here rather than exposed from that module because it is
        // one line and pulling in a whole extra `pub(crate)` surface for it
        // would cost more than it saves.
        let mut per_message: BTreeMap<i64, Vec<f64>> = BTreeMap::new();
        for (message_id, distance) in rows {
            let cosine = 1.0 - distance * distance / 2.0;
            per_message.entry(message_id).or_default().push(cosine);
        }

        let mut scored: Vec<(i64, f64, f64)> = per_message
            .into_iter()
            .map(|(message_id, sims)| {
                let max = sims.iter().copied().fold(f64::MIN, f64::max);
                let mean = sims.iter().sum::<f64>() / sims.len() as f64;
                (message_id, max, mean)
            })
            .collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        scored.truncate(usize::try_from(page).unwrap_or(usize::MAX));

        let candidates: Vec<Candidate> = scored
            .into_iter()
            .enumerate()
            .map(|(i, (message_id, max, mean))| Candidate {
                message_id,
                source: Source::Dense,
                score: max,
                rank: u32::try_from(i + 1).unwrap_or(u32::MAX),
                mean_score: Some(mean),
            })
            .collect();
        tracing::Span::current().record("hits", candidates.len());
        Ok(candidates)
    }

    /// One kNN pass at a given `fetch` (`k`), returning raw `(message_id,
    /// distance)` pairs — chunk-granular and undeduplicated, exactly what
    /// [`retrieve`](Self::retrieve) needs to decide whether a widen retry is
    /// warranted before doing the max/mean aggregation.
    ///
    /// `Ok(None)` means `cancel` fired; see [`interruptible_read`].
    async fn knn(
        &self,
        cancel: &CancellationToken,
        sql: &str,
        bytes: &[u8],
        fetch: i64,
        dim: i64,
        mask_params: &[Value],
    ) -> Result<Option<Vec<(i64, f64)>>, Error> {
        let sql = sql.to_owned();
        let bytes = bytes.to_owned();
        let model = self.model.clone();
        let mask_params = mask_params.to_owned();
        Ok(interruptible_read(&self.db, cancel, move |conn| {
            let mut stmt = conn.prepare(&sql)?;
            let mut bound: Vec<&dyn rusqlite::ToSql> = vec![&bytes, &fetch, &model, &dim];
            for value in &mask_params {
                bound.push(value);
            }
            let rows = stmt
                .query_map(bound.as_slice(), |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, f64>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<(i64, f64)>>>()?;
            Ok(rows)
        })
        .await?)
    }
}

fn clamp_limit(limit: i64) -> i64 {
    if limit <= 0 {
        MAX_LIMIT
    } else {
        limit.min(MAX_LIMIT)
    }
}

#[cfg(test)]
mod tests;
