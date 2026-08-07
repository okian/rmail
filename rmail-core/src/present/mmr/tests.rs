//! What task 32's own acceptance bullet demands proven, not merely asserted:
//! "given ten near-identical newsletters plus three distinct messages,
//! exploratory intent must surface the distinct ones inside the top-N while
//! navigational intent returns strict score order" — checked on the
//! *composition* of the result, not just that [`diversify`] ran without
//! panicking.

use std::collections::BTreeMap;

use super::*;
use crate::query::Intent;
use crate::rank::RankedCandidate;

fn candidate(message_id: i64, score: f64) -> RankedCandidate {
    RankedCandidate { message_id, score }
}

// ---------------------------------------------------------------------------
// The acceptance scenario itself
// ---------------------------------------------------------------------------

/// Ten near-identical newsletters (ids 1..=10, fingerprints 0..=9 — a few
/// bits apart from any other newsletter, well outside Stage 2's near-dup
/// collapse threshold of 3, but still each other's closest neighbor) and
/// three topically distinct messages (ids 101..=103), fingerprints chosen
/// far from the newsletter cluster (Hamming distance 32-48 of 64, i.e.
/// similarity 0.25-0.5 — clearly dissimilar, not the literal maximum
/// distance 64/similarity 0.0 a stronger-sounding "maximally different"
/// would claim) and from each other. The top two newsletters (ids 1, 2)
/// outscore all three distinct messages by raw relevance alone; MMR must
/// still demote id 2 below them once its redundancy with the already-picked
/// id 1 is weighed. See `newsletter_flood_scenario`'s own doc comment for
/// the exact score shape this depends on.
struct Scenario {
    candidates: Vec<RankedCandidate>,
    fingerprints: BTreeMap<i64, u64>,
}

fn newsletter_flood_scenario() -> Scenario {
    let mut candidates = Vec::new();
    let mut fingerprints = BTreeMap::new();
    // The two best-scoring newsletters (ids 1, 2) score above all three
    // distinct messages by *raw relevance alone* — id 2 in particular
    // (0.70) beats every distinct message's score (0.55..=0.65) — so a
    // pure sort by score would keep both newsletters ahead of every
    // distinct message. The remaining eight newsletters are compressed
    // into a low band (0.05..=0.40), well below the distinct band.
    // Fingerprints cluster newsletters tightly around zero (each only a
    // few bits from its neighbors — near enough to be each other's closest
    // match, far enough apart that Stage 2's own near-dup collapse, which
    // never runs at this layer, would not have caught them) and put the
    // three distinct messages maximally far from that cluster and from
    // each other.
    //
    // This shape is what makes
    // `exploratory_mmr_surfaces_distinct_messages_inside_the_top_n`'s
    // central claim — that MMR's top-5 *differs in composition* from
    // strict relevance order, not merely that it contains a distinct
    // message — checkable against a genuine crossover: id 2 outscores
    // every distinct message, so *only* the redundancy penalty against the
    // already-picked id 1 (both near-identical newsletters, real
    // similarity ~0.98 from their hand-set fingerprints 0 and 1) can be
    // what demotes it below them.
    let newsletter_scores = [1.0, 0.70, 0.40, 0.35, 0.30, 0.25, 0.20, 0.15, 0.10, 0.05];
    for (i, score) in newsletter_scores.into_iter().enumerate() {
        let id = i as i64 + 1;
        candidates.push(candidate(id, score));
        fingerprints.insert(id, i as u64);
    }
    candidates.push(candidate(101, 0.65));
    fingerprints.insert(101, 0xFFFF_FFFF_FFFF_0000);
    candidates.push(candidate(102, 0.60));
    fingerprints.insert(102, 0x0000_FFFF_FFFF_FFFF);
    candidates.push(candidate(103, 0.55));
    fingerprints.insert(103, 0xF0F0_F0F0_0F0F_0F0F);
    Scenario {
        candidates,
        fingerprints,
    }
}

