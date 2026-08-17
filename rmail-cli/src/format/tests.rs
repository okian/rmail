//! The `--format` contract: terminal-safe JSON, frame shapes, and the
//! declaration every verb owes about how it answers `--format json`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use clap::CommandFactory as _;
use serde_json::json;

use super::*;
use crate::Cli;

// ---------------------------------------------------------------------------
// Terminal safety
// ---------------------------------------------------------------------------

/// A subject carrying an ANSI escape, a bidi override and an invisible tag
/// character must reach the terminal as printable text and reach `jq` as the
/// original bytes. Both halves matter: escaping that loses data would make
/// `--format json` a lossy interchange format, and not escaping at all is how
/// a mailbox repaints somebody's screen.
#[test]
fn hostile_text_is_escaped_on_the_wire_and_intact_after_parsing() {
    let hostile = "Invoice \u{1b}[2J\u{202e}gnp.exe\u{e0041} due";
    let line = to_line(&json!({ "subject": hostile })).unwrap();

    for (needle, what) in [
        ('\u{1b}', "ESC, which starts every ANSI/CSI/OSC sequence"),
        ('\u{202e}', "RIGHT-TO-LEFT OVERRIDE"),
        ('\u{e0041}', "a tag character"),
    ] {
        assert!(
            !line.contains(needle),
            "{what} reached the output verbatim: {line:?}"
        );
    }
    assert!(line.contains("\\u202e"), "expected an escape in {line:?}");
    assert!(
        line.contains("\\udb40\\udc41"),
        "a non-BMP code point must be escaped as a surrogate pair: {line:?}"
    );

    let parsed: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(
        parsed["subject"], hostile,
        "escaping must be lossless — a consumer gets the real subject back"
    );
}

/// The pretty (`--format json`) writer escapes exactly as the compact one
/// does. It is a separate `Formatter` implementation, so "the compact path is
/// safe" proves nothing about it.
#[test]
fn the_pretty_writer_escapes_the_same_characters() {
    let hostile = "a\u{1b}b\u{202e}c";
    let doc = to_document(&json!({ "subject": hostile })).unwrap();
    assert!(!doc.contains('\u{1b}'), "{doc:?}");
    assert!(!doc.contains('\u{202e}'), "{doc:?}");
    assert!(doc.contains('\n'), "the document form is indented: {doc:?}");
    let parsed: serde_json::Value = serde_json::from_str(&doc).unwrap();
    assert_eq!(parsed["subject"], hostile);
}

/// Ordinary non-ASCII text is *not* mangled. An escaper that reached for
/// "escape everything above 0x7f" would turn every European name and every CJK
/// subject into unreadable soup on the terminal, which is a worse outcome than
/// the problem it solves.
#[test]
fn legitimate_unicode_survives_unescaped() {
    let line =
        to_line(&json!({ "from": "Ada Lovelace <ada@académie.fr>", "s": "請回覆" })).unwrap();
    assert!(line.contains("académie"), "{line:?}");
    assert!(line.contains("請回覆"), "{line:?}");
}

/// A newline inside a value must not be able to forge an ndjson record
/// boundary. `serde_json` escapes it as `\n` because RFC 8259 requires it —
/// this pins that the safe formatter did not undo that.
#[test]
fn a_newline_in_a_value_cannot_split_an_ndjson_record() {
    let line = to_line(&json!({ "subject": "one\ntwo" })).unwrap();
    assert_eq!(line.lines().count(), 1, "{line:?}");
    assert!(line.contains("\\n"), "{line:?}");
}

// ---------------------------------------------------------------------------
// Format selection
// ---------------------------------------------------------------------------

#[test]
fn only_table_is_unstructured() {
    assert!(!OutputFormat::Table.is_structured());
    assert!(OutputFormat::Json.is_structured());
    assert!(OutputFormat::Ndjson.is_structured());
    assert_eq!(OutputFormat::default(), OutputFormat::Table);
}

