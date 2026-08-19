//! `panic!` in a branch that cannot happen reads better here than the
//! `unreachable!` dance, and this module is test-only — the same exemption
//! `tui::model::tests` and `tui::whichkey::tests` take.
#![allow(clippy::panic)]

use rmail_core::keymap::{Chord, Keymap};

use super::*;

fn chord(text: &str) -> Chord {
    match Chord::parse(text) {
        Ok(chord) => chord,
        Err(error) => panic!("{text:?} should parse: {error}"),
    }
}

fn bind(keymap: &mut Keymap, mode: Mode, text: &str, action: Action) {
    match keymap.bind(mode, chord(text), action) {
        Ok(()) => {}
        Err(error) => panic!("binding {text:?} in {} failed: {error}", mode.id()),
    }
}

fn bindings(rows: &[Row]) -> Vec<(String, Action)> {
    rows.iter()
        .filter_map(|row| match row {
            Row::Binding { action, chords, .. } => Some((chords.clone(), *action)),
            Row::Group(_) => None,
        })
        .collect()
}

fn groups(rows: &[Row]) -> Vec<String> {
    rows.iter()
        .filter_map(|row| match row {
            Row::Group(label) => Some(label.clone()),
            Row::Binding { .. } => None,
        })
        .collect()
}

#[test]
fn an_action_with_no_chord_in_this_mode_is_skipped_entirely() {
    let mut keymap = Keymap::empty();
    bind(&mut keymap, Mode::Normal, "j", Action::CursorDown);
    // Bound in a different mode only.
    bind(&mut keymap, Mode::Viewer, "k", Action::CursorUp);

    let rows = rows(Mode::Normal, "", &keymap);
    let bound = bindings(&rows);
    assert_eq!(bound, [("j".to_owned(), Action::CursorDown)]);
}

#[test]
fn every_chord_reaching_the_same_action_is_joined_on_one_row() {
    let mut keymap = Keymap::empty();
    bind(&mut keymap, Mode::Normal, "j", Action::CursorDown);
    bind(&mut keymap, Mode::Normal, "<down>", Action::CursorDown);

    let rows = rows(Mode::Normal, "", &keymap);
    let bound = bindings(&rows);
    assert_eq!(bound, [("j / <down>".to_owned(), Action::CursorDown)]);
}

#[test]
fn two_actions_sharing_a_first_segment_get_one_group_header() {
    let mut keymap = Keymap::empty();
    bind(&mut keymap, Mode::Normal, "za", Action::AiPanel);
    bind(&mut keymap, Mode::Normal, "zb", Action::AiQuick);

    let rows = rows(Mode::Normal, "", &keymap);
    assert_eq!(groups(&rows), ["ai"]);
    assert_eq!(
        bindings(&rows),
        [
            ("za".to_owned(), Action::AiPanel),
            ("zb".to_owned(), Action::AiQuick),
        ],
        "members keep Action::ALL's own relative order within the group"
    );
}

#[test]
fn a_solitary_action_gets_no_group_header() {
    let mut keymap = Keymap::empty();
    bind(&mut keymap, Mode::Normal, "j", Action::CursorDown);

    let rows = rows(Mode::Normal, "", &keymap);
    assert!(groups(&rows).is_empty(), "{rows:?}");
    assert_eq!(bindings(&rows), [("j".to_owned(), Action::CursorDown)]);
}

#[test]
fn groups_are_ordered_alphabetically_by_their_bucket_key_not_by_action_all() {
    // `Action::ALL` declares `cursor.*` well before `ai.*`; the derived
    // group order must not just mirror that declaration order.
    let mut keymap = Keymap::empty();
    bind(&mut keymap, Mode::Normal, "j", Action::CursorDown);
    bind(&mut keymap, Mode::Normal, "k", Action::CursorUp);
    bind(&mut keymap, Mode::Normal, "za", Action::AiPanel);
    bind(&mut keymap, Mode::Normal, "zb", Action::AiQuick);

    let rows = rows(Mode::Normal, "", &keymap);
    assert_eq!(groups(&rows), ["ai", "cursor"]);
}

#[test]
fn a_bare_action_and_its_dotted_relative_share_a_group() {
    // The exact pairing `common_id_prefix`'s own doc names: `search` (bare)
    // and `search.explain` share the segment `search`.
    let mut keymap = Keymap::empty();
    bind(&mut keymap, Mode::Normal, "/", Action::SearchOpen);
    bind(&mut keymap, Mode::Normal, "x", Action::SearchExplain);

    let rows = rows(Mode::Normal, "", &keymap);
    assert_eq!(groups(&rows), ["search"]);
    assert_eq!(
        bindings(&rows),
        [
            ("/".to_owned(), Action::SearchOpen),
            ("x".to_owned(), Action::SearchExplain),
        ]
    );
}

