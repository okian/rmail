use super::{Scorer, EXACT_SUBSTRING_BONUS, MAX_MATCH_CHARS, MAX_QUERY_CHARS};
use crate::finder::fold;

/// Score a query against text that is treated as entirely `primary`.
fn hit(query: &str, text: &str) -> Option<(u32, Vec<u32>)> {
    let blob = fold::fold(text);
    let folded_len = blob.chars().count();
    let mut scorer = Scorer::new(query)?;
    let score = scorer.score(&blob)?;
    let positions = scorer.positions(&blob, text, folded_len);
    Some((score, positions))
}

fn score_of(query: &str, text: &str) -> Option<u32> {
    hit(query, text).map(|(score, _)| score)
}

#[test]
fn characters_must_appear_in_order() {
    assert!(score_of("qr", "Quarterly Report").is_some());
    // Same characters, wrong order.
    assert!(score_of("rq", "Quarterly").is_none());
}

#[test]
fn a_query_char_that_is_absent_does_not_match() {
    assert!(score_of("zzz", "Quarterly Report").is_none());
}

/// prd.md's exact-substring rule. The bonus has to actually move the score,
/// not merely be documented.
#[test]
fn an_exact_substring_outscores_a_scattered_match() {
    let contiguous = score_of("report", "Quarterly Report").expect("substring matches");
    let scattered = score_of("report", "Rewrite Every Position Or Restart Tomorrow")
        .expect("scattered subsequence matches");
    assert!(
        contiguous > scattered,
        "contiguous {contiguous} must beat scattered {scattered}"
    );
    assert!(
        contiguous >= EXACT_SUBSTRING_BONUS,
        "the flat substring bonus is not being applied: {contiguous}"
    );
}

/// The short-circuit must not change *whether* something matches, only how it
/// is scored — a substring is also a subsequence.
#[test]
fn the_substring_path_and_the_fuzzy_path_agree_on_matching() {
    for (query, text) in [
        ("acme", "Acme invoice 338"),
        ("acmeinv", "Acme invoice 338"),
        ("338", "Acme invoice 338"),
    ] {
        assert!(
            score_of(query, text).is_some(),
            "{query:?} should match {text:?}"
        );
    }
}

/// prd.md: "case-insensitive with smart-case (any uppercase -> case-sensitive)".
#[test]
fn smart_case_is_insensitive_until_an_uppercase_is_typed() {
    assert!(score_of("report", "Quarterly Report").is_some());
    let sensitive = Scorer::new("Report").expect("non-empty");
    assert!(sensitive.case_sensitive());
    let insensitive = Scorer::new("report").expect("non-empty");
    assert!(!insensitive.case_sensitive());
}

#[test]
fn an_uppercase_query_stops_matching_lowercase_text() {
    assert!(score_of("report", "quarterly report").is_some());
    assert!(score_of("Report", "quarterly report").is_none());
    assert!(score_of("Report", "Quarterly Report").is_some());
}

/// prd.md's NFKC+ASCII-fold requirement, seen from the matcher's side.
#[test]
fn folding_makes_an_accented_subject_typeable_in_ascii() {
    assert!(score_of("cafe", "Café meeting notes").is_some());
    // ...and the other direction, since folding is applied to both sides.
    assert!(score_of("café", "Cafe meeting notes").is_some());
}

