//! The optimizer: inverse-propensity-weighted pairwise logistic regression
//! over [`super::labels::PreferencePair`]s, producing a
//! [`crate::rank::l1::Weights`] table.
//!
//! # Why this optimizes a pairwise surrogate rather than NDCG directly
//!
//! NDCG is a step function of the ranking — it is piecewise constant in the
//! weights, so its gradient is zero almost everywhere and undefined at the
//! swaps. Nothing gradient-based can descend it. The standard resolution
//! (RankNet, and everything descended from it) is to descend a smooth
//! *surrogate* whose minimum orders pairs the way the labels do, and then to
//! judge the result by the metric you actually care about. That is exactly
//! what this module and [`super::Trainer`] do together: fit the surrogate
//! here, and let the guardrail decide on measured NDCG@10 over a held-out
//! slice. prd.md's "optimizing NDCG" is a statement about the acceptance
//! criterion, and it is enforced where acceptance happens.
//!
//! # Why features are standardized, and why the model is not
//!
//! The 34 features are on wildly different scales — `msg_length` is in the
//! thousands, `is_flagged` is 0 or 1, `rrf_score` is a small fraction. Plain
//! gradient descent on raw values is dominated by whichever feature happens
//! to have the largest variance, and no single learning rate works for all of
//! them. So the fit runs in standardized space (`z = (x - mu) / sigma`) and
//! then folds `sigma` back into the weights:
//!
//! ```text
//! score(a) - score(b) = Σ w_z,i (z_a,i - z_b,i) = Σ (w_z,i / sigma_i) (x_a,i - x_b,i)
//! ```
//!
//! The centering term cancels because ranking only ever compares two
//! candidates *within* one query, so a constant offset shared by every
//! candidate cannot change an order. That is what lets the artifact be an
//! ordinary [`crate::rank::l1::Weights`] table over **raw** features, with no
//! normalization step to carry alongside it and no second place for the
//! serving path to disagree with the trainer. A model that needed its
//! training-set statistics at serving time would be a model whose scores
//! silently change meaning when those statistics are lost.
//!
//! Two kinds of feature come out of the fit exactly as they went in, for
//! different reasons and with different consequences — see [`Standardizer`].
//! One is a feature with (near-)zero variance in the training data: it
//! carries no gradient, and dividing by its `sigma` to fold back would turn a
//! rounding error into an enormous weight. That is not hypothetical —
//! `is_pinned`, `has_tag_match` and `ai_priority` are hard-coded constants in
//! this build (see `features::extract`), so on a real corpus at least three
//! features are constant every time. The other is the two features whose
//! flattened value is a *category ordinal* rather than a magnitude
//! ([`is_categorical`]); those stay in the margin and only their weight is
//! held still.
//!
//! # Why the pull is toward the live model, not toward zero
//!
//! The L2 term penalizes `(w - w_init)`, not `w`. Ordinary ridge regression
//! shrinks an under-determined weight toward zero, which here means "toward a
//! ranker that ignores that feature" — a strictly worse default than the
//! hand-tuned value prd.md spent a section deriving. Regularizing toward the
//! incumbent instead makes the trainer's null result "leave it alone", which
//! is the behaviour a nightly job on a quiet mailbox should have. It also
//! makes the guardrail comparison honest: with no usable signal the candidate
//! converges to the incumbent, the two score identically on the held-out
//! slice, the difference is zero, and no swap happens.

use tokio_util::sync::CancellationToken;

use crate::features::FeatureName;
use crate::rank::l1::Weights;

use super::labels::{PreferencePair, FEATURES};
use super::TrainError;

/// Below this standard deviation a feature counts as constant across the
/// training set and is left out of the fit.
///
/// Not `0.0`: the variance is accumulated in floating point over thousands of
/// rows, so a genuinely constant feature can land a few ULPs off zero, and
/// dividing by that when folding `sigma` back would produce a weight of
/// 10^15. The threshold is far below any real feature's spread (the smallest
/// is a boolean, whose standard deviation cannot be under ~0.001 unless one
/// value occurs less than a millionth of the time).
const MIN_STDDEV: f64 = 1e-9;

