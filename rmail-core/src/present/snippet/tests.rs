//! What task 32's own acceptance bullet demands proven: "a query term
//! containing FTS5 metacharacters, or a message body containing the
//! highlight delimiters, must not corrupt the output or let markup escape."
//! This module's design (offsets into the source text, never embedded
//! markup — see the module docs) makes the second half true by
//! construction; the tests below still exercise both directions against
//! real adversarial input rather than trusting the design argument alone.

use super::*;

fn terms(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| (*w).to_owned()).collect()
}

fn slice<'a>(snippet: &'a Snippet, range: &std::ops::Range<usize>) -> &'a str {
    &snippet.text[range.clone()]
}

// ---------------------------------------------------------------------------
// Safety: FTS5 metacharacters in a query term
// ---------------------------------------------------------------------------

/// A term shaped like FTS5 syntax (`NEAR(...)`, unbalanced quotes, a bare
/// `OR`, a `*` prefix wildcard) is never turned into a query — this module
/// builds no `MATCH` string at all, so there is no parser for it to
/// restructure. Since none of these appear literally in the body, the
/// correct, unremarkable outcome is simply "no match" — not a panic, not a
/// corrupted snippet, not a hang.
#[test]
fn fts5_metacharacter_terms_that_do_not_literally_occur_are_simply_not_matched() {
    let body = "the quarterly report is attached, please review by friday";
    let dangerous_terms = terms(&[
        "\"unterminated",
        "NEAR(x,1)",
        "a\" OR \"1\"=\"1",
        "term*",
        "(nested (parens",
        "col:injected",
    ]);
    for term in &dangerous_terms {
        let result = extract(body, std::slice::from_ref(term), &[]);
        assert!(
            result.is_none(),
            "term {term:?} does not literally occur in the body and must not match"
        );
    }
}

