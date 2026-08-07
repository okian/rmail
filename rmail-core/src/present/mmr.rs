//! Maximal Marginal Relevance: prd.md's Stage 6 diversification step —
//! "greedily pick results maximizing `λ·relevance − (1−λ)·max_similarity_to_
//! already_picked` so the top-10 isn't ten near-identical newsletters."
//!
//! # What "similarity" means here
//!
//! prd.md never pins down what "similarity" is computed *over* for this
//! step. [`crate::fuse::simhash`] already gives this crate a cheap, local,
//! well-tested notion of textual similarity — a 64-bit SimHash fingerprint
//! over a body's word-bigram shingles, with Hamming distance as the distance
//! metric — built for exactly this "are these two message bodies basically
//! the same text" question (Stage 2's near-duplicate collapse). This module
//! reuses that same fingerprint as a *graded* similarity
//! (`1 − hamming_distance / 64`) rather than [`crate::fuse::simhash::is_near_duplicate`]'s
//! binary threshold test: Stage 2 already collapsed anything within
//! [`crate::fuse::simhash::NEAR_DUP_HAMMING_THRESHOLD`] before this stage ever sees a
//! candidate, so what is left to diversify is exactly the messages that are
//! *not* byte-level duplicates but are still similar enough in substance to
//! flood a result page — ten newsletter issues from the same sender, each
//! personalized with a different tracking id or greeting, sit well outside
//! the near-dup threshold yet are still each other's closest neighbor in
//! SimHash space. That gap between "collapsed as a literal duplicate" and
//! "flagged as topically redundant" is precisely what MMR is for.
//!
//! A candidate whose body is too short to fingerprint at all
//! ([`crate::fuse::simhash::MIN_TOKENS_FOR_FINGERPRINT`]) is treated as similarity `0.0`
//! to everything — "no evidence of overlap," not "maximally similar" or
//! "excluded from diversification." A short message competing on relevance
//! alone, never penalized for a fingerprint it cannot have, is the safer
//! default in both directions: it can still win a slot on relevance, and it
//! can never be used to justify demoting something else as its "duplicate."
//!
//! # Relevance is normalized before it is compared against similarity
//!
//! [`crate::rank::RankedCandidate::score`]'s own doc comment is explicit that
//! its scale means nothing outside "whichever `Ranker` produced it" — the
//! cold-start [`crate::rank::l1::L1Ranker`]'s linear-combination scale is
//! typically a small positive number (a handful of RRF terms, each at most
//! `~1/(k_rrf+1)`, plus feature weights on `0..=1`-ish inputs), nothing like
//! the `0.0..=1.0` range similarity lives in. Mixing the two directly, as
//! prd.md's formula reads literally, would let the `(1−λ)` similarity term
//! dominate almost any relevance gap regardless of `λ`'s configured value —
//! diversity would win by default instead of by the tuned trade-off `λ` is
//! supposed to express. [`relevance_range`]/[`normalize`] apply the same
//! per-batch min-max normalization [`crate::fuse::fuse_scores`]'s `Fusion::Linear`
//! mode already uses for the identical reason ("BM25 magnitude vs cosine
//! incomparable"), including its degenerate-range convention: a single
//! candidate, or a batch where every score ties, normalizes to `1.0` rather
//! than `0.0` — evidence that the ranker found and scored it, not the
//! absence of a signal.
//!
//! # Enabled only for [`Intent::Exploratory`] — Lookup is not navigational
//! # by name, but behaves like it here
//!
//! prd.md's Stage 6 text names MMR "for exploratory intent... disabled for
//! navigational intent (where the user wants the single best match first)"
//! and says nothing about [`Intent::Lookup`] at all. This module resolves
//! that silence the same way task 31's `rank::l1::bulk_downweight_suppressed`
//! resolves an analogous prd.md gap: by the *shape* of the intent, not by a
//! literal absence of wording. Lookup's own prd.md examples — "tracking
//! number for my order", "AWS bill" — are structured-fact lookups with
//! exactly one right answer, the same "user wants the single best match
//! first" property Stage 6 gives as navigational's own reason to disable
//! MMR; diversifying away from the one correct tracking number to make room
//! for three unrelated-but-different results would contradict what Lookup
//! intent means. MMR is therefore enabled only for [`Intent::Exploratory`] —
//! the one intent whose Stage 2 fusion weights are also tuned toward *broad*
//! recall (`dense: 1.0`, `lexical: 0.7`, `recency: 0.3`) rather than
//! precision, which is the only intent where a diverse top-N instead of a
//! strict-relevance top-N is the stated goal in the first place.
//!
//! # The first pick is always the single most relevant candidate
//!
//! With nothing yet selected, every candidate's `max_similarity_to_already_
//! picked` term is `0.0` regardless of `λ` (there is nothing to compare
//! against), so the very first slot is decided by relevance alone. This
//! falls out of the algorithm rather than being special-cased, and it is a
//! useful invariant to keep in mind reading the tests: MMR only ever
//! *reorders what comes after* the top relevance pick, never bumps a
//! stronger match out of first place for the sake of diversity.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use crate::fuse::simhash::hamming_distance;
use crate::query::Intent;
use crate::rank::RankedCandidate;