/// Knobs [`fit`] reads. A struct rather than four arguments so a caller
/// cannot transpose the learning rate and the L2 term.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FitParams {
    /// Full-batch gradient-descent passes.
    pub epochs: u32,
    /// Step size in standardized space.
    pub learning_rate: f64,
    /// Strength of the pull back toward `init`.
    pub l2: f64,
}

/// What one fit produced.
#[derive(Debug, Clone, PartialEq)]
pub struct FitOutcome {
    /// The candidate weight table, over raw feature values.
    pub weights: Weights,
    /// Weighted mean pairwise loss before the first step — the incumbent
    /// model's own loss on this pair set.
    pub initial_loss: f64,
    /// The same quantity after the last step. Reported alongside the initial
    /// one because "the loss went down" and "the ranking got better for the
    /// user" are different claims, and only the second one is allowed to swap
    /// a model; seeing both is how an operator tells a run that learned
    /// nothing from a run that learned the wrong thing.
    pub final_loss: f64,
}

/// Fit a weight table to `pairs`, starting from `init`.
///
/// The loss reported is the data term only (the regularizer is part of the
/// objective descended, not of the number reported): a penalty for moving
/// away from the incumbent is not something an operator reading "did it order
/// the observed preferences better" wants folded in.
///
/// # Errors
///
/// [`TrainError::Cancelled`] if `cancel` fires between epochs — checked
/// per epoch rather than per pair, so a run stops within one pass rather than
/// promptly-but-halfway through a gradient. [`TrainError::Diverged`] if the
/// weights leave the finite range, which a large `learning_rate` can do; the
/// alternative is persisting a table whose every score is `NaN`, and
/// `L1Ranker::rank`'s sort would answer that with silent `message_id` order.
pub fn fit(
    pairs: &[PreferencePair],
    init: &Weights,
    params: &FitParams,
    cancel: &CancellationToken,
) -> Result<FitOutcome, TrainError> {
    let stats = Standardizer::over(pairs);
    let init_raw = flatten(init);
    // The incumbent, expressed in standardized space: `w_z = w_raw * sigma`
    // is the inverse of the fold-back below, so epoch 0 scores exactly what
    // the live ranker scores.
    let mut weights = [0.0f64; FEATURES];
    for i in 0..FEATURES {
        weights[i] = init_raw[i] * stats.stddev[i];
    }
    let anchor = weights;

    let total_weight: f64 = pairs.iter().map(|pair| pair.weight).sum();
    if pairs.is_empty() || total_weight <= 0.0 || !total_weight.is_finite() {
        // Nothing to learn from. Returning `init` unchanged (rather than an
        // error) keeps this total; the caller's `min_pairs` bound is what
        // decides that a run with too little data should not have started.
        return Ok(FitOutcome {
            weights: init.clone(),
            initial_loss: 0.0,
            final_loss: 0.0,
        });
    }

    // Pair differences, standardized, computed once: every epoch needs
    // exactly `z_pos - z_neg` and nothing else about the two documents, and
    // recomputing it per epoch would be the dominant cost of the whole run.
    let deltas: Vec<[f64; FEATURES]> = pairs
        .iter()
        .map(|pair| stats.standardized_delta(&pair.positive, &pair.negative))
        .collect();

    let initial_loss = mean_loss(&deltas, pairs, &weights, total_weight);

    for _ in 0..params.epochs {
        if cancel.is_cancelled() {
            return Err(TrainError::Cancelled);
        }
        let mut gradient = [0.0f64; FEATURES];
        for (delta, pair) in deltas.iter().zip(pairs) {
            let margin = dot(&weights, delta);
            // d/dmargin of softplus(-margin) is -sigmoid(-margin): large when
            // the pair is ordered wrongly, vanishing once it is ordered right
            // by a comfortable margin. That saturation is why a handful of
            // heavily-weighted deep clicks cannot drag the model arbitrarily
            // far once they are already satisfied.
            let scale = -sigmoid(-margin) * pair.weight;
            for i in 0..FEATURES {
                gradient[i] += scale * delta[i];
            }
        }
        for i in 0..FEATURES {
            if stats.pinned[i] {
                continue;
            }
            let grad = gradient[i] / total_weight + params.l2 * (weights[i] - anchor[i]);
            weights[i] -= params.learning_rate * grad;
        }
        if weights.iter().any(|w| !w.is_finite()) {
            return Err(TrainError::Diverged);
        }
    }

    let final_loss = mean_loss(&deltas, pairs, &weights, total_weight);

    let mut out = init.clone();
    for (i, name) in FeatureName::ALL.into_iter().enumerate() {
        if stats.pinned[i] {
            // Left exactly where it started — see the module docs.
            continue;
        }
        let raw = weights[i] / stats.stddev[i];
        if !raw.is_finite() {
            return Err(TrainError::Diverged);
        }
        out.set(name, raw);
    }

    Ok(FitOutcome {
        weights: out,
        initial_loss,
        final_loss,
    })
}