#[test]
fn exploratory_mmr_surfaces_distinct_messages_inside_the_top_n() {
    let scenario = newsletter_flood_scenario();
    let top5 = diversify(
        &scenario.candidates,
        &scenario.fingerprints,
        DEFAULT_LAMBDA,
        5,
    );

    assert_eq!(top5.len(), 5);
    let distinct_in_top5 = top5.iter().filter(|c| c.message_id >= 101).count();
    assert!(
        distinct_in_top5 >= 1,
        "MMR must pull at least one topically distinct message into the top 5; got {top5:?}"
    );

    let actual_ids: Vec<i64> = top5.iter().map(|c| c.message_id).collect();
    assert_eq!(
        actual_ids,
        vec![1, 101, 102, 103, 2],
        "MMR must diversify: id 1 (dominant relevance), then the three \
         distinct messages ahead of id 2 (a newsletter that outscores all \
         three by raw relevance but is redundant with the already-picked \
         id 1), then id 2 itself once nothing else is left to prefer"
    );

    // Composition, not just count: strict relevance order would keep id 2
    // (0.70) ahead of every distinct message (0.55..=0.65) — MMR's own
    // order must genuinely differ from that, not merely happen to satisfy
    // "contains a distinct message" by coincidence.
    let mut strict_order = scenario.candidates.clone();
    strict_order.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.message_id.cmp(&b.message_id))
    });
    let strict_top5: Vec<i64> = strict_order.iter().take(5).map(|c| c.message_id).collect();
    assert_eq!(
        strict_top5,
        vec![1, 2, 101, 102, 103],
        "sanity check on the scenario's own scoring: id 2 must genuinely \
         outrank every distinct message by raw relevance alone"
    );
    assert_ne!(
        actual_ids, strict_top5,
        "a diversified top-5 must differ in composition from the undiversified one"
    );
}

#[test]
fn navigational_style_strict_order_returns_the_undiversified_top_n() {
    // This module only ever produces MMR's own diversified order; whether
    // navigational intent *runs* MMR at all is `enabled_for`'s job (see
    // `intent_gating` below). This test instead pins the complementary
    // half: `lambda = 1.0` (all relevance, no diversity term) must degrade
    // `diversify` itself to strict score order — proving the objective
    // formula is correct at the boundary, independent of the intent gate.
    let scenario = newsletter_flood_scenario();
    let top5 = diversify(&scenario.candidates, &scenario.fingerprints, 1.0, 5);

    let mut sorted_by_score = scenario.candidates.clone();
    sorted_by_score.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.message_id.cmp(&b.message_id))
    });
    let expected: Vec<i64> = sorted_by_score
        .iter()
        .take(5)
        .map(|c| c.message_id)
        .collect();
    let actual: Vec<i64> = top5.iter().map(|c| c.message_id).collect();
    assert_eq!(
        actual, expected,
        "lambda=1.0 must reduce to strict relevance order"
    );
}

// ---------------------------------------------------------------------------
// Intent gating
// ---------------------------------------------------------------------------

#[test]
fn mmr_is_enabled_only_for_exploratory_intent() {
    assert!(enabled_for(Intent::Exploratory));
    assert!(!enabled_for(Intent::Navigational));
    assert!(
        !enabled_for(Intent::Lookup),
        "see the module docs: Lookup behaves like navigational here"
    );
}

// ---------------------------------------------------------------------------
// Algorithmic properties
// ---------------------------------------------------------------------------

#[test]
fn the_first_pick_is_always_the_single_most_relevant_candidate() {
    // With nothing yet selected, every candidate's similarity term is 0.0
    // regardless of lambda, so the first slot must always go to the highest
    // relevance score -- this holds even at lambda values that heavily
    // favor diversity for every later pick.
    let scenario = newsletter_flood_scenario();
    for lambda in [0.0, 0.3, 0.5, 0.7, 1.0] {
        let picked = diversify(&scenario.candidates, &scenario.fingerprints, lambda, 1);
        assert_eq!(
            picked.first().map(|c| c.message_id),
            Some(1),
            "lambda={lambda}: id 1 has the single highest score and must be picked first"
        );
    }
}

#[test]
fn empty_candidates_returns_empty() {
    let out = diversify(&[], &BTreeMap::new(), DEFAULT_LAMBDA, 5);
    assert!(out.is_empty());
}

#[test]
fn a_limit_of_zero_returns_empty() {
    let scenario = newsletter_flood_scenario();
    let out = diversify(
        &scenario.candidates,
        &scenario.fingerprints,
        DEFAULT_LAMBDA,
        0,
    );
    assert!(out.is_empty());
}

#[test]
fn a_limit_past_the_candidate_count_returns_every_candidate_exactly_once() {
    let scenario = newsletter_flood_scenario();
    let out = diversify(
        &scenario.candidates,
        &scenario.fingerprints,
        DEFAULT_LAMBDA,
        1000,
    );
    assert_eq!(out.len(), scenario.candidates.len());
    let mut ids: Vec<i64> = out.iter().map(|c| c.message_id).collect();
    ids.sort_unstable();
    let mut expected: Vec<i64> = scenario.candidates.iter().map(|c| c.message_id).collect();
    expected.sort_unstable();
    assert_eq!(ids, expected, "every candidate must appear exactly once");
}

