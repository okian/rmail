//! Pure-logic unit tests for the `--json` contract and the terminal
//! renderer's control-character/ANSI defenses. End-to-end coverage (the
//! compiled `mail search`/`mail similar` against a real daemon, streaming
//! behavior, exit codes) lives in `rmail-cli/tests/search_cli.rs` — this
//! crate is bin-only (no lib target), so an external integration test can
//! only exec the built binary, never call these functions directly; the
//! functions worth testing in isolation (schema shape, sanitization) are
//! tested here instead.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use rmail_proto::v1::Message as ProtoMessage;

fn message(id: i64) -> ProtoMessage {
    ProtoMessage {
        id,
        ..Default::default()
    }
}

// ---------------------------------------------------------------------------
// The `--json` contract: exact field set, offsets not markup
// ---------------------------------------------------------------------------

/// The emitted JSON object must carry exactly the documented key set, no
/// more, no fewer — a key silently appearing or disappearing is the failure
/// mode task 42's `--format json` is relying on this contract to avoid.
#[test]
fn json_hit_has_the_exact_documented_key_set() {
    let hit = SearchHit {
        message: Some(message(4471)),
        score: 18.42,
        snippet: Some(Snippet {
            text: "Your invoice is attached.".to_owned(),
            highlights: vec![ByteRange { start: 5, end: 12 }],
        }),
        sources: vec!["lexical".to_owned(), "dense".to_owned()],
        why: None,
        thread_id: Some(88),
        thread_collapsed: vec![],
        near_duplicates: vec![],
        // Task 64's feedback handle. Deliberately *not* part of the `--json`
        // key set asserted below: it is a gRPC-session handle for
        // `LogFeedback`, meaningless to a shell pipeline that has already
        // exited by the time any feedback could be reported.
        query_id: 0,
    };

    let value = serde_json::to_value(JsonHit::from_wire(&hit)).expect("hit serializes");
    let object = value.as_object().expect("top level is a JSON object");
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "date",
            "from",
            "near_duplicates",
            "score",
            "snippet",
            "sources",
            "subject",
            "thread_collapsed",
            "thread_id",
            "uid",
            "why",
        ],
        "the --json field set is a stable contract; every key here must be \
         intentional"
    );
    assert_eq!(object["uid"], serde_json::json!(4471));
    assert_eq!(object["score"], serde_json::json!(18.42));
    assert_eq!(object["thread_id"], serde_json::json!(88));
    assert_eq!(object["why"], serde_json::Value::Null);
}

/// `thread_collapsed`/`near_duplicates`/`why` are present (not omitted) even
/// when empty/absent — a consumer should never have to branch on "is this
/// key here at all" the way it would with `skip_serializing_if`.
#[test]
fn empty_collections_and_absent_why_still_serialize_as_present_keys() {
    let hit = SearchHit {
        message: Some(message(1)),
        score: 1.0,
        snippet: None,
        sources: vec![],
        why: None,
        thread_id: None,
        thread_collapsed: vec![],
        near_duplicates: vec![],
        query_id: 0,
    };
    let value = serde_json::to_value(JsonHit::from_wire(&hit)).unwrap();
    assert_eq!(value["thread_id"], serde_json::Value::Null);
    assert_eq!(value["thread_collapsed"], serde_json::json!([]));
    assert_eq!(value["near_duplicates"], serde_json::json!([]));
    assert_eq!(value["why"], serde_json::Value::Null);
    // A missing snippet degrades to an empty-but-present snippet object,
    // never a missing key or a null.
    assert_eq!(
        value["snippet"],
        serde_json::json!({ "text": "", "highlights": [] })
    );
}

/// The snippet carries byte-offset highlights unmodified — never markup
/// spliced into the text. This is the whole point of `Snippet`'s wire shape
/// (see the module docs' "Offsets, never embedded markup" discussion); this
/// test would fail the moment someone "helpfully" rendered `**bold**` or
/// `<mark>` into `JsonSnippet::text` instead of leaving it verbatim.
#[test]
fn json_snippet_carries_offsets_not_rendered_markup() {
    let snippet = Snippet {
        text: "the invoice total is $4,200".to_owned(),
        highlights: vec![ByteRange { start: 4, end: 11 }],
    };
    let json_snippet = JsonSnippet::from_wire(&snippet);
    assert_eq!(json_snippet.text, "the invoice total is $4,200");
    assert_eq!(&json_snippet.text[4..11], "invoice");
    assert_eq!(
        json_snippet.highlights,
        vec![JsonRange { start: 4, end: 11 }]
    );
}

/// A message containing raw control bytes (including `ESC`, the byte that
/// starts every ANSI/CSI sequence) round-trips through `serde_json` with
/// every one escaped — this is what makes `--json` safe to print to a
/// terminal with no sanitization pass of its own.
#[test]
fn json_output_escapes_every_control_byte_including_esc() {
    let snippet = Snippet {
        text: "click \u{1b}[31mhere\u{1b}[0m now".to_owned(),
        highlights: vec![],
    };
    let json_snippet = JsonSnippet::from_wire(&snippet);
    let line = serde_json::to_string(&json_snippet).unwrap();
    assert!(
        !line.contains('\u{1b}'),
        "the raw ESC byte must never appear in JSON output: {line}"
    );
    assert!(
        line.contains("\\u001b"),
        "ESC must be escaped as \\u001b: {line}"
    );
}

// ---------------------------------------------------------------------------
// Human-readable rendering: highlights, sanitized against terminal injection
// ---------------------------------------------------------------------------

