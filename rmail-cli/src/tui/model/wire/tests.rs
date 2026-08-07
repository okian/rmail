//! Tests for the wire boundary, including the seam that proves the viewer
//! shows what `rmail_core`'s parser decoded rather than decoding again.

use rmail_core::message::parse::parse_message;
use rmail_proto::v1::{Attachment, FullMessage, Message as ProtoMessage};

use super::*;

/// A message exercising every decoding prd.md's viewer section names:
/// multipart/mixed, an RFC 2047 encoded-word subject, a quoted-printable
/// text part, and a base64 attachment.
const AWKWARD: &[u8] = b"From: =?UTF-8?Q?Zo=C3=AB?= <zoe@example.com>\r\n\
To: bob@example.com\r\n\
Subject: =?UTF-8?B?SW52b2ljZSDigqwxMA==?=\r\n\
Content-Type: multipart/mixed; boundary=\"b\"\r\n\
\r\n\
--b\r\n\
Content-Type: text/plain; charset=utf-8\r\n\
Content-Transfer-Encoding: quoted-printable\r\n\
\r\n\
Total: =E2=82=AC10 =3D cheap\r\n\
--b\r\n\
Content-Type: text/html\r\n\
\r\n\
<p>Total: &euro;10</p>\r\n\
--b\r\n\
Content-Type: application/pdf; name=\"doc.pdf\"\r\n\
Content-Disposition: attachment; filename=\"doc.pdf\"\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
aGVsbG8=\r\n\
--b--\r\n";

/// Wrap a parse exactly as the daemon does: `rmail_core`'s parser fills the
/// `messages` row at sync time, and `MailService.Get` projects that row into
/// a `FullMessage` (see `rmaild::mail_service::full_message_to_proto`).
fn as_daemon_would(raw: &[u8]) -> FullMessage {
    let parsed = parse_message(raw);
    FullMessage {
        message: Some(ProtoMessage {
            id: 42,
            subject: parsed.subject.clone(),
            from_addr: parsed.from_addr.clone(),
            from_name: parsed.from_name.clone(),
            to_addrs: parsed.to_addrs.clone(),
            cc_addrs: parsed.cc_addrs.clone(),
            date: parsed.date,
            has_attachments: parsed.has_attachments(),
            ..ProtoMessage::default()
        }),
        body_text: parsed.body_text.clone(),
        body_html: parsed.body_html.clone(),
        attachments: parsed
            .attachments
            .iter()
            .enumerate()
            .map(|(i, a)| Attachment {
                id: i64::try_from(i).unwrap_or_default(),
                part_id: a.part_id.clone().unwrap_or_default(),
                filename: a.filename.clone(),
                content_type: a.content_type.clone(),
                size: a.size,
                content_id: a.content_id.clone(),
                is_inline: a.is_inline,
            })
            .collect(),
    }
}

#[test]
fn viewer_shows_what_parse_message_decoded() {
    // The point of this test is the *seam*: the TUI contains no decoder, so
    // if this shows decoded text it is because `parse_message`'s output
    // reached it intact. A second decoder in the client would be a copy that
    // drifts from what search matched and AI summarised.
    let open = open_message(as_daemon_would(AWKWARD));

    let subject = open
        .headers
        .iter()
        .find(|(name, _)| name == "Subject")
        .map(|(_, value)| value.clone())
        .expect("a Subject header");
    assert_eq!(subject, "Invoice €10", "RFC 2047 base64 encoded-word");

    let from = open
        .headers
        .iter()
        .find(|(name, _)| name == "From")
        .map(|(_, value)| value.clone())
        .expect("a From header");
    assert_eq!(from, "Zoë <zoe@example.com>", "RFC 2047 Q encoded-word");

    let body = open.body.join("\n");
    assert!(body.contains("€10"), "quoted-printable decoded: {body:?}");
    assert!(
        body.contains("= cheap"),
        "=3D decoded, not literal: {body:?}"
    );

    assert!(open.has_html, "the HTML alternative is offered");
    assert_eq!(open.attachments.len(), 1);
    assert!(
        open.attachments[0].contains("doc.pdf") && open.attachments[0].contains("5 bytes"),
        "base64 attachment decoded to its real length: {:?}",
        open.attachments[0]
    );
}

