//! What the renderer owes: the octets it produces are a valid RFC 5322
//! message that a real parser reads back as the draft that went in.
//!
//! # Why round-tripping is the load-bearing assertion here
//!
//! Asserting on substrings of the rendered text ("the message contains
//! `Content-Type: multipart/mixed`") proves the builder wrote what the
//! builder meant. It cannot prove a recipient can *read* it — a message with
//! a boundary that does not match its parts, an encoded-word that decodes to
//! mojibake, or a quoted-printable body with a mis-wrapped soft break all
//! pass that kind of test and fail on delivery. So the substantive tests
//! below parse the builder's own output back with `mail_parser` — the same
//! parser `message::parse` already uses on inbound mail, i.e. the code path
//! rmail itself would take if it received the message — and compare against
//! the draft.

use mail_parser::{Address, MessageParser, MimeHeaders};

use super::*;
use crate::error::ErrorReason;
use crate::message::parse_message;

fn mailbox(spec: &str) -> Mailbox {
    Mailbox::parse(spec).expect("test address parses")
}

fn envelope() -> Envelope {
    Envelope::new(
        "fixed.id@example.com".to_owned(),
        chrono::DateTime::parse_from_rfc3339("2026-08-05T09:30:00-07:00").expect("fixed date"),
    )
}

/// A minimal, valid draft: one recipient, plain text, nothing else.
fn draft() -> Draft {
    Draft {
        id: 1,
        account_id: 1,
        from: mailbox("Alice <alice@example.com>"),
        to: vec![mailbox("bob@example.net")],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: "Lunch".to_owned(),
        body_text: "Shall we say noon?".to_owned(),
        body_html: None,
        attachments: Vec::new(),
        in_reply_to_message_id: None,
        in_reply_to: None,
        references: Vec::new(),
        created_at: 0,
        updated_at: 0,
    }
}

fn attachment(filename: &str, content_type: &str, content: &[u8]) -> DraftAttachment {
    DraftAttachment {
        id: 1,
        filename: filename.to_owned(),
        content_type: content_type.to_owned(),
        size: i64::try_from(content.len()).unwrap_or(0),
        content: content.to_vec(),
    }
}

fn render(draft: &Draft) -> Vec<u8> {
    build(draft, &envelope()).expect("a valid draft renders")
}

/// The rendered message as text. Sound because every encoder in this module
/// emits ASCII — an invariant `the_rendered_message_is_always_ascii` proves
/// independently.
fn text(draft: &Draft) -> String {
    String::from_utf8(render(draft)).expect("rendered output is ASCII")
}

/// The header block, unfolded onto single lines, for assertions that care
/// about a field's value rather than its wrapping.
fn unfolded_headers(rendered: &str) -> Vec<String> {
    let block = rendered.split("\r\n\r\n").next().unwrap_or_default();
    let mut out: Vec<String> = Vec::new();
    for line in block.split("\r\n") {
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some(last) = out.last_mut() {
                last.push(' ');
                last.push_str(line.trim_start());
            }
        } else {
            out.push(line.to_owned());
        }
    }
    out
}

fn header<'a>(headers: &'a [String], name: &str) -> Option<&'a str> {
    let prefix = format!("{name}: ");
    headers
        .iter()
        .find(|h| h.starts_with(&prefix))
        .map(|h| &h[prefix.len()..])
}

// ---------------------------------------------------------------------------
// Wire-level invariants
// ---------------------------------------------------------------------------

/// A body that stresses every encoder at once: non-ASCII, a line far past the
/// limit, trailing whitespace, mbox-mangling bait, and lone LFs.
fn adversarial_body() -> String {
    format!(
        "From the desk of Alice\nCafé — naïve, résumé\ntrailing space   \n.leading dot\n{}\r\nmixed\rendings\n",
        "x".repeat(4000)
    )
}

fn adversarial_draft() -> Draft {
    Draft {
        subject: format!("Ünicode {} end", "subject ".repeat(60)),
        body_text: adversarial_body(),
        body_html: Some(format!("<p>Café {}</p>", "&nbsp;".repeat(500))),
        to: vec![
            mailbox("\"Doe, Jane\" <jane@example.net>"),
            mailbox("bob@example.net"),
            mailbox("a-very-long-local-part-that-goes-on-and-on@subdomain.example.org"),
        ],
        cc: vec![mailbox(
            "Café Ünicode Ünlimited Ünternational <cc@example.org>",
        )],
        attachments: vec![attachment(
            "réport ünicode.pdf",
            "application/pdf",
            &[0u8; 3000],
        )],
        references: (0..40)
            .map(|n| format!("ref{n}.{}@example.com", "y".repeat(60)))
            .collect(),
        in_reply_to: Some("parent@example.com".to_owned()),
        ..draft()
    }
}