/// Per-feature spread over the training pairs, and which features the fit
/// must leave alone.
///
/// Two distinct exclusions, and conflating them is a real bug rather than an
/// inefficiency:
///
/// - [`Standardizer::constant`] means "this feature does not vary here", so
///   its standardized delta is `0` and dividing by its `sigma` would be
///   dividing by noise. It is out of the *arithmetic* entirely.
/// - [`Standardizer::pinned`] means "this feature's weight must not move".
///   Its delta is real and stays in the margin, because the margin has to be
///   the score difference the incumbent would actually compute — the whole
///   claim that epoch 0 reproduces the live ranker rests on it. Only the
///   gradient step and the fold-back skip it.
///
/// Freezing a categorical out of the *margin* as well would silently descend
/// a different function than the one being served whenever
/// `[search.rank_weights]` gives `best_match_field`/`best_source` a weight —
/// which `Weights::from_config` accepts for any feature name.
struct Standardizer {
    stddev: [f64; FEATURES],
    /// No variance in this corpus: contributes nothing to any margin.
    constant: [bool; FEATURES],
    /// Weight held at its starting value: no gradient step, no fold-back.
    /// Implied by `constant` (a feature with no gradient cannot move anyway)
    /// and additionally true of the two category ordinals.
    pinned: [bool; FEATURES],
}

/// Whether `name` is one of the two features whose numeric value is a
/// category ordinal rather than a magnitude.
///
/// `best_match_field` and `best_source` flatten to fixed ordinals — subject
/// is 1, from is 2, body is 3 — and those numbers order nothing. A linear
/// model given a weight for them is asserting "attachment is four times
/// subject", which is not a claim anyone made; whichever sign the gradient
/// happens to land on would then tilt every ranking by an accident of how
/// [`crate::features::MatchField`]'s variants are declared. This is the same
/// judgement [`crate::rank::l1::Weights::cold_start`] already made by
/// assigning them no weight ("a category has no sign a linear model could
/// sensibly assign without a per-category expansion"); training has to make
/// it too, or the first nightly run quietly undoes it.
///
/// A tree model would be a different matter — `as_pairs`' own docs note these
/// are "a real, useful split feature" for one — which is why the exclusion
/// lives here, in the linear optimizer, rather than in the flattening.
fn is_categorical(name: FeatureName) -> bool {
    matches!(name, FeatureName::BestMatchField | FeatureName::BestSource)
}