/// prd.md's Stage 6 default: `λ default 0.7`. Config-overridable via
/// `search.mmr_lambda` ([`crate::config::SearchConfig::mmr_lambda`]) —
/// [`diversify`] takes `lambda` as a plain argument rather than reading that
/// config field itself, mirroring [`crate::rank::Ranker::rank`]'s own
/// "`top_k` a caller-supplied cut... rather than a config value read
/// internally" contract (see that trait's doc comment): a pure function of
/// its arguments needs no [`crate::config::SearchConfig`] fixture to test.
pub const DEFAULT_LAMBDA: f64 = 0.7;

/// Fingerprint width, bits — mirrors [`crate::fuse::simhash::fingerprint`]'s 64-bit
/// output, named here rather than inlined as a bare `64.0` so
/// [`similarity`]'s normalization is visibly tied to that width rather than
/// an unexplained magic number.
const FINGERPRINT_BITS: f64 = 64.0;

/// Whether MMR runs at all for `intent` — see the module docs' "Enabled only
/// for `Intent::Exploratory`" section for why [`Intent::Lookup`] joins
/// [`Intent::Navigational`] here despite prd.md never naming it explicitly.
#[must_use]
pub fn enabled_for(intent: Intent) -> bool {
    matches!(intent, Intent::Exploratory)
}

/// Greedily select and order up to `limit` of `candidates`, trading
/// relevance for diversity per prd.md's Stage 6 formula:
/// `λ·relevance − (1−λ)·max_similarity_to_already_picked`.
///
/// `relevance` is `candidates`' own [`RankedCandidate::score`], min-max
/// normalized over the whole `candidates` batch (see the module docs).
/// `fingerprints` supplies each candidate's [`crate::fuse::simhash::fingerprint`], keyed
/// by [`RankedCandidate::message_id`]; a candidate absent from it (body too
/// short to fingerprint, or no body at all) contributes similarity `0.0` to
/// every comparison. `lambda` outside `0.0..=1.0` or non-finite is clamped
/// to [`DEFAULT_LAMBDA`] — an untrusted config value must not invert the
/// trade-off or poison every objective to `NaN`.
///
/// A selected candidate's [`RankedCandidate::score`] in the output is
/// unchanged from its input — MMR decides *order and membership*, not a new
/// notion of "score"; see `present`'s module docs for why the field a
/// diversified list carries forward is still the original relevance, and
/// why that list is not guaranteed monotonic in it.
///
/// Ties in the per-step objective break toward higher original relevance,
/// then lower `message_id` — the same two-level tie-break
/// [`crate::rank::Ranker::rank`] itself uses, so two candidates that are
/// indistinguishable by every signal this function has still resolve to one
/// deterministic order rather than whatever [`Vec`] iteration happened to
/// produce.
#[must_use]
pub fn diversify(
    candidates: &[RankedCandidate],
    fingerprints: &BTreeMap<i64, u64>,
    lambda: f64,
    limit: usize,
) -> Vec<RankedCandidate> {
    if candidates.is_empty() || limit == 0 {
        return Vec::new();
    }
    let lambda = sane_lambda(lambda);
    let (min, max) = relevance_range(candidates);

    let mut remaining: Vec<RankedCandidate> = candidates.to_vec();
    let mut selected: Vec<RankedCandidate> = Vec::with_capacity(limit.min(candidates.len()));
    let mut selected_fingerprints: Vec<u64> = Vec::with_capacity(limit.min(candidates.len()));

    while !remaining.is_empty() && selected.len() < limit {
        let mut best_idx = 0usize;
        let mut best_objective = f64::MIN;
        for (idx, candidate) in remaining.iter().enumerate() {
            let relevance = normalize(candidate.score, min, max);
            let similarity = fingerprints
                .get(&candidate.message_id)
                .map_or(0.0, |&fp| max_similarity(fp, &selected_fingerprints));
            let objective = lambda * relevance - (1.0 - lambda) * similarity;

            if is_better(objective, candidate, best_objective, &remaining[best_idx]) {
                best_objective = objective;
                best_idx = idx;
            }
        }
        let picked = remaining.remove(best_idx);
        if let Some(&fp) = fingerprints.get(&picked.message_id) {
            selected_fingerprints.push(fp);
        }
        selected.push(picked);
    }
    selected
}

