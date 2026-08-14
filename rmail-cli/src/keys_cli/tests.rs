//! Tests for `mail keys`.
//!
//! These call the verbs directly with a path rather than going through
//! `$RMAIL_KEYS`: environment variables are process-global, and the test
//! binary runs these alongside everything else in it.
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

struct TempKeys(PathBuf);

impl TempKeys {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        Self(std::env::temp_dir().join(format!("rmail-keys-cli-{pid}-{n}.toml")))
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn read(&self) -> String {
        std::fs::read_to_string(&self.0).unwrap_or_default()
    }
}

impl Drop for TempKeys {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn chord(text: &str) -> Chord {
    Chord::parse(text).unwrap()
}

#[test]
fn set_creates_the_file_and_the_tui_would_load_the_binding() {
    let keys = TempKeys::new();
    set(keys.path(), "normal", "<c-d>", Some("cursor.down")).unwrap();

    assert!(keys.read().contains("cursor.down"), "{}", keys.read());
    let keymap = file::load(keys.path()).unwrap();
    assert_eq!(
        keymap.lookup(Mode::Normal, &chord("<c-d>")),
        Some(Action::CursorDown),
        "what `mail keys set` writes is what the TUI reads"
    );
}

#[test]
fn set_then_unset_returns_to_the_built_in_binding() {
    let keys = TempKeys::new();
    set(keys.path(), "normal", "j", Some("quit")).unwrap();
    assert_eq!(
        file::load(keys.path())
            .unwrap()
            .lookup(Mode::Normal, &chord("j")),
        Some(Action::Quit)
    );

    set(keys.path(), "normal", "j", None).unwrap();
    assert_eq!(
        file::load(keys.path())
            .unwrap()
            .lookup(Mode::Normal, &chord("j")),
        Some(Action::CursorDown),
        "unsetting restores the default rather than leaving the key dead"
    );
}

#[test]
fn a_rejected_binding_leaves_the_file_untouched() {
    let keys = TempKeys::new();
    set(keys.path(), "normal", "x", Some("message.archive")).unwrap();
    let before = keys.read();

    for (mode, chord, action, expected) in [
        ("normal", "<esc>", Some("quit"), "cannot be bound"),
        ("normal", "g", Some("quit"), "could never be typed"),
        ("normal", "y", Some("no.such.action"), "unknown action"),
        ("nonsense", "y", Some("quit"), "unknown mode"),
        ("normal", "<nope>", Some("quit"), "unknown key"),
        ("insert", "jk", Some("cancel"), "insert"),
        ("normal", "z", None, "not bound"),
    ] {
        let error = match set(keys.path(), mode, chord, action) {
            Ok(()) => panic!("{mode} {chord} {action:?} should have been refused"),
            Err(error) => format!("{error:#}"),
        };
        assert!(
            error.contains(expected),
            "{chord} reported {error:?}, which does not mention {expected:?}"
        );
        assert_eq!(
            keys.read(),
            before,
            "{chord} was refused but the file changed anyway"
        );
    }
}

#[test]
fn a_write_leaves_no_temp_file_behind() {
    let keys = TempKeys::new();
    set(keys.path(), "normal", "x", Some("quit")).unwrap();
    let temp = keys
        .path()
        .with_extension(format!("toml.tmp.{}", std::process::id()));
    assert!(
        !temp.exists(),
        "the temp file used for the atomic rename is still there"
    );
}

#[test]
fn set_does_not_replace_a_file_it_could_not_read() {
    // A read error that is not "no such file" must not be treated as an empty
    // file: the write would then replace the user's real, unread bindings
    // with one line. A directory at the path is the readable-error case that
    // does not need root to arrange.
    let keys = TempKeys::new();
    std::fs::create_dir(keys.path()).unwrap();
    let result = set(keys.path(), "normal", "x", Some("quit"));
    let _ = std::fs::remove_dir(keys.path());
    assert!(result.is_err(), "a directory is not an empty keys.toml");
}

#[test]
fn list_prints_the_effective_bindings_including_inherited_ones() {
    let keys = TempKeys::new();
    set(keys.path(), "normal", "<c-d>", Some("cursor.down")).unwrap();
    // Exercised for its error paths and its refusal to panic on an odd mode;
    // what it prints is checked by the help overlay's own view test, which
    // reads the same `chords_for`.
    list(keys.path(), None).unwrap();
    list(keys.path(), Some("viewer")).unwrap();
    let error = match list(keys.path(), Some("nonsense")) {
        Ok(()) => panic!("an unknown mode should be refused"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("unknown mode"), "{error}");
}

#[test]
fn every_action_id_the_help_names_is_one_keys_set_accepts() {
    // `mail keys actions` prints these and `mail keys set` parses them; a
    // registry that disagreed with itself would print ids that cannot be
    // bound.
    let keys = TempKeys::new();
    for action in Action::ALL {
        assert_eq!(Action::from_id(action.id()), Some(*action));
    }
    set(keys.path(), "normal", "<c-y>", Some(Action::Reply.id())).unwrap();
    assert_eq!(
        file::load(keys.path())
            .unwrap()
            .lookup(Mode::Normal, &chord("<c-y>")),
        Some(Action::Reply)
    );
}
