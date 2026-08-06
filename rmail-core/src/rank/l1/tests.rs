//! What this task's acceptance bullets ask to be proven by test, not just
//! asserted in a doc comment (see this crate's `rank` module docs and
//! `l1`'s own docs for the design reasoning these tests pin down):
//!
//! - [`score_is_a_pure_function_of_the_feature_vector`] — the same
//!   [`FeatureVector`], the same score, computed with no database and no
//!   clock (this whole file runs without a `tokio` runtime).
//! - [`newsletter_ranks_lower_under_exploratory_but_not_navigational`] — the
//!   intent gate actually changes which candidate a real [`L1Ranker::rank`]
//!   call puts first, not just which raw number [`Weights::score`] returns.
//! - [`unknown_feature_name_in_override_is_a_clear_error`] and
//!   [`toml_override_changes_which_candidate_ranks_first`] — a
//!   `[search.rank_weights]`-shaped override both reaches the scorer and
//!   changes a ranking, and a key that names no real feature is a clear
//!   [`RankError`], never a silently-ignored one.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use super::*;
use crate::config::{RankWeights, SearchConfig};
use crate::features::{FeatureName, FeatureVector, MatchField};
use crate::query::Intent;
use crate::retrieve::Source;

const EPSILON: f64 = 1e-9;

fn approx_eq(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < EPSILON,
        "expected {expected}, got {actual}"
    );
}

/// Every field zeroed/defaulted — a baseline test callers flip individual
/// fields on top of, the same pattern `features::vector::tests::sample`
/// uses for its own (differently-shaped) fixture.
fn zero_vector() -> FeatureVector {
    FeatureVector {
        bm25_subject: 0.0,
        bm25_body: 0.0,
        bm25_from: 0.0,
        bm25_attach: 0.0,
        exact_phrase_hit: false,
        term_coverage: 0.0,
        proximity_min_span: None,
        best_match_field: MatchField::None,
        fuzzy_score: 0.0,
        cos_max_chunk: 0.0,
        cos_mean_chunk: 0.0,
        rrf_score: 0.0,
        num_sources_hit: 0,
        best_source: Source::Lexical,
        sender_affinity: 0.0,
        user_replied_thread: false,
        prior_opens_from_sender: 0.0,
        thread_activity: 0.0,
        age_days: None,
        recency_decay: 0.0,
        matches_date_intent: false,
        is_unread: false,
        is_flagged: false,
        is_pinned: false,
        ai_priority: 0.0,
        has_tag_match: false,
        folder_prior: 0.0,
        has_attachment_match: false,
        is_thread_root: false,
        thread_size: 0,
        msg_length: 0,
        sender_reputation: 0.0,
        is_newsletter: false,
        is_automated: false,
    }
}

/// A vector with a distinct, nonzero value in every feature the cold-start
/// formula weights — hand-computed against prd.md's formula by
/// [`score_is_a_pure_function_of_the_feature_vector`], so that test proves
/// the formula is *correct*, not merely self-consistent (a wrong formula
/// would still return the same wrong number twice).
fn formula_vector() -> FeatureVector {
    let mut v = zero_vector();
    v.rrf_score = 0.6;
    v.bm25_subject = 2.0;
    v.bm25_body = 1.0;
    v.cos_max_chunk = 0.8;
    v.cos_mean_chunk = 0.5;
    v.exact_phrase_hit = true;
    v.term_coverage = 0.75;
    v.sender_affinity = 0.4;
    v.user_replied_thread = true;
    v.recency_decay = 0.9;
    v.ai_priority = 0.2;
    v.is_flagged = true;
    v.is_unread = true;
    v.has_tag_match = true;
    v.has_attachment_match = true;
    v
}

fn candidate(message_id: i64, features: FeatureVector) -> CandidateFeatures {
    CandidateFeatures {
        message_id,
        features,
    }
}

// ---------------------------------------------------------------------------
// Cold-start table
// ---------------------------------------------------------------------------