#[test]
fn an_empty_filter_shows_everything_bound() {
    let mut keymap = Keymap::empty();
    bind(&mut keymap, Mode::Normal, "j", Action::CursorDown);
    bind(&mut keymap, Mode::Normal, "a", Action::Archive);

    assert_eq!(rows(Mode::Normal, "", &keymap).len(), 2);
    assert_eq!(rows(Mode::Normal, "   ", &keymap).len(), 2, "trimmed too");
}

#[test]
fn a_filter_matching_the_chord_keeps_only_that_row() {
    let mut keymap = Keymap::empty();
    bind(&mut keymap, Mode::Normal, "j", Action::CursorDown);
    bind(&mut keymap, Mode::Normal, "a", Action::Archive);

    let rows = rows(Mode::Normal, "j", &keymap);
    assert_eq!(bindings(&rows), [("j".to_owned(), Action::CursorDown)]);
}

#[test]
fn a_filter_matching_the_action_id_keeps_only_that_row() {
    let mut keymap = Keymap::empty();
    bind(&mut keymap, Mode::Normal, "j", Action::CursorDown);
    bind(&mut keymap, Mode::Normal, "a", Action::Archive);

    let rows = rows(Mode::Normal, "archive", &keymap);
    assert_eq!(bindings(&rows), [("a".to_owned(), Action::Archive)]);
}

#[test]
fn a_filter_matching_only_the_description_still_finds_the_row() {
    let mut keymap = Keymap::empty();
    bind(&mut keymap, Mode::Normal, "j", Action::CursorDown);
    // Archive's own description is "archive"; delete's is "delete (asks
    // first — this expunges)" — "expunges" appears nowhere in its id.
    bind(&mut keymap, Mode::Normal, "d", Action::Delete);

    let rows = rows(Mode::Normal, "expunges", &keymap);
    assert_eq!(bindings(&rows), [("d".to_owned(), Action::Delete)]);
}

#[test]
fn a_filter_matching_nothing_leaves_no_rows_at_all() {
    let mut keymap = Keymap::empty();
    bind(&mut keymap, Mode::Normal, "j", Action::CursorDown);

    assert!(rows(Mode::Normal, "no such text anywhere", &keymap).is_empty());
}

#[test]
fn filtering_is_case_insensitive() {
    let mut keymap = Keymap::empty();
    bind(&mut keymap, Mode::Normal, "a", Action::Archive);

    assert_eq!(rows(Mode::Normal, "ARCHIVE", &keymap).len(), 1);
}

#[test]
fn a_filter_that_empties_a_group_to_one_member_drops_its_header() {
    // Filtering acts on individual actions; a group is a property of what
    // survives the filter, not of the unfiltered set it was drawn from.
    let mut keymap = Keymap::empty();
    bind(&mut keymap, Mode::Normal, "za", Action::AiPanel);
    bind(&mut keymap, Mode::Normal, "zb", Action::AiQuick);

    let rows = rows(Mode::Normal, "panel", &keymap);
    assert!(groups(&rows).is_empty(), "{rows:?}");
    assert_eq!(bindings(&rows), [("za".to_owned(), Action::AiPanel)]);
}

#[test]
fn binding_count_counts_rows_not_groups() {
    let mut keymap = Keymap::empty();
    bind(&mut keymap, Mode::Normal, "za", Action::AiPanel);
    bind(&mut keymap, Mode::Normal, "zb", Action::AiQuick);
    let pane = HelpPane::new(Mode::Normal, &keymap);

    assert_eq!(pane.rows.len(), 3, "one group header plus two bindings");
    assert_eq!(binding_count(&pane), 2);
}

#[test]
fn selected_follows_the_cursor_past_group_headers() {
    let mut keymap = Keymap::empty();
    bind(&mut keymap, Mode::Normal, "za", Action::AiPanel);
    bind(&mut keymap, Mode::Normal, "zb", Action::AiQuick);
    let mut pane = HelpPane::new(Mode::Normal, &keymap);

    assert_eq!(
        selected(&pane),
        Some(Action::AiPanel),
        "cursor 0 is the first binding, not the header at rows[0]"
    );
    pane.cursor = 1;
    assert_eq!(selected(&pane), Some(Action::AiQuick));
}

#[test]
fn selected_is_none_when_there_are_no_rows_at_all() {
    let pane = HelpPane::new(Mode::Normal, &Keymap::empty());
    assert_eq!(pane.rows.len(), 0);
    assert_eq!(selected(&pane), None);
}

#[test]
fn help_pane_new_starts_unfiltered_and_browsing_at_the_top() {
    let mut keymap = Keymap::empty();
    bind(&mut keymap, Mode::Normal, "j", Action::CursorDown);
    let pane = HelpPane::new(Mode::Normal, &keymap);

    assert_eq!(pane.mode, Mode::Normal);
    assert_eq!(pane.filter, "");
    assert!(!pane.editing);
    assert_eq!(pane.cursor, 0);
    assert_eq!(pane.rows.len(), 1);
}
