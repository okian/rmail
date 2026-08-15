use super::{char_mask, fold, fold_with_map, mask_admits};

/// prd.md's own worked example: `café` has to be findable by typing `cafe`.
#[test]
fn diacritics_fold_away() {
    assert_eq!(fold("café"), "cafe");
    // The decomposed spelling of the same word has to fold identically, or
    // whether a match happens would depend on which client wrote the subject.
    assert_eq!(fold("cafe\u{0301}"), "cafe");
}

/// The compatibility half of NFKD, which is what makes a ligature or a
/// full-width character typeable on an ordinary keyboard.
#[test]
fn compatibility_forms_fold_to_ascii() {
    assert_eq!(fold("ﬁle"), "file");
    assert_eq!(fold("Ｒeport"), "Report");
}

/// Case survives, because smart-case needs it. See the module docs.
#[test]
fn case_is_preserved() {
    assert_eq!(fold("Quarterly Report"), "Quarterly Report");
}

/// Non-Latin scripts are not damaged by the fold — they have no compatibility
/// decomposition to ASCII and must come through intact.
#[test]
fn non_latin_scripts_survive() {
    assert_eq!(fold("会議の議事録"), "会議の議事録");
    assert_eq!(fold("Привет"), "Привет");
}

/// The map is what keeps a highlight on the character it belongs to when
/// folding changes the length of the string.
#[test]
fn the_map_points_at_the_source_character() {
    let (folded, map) = fold_with_map("ﬁle");
    assert_eq!(folded, "file");
    // Both `f` and `i` came from the single ligature at source index 0.
    assert_eq!(map, vec![0, 0, 1, 2]);
}

/// A combining mark contributes no folded character, so the map stays aligned
/// with what survived rather than with what was typed.
#[test]
fn a_dropped_mark_leaves_no_map_entry() {
    let (folded, map) = fold_with_map("cafe\u{0301}s");
    assert_eq!(folded, "cafes");
    // `s` is source char 5 (c,a,f,e,combining,s) but folded char 4.
    assert_eq!(map, vec![0, 1, 2, 3, 5]);
}

/// The map must always describe exactly the folded string it came with — the
/// property `score::map_positions` relies on when it indexes into it.
#[test]
fn the_map_has_one_entry_per_folded_char() {
    for text in ["café", "ﬁle", "会議", "plain ascii", "", "Ｒeport"] {
        let (folded, map) = fold_with_map(text);
        assert_eq!(
            folded.chars().count(),
            map.len(),
            "map and folded text disagree for {text:?}"
        );
        assert_eq!(fold(text), folded, "fold and fold_with_map disagree");
    }
}

/// A source char index must always be a valid index into the source string,
/// or a highlight would point past the end of the row it decorates.
#[test]
fn map_entries_are_in_range() {
    let text = "ﬁle café 会議";
    let (_, map) = fold_with_map(text);
    let source_chars = text.chars().count();
    for index in map {
        assert!(
            (index as usize) < source_chars,
            "map entry {index} is past the {source_chars} chars of the source"
        );
    }
}

#[test]
fn the_mask_admits_a_subset_and_rejects_a_missing_char() {
    let candidate = char_mask("quarterly report");
    assert!(mask_admits(candidate, char_mask("qrt")));
    assert!(mask_admits(candidate, char_mask("report")));
    // 'z' appears nowhere in the candidate.
    assert!(!mask_admits(candidate, char_mask("zap")));
}

/// The prefilter is only ever allowed to be *looser* than the matcher. Case is
/// where that bites: under smart-case the matcher is case-sensitive, so a
/// case-sensitive mask would reject candidates the matcher would have
/// accepted the other way round.
#[test]
fn the_mask_ignores_case() {
    assert_eq!(char_mask("Report"), char_mask("report"));
    assert!(mask_admits(char_mask("Quarterly Report"), char_mask("QR")));
}

/// Every non-alphanumeric character shares one bit, so a query containing one
/// is admitted by any candidate containing any of them. Loose, never strict.
#[test]
fn non_alphanumeric_chars_share_the_catch_all_bit() {
    assert_eq!(char_mask("会"), char_mask("議"));
    assert!(mask_admits(char_mask("a-b"), char_mask("a.b")));
    // ...and the letters still have to be there.
    assert!(!mask_admits(char_mask("a-b"), char_mask("a.c")));
}

#[test]
fn an_empty_query_mask_admits_everything() {
    assert!(mask_admits(char_mask(""), char_mask("")));
    assert!(mask_admits(char_mask("anything"), char_mask("")));
}