/// The complementary case: a body that *does* literally contain FTS5-
/// metacharacter-shaped text (a message quoting a bug report full of SQL/FTS
/// syntax) still matches and highlights correctly when a *phrase* is
/// exactly that literal text — proving the metacharacters are being
/// compared as plain bytes, not silently stripped, escaped, or reinterpreted
/// as syntax. Phrases (not single-token terms) are the path that exercises
/// this: `find_phrase` does substring comparison over arbitrary bytes,
/// including quotes, colons, and asterisks, with no notion of "syntax" at
/// all.
#[test]
fn fts5_metacharacter_text_matches_literally_when_actually_present() {
    let body = r#"the failing query was subject:"foo" OR body:bar* -- please fix"#;
    let phrase = vec![r#"subject:"foo" OR body:bar*"#.to_owned()];
    let snippet = extract(body, &[], &phrase).expect("the literal text is present");
    assert_eq!(snippet.highlights.len(), 1);
    assert_eq!(
        slice(&snippet, &snippet.highlights[0]),
        r#"subject:"foo" OR body:bar*"#,
        "the exact literal bytes, punctuation included, must be what gets highlighted"
    );
}

/// A genuinely alphanumeric-token term is still highlighted exactly, even
/// while sitting inside a body full of FTS5-shaped punctuation around it —
/// proving the punctuation is not corrupting position tracking for a real
/// match nearby.
#[test]
fn a_real_token_still_highlights_correctly_next_to_fts5_syntax_noise() {
    let body = r#"query: subject:"foo" OR bar* AND NEAR(baz, 2) -- confidential"#;
    let term = terms(&["confidential"]);
    let snippet = extract(body, &term, &[]).expect("confidential occurs literally");
    assert_eq!(snippet.highlights.len(), 1);
    assert_eq!(
        slice(&snippet, &snippet.highlights[0]).to_lowercase(),
        "confidential"
    );
}

// ---------------------------------------------------------------------------
// Safety: highlight-delimiter-shaped text in the body
// ---------------------------------------------------------------------------

/// A body that already contains text shaped like markup this module might
/// otherwise have chosen as a delimiter (`<mark>...</mark>`, `**...**`)
/// must survive untouched in the output, and a real match nearby must still
/// highlight correctly — nothing about this module's extraction treats
/// those substrings specially, because it never emits delimiters into
/// `text` at all (see the module docs' "Offsets, never embedded markup"
/// section).
#[test]
fn body_containing_markup_lookalikes_is_never_corrupted() {
    let body = "click <mark>here</mark> to confirm your **invoice** now";
    let term = terms(&["invoice"]);
    let snippet = extract(body, &term, &[]).expect("invoice occurs literally");
    assert!(
        snippet.text.contains("<mark>here</mark>"),
        "the body's own markup-lookalike text must survive verbatim: {:?}",
        snippet.text
    );
    assert_eq!(snippet.highlights.len(), 1);
    assert_eq!(
        slice(&snippet, &snippet.highlights[0]).to_lowercase(),
        "invoice"
    );
}

/// The starkest version of the same property: the query term *is* the
/// delimiter-shaped text. Even then, the output is offsets into unmodified
/// source text — there is no delimiter string anywhere in `Snippet` for a
/// downstream renderer to misinterpret as its own markup.
#[test]
fn a_query_term_that_is_itself_delimiter_shaped_text_still_only_produces_offsets() {
    let body = "the tag literally says mark right there in the sentence";
    let term = terms(&["mark"]);
    let snippet = extract(body, &term, &[]).expect("mark occurs literally");
    assert_eq!(snippet.highlights.len(), 1);
    // No literal delimiter characters were ever inserted -- the only marks
    // of this being a "highlight" are the byte offsets themselves.
    assert!(!snippet.text.contains("<mark>"));
    assert!(!snippet.text.contains("</mark>"));
}

/// A phrase containing characters that would be delimiter-shaped in many
/// markup schemes (`*bold*`) must not corrupt output positions either.
#[test]
fn phrase_with_delimiter_shaped_characters_matches_and_highlights_safely() {
    let body = "please read the *bold* warning before continuing";
    let phrase = vec!["*bold*".to_owned()];
    let snippet = extract(body, &[], &phrase).expect("the phrase occurs literally");
    assert_eq!(snippet.highlights.len(), 1);
    assert_eq!(slice(&snippet, &snippet.highlights[0]), "*bold*");
}

// ---------------------------------------------------------------------------
// Byte-safety: multi-byte text, control characters, degenerate input
// ---------------------------------------------------------------------------

/// Highlight ranges must always be valid char-boundary slices of the
/// returned `text`, even for multi-byte terms/bodies — an offset that lands
/// mid-character would panic the moment a caller sliced it.
#[test]
fn multibyte_terms_produce_valid_char_boundary_highlights() {
    let body = "the café serves crème brûlée at noon";
    let term = terms(&["café", "brûlée"]);
    let snippet = extract(body, &term, &[]).expect("both terms occur literally");
    for range in &snippet.highlights {
        assert!(snippet.text.is_char_boundary(range.start));
        assert!(snippet.text.is_char_boundary(range.end));
        // Slicing must not panic.
        let _ = &snippet.text[range.clone()];
    }
}

/// A body containing NUL bytes, control characters, and other unusual (but
/// perfectly valid) `String` content must not panic anywhere in the
/// extraction path.
#[test]
fn control_characters_and_nul_bytes_do_not_panic() {
    let bodies = [
        "before\u{0}after invoice text",
        "line1\r\nline2\tinvoice\u{7}",
        "\u{200B}zero\u{200B}width\u{200B}invoice",
        "",
        "   ",
        "a",
    ];
    let term = terms(&["invoice"]);
    for body in bodies {
        // Must not panic regardless of whether it finds a match.
        let _ = extract(body, &term, &[]);
        let _ = plain_excerpt(body);
    }
}

/// A term/phrase list containing empty strings or strings with no
/// alphanumeric content must not panic and must simply never match (an
/// empty term can never equal a non-empty token; an empty phrase is
/// rejected by `find_phrase`'s own guard).
#[test]
fn degenerate_terms_and_phrases_never_panic() {
    let body = "some ordinary body text";
    let weird_terms = terms(&["", "   ", "-", "~~~"]);
    let weird_phrases = vec![String::new(), "   ".to_owned()];
    let _ = extract(body, &weird_terms, &weird_phrases);
}

/// A phrase longer than the entire body must not panic (the early-return
/// guard in `find_phrase`), and correctly reports no match.
#[test]
fn a_phrase_longer_than_the_body_does_not_panic_and_does_not_match() {
    let body = "short";
    let phrase = vec!["this phrase is much longer than the body itself".to_owned()];
    assert!(extract(body, &[], &phrase).is_none());
}

// ---------------------------------------------------------------------------
// Correctness: matching, window selection, highlighting
// ---------------------------------------------------------------------------

#[test]
fn a_term_present_in_the_body_is_found_and_highlighted() {
    let body = "please review the quarterly invoice before Friday";
    let term = terms(&["invoice"]);
    let snippet = extract(body, &term, &[]).expect("term occurs");
    assert_eq!(snippet.highlights.len(), 1);
    assert_eq!(
        slice(&snippet, &snippet.highlights[0]).to_lowercase(),
        "invoice"
    );
}

#[test]
fn matching_is_case_insensitive_on_ascii() {
    let body = "The INVOICE is attached";
    let term = terms(&["invoice"]);
    let snippet = extract(body, &term, &[]).expect("case-insensitive match");
    assert_eq!(slice(&snippet, &snippet.highlights[0]), "INVOICE");
}

#[test]
fn a_term_absent_from_the_body_returns_none() {
    let body = "nothing relevant in here at all";
    let term = terms(&["invoice"]);
    assert!(extract(body, &term, &[]).is_none());
}

#[test]
fn a_phrase_requires_verbatim_adjacency() {
    let verbatim = "the quarterly report is attached";
    let scrambled = "report: quarterly figures inside";
    let phrase = vec!["quarterly report".to_owned()];
    assert!(extract(verbatim, &[], &phrase).is_some());
    assert!(extract(scrambled, &[], &phrase).is_none());
}

/// The chosen window prefers the region with the most matches, not merely
/// the first match encountered — a body with an isolated single hit far
/// from a dense cluster of three later on must pick the cluster, and the
/// isolated hit must not appear in the final snippet at all.
#[test]
fn window_selection_prefers_the_densest_match_cluster() {
    // 600 bytes of filler on each side is comfortably wider than the
    // ~220-byte window budget (even after the edge-of-text reclaim that
    // widens a clamped window on its open side), so the "alpha" anchor's
    // own window can never accidentally reach the cluster and vice versa —
    // the two are genuinely disjoint candidate windows.
    let filler = "x ".repeat(300);
    let body = format!("alpha {filler}beta gamma delta {filler}");
    let term = terms(&["alpha", "beta", "gamma", "delta"]);
    let snippet = extract(&body, &term, &[]).expect("terms occur");
    let lower = snippet.text.to_lowercase();
    assert!(
        !lower.contains("alpha"),
        "an isolated single hit far from the cluster must lose out to it: {:?}",
        snippet.text
    );
    assert!(
        snippet.highlights.len() >= 2,
        "expected the densest cluster (beta, gamma, delta) to be chosen, got {} highlights in {:?}",
        snippet.highlights.len(),
        snippet.text
    );
}

#[test]
fn ellipsis_is_added_only_when_the_window_does_not_reach_the_text_edges() {
    let short_body = "invoice attached";
    let term = terms(&["invoice"]);
    let snippet = extract(short_body, &term, &[]).expect("term occurs");
    assert!(
        !snippet.text.contains(ELLIPSIS),
        "a window covering the whole short body needs no ellipsis: {:?}",
        snippet.text
    );

    let long_prefix = "filler word ".repeat(200);
    // A space on both sides of "invoice": `long_prefix` itself has no
    // leading separator, so concatenating it directly after "invoice"
    // would merge into one "invoicefiller..." token that can never match
    // the bare term "invoice" again.
    let long_body = format!("{long_prefix}invoice {long_prefix}");
    let snippet = extract(&long_body, &term, &[]).expect("term occurs");
    assert!(
        snippet.text.contains(ELLIPSIS),
        "a window that had to cut into a much longer body needs an ellipsis: {:?}",
        snippet.text
    );
}

#[test]
fn overlapping_term_and_phrase_highlights_are_merged_not_duplicated() {
    let body = "please see the quarterly report attached";
    let term = terms(&["report"]);
    let phrase = vec!["quarterly report".to_owned()];
    let snippet = extract(body, &term, &phrase).expect("both match");
    // The phrase's range subsumes the term's own "report" range; they must
    // merge into one highlight, not two overlapping ones.
    assert_eq!(
        snippet.highlights.len(),
        1,
        "an overlapping term+phrase match must merge into one range: {:?}",
        snippet.highlights
    );
    assert_eq!(slice(&snippet, &snippet.highlights[0]), "quarterly report");
    // General invariant, checked unconditionally: whatever the final count,
    // no two highlight ranges may overlap.
    for pair in snippet.highlights.windows(2) {
        assert!(
            pair[0].end <= pair[1].start,
            "highlight ranges must never overlap: {:?}",
            snippet.highlights
        );
    }
}

// ---------------------------------------------------------------------------
// plain_excerpt
// ---------------------------------------------------------------------------

#[test]
fn plain_excerpt_of_empty_text_is_an_empty_snippet() {
    assert_eq!(plain_excerpt(""), Snippet::default());
    assert_eq!(plain_excerpt("   "), Snippet::default());
}

#[test]
fn plain_excerpt_never_has_highlights() {
    let excerpt = plain_excerpt("some perfectly ordinary opening text for a message body");
    assert!(excerpt.highlights.is_empty());
}

#[test]
fn plain_excerpt_truncates_long_text_with_an_ellipsis_and_a_word_boundary() {
    let body = "word ".repeat(200);
    let excerpt = plain_excerpt(&body);
    assert!(excerpt.text.len() < body.len());
    assert!(excerpt.text.ends_with(ELLIPSIS));
    assert!(
        !excerpt
            .text
            .trim_end_matches(ELLIPSIS)
            .trim_end()
            .ends_with("wor"),
        "must not cut mid-word: {:?}",
        excerpt.text
    );
}

#[test]
fn plain_excerpt_of_short_text_returns_it_whole_with_no_ellipsis() {
    let body = "short body";
    let excerpt = plain_excerpt(body);
    assert_eq!(excerpt.text, body);
}

// ---------------------------------------------------------------------------
// query_terms
// ---------------------------------------------------------------------------

#[test]
fn query_terms_excludes_negated_and_semantic_forced_terms() {
    let q = query_terms("invoice -spam ~vague \"exact phrase\" -\"excluded phrase\"");
    assert_eq!(q.terms, vec!["invoice".to_owned()]);
    assert_eq!(q.phrases, vec!["exact phrase".to_owned()]);
}

#[test]
fn query_terms_deduplicates_case_insensitively() {
    let q = query_terms("Invoice invoice INVOICE");
    assert_eq!(q.terms.len(), 1);
}

#[test]
fn query_terms_of_an_empty_query_is_empty() {
    let q = query_terms("");
    assert!(q.terms.is_empty());
    assert!(q.phrases.is_empty());
}

#[test]
fn query_terms_excludes_operator_only_input() {
    let q = query_terms("from:alice is:unread");
    assert!(
        q.terms.is_empty(),
        "operators are not free-text terms: {q:?}"
    );
}

// ---------------------------------------------------------------------------
// Pure helper units
// ---------------------------------------------------------------------------

#[test]
fn eq_ignore_ascii_case_requires_equal_length() {
    assert!(eq_ignore_ascii_case("Invoice", "invoice"));
    assert!(!eq_ignore_ascii_case("invoice", "invoices"));
}

#[test]
fn find_phrase_returns_no_overlapping_matches() {
    let matches = find_phrase("aaaa", "aa");
    // Non-overlapping scan: "aa" at 0..2, then resumes at 2, finds "aa" at
    // 2..4 -- two matches, not three overlapping ones.
    assert_eq!(matches, vec![0..2, 2..4]);
}

#[test]
fn cap_chars_snaps_to_a_character_boundary() {
    let text = "café"; // 'é' is 2 bytes; cap at 3 chars keeps "caf"
    let capped = cap_chars(text, 3);
    assert_eq!(capped, "caf");
    assert!(std::str::from_utf8(capped.as_bytes()).is_ok());
}

#[test]
fn merge_ranges_merges_touching_and_overlapping_but_not_disjoint() {
    let mut ranges = vec![0..3, 3..5, 10..12, 4..6];
    merge_ranges(&mut ranges);
    assert_eq!(ranges, vec![0..6, 10..12]);
}
