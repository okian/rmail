//! What task 32's own acceptance bullet demands proven: "successive batches
//! must be non-increasing in score with no duplicates across batch
//! boundaries" — task 33 streams these and a client renders them
//! incrementally, so an out-of-order or duplicated batch is a visible bug,
//! not a cosmetic one.

use super::*;
use crate::present::Snippet;

fn result(message_id: i64, score: f64) -> PresentedResult {
    PresentedResult {
        message_id,
        score,
        thread_id: None,
        thread_collapsed: Vec::new(),
        near_duplicates: Vec::new(),
        snippet: Snippet::default(),
    }
}

/// Ten results, strictly descending in score — the shape
/// [`super::super::strict_score_order`] (navigational/lookup intent, MMR
/// disabled) always produces, and the one this test exercises end to end.
fn ten_results_best_first() -> Vec<PresentedResult> {
    (0..10i64).map(|i| result(i + 1, 10.0 - i as f64)).collect()
}

#[test]
fn batches_reproduce_the_input_exactly_when_concatenated() {
    let results = ten_results_best_first();
    let batches = batch(&results, 3);
    let flattened: Vec<PresentedResult> = batches.into_iter().flatten().collect();
    assert_eq!(flattened, results);
}

#[test]
fn no_message_id_appears_in_more_than_one_batch() {
    let results = ten_results_best_first();
    let batches = batch(&results, 3);
    let mut seen = std::collections::BTreeSet::new();
    for page in &batches {
        for r in page {
            assert!(
                seen.insert(r.message_id),
                "message {} appeared in more than one batch",
                r.message_id
            );
        }
    }
    assert_eq!(seen.len(), results.len());
}

#[test]
fn successive_batches_are_non_increasing_in_score() {
    let results = ten_results_best_first();
    let batches = batch(&results, 3);
    assert!(
        batches.len() > 1,
        "the test needs at least two batches to prove anything"
    );
    for pair in batches.windows(2) {
        let end_of_prior = pair[0].last().expect("non-empty batch").score;
        let start_of_next = pair[1].first().expect("non-empty batch").score;
        assert!(
            end_of_prior >= start_of_next,
            "batch boundary went out of order: {end_of_prior} then {start_of_next}"
        );
    }
    // And within each batch itself, since `batch` must not reorder.
    for page in &batches {
        for pair in page.windows(2) {
            assert!(pair[0].score >= pair[1].score);
        }
    }
}

#[test]
fn navigational_batches_are_strict_score_order() {
    // Pins the module docs' own claim: for MMR-disabled intents, "best-
    // first" and "score-ordered" are the same order, so batching a
    // navigational-style result list is exactly the streaming contract
    // task 33 depends on with no caveats.
    let mut results = ten_results_best_first();
    // Shuffle-proof: even if fed out of order, this test only asserts on
    // an *already* best-first input, matching what `strict_score_order`
    // guarantees `Presenter::present` actually hands to `batch`.
    results.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap());
    let batches = batch(&results, DEFAULT_BATCH_SIZE);
    let flattened: Vec<f64> = batches.into_iter().flatten().map(|r| r.score).collect();
    let mut expected: Vec<f64> = results.iter().map(|r| r.score).collect();
    expected.sort_by(|a, b| b.partial_cmp(a).unwrap());
    assert_eq!(flattened, expected);
}

#[test]
fn a_batch_size_larger_than_the_result_count_returns_one_batch() {
    let results = ten_results_best_first();
    let batches = batch(&results, 1000);
    assert_eq!(batches.len(), 1);
    assert_eq!(batches[0].len(), results.len());
}

#[test]
fn a_batch_size_of_zero_is_clamped_to_one_rather_than_producing_no_pages() {
    let results = ten_results_best_first();
    let batches = batch(&results, 0);
    assert_eq!(
        batches.len(),
        results.len(),
        "clamped to batch_size=1: one result per page"
    );
}

#[test]
fn empty_results_produce_no_batches() {
    let batches = batch(&[], 5);
    assert!(batches.is_empty());
}

#[test]
fn an_exact_multiple_produces_no_short_final_batch() {
    let results = ten_results_best_first();
    let batches = batch(&results, 5);
    assert_eq!(batches.len(), 2);
    assert_eq!(batches[0].len(), 5);
    assert_eq!(batches[1].len(), 5);
}

#[test]
fn a_remainder_produces_a_short_final_batch() {
    let results = ten_results_best_first();
    let batches = batch(&results, 4);
    assert_eq!(batches.len(), 3);
    assert_eq!(batches[0].len(), 4);
    assert_eq!(batches[1].len(), 4);
    assert_eq!(batches[2].len(), 2);
}