/// Every weight prd.md's Stage 4 formula names, and every other
/// [`FeatureName`] left unweighted (`0.0`) — a table-driven check against
/// prd.md's text, not just "some weights exist."
#[test]
fn cold_start_matches_every_documented_prd_weight() {
    let weights = Weights::cold_start();
    let documented = [
        (FeatureName::RrfScore, 1.00),
        (FeatureName::Bm25Subject, 0.90),
        (FeatureName::Bm25Body, 0.35),
        (FeatureName::CosMaxChunk, 0.80),
        (FeatureName::CosMeanChunk, 0.30),
        (FeatureName::ExactPhraseHit, 0.60),
        (FeatureName::TermCoverage, 0.40),
        (FeatureName::SenderAffinity, 0.50),
        (FeatureName::UserRepliedThread, 0.30),
        (FeatureName::RecencyDecay, 0.45),
        (FeatureName::AiPriority, 0.25),
        (FeatureName::IsFlagged, 0.20),
        (FeatureName::IsUnread, 0.15),
        (FeatureName::HasTagMatch, 0.15),
        (FeatureName::HasAttachmentMatch, 0.20),
        (FeatureName::IsNewsletter, -0.40),
        (FeatureName::IsAutomated, -0.25),
    ];
    for (name, weight) in documented {
        approx_eq(weights.get(name), weight);
    }

    let weighted: std::collections::BTreeSet<&str> =
        documented.iter().map(|(n, _)| n.as_str()).collect();
    for name in FeatureName::ALL {
        if !weighted.contains(name.as_str()) {
            approx_eq(weights.get(name), 0.0);
        }
    }
}

#[test]
fn l1ranker_default_uses_cold_start_weights() {
    let ranker = L1Ranker::default();
    approx_eq(ranker.weights().get(FeatureName::RrfScore), 1.00);
    approx_eq(ranker.weights().get(FeatureName::IsNewsletter), -0.40);
    approx_eq(ranker.weights().get(FeatureName::Bm25From), 0.0);
}

#[test]
fn default_top_k_matches_the_search_config_default() {
    // Two independent places name "the PRD default top-K" — this module's
    // own constant and `SearchConfig::top_k_rerank`'s default — and they
    // must never silently drift apart.
    let cfg = SearchConfig::default();
    assert_eq!(DEFAULT_TOP_K, cfg.top_k_rerank as usize);
}

// ---------------------------------------------------------------------------
// Pure function of the feature vector
// ---------------------------------------------------------------------------

#[test]
fn score_is_a_pure_function_of_the_feature_vector() {
    let weights = Weights::cold_start();
    let v = formula_vector();

    // Hand-computed against prd.md's formula (see `formula_vector`'s doc
    // comment): 1.00*0.6 + 0.90*2.0 + 0.35*1.0 + 0.80*0.8 + 0.30*0.5 +
    // 0.60*1.0 + 0.40*0.75 + 0.50*0.4 + 0.30*1.0 + 0.45*0.9 + 0.25*0.2 +
    // 0.20*1.0 + 0.15*1.0 + 0.15*1.0 + 0.20*1.0 = 6.095.
    let first = weights.score(&v, Intent::Navigational);
    approx_eq(first, 6.095);

    // Calling it again with the identical inputs — no database, no clock,
    // nothing mutated in between — must return the identical value.
    let second = weights.score(&v, Intent::Navigational);
    assert_eq!(
        first, second,
        "identical inputs must yield identical output"
    );

    // A third call through `L1Ranker::score` (the public wrapper a caller
    // actually uses) must agree too.
    let ranker = L1Ranker::new(weights);
    assert_eq!(ranker.score(&v, Intent::Navigational), first);
}

/// Same proof at batch granularity: two `rank()` calls over the identical
/// candidates, intent, and top-K produce identical output — nothing in
/// `L1Ranker::rank`'s sort/truncate reads external state either.
#[test]
fn rank_is_a_pure_function_of_its_arguments() {
    let ranker = L1Ranker::default();
    let candidates = vec![candidate(1, formula_vector()), candidate(2, zero_vector())];
    let first = ranker.rank(&candidates, Intent::Exploratory, 10);
    let second = ranker.rank(&candidates, Intent::Exploratory, 10);
    assert_eq!(first, second);
}