impl Standardizer {
    /// Accumulate over every endpoint of every pair.
    ///
    /// Endpoints rather than distinct documents: a document appearing in five
    /// pairs is counted five times. That is deliberate — the scale this is
    /// correcting for is the scale the *gradient* sees, and the gradient sums
    /// over pairs, so the distribution that matters is the distribution over
    /// pair endpoints.
    fn over(pairs: &[PreferencePair]) -> Self {
        let mut sum = [0.0f64; FEATURES];
        let mut sum_sq = [0.0f64; FEATURES];
        let mut n = 0.0f64;
        for pair in pairs {
            for side in [&pair.positive, &pair.negative] {
                for i in 0..FEATURES {
                    sum[i] += side[i];
                    sum_sq[i] += side[i] * side[i];
                }
            }
            n += 2.0;
        }

        let mut stddev = [1.0f64; FEATURES];
        let mut constant = [true; FEATURES];
        let mut pinned = [true; FEATURES];
        if n > 0.0 {
            for (i, name) in FeatureName::ALL.into_iter().enumerate() {
                let mean = sum[i] / n;
                // `max(0.0)` because catastrophic cancellation in this
                // one-pass form can make a genuinely-zero variance come out
                // very slightly negative, and `sqrt` of that is `NaN`.
                let variance = (sum_sq[i] / n - mean * mean).max(0.0);
                let sd = variance.sqrt();
                if sd.is_finite() && sd > MIN_STDDEV {
                    stddev[i] = sd;
                    constant[i] = false;
                    // A varying categorical still contributes to the margin;
                    // it just never gets a gradient step.
                    pinned[i] = is_categorical(name);
                }
            }
        }
        Self {
            stddev,
            constant,
            pinned,
        }
    }

    /// `z_positive - z_negative`. The means cancel, so this is just the raw
    /// difference scaled — which is also why the fitted model needs no
    /// intercept.
    fn standardized_delta(
        &self,
        positive: &[f64; FEATURES],
        negative: &[f64; FEATURES],
    ) -> [f64; FEATURES] {
        let mut delta = [0.0f64; FEATURES];
        for i in 0..FEATURES {
            if self.constant[i] {
                continue;
            }
            delta[i] = (positive[i] - negative[i]) / self.stddev[i];
        }
        delta
    }
}

/// The weight table as a flat array in [`FeatureName::ALL`] order — the same
/// order [`crate::features::FeatureVector::as_pairs`] emits, which is what
/// makes index `i` here and index `i` in a flattened feature vector the same
/// feature. `rank::train::tests::feature_order_is_shared_by_name_and_vector`
/// pins that.
fn flatten(weights: &Weights) -> [f64; FEATURES] {
    let mut out = [0.0f64; FEATURES];
    for (slot, name) in out.iter_mut().zip(FeatureName::ALL) {
        *slot = weights.get(name);
    }
    out
}

fn dot(weights: &[f64; FEATURES], delta: &[f64; FEATURES]) -> f64 {
    (0..FEATURES).map(|i| weights[i] * delta[i]).sum()
}

/// Numerically stable logistic sigmoid.
fn sigmoid(x: f64) -> f64 {
    if x >= 0.0 {
        1.0 / (1.0 + (-x).exp())
    } else {
        let e = x.exp();
        e / (1.0 + e)
    }
}

/// `ln(1 + e^x)`, without overflowing for large `x`.
fn softplus(x: f64) -> f64 {
    x.max(0.0) + (-(x.abs())).exp().ln_1p()
}

/// Propensity-weighted mean pairwise logistic loss.
fn mean_loss(
    deltas: &[[f64; FEATURES]],
    pairs: &[PreferencePair],
    weights: &[f64; FEATURES],
    total_weight: f64,
) -> f64 {
    if total_weight <= 0.0 || !total_weight.is_finite() {
        return 0.0;
    }
    let sum: f64 = deltas
        .iter()
        .zip(pairs)
        .map(|(delta, pair)| pair.weight * softplus(-dot(weights, delta)))
        .sum();
    sum / total_weight
}
