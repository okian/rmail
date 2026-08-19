//! The history ring, the redaction rule, and the file behind them.
//!
//! `panic!`/`unwrap` in a branch that cannot happen reads better here than
//! the `unreachable!` dance, and this module is test-only — the same
//! exemption `tui::model::tests` and `tui::manual::tests` take.
#![allow(clippy::panic, clippy::unwrap_used)]

use super::*;

fn history_of(lines: &[&str]) -> History {
    History::new(lines.iter().map(|line| (*line).to_owned()).collect())
}

// ---------------------------------------------------------------------------
// the ring
// ---------------------------------------------------------------------------

#[test]
fn a_recorded_line_is_the_first_one_up_finds() {
    let mut history = History::default();
    assert!(history.record("message archive"));
    assert!(history.record("search invoice"));
    assert_eq!(
        history.matching(""),
        vec!["search invoice", "message archive"],
        "newest first"
    );
}

#[test]
fn a_repeated_line_moves_rather_than_repeating() {
    // Three runs of the same command should cost one `<up>`, not three.
    let mut history = History::default();
    history.record("message archive");
    history.record("search invoice");
    history.record("message archive");
    assert_eq!(
        history.matching(""),
        vec!["message archive", "search invoice"]
    );
}

#[test]
fn a_blank_line_is_not_recorded() {
    let mut history = History::default();
    assert!(!history.record("   "));
    assert!(!history.record(""));
    assert!(history.entries().is_empty());
}

#[test]
fn the_ring_drops_the_oldest_past_the_cap() {
    let mut history = History::default();
    for n in 0..MAX_ENTRIES + 10 {
        history.record(&format!("verb{n}"));
    }
    assert_eq!(history.entries().len(), MAX_ENTRIES);
    assert_eq!(
        history.entries().first().map(String::as_str),
        Some("verb10"),
        "the oldest ten went"
    );
}

#[test]
fn a_history_built_over_the_cap_is_trimmed_on_construction() {
    // A file edited by hand, or written by an older build with a larger cap:
    // the ring's bound has to be this constant, not whatever is on disk.
    let lines: Vec<String> = (0..MAX_ENTRIES + 5).map(|n| format!("verb{n}")).collect();
    let history = History::new(lines);
    assert_eq!(history.entries().len(), MAX_ENTRIES);
    assert_eq!(history.entries().first().map(String::as_str), Some("verb5"));
}

#[test]
fn matching_filters_by_prefix_newest_first() {
    let history = history_of(&["message archive", "search a", "message move", "search b"]);
    assert_eq!(
        history.matching("message"),
        vec!["message move", "message archive"]
    );
    assert_eq!(history.matching("search"), vec!["search b", "search a"]);
    assert!(history.matching("nothing").is_empty());
}

#[test]
fn an_empty_prefix_matches_everything() {
    let history = history_of(&["a", "b"]);
    assert_eq!(history.matching("").len(), 2);
}

// ---------------------------------------------------------------------------
// redaction
// ---------------------------------------------------------------------------

#[test]
fn a_token_line_is_never_recorded() {
    let mut history = History::default();
    assert!(is_secret("token create --name claude"));
    assert!(is_secret(":token revoke 7"));
    assert!(!history.record("token create --name claude"));
    assert!(history.entries().is_empty());
}

#[test]
fn an_account_login_line_is_never_recorded() {
    assert!(is_secret("account login --oauth google --client-id abc 1"));
    // Dots and spaces are one separator everywhere else in this vocabulary,
    // so the rule cannot be sidestepped by typing the other one.
    assert!(is_secret("account.login 1"));
    // ...and the sibling verbs are not swept up with it.
    assert!(!is_secret("account refresh 1"));
}

#[test]
fn any_secret_or_password_flag_takes_the_whole_line_out() {
    assert!(is_secret("webhook add --secret-env WH x"));
    assert!(is_secret("webhook add --secret-keychain=rmail x"));
    assert!(is_secret("account add --password-command 'security find'"));
    assert!(is_secret("verb --PASSWORD x"), "case does not matter");
    assert!(!is_secret("message archive"));
}

#[test]
fn a_range_or_a_bang_does_not_hide_a_secret_line() {
    // The rules read the verb, so anything that can precede one has to be
    // stripped first — otherwise `'<,'>token …` is recorded and `token …`
    // is not, which is a redaction with a documented bypass.
    assert!(is_secret("'<,'>token create"));
    assert!(is_secret("%token create"));
    assert!(is_secret("20token create"));
    assert!(is_secret(":  token create"));
}

#[test]
fn a_word_containing_secret_that_is_not_a_flag_is_not_a_secret() {
    // The rule is about flags. A search for the word is a search.
    assert!(!is_secret("search secret project"));
    assert!(!is_secret("search password reset"));
}

// ---------------------------------------------------------------------------
// the file
// ---------------------------------------------------------------------------

/// A scratch path that does not exist yet, in the process's own temp dir.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("rmail-history-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("command-history")
}

