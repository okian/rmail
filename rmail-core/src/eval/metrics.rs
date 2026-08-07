//! The four ranking metrics prd.md's "Evaluation Harness & Metrics" names —
//! NDCG@10, MRR, Recall@50, P@3 — as pure functions over a ranked id list
//! and a judgment table.
//!
//! Nothing here touches a database, a config, or the pipeline. That is
//! deliberate: these are the numbers a ranking change is judged by, so they
//! have to be testable against hand-computable examples rather than against
//! whatever the pipeline happens to return today. [`super::golden`] supplies
//! the judgments, [`crate::eval::replay`] reuses the same functions for
//! shadow scoring, and task 65's hot-swap guardrail gates on them.
//!
//! # Every function here is total
//!
//! A metric is a number a CI gate compares against a threshold, so a metric
//! that can be `NaN` is a metric that can silently pass a `>=` check
//! (`NaN >= x` is false, `!(NaN < x)` is true — which of those a guard hits
//! depends on how someone wrote the comparison). Every degenerate input
//! therefore returns `0.0` rather than a division result:
//!
//! - `k == 0`, or an empty `ranked` list → `0.0`.
//! - No judged-relevant documents at all → `0.0` (an undefined IDCG, not a
//!   perfect score). [`super::golden`] rejects such a query at load time so
//!   this case does not arise from a golden set, but [`replay`] can hit it
//!   with an impression nobody clicked.
//!
//! [`replay`]: crate::eval::replay
//!
//! # Duplicate ids
//!
//! `ranked` is deduplicated (first occurrence wins) before scoring. The
//! pipeline does not emit a message twice — `fuse::Fuser` collapses on
//! message id — but a metric that rewards repetition is a metric that can be
//! gamed by a bug rather than failing on it: without the dedup, a fusion
//! regression that emitted the one relevant message ten times would *raise*
//! NDCG instead of exposing itself.

use std::collections::{HashMap, HashSet};

/// Rank cutoff for the headline relevance number (prd.md: NDCG@10).
pub const NDCG_K: usize = 10;

/// Rank cutoff for recall (prd.md: Recall@50). Also the minimum number of
/// results [`super::Evaluator`] asks the pipeline for, since a shorter page
/// makes the metric unmeasurable rather than merely low.
pub const RECALL_K: usize = 50;

/// Rank cutoff for precision (prd.md: P@3) — and the cutoff prd.md's success
/// criterion ("the intended message is in the top 3") is stated against.
pub const PRECISION_K: usize = 3;

/// The largest relevance grade a judgment may carry.
///
/// NDCG's exponential gain (`2^g - 1`) means a grade is an exponent: an
/// unbounded one lets a single typo'd judgment (`gain = 400`) produce an
/// infinite ideal DCG and drive every NDCG for that query to zero. Four
/// grades is also as many as a human judge can apply consistently — the
/// standard TREC scale is 0-3.
pub const MAX_GAIN: u32 = 3;

/// Graded relevance for one query: message id -> gain, where `0` (or absent)
/// means not relevant.
pub type Judgments = HashMap<i64, u32>;

/// The four headline numbers for a single query, or macro-averaged over a
/// set of them.
///
/// Comparable across runs *only* for the same golden set — these are
/// corpus-relative, so a number from one golden set says nothing about a
/// different one.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Metrics {
    /// NDCG@10 — the headline. Graded, exponential-gain, ideal-normalized.
    pub ndcg_at_10: f64,
    /// Reciprocal rank of the first relevant result, `0.0` if none appears.
    pub mrr: f64,
    /// Fraction of all judged-relevant messages that appear in the top 50.
    pub recall_at_50: f64,
    /// Fraction of the top 3 that is relevant.
    pub p_at_3: f64,
}

impl Metrics {
    /// Score one ranked id list against `judgments` at the prd.md cutoffs.
    #[must_use]
    pub fn score(ranked: &[i64], judgments: &Judgments) -> Self {
        let ranked = dedup(ranked);
        Self {
            ndcg_at_10: ndcg_at(&ranked, judgments, NDCG_K),
            mrr: reciprocal_rank(&ranked, judgments),
            recall_at_50: recall_at(&ranked, judgments, RECALL_K),
            p_at_3: precision_at(&ranked, judgments, PRECISION_K),
        }
    }

    /// Macro-average over per-query metrics — every query weighted equally,
    /// regardless of how many relevant documents it has.
    ///
    /// Macro rather than micro because the golden set is a sample of *query
    /// shapes*, not of traffic: micro-averaging would let one query with
    /// forty judged-relevant messages outvote thirty navigational queries
    /// with one each, and the navigational case is the one prd.md's "top 3"
    /// criterion is about.
    ///
    /// Returns all-zero for an empty input.
    #[must_use]
    pub fn mean(per_query: &[Metrics]) -> Self {
        let n = per_query.len();
        if n == 0 {
            return Self::default();
        }
        #[allow(clippy::cast_precision_loss)]
        let n = n as f64;
        let sum = per_query.iter().fold(Self::default(), |acc, m| Self {
            ndcg_at_10: acc.ndcg_at_10 + m.ndcg_at_10,
            mrr: acc.mrr + m.mrr,
            recall_at_50: acc.recall_at_50 + m.recall_at_50,
            p_at_3: acc.p_at_3 + m.p_at_3,
        });
        Self {
            ndcg_at_10: sum.ndcg_at_10 / n,
            mrr: sum.mrr / n,
            recall_at_50: sum.recall_at_50 / n,
            p_at_3: sum.p_at_3 / n,
        }
    }
}

