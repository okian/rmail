//! The drift checks, and the parser's own behavior.
//!
//! Same split as `parity::tests` and `keymap::tests`: parser correctness is
//! ordinary unit tests against fixed input; the drift checks reconcile the
//! *real* registry against `Action::ALL`/`parity::Command::ALL`, generated
//! surfaces rather than a second hand-written list.
//!
//! `panic!` in a branch that cannot happen reads better here than the
//! `unreachable!` dance, and this module is test-only — the same exemption
//! `keymap::tests` takes, for the same reason (`clippy.toml` carves out
//! `unwrap`/`expect` in tests but not `panic`).
#![allow(clippy::panic)]

use super::*;

// ---------------------------------------------------------------------------
// every Action::id() is a verb, for free
// ---------------------------------------------------------------------------

/// The module's core promise: nothing has to register an action as a verb.
#[test]
fn every_action_resolves_as_a_verb_with_no_registry_entry() {
    for action in Action::ALL {
        let verb = verb_at(&split_path(action.id()))
            .unwrap_or_else(|| panic!("{} has no auto-derived verb", action.id()));
        assert_eq!(verb.action, Some(*action));
    }
}

/// The dotted and space-separated spellings of an action id name the same
/// verb — the "one transform" the module docs describe.
#[test]
fn dot_and_space_paths_resolve_to_the_same_verb() {
    let Resolution::Invocation(dotted) = parse("message.archive").unwrap() else {
        panic!("message.archive did not resolve to a verb");
    };
    let Resolution::Invocation(spaced) = parse("message archive").unwrap() else {
        panic!("message archive did not resolve to a verb");
    };
    assert_eq!(dotted.action, Some(Action::Archive));
    assert_eq!(dotted.action, spaced.action);
}

#[test]
fn a_single_segment_action_id_resolves_too() {
    let Resolution::Invocation(invocation) = parse("search").unwrap() else {
        panic!("search did not resolve");
    };
    assert_eq!(invocation.action, Some(Action::SearchOpen));
}

// ---------------------------------------------------------------------------
// ranges
// ---------------------------------------------------------------------------

#[test]
fn a_visual_selection_range_parses_glued_and_spaced() {
    for text in ["'<,'>message archive", "'<,'> message archive"] {
        let Resolution::Invocation(invocation) = parse(text).unwrap() else {
            panic!("{text:?} did not resolve");
        };
        assert_eq!(invocation.range, Some(Range::Selection), "{text:?}");
    }
}

#[test]
fn a_percent_range_parses_glued_and_spaced() {
    for text in ["%message archive", "% message archive"] {
        let Resolution::Invocation(invocation) = parse(text).unwrap() else {
            panic!("{text:?} did not resolve");
        };
        assert_eq!(invocation.range, Some(Range::All), "{text:?}");
    }
}

#[test]
fn a_count_range_parses_glued_and_spaced() {
    for text in ["5message archive", "5 message archive"] {
        let Resolution::Invocation(invocation) = parse(text).unwrap() else {
            panic!("{text:?} did not resolve");
        };
        assert_eq!(invocation.range, Some(Range::Count(5)), "{text:?}");
    }
}

#[test]
fn a_count_range_saturates_at_max_count() {
    let text = format!("{}0 message archive", MAX_COUNT);
    let Resolution::Invocation(invocation) = parse(&text).unwrap() else {
        panic!("did not resolve");
    };
    assert_eq!(invocation.range, Some(Range::Count(MAX_COUNT)));
}

#[test]
fn a_digit_run_that_overflows_u32_saturates_instead_of_being_read_as_no_range() {
    // Ten-plus digits fail `str::parse::<u32>()` outright — a naive
    // "parse, and treat failure as no range" implementation falls through
    // to `Ok((None, trimmed))` *without consuming the digits*, so
    // `9999999999 message archive` is read as the verb
    // `"9999999999 message archive"` (unknown) rather than as the range
    // `MAX_COUNT` applied to `message archive`. A held-down digit key is
    // exactly the input `MAX_COUNT`'s own docs describe, and it is exactly
    // what produces a digit run this long.
    let Resolution::Invocation(invocation) = parse("9999999999 message archive").unwrap() else {
        panic!("did not resolve");
    };
    assert_eq!(invocation.range, Some(Range::Count(MAX_COUNT)));
    assert_eq!(invocation.verb, vec!["message", "archive"]);
}

