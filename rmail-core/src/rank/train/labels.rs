//! Clicks into position-bias-corrected pairwise labels (prd.md, "Turning
//! clicks into labels").
//!
//! Everything here is a pure function over already-decoded feedback rows: no
//! database, no clock, no configuration read behind the caller's back. That is
//! what lets the propensity model be tested as arithmetic rather than as an
//! emergent property of a training run.
//!
//! # The propensity model, and exactly what it assumes
//!
//! prd.md asks for "a simple position-based click model" estimating
//! examination propensity. This module implements the standard PBM
//! factorization:
//!
//! ```text
//! P(action on d at rank p) = P(examine | p) * P(want | d, query)
//! P(examine | p)           = p^-eta            (1-based p, eta >= 0)
//! ```
//!
//! and weights each preference pair by the inverse of the *positive*
//! document's examination probability, `p^eta`, clipped. A click at rank 8
//! therefore carries eight times the weight of a click at rank 1 under the
//! default `eta = 1` — prd.md's own worked example, and the whole point:
//! without it the trainer would mostly relearn whatever the incumbent ranker
//! already put on top, because that is where clicks are.
//!
//! Five assumptions, all of them real and none of them hidden:
//!
//! 1. **Examination depends on rank alone.** Not on the query, not on the
//!    document, not on how good its neighbours looked. A visually distinctive
//!    result (an attachment icon, a bold sender) genuinely does draw more
//!    attention than this model admits, and rmail's own presentation layer is
//!    not uniform across results. What the model gets wrong here it gets
//!    wrong in a direction that is uncorrelated with rank, so it adds noise
//!    rather than a systematic tilt.
//! 2. **An action implies examination.** Used in both directions: a clicked
//!    document was seen, and — the skip-above rule below — every document
//!    *above* a clicked one was seen on the way to it. This is Joachims'
//!    original skip-above heuristic and it is why a result ranked below the
//!    click is never used as a negative: not looking at something is not
//!    evidence against it.
//! 3. **Relevance is independent of position given examination.** The second
//!    factor above has no `p` in it. If a user's willingness to open a result
//!    genuinely depends on where it sat — not just whether they saw it —
//!    IPS does not remove that bias, it only removes the examination half.
//! 4. **The held-out slice never rotates.** [`is_holdout`] is a pure function
//!    of the group key, which is what makes a split reproducible and
//!    leak-free — and also means the *same* quarter of a user's search
//!    vocabulary is held out for the life of the mailbox. Two consequences,
//!    both accepted deliberately: the trainer never learns from those
//!    queries, and hundreds of nightly accept/reject decisions accumulate
//!    against one fixed slice, which is the classic adaptive-overfitting
//!    setup (each verdict leaks a little information about the holdout into
//!    the model that survives). Rotating the slice would trade both away for
//!    a worse problem — a model accepted against one slice and then judged
//!    against another is not being compared to anything — so the honest
//!    position is that the guardrail is a *filter on regressions*, which it
//!    is reliable at, rather than an unbiased estimate of how much better a
//!    model is.
//! 5. **`eta` is a constant, not a fit.** A real unbiased-LTR pipeline
//!    estimates the propensity curve from result randomization or from an
//!    EM pass over the logs. rmail deliberately does neither: randomizing a
//!    user's own search results to estimate a curve is a cost paid by the one
//!    person the product exists for, and an EM estimate over a few thousand
//!    local impressions is noise. `eta` is therefore a configured constant
//!    (`search.training.position_bias_eta`) with `1.0` as the default, and
//!    `0.0` turns the correction off entirely if an operator decides their
//!    click pattern does not fit the model.
//!
//! The clipping in [`pair_weight`] is standard IPS variance control: unclipped
//! inverse propensity has unbounded variance, and on a corpus of a few
//! thousand impressions a single click at rank 50 would otherwise outvote
//! every honest signal above it. It trades a little bias for a lot of
//! variance, and the ceiling is configured
//! (`search.training.max_propensity_weight`).
//!
//! # Which pairs exist
//!
//! For one logged query, with [`grade`] over its actions:
//!
//! > `a ≻ b` whenever `grade(a) > grade(b)` **and** the user demonstrably
//! > examined `b` — that is, `b` was ranked *above* `a` (assumption 2), or `b`
//! > carries an action of its own (an archive-from-results, a scroll-past).
//!
//! A document ranked below `a` that the user never touched is not a negative.
//! It is unlabeled, and treating it as a negative is the single most common
//! way an offline LTR pipeline teaches itself never to surface anything new —
//! the identical argument [`crate::eval::replay`]'s module docs make for why
//! shadow scoring drops unshown candidates.