/// A stronger purity probe than back-to-back identical calls above: feeding
/// the *same* candidates in a different input order must still produce the
/// identical sorted output. This is what actually pins
/// `Weights`'s module docs' claim that `HashMap<FeatureName, f64>`'s own
/// (unspecified) iteration order is never observed — `score` never walks
/// the map, only looks values up by name, so shuffling the candidates (which
/// changes nothing about the map, only the order `rank` visits them in)
/// cannot change a single score, only the transient order they are computed
/// in before the sort re-imposes the canonical one.
#[test]
fn rank_does_not_depend_on_input_candidate_order() {
    let ranker = L1Ranker::default();
    let forward = vec![
        candidate(1, formula_vector()),
        candidate(2, zero_vector()),
        candidate(3, {
            let mut v = zero_vector();
            v.rrf_score = 0.3;
            v
        }),
    ];
    let mut reversed = forward.clone();
    reversed.reverse();

    let from_forward = ranker.rank(&forward, Intent::Exploratory, 10);
    let from_reversed = ranker.rank(&reversed, Intent::Exploratory, 10);
    assert_eq!(from_forward, from_reversed);
}

// ---------------------------------------------------------------------------
// `Ranker` is genuinely hot-swappable (task 65's seam)
// ---------------------------------------------------------------------------

/// A second, trivial [`Ranker`] implementation — not something task 65 would
/// ship, just proof that [`Ranker`] is actually `dyn`-compatible and that a
/// caller can hold either implementation behind the identical
/// `Box<dyn Ranker>` and call through it uniformly. Without this, the
/// trait's object safety is accidental: a future change (a generic method,
/// an `-> Self` return) would silently break the hot-swap story tasks.md
/// asks for while every other test here still passes, since `L1Ranker` alone
/// never has to be used as a trait object.
#[derive(Debug)]
struct ConstantRanker {
    score: f64,
}

impl Ranker for ConstantRanker {
    fn rank(
        &self,
        candidates: &[CandidateFeatures],
        _intent: Intent,
        top_k: usize,
    ) -> Vec<RankedCandidate> {
        let mut scored: Vec<RankedCandidate> = candidates
            .iter()
            .map(|c| RankedCandidate {
                message_id: c.message_id,
                score: self.score,
            })
            .collect();
        scored.sort_by_key(|r| r.message_id);
        scored.truncate(top_k);
        scored
    }
}

#[test]
fn ranker_trait_objects_hot_swap() {
    let candidates = vec![candidate(1, formula_vector()), candidate(2, zero_vector())];

    // Two different `Ranker`s, one `Vec<Box<dyn Ranker>>` — the shape task
    // 65's learned model needs to slot in beside (or in place of)
    // `L1Ranker` at whatever call site builds the live ranker.
    let rankers: Vec<Box<dyn Ranker>> = vec![
        Box::new(L1Ranker::default()),
        Box::new(ConstantRanker { score: 42.0 }),
    ];

    let l1_result = rankers[0].rank(&candidates, Intent::Navigational, 10);
    let constant_result = rankers[1].rank(&candidates, Intent::Navigational, 10);

    // `L1Ranker` differentiates the two candidates (formula_vector scores
    // higher than zero_vector); `ConstantRanker` does not — proof both
    // implementations actually ran through the identical `&dyn Ranker`
    // call, not that they happen to agree.
    assert_eq!(l1_result[0].message_id, 1);
    assert!(l1_result[0].score > l1_result[1].score);
    assert!((constant_result[0].score - constant_result[1].score).abs() < EPSILON);
}

// ---------------------------------------------------------------------------
// Intent gating
// ---------------------------------------------------------------------------

