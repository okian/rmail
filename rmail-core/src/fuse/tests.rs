//! What this module owes task 30: RRF arithmetic that matches prd.md's
//! formula exactly (checked against hand-computed values, not just
//! orderings — a subtly wrong formula still produces a plausible-looking
//! ranking), intent actually changing which candidate wins, `k=60` damping
//! that is real but not the naive `1/rank` a missing `+k` would produce,
//! thread collapse, and SimHash near-dup collapse that favors the
//! false-negative direction over the false-positive one.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use tokio_util::sync::CancellationToken;

use super::*;
use crate::config::{FusionSourceWeights, SearchConfig};
use crate::embed::Embedding;
use crate::query::{Intent, Mode, Phrase, PlanTerm, QueryPlan, Scope, SortSpec, TermOrigin};
use crate::repo;

const EPSILON: f64 = 1e-9;

fn approx_eq(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < EPSILON,
        "expected {expected}, got {actual}"
    );
}

fn cand(source: Source, message_id: i64, score: f64, rank: u32) -> Candidate {
    Candidate {
        message_id,
        source,
        score,
        rank,
        mean_score: None,
    }
}

fn cand_with_mean(source: Source, message_id: i64, score: f64, rank: u32, mean: f64) -> Candidate {
    Candidate {
        message_id,
        source,
        score,
        rank,
        mean_score: Some(mean),
    }
}

fn plan_with_intent(intent: Intent) -> QueryPlan {
    QueryPlan {
        raw: String::new(),
        hard_filters: Vec::new(),
        lexical_terms: Vec::new(),
        phrases: Vec::new(),
        expansions: Vec::new(),
        query_vector: None,
        entities: Vec::new(),
        intent,
        sort: SortSpec::default(),
        scope: Scope::default(),
        needs_nl_compile: false,
    }
}

/// A minimal, already-fused-looking candidate for the collapse tests, which
/// only care about `message_id`/`fused_score`/the collapse fields.
fn fc(message_id: i64, fused_score: f64) -> FusedCandidate {
    FusedCandidate {
        message_id,
        fused_score,
        hits: Vec::new(),
        num_sources_hit: 0,
        best_source: Source::Lexical,
        thread_id: None,
        thread_collapsed: Vec::new(),
        near_duplicates: Vec::new(),
    }
}

/// As [`fc`], with `hits` populated from `sources` (one [`SourceHit`] per
/// entry, arbitrary but distinct rank/score — [`drop_prior_only_candidates`]
/// only reads `hit.source`) — what its own tests need that the collapse
/// tests' bare `fc` does not.
fn fc_from(message_id: i64, fused_score: f64, sources: &[Source]) -> FusedCandidate {
    let mut candidate = fc(message_id, fused_score);
    candidate.hits = sources
        .iter()
        .enumerate()
        .map(|(i, &source)| SourceHit {
            source,
            rank: u32::try_from(i + 1).unwrap_or(u32::MAX),
            score: 1.0,
            mean_score: None,
        })
        .collect();
    candidate
}

fn meta(thread_id: Option<i64>, date: Option<i64>, body: Option<&str>) -> MessageMeta {
    MessageMeta {
        thread_id,
        date,
        body: body.map(str::to_owned),
    }
}

/// Uniform weights (every source at `1.0`) for tests that want ties or want
/// the formula isolated from prd.md's tuned defaults.
fn uniform_weights() -> FusionWeights {
    FusionWeights {
        navigational: FusionSourceWeights::default(),
        exploratory: FusionSourceWeights::default(),
        lookup: FusionSourceWeights::default(),
    }
}

// ---------------------------------------------------------------------------
// fuse_scores: hand-computed RRF arithmetic
// ---------------------------------------------------------------------------

#[test]
fn single_source_rrf_matches_the_hand_computed_formula() {
    let candidates = [cand(Source::Lexical, 1, 10.0, 1)];
    let out = fuse_scores(
        &candidates,
        Intent::Navigational,
        Fusion::Rrf,
        60,
        &FusionWeights::default(),
    );
    assert_eq!(out.len(), 1);
    // w_lexical(navigational) = 1.0, rank = 1, k = 60 => 1.0 / 61.
    approx_eq(out[0].fused_score, 1.0 / 61.0);
    assert_eq!(out[0].num_sources_hit, 1);
    assert_eq!(out[0].best_source, Source::Lexical);
    assert_eq!(
        out[0].hits,
        vec![SourceHit {
            source: Source::Lexical,
            rank: 1,
            score: 10.0,
            mean_score: None,
        }]
    );
}

#[test]
fn a_document_found_by_several_sources_outranks_one_found_by_a_single_strong_source() {
    // doc_a: only lexical, at its very best rank (1).
    // doc_b: lexical rank 3 *and* dense rank 2 — weaker in either source
    // alone, but RRF sums across sources, so agreement should win.
    let candidates = [
        cand(Source::Lexical, 1, 5.0, 1),
        cand(Source::Lexical, 2, 3.0, 3),
        cand_with_mean(Source::Dense, 2, 0.8, 2, 0.7),
    ];
    let weights = FusionWeights::default(); // navigational: lexical 1.0, dense 0.6
    let out = fuse_scores(&candidates, Intent::Navigational, Fusion::Rrf, 60, &weights);

    let doc_a_score = 1.0 / 61.0; // 1.0 / (60 + 1)
    let doc_b_score = 1.0 / 63.0 + 0.6 / 62.0; // 1.0/(60+3) + 0.6/(60+2)
                                               // Hand-computed decimal check: 0.015873015873... + 0.009677419354...
                                               // = 0.025550435227...
    assert!(
        doc_b_score > doc_a_score,
        "sanity: the multi-source sum should exceed the single-source term"
    );

    assert_eq!(out[0].message_id, 2, "multi-source doc ranks first");
    approx_eq(out[0].fused_score, doc_b_score);
    assert_eq!(out[0].num_sources_hit, 2);

    assert_eq!(out[1].message_id, 1);
    approx_eq(out[1].fused_score, doc_a_score);
    assert_eq!(out[1].num_sources_hit, 1);
}