/// Whether `(objective, candidate)` beats the current best
/// `(best_objective, best)` — `Greater` on the objective wins outright;
/// exact equality falls through to the same relevance-then-`message_id`
/// tie-break [`crate::rank::l1::L1Ranker::rank`] uses for its own sort, so
/// two candidates with an identical MMR objective (routine when `lambda` is
/// `0.0` or `1.0`, or two candidates share every relevant signal) still
/// resolve deterministically.
fn is_better(
    objective: f64,
    candidate: &RankedCandidate,
    best_objective: f64,
    best: &RankedCandidate,
) -> bool {
    match objective
        .partial_cmp(&best_objective)
        .unwrap_or(Ordering::Equal)
    {
        Ordering::Greater => true,
        Ordering::Less => false,
        Ordering::Equal => match candidate
            .score
            .partial_cmp(&best.score)
            .unwrap_or(Ordering::Equal)
        {
            Ordering::Greater => true,
            Ordering::Less => false,
            Ordering::Equal => candidate.message_id < best.message_id,
        },
    }
}

/// The highest [`similarity`] between `fingerprint` and any fingerprint
/// already in `selected` — prd.md's `max_similarity_to_already_picked`.
/// `0.0` (no penalty) when `selected` is empty, which is what makes the
/// first pick relevance-only (see the module docs).
fn max_similarity(fingerprint: u64, selected: &[u64]) -> f64 {
    selected
        .iter()
        .map(|&other| similarity(fingerprint, other))
        .fold(0.0_f64, f64::max)
}

/// Graded textual similarity from two SimHash fingerprints: `1.0` for
/// identical fingerprints, `0.0` for maximally different (all 64 bits
/// flipped) — see the module docs' "What similarity means here" section.
fn similarity(a: u64, b: u64) -> f64 {
    1.0 - f64::from(hamming_distance(a, b)) / FINGERPRINT_BITS
}

/// `(min, max)` of `candidates`' [`RankedCandidate::score`] — the range
/// [`normalize`] scales relevance into `0.0..=1.0` against.
fn relevance_range(candidates: &[RankedCandidate]) -> (f64, f64) {
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for candidate in candidates {
        min = min.min(candidate.score);
        max = max.max(candidate.score);
    }
    (min, max)
}

/// `score`, min-max normalized against `[min, max]`. A degenerate range
/// (`max <= min` — one candidate, or every score tied) maps to `1.0` rather
/// than `0.0`: see the module docs' "Relevance is normalized" section for
/// why that mirrors [`crate::fuse::fuse_scores`]'s identical convention.
fn normalize(score: f64, min: f64, max: f64) -> f64 {
    if max > min {
        (score - min) / (max - min)
    } else {
        1.0
    }
}

/// A `lambda` that cannot invert the trade-off or poison every objective to
/// `NaN` — mirrors `retrieve::lexical`'s `sane`/`features::extract`'s
/// `sane_weight` precedent for an untrusted config float read straight from
/// TOML/env with no upper-bound validation anywhere in `config`.
fn sane_lambda(lambda: f64) -> f64 {
    if lambda.is_finite() && (0.0..=1.0).contains(&lambda) {
        lambda
    } else {
        tracing::warn!(
            configured = lambda,
            default = DEFAULT_LAMBDA,
            "mmr_lambda must be within 0.0..=1.0; using the default"
        );
        DEFAULT_LAMBDA
    }
}

#[cfg(test)]
mod tests;