/// The three spellings `--format` accepts are the three prd.md names.
#[test]
fn the_accepted_spellings_are_the_documented_three() {
    use clap::ValueEnum as _;
    let spellings: Vec<&str> = OutputFormat::value_variants()
        .iter()
        .map(|v| v.as_str())
        .collect();
    assert_eq!(spellings, vec!["table", "json", "ndjson"]);
    for spelling in &spellings {
        assert_eq!(
            OutputFormat::from_str(spelling, true)
                .expect("a documented spelling parses")
                .as_str(),
            *spelling
        );
    }
    assert!(
        OutputFormat::from_str("yaml", true).is_err(),
        "an unknown format must be rejected, not silently defaulted"
    );
}

/// **No subcommand may declare an argument whose id is `format`.**
///
/// `clap` does not treat a global argument and a subcommand argument sharing
/// an id as a conflict. `ArgMatcher::fill_in_global_values` picks a winner by
/// value-source precedence and writes it into *both* sets of matches — so an
/// explicit global value silently lands in the subcommand's own field. That is
/// not theoretical: `RMAIL_FORMAT=json mail export -o backup.mbox` wrote a
/// **JSON archive into a file named `.mbox`**, with no diagnostic, and
/// `mail --format json export …` aborted the process on a failed downcast.
///
/// `mail export` therefore spells its archive flag `--archive-format`. This
/// test is what stops the next verb from re-introducing the collision — the
/// failure mode is silent data corruption, so nothing softer will do.
/// Extended to *every* global flag, not only `--format`: `--socket`,
/// `--token`, `--deadline` and the rest would each be silently merged the same
/// way, and only `--format` happened to be the one that corrupted a file.
#[test]
fn no_subcommand_shadows_the_global_format_flag() {
    let root = Cli::command();
    // `clap` *skips* propagating a global into a subcommand that already
    // declares its id, so the two never coexist in the built tree and counting
    // them finds nothing — the first version of this test passed against the
    // very bug it was written for. What distinguishes them is the flag itself:
    // a propagated argument reports `is_global_set()`, a locally declared one
    // does not.
    let globals: Vec<String> = root
        .get_arguments()
        .filter(|a| a.is_global_set())
        .map(|a| a.get_id().to_string())
        .collect();
    assert!(
        globals.contains(&"format".to_owned()),
        "the global --format is what this test is about: {globals:?}"
    );

    fn walk(command: &clap::Command, prefix: &str, globals: &[String], out: &mut Vec<String>) {
        for sub in command.get_subcommands() {
            let path = if prefix.is_empty() {
                sub.get_name().to_owned()
            } else {
                format!("{prefix} {}", sub.get_name())
            };
            for arg in sub.get_arguments() {
                if !arg.is_global_set() && globals.iter().any(|g| g == arg.get_id().as_str()) {
                    out.push(format!("mail {path} --{}", arg.get_id()));
                }
            }
            walk(sub, &path, globals, out);
        }
    }
    let mut shadowing = Vec::new();
    walk(&root, "", &globals, &mut shadowing);
    assert!(
        shadowing.is_empty(),
        "these arguments share an id with a global flag. clap does not report that as a \
         conflict — `fill_in_global_values` picks a winner by value-source precedence and writes \
         it into *both* sets of matches, so an explicit global value silently lands in the \
         subcommand's field. For `--format` that wrote a JSON archive into a .mbox file. Rename \
         them, as `mail export` renamed its to --archive-format: {shadowing:?}"
    );
}

/// `mail export` keeps an archive-format flag, under its new name, and `-f`
/// still works.
#[test]
fn export_keeps_its_archive_formats_under_the_renamed_flag() {
    let export = Cli::command()
        .get_subcommands()
        .find(|s| s.get_name() == "export")
        .cloned()
        .expect("mail export exists");
    let archive = export
        .get_arguments()
        .find(|a| a.get_id() == "archive_format")
        .expect("mail export declares --archive-format");
    assert_eq!(archive.get_long(), Some("archive-format"));
    assert_eq!(archive.get_short(), Some('f'));
    let values: Vec<String> = archive
        .get_possible_values()
        .iter()
        .map(|v| v.get_name().to_owned())
        .collect();
    assert_eq!(values, vec!["mbox", "maildir", "eml", "json"]);
}