#[test]
fn intent_weights_change_which_candidate_wins() {
    // doc_x: lexical rank 1 only. doc_y: dense rank 1 only. Same rank, same
    // k, so which one leads depends purely on the intent's lexical-vs-dense
    // weight — navigational favors lexical (1.0 vs 0.6), exploratory favors
    // dense (1.0 vs 0.7).
    let candidates = [
        cand(Source::Lexical, 1, 1.0, 1),
        cand_with_mean(Source::Dense, 2, 1.0, 1, 1.0),
    ];
    let weights = FusionWeights::default();

    let navigational = fuse_scores(&candidates, Intent::Navigational, Fusion::Rrf, 60, &weights);
    assert_eq!(navigational[0].message_id, 1, "navigational favors lexical");
    approx_eq(navigational[0].fused_score, 1.0 / 61.0);
    approx_eq(navigational[1].fused_score, 0.6 / 61.0);

    let exploratory = fuse_scores(&candidates, Intent::Exploratory, Fusion::Rrf, 60, &weights);
    assert_eq!(exploratory[0].message_id, 2, "exploratory favors dense");
    approx_eq(exploratory[0].fused_score, 1.0 / 61.0);
    approx_eq(exploratory[1].fused_score, 0.7 / 61.0);
}

#[test]
fn k60_damping_at_rank1_vs_rank50() {
    // Same source/weight, different rank: k=60 damps the raw 1/rank curve
    // (which would put rank 50 at 50x worse than rank 1) down to a much
    // gentler ratio, but rank 1 still clearly beats rank 50.
    let candidates = [
        cand(Source::Lexical, 1, 1.0, 1),
        cand(Source::Lexical, 2, 1.0, 50),
    ];
    let out = fuse_scores(
        &candidates,
        Intent::Navigational,
        Fusion::Rrf,
        60,
        &FusionWeights::default(),
    );
    let rank1_score = 1.0 / 61.0; // 60 + 1
    let rank50_score = 1.0 / 110.0; // 60 + 50
    approx_eq(out[0].fused_score, rank1_score);
    approx_eq(out[1].fused_score, rank50_score);
    assert_eq!(out[0].message_id, 1);
    assert_eq!(out[1].message_id, 2);
    // Damped, not annihilated: rank 50 keeps roughly 55% of rank 1's term
    // (110/61 ≈ 1.80x apart), a world away from the 50x a plain 1/rank
    // (no +k) would produce.
    let ratio = rank1_score / rank50_score;
    assert!(
        (1.7..1.9).contains(&ratio),
        "expected k=60 to keep the ratio well under 50x, got {ratio}"
    );
}

#[test]
fn best_source_tie_break_is_deterministic_not_hashmap_order() {
    // Uniform weights + identical rank => identical weighted term from both
    // sources. Insertion order is Dense-then-Lexical, deliberately the
    // reverse of the tie-break order, so this would fail if `best_source`
    // picked "whichever arrived last" instead of the fixed source ordinal.
    let candidates = [
        cand_with_mean(Source::Dense, 1, 0.5, 5, 0.4),
        cand(Source::Lexical, 1, 9.0, 5),
    ];
    let out = fuse_scores(
        &candidates,
        Intent::Navigational,
        Fusion::Rrf,
        60,
        &uniform_weights(),
    );
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].best_source, Source::Lexical);
}

#[test]
fn hits_carries_every_sources_rank_score_and_mean_for_task_30() {
    let candidates = [
        cand(Source::Entity, 7, 2.2, 4),
        cand(Source::Lexical, 7, 7.0, 2),
        cand_with_mean(Source::Dense, 7, 0.9, 1, 0.5),
    ];
    let out = fuse_scores(
        &candidates,
        Intent::Navigational,
        Fusion::Rrf,
        60,
        &FusionWeights::default(),
    );
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].num_sources_hit, 3);
    // Sorted by a fixed source order, independent of input/insertion order.
    assert_eq!(
        out[0].hits,
        vec![
            SourceHit {
                source: Source::Lexical,
                rank: 2,
                score: 7.0,
                mean_score: None,
            },
            SourceHit {
                source: Source::Dense,
                rank: 1,
                score: 0.9,
                mean_score: Some(0.5),
            },
            SourceHit {
                source: Source::Entity,
                rank: 4,
                score: 2.2,
                mean_score: None,
            },
        ]
    );
}

#[test]
fn duplicate_rows_from_the_same_source_do_not_double_count() {
    // Defensive de-dup: a source should never return two rows for the same
    // message, but if one did, the better (lower) rank wins and the vote is
    // not counted twice.
    let candidates = [
        cand(Source::Lexical, 1, 1.0, 5),
        cand(Source::Lexical, 1, 1.0, 2),
    ];
    let out = fuse_scores(
        &candidates,
        Intent::Navigational,
        Fusion::Rrf,
        60,
        &FusionWeights::default(),
    );
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].num_sources_hit, 1);
    approx_eq(out[0].fused_score, 1.0 / 62.0); // rank 2, not rank 5
}

