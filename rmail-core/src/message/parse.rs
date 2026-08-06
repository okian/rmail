//! RFC822 parsing via `mail-parser` into owned, storable metadata.

use mail_parser::{Address, ContentType, MessageParser, MimeHeaders};

/// Parsed attachment metadata (bytes are not retained here).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedAttachment {
    /// MIME part index/path.
    pub part_id: Option<String>,
    /// Filename, if any.
    pub filename: Option<String>,
    /// `type/subtype`.
    pub content_type: Option<String>,
    /// Decoded content length in bytes.
    pub size: Option<i64>,
    /// `Content-ID` (inline references).
    pub content_id: Option<String>,
    /// Whether the part is inline.
    pub is_inline: bool,
}

/// The storable projection of a parsed message: headers, body text, HTML, and
/// attachment metadata. All fields are owned so the parse's borrow of the raw
/// bytes does not escape.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedMessage {
    /// RFC822 `Message-ID`.
    pub message_id: Option<String>,
    /// `In-Reply-To` (ids joined by space).
    pub in_reply_to: Option<String>,
    /// `References` chain (ids joined by space).
    pub references: Option<String>,
    /// Decoded subject.
    pub subject: Option<String>,
    /// Primary From address.
    pub from_addr: Option<String>,
    /// From display name.
    pub from_name: Option<String>,
    /// To addresses, comma-joined.
    pub to_addrs: Option<String>,
    /// Cc addresses, comma-joined.
    pub cc_addrs: Option<String>,
    /// `Date` header as a unix timestamp (seconds).
    pub date: Option<i64>,
    /// Plain-text body (the text/plain part, or HTML stripped to text).
    pub body_text: Option<String>,
    /// Raw HTML body, if present.
    pub body_html: Option<String>,
    /// Attachment metadata.
    pub attachments: Vec<ParsedAttachment>,
}

impl ParsedMessage {
    /// Whether the message carries any attachments.
    #[must_use]
    pub fn has_attachments(&self) -> bool {
        !self.attachments.is_empty()
    }
}

/// Parse raw RFC822 bytes into a [`ParsedMessage`], best-effort: an unparsable
/// message yields an all-empty result rather than an error (the raw is still
/// stored by the caller).
#[must_use]
pub fn parse_message(raw: &[u8]) -> ParsedMessage {
    let Some(msg) = MessageParser::default().parse(raw) else {
        return ParsedMessage::default();
    };

    let from = msg.from().and_then(Address::first);
    let body_html = msg.body_html(0).map(|c| c.into_owned());
    let body_text = msg
        .body_text(0)
        .map(|c| c.into_owned())
        .or_else(|| body_html.as_deref().map(strip_html));

    let attachments = msg
        .attachments()
        .enumerate()
        .map(|(index, part)| ParsedAttachment {
            part_id: Some(index.to_string()),
            filename: part.attachment_name().map(str::to_owned),
            content_type: part.content_type().map(format_content_type),
            size: i64::try_from(part.contents().len()).ok(),
            content_id: part.content_id().map(str::to_owned),
            is_inline: part
                .content_disposition()
                .is_some_and(ContentType::is_inline),
        })
        .collect();

    ParsedMessage {
        message_id: msg.message_id().map(str::to_owned),
        in_reply_to: join_ids(msg.in_reply_to()),
        references: join_ids(msg.references()),
        subject: msg.subject().map(str::to_owned),
        from_addr: from.and_then(|a| a.address()).map(str::to_owned),
        from_name: from.and_then(|a| a.name()).map(str::to_owned),
        to_addrs: join_addresses(msg.to()),
        cc_addrs: join_addresses(msg.cc()),
        date: msg.date().map(mail_parser::DateTime::to_timestamp),
        body_text,
        body_html,
        attachments,
    }
}

/// Join an address header's addresses into a comma-separated string.
fn join_addresses(address: Option<&Address<'_>>) -> Option<String> {
    let address = address?;
    let joined: Vec<&str> = address
        .iter()
        .filter_map(mail_parser::Addr::address)
        .collect();
    if joined.is_empty() {
        None
    } else {
        Some(joined.join(", "))
    }
}

/// Join a message-id header value (single or list) into a space-separated string.
fn join_ids(value: &mail_parser::HeaderValue<'_>) -> Option<String> {
    let ids = value.as_text_list()?;
    if ids.is_empty() {
        None
    } else {
        Some(ids.join(" "))
    }
}

/// `type/subtype`, lowercased by mail-parser.
///
/// `pub(crate)`: `mail::attachment_bytes` (task 39) reads a single attachment
/// straight off `messages.raw` for streaming and needs the same
/// `type/subtype` formatting `parse_message` uses, rather than a second,
/// possibly-drifting copy of it.
pub(crate) fn format_content_type(content_type: &ContentType<'_>) -> String {
    match content_type.subtype() {
        Some(subtype) => format!("{}/{}", content_type.ctype(), subtype),
        None => content_type.ctype().to_owned(),
    }
}