#[test]
fn an_html_only_message_still_has_something_to_show_and_offers_the_browser() {
    let raw = b"From: a@example.com\r\n\
Subject: Newsletter\r\n\
Content-Type: text/html\r\n\
\r\n\
<html><body><h1>Sale</h1><p>Everything must go.</p></body></html>\r\n";
    let open = open_message(as_daemon_would(raw));

    assert!(open.has_html);
    let body = open.body.join("\n");
    assert!(
        body.contains("Sale"),
        "parse_message's HTML-stripped fallback is shown: {body:?}"
    );
    assert!(
        !body.contains('<'),
        "no markup leaks into the pane: {body:?}"
    );
}

#[test]
fn a_message_with_no_body_at_all_says_so_rather_than_rendering_blank() {
    let full = FullMessage {
        message: Some(ProtoMessage {
            id: 1,
            ..ProtoMessage::default()
        }),
        body_text: None,
        body_html: None,
        attachments: Vec::new(),
    };
    let open = open_message(full);
    assert_eq!(open.body, vec!["(no body)".to_owned()]);
    assert!(!open.has_html);
}

#[test]
fn a_whitespace_only_html_part_is_not_treated_as_an_html_alternative() {
    let full = FullMessage {
        message: Some(ProtoMessage::default()),
        body_text: Some("text".to_owned()),
        body_html: Some("   \n  ".to_owned()),
        attachments: Vec::new(),
    };
    assert!(!open_message(full.clone()).has_html);
    assert!(html_body(&full).is_none());
}

#[test]
fn crlf_and_tabs_are_normalised_for_the_terminal() {
    let full = FullMessage {
        message: Some(ProtoMessage::default()),
        body_text: Some("one\r\ntwo\tthree\r\n".to_owned()),
        body_html: None,
        attachments: Vec::new(),
    };
    let open = open_message(full);
    assert_eq!(
        open.body,
        vec!["one".to_owned(), "two    three".to_owned(), String::new()],
        "no stray carriage returns, tabs expanded"
    );
}

#[test]
fn a_subjectless_message_shows_a_placeholder_in_both_the_list_and_the_viewer() {
    let row = message_row(ProtoMessage {
        id: 3,
        subject: Some("   ".to_owned()),
        from_addr: Some("a@example.com".to_owned()),
        ..ProtoMessage::default()
    });
    assert_eq!(row.subject, "(no subject)");
    assert_eq!(row.from, "a@example.com", "falls back to the bare address");

    let open = open_message(FullMessage {
        message: Some(ProtoMessage::default()),
        body_text: Some("hi".to_owned()),
        body_html: None,
        attachments: Vec::new(),
    });
    assert!(open
        .headers
        .iter()
        .any(|(name, value)| name == "Subject" && value == "(no subject)"));
}

#[test]
fn a_list_row_prefers_the_display_name_and_keeps_the_address_for_replying() {
    let row = message_row(ProtoMessage {
        id: 4,
        from_name: Some("Zoë".to_owned()),
        from_addr: Some("zoe@example.com".to_owned()),
        internaldate: Some(1_700_000_000),
        ..ProtoMessage::default()
    });
    assert_eq!(row.from, "Zoë");
    assert_eq!(row.from_addr.as_deref(), Some("zoe@example.com"));
    assert_eq!(
        row.date,
        Some(1_700_000_000),
        "internaldate stands in for a missing Date header"
    );
}

// ---------------------------------------------------------------------------
// drafts
// ---------------------------------------------------------------------------