// ---------------------------------------------------------------------------
// fuse_scores: Structured/Prefix's fixed weights
// ---------------------------------------------------------------------------

#[test]
fn structured_and_prefix_use_their_own_fixed_weights() {
    let candidates = [
        cand(Source::Structured, 1, 1.0, 1),
        cand(Source::Prefix, 2, 1.0, 1),
    ];
    let out = fuse_scores(
        &candidates,
        Intent::Navigational,
        Fusion::Rrf,
        60,
        &FusionWeights::default(),
    );
    let structured = out
        .iter()
        .find(|c| c.message_id == 1)
        .expect("structured hit");
    let prefix = out.iter().find(|c| c.message_id == 2).expect("prefix hit");
    approx_eq(structured.fused_score, STRUCTURED_SOURCE_WEIGHT / 61.0);
    approx_eq(prefix.fused_score, PREFIX_SOURCE_WEIGHT / 61.0);
}

#[test]
fn structured_plus_recency_no_longer_overrides_a_single_strong_lexical_hit() {
    // Regression guard for the P1 this module used to have: `Structured`
    // and `Recency` gate on the identical hard-filter mask and both sort by
    // the identical recency order, so on any filtered query they return the
    // same ids at the same ranks. At the old neutral weight of `1.0`,
    // `structured + recency` (1.0 + 0.3 = 1.3, exploratory) beat a single
    // best-ranked lexical hit (0.7) outright -- a recent-but-irrelevant
    // message would outrank the query's actual best match. At the fixed,
    // low weight this asserts, it must not.
    let recent_irrelevant = [
        cand(Source::Structured, 1, 1.0, 1),
        cand(Source::Recency, 1, 1.0, 1),
    ];
    let best_lexical_hit = [cand(Source::Lexical, 2, 1.0, 1)];
    let all: Vec<Candidate> = recent_irrelevant
        .into_iter()
        .chain(best_lexical_hit)
        .collect();
    let out = fuse_scores(
        &all,
        Intent::Exploratory,
        Fusion::Rrf,
        60,
        &FusionWeights::default(),
    );
    assert_eq!(
        out[0].message_id, 2,
        "the single strong lexical match must still win"
    );
}

#[test]
fn prefix_stacked_on_lexical_no_longer_overrides_a_strong_dense_hit() {
    // The same shape of regression as `Structured`/`Recency` above:
    // `retrieve::prefix` builds its `"term"*` query from the same original
    // free-text terms `retrieve::lexical` ANDs together, so for a matching
    // term it returns a near-superset of lexical's hits at essentially the
    // same rank. A document found by both must not out-weigh a document
    // found only by dense in exploratory intent, where dense (1.0) is
    // supposed to dominate lexical (0.7) -- at the old neutral `1.0` weight,
    // `lexical + prefix` (0.7 + 1.0 = 1.7) beat dense outright.
    let lexical_and_prefix = [
        cand(Source::Lexical, 1, 1.0, 1),
        cand(Source::Prefix, 1, 1.0, 1),
    ];
    let dense_only = [cand_with_mean(Source::Dense, 2, 1.0, 1, 1.0)];
    let all: Vec<Candidate> = lexical_and_prefix.into_iter().chain(dense_only).collect();
    let out = fuse_scores(
        &all,
        Intent::Exploratory,
        Fusion::Rrf,
        60,
        &FusionWeights::default(),
    );
    assert_eq!(
        out[0].message_id, 2,
        "dense recall must still win over a stacked lexical+prefix hit"
    );
}

// ---------------------------------------------------------------------------
// drop_prior_only_candidates: recency/structured alone is not a match
// ---------------------------------------------------------------------------

#[test]
fn has_free_text_intent_is_false_for_an_empty_plan() {
    let plan = plan_with_intent(Intent::Navigational);
    assert!(!has_free_text_intent(&plan));
}

#[test]
fn has_free_text_intent_is_true_for_lexical_terms() {
    let mut plan = plan_with_intent(Intent::Navigational);
    plan.lexical_terms = vec![PlanTerm {
        text: "budgetary".to_owned(),
        negated: false,
        mode: Mode::Auto,
        weight: 1.0,
        origin: TermOrigin::Original,
    }];
    assert!(has_free_text_intent(&plan));
}

#[test]
fn has_free_text_intent_is_true_for_phrases() {
    let mut plan = plan_with_intent(Intent::Navigational);
    plan.phrases = vec![Phrase {
        text: "office move".to_owned(),
        negated: false,
        mode: Mode::Auto,
    }];
    assert!(has_free_text_intent(&plan));
}

#[test]
fn has_free_text_intent_is_true_for_a_query_vector() {
    let mut plan = plan_with_intent(Intent::Navigational);
    plan.query_vector = Some(Embedding::new(vec![1.0; 8]));
    assert!(has_free_text_intent(&plan));
}