#[test]
fn navigational_intent_suppresses_the_bulk_downweight() {
    let weights = Weights::cold_start();

    let mut newsletter = zero_vector();
    newsletter.rrf_score = 1.0;
    newsletter.is_newsletter = true;
    approx_eq(weights.score(&newsletter, Intent::Navigational), 1.0);
    approx_eq(weights.score(&newsletter, Intent::Exploratory), 1.0 - 0.40);
    approx_eq(weights.score(&newsletter, Intent::Lookup), 1.0 - 0.40);

    let mut automated = zero_vector();
    automated.rrf_score = 1.0;
    automated.is_automated = true;
    approx_eq(weights.score(&automated, Intent::Navigational), 1.0);
    approx_eq(weights.score(&automated, Intent::Exploratory), 1.0 - 0.25);
}

/// prd.md's own Lookup examples ("tracking number for my order", "AWS
/// bill") are, by construction, automated mail — see
/// `bulk_downweight_suppressed`'s doc comment. `is_automated` is suppressed
/// under Lookup for that reason; `is_newsletter` is not, since a
/// promotional newsletter is not the answer a Lookup query is asking for.
#[test]
fn lookup_intent_suppresses_only_the_automated_downweight() {
    let weights = Weights::cold_start();

    let mut automated = zero_vector();
    automated.rrf_score = 1.0;
    automated.is_automated = true;
    approx_eq(weights.score(&automated, Intent::Lookup), 1.0);

    let mut newsletter = zero_vector();
    newsletter.rrf_score = 1.0;
    newsletter.is_newsletter = true;
    approx_eq(weights.score(&newsletter, Intent::Lookup), 1.0 - 0.40);
}

/// The behavioral proof: not just that the scalar score differs, but that a
/// real `rank()` call puts a different candidate first depending on intent —
/// this task's acceptance bullet, verbatim.
#[test]
fn newsletter_ranks_lower_under_exploratory_but_not_navigational() {
    let ranker = L1Ranker::default();

    let mut newsletter = zero_vector();
    newsletter.rrf_score = 0.5;
    newsletter.is_newsletter = true;

    let mut plain = zero_vector();
    plain.rrf_score = 0.45;

    let candidates = vec![candidate(1, newsletter), candidate(2, plain)];

    // Exploratory ("everything about the office move" — no named target):
    // the newsletter's -0.40 penalty applies (0.5 - 0.40 = 0.10) and drops
    // it behind the plain candidate (0.45).
    let exploratory = ranker.rank(&candidates, Intent::Exploratory, 10);
    assert_eq!(
        exploratory[0].message_id, 2,
        "the plain candidate should rank first under exploratory intent"
    );
    assert_eq!(exploratory[1].message_id, 1);

    // Navigational ("the invoice Acme sent last week" — a named target): the
    // down-weight is suppressed, so the newsletter's higher raw rrf_score
    // (0.5 vs 0.45) decides and it ranks first.
    let navigational = ranker.rank(&candidates, Intent::Navigational, 10);
    assert_eq!(
        navigational[0].message_id, 1,
        "the newsletter should not be down-weighted once the query names a known item"
    );
    assert_eq!(navigational[1].message_id, 2);
}

// ---------------------------------------------------------------------------
// TOML weight overrides
// ---------------------------------------------------------------------------

#[test]
fn weights_from_config_applies_sparse_overrides_and_leaves_the_rest() {
    let mut raw = BTreeMap::new();
    raw.insert("bm25_subject".to_owned(), 5.0);
    let cfg = RankWeights(raw);

    let weights = Weights::from_config(&cfg).expect("a real feature name must parse");
    approx_eq(weights.get(FeatureName::Bm25Subject), 5.0);
    // Every key `cfg` did not mention keeps its cold-start value.
    approx_eq(weights.get(FeatureName::RrfScore), 1.00);
    approx_eq(weights.get(FeatureName::IsNewsletter), -0.40);
    approx_eq(weights.get(FeatureName::Bm25Body), 0.35);
}

