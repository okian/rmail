//! What every retriever depends on this module for: turning a scored,
//! best-first list into ranked candidates the same way every time.

use super::*;

#[test]
fn ranks_are_one_based_and_follow_input_order() {
    let candidates = rank_by_score(Source::Lexical, vec![(10, 9.5), (20, 4.0), (30, 1.0)]);
    assert_eq!(
        candidates,
        vec![
            Candidate {
                message_id: 10,
                source: Source::Lexical,
                score: 9.5,
                rank: 1,
                mean_score: None,
            },
            Candidate {
                message_id: 20,
                source: Source::Lexical,
                score: 4.0,
                rank: 2,
                mean_score: None,
            },
            Candidate {
                message_id: 30,
                source: Source::Lexical,
                score: 1.0,
                rank: 3,
                mean_score: None,
            },
        ]
    );
}

#[test]
fn an_empty_list_ranks_to_nothing() {
    assert!(rank_by_score(Source::Lexical, Vec::new()).is_empty());
}

#[test]
fn every_candidate_carries_the_source_it_was_ranked_under() {
    // `rank_by_score` does not get to decide which source produced a
    // candidate — it only numbers the list it is handed. A bug that dropped
    // or overwrote the source would still pass every score/rank assertion.
    for source in [
        Source::Lexical,
        Source::Dense,
        Source::Fuzzy,
        Source::Entity,
        Source::Structured,
        Source::Prefix,
        Source::Recency,
    ] {
        let candidates = rank_by_score(source, vec![(1, 1.0)]);
        assert_eq!(candidates[0].source, source);
    }
}