#[test]
fn a_filters_only_query_keeps_recency_and_structured_only_candidates() {
    // `is:flagged`/`from:alice` with no free text: recency/structured-only
    // results *are* the intended answer (nothing else could have matched
    // free text that was never typed), so the drop must be a no-op.
    let plan = plan_with_intent(Intent::Navigational);
    let fused = vec![
        fc_from(1, 1.0, &[Source::Recency]),
        fc_from(2, 0.9, &[Source::Structured]),
        fc_from(3, 0.8, &[Source::Structured, Source::Recency]),
    ];
    let out = drop_prior_only_candidates(fused, &plan);
    assert_eq!(out.len(), 3, "no free text means nothing to have missed");
}

#[test]
fn free_text_intent_drops_recency_only_and_structured_only_candidates() {
    let mut plan = plan_with_intent(Intent::Navigational);
    plan.lexical_terms = vec![PlanTerm {
        text: "budgetary".to_owned(),
        negated: false,
        mode: Mode::Auto,
        weight: 1.0,
        origin: TermOrigin::Original,
    }];
    let fused = vec![
        fc_from(1, 1.0, &[Source::Recency]),
        fc_from(2, 0.9, &[Source::Structured]),
        fc_from(3, 0.8, &[Source::Structured, Source::Recency]),
        fc_from(4, 0.5, &[Source::Lexical]),
        fc_from(5, 0.4, &[Source::Lexical, Source::Recency]),
        fc_from(6, 0.3, &[Source::Dense]),
        fc_from(7, 0.2, &[Source::Fuzzy]),
        fc_from(8, 0.1, &[Source::Entity]),
    ];
    let out = drop_prior_only_candidates(fused, &plan);
    let ids: Vec<i64> = out.iter().map(|c| c.message_id).collect();
    assert_eq!(
        ids,
        vec![4, 5, 6, 7, 8],
        "only candidates with at least one free-text-matching source survive"
    );
}

// ---------------------------------------------------------------------------
// fuse_scores: linear blend (`fusion = "linear"`)
// ---------------------------------------------------------------------------

#[test]
fn linear_fusion_hand_computed_minmax() {
    let candidates = [
        cand(Source::Lexical, 1, 10.0, 1),
        cand(Source::Lexical, 2, 5.0, 2),
        cand(Source::Lexical, 3, 0.0, 3),
    ];
    let out = fuse_scores(
        &candidates,
        Intent::Navigational,
        Fusion::Linear,
        60,
        &FusionWeights::default(), // navigational lexical weight = 1.0
    );
    assert_eq!(out.len(), 3);
    assert_eq!(out[0].message_id, 1);
    approx_eq(out[0].fused_score, 1.0); // (10-0)/(10-0) * 1.0
    assert_eq!(out[1].message_id, 2);
    approx_eq(out[1].fused_score, 0.5); // (5-0)/(10-0) * 1.0
    assert_eq!(out[2].message_id, 3);
    approx_eq(out[2].fused_score, 0.0); // (0-0)/(10-0) * 1.0
}

#[test]
fn linear_fusion_degenerate_range_gives_full_weight_not_a_divide_by_zero() {
    let candidates = [
        cand(Source::Recency, 1, 3.0, 1),
        cand(Source::Recency, 2, 3.0, 2),
    ];
    let out = fuse_scores(
        &candidates,
        Intent::Navigational,
        Fusion::Linear,
        60,
        &FusionWeights::default(), // navigational recency weight = 0.8
    );
    assert_eq!(out.len(), 2);
    approx_eq(out[0].fused_score, 0.8);
    approx_eq(out[1].fused_score, 0.8);
    // Deterministic tie-break: ascending message_id.
    assert_eq!(out[0].message_id, 1);
    assert_eq!(out[1].message_id, 2);
}

// ---------------------------------------------------------------------------
// collapse_threads
// ---------------------------------------------------------------------------

#[test]
fn same_thread_collapses_to_the_higher_scoring_candidate() {
    let fused = vec![fc(10, 9.0), fc(20, 4.0)];
    let meta: BTreeMap<i64, MessageMeta> = BTreeMap::from([
        (10, meta(Some(1), None, None)),
        (20, meta(Some(1), None, None)),
    ]);
    let out = collapse_threads(fused, &meta);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].message_id, 10);
    assert_eq!(out[0].thread_id, Some(1));
    assert_eq!(out[0].thread_collapsed, vec![20]);
}

#[test]
fn a_standalone_candidate_outside_the_thread_is_left_alone() {
    let fused = vec![fc(1, 9.0), fc(2, 7.0), fc(3, 3.0)];
    let meta: BTreeMap<i64, MessageMeta> = BTreeMap::from([
        (1, meta(Some(5), None, None)),
        (2, meta(None, None, None)),
        (3, meta(Some(5), None, None)),
    ]);
    let out = collapse_threads(fused, &meta);
    assert_eq!(out.len(), 2);
    assert_eq!(out[0].message_id, 1);
    assert_eq!(out[0].thread_id, Some(5));
    assert_eq!(out[0].thread_collapsed, vec![3]);
    assert_eq!(out[1].message_id, 2);
    assert_eq!(out[1].thread_id, None);
    assert!(out[1].thread_collapsed.is_empty());
}

#[test]
fn missing_metadata_never_collapses() {
    let fused = vec![fc(1, 9.0), fc(2, 7.0)];
    let out = collapse_threads(fused.clone(), &BTreeMap::new());
    assert_eq!(out, fused);
}

// ---------------------------------------------------------------------------
// collapse_near_duplicates (pure)
// ---------------------------------------------------------------------------