#[test]
fn no_line_exceeds_the_rfc_5322_limit() {
    let rendered = render(&adversarial_draft());
    for (index, line) in rendered.split(|&b| b == b'\n').enumerate() {
        let len = line.strip_suffix(b"\r").unwrap_or(line).len();
        assert!(
            len <= MAX_LINE,
            "line {} is {len} octets, over the {MAX_LINE}-octet limit",
            index + 1
        );
    }
}

/// A draft built at **every documented maximum at once**.
///
/// `adversarial_draft` is hand-picked and therefore proves only what its
/// author thought to include. This one is derived from the constants: if a
/// bound is ever raised past what the renderer can emit on one line, this is
/// what fails, and it fails for the arm that was raised rather than for
/// whatever the author happened to type. Every string here is chosen to be the
/// *worst* case for its rendering form, not merely the longest.
fn maximal_draft() -> Draft {
    // The longest addr-spec `address.rs` accepts: 64-octet local part,
    // 255-octet domain.
    let local = "a".repeat(64);
    let domain = format!("{}.example", "b".repeat(255 - ".example".len()));
    let longest_addr = format!("{local}@{domain}");

    // The worst display name is the quoted-string form, where every `\` and
    // `"` doubles — and it is the one form that cannot fold.
    let quoted_worst = "\\".repeat(400);
    // ...and the worst encoded form is 4-octet characters, which cost 12
    // octets each in a Q-encoded word.
    let encoded_worst = "\u{1F600}".repeat(100);

    Draft {
        from: Mailbox::new(&longest_addr, Some(&quoted_worst)).expect("a maximal from"),
        to: vec![
            Mailbox::new(&longest_addr, Some(&encoded_worst)).expect("a maximal to"),
            Mailbox::new(&longest_addr, Some(&quoted_worst)).expect("a maximal to"),
        ],
        cc: vec![Mailbox::new(&longest_addr, None).expect("a maximal cc")],
        // `MAX_SUBJECT` octets of 4-octet characters — the most expensive
        // subject `DraftStore` will accept.
        subject: "\u{1F600}".repeat(super::super::MAX_SUBJECT / 4),
        body_text: "Body".to_owned(),
        attachments: vec![DraftAttachment {
            id: 1,
            filename: "\u{1F600}".repeat(super::super::MAX_FILENAME / 4),
            content_type: format!(
                "{}/{}",
                "x".repeat(super::super::MAX_CONTENT_TYPE / 2 - 1),
                "y".repeat(super::super::MAX_CONTENT_TYPE / 2 - 1)
            ),
            size: 4,
            content: b"data".to_vec(),
        }],
        in_reply_to: Some(format!("{}@example.com", "z".repeat(MAX_MESSAGE_ID - 20))),
        references: (0..80)
            .map(|n| format!("id{n}.{}@example.com", "w".repeat(MAX_MESSAGE_ID - 40)))
            .collect(),
        ..draft()
    }
}

#[test]
fn a_message_at_every_documented_maximum_stays_within_the_line_limit() {
    // This is the test that makes `join_folded`'s "the callers all bound
    // their token lengths" doc comment a fact rather than an aspiration.
    let rendered = render(&maximal_draft());
    for (index, line) in rendered.split(|&b| b == b'\n').enumerate() {
        let len = line.strip_suffix(b"\r").unwrap_or(line).len();
        assert!(
            len <= MAX_LINE,
            "line {} is {len} octets, over the {MAX_LINE}-octet limit:\n{}",
            index + 1,
            String::from_utf8_lossy(line)
        );
    }
    // And it is still a message, not just a short-lined blob.
    let parsed = parse_message(&rendered);
    assert!(parsed.from_addr.is_some());
    assert_eq!(parsed.attachments.len(), 1);
}

#[test]
fn a_display_name_and_its_address_can_fold_apart() {
    // A long display name plus a long addr-spec is the combination that has
    // no single fold point unless the two are separate tokens.
    let name = "Ünicode".repeat(40);
    let addr = format!("{}@{}.example", "a".repeat(64), "b".repeat(200));
    let draft = Draft {
        to: vec![Mailbox::new(&addr, Some(&name)).expect("a valid mailbox")],
        ..draft()
    };
    let rendered = text(&draft);
    let headers = unfolded_headers(&rendered);
    let to = header(&headers, "To").expect("To header");
    assert!(to.ends_with(&format!("<{addr}>")), "{to:?}");
    // The comma separator belongs to the address, not to the tokens inside
    // it: a fold within one address must never introduce one.
    assert!(!to.contains(','), "a single address grew a comma: {to:?}");
    let parsed = MessageParser::default()
        .parse(rendered.as_bytes())
        .expect("parses");
    assert_eq!(
        parsed
            .to()
            .and_then(Address::first)
            .and_then(|a| a.address()),
        Some(addr.as_str())
    );
}

