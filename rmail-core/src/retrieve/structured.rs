//! The structured filter retriever: the hard-gate row of prd.md's Stage 1
//! table, run as its own source rather than only as a mask every other
//! retriever applies.
//!
//! # Why a filter-only query needs its own retriever at all
//!
//! "Hard filters gate everything" (prd.md, Stage 1 notes) describes how
//! [`super::filtermask`] is used by [`super::dense`], [`super::entity`],
//! [`super::fuzzy`], [`super::prefix`], and [`super::recency`] — but every one
//! of those retrievers still needs *something else* to rank on: a query
//! vector, an entity match, a fuzzy string, free text, recency. A query that
//! is *only* operators (`from:acme has:attachment`, no free text at all) gives
//! every one of them nothing to work with — [`super::lexical`]'s own docs
//! note exactly this ("a pure filter query... returns nothing, even though a
//! message exists that would pass the filter"), because a BM25/kNN/fuzzy
//! score answers "how relevant," and a pure filter query has not asked that
//! question. This module is the retriever whose only question is "does it
//! pass" — prd.md's own source-score column for this row is literally
//! `pass/fail` — so a filter-only query still returns something instead of
//! nothing.
//!
//! # Score is uniform; order comes from recency
//!
//! Every surviving row passed the same predicate, so nothing distinguishes
//! one from another on relevance grounds — `score` is a flat `1.0` for all of
//! them, matching prd.md's "pass/fail" characterization literally. Ordering
//! (and therefore `rank`, and which rows survive `LIMIT`) still has to come
//! from somewhere: recency is the same tie-break prd.md's own "known-item
//! bias" principle uses elsewhere (`recency prior`, `sender_affinity`'s
//! "recency of last interaction"), and it means `from:acme` alone reads as
//! "acme's most recent mail," which is what a user typing only an operator
//! and nothing else almost always wants.
//!
//! # An unconstrained query contributes nothing
//!
//! With no hard filters at all, this retriever's predicate is "every
//! message" — identical to what [`super::recency`] already returns for an
//! empty query (prd.md's own edge case: "Empty query → recency-ranked recent
//! mail"). Returning the same list under a second [`super::Source`] would not
//! add recall, only inflate `num_sources_hit` for every result identically,
//! which is a fusion feature (task 30) this retriever has no business
//! distorting. So [`StructuredRetriever::retrieve`] returns empty rather than
//! "everything" when there is nothing to gate on.

use tokio_util::sync::CancellationToken;

use super::cancel::interruptible_read;
use super::filtermask::{self, FilterMask};
use super::{rank_by_score, Candidate, Source};
use crate::error::Error;
use crate::index::fts::MAX_LIMIT;
use crate::query::QueryPlan;
use crate::storage::Database;

/// Structured hard-filter retrieval: `SELECT ... WHERE <mask>`, gated the
/// same way every non-lexical retriever in this task is.
#[derive(Debug, Clone)]
pub struct StructuredRetriever {
    db: Database,
}

impl StructuredRetriever {
    /// Build a retriever over `db`.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Retrieve up to `limit` messages that pass `plan`'s hard filters,
    /// most-recent first, each carrying the uniform pass/fail score `1.0`.
    ///
    /// Returns an empty list, not an error, when there are no hard filters to
    /// gate on (see the module docs), when a filter provably excludes every
    /// message, or when `cancel` fires before the scan completes.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    #[tracing::instrument(skip(self, plan, cancel), fields(filters = plan.hard_filters.len(), hits))]
    pub async fn retrieve(
        &self,
        plan: &QueryPlan,
        limit: i64,
        cancel: &CancellationToken,
    ) -> Result<Vec<Candidate>, Error> {
        let mask = match filtermask::compile(&plan.hard_filters) {
            FilterMask::Unconstrained => {
                tracing::debug!("no hard filters to gate on; this source contributes nothing");
                return Ok(Vec::new());
            }
            FilterMask::ExcludesEverything => {
                tracing::debug!("a hard filter provably excludes every message");
                return Ok(Vec::new());
            }
            FilterMask::Sql(mask) => mask,
        };

        let page = clamp_limit(limit);
        let sql = format!(
            "SELECT id FROM messages WHERE {} ORDER BY COALESCE(date, internaldate) DESC LIMIT ?",
            mask.sql
        );
        let params = mask.params;
        let ids = interruptible_read(&self.db, cancel, move |conn| {
            let mut stmt = conn.prepare(&sql)?;
            let mut bound: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p as _).collect();
            bound.push(&page);
            let rows = stmt
                .query_map(bound.as_slice(), |row| row.get::<_, i64>(0))?
                .collect::<rusqlite::Result<Vec<i64>>>()?;
            Ok(rows)
        })
        .await?;

        let Some(ids) = ids else {
            tracing::debug!("scan cancelled; superseded by a newer query");
            return Ok(Vec::new());
        };

        // Pass/fail: every surviving row is equally "matched" — see the
        // module docs on why `ORDER BY` (recency), not score, decides who
        // makes the cut.
        let scored: Vec<(i64, f64)> = ids.into_iter().map(|id| (id, 1.0)).collect();
        let candidates = rank_by_score(Source::Structured, scored);
        tracing::Span::current().record("hits", candidates.len());
        Ok(candidates)
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