const NEAR_DUP_A: &str = "\
Hi team, quick update on the Q3 roadmap. We are moving the launch date for \
the new billing dashboard to the third week of September so the payments \
squad has time to finish the reconciliation work first. The design review \
happens next Tuesday at 10am, and I would like everyone who touches the \
invoicing flow to attend, including anyone from support who has fielded \
customer questions about the current export format. After the review we \
will finalize the migration plan for existing customers, draft the release \
notes, and schedule a dry run against the staging environment a full week \
before the actual cutover so we have time to fix anything that breaks \
without pressure. Please reply here with any blockers you are already \
aware of so we can bring them up on Tuesday instead of discovering them \
during the dry run itself. On the engineering side, the backend team has \
already merged the new ledger reconciliation service behind a feature \
flag, and the frontend team is finishing the redesigned invoice table with \
the new filtering and export controls customers have been asking for \
since the spring survey. QA has a full regression pass scheduled for the \
following Monday, covering both the legacy CSV export and the new PDF \
export path, and we will need at least two people from support shadowing \
that pass so they can flag anything that looks confusing from a \
customer's perspective rather than just a technical one.";

fn near_dup_b_quoted_reply() -> String {
    format!("Thanks!\n\n> {NEAR_DUP_A}")
}

const DISTINCT_TOPIC: &str = "\
Hey everyone, wanted to flag a change to the mobile release schedule. We \
are pushing the app store submission out to the second week of October \
because the crash reports from the beta channel need another pass before \
we ship. There will be a triage meeting Thursday afternoon for anyone on \
QA who ran the beta build. Once triage wraps we will decide whether to \
cut a new beta or go straight to a release candidate, write up the \
known-issues doc for support, and lock the App Store listing copy. On the \
engineering side, the platform team already isolated two of the three top \
crash signatures to a race condition in the local cache layer, and a fix \
is in review now; the third signature is still unclear and might need a \
repro device from one of the affected beta testers before we can make any \
real progress. Design is finishing a small in-app banner explaining the \
sync delay to beta users so we are not silently missing their feedback \
while the fix lands.";

#[test]
fn a_near_duplicate_pair_collapses_to_the_higher_scoring_candidate() {
    // Not "the newer" -- collapsing to anything but the strongest evidence
    // would silently demote the query's own best result behind a weaker
    // duplicate of it (see `collapse_near_duplicates`'s doc comment). Dates
    // are deliberately omitted here: score alone must decide.
    let fused = vec![fc(1, 5.0), fc(2, 3.0)];
    let reply = near_dup_b_quoted_reply();
    let meta: BTreeMap<i64, MessageMeta> = BTreeMap::from([
        (1, meta(None, None, Some(NEAR_DUP_A))),
        (2, meta(None, None, Some(&reply))),
    ]);
    let out = collapse_near_duplicates(fused, &meta);
    assert_eq!(out.len(), 1, "the pair should collapse to one survivor");
    assert_eq!(
        out[0].message_id, 1,
        "the higher-scoring message is canonical"
    );
    assert_eq!(out[0].near_duplicates, vec![2]);
    // fused_score is untouched by collapsing -- it stays candidate 1's own,
    // already the max in the cluster.
    approx_eq(out[0].fused_score, 5.0);
}

#[test]
fn date_has_no_effect_on_which_candidate_is_canonical() {
    // Regression guard for the bug this function used to have: the
    // higher-scoring candidate (1) is deliberately given no date at all,
    // and the lower-scoring one (2) a date far in the future -- if date
    // ever crept back into the selection rule, this would flip.
    let fused = vec![fc(1, 5.0), fc(2, 3.0)];
    let reply = near_dup_b_quoted_reply();
    let meta: BTreeMap<i64, MessageMeta> = BTreeMap::from([
        (1, meta(None, None, Some(NEAR_DUP_A))),
        (2, meta(None, Some(9_999_999_999), Some(&reply))),
    ]);
    let out = collapse_near_duplicates(fused, &meta);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].message_id, 1, "score wins regardless of date");
    assert_eq!(out[0].near_duplicates, vec![2]);
}

#[test]
fn output_order_stays_fused_score_order() {
    let fused = vec![fc(10, 9.0), fc(1, 5.0), fc(2, 3.0)];
    let reply = near_dup_b_quoted_reply();
    let meta: BTreeMap<i64, MessageMeta> = BTreeMap::from([
        (10, meta(None, None, Some(DISTINCT_TOPIC))),
        (1, meta(None, None, Some(NEAR_DUP_A))),
        (2, meta(None, None, Some(&reply))),
    ]);
    let out = collapse_near_duplicates(fused, &meta);
    let ids: Vec<i64> = out.iter().map(|c| c.message_id).collect();
    assert_eq!(
        ids,
        vec![10, 1],
        "unrelated doc first (unaffected), then the collapsed pair's higher-scoring survivor"
    );
    assert_eq!(out[1].near_duplicates, vec![2]);
}

#[test]
fn an_absorbed_candidates_own_thread_collapsed_members_are_not_lost() {
    // If thread collapse already folded a sibling into candidate 1
    // (`thread_collapsed: [99]`), and candidate 1 is then itself absorbed
    // into candidate 2's near-dup cluster, message 99 must still be
    // reachable from the survivor -- not silently dropped by the merge.
    let mut absorbed_after_thread_collapse = fc(1, 5.0);
    absorbed_after_thread_collapse.thread_collapsed = vec![99];
    let fused = vec![fc(2, 9.0), absorbed_after_thread_collapse];
    let reply = near_dup_b_quoted_reply();
    let meta: BTreeMap<i64, MessageMeta> = BTreeMap::from([
        (2, meta(None, None, Some(NEAR_DUP_A))),
        (1, meta(None, None, Some(&reply))),
    ]);
    let out = collapse_near_duplicates(fused, &meta);
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].message_id, 2);
    let mut absorbed = out[0].near_duplicates.clone();
    absorbed.sort_unstable();
    assert_eq!(
        absorbed,
        vec![1, 99],
        "both the directly-absorbed candidate and what it had already absorbed must survive"
    );
}