/// Strip HTML to plain text for indexing, best-effort.
///
/// `html2text::from_read` *panics* on content it can't render at the given
/// width (e.g. deeply nested blockquotes/lists → `TooNarrow`). Since this runs
/// on attacker-controlled email, use the fallible API and fall back to empty
/// text on any render error rather than panicking.
fn strip_html(html: &str) -> String {
    html2text::config::plain()
        .string_from_read(html.as_bytes(), 120)
        .map(|text| text.trim().to_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIMPLE: &[u8] = b"From: Alice <alice@example.com>\r\n\
To: bob@example.com\r\n\
Subject: Hello\r\n\
Message-ID: <simple@example.com>\r\n\
Date: Mon, 1 Jan 2024 12:00:00 +0000\r\n\
\r\n\
Hello, world.\r\n";

    #[test]
    fn parses_simple_headers_and_body() {
        let p = parse_message(SIMPLE);
        assert_eq!(p.subject.as_deref(), Some("Hello"));
        assert_eq!(p.from_addr.as_deref(), Some("alice@example.com"));
        assert_eq!(p.from_name.as_deref(), Some("Alice"));
        assert_eq!(p.to_addrs.as_deref(), Some("bob@example.com"));
        assert_eq!(p.message_id.as_deref(), Some("simple@example.com"));
        assert!(p
            .body_text
            .as_deref()
            .unwrap_or_default()
            .contains("Hello, world."));
        assert!(p.date.is_some());
        assert!(!p.has_attachments());
    }

    #[test]
    fn decodes_encoded_subject_and_threading_headers() {
        let raw = b"From: a@example.com\r\n\
Subject: =?UTF-8?B?SGVsbG8gV29ybGQ=?=\r\n\
In-Reply-To: <parent@example.com>\r\n\
References: <root@example.com> <parent@example.com>\r\n\
\r\n\
body\r\n";
        let p = parse_message(raw);
        assert_eq!(
            p.subject.as_deref(),
            Some("Hello World"),
            "RFC2047 subject decoded"
        );
        assert_eq!(p.in_reply_to.as_deref(), Some("parent@example.com"));
        assert_eq!(
            p.references.as_deref(),
            Some("root@example.com parent@example.com")
        );
    }

    #[test]
    fn multipart_alternative_keeps_text_and_html() {
        let raw = b"From: a@example.com\r\n\
Subject: Multi\r\n\
Content-Type: multipart/alternative; boundary=\"b\"\r\n\
\r\n\
--b\r\n\
Content-Type: text/plain\r\n\
\r\n\
plain body\r\n\
--b\r\n\
Content-Type: text/html\r\n\
\r\n\
<p>html <b>body</b></p>\r\n\
--b--\r\n";
        let p = parse_message(raw);
        assert_eq!(p.body_text.as_deref(), Some("plain body"));
        assert!(p
            .body_html
            .as_deref()
            .unwrap_or_default()
            .contains("<b>body</b>"));
    }

    #[test]
    fn html_only_is_stripped_into_body_text() {
        let raw = b"From: a@example.com\r\n\
Subject: HTML-only\r\n\
Content-Type: text/html\r\n\
\r\n\
<html><body><h1>Title</h1><p>Some <i>text</i>.</p></body></html>\r\n";
        let p = parse_message(raw);
        assert!(p.body_html.is_some());
        let text = p.body_text.as_deref().unwrap_or_default();
        assert!(
            text.contains("Title"),
            "stripped text has heading: {text:?}"
        );
        assert!(text.contains("text"), "stripped text has body: {text:?}");
        assert!(!text.contains('<'), "no tags remain: {text:?}");
    }

    #[test]
    fn quoted_printable_body_is_decoded() {
        let raw = b"From: a@example.com\r\n\
Subject: QP\r\n\
Content-Type: text/plain\r\n\
Content-Transfer-Encoding: quoted-printable\r\n\
\r\n\
Price: =E2=82=AC10 =3D cheap\r\n";
        let p = parse_message(raw);
        let body = p.body_text.as_deref().unwrap_or_default();
        assert!(body.contains("€10"), "QP decoded to euro sign: {body:?}");
        assert!(body.contains("= cheap"), "QP =3D decoded: {body:?}");
    }

    #[test]
    fn base64_attachment_is_extracted() {
        // "hello" base64 = aGVsbG8=
        let raw = b"From: a@example.com\r\n\
Subject: WithAttachment\r\n\
Content-Type: multipart/mixed; boundary=\"b\"\r\n\
\r\n\
--b\r\n\
Content-Type: text/plain\r\n\
\r\n\
see attached\r\n\
--b\r\n\
Content-Type: application/pdf; name=\"doc.pdf\"\r\n\
Content-Disposition: attachment; filename=\"doc.pdf\"\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
aGVsbG8=\r\n\
--b--\r\n";
        let p = parse_message(raw);
        assert!(p.has_attachments());
        assert_eq!(p.attachments.len(), 1);
        let att = &p.attachments[0];
        assert_eq!(att.filename.as_deref(), Some("doc.pdf"));
        assert_eq!(att.content_type.as_deref(), Some("application/pdf"));
        assert_eq!(att.size, Some(5), "decoded 'hello' is 5 bytes");
        assert_eq!(p.body_text.as_deref(), Some("see attached"));
    }

    #[test]
    fn unparsable_input_yields_empty() {
        let p = parse_message(b"");
        assert_eq!(p, ParsedMessage::default());
    }

    #[test]
    fn deeply_nested_html_does_not_panic() {
        // Deeply nested blockquotes make html2text's infallible `from_read`
        // panic with TooNarrow; parse_message must not panic and must still
        // produce a message (stripped text may be empty on a render failure).
        let mut html =
            String::from("From: a@example.com\r\nSubject: Deep\r\nContent-Type: text/html\r\n\r\n");
        for _ in 0..40 {
            html.push_str("<blockquote>");
        }
        html.push_str("hello");
        for _ in 0..40 {
            html.push_str("</blockquote>");
        }
        let p = parse_message(html.as_bytes());
        assert!(p.body_html.is_some());
        // body_text is Some (possibly empty) — the point is: no panic.
        assert!(p.body_text.is_some());
    }
}