#[test]
fn candidates_with_no_fingerprint_never_penalize_or_get_penalized() {
    // A message too short to fingerprint contributes similarity 0.0 in both
    // directions: it does not count as a duplicate of anything, and nothing
    // else is judged similar to it.
    let candidates = vec![candidate(1, 1.0), candidate(2, 0.9), candidate(3, 0.5)];
    let mut fingerprints = BTreeMap::new();
    fingerprints.insert(1, 0u64);
    fingerprints.insert(2, 0u64); // identical fingerprint to 1 -- maximally similar
                                  // id 3 has no fingerprint at all.
    let out = diversify(&candidates, &fingerprints, 0.5, 3);
    assert_eq!(out.len(), 3);
    // id 3, with no fingerprint, must never be penalized as "too similar" to
    // anything -- it should out-rank the still-unfingerprinted-but-heavily-
    // redundant id 2 once id 1 is already picked (id 2 is a perfect
    // fingerprint match for id 1, id 3 has no evidence of overlap at all).
    let order: Vec<i64> = out.iter().map(|c| c.message_id).collect();
    assert_eq!(order[0], 1, "highest relevance picked first");
    assert_eq!(
        order[1], 3,
        "id 3 (no fingerprint, so similarity 0.0) must beat id 2 (identical \
         fingerprint to the already-picked id 1) once diversity is weighed: {order:?}"
    );
}

#[test]
fn ties_break_by_higher_original_relevance_then_lower_message_id() {
    // Two candidates that are identical in every way but message_id: with
    // lambda=1.0 (pure relevance) a genuine score tie must resolve to the
    // lower message_id, deterministically.
    let candidates = vec![candidate(20, 0.5), candidate(10, 0.5)];
    let out = diversify(&candidates, &BTreeMap::new(), 1.0, 2);
    assert_eq!(out[0].message_id, 10);
    assert_eq!(out[1].message_id, 20);
}

#[test]
fn output_score_is_the_original_relevance_not_an_mmr_objective_value() {
    let scenario = newsletter_flood_scenario();
    let out = diversify(&scenario.candidates, &scenario.fingerprints, 0.5, 13);
    for picked in &out {
        let original = scenario
            .candidates
            .iter()
            .find(|c| c.message_id == picked.message_id)
            .expect("every picked id came from the input");
        assert_eq!(
            picked.score, original.score,
            "diversify must not rewrite score to the internal mmr objective"
        );
    }
}

// ---------------------------------------------------------------------------
// Lambda sanitization
// ---------------------------------------------------------------------------

#[test]
fn sane_lambda_clamps_every_out_of_range_value_to_the_default() {
    // The direct unit test on the private clamp itself, so this property
    // does not depend on choosing candidates/fingerprints that happen to
    // make a downstream ordering difference visible.
    for bad in [
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        -1.0,
        1.5,
        -0.0001,
        1.0001,
    ] {
        assert_eq!(
            sane_lambda(bad),
            DEFAULT_LAMBDA,
            "lambda={bad} must clamp to the default"
        );
    }
    // The boundary values themselves are valid and must pass through
    // unchanged.
    assert_eq!(sane_lambda(0.0), 0.0);
    assert_eq!(sane_lambda(1.0), 1.0);
    assert_eq!(sane_lambda(0.3), 0.3);
}

#[test]
fn an_out_of_range_lambda_is_clamped_to_the_default_not_trusted() {
    // Real fingerprints (not an empty map), so the similarity term is
    // genuinely nonzero and a `NaN`/out-of-range `lambda` that *wasn't*
    // clamped would produce a visibly different order than the default —
    // an empty-fingerprints scenario would make this pass vacuously for
    // any `lambda > 0`, since the objective collapses to a monotone
    // transform of relevance alone regardless of whether clamping fired.
    let scenario = newsletter_flood_scenario();
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -1.0, 1.5] {
        let with_bad = diversify(&scenario.candidates, &scenario.fingerprints, bad, 5);
        let with_default = diversify(
            &scenario.candidates,
            &scenario.fingerprints,
            DEFAULT_LAMBDA,
            5,
        );
        assert_eq!(
            with_bad, with_default,
            "lambda={bad} must clamp to the default rather than poisoning every objective"
        );
    }
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

#[test]
fn similarity_of_identical_fingerprints_is_one_and_maximally_different_is_zero() {
    assert_eq!(similarity(0, 0), 1.0);
    assert_eq!(similarity(0, u64::MAX), 0.0);
}

#[test]
fn normalize_handles_a_degenerate_range_as_full_relevance() {
    // A single candidate, or every score tied: `max <= min`, so normalize
    // must read as "fully relevant" (1.0), matching `fuse::fuse_scores`'s
    // own linear-fusion convention for a degenerate range.
    assert_eq!(normalize(5.0, 5.0, 5.0), 1.0);
}

#[test]
fn normalize_scales_a_real_range_into_zero_one() {
    assert_eq!(normalize(0.0, 0.0, 10.0), 0.0);
    assert_eq!(normalize(10.0, 0.0, 10.0), 1.0);
    assert_eq!(normalize(5.0, 0.0, 10.0), 0.5);
}