#[test]
fn distinct_messages_on_a_similar_topic_do_not_collapse() {
    let fused = vec![fc(1, 5.0), fc(2, 4.0)];
    let meta: BTreeMap<i64, MessageMeta> = BTreeMap::from([
        (1, meta(None, Some(1_000), Some(NEAR_DUP_A))),
        (2, meta(None, Some(2_000), Some(DISTINCT_TOPIC))),
    ]);
    let out = collapse_near_duplicates(fused.clone(), &meta);
    assert_eq!(out.len(), 2, "distinct messages must both survive");
    assert!(out.iter().all(|c| c.near_duplicates.is_empty()));
    assert_eq!(out, fused);
}

#[test]
fn a_message_with_no_body_never_joins_a_cluster() {
    let reply = near_dup_b_quoted_reply();
    let fused = vec![fc(1, 5.0), fc(2, 4.0), fc(3, 3.0)];
    let meta: BTreeMap<i64, MessageMeta> = BTreeMap::from([
        (1, meta(None, None, Some(NEAR_DUP_A))),
        (2, meta(None, None, None)), // no index_content row
        (3, meta(None, None, Some(&reply))),
    ]);
    let out = collapse_near_duplicates(fused, &meta);
    let ids: Vec<i64> = out.iter().map(|c| c.message_id).collect();
    assert_eq!(
        ids,
        vec![1, 2],
        "1 absorbs 3 (near-dup); 2 stands alone (no body)"
    );
    assert_eq!(out[0].near_duplicates, vec![3]);
    assert!(out[1].near_duplicates.is_empty());
}

#[test]
fn fewer_than_two_fingerprints_short_circuits_to_no_collapsing() {
    let fused = vec![fc(1, 5.0), fc(2, 4.0)];
    let meta: BTreeMap<i64, MessageMeta> = BTreeMap::from([
        (1, meta(None, None, None)),
        (2, meta(None, None, Some("hi, just a short one"))), // under the minimum: no fingerprint
    ]);
    let out = collapse_near_duplicates(fused.clone(), &meta);
    assert_eq!(out, fused);
}

// ---------------------------------------------------------------------------
// Fuser: DB-backed metadata lookup + end-to-end orchestration
// ---------------------------------------------------------------------------

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Fixture {
    db: Database,
    account_id: i64,
    mailbox_id: i64,
    next_uid: Cell<i64>,
    path: PathBuf,
}

impl Fixture {
    async fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-fuse-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", path.display())));
        }
        let db = Database::open(&path).expect("open test db");
        let (account_id, mailbox_id) = db
            .write(|c| {
                let account_id = repo::insert_account(
                    c,
                    &repo::NewAccount {
                        name: "Personal".to_owned(),
                        ..Default::default()
                    },
                )?;
                let mailbox_id = repo::insert_mailbox(
                    c,
                    &repo::NewMailbox {
                        account_id,
                        name: "INBOX".to_owned(),
                        ..Default::default()
                    },
                )?;
                Ok((account_id, mailbox_id))
            })
            .await
            .expect("seed account/mailbox");
        Self {
            db,
            account_id,
            mailbox_id,
            next_uid: Cell::new(1),
            path,
        }
    }

    async fn insert_thread(&self) -> i64 {
        let account_id = self.account_id;
        self.db
            .write(move |c| {
                repo::insert_thread(
                    c,
                    &repo::NewThread {
                        account_id,
                        ..Default::default()
                    },
                )
            })
            .await
            .expect("insert thread")
    }

    async fn insert_message(
        &self,
        thread_id: Option<i64>,
        date: Option<i64>,
        body: Option<&str>,
    ) -> i64 {
        let uid = self.next_uid.get();
        self.next_uid.set(uid + 1);
        let (account_id, mailbox_id) = (self.account_id, self.mailbox_id);
        let body = body.map(str::to_owned);
        self.db
            .write(move |c| {
                let id = repo::insert_message(
                    c,
                    &repo::NewMessage {
                        account_id,
                        mailbox_id,
                        uid,
                        uidvalidity: 1,
                        thread_id,
                        date,
                        ..Default::default()
                    },
                )?;
                if let Some(body) = &body {
                    c.execute(
                        "INSERT INTO index_content
                             (message_id, part, text, chars, content_hash, extractor)
                         VALUES (?1, 'body', ?2, ?3, X'00', 'test')",
                        rusqlite::params![id, body, body.len() as i64],
                    )?;
                }
                Ok(id)
            })
            .await
            .expect("insert message")
    }

    fn fuser(&self) -> Fuser {
        Fuser::new(self.db.clone())
    }

    fn no_cancel() -> CancellationToken {
        CancellationToken::new()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.path.display())));
        }
    }
}

