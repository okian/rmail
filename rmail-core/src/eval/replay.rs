//! Offline replay and shadow ranking over logged impressions — prd.md's
//! "Online metrics" and "Offline replay / shadow ranking" bullets.
//!
//! Where the golden set asks "does the ranker agree with a human judge",
//! replay asks "would this ranker have done better on what users actually
//! did". Both matter and neither substitutes for the other: the golden set
//! covers queries nobody has run yet, replay covers the long tail nobody has
//! judged.
//!
//! # What this module owns, and what task 64 owns
//!
//! Everything here is a pure function over an [`Impression`] slice. The
//! `search_log`/`search_impression`/`search_action` tables that *produce*
//! those impressions belong to task 64 (feedback logging), which does not
//! exist yet — so today's only source is a caller-supplied slice (the CLI
//! reads a JSONL file; the tests build them inline). That split is
//! deliberate rather than a stub: task 65's hot-swap guardrail depends on
//! *this* scoring math and on task 64's tables independently, and writing
//! the math against an owned in-memory type means the guardrail can be
//! tested to the last edge case without a feedback log existing at all.
//! When 64 lands, it adds a query that yields `Vec<Impression>`; nothing
//! here changes.
//!
//! # Why shadow scoring is restricted to what was shown
//!
//! [`shadow`] drops any candidate the logged impression did not display.
//! This is not a simplification — it is the only sound thing to do. An
//! engagement label exists only for a document the user *saw*; a document
//! that was never shown has no label, and scoring it as "not engaged" would
//! systematically punish any candidate ranker that surfaces something new.
//! That is precisely backwards, since surfacing something better than the
//! incumbent found is the entire point of replacing a ranker. The honest
//! reading of a shadow score is therefore "how well would this ranker have
//! ordered the results we already know about", and
//! [`ShadowOutcome::unlabeled`] reports how much of the candidate's output
//! that reading had to ignore — a large value means the shadow number is
//! weak evidence and the change needs a live A/B instead.
//!
//! # Position bias is *not* corrected here
//!
//! A result at rank 1 gets engaged with more often than an equally relevant
//! result at rank 8, so raw engagement is a biased relevance signal and
//! these metrics inherit that bias. Correcting it (propensity weighting from
//! an examination model) is task 65's stated job, on the training side where
//! the labels are actually consumed. Reporting the uncorrected numbers here
//! and saying so is better than applying a half-correction that would make
//! the bias invisible without removing it.

use std::collections::HashSet;

use crate::eval::metrics::{Judgments, Metrics};

/// What a user did with a result they were shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngagementAction {
    /// Opened the message.
    Open,
    /// Replied to it — the strongest positive signal available.
    Reply,
    /// Archived it straight from the result list.
    Archive,
    /// Scrolled past without opening.
    ScrollPast,
    /// Dwelled on the preview long enough to count as reading it.
    Dwell,
}

impl EngagementAction {
    /// Whether this action counts as the result having been *wanted*.
    ///
    /// `Archive` is not positive even though it is deliberate: archiving
    /// from a result list is how a user disposes of something they did not
    /// want to find. `ScrollPast` is an explicit negative — the strongest
    /// "shown and rejected" signal there is, and the one that makes
    /// abandonment measurable rather than merely inferred from silence.
    #[must_use]
    pub const fn is_positive(self) -> bool {
        matches!(self, Self::Open | Self::Reply | Self::Dwell)
    }
}

/// One thing a user did to one result.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Engagement {
    /// The message acted on. Must appear in [`Impression::shown`] to count —
    /// see [`Impression::judgments`].
    pub message_id: i64,
    /// What was done.
    pub action: EngagementAction,
}

/// One logged search: the query, the ranked page the user was shown, and
/// what they did with it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Impression {
    /// The query string as typed.
    pub query: String,
    /// Message ids in the exact order they were presented.
    pub shown: Vec<i64>,
    /// Actions taken on results from this page.
    #[serde(default)]
    pub engagements: Vec<Engagement>,
}

impl Impression {
    /// Implicit relevance judgments: every positively-engaged shown result,
    /// gain `1`.
    ///
    /// Engagements naming a message that was not shown are ignored rather
    /// than trusted. A well-formed log cannot contain one, but a malformed
    /// entry that did would otherwise inject a judgment for a document no
    /// ranker could have been credited with returning, quietly deflating
    /// every recall number computed from this log.
    #[must_use]
    pub fn judgments(&self) -> Judgments {
        let shown: HashSet<i64> = self.shown.iter().copied().collect();
        self.engagements
            .iter()
            .filter(|e| e.action.is_positive() && shown.contains(&e.message_id))
            .map(|e| (e.message_id, 1))
            .collect()
    }

    /// Whether the user positively engaged with anything on this page.
    #[must_use]
    pub fn is_successful(&self) -> bool {
        !self.judgments().is_empty()
    }
}

/// The behavioral metrics prd.md lists under "Online metrics".
///
/// Every field is a fraction in `0.0..=1.0`, macro-averaged over
/// impressions; an empty input yields all zeros.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct OnlineMetrics {
    /// Fraction of impressions with at least one positive engagement.
    pub ctr: f64,
    /// Mean reciprocal rank of the first positively-engaged result, counting
    /// `0` for impressions with none.
    pub engaged_mrr: f64,
    /// Fraction of impressions whose top result was engaged with.
    pub success_at_1: f64,
    /// Fraction of impressions with a positive engagement in the top 3 —
    /// the behavioral counterpart to prd.md's "top 3" success criterion.
    pub success_at_3: f64,
    /// Fraction of impressions with no positive engagement at all. Always
    /// `1.0 - ctr`; carried explicitly because it is the number an
    /// abandonment regression is read off.
    pub abandonment: f64,
    /// How many impressions produced these numbers. A metric computed from
    /// nine impressions and one computed from nine thousand are not
    /// comparable, and a report that omits this invites treating them as if
    /// they were.
    pub impressions: usize,
}

