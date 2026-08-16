//! Unit tests for the pure helpers in [`super`]. Everything else in that
//! module is transport, which `rmaild/tests/nl_smart_folders.rs` covers against
//! a real server.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use clap::CommandFactory as _;

use super::{slug, truncate};
use crate::Cli;

#[test]
fn a_slug_is_lowercase_hyphenated_and_bounded() {
    // Six words, so "lease renewal" is dropped — the name is a handle for
    // typing, not a summary.
    assert_eq!(
        slug("Anything from the Landlord about the lease renewal"),
        "anything-from-the-landlord-about-the"
    );
    // Punctuation is dropped, not turned into hyphens: `who's` must not
    // become `who-s`.
    assert_eq!(slug("Who's chasing me?"), "whos-chasing-me");
}

#[test]
fn a_description_with_no_word_characters_slugs_to_empty() {
    // Deliberately empty rather than invented: the daemon rejects an empty
    // name, which is the honest error for a description of nothing.
    assert_eq!(slug("!!! ???"), "");
    assert_eq!(slug("   "), "");
}

#[test]
fn truncate_cuts_on_character_boundaries() {
    assert_eq!(truncate("abcdef", 3), "abc…");
    assert_eq!(truncate("abc", 8), "abc");
    // Multi-byte: slicing at a byte offset here would panic.
    assert_eq!(truncate("héllo wörld", 4), "héll…");
}

/// `mail folder new "<nl>"` parses with the description positional and no
/// `--predicate`, which is the form prd.md names.
///
/// Against `clap`'s real parser rather than the struct: a `requires`/`conflicts`
/// attribute that made the headline invocation fail would compile fine and
/// fail only at the terminal.
#[test]
fn folder_new_accepts_a_bare_english_description() {
    let cli = Cli::command();
    let matches = cli
        .clone()
        .try_get_matches_from([
            "mail",
            "folder",
            "new",
            "anything from the landlord about the lease",
            "--account",
            "1",
        ])
        .expect("`mail folder new \"<nl>\" --account 1` must parse");
    let (name, sub) = matches.subcommand().expect("a subcommand");
    assert_eq!(name, "folder");
    let (name, sub) = sub.subcommand().expect("a folder subcommand");
    assert_eq!(name, "new");
    assert_eq!(
        sub.get_one::<String>("description").map(String::as_str),
        Some("anything from the landlord about the lease")
    );
    assert!(sub.get_one::<String>("predicate").is_none());
}

/// A predicate starting with the grammar's own negation is passed through
/// rather than read as an unknown flag — the `allow_hyphen_values` reasoning
/// `mail search` documents, which is easy to lose on a new arg.
#[test]
fn folder_new_accepts_a_negated_predicate() {
    let matches = Cli::command()
        .try_get_matches_from([
            "mail",
            "folder",
            "new",
            "unread stripe mail",
            "--account",
            "1",
            "--predicate",
            "-in:Spam from:stripe",
        ])
        .expect("a `-`-leading predicate must parse");
    let sub = matches
        .subcommand()
        .and_then(|(_, m)| m.subcommand())
        .map(|(_, m)| m)
        .expect("folder new");
    assert_eq!(
        sub.get_one::<String>("predicate").map(String::as_str),
        Some("-in:Spam from:stripe")
    );
}

/// `mail search --nl` requires `--account`, because the plan cache and the AI
/// budget that admits the call are both per account. Enforced by `clap` so the
/// user learns it before a round trip.
#[test]
fn search_nl_requires_an_account() {
    let err = Cli::command()
        .try_get_matches_from(["mail", "search", "--nl", "who owes me money"])
        .expect_err("--nl without --account must be rejected");
    assert_eq!(
        err.kind(),
        clap::error::ErrorKind::MissingRequiredArgument,
        "expected a missing-argument error, got: {err}"
    );
}

/// `--plan-only` and `--refresh` are meaningless without `--nl`, and saying so
/// at parse time beats silently ignoring them.
#[test]
fn plan_only_and_refresh_require_nl() {
    for flag in ["--plan-only", "--refresh"] {
        let result = Cli::command().try_get_matches_from(["mail", "search", "lease", flag]);
        let err = result
            .err()
            .unwrap_or_else(|| panic!("{flag} was accepted without --nl, where it does nothing"));
        assert_eq!(
            err.kind(),
            clap::error::ErrorKind::MissingRequiredArgument,
            "expected a missing-argument error for {flag}, got: {err}"
        );
    }
}