#[tokio::test]
async fn fetch_meta_joins_thread_date_and_body_in_one_round_trip() {
    let fx = Fixture::open().await;
    let thread = fx.insert_thread().await;
    let with_everything = fx
        .insert_message(Some(thread), Some(1_700_000_000), Some("hello world"))
        .await;
    let plain = fx.insert_message(None, None, None).await;

    let result = fx
        .fuser()
        .fetch_meta(&[with_everything, plain, 999_999], &Fixture::no_cancel())
        .await
        .expect("fetch should succeed");

    assert_eq!(result.len(), 2, "the nonexistent id contributes no row");
    let got = &result[&with_everything];
    assert_eq!(got.thread_id, Some(thread));
    assert_eq!(got.date, Some(1_700_000_000));
    assert_eq!(got.body.as_deref(), Some("hello world"));

    let plain_meta = &result[&plain];
    assert_eq!(plain_meta.thread_id, None);
    assert_eq!(plain_meta.body, None);
}

#[tokio::test]
async fn fuse_end_to_end_collapses_a_thread_when_asked() {
    let fx = Fixture::open().await;
    let thread = fx.insert_thread().await;
    // Distinct bodies so only thread collapse -- not SimHash -- is in play.
    let root = fx
        .insert_message(Some(thread), Some(1_000), Some(NEAR_DUP_A))
        .await;
    let followup = fx
        .insert_message(Some(thread), Some(2_000), Some(DISTINCT_TOPIC))
        .await;

    let candidates = vec![
        cand(Source::Lexical, root, 5.0, 1),
        cand(Source::Lexical, followup, 3.0, 2),
    ];
    let plan = plan_with_intent(Intent::Navigational);
    let out = fx
        .fuser()
        .fuse(
            candidates,
            &plan,
            &SearchConfig::default(),
            true,
            &Fixture::no_cancel(),
        )
        .await;

    assert_eq!(out.len(), 1, "same-thread messages collapse to one");
    assert_eq!(
        out[0].message_id, root,
        "higher-ranked message is canonical"
    );
    assert_eq!(out[0].thread_collapsed, vec![followup]);
}

#[tokio::test]
async fn fuse_end_to_end_near_dup_collapse_catches_what_thread_collapse_cannot() {
    // The exact case both the module docs and the acceptance bullet call
    // out: a near-duplicate body in two *different* threads. thread_collapse
    // is on, but these two never share a thread_id, so only SimHash's own
    // unconditional pass can catch them.
    let fx = Fixture::open().await;
    let thread_a = fx.insert_thread().await;
    let thread_b = fx.insert_thread().await;
    let in_thread_a = fx
        .insert_message(Some(thread_a), Some(1_000), Some(NEAR_DUP_A))
        .await;
    let reply = near_dup_b_quoted_reply();
    let in_thread_b = fx
        .insert_message(Some(thread_b), Some(2_000), Some(&reply))
        .await;

    let candidates = vec![
        cand(Source::Lexical, in_thread_a, 5.0, 1),
        cand(Source::Lexical, in_thread_b, 3.0, 2),
    ];
    let plan = plan_with_intent(Intent::Navigational);
    let out = fx
        .fuser()
        .fuse(
            candidates,
            &plan,
            &SearchConfig::default(),
            true,
            &Fixture::no_cancel(),
        )
        .await;

    assert_eq!(
        out.len(),
        1,
        "different threads, same underlying text: near-dup collapse still catches it"
    );
    assert_eq!(out[0].message_id, in_thread_a);
    assert_eq!(out[0].near_duplicates, vec![in_thread_b]);
    assert!(
        out[0].thread_collapsed.is_empty(),
        "they were never in the same thread, so thread collapse contributed nothing"
    );
}

#[tokio::test]
async fn fuse_end_to_end_leaves_threads_uncollapsed_when_not_asked() {
    let fx = Fixture::open().await;
    let thread = fx.insert_thread().await;
    let root = fx
        .insert_message(Some(thread), Some(1_000), Some(NEAR_DUP_A))
        .await;
    let followup = fx
        .insert_message(Some(thread), Some(2_000), Some(DISTINCT_TOPIC))
        .await;

    let candidates = vec![
        cand(Source::Lexical, root, 5.0, 1),
        cand(Source::Lexical, followup, 3.0, 2),
    ];
    let plan = plan_with_intent(Intent::Navigational);
    let out = fx
        .fuser()
        .fuse(
            candidates,
            &plan,
            &SearchConfig::default(),
            false,
            &Fixture::no_cancel(),
        )
        .await;

    assert_eq!(out.len(), 2, "thread_collapse=false must not merge them");
    // The metadata lookup still ran (near-dup collapse needs it too), so
    // `thread_id` should be populated even though nothing was merged --
    // a consumer should not have to pay for a second round trip just to
    // learn what this call already knew.
    assert_eq!(out[0].thread_id, Some(thread));
    assert_eq!(out[1].thread_id, Some(thread));
    assert!(out[0].thread_collapsed.is_empty());
    assert!(out[1].thread_collapsed.is_empty());
}