/// nucleo case-folds the haystack only and compares the needle raw, so under
/// `ignore_case` the needle must arrive lowercased. Folding can *introduce*
/// uppercase the smart-case check never saw, and such a query would then
/// match nothing that is not already spelled with the same capitals.
///
/// Each haystack here is deliberately **lower**case. That is the case that
/// actually bites: pairing `ǅ` with text also containing `ǅ` passes either
/// way, because both sides fold to the identical `Dz` and nucleo's substring
/// path memchr's for the needle's literal first byte. Case-insensitive
/// matching is supposed to find `hz` for a query of `㎐`, and an
/// un-lowercased needle silently does not.
#[test]
fn a_fold_that_introduces_uppercase_still_matches_lowercase_text() {
    for (query, text) in [
        // U+01C5, a titlecase digraph: compatibility-decomposes to `Dz`, and
        // `is_uppercase` is false for it, so smart-case stays off.
        ("\u{01C5}", "dzagreb notes"),
        // U+1D2C modifier capital A -> "A".
        ("\u{1D2C}", "a modifier"),
        // U+3390, squared hertz -> "Hz".
        ("\u{3390}", "clock hz reading"),
    ] {
        assert!(
            !query.chars().any(char::is_uppercase),
            "{query:?} must not look uppercase, or the test proves nothing"
        );
        assert!(
            fold::fold(query).chars().any(char::is_uppercase),
            "{query:?} must fold to something uppercase, or the test proves nothing"
        );
        assert!(
            score_of(query, text).is_some(),
            "{query:?} lost its match against the lowercase text {text:?}"
        );
    }
}

/// ...and the needle handed to the matcher is genuinely case-folded whenever
/// case-insensitive matching is on, which is nucleo's documented contract.
#[test]
fn the_needle_is_lowercased_unless_smart_case_fired() {
    let insensitive = Scorer::new("\u{3390}").expect("non-empty");
    assert!(!insensitive.case_sensitive());
    assert_eq!(insensitive.needle(), "hz");

    // An uppercase the user actually typed turns smart-case on, and then the
    // needle keeps its case because nucleo is no longer folding either side.
    let sensitive = Scorer::new("Hz").expect("non-empty");
    assert!(sensitive.case_sensitive());
    assert_eq!(sensitive.needle(), "Hz");
}

#[test]
fn an_empty_query_has_no_scorer() {
    assert!(Scorer::new("").is_none());
    // A lone combining mark folds away to nothing, which is the same case.
    assert!(Scorer::new("\u{0301}").is_none());
}

// ---------------------------------------------------------------------------
// positions
// ---------------------------------------------------------------------------

#[test]
fn positions_point_at_the_matched_characters() {
    let (_, positions) = hit("qr", "Quarterly Report").expect("matches");
    // 'Q' at char 0, 'R' at char 10.
    assert_eq!(positions, vec![0, 10]);
}

/// The bug class this module exists to avoid: a highlight offset that lands
/// mid-character. Every position must be a *char* index into the rendered
/// string, so slicing at the corresponding byte boundary must be valid.
#[test]
fn positions_are_char_offsets_not_byte_offsets() {
    let text = "Café résumé draft";
    let (_, positions) = hit("crd", text).expect("matches");
    let chars: Vec<char> = text.chars().collect();
    assert!(!positions.is_empty());
    for position in &positions {
        let index = *position as usize;
        assert!(
            index < chars.len(),
            "position {position} is past the {} chars of {text:?}",
            chars.len()
        );
        // The byte offset of that char is a valid boundary by construction;
        // a byte-offset implementation would have handed back 5 for the 'r'
        // of `résumé` (which is byte 6), landing inside the 'é'.
        let byte = text
            .char_indices()
            .nth(index)
            .map(|(byte, _)| byte)
            .expect("char index is in range");
        assert!(text.is_char_boundary(byte));
    }
    assert_eq!(
        positions
            .iter()
            .map(|p| chars[*p as usize])
            .collect::<String>(),
        "Crd"
    );
}

/// Folding is not length-preserving, so a naive implementation that reported
/// folded indices directly would be off by one from the first ligature on.
#[test]
fn positions_survive_a_length_changing_fold() {
    let text = "ﬁle report";
    let (_, positions) = hit("fr", text).expect("matches");
    let chars: Vec<char> = text.chars().collect();
    // 'f' is inside the ligature at char 0; 'r' of "report" is char 4.
    assert_eq!(positions, vec![0, 4]);
    assert_eq!(chars[4], 'r');
}