use std::collections::HashMap;

use crate::features::FeatureVector;
use crate::feedback::ActionKind;
use crate::query::Intent;

/// Dwell at or above which a dwell counts as prd.md's "long dwell" — the
/// signal it ranks alongside a reply.
///
/// Thirty seconds is the conventional "satisfied click" threshold in the
/// click-model literature and is long enough that it cannot be an accidental
/// preview. It is a constant rather than a config knob because it is part of
/// the *definition* of the grade vocabulary below, not a tuning dial: moving
/// it would silently redefine what every previously stored model was trained
/// on.
pub const LONG_DWELL_MS: i64 = 30_000;

/// One result the user was shown, as the ranker scored it.
#[derive(Debug, Clone, PartialEq)]
pub struct ShownResult {
    /// `messages.id`.
    pub message_id: i64,
    /// 1-based rank on the page the user saw.
    pub position: u32,
    /// The vector the live ranker actually scored — never a re-derivation;
    /// see [`crate::feedback`]'s module docs for why that distinction decides
    /// whether training is sound.
    pub features: FeatureVector,
}

/// One thing the user did with a result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObservedAction {
    /// The result acted on.
    pub message_id: i64,
    /// Which action.
    pub kind: ActionKind,
    /// Milliseconds dwelled, for [`ActionKind::Dwell`].
    pub dwell_ms: Option<i64>,
}

/// One logged search, decoded: what it showed, in what order, under which
/// intent, and what the user then did.
#[derive(Debug, Clone, PartialEq)]
pub struct LoggedQuery {
    /// `search_log.query_id`.
    pub query_id: i64,
    /// The query text as typed. Carried for the report and for
    /// [`crate::eval::replay::Impression::query`]; nothing here keys on it.
    pub raw_query: String,
    /// `search_log.norm_hash` — the split key. Two spellings of the same
    /// search share it, which is what keeps them on the same side of the
    /// train/holdout line (see [`is_holdout`]).
    pub group_key: Vec<u8>,
    /// The intent the ranker scored under. Load-bearing: the L1 scorer gates
    /// two weights on it, so a replay under the wrong intent reproduces a
    /// different score than the user saw.
    pub intent: Intent,
    /// Results in the order they were presented, best first.
    pub shown: Vec<ShownResult>,
    /// Actions against those results.
    pub actions: Vec<ObservedAction>,
}

/// What the user's actions on one result say about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Verdict {
    /// prd.md's ordering as a small integer, highest first: reply and
    /// long-dwell 3, open 2, short dwell 1, untouched or scrolled past 0,
    /// archived-from-results -1 (prd.md's "mild negative").
    pub grade: i32,
    /// Whether the user did *anything* to this result. Distinct from a grade
    /// of `0`: an untouched result below the click and a deliberate
    /// scroll-past both grade `0`, and only the second one is evidence.
    pub examined: bool,
}

/// The verdict for every result `query` showed, keyed by message id.
///
/// A result with no action at all gets `Verdict::default()` — grade `0`, not
/// examined. Multiple actions on one result collapse to the *highest* grade:
/// a result the user opened and then archived was wanted (they opened it),
/// and reading that pair as a negative would label the ranker wrong for
/// having found it.
#[must_use]
pub fn grades(query: &LoggedQuery) -> HashMap<i64, Verdict> {
    let mut verdicts: HashMap<i64, Verdict> = query
        .shown
        .iter()
        .map(|shown| (shown.message_id, Verdict::default()))
        .collect();
    for action in &query.actions {
        // An action naming a result this query never showed cannot be
        // attributed to a position or a feature vector, so it contributes
        // nothing. `feedback::repo::insert_actions` already refuses to write
        // one; this is what keeps that guarantee from being the only thing
        // standing between a corrupted row and a fabricated label.
        let Some(verdict) = verdicts.get_mut(&action.message_id) else {
            continue;
        };
        verdict.examined = true;
        verdict.grade = verdict.grade.max(grade(action.kind, action.dwell_ms));
    }
    verdicts
}