/// Drop repeat ids, keeping first occurrence. See the module docs.
fn dedup(ranked: &[i64]) -> Vec<i64> {
    let mut seen = HashSet::with_capacity(ranked.len());
    ranked
        .iter()
        .copied()
        .filter(|id| seen.insert(*id))
        .collect()
}

/// Exponential gain, `2^g - 1`: the standard graded-relevance formulation,
/// which makes one highly-relevant result worth more than several marginal
/// ones rather than merely as much.
fn gain(g: u32) -> f64 {
    f64::from(2u32.saturating_pow(g.min(MAX_GAIN))) - 1.0
}

/// Positional discount, `1 / log2(rank + 1)` for 1-based `rank`.
fn discount(zero_based_position: usize) -> f64 {
    #[allow(clippy::cast_precision_loss)]
    let rank = (zero_based_position + 1) as f64;
    1.0 / (rank + 1.0).log2()
}

/// Discounted cumulative gain over the first `k` of `ranked`.
fn dcg_at(ranked: &[i64], judgments: &Judgments, k: usize) -> f64 {
    ranked
        .iter()
        .take(k)
        .enumerate()
        .map(|(i, id)| gain(judgments.get(id).copied().unwrap_or(0)) * discount(i))
        .sum()
}

/// The best DCG@k any ordering of `judgments` could achieve — every relevant
/// document sorted by gain descending, truncated to `k`.
fn ideal_dcg_at(judgments: &Judgments, k: usize) -> f64 {
    let mut gains: Vec<u32> = judgments.values().copied().filter(|g| *g > 0).collect();
    gains.sort_unstable_by(|a, b| b.cmp(a));
    gains
        .into_iter()
        .take(k)
        .enumerate()
        .map(|(i, g)| gain(g) * discount(i))
        .sum()
}

/// Normalized discounted cumulative gain at `k` — `0.0` for a degenerate
/// input, never `NaN` (see the module docs).
#[must_use]
pub fn ndcg_at(ranked: &[i64], judgments: &Judgments, k: usize) -> f64 {
    if k == 0 {
        return 0.0;
    }
    let ideal = ideal_dcg_at(judgments, k);
    if ideal <= 0.0 {
        return 0.0;
    }
    // Bounded above by 1.0: DCG can exceed the *truncated* IDCG when more
    // than `k` documents are judged relevant and the run happens to place
    // higher-gain ones early — clamping keeps "1.0 means perfect" true.
    (dcg_at(ranked, judgments, k) / ideal).min(1.0)
}

/// Reciprocal rank of the first relevant result — `1/rank`, 1-based, or
/// `0.0` if no relevant result appears anywhere in `ranked`.
///
/// Uncapped by design: MRR over the full returned list is what makes the
/// "did we find it at all, and how far down" question answerable. A cutoff
/// would conflate "ranked 60th" with "absent", which is exactly the
/// distinction a recall regression needs.
#[must_use]
pub fn reciprocal_rank(ranked: &[i64], judgments: &Judgments) -> f64 {
    ranked
        .iter()
        .position(|id| judgments.get(id).copied().unwrap_or(0) > 0)
        .map_or(0.0, |i| {
            #[allow(clippy::cast_precision_loss)]
            let rank = (i + 1) as f64;
            1.0 / rank
        })
}

/// Fraction of all judged-relevant documents appearing in the top `k`.
#[must_use]
pub fn recall_at(ranked: &[i64], judgments: &Judgments, k: usize) -> f64 {
    let total = judgments.values().filter(|g| **g > 0).count();
    if total == 0 || k == 0 {
        return 0.0;
    }
    let found = ranked
        .iter()
        .take(k)
        .filter(|id| judgments.get(id).copied().unwrap_or(0) > 0)
        .count();
    #[allow(clippy::cast_precision_loss)]
    let ratio = found as f64 / total as f64;
    ratio
}

/// Fraction of the top `k` that is relevant.
///
/// The denominator is `k` even when `ranked` is shorter — returning fewer
/// than `k` results is a recall failure, and dividing by the returned count
/// would hide it by scoring a single correct result out of one returned as a
/// perfect `1.0`.
#[must_use]
pub fn precision_at(ranked: &[i64], judgments: &Judgments, k: usize) -> f64 {
    if k == 0 {
        return 0.0;
    }
    let hits = ranked
        .iter()
        .take(k)
        .filter(|id| judgments.get(id).copied().unwrap_or(0) > 0)
        .count();
    #[allow(clippy::cast_precision_loss)]
    let ratio = hits as f64 / k as f64;
    ratio
}