#[test]
fn an_over_long_content_type_falls_back_rather_than_overflowing_the_line() {
    // The renderer's own guard, independent of `DraftStore`'s validation:
    // `Content-Type` is one unfoldable token, so a value that reached a
    // `Draft` some other way must not become a 1000-octet line.
    let draft = Draft {
        attachments: vec![attachment(
            "x.bin",
            &format!("{}/{}", "a".repeat(500), "b".repeat(500)),
            b"bytes",
        )],
        ..draft()
    };
    let rendered = text(&draft);
    assert!(
        rendered.contains("Content-Type: application/octet-stream"),
        "{rendered}"
    );
}

#[test]
fn a_literal_encoded_word_is_re_encoded_rather_than_passed_through() {
    // Otherwise the recipient's client decodes the author's literal text:
    // a subject of `=?utf-8?B?QmFuayBvZiBBbWVyaWNh?=` would *display* as
    // "Bank of America". prd.md has Claude drafting from untrusted mail, so
    // this is a reachable spoof, not a curiosity.
    let spoof = "=?utf-8?B?QmFuayBvZiBBbWVyaWNh?=";
    let draft = Draft {
        subject: spoof.to_owned(),
        from: Mailbox::new("mallory@example.com", Some(spoof)).expect("a valid mailbox"),
        ..draft()
    };
    let rendered = text(&draft);
    assert!(
        !rendered.contains(spoof),
        "the literal encoded-word was passed through:\n{rendered}"
    );
    let parsed = parse_message(rendered.as_bytes());
    assert_eq!(parsed.subject.as_deref(), Some(spoof));
    assert_eq!(parsed.from_name.as_deref(), Some(spoof));
}

#[test]
fn a_leading_dot_is_never_left_in_a_7bit_body() {
    // `.` alone on a line is what terminates SMTP `DATA`. The
    // quoted-printable path escapes it; `7bit` has no escaping mechanism, so
    // the only protection is to not claim `7bit`.
    let draft = Draft {
        body_text: "ok\n.\nstill ok".to_owned(),
        ..draft()
    };
    let rendered = text(&draft);
    assert!(
        !rendered.contains("Content-Transfer-Encoding: 7bit"),
        "{rendered}"
    );
    assert_eq!(
        parse_message(rendered.as_bytes())
            .body_text
            .as_deref()
            .map(str::trim_end),
        Some("ok\r\n.\r\nstill ok")
    );
}

#[test]
fn every_line_ends_with_crlf() {
    // A lone LF is an SMTP protocol violation, and the one that survives
    // review most easily because every local tool renders it identically.
    let rendered = render(&adversarial_draft());
    for (index, &byte) in rendered.iter().enumerate() {
        if byte == b'\n' {
            assert_eq!(
                rendered.get(index.wrapping_sub(1)),
                Some(&b'\r'),
                "bare LF at offset {index}"
            );
        }
    }
    assert!(
        rendered.ends_with(CRLF),
        "message must end on a line boundary"
    );
}

#[test]
fn the_rendered_message_is_always_ascii() {
    // Not decoration: it is what makes "7bit is only claimed when true"
    // checkable, and what the `text()` helper above relies on.
    let rendered = render(&adversarial_draft());
    assert!(rendered.iter().all(u8::is_ascii));
}

// ---------------------------------------------------------------------------
// Round trips
// ---------------------------------------------------------------------------

#[test]
fn a_plain_text_message_round_trips() {
    let draft = draft();
    let parsed = parse_message(&render(&draft));

    assert_eq!(parsed.subject.as_deref(), Some("Lunch"));
    assert_eq!(parsed.from_addr.as_deref(), Some("alice@example.com"));
    assert_eq!(parsed.from_name.as_deref(), Some("Alice"));
    assert_eq!(parsed.to_addrs.as_deref(), Some("bob@example.net"));
    assert_eq!(parsed.message_id.as_deref(), Some("fixed.id@example.com"));
    assert_eq!(
        parsed.body_text.as_deref().map(str::trim_end),
        Some("Shall we say noon?")
    );
    // The `Date` survives as the instant it names, offset included.
    assert_eq!(
        parsed.date,
        Some(
            chrono::DateTime::parse_from_rfc3339("2026-08-05T09:30:00-07:00")
                .expect("fixed date")
                .timestamp()
        )
    );
    assert!(parsed.attachments.is_empty());
}

#[test]
fn a_non_ascii_subject_round_trips_through_an_encoded_word() {
    let draft = Draft {
        subject: "Café: résumé für Ünicode ✓".to_owned(),
        ..draft()
    };
    let rendered = text(&draft);

    // Encoded on the wire...
    let headers = unfolded_headers(&rendered);
    let subject = header(&headers, "Subject").expect("Subject header");
    assert!(
        subject.starts_with("=?utf-8?"),
        "non-ASCII subject must be an encoded-word, got {subject:?}"
    );
    // ...and decoded on arrival.
    assert_eq!(
        parse_message(rendered.as_bytes()).subject.as_deref(),
        Some("Café: résumé für Ünicode ✓")
    );
}