#[tokio::test]
async fn fuse_end_to_end_collapses_near_duplicates_via_the_real_db() {
    let fx = Fixture::open().await;
    // Ranks (not dates) decide the canonical: `better_ranked` is lexical
    // rank 1, `worse_ranked` rank 2, so `better_ranked` gets the higher
    // fused_score and must survive -- deliberately given the *older* date
    // of the two, so a "newest wins" implementation would fail this
    // assertion instead of passing it by coincidence.
    let better_ranked = fx.insert_message(None, Some(1_000), Some(NEAR_DUP_A)).await;
    let reply = near_dup_b_quoted_reply();
    let worse_ranked = fx.insert_message(None, Some(2_000), Some(&reply)).await;

    let candidates = vec![
        cand(Source::Lexical, better_ranked, 5.0, 1),
        cand(Source::Lexical, worse_ranked, 3.0, 2),
    ];
    let plan = plan_with_intent(Intent::Navigational);
    let out = fx
        .fuser()
        .fuse(
            candidates,
            &plan,
            &SearchConfig::default(),
            false,
            &Fixture::no_cancel(),
        )
        .await;

    assert_eq!(
        out.len(),
        1,
        "near-duplicate bodies collapse unconditionally"
    );
    assert_eq!(
        out[0].message_id, better_ranked,
        "the higher-scoring copy is canonical, not the newer one"
    );
    assert_eq!(out[0].near_duplicates, vec![worse_ranked]);
}

#[tokio::test]
async fn a_storage_error_degrades_fetch_meta_to_none_not_a_panic() {
    let fx = Fixture::open().await;
    let id = fx
        .insert_message(None, Some(1_000), Some("hello world"))
        .await;
    // Force a real `rusqlite` error deterministically -- no mocking needed,
    // just make the query `fetch_meta` runs impossible to prepare.
    fx.db
        .write(|c| c.execute("DROP TABLE index_content", []))
        .await
        .expect("drop table");

    let result = fx.fuser().fetch_meta(&[id], &Fixture::no_cancel()).await;
    assert!(
        result.is_none(),
        "a storage error must degrade to None, not panic or propagate"
    );
}

#[tokio::test]
async fn fuse_end_to_end_honors_fusion_linear_from_config() {
    let fx = Fixture::open().await;
    let strong = fx.insert_message(None, Some(1_000), Some(NEAR_DUP_A)).await;
    let weak = fx
        .insert_message(None, Some(1_000), Some(DISTINCT_TOPIC))
        .await;

    let candidates = vec![
        cand(Source::Lexical, strong, 10.0, 1),
        cand(Source::Lexical, weak, 0.0, 2),
    ];
    let plan = plan_with_intent(Intent::Navigational);
    let cfg = SearchConfig {
        fusion: Fusion::Linear,
        ..SearchConfig::default()
    };
    let out = fx
        .fuser()
        .fuse(candidates, &plan, &cfg, false, &Fixture::no_cancel())
        .await;

    assert_eq!(out.len(), 2);
    assert_eq!(out[0].message_id, strong);
    // navigational lexical weight = 1.0; minmax(10.0 over [0,10]) = 1.0.
    approx_eq(out[0].fused_score, 1.0);
    assert_eq!(out[1].message_id, weak);
    approx_eq(out[1].fused_score, 0.0);
}

#[tokio::test]
async fn fuse_end_to_end_drops_a_recency_only_match_when_the_query_has_free_text() {
    // The exact regression task 33's own `SearchService` integration tests
    // surfaced: a mailbox with one message that actually matches the query
    // and one that does not, where the recency retriever (unconditional,
    // gated only by hard filters) still returns *both* — without this drop,
    // the irrelevant message would reach presentation.
    let fx = Fixture::open().await;
    let matching = fx
        .insert_message(None, Some(1_000), Some("budgetary review"))
        .await;
    let irrelevant = fx
        .insert_message(None, Some(2_000), Some("lunch plans"))
        .await;

    let candidates = vec![
        cand(Source::Lexical, matching, 5.0, 1),
        cand(Source::Recency, matching, 1.0, 2),
        cand(Source::Recency, irrelevant, 1.0, 1),
    ];
    let mut plan = plan_with_intent(Intent::Navigational);
    plan.lexical_terms = vec![PlanTerm {
        text: "budgetary".to_owned(),
        negated: false,
        mode: Mode::Auto,
        weight: 1.0,
        origin: TermOrigin::Original,
    }];
    let out = fx
        .fuser()
        .fuse(
            candidates,
            &plan,
            &SearchConfig::default(),
            false,
            &Fixture::no_cancel(),
        )
        .await;

    assert_eq!(
        out.len(),
        1,
        "the recency-only, lexically-unrelated message must not survive fusion"
    );
    assert_eq!(out[0].message_id, matching);
}

#[tokio::test]
async fn a_cancelled_token_degrades_to_uncollapsed_fusion_not_a_failure() {
    let fx = Fixture::open().await;
    let thread = fx.insert_thread().await;
    let root = fx
        .insert_message(Some(thread), Some(1_000), Some(NEAR_DUP_A))
        .await;
    let followup = fx
        .insert_message(Some(thread), Some(2_000), Some(DISTINCT_TOPIC))
        .await;

    let candidates = vec![
        cand(Source::Lexical, root, 5.0, 1),
        cand(Source::Lexical, followup, 3.0, 2),
    ];
    let plan = plan_with_intent(Intent::Navigational);
    let cancelled = CancellationToken::new();
    cancelled.cancel();

    let out = fx
        .fuser()
        .fuse(
            candidates,
            &plan,
            &SearchConfig::default(),
            true,
            &cancelled,
        )
        .await;

    // The RRF fusion itself needed no I/O, so it still ran; only the
    // DB-backed collapse steps degrade.
    assert_eq!(
        out.len(),
        2,
        "a cancelled metadata lookup must not merge threads"
    );
    approx_eq(out[0].fused_score, 1.0 / 61.0);
}