/// The end-to-end proof: the override does not just change what
/// `Weights::get` reports, it changes which candidate a real `rank()` call
/// puts first.
#[test]
fn toml_override_changes_which_candidate_ranks_first() {
    let mut a = zero_vector();
    a.rrf_score = 0.6;
    let mut b = zero_vector();
    b.rrf_score = 0.55;
    b.bm25_subject = 0.05;

    let candidates = vec![candidate(1, a), candidate(2, b)];

    // Cold-start: A's plain rrf_score (0.6) beats B's rrf_score + a modest
    // bm25_subject contribution (0.55 + 0.05*0.90 = 0.595).
    let default_order = L1Ranker::default().rank(&candidates, Intent::Navigational, 10);
    assert_eq!(default_order[0].message_id, 1);

    // Overriding bm25_subject's weight to 5.0 pushes B's score to
    // 0.55 + 0.05*5.0 = 0.80, past A's 0.6 — the ranking flips.
    let mut raw = BTreeMap::new();
    raw.insert("bm25_subject".to_owned(), 5.0);
    let weights = Weights::from_config(&RankWeights(raw)).expect("valid override");
    let overridden_order = L1Ranker::new(weights).rank(&candidates, Intent::Navigational, 10);
    assert_eq!(
        overridden_order[0].message_id, 2,
        "the TOML override should flip the ranking"
    );
}

#[test]
fn unknown_feature_name_in_override_is_a_clear_error() {
    let mut raw = BTreeMap::new();
    raw.insert("not_a_real_feature".to_owned(), 1.0);
    let cfg = RankWeights(raw);

    let err = Weights::from_config(&cfg).expect_err("an unrecognized key must be rejected");
    assert_eq!(
        err,
        RankError::UnknownFeature("not_a_real_feature".to_owned())
    );
    assert!(
        err.to_string().contains("not_a_real_feature"),
        "the error message should name the offending key: {err}"
    );
}

#[test]
fn empty_override_table_is_the_unmodified_cold_start_table() {
    let weights = Weights::from_config(&RankWeights(BTreeMap::new())).expect("empty is valid");
    assert_eq!(weights, Weights::cold_start());
}

/// TOML's float grammar accepts `nan`/`inf`/`-inf` literals — a real,
/// reachable input, not just a defensive-only case (see `RankError`'s doc
/// comment). A `NaN` weight would otherwise poison every candidate's score
/// and silently collapse `rank()`'s ordering to `message_id` order with no
/// error anywhere on the path.
#[test]
fn non_finite_override_value_is_a_clear_error() {
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let mut raw = BTreeMap::new();
        raw.insert("bm25_subject".to_owned(), bad);
        let err = Weights::from_config(&RankWeights(raw))
            .expect_err("a non-finite override value must be rejected");
        let is_expected = matches!(
            &err,
            RankError::NonFiniteWeight { name, .. } if name.as_str() == "bm25_subject"
        );
        assert!(
            is_expected,
            "expected NonFiniteWeight naming bm25_subject, got {err:?}"
        );
    }
}

/// The full chain the acceptance bullet actually describes: a TOML string,
/// parsed through [`crate::config::Config`] exactly as `rmail.toml` would
/// be, validated into [`Weights`], and run through a real `rank()` call —
/// not just the two halves (`config` parses the table;
/// `Weights::from_config` accepts a hand-built one) proven separately above.
#[test]
fn toml_config_string_reaches_the_scorer_end_to_end() {
    let toml = "[search.rank_weights]\nbm25_subject = 5.0\n";
    let cfg = crate::config::Config::from_toml_str(toml).expect("valid config");
    let weights =
        Weights::from_config(&cfg.search.rank_weights).expect("a real feature name must parse");
    let ranker = L1Ranker::new(weights);

    let mut a = zero_vector();
    a.rrf_score = 0.6;
    let mut b = zero_vector();
    b.rrf_score = 0.55;
    b.bm25_subject = 0.05;
    let candidates = vec![candidate(1, a), candidate(2, b)];

    // Same arithmetic as `toml_override_changes_which_candidate_ranks_first`
    // (0.55 + 0.05*5.0 = 0.80 beats 0.6) — this time reached by parsing a
    // TOML string through the real `Config` type, not a hand-built
    // `RankWeights`.
    let ranked = ranker.rank(&candidates, Intent::Navigational, 10);
    assert_eq!(ranked[0].message_id, 2);
}

