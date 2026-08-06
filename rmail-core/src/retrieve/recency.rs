//! The recency-prior retriever: prd.md's "recent mail with weak textual
//! match (known-item bias)" row, scored by `exp(-age_days/half_life)`.
//!
//! # What this catches that the others do not
//!
//! A user who typed three words that only loosely describe a message they
//! remember receiving recently is exactly the case lexical/dense/fuzzy can
//! all rank poorly on their own terms — the words are too generic to stand
//! out in a BM25/cosine/subsequence sense, but "it was recent" is real
//! evidence the other retrievers never look at. This retriever exists to
//! surface that evidence on its own, as a distinct source, so fusion (task
//! 29) can credit a candidate that several sources agree on lightly *and*
//! that is unusually recent, rather than needing recency baked into every
//! other retriever's own scoring.
//!
//! # Recency decay, not a plain date sort
//!
//! `score` is `exp(-age_days / half_life_days)` — prd.md's exact formula for
//! `recency_decay` — rather than the raw date or a message's ordinal
//! position in a date-sorted list. A raw date is not comparable across
//! queries and is not a *relevance* score in the sense fusion needs (bounded,
//! higher-is-better, meaningful in isolation); a decay curve is both, and it
//! is the same shape [`crate::rank::l1::Weights`]'s `recency_decay` entry
//! later multiplies as an L1 ranking feature — this retriever computes the
//! feature's raw ingredient, not a value already scaled by that weight.

use chrono::{DateTime, Utc};
use tokio_util::sync::CancellationToken;

use super::cancel::interruptible_read;
use super::filtermask::{self, FilterMask};
use super::{rank_by_score, Candidate, Source};
use crate::error::Error;
use crate::index::fts::MAX_LIMIT;
use crate::query::QueryPlan;
use crate::storage::Database;

/// Recency-decay retrieval over `messages(date)`.
#[derive(Debug, Clone)]
pub struct RecencyRetriever {
    db: Database,
    half_life_days: f64,
}

impl RecencyRetriever {
    /// Build a retriever over `db`, decaying with the given half-life (days).
    /// A non-positive or non-finite half-life is clamped to
    /// [`DEFAULT_HALF_LIFE_DAYS`] — configuration is untrusted input, and a
    /// `0`/negative half-life would divide the decay exponent by zero or flip
    /// its sign, turning "recent" into the lowest-scoring messages instead of
    /// the highest.
    #[must_use]
    pub fn new(db: Database, half_life_days: f64) -> Self {
        let half_life_days = if half_life_days.is_finite() && half_life_days > 0.0 {
            half_life_days
        } else {
            // A config file's `recency_half_life_days` is user-authored and
            // reaches here unvalidated by the loader (`RetrieversConfig` has
            // no range constraint) — silently substituting the default would
            // leave a typo (`half_life_days = -30`) looking like it worked.
            tracing::warn!(
                configured = half_life_days,
                default = DEFAULT_HALF_LIFE_DAYS,
                "recency half-life must be a positive, finite number of days; using the default"
            );
            DEFAULT_HALF_LIFE_DAYS
        };
        Self { db, half_life_days }
    }

    /// Retrieve up to `limit` messages that pass `plan`'s hard filters,
    /// scored by recency decay from now.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    pub async fn retrieve(
        &self,
        plan: &QueryPlan,
        limit: i64,
        cancel: &CancellationToken,
    ) -> Result<Vec<Candidate>, Error> {
        self.retrieve_at(plan, limit, cancel, Utc::now()).await
    }

    /// As [`RecencyRetriever::retrieve`], with an injected "now" so decay
    /// scores are reproducible in a test.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    #[tracing::instrument(
        skip(self, plan, cancel, now),
        fields(filters = plan.hard_filters.len(), hits)
    )]
    pub async fn retrieve_at(
        &self,
        plan: &QueryPlan,
        limit: i64,
        cancel: &CancellationToken,
        now: DateTime<Utc>,
    ) -> Result<Vec<Candidate>, Error> {
        let (where_sql, params) = match filtermask::compile(&plan.hard_filters) {
            FilterMask::ExcludesEverything => {
                tracing::debug!("a hard filter provably excludes every message");
                return Ok(Vec::new());
            }
            FilterMask::Unconstrained => (String::new(), Vec::new()),
            FilterMask::Sql(mask) => (format!(" AND {}", mask.sql), mask.params),
        };

        let page = clamp_limit(limit);
        // A message with neither `date` nor `internaldate` cannot be scored
        // by recency at all — excluded here rather than assigned an
        // arbitrary age, the same "genuine uncertainty is not a claim" rule
        // `retrieve::lexical`'s date filters follow.
        let sql = format!(
            "SELECT id, COALESCE(date, internaldate) FROM messages \
             WHERE COALESCE(date, internaldate) IS NOT NULL{where_sql} \
             ORDER BY COALESCE(date, internaldate) DESC LIMIT ?"
        );
        let rows = interruptible_read(&self.db, cancel, move |conn| {
            let mut stmt = conn.prepare(&sql)?;
            let mut bound: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p as _).collect();
            bound.push(&page);
            let rows = stmt
                .query_map(bound.as_slice(), |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<(i64, i64)>>>()?;
            Ok(rows)
        })
        .await?;

        let Some(rows) = rows else {
            tracing::debug!("scan cancelled; superseded by a newer query");
            return Ok(Vec::new());
        };

        let now_ts = now.timestamp();
        let scored: Vec<(i64, f64)> = rows
            .into_iter()
            .map(|(id, ts)| {
                // Clamped at zero: a future-dated message (clock skew, a
                // scheduled send) must not score *above* `1.0` — decay is
                // meant to bound the score to `(0, 1]`, not extrapolate past
                // it for a date that has not happened yet.
                let age_days = ((now_ts - ts) as f64 / SECONDS_PER_DAY).max(0.0);
                (id, (-age_days / self.half_life_days).exp())
            })
            .collect();

        let candidates = rank_by_score(Source::Recency, scored);
        tracing::Span::current().record("hits", candidates.len());
        Ok(candidates)
    }
}

/// Seconds in a day, for turning a unix-seconds age into days.
const SECONDS_PER_DAY: f64 = 86_400.0;

/// Half-life used when configuration supplies none, or an invalid one —
/// [`crate::config::FinderConfig`]'s analogous `half_life_days` default for
/// the fuzzy finder's own recency decay (prd.md, Part III), reused here since
/// prd.md does not give Part I's recency prior a distinct default of its own.
const DEFAULT_HALF_LIFE_DAYS: f64 = 30.0;

fn clamp_limit(limit: i64) -> i64 {
    if limit <= 0 {
        MAX_LIMIT
    } else {
        limit.min(MAX_LIMIT)
    }
}

#[cfg(test)]
mod tests;