/// A ligature can produce two folded characters from one source character;
/// both must not be reported twice.
#[test]
fn positions_are_deduped_and_ascending() {
    let text = "ﬁle";
    let (_, positions) = hit("fi", text).expect("matches");
    assert_eq!(positions, vec![0]);
    let text = "quarterly report";
    let (_, positions) = hit("qrt", text).expect("matches");
    assert!(
        positions.windows(2).all(|w| w[0] < w[1]),
        "positions must be strictly ascending: {positions:?}"
    );
}

/// Positions belong to `primary_text` and nothing else. A match inside the
/// secondary half of the blob is silently dropped rather than reported as an
/// offset into a string the caller never passed.
#[test]
fn positions_inside_the_secondary_half_are_dropped() {
    let primary = "Invoice";
    let secondary = "billing@acme.com";
    let mut blob = fold::fold(primary);
    let primary_folded_len = blob.chars().count();
    blob.push(' ');
    blob.push_str(&fold::fold(secondary));

    let mut scorer = Scorer::new("acme").expect("non-empty");
    assert!(
        scorer.score(&blob).is_some(),
        "the secondary half must still be matchable"
    );
    let positions = scorer.positions(&blob, primary, primary_folded_len);
    assert!(
        positions.is_empty(),
        "a match entirely in the secondary text must yield no primary highlight: {positions:?}"
    );
}

/// Non-ASCII text goes through nucleo's `Unicode` representation, and this is
/// the case where handing it grapheme clusters instead of codepoints would
/// silently shift every position.
#[test]
fn positions_are_correct_for_non_latin_text() {
    let text = "会議 report";
    let (_, positions) = hit("会r", text).expect("matches");
    let chars: Vec<char> = text.chars().collect();
    assert_eq!(chars[positions[0] as usize], '会');
    assert_eq!(chars[positions[1] as usize], 'r');
}

// ---------------------------------------------------------------------------
// bounds
// ---------------------------------------------------------------------------

/// The DP's cost is `O(query x candidate)`; both factors are capped, and the
/// query side is capped here.
#[test]
fn a_long_query_is_truncated_to_the_dp_bound() {
    let long = "a".repeat(MAX_QUERY_CHARS * 4);
    let scorer = Scorer::new(&long).expect("non-empty");
    assert_eq!(scorer.needle().chars().count(), MAX_QUERY_CHARS);
}

/// The other factor: a query still has to *work* against a blob that was
/// truncated at the candidate bound, rather than silently stop matching.
#[test]
fn a_query_matches_inside_a_capped_blob() {
    let head: String = std::iter::repeat_n("word ", MAX_MATCH_CHARS / 5).collect();
    let text = format!("{head}needle");
    let blob: String = fold::fold(&text).chars().take(MAX_MATCH_CHARS).collect();
    let mut scorer = Scorer::new("word").expect("non-empty");
    assert!(
        scorer.score(&blob).is_some(),
        "text inside the cap must still match"
    );
    let mut scorer = Scorer::new("needle").expect("non-empty");
    assert!(
        scorer.score(&blob).is_none(),
        "the cap is a real truncation, not a suggestion"
    );
}

/// The mask a caller prefilters with has to agree with what the scorer will
/// actually accept: anything the scorer matches must have been admitted.
#[test]
fn the_scorers_mask_never_rejects_something_it_would_match() {
    let candidates = [
        "Quarterly Report",
        "café meeting",
        "ﬁle a bug",
        "会議の議事録",
        "invoice 338",
    ];
    for query in ["qr", "cafe", "file", "会", "338", "Report"] {
        let Some(mut scorer) = Scorer::new(query) else {
            continue;
        };
        for text in candidates {
            let blob = fold::fold(text);
            if scorer.score(&blob).is_some() {
                assert!(
                    fold::mask_admits(fold::char_mask(&blob), scorer.mask()),
                    "the prefilter rejected {text:?} which {query:?} actually matches"
                );
            }
        }
    }
}