#[test]
fn a_written_history_reads_back_in_the_same_order() {
    let path = scratch("roundtrip");
    let entries = vec!["message archive".to_owned(), "search invoice".to_owned()];
    write(&path, &entries).unwrap();
    assert_eq!(read(&path), entries);
}

#[test]
fn a_missing_file_is_an_empty_history_rather_than_an_error() {
    let path = scratch("missing").with_file_name("nothing-here");
    assert!(read(&path).is_empty());
}

#[test]
fn a_file_longer_than_the_cap_is_trimmed_to_the_newest() {
    let path = scratch("overlong");
    let entries: Vec<String> = (0..MAX_ENTRIES + 7).map(|n| format!("verb{n}")).collect();
    // Written directly, bypassing `write`'s own cap, which is the state a
    // hand-edited file would be in.
    std::fs::write(&path, entries.join("\n")).unwrap();
    let back = read(&path);
    assert_eq!(back.len(), MAX_ENTRIES);
    assert_eq!(back.first().map(String::as_str), Some("verb7"));
}

#[test]
fn write_caps_what_it_stores_at_the_newest_entries() {
    let path = scratch("writecap");
    let entries: Vec<String> = (0..MAX_ENTRIES + 3).map(|n| format!("verb{n}")).collect();
    write(&path, &entries).unwrap();
    let back = read(&path);
    assert_eq!(back.len(), MAX_ENTRIES);
    assert_eq!(back.last().map(String::as_str), Some("verb502"));
    assert_eq!(back.first().map(String::as_str), Some("verb3"));
}

#[test]
fn blank_lines_in_the_file_are_ignored() {
    let path = scratch("blanks");
    std::fs::write(&path, "a\n\n  \nb\n").unwrap();
    assert_eq!(read(&path), vec!["a".to_owned(), "b".to_owned()]);
}

#[cfg(unix)]
#[test]
fn the_file_is_created_private_and_stays_private_across_rewrites() {
    use std::os::unix::fs::PermissionsExt as _;

    let path = scratch("mode");
    write(&path, &["one".to_owned()]).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "created private");

    // The rewrite goes through `write_atomic`, which renames a temp file into
    // place; the mode has to survive that, or every write after the first
    // would publish the file at the umask's default.
    write(&path, &["one".to_owned(), "two".to_owned()]).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "still private after a rewrite");
    assert_eq!(read(&path).len(), 2);
}

#[test]
fn the_path_follows_the_config_file_and_the_env_var_wins() {
    // Same rule `keys.toml` follows, so pointing `$RMAIL_CONFIG` at a second
    // profile moves all three. nextest gives every test its own process, so
    // setting these here cannot reach another one.
    std::env::remove_var(HISTORY_ENV);
    std::env::set_var(rmail_core::CONFIG_ENV, "/tmp/profile-two/config.toml");
    assert_eq!(
        path_from_env(),
        PathBuf::from("/tmp/profile-two").join(HISTORY_FILE)
    );

    std::env::set_var(HISTORY_ENV, "/tmp/elsewhere/lines");
    assert_eq!(
        path_from_env(),
        PathBuf::from("/tmp/elsewhere/lines"),
        "the variable wins outright, config or no config"
    );
}

#[test]
fn a_secret_already_in_the_file_is_dropped_on_load() {
    // The whole list is what gets written back, so a line an older build let
    // in would otherwise be offered by `<up>` *and* curated back into the
    // file for ever.
    let history = History::new(vec![
        "message archive".to_owned(),
        "token create --name claude".to_owned(),
        "account login 1".to_owned(),
        "".to_owned(),
    ]);
    assert_eq!(history.entries(), ["message archive"]);
}

#[test]
fn a_history_file_that_is_not_a_regular_file_reads_as_empty() {
    // A FIFO's `open` blocks before there is anything for the read bound to
    // bound, and this read happens with the terminal already in raw mode.
    let dir = scratch("notafile");
    let parent = dir.parent().unwrap_or(&dir).to_owned();
    assert!(read(&parent).is_empty(), "a directory is not a history");
}

#[cfg(unix)]
#[test]
fn a_history_file_that_arrives_world_readable_is_made_private() {
    use std::os::unix::fs::PermissionsExt as _;

    let path = scratch("relaxed");
    std::fs::write(
        &path,
        "message archive
",
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    write(&path, &["message archive".to_owned()]).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "an existing file's mode is fixed, not kept");
}

#[test]
fn quoting_a_flag_does_not_hide_it() {
    // `command::tokenize` strips quotes, so a rule that read the raw text
    // would have a published bypass.
    assert!(is_secret("account add \"--password-command\" x"));
    assert!(is_secret("verb '--secret-env' x"));
}

#[test]
fn a_verb_typed_in_capitals_is_the_same_verb() {
    // The fallback dispatch matches case-insensitively, so `:TOKEN create`
    // will run the `token` verb the day one exists.
    assert!(is_secret("TOKEN create"));
    assert!(is_secret("Account Login 1"));
}