#[test]
fn a_long_non_ascii_subject_splits_into_several_encoded_words_and_still_round_trips() {
    // The case that breaks naive encoders: one encoded-word may not exceed 75
    // octets, and a chunk boundary must never fall inside a UTF-8 sequence.
    let subject = "Übergrößenträger ".repeat(12).trim_end().to_owned();
    let draft = Draft {
        subject: subject.clone(),
        ..draft()
    };
    let rendered = text(&draft);
    let headers = unfolded_headers(&rendered);
    let raw = header(&headers, "Subject").expect("Subject header");
    assert!(
        raw.matches("=?utf-8?").count() > 1,
        "expected several encoded-words, got {raw:?}"
    );
    for word in raw.split_whitespace() {
        assert!(
            word.len() <= MAX_ENCODED_WORD,
            "encoded-word {word:?} is {} octets, over the RFC 2047 limit",
            word.len()
        );
    }
    assert_eq!(
        parse_message(rendered.as_bytes()).subject.as_deref(),
        Some(subject.as_str())
    );
}

#[test]
fn a_non_ascii_display_name_is_encoded_but_the_addr_spec_never_is() {
    let draft = Draft {
        from: mailbox("Café Ünicode <cafe@example.com>"),
        to: vec![mailbox("Björn Åström <bjorn@example.net>")],
        ..draft()
    };
    let rendered = text(&draft);
    let headers = unfolded_headers(&rendered);

    let from = header(&headers, "From").expect("From header");
    assert!(
        from.contains("=?utf-8?"),
        "display name must be encoded: {from:?}"
    );
    assert!(
        from.contains("<cafe@example.com>"),
        "the addr-spec must appear verbatim: {from:?}"
    );
    // RFC 2047 §5: an encoded-word must not sit inside a quoted-string, or it
    // is displayed literally.
    assert!(
        !from.contains("\"=?utf-8?"),
        "an encoded-word must never be quoted: {from:?}"
    );

    let parsed = parse_message(rendered.as_bytes());
    assert_eq!(parsed.from_name.as_deref(), Some("Café Ünicode"));
    assert_eq!(parsed.from_addr.as_deref(), Some("cafe@example.com"));
    assert_eq!(parsed.to_addrs.as_deref(), Some("bjorn@example.net"));
}

