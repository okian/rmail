//! Rendering tests for `mail agent`.
//!
//! The rendered path is where mail- and model-authored text meets a terminal,
//! so that is what these cover; the RPC plumbing is covered end to end by
//! `rmaild/tests/agent_service.rs` against a real in-process server.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::{action_name, outcome_name, sanitize, stop_reason_name, write_entry};
use rmail_proto::v1::{AgentAction, AgentActionEntry, AgentActionOutcome, AgentStopReason};

fn entry() -> AgentActionEntry {
    AgentActionEntry {
        id: 1,
        message_id: 7,
        rfc_message_id: "<a@b>".to_owned(),
        subject: "Invoice".to_owned(),
        sender: "Bob <bob@example.com>".to_owned(),
        action: AgentAction::Archive as i32,
        argument: "Archive".to_owned(),
        reason: "a routine receipt".to_owned(),
        outcome: AgentActionOutcome::Applied as i32,
        detail: "moved to \"Archive\"".to_owned(),
        decided_at: 1_700_000_000,
    }
}

/// The reason is written by a model that has just read attacker-authored
/// text, and the subject is written by the attacker directly. Neither may
/// reach the terminal with an escape sequence in it.
#[test]
fn hostile_text_in_a_subject_or_a_reason_cannot_reach_the_terminal() {
    let mut hostile = entry();
    hostile.subject = "Invoice\u{1b}[2J\u{1b}[H owned".to_owned();
    hostile.reason = "looks fine\u{7}\u{1b}]0;pwned\u{7}".to_owned();
    hostile.sender = "a\u{0}b@example.com".to_owned();
    hostile.detail = "moved\u{1b}[31m".to_owned();

    let mut out: Vec<u8> = Vec::new();
    write_entry(&mut out, &hostile).unwrap();
    let text = String::from_utf8(out).unwrap();

    assert!(
        !text.contains('\u{1b}'),
        "an escape byte reached the terminal: {text:?}"
    );
    assert!(
        !text.chars().any(|c| c.is_control() && c != '\n'),
        "a control character other than the line break survived: {text:?}"
    );
    // The *text* must still be there — a sanitizer that dropped the field
    // would hide the very entry a reader is auditing.
    assert!(
        text.contains("Invoice"),
        "the subject was dropped: {text:?}"
    );
    assert!(
        text.contains("looks fine"),
        "the reason was dropped: {text:?}"
    );
}

/// Multi-byte text comes through intact: the filter is by character class,
/// not by byte, and mangling non-ASCII subjects would be a bug of its own.
#[test]
fn sanitize_keeps_multibyte_text_and_flattens_whitespace() {
    assert_eq!(sanitize("Café 会議 résumé"), "Café 会議 résumé");
    assert_eq!(sanitize("one\ttwo\nthree\rfour"), "one two three four");
}

/// An entry with no reason cannot happen (the daemon refuses one), but the
/// renderer must not print an empty `reason:` line if it ever did.
#[test]
fn an_entry_with_no_reason_or_detail_prints_neither_line() {
    let mut bare = entry();
    bare.reason = String::new();
    bare.detail = String::new();
    let mut out: Vec<u8> = Vec::new();
    write_entry(&mut out, &bare).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert_eq!(text.lines().count(), 1, "expected one line, got {text:?}");
    assert!(!text.contains("reason:"));
}

/// The `--json` spellings are a contract with anything parsing this output.
/// Pinned literally, because deriving them from the generated enum would emit
/// `AGENT_ACTION_ARCHIVE` and would change shape on a rename.
#[test]
fn json_names_are_stable_and_cover_every_variant() {
    for (action, name) in [
        (AgentAction::Archive, "archive"),
        (AgentAction::Label, "label"),
        (AgentAction::Snooze, "snooze"),
        (AgentAction::DraftReply, "draft_reply"),
        (AgentAction::Escalate, "escalate"),
        (AgentAction::None, "none"),
        (AgentAction::Unspecified, "unknown"),
    ] {
        assert_eq!(action_name(action as i32), name);
    }
    for (outcome, name) in [
        (AgentActionOutcome::Attempted, "attempted"),
        (AgentActionOutcome::Applied, "applied"),
        (AgentActionOutcome::Failed, "failed"),
        (AgentActionOutcome::Withheld, "withheld"),
        (AgentActionOutcome::Refused, "refused"),
        (AgentActionOutcome::Planned, "would"),
        (AgentActionOutcome::Unspecified, "unknown"),
    ] {
        assert_eq!(outcome_name(outcome as i32), name);
    }
    for (reason, name) in [
        (AgentStopReason::Running, "running"),
        (AgentStopReason::Completed, "completed"),
        (AgentStopReason::IterationCap, "iteration cap"),
        (AgentStopReason::ActionCap, "action cap"),
        (AgentStopReason::Deadline, "deadline"),
        (AgentStopReason::Cancelled, "cancelled"),
        (AgentStopReason::Error, "error"),
        (AgentStopReason::Unspecified, "unknown"),
    ] {
        assert_eq!(stop_reason_name(reason as i32), name);
    }
}

/// A number no build of this enum has a variant for — a newer daemon talking
/// to an older CLI — renders as `unknown` rather than panicking or silently
/// reading as the zero variant's neighbour.
#[test]
fn an_unknown_wire_value_renders_as_unknown() {
    assert_eq!(action_name(9_999), "unknown");
    assert_eq!(outcome_name(9_999), "unknown");
    assert_eq!(stop_reason_name(9_999), "unknown");
}
