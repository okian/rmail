//! Relevance evaluation: the golden set, the four metrics, the offline
//! replay harness, and the regression guard (task 37; prd.md, "Evaluation
//! Harness & Metrics").
//!
//! > *Relevance is measured, not asserted.*
//!
//! Search is the product (prd.md, "Relevance-First"), which makes "did that
//! change make search better or worse" the single most important question
//! this codebase has to be able to answer. This module is the apparatus that
//! answers it, and every design choice in it follows from one constraint:
//! the answer has to be trustworthy enough to *block a merge*.
//!
//! # The pieces
//!
//! - [`golden`] — the versioned `(query, judged-relevant Message-ID)` file.
//! - [`metrics`] — NDCG@10, MRR, Recall@50, P@3 as pure, total functions.
//! - [`replay`] — online metrics and shadow ranking over logged impressions.
//! - [`Evaluator`] — runs a golden set through a [`RankedSearch`] and
//!   assembles an [`EvalReport`].
//! - [`EvalThresholds`] — turns a report into a pass/fail verdict.
//!
//! # Why the pipeline arrives as a trait
//!
//! [`RankedSearch`] is an abstraction over "run this query, give me ranked
//! message ids" rather than a direct dependency on the pipeline, because the
//! pipeline lives in `rmaild` (it is assembled from a `Database`, an
//! `Embedder`, a validated `Weights` table and the `[search]` config) and
//! `rmail-core` is downstream of none of that. `rmaild::search_service`
//! implements the trait over the *same* `SearchApi` that serves
//! `SearchService.Search`, so an evaluated query and a typed one traverse
//! one code path — an eval harness that scored a reimplementation of the
//! ranker would be measuring the wrong program, and would keep passing
//! while the shipped one regressed.
//!
//! # Why `limit` is forced to at least 50
//!
//! [`Evaluator`] requests [`metrics::RECALL_K`] results regardless of the
//! daemon's configured `search.default_limit` (25). Recall@50 computed over
//! a 25-result page is not a low number, it is an *unmeasurable* one capped
//! at whatever fraction 25 results can contain — and it would move whenever
//! someone retuned an unrelated config default, which is exactly the kind of
//! phantom regression that teaches people to ignore a CI gate.

pub mod golden;
pub mod metrics;
pub mod replay;

#[cfg(test)]
mod tests;

use crate::error::Error;
use crate::storage::Database;

pub use golden::{GoldenQuery, GoldenSet, JudgedMessage, Resolved};
pub use metrics::{Judgments, Metrics, NDCG_K, PRECISION_K, RECALL_K};
pub use replay::{
    bucket, replay, shadow, Engagement, EngagementAction, Impression, OnlineMetrics, ShadowOutcome,
};

/// Whatever can answer "run this query and give me ranked message ids".
///
/// Implemented by `rmaild::search_service::SearchApi` over the real
/// pipeline; implemented by fakes in tests. See the module docs for why this
/// is a trait.
#[async_trait::async_trait]
pub trait RankedSearch: Send + Sync {
    /// Ranked message ids, best first, at most `limit` of them.
    ///
    /// `account_id` of `0` means every account, matching
    /// `SearchRequest.account_id`.
    ///
    /// # Errors
    /// Whatever the underlying pipeline fails with.
    async fn ranked_ids(
        &self,
        query: &str,
        account_id: i64,
        limit: usize,
    ) -> Result<Vec<i64>, Error>;
}

/// Per-query evaluation output.
#[derive(Debug, Clone, PartialEq)]
pub struct QueryEval {
    /// [`GoldenQuery::name`].
    pub name: String,
    /// The query string that was run.
    pub query: String,
    /// This query's four metrics.
    pub metrics: Metrics,
    /// How many results the pipeline returned (before metric cutoffs).
    pub returned: usize,
    /// How many judged-relevant messages this query has in this corpus,
    /// after resolution.
    pub relevant: usize,
    /// Golden `Message-ID`s absent from the corpus. Non-empty means the
    /// numbers above understate the ranker — see [`golden`]'s module docs.
    pub unresolved: Vec<String>,
}

/// A full evaluation run.
#[derive(Debug, Clone, PartialEq)]
pub struct EvalReport {
    /// [`GoldenSet::corpus`], carried through so a report is self-describing.
    pub corpus: String,
    /// One entry per golden query, in file order.
    pub per_query: Vec<QueryEval>,
    /// Macro-average across queries — the headline numbers.
    pub aggregate: Metrics,
}

impl EvalReport {
    /// Every golden `Message-ID` that no message in the corpus matched,
    /// across all queries.
    #[must_use]
    pub fn unresolved(&self) -> Vec<&str> {
        self.per_query
            .iter()
            .flat_map(|q| q.unresolved.iter().map(String::as_str))
            .collect()
    }