#[test]
fn a_display_name_with_specials_is_quoted_and_round_trips() {
    let draft = Draft {
        to: vec![mailbox(r#""Doe, Jane" <jane@example.net>"#)],
        ..draft()
    };
    let rendered = text(&draft);
    assert!(
        rendered.contains(r#""Doe, Jane" <jane@example.net>"#),
        "{rendered}"
    );

    let parsed = MessageParser::default()
        .parse(rendered.as_bytes())
        .expect("parses");
    let to = parsed.to().and_then(Address::first).expect("a To address");
    assert_eq!(to.name(), Some("Doe, Jane"));
    assert_eq!(to.address(), Some("jane@example.net"));
}

#[test]
fn text_and_html_become_multipart_alternative_and_both_survive() {
    let draft = Draft {
        body_html: Some("<p>Shall we say <b>noon</b>?</p>".to_owned()),
        ..draft()
    };
    let rendered = text(&draft);
    assert!(
        rendered.contains("Content-Type: multipart/alternative;"),
        "{rendered}"
    );
    assert!(!rendered.contains("multipart/mixed"));

    let parsed = parse_message(rendered.as_bytes());
    assert_eq!(
        parsed.body_text.as_deref().map(str::trim_end),
        Some("Shall we say noon?")
    );
    assert_eq!(
        parsed.body_html.as_deref().map(str::trim_end),
        Some("<p>Shall we say <b>noon</b>?</p>")
    );
}

#[test]
fn attachments_alone_become_multipart_mixed_with_their_bytes_intact() {
    let bytes: Vec<u8> = (0u8..=255).cycle().take(5000).collect();
    let draft = Draft {
        attachments: vec![attachment("doc.bin", "application/octet-stream", &bytes)],
        ..draft()
    };
    let rendered = render(&draft);
    let rendered_text = String::from_utf8(rendered.clone()).expect("ASCII");
    assert!(rendered_text.contains("Content-Type: multipart/mixed;"));
    assert!(!rendered_text.contains("multipart/alternative"));

    let parsed = MessageParser::default().parse(&rendered).expect("parses");
    let attachments: Vec<_> = parsed.attachments().collect();
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments[0].attachment_name(), Some("doc.bin"));
    assert_eq!(
        attachments[0].contents(),
        bytes.as_slice(),
        "binary attachment bytes must survive base64 exactly"
    );
}

#[test]
fn text_html_and_attachments_nest_mixed_around_alternative() {
    // The structure the acceptance criterion names, and the one that is wrong
    // in most hand-rolled builders: an attachment placed inside the
    // `alternative` reads as "an alternative rendering of the body".
    let pdf = b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\ntrailer\n".to_vec();
    let draft = Draft {
        subject: "Rapport für Q3 — résumé".to_owned(),
        body_text: "Le rapport est joint.".to_owned(),
        body_html: Some("<p>Le rapport est <i>joint</i>.</p>".to_owned()),
        attachments: vec![
            attachment("rapport.pdf", "application/pdf", &pdf),
            attachment("notes.txt", "text/plain", b"plain notes"),
        ],
        cc: vec![mailbox("Carol <carol@example.org>")],
        ..draft()
    };
    let rendered = render(&draft);
    let as_text = String::from_utf8(rendered.clone()).expect("ASCII");

    // Nesting: `mixed` is the outer type, `alternative` appears after it.
    let mixed = as_text
        .find("Content-Type: multipart/mixed;")
        .expect("a mixed container");
    let alternative = as_text
        .find("Content-Type: multipart/alternative;")
        .expect("an alternative container");
    assert!(
        mixed < alternative,
        "multipart/mixed must wrap multipart/alternative, not the other way round"
    );

    let parsed = parse_message(&rendered);
    assert_eq!(parsed.subject.as_deref(), Some("Rapport für Q3 — résumé"));
    assert_eq!(parsed.to_addrs.as_deref(), Some("bob@example.net"));
    assert_eq!(parsed.cc_addrs.as_deref(), Some("carol@example.org"));
    assert_eq!(
        parsed.body_text.as_deref().map(str::trim_end),
        Some("Le rapport est joint.")
    );
    assert_eq!(
        parsed.body_html.as_deref().map(str::trim_end),
        Some("<p>Le rapport est <i>joint</i>.</p>")
    );

    let raw = MessageParser::default().parse(&rendered).expect("parses");
    let attachments: Vec<_> = raw.attachments().collect();
    assert_eq!(attachments.len(), 2, "both attachments survive");
    assert_eq!(attachments[0].attachment_name(), Some("rapport.pdf"));
    assert_eq!(attachments[0].contents(), pdf.as_slice());
    assert_eq!(attachments[1].attachment_name(), Some("notes.txt"));
    assert_eq!(attachments[1].contents(), b"plain notes");
}

#[test]
fn a_non_ascii_filename_round_trips_through_rfc_2231() {
    let draft = Draft {
        attachments: vec![attachment("réport — 2026.pdf", "application/pdf", b"pdf")],
        ..draft()
    };
    let rendered = render(&draft);
    let as_text = String::from_utf8(rendered.clone()).expect("ASCII");
    assert!(
        as_text.contains("filename*=utf-8''"),
        "a non-ASCII filename uses RFC 2231's extended syntax: {as_text}"
    );

    let raw = MessageParser::default().parse(&rendered).expect("parses");
    let attachment = raw.attachments().next().expect("one attachment");
    assert_eq!(attachment.attachment_name(), Some("réport — 2026.pdf"));
}

// ---------------------------------------------------------------------------
// Threading
// ---------------------------------------------------------------------------

#[test]
fn a_reply_carries_in_reply_to_and_the_appended_references_chain() {
    let draft = Draft {
        in_reply_to: Some("parent@example.com".to_owned()),
        references: vec![
            "root@example.com".to_owned(),
            "middle@example.com".to_owned(),
            "parent@example.com".to_owned(),
        ],
        ..draft()
    };
    let parsed = parse_message(&render(&draft));
    assert_eq!(parsed.in_reply_to.as_deref(), Some("parent@example.com"));
    assert_eq!(
        parsed.references.as_deref(),
        Some("root@example.com middle@example.com parent@example.com")
    );
}

#[test]
fn a_parent_with_no_references_still_produces_a_chain() {
    // The first reply in a thread: the parent contributes only its own id,
    // and dropping it would start a second conversation.
    let chain = reply_references(&[], &[], Some("root@example.com"));
    assert_eq!(chain, vec!["root@example.com".to_owned()]);
}

#[test]
fn a_parent_with_only_in_reply_to_does_not_lose_the_grandparent() {
    let chain = reply_references(
        &[],
        &["grandparent@example.com".to_owned()],
        Some("parent@example.com"),
    );
    assert_eq!(
        chain,
        vec![
            "grandparent@example.com".to_owned(),
            "parent@example.com".to_owned()
        ]
    );
}

#[test]
fn references_wins_over_in_reply_to_and_duplicates_collapse() {
    // RFC 5322 §3.6.4 builds on the parent's References; In-Reply-To is only
    // the fallback. A parent that repeats its own id in the chain must not
    // produce a doubled link.
    let chain = reply_references(
        &[
            "root@example.com".to_owned(),
            "parent@example.com".to_owned(),
        ],
        &["ignored@example.com".to_owned()],
        Some("parent@example.com"),
    );
    assert_eq!(
        chain,
        vec![
            "root@example.com".to_owned(),
            "parent@example.com".to_owned()
        ]
    );
}

#[test]
fn a_parent_with_no_message_id_contributes_nothing_new() {
    let chain = reply_references(&["root@example.com".to_owned()], &[], None);
    assert_eq!(chain, vec!["root@example.com".to_owned()]);
}

#[test]
fn references_are_capped_keeping_the_root_and_the_most_recent_ancestors() {
    let chain: Vec<String> = (0..60).map(|n| format!("id{n}@example.com")).collect();
    let draft = Draft {
        references: chain.clone(),
        ..draft()
    };
    let parsed = parse_message(&render(&draft));
    let emitted: Vec<&str> = parsed
        .references
        .as_deref()
        .expect("References header")
        .split_whitespace()
        .collect();

    assert_eq!(emitted.len(), MAX_REFERENCES);
    assert_eq!(emitted[0], "id0@example.com", "the thread root survives");
    assert_eq!(
        emitted[MAX_REFERENCES - 1],
        "id59@example.com",
        "the immediate parent survives"
    );
    assert_eq!(
        emitted[1], "id41@example.com",
        "the middle is what is dropped"
    );
}

#[test]
fn an_unfoldably_long_parent_id_is_dropped_rather_than_emitted() {
    // The one input that could push a header past 998 octets: folding cannot
    // break inside a single `<...>` token, and the id comes from whoever sent
    // the parent.
    let absurd = format!("{}@example.com", "z".repeat(MAX_MESSAGE_ID));
    let draft = Draft {
        in_reply_to: Some(absurd.clone()),
        references: vec!["root@example.com".to_owned(), absurd],
        ..draft()
    };
    let rendered = text(&draft);
    let headers = unfolded_headers(&rendered);
    assert_eq!(header(&headers, "In-Reply-To"), None);
    assert_eq!(header(&headers, "References"), Some("<root@example.com>"));
}

// ---------------------------------------------------------------------------
// Transfer encoding
// ---------------------------------------------------------------------------

#[test]
fn seven_bit_is_claimed_only_when_the_body_really_is() {
    let plain = Draft {
        body_text: "Short ASCII line.\nAnother one.".to_owned(),
        ..draft()
    };
    assert!(text(&plain).contains("Content-Transfer-Encoding: 7bit"));

    // Each of these is 7-bit *in the naive sense* and still must not claim it.
    for body in [
        "x".repeat(SOFT_LINE + 1),           // relays wrap over-long lines
        "trailing space   ".to_owned(),      // relays strip trailing whitespace
        "From the desk of Alice".to_owned(), // mbox rewrites a leading "From "
        "a\u{7f}b".to_owned(),               // DEL is not printable
    ] {
        let draft = Draft {
            body_text: body.clone(),
            ..draft()
        };
        let rendered = text(&draft);
        assert!(
            !rendered.contains("Content-Transfer-Encoding: 7bit"),
            "{body:?} must not be labelled 7bit:\n{rendered}"
        );
        // And whatever it *is* labelled, it must decode back to the original.
        assert_eq!(
            parse_message(rendered.as_bytes())
                .body_text
                .as_deref()
                .map(str::trim_end),
            Some(body.trim_end()),
            "{body:?} must survive its encoding"
        );
    }
}

#[test]
fn mostly_ascii_text_uses_quoted_printable_and_round_trips() {
    let body = "Price: €10 = cheap.\nA line with a trailing space \nand an = sign.";
    let draft = Draft {
        body_text: body.to_owned(),
        ..draft()
    };
    let rendered = text(&draft);
    assert!(
        rendered.contains("Content-Transfer-Encoding: quoted-printable"),
        "{rendered}"
    );
    assert!(
        !rendered.contains("Content-Transfer-Encoding: base64"),
        "mostly-ASCII text should stay readable in the raw source"
    );
    // Compared against the CRLF-normalized form: a body is canonicalized
    // before encoding (see `normalize_crlf`), so `\n` on the way in is
    // `\r\n` on the way out — by design, not by accident.
    assert_eq!(
        parse_message(rendered.as_bytes())
            .body_text
            .as_deref()
            .map(str::trim_end),
        Some(body.replace('\n', "\r\n").as_str())
    );
}

#[test]
fn heavily_non_ascii_text_uses_base64_and_round_trips() {
    let body =
        "日本語のテキストです。これはかなり長い本文で、quoted-printable では三倍に膨らみます。"
            .repeat(4);
    let draft = Draft {
        body_text: body.clone(),
        ..draft()
    };
    let rendered = text(&draft);
    assert!(
        rendered.contains("Content-Transfer-Encoding: base64"),
        "{rendered}"
    );
    assert_eq!(
        parse_message(rendered.as_bytes())
            .body_text
            .as_deref()
            .map(str::trim_end),
        Some(body.as_str())
    );
}

#[test]
fn quoted_printable_lines_stay_within_the_rfc_2045_limit() {
    let draft = Draft {
        body_text: format!("Café {}", "long ".repeat(400)),
        ..draft()
    };
    let rendered = text(&draft);
    let body = rendered.split("\r\n\r\n").nth(1).expect("a body");
    for line in body.split("\r\n") {
        assert!(
            line.len() <= MAX_QP_LINE + 1,
            "quoted-printable line is {} octets: {line:?}",
            line.len()
        );
    }
}

#[test]
fn no_quoted_printable_line_ends_in_whitespace() {
    // RFC 2045 rule #3, and the half of it that is easy to get wrong: the
    // rule covers a line ended by a *soft break*, not only the end of the
    // author's line. A space left sitting before the `=` is stripped by any
    // relay that trims trailing whitespace, and the author's word spacing
    // quietly disappears. The body below puts a space at every column, so
    // some of them necessarily land on a wrap point.
    let spaced: String = (0..400).map(|n| format!("w{n} ")).collect();
    let draft = Draft {
        body_text: format!("é {spaced}"),
        ..draft()
    };
    let rendered = text(&draft);
    assert!(rendered.contains("Content-Transfer-Encoding: quoted-printable"));
    let body = rendered.split("\r\n\r\n").nth(1).expect("a body");

    for line in body.split("\r\n") {
        // The last octet of an encoded line is the soft-break `=` when there
        // is one; what must not be whitespace is the octet it terminates.
        let content = line.strip_suffix('=').unwrap_or(line);
        assert!(
            !content.ends_with(' ') && !content.ends_with('\t'),
            "encoded line ends in whitespace: {line:?}"
        );
    }
    // And the spacing survives the round trip unchanged.
    assert_eq!(
        parse_message(rendered.as_bytes())
            .body_text
            .as_deref()
            .map(str::trim_end),
        Some(format!("é {spaced}").trim_end())
    );
}

#[test]
fn mixed_line_endings_are_normalized_to_crlf_before_encoding() {
    let draft = Draft {
        // A body carrying every ending a client might send, plus non-ASCII so
        // it goes through an encoder rather than straight out as 7bit.
        body_text: "café\nlf\r\ncrlf\rcr".to_owned(),
        ..draft()
    };
    let parsed = parse_message(&render(&draft));
    assert_eq!(
        parsed.body_text.as_deref().map(str::trim_end),
        Some("café\r\nlf\r\ncrlf\r\ncr")
    );
}

#[test]
fn an_empty_body_is_still_a_valid_message() {
    let draft = Draft {
        body_text: String::new(),
        ..draft()
    };
    let parsed = parse_message(&render(&draft));
    assert_eq!(parsed.subject.as_deref(), Some("Lunch"));
    assert_eq!(parsed.body_text.as_deref().unwrap_or_default().trim(), "");
}

// ---------------------------------------------------------------------------
// Boundaries, Bcc, identity, injection
// ---------------------------------------------------------------------------

#[test]
fn the_boundary_never_occurs_inside_a_part() {
    // A boundary that appears in the content truncates the message at exactly
    // that point, silently.
    let draft = Draft {
        body_text: "boundary bait: ----=_rmail_deadbeef".to_owned(),
        attachments: vec![attachment("a.txt", "text/plain", b"----=_rmail_deadbeef")],
        ..draft()
    };
    let rendered = String::from_utf8(render(&draft)).expect("ASCII");
    let boundary = rendered
        .split("boundary=\"")
        .nth(1)
        .and_then(|rest| rest.split('"').next())
        .expect("a boundary parameter");
    assert_eq!(
        rendered.matches(&format!("--{boundary}")).count(),
        3,
        "exactly two delimiters and one terminator, i.e. no collision with content"
    );
    // And it still parses back with both parts intact.
    let parsed = MessageParser::default()
        .parse(rendered.as_bytes())
        .expect("parses");
    assert_eq!(parsed.attachments().count(), 1);
}

#[test]
fn bcc_recipients_never_appear_in_the_rendered_message() {
    let draft = Draft {
        to: vec![mailbox("bob@example.net")],
        bcc: vec![mailbox("Secret Watcher <secret@example.org>")],
        ..draft()
    };
    let rendered = text(&draft);
    assert!(
        !rendered.contains("secret@example.org"),
        "a blind recipient that appears in the message is not blind:\n{rendered}"
    );
    assert!(!rendered.to_ascii_lowercase().contains("\r\nbcc:"));
    // But the submission path still learns about them.
    assert_eq!(
        draft.envelope_recipients(),
        vec![
            "bob@example.net".to_owned(),
            "secret@example.org".to_owned()
        ]
    );
}

#[test]
fn a_bcc_only_draft_still_renders() {
    // Legal, and the shape a "blind announcement" takes.
    let draft = Draft {
        to: Vec::new(),
        bcc: vec![mailbox("secret@example.org")],
        ..draft()
    };
    let rendered = text(&draft);
    let headers = unfolded_headers(&rendered);
    assert_eq!(header(&headers, "To"), None);
    assert_eq!(header(&headers, "Cc"), None);
    assert!(header(&headers, "From").is_some());
}

#[test]
fn a_draft_with_no_recipient_at_all_is_invalid_argument() {
    let draft = Draft {
        to: Vec::new(),
        cc: Vec::new(),
        bcc: Vec::new(),
        ..draft()
    };
    let err = build(&draft, &envelope()).expect_err("no recipients");
    assert_eq!(err.reason(), ErrorReason::InvalidArgument);
}

#[test]
fn header_injection_through_a_subject_cannot_add_a_header() {
    // `DraftStore` rejects control characters outright; this proves the
    // renderer is safe on its own, for text that reached a `Draft` some other
    // way (an AI-generated draft, a future import path).
    let draft = Draft {
        subject: "Hi\r\nBcc: victim@example.org\r\nX-Evil: yes".to_owned(),
        ..draft()
    };
    let rendered = text(&draft);
    let headers = unfolded_headers(&rendered);
    assert!(
        !headers
            .iter()
            .any(|h| h.starts_with("Bcc:") || h.starts_with("X-Evil:")),
        "injected headers appeared: {headers:?}"
    );
    assert!(!rendered.contains("victim@example.org\r\n"));
}

#[test]
fn a_message_id_is_unique_and_uses_the_sending_domain() {
    let a = generate_message_id("example.com");
    let b = generate_message_id("example.com");
    assert_ne!(a, b, "two sends must never share an id");
    assert!(a.ends_with("@example.com"), "{a}");
    assert!(!a.contains(' ') && !a.contains('<') && !a.contains('>'));

    // A domain that cannot go in an id falls back to a reserved TLD rather
    // than producing something that impersonates a real host.
    assert!(generate_message_id("").ends_with("@rmail.invalid"));
    assert!(generate_message_id("not a domain").ends_with("@rmail.invalid"));
}

#[test]
fn the_envelope_message_id_is_what_reaches_the_header() {
    // Task 61 persists this id before SMTP `DATA` for at-most-once delivery,
    // so "the id we recorded" and "the id we sent" must be the same string.
    let envelope = envelope();
    let rendered = String::from_utf8(build(&draft(), &envelope).expect("renders")).expect("ASCII");
    assert!(rendered.contains(&format!("Message-ID: <{}>", envelope.message_id())));
    assert_eq!(
        parse_message(rendered.as_bytes()).message_id.as_deref(),
        Some(envelope.message_id())
    );
}

#[test]
fn an_unparseable_content_type_falls_back_to_octet_stream() {
    let draft = Draft {
        attachments: vec![attachment("x.bin", "not a content type", b"bytes")],
        ..draft()
    };
    let rendered = text(&draft);
    assert!(
        rendered.contains("Content-Type: application/octet-stream"),
        "{rendered}"
    );
}

#[test]
fn a_content_type_parameter_cannot_smuggle_a_header() {
    let draft = Draft {
        attachments: vec![attachment("x.bin", "text/plain;\r\nX-Evil: yes", b"bytes")],
        ..draft()
    };
    let rendered = text(&draft);
    assert!(!rendered.contains("X-Evil"), "{rendered}");
    assert!(rendered.contains("Content-Type: text/plain"), "{rendered}");
}

#[test]
fn addresses_fold_between_recipients_and_never_inside_one() {
    let draft = Draft {
        to: (0..12)
            .map(|n| {
                mailbox(&format!(
                    "recipient-number-{n}@quite-a-long-domain.example.com"
                ))
            })
            .collect(),
        ..draft()
    };
    let rendered = text(&draft);
    let block = rendered.split("\r\n\r\n").next().unwrap_or_default();
    for line in block.split("\r\n") {
        assert!(line.len() <= MAX_LINE);
    }
    // Every address still arrives whole.
    let parsed = MessageParser::default()
        .parse(rendered.as_bytes())
        .expect("parses");
    let to = parsed.to().expect("To");
    assert_eq!(to.iter().count(), 12);
    for (index, addr) in to.iter().enumerate() {
        assert_eq!(
            addr.address(),
            Some(format!("recipient-number-{index}@quite-a-long-domain.example.com").as_str())
        );
    }
}

#[test]
fn a_long_ascii_token_in_a_subject_stays_readable_rather_than_encoded() {
    // A URL in a subject is legible as-is and illegible once RFC 2047-encoded.
    let url = format!("https://example.com/{}", "path/".repeat(30));
    let draft = Draft {
        subject: format!("See {url}"),
        ..draft()
    };
    let rendered = text(&draft);
    assert!(rendered.contains(&url), "{rendered}");
    assert!(!rendered.contains("=?utf-8?"));
    assert_eq!(
        parse_message(rendered.as_bytes()).subject.as_deref(),
        Some(format!("See {url}").as_str())
    );
}