/// prd.md's action ordering as an integer grade.
#[must_use]
pub fn grade(kind: ActionKind, dwell_ms: Option<i64>) -> i32 {
    match kind {
        ActionKind::Reply => 3,
        ActionKind::Dwell => {
            if dwell_ms.unwrap_or(0) >= LONG_DWELL_MS {
                3
            } else {
                1
            }
        }
        ActionKind::Open => 2,
        ActionKind::ScrollPast => 0,
        ActionKind::Archive => -1,
    }
}

/// `P(the user examined the result at 1-based rank `position`)` under the
/// position-based click model — see the module docs.
///
/// Total: a `position` of `0` (which the schema's own `CHECK (position >= 1)`
/// makes unreachable) and a non-finite result both read as "certainly
/// examined", the assumption that yields the *smallest* weight and therefore
/// cannot inflate a corrupt row into a dominant training example.
#[must_use]
pub fn examination_propensity(position: u32, eta: f64) -> f64 {
    if position == 0 {
        return 1.0;
    }
    let p = f64::from(position).powf(-eta);
    if p.is_finite() && p > 0.0 {
        p
    } else {
        1.0
    }
}

/// The inverse-propensity weight of a preference pair whose *positive*
/// document sat at `position`, clipped to `max_weight`.
///
/// Clamped with `min`/`max` rather than [`f64::clamp`] deliberately: `clamp`
/// panics when its bounds cross, and `max_weight` reaches here from
/// configuration. A total function cannot be the reason a nightly job dies.
#[must_use]
pub fn pair_weight(position: u32, eta: f64, max_weight: f64) -> f64 {
    let propensity = examination_propensity(position, eta);
    let raw = 1.0 / propensity;
    if !raw.is_finite() {
        return max_weight.max(1.0);
    }
    raw.min(max_weight.max(1.0)).max(1.0)
}

/// One preference the trainer fits against: the user preferred `positive`
/// over `negative` for this query, and this is how much that observation is
/// worth.
#[derive(Debug, Clone, PartialEq)]
pub struct PreferencePair {
    /// The query this preference was observed in — carried so a diagnostic
    /// can name it, never used as a feature.
    pub query_id: i64,
    /// The preferred document's feature vector, already intent-gated (see
    /// [`gated_values`]).
    pub positive: [f64; FEATURES],
    /// The rejected document's, likewise.
    pub negative: [f64; FEATURES],
    /// Inverse propensity weight from [`pair_weight`].
    pub weight: f64,
}

/// How many features one candidate carries — [`FeatureVector::as_pairs`]'s
/// own arity, restated as a constant so the flat arrays this module passes to
/// the optimizer cannot silently disagree with it. `rank::train::tests`'
/// `feature_arity_matches_the_feature_vector` pins the two together.
pub const FEATURES: usize = 34;

/// A feature vector flattened into the exact values
/// [`crate::rank::l1::Weights::score`] would multiply its weights by under
/// `intent`.
///
/// The intent gate is applied *here*, to the values, rather than being left
/// for the model to rediscover. That is what makes the learned weight table
/// mean the same thing at training time and at serving time: the L1 scorer
/// zeroes the `is_newsletter`/`is_automated` weights under some intents, so a
/// model trained on ungated values would fit a coefficient for a term that
/// production then throws away — and would compensate for its absence by
/// distorting every other weight.
#[must_use]
pub fn gated_values(features: &FeatureVector, intent: Intent) -> [f64; FEATURES] {
    let mut out = [0.0; FEATURES];
    for (slot, (name, value)) in out.iter_mut().zip(features.as_pairs()) {
        *slot = if crate::rank::l1::bulk_downweight_suppressed(name, intent) {
            0.0
        } else {
            value
        };
    }
    out
}