/// Styled rendering wraps a highlighted span in SGR bold/reset and leaves
/// ordinary text untouched.
#[test]
fn styled_snippet_wraps_highlights_in_ansi_bold() {
    let snippet = Snippet {
        text: "the invoice total".to_owned(),
        highlights: vec![ByteRange { start: 4, end: 11 }],
    };
    let rendered = render_snippet(&snippet, true);
    assert_eq!(rendered, "the \x1b[1minvoice\x1b[0m total");
}

/// Unstyled rendering (piped output, `IsTerminal` false) never emits an
/// escape code at all — a downstream program reading `mail search` output
/// with `--json` omitted should see plain text.
#[test]
fn unstyled_snippet_never_emits_escape_codes() {
    let snippet = Snippet {
        text: "the invoice total".to_owned(),
        highlights: vec![ByteRange { start: 4, end: 11 }],
    };
    let rendered = render_snippet(&snippet, false);
    assert_eq!(rendered, "the invoice total");
    assert!(!rendered.contains('\x1b'));
}

/// The core safety property: a snippet body containing a raw ANSI escape
/// sequence (as hostile mail could produce, since `present::Snippet` is
/// documented to pass control bytes through untouched) never reaches the
/// terminal renderer's output as a live escape sequence, whether or not it
/// happens to fall inside a highlight.
#[test]
fn hostile_ansi_and_control_bytes_are_neutralized_in_the_terminal_renderer() {
    let hostile = "click \u{1b}[31mhere\u{1b}[0m\u{7}\u{8} now";
    let snippet = Snippet {
        text: hostile.to_owned(),
        highlights: vec![],
    };
    for styled in [true, false] {
        let rendered = render_snippet(&snippet, styled);
        assert!(
            !rendered.contains('\u{1b}'),
            "styled={styled}: raw ESC must never survive rendering: {rendered:?}"
        );
        assert!(
            !rendered.contains('\u{7}') && !rendered.contains('\u{8}'),
            "styled={styled}: BEL/backspace must never survive rendering: {rendered:?}"
        );
    }
    // The printable payload is still legible -- sanitization drops control
    // bytes, it does not eat the surrounding real text.
    let rendered = render_snippet(&snippet, false);
    assert!(rendered.contains("click"));
    assert!(rendered.contains("here"));
    assert!(rendered.contains("now"));
}

/// Newlines/tabs/carriage returns inside a snippet collapse to a single
/// space rather than being dropped outright or left to break a one-line
/// table row across lines.
#[test]
fn whitespace_control_characters_collapse_to_a_space() {
    let snippet = Snippet {
        text: "line one\nline two\ttabbed\rcr".to_owned(),
        highlights: vec![],
    };
    let rendered = render_snippet(&snippet, false);
    assert_eq!(rendered, "line one line two tabbed cr");
}

/// A highlight range that does not land on a char boundary (would panic a
/// naive slice of multi-byte text) is dropped, not treated as a crash or as
/// "highlight from the nearest boundary" (which would misrepresent what the
/// server actually reported).
#[test]
fn out_of_bounds_or_non_boundary_highlights_are_dropped_defensively() {
    let snippet = Snippet {
        text: "café".to_owned(), // 'é' is 2 bytes; byte 3 is mid-character
        highlights: vec![
            ByteRange { start: 3, end: 4 },  // mid-character
            ByteRange { start: 0, end: 99 }, // past the end of `text`
            ByteRange { start: 0, end: 3 },  // valid: "caf"
        ],
    };
    let ranges = valid_ranges(&snippet);
    assert_eq!(ranges, vec![(0, 3)]);
    // And rendering must not panic despite the two malformed ranges.
    let _ = render_snippet(&snippet, true);
}

// ---------------------------------------------------------------------------
// `mail similar`'s query-building
// ---------------------------------------------------------------------------

fn full_message(subject: Option<&str>, body: Option<&str>) -> FullMessage {
    FullMessage {
        message: Some(ProtoMessage {
            subject: subject.map(str::to_owned),
            ..Default::default()
        }),
        body_text: body.map(str::to_owned),
        body_html: None,
        attachments: vec![],
    }
}

#[test]
fn similar_query_joins_subject_and_body_when_both_present() {
    let full = full_message(Some("Invoice #338"), Some("Total due is $4,200."));
    assert_eq!(
        similar_query_text(&full),
        Some("Invoice #338 Total due is $4,200.".to_owned())
    );
}

#[test]
fn similar_query_falls_back_to_subject_only() {
    let full = full_message(Some("Invoice #338"), None);
    assert_eq!(similar_query_text(&full), Some("Invoice #338".to_owned()));
}

#[test]
fn similar_query_falls_back_to_body_only() {
    let full = full_message(None, Some("Total due is $4,200."));
    assert_eq!(
        similar_query_text(&full),
        Some("Total due is $4,200.".to_owned())
    );
}

#[test]
fn similar_query_is_none_when_message_has_no_text_at_all() {
    let full = full_message(None, None);
    assert_eq!(similar_query_text(&full), None);
    let blank = full_message(Some("   "), Some(""));
    assert_eq!(similar_query_text(&blank), None);
}

#[test]
fn truncate_chars_snaps_to_a_char_boundary_not_a_byte_offset() {
    // 'é' is 2 bytes; a byte-oriented cap at 3 would split it.
    let text = "café résumé";
    let truncated = truncate_chars(text, 4);
    assert_eq!(truncated, "café");
    assert!(text.is_char_boundary(truncated.len()));
}

#[test]
fn truncate_chars_is_a_no_op_when_text_is_already_short() {
    assert_eq!(truncate_chars("short", 100), "short");
}

// ---------------------------------------------------------------------------
// Date formatting
// ---------------------------------------------------------------------------

#[test]
fn format_rfc3339_matches_the_prd_example_style() {
    // The Unix epoch is a fixed, hand-verifiable reference point.
    assert_eq!(format_rfc3339(0), Some("1970-01-01T00:00:00Z".to_owned()));
}