/// The legacy `--json` flags are aliases, not a second switch: a verb that
/// consulted only its own would print a table to a `--format json` caller.
#[test]
fn wants_json_counts_both_the_legacy_flag_and_the_global_one() {
    // `current()` is `Table` in a unit test (nothing has called `init`), which
    // is what makes this the interesting half: the legacy flag alone must
    // still select JSON.
    assert!(wants_json(true));
    assert!(!wants_json(false));
}

// ---------------------------------------------------------------------------
// Response rendering
// ---------------------------------------------------------------------------

/// The proto field names are the JSON keys, and they come from the descriptor
/// set rather than from a name written here. A message the descriptor set
/// describes must round-trip to exactly its declared fields.
#[test]
fn a_response_renders_with_its_proto_field_names() {
    let response = rmail_proto::v1::ScoreMessageResponse {
        state: rmail_proto::v1::NotificationState::Delivered as i32,
        tier: Some(rmail_proto::v1::NotificationTier::High as i32),
        reason: Some("mentioned you".to_owned()),
        suppressed_reason: String::new(),
        effective_threshold: "NOTIFICATION_TIER_NORMAL".to_owned(),
        account_enabled: true,
        would_notify: true,
    };
    let value = response_json(Command::NotificationScoreMessage, &response).unwrap();
    let object = value.as_object().expect("a message renders as an object");

    // Set-equality against the proto, not a hand-copied list: the point is
    // that this output *is* `proto/rmail/v1/notification.proto`, so the test
    // must fail if the two ever disagree rather than if someone forgot to
    // edit a literal here.
    assert_eq!(object["would_notify"], json!(true));
    assert_eq!(
        object["effective_threshold"],
        json!("NOTIFICATION_TIER_NORMAL")
    );
    assert_eq!(object["reason"], json!("mentioned you"));
    // Enums by name, never by tag number — a script that has to know that
    // `2` means "high" is a script that breaks when a value is inserted.
    assert_eq!(object["tier"], json!("NOTIFICATION_TIER_HIGH"));
    assert!(
        !object.contains_key("tierValue") && !object.contains_key("wouldNotify"),
        "keys are proto field names, not camelCase: {object:?}"
    );
}

/// An id past 2^53 must not come back as a float that lost its low bits — the
/// hard-won behaviour `rmaild::mcp::codec` already has, inherited rather than
/// re-derived.
#[test]
fn a_64_bit_id_survives_as_a_string() {
    let response = rmail_proto::v1::MintTokenResponse {
        id: 9_007_199_254_740_993,
        token: "rmail_tok_x".to_owned(),
        ..Default::default()
    };
    let value = response_json(Command::AdminMintToken, &response).unwrap();
    assert_eq!(value["id"], json!("9007199254740993"));
}

/// The row a command names has to be the row whose RPC actually produced the
/// message; a mismatch is a decode against the wrong descriptor. This is the
/// failure mode of naming the message type at the call site, which is why the
/// helper takes a capability row instead.
#[test]
fn rendering_a_message_against_the_wrong_command_is_an_error_not_garbage() {
    let response = rmail_proto::v1::MintTokenResponse {
        id: 1,
        token: "s".to_owned(),
        ..Default::default()
    };
    // `ScoreMessageResponse` has an enum where this has a string; the codec
    // refuses rather than inventing a value.
    let err = response_json(Command::NotificationScoreMessage, &response)
        .expect_err("a mismatched descriptor must fail");
    let rendered = format!("{err:#}");
    assert!(
        rendered.contains("ScoreMessageResponse"),
        "the error should name what it was trying to decode: {rendered}"
    );
}

// ---------------------------------------------------------------------------
// The declaration every verb owes
// ---------------------------------------------------------------------------

