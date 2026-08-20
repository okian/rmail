//! Tests for reading, watching and editing `keys.toml`.
//!
//! `panic!` in a match arm that cannot happen reads better here than the
//! `unreachable!` dance, and this module is test-only — the same exemption
//! `tag_cli::tests` takes, for the same reason (`clippy.toml` carves out
//! `unwrap`/`expect` in tests but not `panic`).
#![allow(clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use super::*;

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A unique temp path that removes itself on drop. Hand-rolled: this
/// workspace has no `tempfile` dependency (see `storage::tests` for the same
/// pattern).
struct TempKeys(PathBuf);

impl TempKeys {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        Self(std::env::temp_dir().join(format!("rmail-keys-{pid}-{n}.toml")))
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write(&self, contents: &str) {
        std::fs::write(&self.0, contents).unwrap();
    }

    fn remove(&self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

impl Drop for TempKeys {
    fn drop(&mut self) {
        self.remove();
    }
}

fn chord(text: &str) -> Chord {
    Chord::parse(text).unwrap()
}

fn parsed(text: &str) -> Keymap {
    match parse(text, "keys.toml") {
        Ok(keymap) => keymap,
        Err(error) => panic!("should have parsed: {error}"),
    }
}

fn refused(text: &str) -> String {
    match parse(text, "keys.toml") {
        Ok(_) => panic!("should have been refused: {text:?}"),
        Err(error) => error.to_string(),
    }
}

// ---------------------------------------------------------------------------
// parsing
// ---------------------------------------------------------------------------

#[test]
fn the_file_is_a_delta_over_the_built_in_bindings() {
    let keymap = parsed(
        r#"
        [normal]
        "<c-d>" = "cursor.down"
        "#,
    );
    assert_eq!(
        keymap.lookup(Mode::Normal, &chord("<c-d>")),
        Some(Action::CursorDown),
        "the new binding is there"
    );
    assert_eq!(
        keymap.lookup(Mode::Normal, &chord("a")),
        Some(Action::Archive),
        "and every default the file did not mention still is — otherwise \
         customising one key silently opts you out of every binding a later \
         release adds"
    );
}

#[test]
fn an_empty_action_unbinds() {
    let keymap = parsed(
        r#"
        [normal]
        d = ""
        "#,
    );
    assert_eq!(keymap.lookup(Mode::Normal, &chord("d")), None);
    assert_eq!(
        keymap.lookup(Mode::Normal, &chord("a")),
        Some(Action::Archive)
    );
}

#[test]
fn unbinds_are_applied_before_binds_in_the_same_mode() {
    // `q` has to go before `qq` can be added, and which of the two TOML hands
    // back first is an accident of how the chords sort. Applying them in
    // intent order is what makes this file work at all.
    let keymap = parsed(
        r#"
        [normal]
        q = ""
        qq = "quit"
        "#,
    );
    assert_eq!(
        keymap.lookup(Mode::Normal, &chord("qq")),
        Some(Action::Quit)
    );
    assert_eq!(keymap.lookup(Mode::Normal, &chord("q")), None);
}

#[test]
fn every_way_a_file_can_be_wrong_names_the_part_that_is_wrong() {
    for (text, expected) in [
        ("[normal]\nj = \"no.such.action\"\n", "unknown action"),
        ("[nonsense]\nj = \"quit\"\n", "unknown mode"),
        ("[global]\nj = \"quit\"\n", "not configurable"),
        ("[normal]\n\"<nope>\" = \"quit\"\n", "unknown key"),
        ("[normal]\n\"<esc>\" = \"quit\"\n", "cannot be bound"),
        ("[insert]\njk = \"cancel\"\n", "insert"),
        ("[normal]\ng = \"quit\"\n", "could never be typed"),
        ("this is not toml", "not valid TOML"),
        ("[normal]\nj = 3\n", "not valid TOML"),
    ] {
        let error = refused(text);
        assert!(
            error.contains(expected),
            "{text:?} reported {error:?}, which does not mention {expected:?}"
        );
    }
}

#[test]
fn a_missing_file_is_the_default_keymap_and_not_an_error() {
    let keys = TempKeys::new();
    let keymap = match load(keys.path()) {
        Ok(keymap) => keymap,
        Err(error) => panic!("a missing keys.toml is the first-run state: {error}"),
    };
    assert_eq!(keymap, Keymap::defaults());
}

#[test]
fn a_file_that_exists_is_read() {
    let keys = TempKeys::new();
    keys.write("[normal]\n\"<c-d>\" = \"cursor.down\"\n");
    let keymap = load(keys.path()).unwrap();
    assert_eq!(
        keymap.lookup(Mode::Normal, &chord("<c-d>")),
        Some(Action::CursorDown)
    );
}

// ---------------------------------------------------------------------------
// hot reload
// ---------------------------------------------------------------------------

#[test]
fn the_first_poll_delivers_the_file_and_says_nothing_about_it() {
    let keys = TempKeys::new();
    keys.write("[normal]\n\"<c-d>\" = \"cursor.down\"\n");
    let mut source = Source::at(keys.path().to_path_buf());

    let reload = match source.poll() {
        Some(reload) => reload,
        None => panic!("the first poll has to deliver the file — nothing else loads it"),
    };
    assert!(
        !reload.announce,
        "the load at startup is silent; a status line about the keymap would \
         stamp on the boot progress the user is waiting for"
    );
    let keymap = reload.result.unwrap();
    assert_eq!(
        keymap.lookup(Mode::Normal, &chord("<c-d>")),
        Some(Action::CursorDown)
    );

    assert!(source.poll().is_none(), "an unchanged file is not a reload");
}

#[test]
fn an_edit_within_the_same_second_is_still_noticed() {
    // Why the poll compares bytes rather than mtimes: mtime granularity is a
    // second on filesystems this runs on, and trying two bindings in a row is
    // exactly the case that lands inside one.
    let keys = TempKeys::new();
    keys.write("[normal]\nx = \"quit\"\n");
    let mut source = Source::at(keys.path().to_path_buf());
    source.poll();

    keys.write("[normal]\nx = \"help\"\n");
    let reload = match source.poll() {
        Some(reload) => reload,
        None => panic!("an edit saved in the same second was missed"),
    };
    assert!(reload.announce, "a real reload is worth saying");
    assert_eq!(
        reload.result.unwrap().lookup(Mode::Normal, &chord("x")),
        Some(Action::Help)
    );
}

#[test]
fn a_broken_file_is_refused_and_the_previous_bindings_stand() {
    let keys = TempKeys::new();
    keys.write("[normal]\nx = \"quit\"\n");
    let mut source = Source::at(keys.path().to_path_buf());
    source.poll();

    keys.write("[normal]\nx = \"quit\"\n[normal\n");
    let reload = match source.poll() {
        Some(reload) => reload,
        None => panic!("a broken file is a change worth reporting"),
    };
    assert!(
        reload.announce,
        "an error is announced even when a silent load would not be"
    );
    let error = match reload.result {
        Ok(_) => panic!("a broken file must not load"),
        Err(error) => error,
    };
    assert!(error.contains("not valid TOML"), "{error}");
    assert!(
        error.contains(&keys.path().display().to_string()),
        "the message names the file: {error}"
    );
}

#[test]
fn deleting_the_file_restores_the_built_in_bindings() {
    let keys = TempKeys::new();
    keys.write("[normal]\nj = \"quit\"\n");
    let mut source = Source::at(keys.path().to_path_buf());
    assert_eq!(
        source
            .poll()
            .and_then(|reload| reload.result.ok())
            .and_then(|keymap| keymap.lookup(Mode::Normal, &chord("j"))),
        Some(Action::Quit)
    );

    keys.remove();
    let reload = match source.poll() {
        Some(reload) => reload,
        None => panic!("the file disappearing is a change"),
    };
    assert_eq!(
        reload.result.unwrap(),
        Keymap::defaults(),
        "the bindings a deleted file used to define must not freeze in place"
    );
}

// ---------------------------------------------------------------------------
// editing
// ---------------------------------------------------------------------------

#[test]
fn setting_a_binding_in_an_empty_file_creates_the_section() {
    let updated = edit("", Mode::Normal, &chord("<c-d>"), Some(Action::CursorDown)).unwrap();
    assert_eq!(updated, "[normal]\n\"<c-d>\" = \"cursor.down\"\n");
    assert_eq!(
        parsed(&updated).lookup(Mode::Normal, &chord("<c-d>")),
        Some(Action::CursorDown)
    );
}

#[test]
fn setting_a_binding_leaves_the_rest_of_the_file_alone() {
    let existing = "\
# my bindings
[normal]
# down one
j = \"cursor.down\"

[visual]
x = \"message.archive\"
";
    let updated = edit(existing, Mode::Normal, &chord("k"), Some(Action::CursorUp)).unwrap();
    assert!(updated.contains("# my bindings"), "{updated}");
    assert!(
        updated.contains("# down one"),
        "comments survive: {updated}"
    );
    assert!(
        updated.contains("\"k\" = \"cursor.up\""),
        "the new binding landed: {updated}"
    );
    let after = parsed(&updated);
    assert_eq!(
        after.lookup(Mode::Visual, &chord("x")),
        Some(Action::Archive)
    );
    assert_eq!(
        after.lookup(Mode::Normal, &chord("j")),
        Some(Action::CursorDown)
    );
}

#[test]
fn a_new_binding_lands_in_its_own_section_not_the_next_one() {
    let existing = "[normal]\nj = \"cursor.down\"\n\n[visual]\nx = \"message.archive\"\n";
    let updated = edit(existing, Mode::Normal, &chord("k"), Some(Action::CursorUp)).unwrap();
    let normal = updated.find("[normal]").unwrap();
    let visual = updated.find("[visual]").unwrap();
    let added = updated.find("\"k\"").unwrap();
    assert!(
        normal < added && added < visual,
        "the binding was written outside its own section:\n{updated}"
    );
}

#[test]
fn rebinding_replaces_the_line_rather_than_duplicating_the_key() {
    // TOML rejects a duplicate key outright, so getting this wrong does not
    // produce a subtly wrong file — it produces one the TUI refuses.
    let existing = "[normal]\n\"j\" = \"cursor.down\"\n";
    let updated = edit(existing, Mode::Normal, &chord("j"), Some(Action::CursorUp)).unwrap();
    assert_eq!(updated.matches("\"j\"").count(), 1, "{updated}");
    assert_eq!(
        parsed(&updated).lookup(Mode::Normal, &chord("j")),
        Some(Action::CursorUp)
    );
}

#[test]
fn a_binding_written_the_other_way_round_is_still_the_same_key() {
    // `<CR>` and `<enter>` are one key; matching on the text would leave the
    // user with both spellings bound and a file that no longer parses.
    let existing = "[insert]\n\"<CR>\" = \"input.submit\"\n";
    let updated = edit(
        existing,
        Mode::Insert,
        &chord("<enter>"),
        Some(Action::Cancel),
    )
    .unwrap();
    assert_eq!(
        parsed(&updated).lookup(Mode::Insert, &chord("<enter>")),
        Some(Action::Cancel)
    );
    assert!(!updated.contains("input.submit"), "{updated}");
}

#[test]
fn unsetting_removes_the_line_and_restores_the_default() {
    let existing = "# keep me\n[normal]\n\"j\" = \"quit\"\n\"k\" = \"cursor.up\"\n";
    let updated = edit(existing, Mode::Normal, &chord("j"), None).unwrap();
    assert!(updated.contains("# keep me"), "{updated}");
    assert!(!updated.contains("quit"), "{updated}");
    let after = parsed(&updated);
    assert_eq!(
        after.lookup(Mode::Normal, &chord("j")),
        Some(Action::CursorDown),
        "the built-in binding is what comes back"
    );
    assert_eq!(
        after.lookup(Mode::Normal, &chord("k")),
        Some(Action::CursorUp)
    );
}

#[test]
fn unsetting_something_the_file_never_bound_is_refused() {
    let error = match edit(
        "[normal]\nk = \"cursor.up\"\n",
        Mode::Normal,
        &chord("j"),
        None,
    ) {
        Ok(text) => panic!("should have been refused, got:\n{text}"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("not bound"), "{error}");
}

#[test]
fn an_edit_to_a_file_that_does_not_parse_changes_nothing() {
    let error = match edit("[normal\n", Mode::Normal, &chord("j"), Some(Action::Quit)) {
        Ok(text) => panic!("should have been refused, got:\n{text}"),
        Err(error) => error.to_string(),
    };
    assert!(
        error.contains("not valid TOML"),
        "the user's own typo is what they need to hear about: {error}"
    );
}

#[test]
fn an_edit_that_would_produce_an_unusable_keymap_is_refused() {
    // `g` shadows the built-in `gg`. Caught here, before anything is written,
    // rather than reported into a status line the next time the TUI reloads.
    let error = match edit("", Mode::Normal, &chord("g"), Some(Action::Quit)) {
        Ok(text) => panic!("should have been refused, got:\n{text}"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("could never be typed"), "{error}");
}

#[test]
fn a_chord_needing_quoting_survives_a_round_trip() {
    // `<space>z` rather than a bare `<space>`: task 105 made `<space>` a leader,
    // so binding it alone is now legitimately refused — and the property here is
    // about *quoting a chord into TOML and reading it back*, not about which
    // chords happen to be free. The three new spellings are here for the same
    // reason: they are what task 105 added, and a `Display` that did not
    // round-trip through `Chord::parse` would be a binding a user could write and
    // never see take effect.
    for text in [
        "?",
        "<c-p>",
        "gg",
        "<space>z",
        "<home>",
        "<pagedown>",
        "<left><right>",
    ] {
        let updated = edit("", Mode::Normal, &chord(text), Some(Action::Help)).unwrap();
        let after = match parse(&updated, "keys.toml") {
            Ok(keymap) => keymap,
            Err(error) => {
                panic!("{text} produced a file that does not parse ({error}):\n{updated}")
            }
        };
        assert_eq!(
            after.lookup(Mode::Normal, &chord(text)),
            Some(Action::Help),
            "{text} did not survive:\n{updated}"
        );
    }
}

#[test]
fn a_file_far_bigger_than_a_keymap_is_refused_rather_than_read() {
    // `$RMAIL_KEYS` is a setting, so a typo can aim the watcher at something
    // that is not a config file. Reading it in full, once a second, behind a
    // TUI that never says why is the failure this bound exists for.
    let keys = TempKeys::new();
    keys.write(&"# padding\n".repeat(MAX_KEYS_BYTES / 5));

    let error = match load(keys.path()) {
        Ok(_) => panic!("a file past the {MAX_KEYS_BYTES}-byte cap was read in full"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("under"), "{error}");

    // And the watcher reports it rather than silently keeping stale bindings.
    let mut source = Source::at(keys.path().to_path_buf());
    let reload = match source.poll() {
        Some(reload) => reload,
        None => panic!("an unreadable file is still a change"),
    };
    assert!(reload.announce);
    assert!(reload.result.is_err());
}