/// Score impressions exactly as they were shown — the "what actually
/// happened" baseline a shadow run is compared against.
#[must_use]
pub fn replay(impressions: &[Impression]) -> OnlineMetrics {
    score_orders(impressions, |imp| imp.shown.clone())
}

/// The result of scoring a candidate ranker against a logged page.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ShadowOutcome {
    /// Behavioral metrics under the candidate's ordering.
    pub online: OnlineMetrics,
    /// Ranking metrics under the candidate's ordering, treating positive
    /// engagements as graded judgments. This is the NDCG task 65's hot-swap
    /// guardrail gates on.
    pub ranking: Metrics,
    /// Candidate-emitted ids that the logged impression never displayed, and
    /// which therefore carry no label. See the module docs.
    pub unlabeled: usize,
}

/// Score a candidate ranker against logged impressions without ever showing
/// it to a user.
///
/// `reorder` receives each impression and returns its preferred ordering of
/// that query's results. Ids it emits that were not in
/// [`Impression::shown`] are dropped (and counted into
/// [`ShadowOutcome::unlabeled`]); shown ids it omits are simply absent from
/// its ordering, which correctly costs it recall.
pub fn shadow<F>(impressions: &[Impression], mut reorder: F) -> ShadowOutcome
where
    F: FnMut(&Impression) -> Vec<i64>,
{
    let mut unlabeled = 0usize;
    let mut orders: Vec<Vec<i64>> = Vec::with_capacity(impressions.len());
    for imp in impressions {
        let shown: HashSet<i64> = imp.shown.iter().copied().collect();
        let proposed = reorder(imp);
        let kept: Vec<i64> = proposed
            .into_iter()
            .filter(|id| {
                let known = shown.contains(id);
                if !known {
                    unlabeled += 1;
                }
                known
            })
            .collect();
        orders.push(kept);
    }

    let online = score_precomputed(impressions, &orders);
    let per_query: Vec<Metrics> = impressions
        .iter()
        .zip(&orders)
        .map(|(imp, order)| Metrics::score(order, &imp.judgments()))
        .collect();

    ShadowOutcome {
        online,
        ranking: Metrics::mean(&per_query),
        unlabeled,
    }
}

/// Deterministically bucket a query for A/B assignment.
///
/// The same query text always lands in the same bucket, on every machine and
/// across restarts, so a user does not see one arm for a query and the other
/// arm for the same query typed again a minute later — an inconsistency that
/// would both confuse the user and contaminate the experiment. FNV-1a over
/// the raw bytes: not cryptographic, and does not need to be — nothing here
/// resists an adversary, it only needs to spread evenly and reproducibly.
///
/// Returns `0` when `buckets` is `0` or `1`, i.e. "everyone in the control
/// arm", which is the correct degenerate reading of "no experiment".
#[must_use]
pub fn bucket(query: &str, buckets: u32) -> u32 {
    if buckets <= 1 {
        return 0;
    }
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in query.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    #[allow(clippy::cast_possible_truncation)]
    let bucket = (hash % u64::from(buckets)) as u32;
    bucket
}

/// Compute [`OnlineMetrics`] over orderings derived from each impression.
fn score_orders<F>(impressions: &[Impression], mut order_of: F) -> OnlineMetrics
where
    F: FnMut(&Impression) -> Vec<i64>,
{
    let orders: Vec<Vec<i64>> = impressions.iter().map(&mut order_of).collect();
    score_precomputed(impressions, &orders)
}

/// Shared body of [`replay`] and [`shadow`]: the behavioral metrics for a
/// set of impressions under an already-decided ordering per impression.
fn score_precomputed(impressions: &[Impression], orders: &[Vec<i64>]) -> OnlineMetrics {
    let n = impressions.len();
    if n == 0 {
        return OnlineMetrics::default();
    }

    let mut engaged = 0usize;
    let mut rr_total = 0.0f64;
    let mut at_1 = 0usize;
    let mut at_3 = 0usize;

    for (imp, order) in impressions.iter().zip(orders) {
        let judgments = imp.judgments();
        if judgments.is_empty() {
            continue;
        }
        engaged += 1;
        if let Some(pos) = order.iter().position(|id| judgments.contains_key(id)) {
            #[allow(clippy::cast_precision_loss)]
            let rank = (pos + 1) as f64;
            rr_total += 1.0 / rank;
            if pos == 0 {
                at_1 += 1;
            }
            if pos < 3 {
                at_3 += 1;
            }
        }
    }

    #[allow(clippy::cast_precision_loss)]
    let total = n as f64;
    #[allow(clippy::cast_precision_loss)]
    let ctr = engaged as f64 / total;
    #[allow(clippy::cast_precision_loss)]
    let success_at_1 = at_1 as f64 / total;
    #[allow(clippy::cast_precision_loss)]
    let success_at_3 = at_3 as f64 / total;

    OnlineMetrics {
        ctr,
        engaged_mrr: rr_total / total,
        success_at_1,
        success_at_3,
        abandonment: 1.0 - ctr,
        impressions: n,
    }
}