/// Every subcommand path a user can invoke, space separated, without the
/// leading `mail`.
///
/// Deliberately the same walk `crate::parity::invocable_paths` does — see that
/// module for why a node with a required subcommand is a namespace rather than
/// a verb. Duplicated rather than shared because both modules are `#[cfg(test)]`
/// islands with no non-test consumer, and a `pub(crate)` helper existing only
/// to be called by another test module would have to be `#[cfg(test)]` too.
fn invocable_paths() -> Vec<String> {
    fn walk(command: &clap::Command, prefix: &str, out: &mut Vec<String>) {
        for sub in command.get_subcommands() {
            if sub.get_name() == "help" {
                continue;
            }
            let path = if prefix.is_empty() {
                sub.get_name().to_owned()
            } else {
                format!("{prefix} {}", sub.get_name())
            };
            if !sub.is_subcommand_required_set() {
                out.push(path.clone());
            }
            walk(sub, &path, out);
        }
    }
    let mut out = Vec::new();
    walk(&Cli::command(), "", &mut out);
    out
}

/// Every `mail` verb either renders structured output or says why it does not.
///
/// The drift this catches is the one that matters most for a flag that is a
/// contract: a verb ships, `--format json` on it prints a human table, and a
/// script somewhere starts parsing columns. A verb reaching neither list fails
/// here by name, and the author has to decide — in a diff a reviewer reads —
/// which it is.
#[test]
fn every_cli_verb_declares_how_it_answers_format_json() {
    let mut undeclared = Vec::new();
    for path in invocable_paths() {
        if is_unstructured(&path).is_some() || STRUCTURED.contains(&path.as_str()) {
            continue;
        }
        undeclared.push(path);
    }
    // The two lists must not overlap either: `is_unstructured` is consulted
    // first at dispatch, so a verb in both would be silently refused despite
    // claiming to render.
    for path in NO_CURATED_SCHEMA {
        assert!(
            !STRUCTURED.contains(path),
            "`mail {path}` is declared both structured and lacking a schema"
        );
    }
    assert!(
        undeclared.is_empty(),
        "these `mail` verbs do not say what `--format json` does on them — add each to \
         format::STRUCTURED (and make it emit) or to format::UNSTRUCTURED (with a reason): {}",
        undeclared.join(", ")
    );
}

/// The mirror direction: a declaration naming a verb nobody can type is a
/// declaration that stopped meaning anything.
#[test]
fn every_declaration_names_a_real_verb() {
    let invocable = invocable_paths();
    for (path, _) in UNSTRUCTURED {
        assert!(
            invocable.contains(&(*path).to_owned()),
            "format::UNSTRUCTURED declares `mail {path}`, which is not an invocable subcommand"
        );
    }
    for path in NO_CURATED_SCHEMA {
        assert!(
            invocable.contains(&(*path).to_owned()),
            "format::NO_CURATED_SCHEMA declares `mail {path}`, which is not an invocable \
             subcommand"
        );
    }
    for path in STRUCTURED {
        assert!(
            invocable.contains(&(*path).to_owned()),
            "format::STRUCTURED declares `mail {path}`, which is not an invocable subcommand"
        );
    }
}

/// A verb cannot be in both lists — that would be a promise and its withdrawal
/// in the same commit, and `is_unstructured` (checked first at dispatch) would
/// silently win.
#[test]
fn no_verb_is_declared_both_ways() {
    for path in STRUCTURED {
        assert!(
            is_unstructured(path).is_none(),
            "`mail {path}` is declared both structured and unstructured"
        );
    }
}

/// The walk really reaches the leaves; a walk that stopped early would report
/// a clean declaration check having examined almost nothing.
#[test]
fn the_walk_reaches_the_deepest_verbs() {
    let paths = invocable_paths();
    assert!(paths.contains(&"api call".to_owned()), "{paths:?}");
    assert!(paths.contains(&"daemon status".to_owned()), "{paths:?}");
    assert!(paths.contains(&"ai budget set".to_owned()), "{paths:?}");
    assert!(
        paths.len() > 40,
        "only {} invocable verbs found — the walk is not reaching the tree",
        paths.len()
    );
}