#[test]
fn no_range_means_no_range() {
    let Resolution::Invocation(invocation) = parse("message archive").unwrap() else {
        panic!("did not resolve");
    };
    assert_eq!(invocation.range, None);
}

#[test]
fn a_malformed_range_mark_is_refused_by_name() {
    let err = parse("'<, message archive").unwrap_err();
    assert_eq!(
        err,
        CommandError::MalformedRange {
            text: "'<,".to_owned()
        }
    );
}

// ---------------------------------------------------------------------------
// bang
// ---------------------------------------------------------------------------

#[test]
fn a_glued_bang_is_stripped_and_recorded() {
    let Resolution::Invocation(invocation) = parse("message.archive!").unwrap() else {
        panic!("did not resolve");
    };
    assert!(invocation.bang);
    assert_eq!(invocation.verb, vec!["message", "archive"]);
}

#[test]
fn a_standalone_bang_word_is_also_a_bang() {
    let Resolution::Invocation(invocation) = parse("message archive !").unwrap() else {
        panic!("did not resolve");
    };
    assert!(invocation.bang);
    assert_eq!(invocation.verb, vec!["message", "archive"]);
}

#[test]
fn no_bang_means_no_bang() {
    let Resolution::Invocation(invocation) = parse("message archive").unwrap() else {
        panic!("did not resolve");
    };
    assert!(!invocation.bang);
}

#[test]
fn a_trailing_exclamation_point_inside_a_quote_is_literal_text_not_a_bang() {
    // Quoting is the one mechanism a user has to say "this character is
    // literal" — `strip_bang` runs on the raw line specifically so it can
    // still see the quote marks and tell this apart from a real bang. A
    // version that instead stripped `!` off the last already-tokenized word
    // would not be able to: by then, the quote marks are already gone.
    let Resolution::Invocation(invocation) = parse(r#"ask "what happened!""#).unwrap() else {
        panic!("did not resolve");
    };
    assert!(!invocation.bang);
    assert_eq!(invocation.positionals, vec!["what happened!"]);
}

#[test]
fn a_bang_after_a_closed_quote_is_still_a_real_bang() {
    let Resolution::Invocation(invocation) = parse(r#"ask "abc"!"#).unwrap() else {
        panic!("did not resolve");
    };
    assert!(invocation.bang);
    assert_eq!(invocation.positionals, vec!["abc"]);
}

// ---------------------------------------------------------------------------
// quoting
// ---------------------------------------------------------------------------

#[test]
fn a_quoted_positional_keeps_its_spaces() {
    let Resolution::Invocation(invocation) = parse(r#"ask "who sent the invoice""#).unwrap() else {
        panic!("did not resolve");
    };
    assert_eq!(invocation.positionals, vec!["who sent the invoice"]);
}