/// Largest number of preference pairs one logged query contributes.
///
/// Pair generation is quadratic in the page: `MAX_IMPRESSIONS_PER_QUERY` is
/// 200 and each pair carries two flattened 34-feature arrays, so a
/// pathological page with a positive action on most of it would produce tens
/// of thousands of pairs and tens of megabytes — from *one* search. A real
/// page shows `search.default_limit` (25) results and carries one or two
/// clicks, which is one or two dozen pairs, so this is a ceiling on a
/// degenerate or forged log rather than a tuning knob.
///
/// Pairs past it are dropped from the tail, which is the *lowest*-ranked and
/// therefore least informative end of the enumeration, and the drop is
/// logged.
pub const MAX_PAIRS_PER_QUERY: usize = 512;

/// Every preference pair one logged query yields, at most
/// [`MAX_PAIRS_PER_QUERY`] of them.
///
/// Returns an empty vector for a query with no positive engagement, which is
/// most of them — a search whose answer was on screen and did not need
/// opening teaches the ranker nothing about ordering.
#[must_use]
pub fn pairs_for(query: &LoggedQuery, eta: f64, max_weight: f64) -> Vec<PreferencePair> {
    let verdicts = grades(query);
    let mut pairs = Vec::new();
    for positive in &query.shown {
        if pairs.len() >= MAX_PAIRS_PER_QUERY {
            tracing::debug!(
                query_id = query.query_id,
                cap = MAX_PAIRS_PER_QUERY,
                "logged query produced more preference pairs than the per-query cap; \
                 dropping the rest"
            );
            break;
        }
        let Some(pos_verdict) = verdicts.get(&positive.message_id) else {
            continue;
        };
        if pos_verdict.grade <= 0 {
            continue;
        }
        let weight = pair_weight(positive.position, eta, max_weight);
        let pos_values = gated_values(&positive.features, query.intent);
        for negative in &query.shown {
            if negative.message_id == positive.message_id {
                continue;
            }
            let Some(neg_verdict) = verdicts.get(&negative.message_id) else {
                continue;
            };
            if neg_verdict.grade >= pos_verdict.grade {
                continue;
            }
            // The skip-above rule, and its one extension. Above the positive:
            // the user passed it to get there. Otherwise: only if they touched
            // it themselves. Anything else is a document nobody looked at.
            let examined = negative.position < positive.position || neg_verdict.examined;
            if !examined {
                continue;
            }
            pairs.push(PreferencePair {
                query_id: query.query_id,
                positive: pos_values,
                negative: gated_values(&negative.features, query.intent),
                weight,
            });
            if pairs.len() >= MAX_PAIRS_PER_QUERY {
                break;
            }
        }
    }
    pairs
}

/// Whether a query group belongs to the held-out slice.
///
/// # Why the key is the group and not the logged query
///
/// The same search runs many times: a keystroke-driven search box logs one
/// query per pause, a user re-runs "acme invoice" every month, and every one
/// of those impressions shows nearly the same candidates with nearly the same
/// feature vectors and the same document clicked. Splitting by `query_id`
/// would therefore put near-duplicates of the *same* observation on both
/// sides of the line, and the guardrail would be measuring how well the model
/// memorized its own training set. Every measured NDCG would look good and the
/// hot-swap would fire on models that do nothing for the user.
///
/// `search_log.norm_hash` is the SHA-256 of the normalized query text, so it
/// is identical for every repeat of the same search regardless of spacing,
/// case or Unicode composition — which makes it exactly the grain a split has
/// to be taken at. Every impression of a given search text lands wholly in
/// training or wholly in holdout, never split across them.
///
/// The assignment is a deterministic hash of the key, not a shuffle: two runs
/// over the same log produce the same split, so a suspicious verdict can be
/// re-derived rather than re-rolled. It reuses [`crate::eval::replay::bucket`]
/// — written for deterministic A/B assignment, which is the same job — rather
/// than introducing a second hashing convention that could drift from it.
#[must_use]
pub fn is_holdout(group_key: &[u8], holdout_percent: u32) -> bool {
    if holdout_percent == 0 {
        return false;
    }
    if holdout_percent >= 100 {
        return true;
    }
    crate::eval::replay::bucket(&hex(group_key), 100) < holdout_percent
}

/// Lowercase hex, so the byte-valued group key can go through
/// [`crate::eval::replay::bucket`]'s `&str` interface without inventing a
/// second hash for it.
fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        // `write!` to a `String` is infallible; the `Result` is discarded
        // rather than unwrapped so this stays free of a panic path.
        let _ = write!(out, "{byte:02x}");
        out
    })
}