    /// The worst-scoring queries by NDCG@10, best-effort ascending — what a
    /// failing CI run should print instead of only the aggregate, since an
    /// aggregate says a regression happened and this says where.
    #[must_use]
    pub fn worst(&self, n: usize) -> Vec<&QueryEval> {
        let mut sorted: Vec<&QueryEval> = self.per_query.iter().collect();
        sorted.sort_by(|a, b| {
            a.metrics
                .ndcg_at_10
                .partial_cmp(&b.metrics.ndcg_at_10)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        sorted.into_iter().take(n).collect()
    }
}

/// The pass/fail contract a CI run gates on.
///
/// Only `min_ndcg_at_10` is required; the rest are opt-in floors so a
/// project can tighten the gate over time without every threshold becoming a
/// number somebody had to invent on day one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EvalThresholds {
    /// Aggregate NDCG@10 must be at least this. prd.md's regression guard:
    /// "a drop in NDCG fails the build".
    pub min_ndcg_at_10: f64,
    /// Optional aggregate MRR floor.
    pub min_mrr: Option<f64>,
    /// Optional aggregate Recall@50 floor.
    pub min_recall_at_50: Option<f64>,
    /// Optional aggregate P@3 floor.
    pub min_p_at_3: Option<f64>,
    /// Fail when any golden judgment did not resolve against the corpus.
    ///
    /// Defaults to `true`, and should stay that way in CI: an unresolved
    /// judgment means the fixture did not seed, and a corpus failure that
    /// presents as a relevance failure is worse than either failure alone.
    pub require_resolved: bool,
}

impl Default for EvalThresholds {
    fn default() -> Self {
        Self {
            // Deliberately not 0.0: a default that passes everything is a
            // gate that exists only on paper. Callers that genuinely want no
            // floor set it explicitly and are visibly doing so.
            min_ndcg_at_10: 0.5,
            min_mrr: None,
            min_recall_at_50: None,
            min_p_at_3: None,
            require_resolved: true,
        }
    }
}

impl EvalThresholds {
    /// Check `report` against these thresholds.
    ///
    /// # Errors
    /// [`Error::FailedPrecondition`] listing *every* violated threshold, not
    /// just the first — a run that regressed three metrics should say so in
    /// one CI log rather than over three fix-and-rerun cycles.
    pub fn check(&self, report: &EvalReport) -> Result<(), Error> {
        let mut failures = Vec::new();
        let agg = &report.aggregate;

        if agg.ndcg_at_10 < self.min_ndcg_at_10 {
            failures.push(format!(
                "NDCG@10 {:.4} < {:.4}",
                agg.ndcg_at_10, self.min_ndcg_at_10
            ));
        }
        if let Some(min) = self.min_mrr {
            if agg.mrr < min {
                failures.push(format!("MRR {:.4} < {min:.4}", agg.mrr));
            }
        }
        if let Some(min) = self.min_recall_at_50 {
            if agg.recall_at_50 < min {
                failures.push(format!("Recall@50 {:.4} < {min:.4}", agg.recall_at_50));
            }
        }
        if let Some(min) = self.min_p_at_3 {
            if agg.p_at_3 < min {
                failures.push(format!("P@3 {:.4} < {min:.4}", agg.p_at_3));
            }
        }
        if self.require_resolved {
            let unresolved = report.unresolved();
            if !unresolved.is_empty() {
                failures.push(format!(
                    "{} golden judgment(s) did not resolve against corpus {:?}: {}",
                    unresolved.len(),
                    report.corpus,
                    unresolved.join(", ")
                ));
            }
        }

        if failures.is_empty() {
            Ok(())
        } else {
            Err(Error::FailedPrecondition(format!(
                "relevance gate failed: {}",
                failures.join("; ")
            )))
        }
    }
}

/// Runs a golden set through a search implementation.
#[derive(Debug, Clone)]
pub struct Evaluator {
    db: Database,
    limit: usize,
}

impl Evaluator {
    /// Build an evaluator over the corpus in `db`.
    ///
    /// The result limit is [`metrics::RECALL_K`] — see the module docs for
    /// why it is not the configured search default.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self {
            db,
            limit: RECALL_K,
        }
    }

    /// Override how many results each query fetches.
    ///
    /// Clamped up to [`metrics::RECALL_K`]: a smaller page cannot express
    /// Recall@50 at all, and silently reporting a capped number is worse
    /// than ignoring the caller's request and saying which limit was used.
    #[must_use]
    pub fn with_limit(mut self, limit: usize) -> Self {
        self.limit = limit.max(RECALL_K);
        self
    }

    /// Run every query in `set` through `search` and assemble the report.
    ///
    /// # Errors
    /// A mapped storage error from judgment resolution, or whatever
    /// `search` fails with. A query that errors fails the whole run rather
    /// than scoring zero: a pipeline error is a broken build, and averaging
    /// a zero into the aggregate would report it as a relevance regression.
    #[tracing::instrument(skip(self, set, search), fields(corpus = %set.corpus, queries = set.queries.len()), err)]
    pub async fn run<S: RankedSearch + ?Sized>(
        &self,
        set: &GoldenSet,
        search: &S,
    ) -> Result<EvalReport, Error> {
        set.validate()?;

        let mut per_query = Vec::with_capacity(set.queries.len());
        for q in &set.queries {
            let resolved = q.resolve(&self.db).await?;
            let ranked = search
                .ranked_ids(&q.query, q.account_id, self.limit)
                .await?;
            let metrics = Metrics::score(&ranked, &resolved.judgments);

            // Positive gains only: a judgment may be present with gain 0
            // (explicitly "not relevant"), and counting those as relevant
            // would overstate what the ranker was asked to find.
            let relevant = resolved.judgments.values().filter(|g| **g > 0).count();

            tracing::debug!(
                query = %q.name,
                ndcg_at_10 = metrics.ndcg_at_10,
                mrr = metrics.mrr,
                returned = ranked.len(),
                relevant,
                unresolved = resolved.unresolved.len(),
                "evaluated golden query"
            );

            per_query.push(QueryEval {
                name: q.name.clone(),
                query: q.query.clone(),
                metrics,
                returned: ranked.len(),
                relevant,
                unresolved: resolved.unresolved,
            });
        }

        let aggregate = Metrics::mean(
            &per_query
                .iter()
                .map(|q| q.metrics)
                .collect::<Vec<Metrics>>(),
        );

        Ok(EvalReport {
            corpus: set.corpus.clone(),
            per_query,
            aggregate,
        })
    }
}