#[test]
fn a_reply_threads_onto_the_original_and_quotes_it() {
    let original = as_daemon_would(AWKWARD);
    let request = draft_request(
        DraftKind::Reply,
        7,
        "me@example.com",
        "zoe@example.com",
        &original,
    );

    assert_eq!(request.account_id, 7);
    assert_eq!(request.subject, "Re: Invoice €10");
    assert_eq!(
        request.to.first().map(|a| a.address.as_str()),
        Some("zoe@example.com")
    );
    assert_eq!(
        request.from.as_ref().map(|a| a.address.as_str()),
        Some("me@example.com")
    );
    assert_eq!(
        request.in_reply_to_message_id,
        Some(42),
        "ComposeService freezes the threading headers from this"
    );
    assert!(
        request.body_text.contains("> Total: €10"),
        "the decoded original is quoted: {:?}",
        request.body_text
    );
    assert!(
        request.attachments.is_empty() && request.body_html.is_none(),
        "the TUI builds no MIME; ComposeService renders the draft"
    );
}

#[test]
fn a_forward_does_not_thread_onto_the_conversation_it_left() {
    let original = as_daemon_would(AWKWARD);
    let request = draft_request(
        DraftKind::Forward,
        7,
        "me@example.com",
        "bob@example.com",
        &original,
    );

    assert_eq!(request.subject, "Fwd: Invoice €10");
    assert_eq!(
        request.in_reply_to_message_id, None,
        "a forward to a new audience must not join the original thread"
    );
    assert!(request.body_text.contains("Forwarded message"));
    assert!(
        !request.body_text.contains("> Total"),
        "a forward is not quoted like a reply"
    );
}

#[test]
fn subject_prefixes_do_not_stack() {
    for (kind, subject, expected) in [
        (DraftKind::Reply, "Re: hello", "Re: hello"),
        (DraftKind::Reply, "RE: hello", "RE: hello"),
        (DraftKind::Reply, "hello", "Re: hello"),
        (DraftKind::Forward, "Fwd: hello", "Fwd: hello"),
        (DraftKind::Forward, "FW: hello", "FW: hello"),
        (DraftKind::Forward, "hello", "Fwd: hello"),
        (DraftKind::Reply, "", "Re:"),
    ] {
        assert_eq!(prefixed_subject(kind, subject), expected, "{subject:?}");
    }
}

#[test]
fn an_enormous_body_is_truncated_on_a_character_boundary_and_says_so() {
    // A multi-byte character repeated past the cap: truncating by byte offset
    // would split a code point and produce invalid UTF-8 (or panic).
    let body = "€".repeat(MAX_QUOTED_CHARS + 500);
    let original = FullMessage {
        message: Some(ProtoMessage {
            id: 1,
            from_addr: Some("a@example.com".to_owned()),
            ..ProtoMessage::default()
        }),
        body_text: Some(body),
        body_html: None,
        attachments: Vec::new(),
    };
    let request = draft_request(
        DraftKind::Reply,
        1,
        "me@example.com",
        "a@example.com",
        &original,
    );
    assert!(request.body_text.contains("[quoted text truncated]"));
    assert!(
        request.body_text.chars().count() < MAX_QUOTED_CHARS + 500,
        "the whole body was carried anyway"
    );
}

#[test]
fn a_message_the_daemon_could_not_parse_still_produces_a_usable_draft() {
    let original = FullMessage {
        message: Some(ProtoMessage {
            id: 9,
            ..ProtoMessage::default()
        }),
        body_text: None,
        body_html: None,
        attachments: Vec::new(),
    };
    let request = draft_request(
        DraftKind::Reply,
        1,
        "me@example.com",
        "a@example.com",
        &original,
    );
    assert_eq!(request.subject, "Re:");
    assert!(request.body_text.is_empty());
    assert_eq!(request.in_reply_to_message_id, Some(9));
}

#[test]
fn folders_come_from_the_sync_status_rows() {
    let folder = folder(rmail_proto::v1::FolderStatus {
        mailbox_id: 12,
        name: "INBOX".to_owned(),
        message_count: 431,
        ..rmail_proto::v1::FolderStatus::default()
    });
    assert_eq!(folder.id, 12);
    assert_eq!(folder.name, "INBOX");
    assert_eq!(folder.message_count, 431);
}