// ---------------------------------------------------------------------------
// `RankError` bridges into the crate's domain error type
// ---------------------------------------------------------------------------

#[test]
fn rank_error_maps_to_invalid_argument() {
    let err: crate::error::Error = RankError::UnknownFeature("bogus".to_owned()).into();
    assert_eq!(err.reason(), crate::error::ErrorReason::InvalidArgument);
    assert!(err.to_string().contains("bogus"));
}

// ---------------------------------------------------------------------------
// Top-K cut and ordering
// ---------------------------------------------------------------------------

#[test]
fn rank_keeps_only_the_best_top_k_scores_best_first() {
    let ranker = L1Ranker::default();
    let candidates: Vec<CandidateFeatures> = (0..10)
        .map(|i| {
            let mut v = zero_vector();
            v.rrf_score = f64::from(i);
            candidate(i64::from(i), v)
        })
        .collect();

    let ranked = ranker.rank(&candidates, Intent::Navigational, 3);
    let ids: Vec<i64> = ranked.iter().map(|r| r.message_id).collect();
    assert_eq!(ids, vec![9, 8, 7]);
}

#[test]
fn rank_returns_every_candidate_when_top_k_exceeds_the_batch() {
    let ranker = L1Ranker::default();
    let candidates = vec![candidate(1, zero_vector()), candidate(2, zero_vector())];
    let ranked = ranker.rank(&candidates, Intent::Navigational, 50);
    assert_eq!(ranked.len(), 2);
}

#[test]
fn rank_breaks_ties_by_message_id_ascending() {
    let ranker = L1Ranker::default();
    let candidates = vec![
        candidate(5, zero_vector()),
        candidate(2, zero_vector()),
        candidate(9, zero_vector()),
    ];
    let ranked = ranker.rank(&candidates, Intent::Navigational, 10);
    let ids: Vec<i64> = ranked.iter().map(|r| r.message_id).collect();
    assert_eq!(ids, vec![2, 5, 9]);
}

#[test]
fn rank_of_an_empty_batch_is_empty() {
    let ranker = L1Ranker::default();
    assert!(ranker.rank(&[], Intent::Navigational, 50).is_empty());
}

// ---------------------------------------------------------------------------
// Performance: pure Rust, no I/O
// ---------------------------------------------------------------------------

/// prd.md: "Inference is microseconds/candidate — pure Rust, no FFI on the
/// hot path." A generous, safety-factored tripwire rather than a strict
/// microsecond budget (which would be flaky on a shared/loaded CI
/// container — see `benches/search_budgets.rs`'s identical reasoning for its
/// own budget check): still enough to catch a real regression (an
/// accidental quadratic pass, a stray per-candidate allocation) while
/// running with no clock, no database, and no `tokio` runtime at all — this
/// whole test is synchronous, itself part of the "pure Rust, no I/O" proof.
#[test]
fn scoring_a_large_batch_is_fast_and_synchronous() {
    let ranker = L1Ranker::default();
    let candidates: Vec<CandidateFeatures> = (0..2_000)
        .map(|i| {
            let mut v = formula_vector();
            v.rrf_score = f64::from(i % 100) / 100.0;
            candidate(i64::from(i), v)
        })
        .collect();

    let started = Instant::now();
    let ranked = ranker.rank(&candidates, Intent::Exploratory, DEFAULT_TOP_K);
    let elapsed = started.elapsed();

    assert_eq!(ranked.len(), DEFAULT_TOP_K);
    assert!(
        elapsed < Duration::from_millis(200),
        "scoring 2,000 candidates took {elapsed:?}, expected well under 200ms"
    );
}