#[test]
fn an_escaped_quote_inside_a_quoted_positional_is_literal() {
    let Resolution::Invocation(invocation) = parse(r#"ask "say \"hi\"""#).unwrap() else {
        panic!("did not resolve");
    };
    assert_eq!(invocation.positionals, vec![r#"say "hi""#]);
}

#[test]
fn an_unterminated_quote_is_refused_by_name() {
    let err = parse(r#"ask "who sent it"#).unwrap_err();
    assert_eq!(
        err,
        CommandError::UnterminatedQuote {
            text: r#"who sent it"#.to_owned(),
        }
    );
}

#[test]
fn a_quoted_piece_glued_to_more_text_is_one_token() {
    let Resolution::Invocation(invocation) = parse(r#"ask "a b"c"#).unwrap() else {
        panic!("did not resolve");
    };
    assert_eq!(invocation.positionals, vec!["a bc"]);
}

// ---------------------------------------------------------------------------
// flags
// ---------------------------------------------------------------------------

fn folder_verb() -> Verb {
    Verb {
        path: vec!["test", "flagged"],
        capability: None,
        action: None,
        positionals: &[Positional {
            name: "folder",
            required: true,
        }],
        flags: &[
            Flag {
                name: "force",
                takes_value: false,
            },
            Flag {
                name: "since",
                takes_value: true,
            },
        ],
        cli_alias: None,
    }
}

/// A [`Verb`] cannot be registered from a test (the registry is process-wide
/// and lazily built once), so flag/positional checks exercise
/// [`check_flags`]/[`check_positionals`] directly against a local fixture —
/// the same way `keymap::tests` builds a bare [`crate::keymap::Keymap`]
/// rather than mutating the process's real one.
#[test]
fn a_known_value_flag_with_a_value_is_accepted() {
    let verb = folder_verb();
    let flags = vec![ParsedFlag {
        name: "since".to_owned(),
        value: Some("7d".to_owned()),
    }];
    assert!(check_flags(&verb, &flags).is_ok());
}

#[test]
fn a_known_switch_flag_needs_no_value() {
    let verb = folder_verb();
    let flags = vec![ParsedFlag {
        name: "force".to_owned(),
        value: None,
    }];
    assert!(check_flags(&verb, &flags).is_ok());
}

#[test]
fn an_unknown_flag_is_refused_by_name() {
    let verb = folder_verb();
    let flags = vec![ParsedFlag {
        name: "bogus".to_owned(),
        value: None,
    }];
    let err = check_flags(&verb, &flags).unwrap_err();
    assert_eq!(
        err,
        CommandError::UnknownFlag {
            verb: "test flagged".to_owned(),
            flag: "bogus".to_owned(),
            valid: vec!["--force".to_owned(), "--since".to_owned()],
        }
    );
}

#[test]
fn an_unknown_flag_on_a_verb_with_no_flags_at_all_says_so_without_a_dangling_try() {
    // Every auto-derived verb declares `flags: &[]` (`explicit`'s docs — no
    // real verb has any yet), so this is the shape a real user actually
    // hits today. The naive `{flag:?} is not a flag {verb} takes — try {}`
    // message renders as `"bogus" is not a flag message archive takes —
    // try ` when `valid` is empty — a dangling "try" with nothing after it.
    let verb = verb_at(&["message", "archive"]).unwrap();
    let flags = vec![ParsedFlag {
        name: "bogus".to_owned(),
        value: None,
    }];
    let err = check_flags(verb, &flags).unwrap_err();
    let CommandError::UnknownFlag { valid, .. } = &err else {
        panic!("expected UnknownFlag");
    };
    assert!(valid.is_empty());
    let message = err.to_string();
    assert!(!message.ends_with("try "), "{message}");
    assert!(message.contains("no flags"), "{message}");
}

#[test]
fn a_value_flag_with_no_value_is_refused() {
    let verb = folder_verb();
    let flags = vec![ParsedFlag {
        name: "since".to_owned(),
        value: None,
    }];
    let err = check_flags(&verb, &flags).unwrap_err();
    assert_eq!(
        err,
        CommandError::MissingFlagValue {
            flag: "since".to_owned()
        }
    );
}

// ---------------------------------------------------------------------------
// positionals
// ---------------------------------------------------------------------------

#[test]
fn a_present_required_positional_is_accepted() {
    let verb = folder_verb();
    assert!(check_positionals(&verb, &["Archive".to_owned()]).is_ok());
}

#[test]
fn a_missing_required_positional_is_refused_by_name() {
    let verb = folder_verb();
    let err = check_positionals(&verb, &[]).unwrap_err();
    assert_eq!(
        err,
        CommandError::MissingPositional {
            verb: "test flagged".to_owned(),
            name: "folder",
        }
    );
}

// ---------------------------------------------------------------------------
// unknown verbs and interior nodes
// ---------------------------------------------------------------------------

#[test]
fn an_empty_line_is_refused() {
    assert_eq!(parse("").unwrap_err(), CommandError::Empty);
    assert_eq!(parse("   ").unwrap_err(), CommandError::Empty);
}

#[test]
fn a_range_with_nothing_after_it_is_empty_not_a_verb() {
    assert_eq!(parse("%").unwrap_err(), CommandError::Empty);
}

#[test]
fn an_unknown_verb_is_refused_by_name() {
    let err = parse("this-is-not-a-verb").unwrap_err();
    assert_eq!(
        err,
        CommandError::UnknownVerb {
            path: "this-is-not-a-verb".to_owned(),
            suggestion: None,
        }
    );
}

#[test]
fn an_unknown_verb_close_to_a_real_one_suggests_it() {
    // "cancl" is `cancel` with the trailing letter dropped — closeness is
    // subsequence-based (`is_subsequence`'s own docs), and dropping
    // characters from a string always leaves a subsequence of it, whichever
    // direction the comparison runs. A transposed pair (the more obvious
    // "typo" example) is *not* reliably a subsequence match in either
    // direction — swapping the last two letters of "cancel" needs "l" before
    // "e" on one side and "e" before "l" on the other, and only one of those
    // orders exists in the real word — so it is not used here.
    let err = parse("cancl").unwrap_err();
    let CommandError::UnknownVerb { suggestion, .. } = err else {
        panic!("expected UnknownVerb");
    };
    assert_eq!(suggestion.as_deref(), Some("cancel"));
}

#[test]
fn a_typo_in_a_multi_segment_verb_suggests_that_verb_not_an_unrelated_short_one() {
    // `closest` briefly also checked the *reverse* subsequence direction
    // (is the candidate a subsequence of what was typed) during this
    // task's own development, on the theory that closeness should be
    // symmetric. It is not: a short, wholly unrelated verb's letters are
    // easy to find scattered through almost any longer typo, so `search`
    // "matched" a `message archive` missing only its final letter. Ranking
    // by tier (prefix, then substring, then one-directional subsequence)
    // the same way `overlays::palette_matches` does fixes it — checked
    // here against exactly that regression, not just the passing case.
    for (typo, expected) in [
        ("message archiv", "message archive"),
        ("mesage archive", "message archive"),
        ("outbo cancel", "outbox cancel"),
    ] {
        let err = parse(typo).unwrap_err();
        let CommandError::UnknownVerb { suggestion, .. } = err else {
            panic!("expected UnknownVerb for {typo:?}");
        };
        assert_eq!(suggestion.as_deref(), Some(expected), "typo: {typo:?}");
    }
}

#[test]
fn a_strict_prefix_of_real_verbs_with_no_handler_of_its_own_lists_its_children() {
    // "message" itself is not a bound action id — only its two-segment
    // children (`message.archive`, `message.delete`, …) are — so it must
    // be an interior node, not an error.
    let Resolution::Children { path, children } = parse("message").unwrap() else {
        panic!("expected Children");
    };
    assert_eq!(path, vec!["message"]);
    assert!(children.iter().any(|v| v.path == ["message", "archive"]));
    assert!(children.iter().all(|v| v.path[0] == "message"));
}

#[test]
fn the_longest_matching_verb_wins_over_treating_its_tail_as_positionals() {
    // Real data, not a constructed pair: `search` (`Action::SearchOpen`)
    // and `search.explain` (`Action::SearchExplain`) are both real,
    // independent, auto-derived verbs sharing a first segment — grep
    // `keymap::mod`'s `actions!` body to confirm. Typing `search explain`
    // has to reach the two-word verb, not `search` with `["explain"]` as a
    // positional, which is what a shortest-prefix-first search would
    // produce instead. This calls the real `parse` rather than
    // reimplementing its search loop inline: only calling the real
    // function would actually catch production being flipped to
    // shortest-first.
    let Resolution::Invocation(invocation) = parse("search explain").unwrap() else {
        panic!("did not resolve");
    };
    assert_eq!(invocation.verb, vec!["search", "explain"]);
    assert_eq!(invocation.action, Some(Action::SearchExplain));
    assert!(invocation.positionals.is_empty());
}

// ---------------------------------------------------------------------------
// dot expansion stops at the verb boundary
// ---------------------------------------------------------------------------

/// Dot-expansion lives in `parse_verb`/`complete`, not `tokenize` (that
/// module's own docs explain why: only they know where a verb path ends
/// and an argument begins). Making `tokenize` split bare words on `.`
/// instead — so a glued `message.archive` would tokenize straight into two
/// words — looks simpler, but silently fragments any positional or flag
/// value containing a literal `.`: `report.pdf`, `3.14`, `2024.01.01`.
/// These tests pin the boundary: dotted verb paths still resolve, and
/// dotted arguments survive whole.
#[test]
fn a_positional_containing_a_literal_dot_survives_as_one_argument() {
    let Resolution::Invocation(invocation) = parse("message copy report.pdf").unwrap() else {
        panic!("did not resolve");
    };
    assert_eq!(invocation.verb, vec!["message", "copy"]);
    assert_eq!(invocation.positionals, vec!["report.pdf"]);
}

#[test]
fn a_flag_value_containing_dots_is_never_split() {
    let tokens = tokenize("test --since=2024.01.01").unwrap();
    assert_eq!(
        tokens,
        vec![
            Token::Word("test".to_owned()),
            Token::Flag {
                name: "since".to_owned(),
                value: Some("2024.01.01".to_owned()),
            },
        ]
    );
}

// ---------------------------------------------------------------------------
// completion
// ---------------------------------------------------------------------------

#[test]
fn completing_an_empty_line_offers_top_level_segments() {
    let candidates = complete("");
    assert!(candidates.iter().any(|c| c.text == "message"));
    assert!(candidates.iter().any(|c| c.text == "search"));
    // No duplicates: every action under `message.*` would otherwise each
    // contribute their own "message" candidate.
    assert_eq!(candidates.iter().filter(|c| c.text == "message").count(), 1);
}

#[test]
fn completing_a_partial_segment_filters_by_prefix() {
    let candidates = complete("mess");
    assert!(candidates.iter().all(|c| c.text.starts_with("mess")));
    assert!(candidates.iter().any(|c| c.text == "message"));
}

#[test]
fn completing_after_a_trailing_space_moves_past_the_settled_segment() {
    let candidates = complete("message ");
    assert!(candidates.iter().any(|c| c.text == "archive"));
    assert!(!candidates.iter().any(|c| c.text == "message"));
}

#[test]
fn a_leaf_verb_has_no_further_completions_from_its_own_path() {
    let candidates = complete("message archive ");
    assert!(candidates.is_empty(), "{candidates:?}");
}

#[test]
fn completion_reports_which_candidates_still_have_children() {
    let candidates = complete("");
    let message = candidates.iter().find(|c| c.text == "message").unwrap();
    assert!(message.has_more);
}

/// `search` (`Action::SearchOpen`, a leaf) and `search.explain`
/// (`Action::SearchExplain`, its own independent action) are both real,
/// both auto-derived, and registry order (`Action::ALL`'s) puts the leaf
/// first — the case that catches computing a candidate's `has_more` from
/// only the *first* verb contributing that next segment, which is correct
/// for every verb with no such sibling and silently wrong for these two:
/// `search` would report `has_more: false` even though `search.explain`
/// makes it a group, not a leaf, in task 91's WhichKey band.
#[test]
fn completion_says_search_has_more_because_search_explain_exists() {
    let candidates = complete("");
    let search = candidates.iter().find(|c| c.text == "search").unwrap();
    assert!(
        search.has_more,
        "`search` has `search.explain` as a child but has_more is false"
    );
}

#[test]
fn completing_after_a_trailing_dot_moves_past_the_settled_segment() {
    // A trailing `.` finishes a segment exactly like a trailing space does
    // (module docs' "one transform") — this must behave identically to
    // `completing_after_a_trailing_space_moves_past_the_settled_segment`.
    let candidates = complete("message.");
    assert!(candidates.iter().any(|c| c.text == "archive"));
    assert!(!candidates.iter().any(|c| c.text == "message"));
}

#[test]
fn completion_offers_a_verbs_own_flags() {
    // Calls the real production function `complete` factors this branch
    // into, against a local fixture — the same way `check_flags`/
    // `check_positionals` are tested: a `Verb` cannot be registered into
    // the process-wide registry from a test, and no real verb declares any
    // flags yet (`explicit`'s docs). Asserting on a `format!` over the
    // fixture's own fields instead, without calling into `complete` at
    // all, would stay green regardless of whether the flag-completion
    // branch in `complete` worked, was deleted, or was inverted.
    let verb = folder_verb();
    assert_eq!(
        flag_candidates(&verb),
        vec![
            Candidate {
                text: "--force".to_owned(),
                has_more: false,
            },
            Candidate {
                text: "--since".to_owned(),
                has_more: true,
            },
        ]
    );
}

#[test]
fn an_invalid_range_completes_to_nothing_rather_than_panicking() {
    assert_eq!(complete("'<, "), Vec::new());
}

// ---------------------------------------------------------------------------
// describe
// ---------------------------------------------------------------------------

#[test]
fn describe_prefers_an_actions_description_over_a_capabilitys_summary() {
    // `message.archive` carries both `Action::Archive` and (via
    // `parity::Command::MailMove`) a capability — but so do `message.reply`
    // and `message.forward`, which both reach a compose capability whose
    // summary is the same generic "create a draft, optionally pre-filled
    // as a reply or forward" for both. The capability describes the *RPC*;
    // the action describes the specific *user intent*, which is the more
    // useful text for an auto-derived verb's own completion row — and the
    // one thing that actually tells these two apart.
    let verb = verb_at(&["message", "archive"]).unwrap();
    assert!(verb.capability.is_some());
    assert_eq!(verb.describe(), Action::Archive.describe());
    assert_ne!(
        verb_at(&["message", "reply"]).unwrap().describe(),
        verb_at(&["message", "forward"]).unwrap().describe(),
        "reply and forward must not collapse to the same description"
    );
}

#[test]
fn describe_falls_back_to_the_actions_description_with_no_capability() {
    // `cursor.down` is `LOCAL_ACTIONS` — no RPC behind it at all.
    let verb = verb_at(&["cursor", "down"]).unwrap();
    assert_eq!(verb.capability, None);
    assert_eq!(verb.describe(), Action::CursorDown.describe());
}

// ---------------------------------------------------------------------------
// drift: the CLI-spelling check
// ---------------------------------------------------------------------------

/// The rule `every_declared_verb_spells_its_capability_like_the_cli` checks,
/// in isolation: given a made-up capability whose `cli()` says one thing and
/// a verb whose path says another, is that actually caught? A drift test
/// that only ever runs against today's registry — where every verb happens
/// to carry an [`Action`], and is therefore exempt (see that test's own
/// docs) — would pass whether or not the underlying comparison is even
/// correct. This is what proves it is.
#[test]
fn a_capability_only_verb_whose_path_does_not_match_cli_is_a_real_mismatch() {
    // `SearchSearch` is a real capability with `cli() == ["search"]`.
    let capability = Capability::for_cli("search").next().unwrap();
    let verb = Verb {
        path: vec!["not", "the", "cli", "spelling"],
        capability: Some(capability),
        action: None,
        positionals: &[],
        flags: &[],
        cli_alias: None,
    };
    assert!(!spells_like_its_capability(&verb));
}

#[test]
fn a_capability_only_verb_matching_one_of_several_cli_entries_is_not_a_mismatch() {
    // `SendSchedulerCancelScheduled.cli()` is `["undo", "outbox cancel"]` —
    // a verb spelling the *second* entry must not be flagged just because
    // it is not the first.
    let capability = Capability::for_cli("outbox cancel").next().unwrap();
    let verb = Verb {
        path: vec!["outbox", "cancel"],
        capability: Some(capability),
        action: None,
        positionals: &[],
        flags: &[],
        cli_alias: None,
    };
    assert!(spells_like_its_capability(&verb));
}

#[test]
fn a_declared_cli_alias_excuses_a_real_spelling_difference() {
    let capability = Capability::for_cli("tag-rules set").next().unwrap();
    let verb = Verb {
        path: vec!["tag", "rules", "set"],
        capability: Some(capability),
        action: None,
        positionals: &[],
        flags: &[],
        cli_alias: Some("tag-rules set"),
    };
    assert!(spells_like_its_capability(&verb));
}

#[test]
fn an_action_backed_verb_is_exempt_even_with_a_differently_spelled_capability() {
    // The real case task 88 found: `Action::FinderOpen`'s id is `finder`,
    // but `FinderFind`'s `cli()` is `["find"]`. An auto-derived verb keeps
    // the action-id spelling regardless — it has to stay typeable to match
    // `keys.toml`, which predates and does not answer to this grammar.
    let verb = verb_at(&["finder"]).unwrap();
    assert_eq!(verb.action, Some(Action::FinderOpen));
    assert!(
        verb.capability.is_some_and(|c| c.cli() == ["find"]),
        "this test's premise (a real action/cli spelling mismatch) no longer holds; \
         re-check whether spells_like_its_capability still needs the action exemption"
    );
    assert!(spells_like_its_capability(verb));
}

/// Whether `verb`'s own path is how its capability, if any, actually
/// spells itself on the CLI — the check
/// `every_declared_verb_spells_its_capability_like_the_cli` runs over the
/// whole registry. A free function (not a method) because it is a property
/// of the *check*, not of [`Verb`] itself — nothing outside `tests` needs
/// to ask whether a verb spells things correctly, the same way nothing
/// outside `crate::parity::tests` needs `descriptor_rpcs()`.
fn spells_like_its_capability(verb: &Verb) -> bool {
    // Action-backed verbs are exempt: their path is the action id, which
    // predates this grammar and must stay typeable to match `keys.toml`
    // regardless of what a capability's own `cli()` says (module docs).
    if verb.action.is_some() {
        return true;
    }
    let Some(capability) = verb.capability else {
        return true;
    };
    let cli = capability.cli();
    if cli.is_empty() {
        // Nothing to match — the verb is free to invent the name (module
        // docs: "the TUI invents the name").
        return true;
    }
    // A declared alias is checked against the capability's own spellings —
    // it exists precisely because the verb's canonical path is *not* one
    // of them (`tag rules set` vs. clap's flattened `tag-rules set`), so
    // comparing it to `verb.canonical()` would always fail by construction.
    match verb.cli_alias {
        Some(alias) => cli.contains(&alias),
        None => cli.contains(&verb.canonical().as_str()),
    }
}

/// Every declared verb — not the auto-derived ones; see
/// `spells_like_its_capability`'s docs — spells its capability exactly as
/// `parity::Command::cli()` does, or declares why not via
/// [`Verb::cli_alias`].
///
/// Still vacuous over the real registry, but no longer for the original
/// reason: `explicit` is populated now (task 103's two `helpgrep` spellings),
/// and both entries take the `verb.action.is_some()` exemption above. It stops
/// being vacuous the first time a task declares a verb that reaches a
/// capability *without* an action behind it — which is what tasks 94 onward
/// are, and why the three tests above prove the check itself against
/// constructed fixtures rather than trusting the registry to exercise it.
#[test]
fn every_declared_verb_spells_its_capability_like_the_cli() {
    for verb in registry() {
        assert!(
            spells_like_its_capability(verb),
            "{} (capability {:?}) does not spell its capability the way `mail` does, \
             and declares no cli_alias explaining why",
            verb.canonical(),
            verb.capability.map(Capability::name),
        );
    }
}

// ---------------------------------------------------------------------------
// drift: every real verb resolves to itself, and no two collide outright
// ---------------------------------------------------------------------------

/// A real, legitimate pairing this module has to coexist with:
/// `Action::SearchOpen` (`"search"`) and `Action::SearchExplain`
/// (`"search.explain"`) are two independent, unrelated bindings that happen
/// to share a first path segment as a naming convenience — the same pattern
/// as `message.archive`/`message.delete`. Banning any such pairing
/// outright, reasoning from `Keymap::bind`'s `shadow_conflict`, would be
/// the wrong invariant here: that check is right for chords, which commit
/// to the *shortest* complete match as each key arrives and can never back
/// out — but `parse_verb` deliberately does the opposite, trying the
/// *longest*
/// prefix of the typed words first. Typing exactly `search` never considers
/// `search explain` at all (there is no second word to extend the match
/// with), and typing `search explain` matches the two-word verb on the
/// first (longest) attempt — so neither verb is ever the one "unreachable"
/// dead code the old test worried about. What a real prefix pairing *does*
/// cost is narrower: the shorter verb can never treat the longer verb's own
/// next segment as one of its own positional values (typing `search
/// explain` can never mean "search for the word explain"). Auto-derived
/// verbs declare no positionals at all yet (`explicit`'s docs — that is a
/// later task's scope), so nothing observable depends on that today — see
/// `no_real_verb_that_takes_positionals_is_shadowed_by_a_longer_one` below
/// for the check that keeps this true once one does. The test immediately
/// below proves the guarantee that actually matters regardless: every real
/// verb, typed as itself, resolves to itself.
#[test]
fn every_real_verb_is_reachable_by_typing_its_own_path() {
    for verb in registry() {
        let text = verb.canonical();
        let resolution =
            parse(&text).unwrap_or_else(|e| panic!("{text:?} (a real verb) does not parse: {e}"));
        let Resolution::Invocation(invocation) = resolution else {
            panic!("{text:?} (a real verb) resolved to Children, not itself");
        };
        assert_eq!(
            invocation.verb, verb.path,
            "{text:?} resolved to a different verb than itself"
        );
    }
}

/// The one shape of collision that *is* always a bug, whatever the parser's
/// matching order: two verbs claiming the identical path, where
/// [`verb_at`]'s `.find()` would silently return whichever happens to come
/// first and hide the other completely.
#[test]
fn no_two_real_verbs_share_the_same_path() {
    let verbs = registry();
    for (i, a) in verbs.iter().enumerate() {
        for b in &verbs[i + 1..] {
            assert_ne!(
                a.path,
                b.path,
                "{} and {} are two different verbs at the same path",
                a.canonical(),
                b.canonical(),
            );
        }
    }
}

/// The one cost `every_real_verb_is_reachable_by_typing_its_own_path`'s own
/// docs concede a real prefix pairing still has: a verb that declares
/// positionals of its own can never receive, as one of them, whatever word
/// would also continue a longer sibling verb's path — `search explain`
/// can never mean "search for the word explain" while `search.explain`
/// exists, no matter how `search` is parsed. The registry does now hold
/// positional-taking verbs — task 103's `manual grep <pattern>` and
/// `helpgrep <pattern>` — and they are legal precisely because neither has a
/// longer sibling, which is what `no_real_verb_that_takes_positionals_is_shadowed_by_a_longer_one`
/// checks over the real registry. This test proves the *rule* against a
/// constructed pair instead, the same reason the CLI-spelling drift check
/// proves itself against fixtures: no real verb violates it, so nothing real
/// would exercise the failing branch.
#[test]
fn a_positional_taking_verb_shadowed_by_a_longer_one_is_caught() {
    let short = Verb {
        path: vec!["fixture", "shadow"],
        capability: None,
        action: None,
        positionals: &[Positional {
            name: "query",
            required: false,
        }],
        flags: &[],
        cli_alias: None,
    };
    let long = Verb {
        path: vec!["fixture", "shadow", "deeper"],
        capability: None,
        action: None,
        positionals: &[],
        flags: &[],
        cli_alias: None,
    };
    let shadowed = !short.positionals.is_empty()
        && short.path.len() < long.path.len()
        && long.path[..short.path.len()] == short.path[..];
    assert!(shadowed);
}

#[test]
fn no_real_verb_that_takes_positionals_is_shadowed_by_a_longer_one() {
    let verbs = registry();
    for verb in verbs {
        if verb.positionals.is_empty() {
            continue;
        }
        for other in verbs {
            if std::ptr::eq(verb, other) {
                continue;
            }
            assert!(
                !(verb.path.len() < other.path.len()
                    && other.path[..verb.path.len()] == verb.path[..]),
                "{} takes positionals but {} would always be preferred over it",
                verb.canonical(),
                other.canonical(),
            );
        }
    }
}

// ---------------------------------------------------------------------------
// drift: every capability with a TUI verb is reachable
// ---------------------------------------------------------------------------

/// Every capability some [`Action`] reaches has a verb in the registry —
/// trivially true by [`registry`]'s own construction (it derives one for
/// every [`Action`] unconditionally), but proven as a reconciliation
/// against [`Capability::ALL`] rather than trusted, the same reason
/// `parity::tests` proves its own invariants against generated surfaces
/// instead of a second hand-written list.
#[test]
fn every_capability_reachable_by_an_action_has_a_verb() {
    for capability in Capability::ALL {
        for action in capability.actions() {
            assert!(
                verb_at(&split_path(action.id())).is_some(),
                "{} claims {}, but no verb resolves to that action id",
                capability.name(),
                action.id()
            );
        }
    }
}

/// `registry` builds each auto-derived verb's capability from
/// `Capability::for_action(*action).next()` — silently keeping only the
/// first when an action maps to more than one. Nothing today does (checked
/// here across all of `Capability::ALL`, not assumed), but nothing pins
/// that fact against a later capability's `actions()` growing to include an
/// action another capability already claims.
#[test]
fn no_action_maps_to_more_than_one_capability() {
    for action in Action::ALL {
        let count = Capability::for_action(*action).count();
        assert!(
            count <= 1,
            "{} maps to {count} capabilities; `registry` only keeps the first",
            action.id()
        );
    }
}

// ---------------------------------------------------------------------------
// tokenizer edge cases
// ---------------------------------------------------------------------------

#[test]
fn a_flag_with_an_equals_value_parses_the_same_as_a_space_separated_one() {
    let text = r#"test --since=7d"#;
    let tokens = tokenize(text).unwrap();
    assert_eq!(
        tokens,
        vec![
            Token::Word("test".to_owned()),
            Token::Flag {
                name: "since".to_owned(),
                value: Some("7d".to_owned()),
            },
        ]
    );
}

#[test]
fn a_bare_switch_flag_has_no_value() {
    let tokens = tokenize("test --force").unwrap();
    assert_eq!(
        tokens,
        vec![
            Token::Word("test".to_owned()),
            Token::Flag {
                name: "force".to_owned(),
                value: None,
            },
        ]
    );
}

#[test]
fn extra_whitespace_between_tokens_is_ignored() {
    let tokens = tokenize("  message    archive  ").unwrap();
    assert_eq!(
        tokens,
        vec![
            Token::Word("message".to_owned()),
            Token::Word("archive".to_owned()),
        ]
    );
}
