//! Conversions between the gRPC wire types and the model's own.
//!
//! # The viewer does not decode anything
//!
//! prd.md asks the message viewer to handle "plain text, multipart,
//! quoted-printable, base64, UTF-8, encoded headers". All of that decoding
//! already exists, and already runs: `rmail_core::message::parse::parse_message`
//! (mail-parser) performs it once, when sync stores the message, and the
//! result is what `messages.body_text`/`body_html` hold and what
//! `MailService.Get` returns as [`FullMessage`].
//!
//! So this module renders that output; it does not re-derive it. A second
//! decoder in the client would be a copy that drifts — the TUI would show a
//! different subject to the one `mail search` matched on and the AI pipeline
//! summarised, for the same message, with no way to tell which was right.
//! The TUI is a gRPC client (prd.md: "UI components never talk to IMAP
//! directly"); the decode belongs on the other side of that boundary and
//! stays there. `tests::viewer_shows_what_parse_message_decoded` pins the
//! seam: it runs a real quoted-printable/base64/RFC 2047 message through
//! `parse_message`, wraps its output exactly as the daemon does, and asserts
//! the decoded text is what reaches the viewer.
//!
//! # Where the plain-text body comes from for an HTML-only message
//!
//! `parse_message` already falls back to HTML stripped to text when a message
//! has no text/plain part, so [`open_message`] has a body to show either way.
//! `has_html` is what lights up "open in browser" for the cases where that
//! stripped text is not enough — see [`crate::tui::html`].

#[cfg(test)]
mod tests;

use rmail_proto::v1::{
    Account as ProtoAccount, Attachment, CreateDraftRequest, DraftAddress, FolderStatus,
    FullMessage, Message as ProtoMessage,
};

use super::{Account, DraftKind, Folder, MessageRow, OpenMessage};

/// How many characters of a quoted body a draft carries.
///
/// A reply quoting a 40 MB mailing-list digest is not a useful draft, and
/// `ComposeService` stores the body verbatim. Truncation is marked, never
/// silent.
const MAX_QUOTED_CHARS: usize = 16_384;

/// What a message with no `Subject` shows as. Empty cells read as a rendering
/// bug; this reads as the message.
const NO_SUBJECT: &str = "(no subject)";

/// Map an account row.
pub fn account(proto: ProtoAccount) -> Account {
    Account {
        id: proto.id,
        name: proto.name,
        username: proto.username,
    }
}

/// Map a folder row.
///
/// The folder pane is built from `SyncService.Status`, which is the one RPC
/// that enumerates an account's mailboxes with their local message counts.
/// There is no `MailService.ListMailboxes` and this task does not add one:
/// the data is already exposed, and a second RPC returning the same rows is
/// exactly the kind of feature drift prd.md's "one core API" rule exists to
/// prevent.
pub fn folder(proto: FolderStatus) -> Folder {
    Folder {
        id: proto.mailbox_id,
        name: proto.name,
        message_count: proto.message_count,
    }
}

/// Map a list row.
pub fn message_row(proto: ProtoMessage) -> MessageRow {
    let from = proto
        .from_name
        .clone()
        .filter(|n| !n.trim().is_empty())
        .or_else(|| proto.from_addr.clone())
        .unwrap_or_else(|| "(unknown sender)".to_owned());
    MessageRow {
        id: proto.id,
        subject: non_empty(proto.subject).unwrap_or_else(|| NO_SUBJECT.to_owned()),
        from,
        from_addr: proto.from_addr,
        date: proto.date.or(proto.internaldate),
        flags: proto.flags,
        has_attachments: proto.has_attachments,
    }
}

/// Project a fetched message into what the viewer draws.
pub fn open_message(full: FullMessage) -> OpenMessage {
    let message = full.message.unwrap_or_default();
    let mut headers = Vec::new();
    push_header(&mut headers, "From", header_from(&message));
    push_header(&mut headers, "To", message.to_addrs.clone());
    push_header(&mut headers, "Cc", message.cc_addrs.clone());
    push_header(
        &mut headers,
        "Subject",
        Some(non_empty(message.subject.clone()).unwrap_or_else(|| NO_SUBJECT.to_owned())),
    );

    let body_html = full.body_html.filter(|h| !h.trim().is_empty());
    let body = full
        .body_text
        .filter(|t| !t.trim().is_empty())
        .map(|text| body_lines(&text))
        .unwrap_or_else(|| {
            vec![if body_html.is_some() {
                "(HTML-only message — press o to open it in a browser)".to_owned()
            } else {
                "(no body)".to_owned()
            }]
        });

    OpenMessage {
        id: message.id,
        headers,
        body,
        has_html: body_html.is_some(),
        attachments: full.attachments.iter().map(attachment_line).collect(),
    }
}

/// The HTML alternative of a fetched message, if it has one.
pub fn html_body(full: &FullMessage) -> Option<&str> {
    full.body_html.as_deref().filter(|h| !h.trim().is_empty())
}

/// Build the `CreateDraft` request for a reply or a forward.
///
/// The TUI does not build MIME. `ComposeService` owns rendering a draft into
/// RFC 5322 octets (and `SendScheduler` owns transmitting them); all that
/// happens here is choosing a subject, a recipient and a quoted body — the
/// same three things a human would type.
pub fn draft_request(
    kind: DraftKind,
    account_id: i64,
    from: &str,
    to: &str,
    original: &FullMessage,
) -> CreateDraftRequest {
    let message = original.message.clone().unwrap_or_default();
    let original_subject = message.subject.clone().unwrap_or_default();
    let body_text = original
        .body_text
        .as_deref()
        .map(|body| quote(kind, &message, body))
        .unwrap_or_default();

    CreateDraftRequest {
        account_id,
        // No fence from the TUI: a draft is created from a keystroke, not a
        // retried RPC, and an empty key means "no fence" (see the proto).
        idempotency_key: String::new(),
        from: Some(DraftAddress {
            address: from.to_owned(),
            display_name: String::new(),
        }),
        to: vec![DraftAddress {
            address: to.to_owned(),
            display_name: String::new(),
        }],
        cc: Vec::new(),
        bcc: Vec::new(),
        subject: prefixed_subject(kind, &original_subject),
        body_text,
        body_html: None,
        attachments: Vec::new(),
        // Only a reply threads onto the original. A forward starts a new
        // conversation with a different audience, and `CreateDraft` would
        // otherwise freeze the original's References chain onto it — putting
        // the forward inside the thread it was forwarded out of.
        in_reply_to_message_id: match kind {
            DraftKind::Reply => Some(message.id).filter(|id| *id != 0),
            DraftKind::Forward => None,
        },
    }
}

/// `Re: `/`Fwd: ` a subject, without stacking a prefix that is already there.
fn prefixed_subject(kind: DraftKind, subject: &str) -> String {
    let subject = subject.trim();
    let (prefix, existing): (&str, &[&str]) = match kind {
        DraftKind::Reply => ("Re: ", &["re:"]),
        DraftKind::Forward => ("Fwd: ", &["fwd:", "fw:"]),
    };
    let lower = subject.to_ascii_lowercase();
    if existing.iter().any(|p| lower.starts_with(p)) {
        return subject.to_owned();
    }
    if subject.is_empty() {
        return prefix.trim_end().to_owned();
    }
    format!("{prefix}{subject}")
}

/// The quoted original, capped at [`MAX_QUOTED_CHARS`].
fn quote(kind: DraftKind, message: &ProtoMessage, body: &str) -> String {
    let who = header_from(message).unwrap_or_else(|| "someone".to_owned());
    let mut out = match kind {
        DraftKind::Reply => format!("\n\nOn a previous message, {who} wrote:\n"),
        DraftKind::Forward => format!(
            "\n\n---------- Forwarded message ----------\nFrom: {who}\nSubject: {}\n\n",
            message.subject.clone().unwrap_or_default()
        ),
    };
    // Count characters, not bytes: truncating a UTF-8 body by byte offset can
    // land inside a code point.
    let truncated: String = body.chars().take(MAX_QUOTED_CHARS).collect();
    let was_truncated = truncated.chars().count() < body.chars().count();
    for line in truncated.lines() {
        match kind {
            DraftKind::Reply => {
                out.push_str("> ");
                out.push_str(line);
            }
            DraftKind::Forward => out.push_str(line),
        }
        out.push('\n');
    }
    if was_truncated {
        out.push_str("\n[quoted text truncated]\n");
    }
    out
}

/// `Name <addr>`, `addr`, or nothing.
fn header_from(message: &ProtoMessage) -> Option<String> {
    match (
        message
            .from_name
            .as_deref()
            .filter(|n| !n.trim().is_empty()),
        message.from_addr.as_deref(),
    ) {
        (Some(name), Some(addr)) => Some(format!("{name} <{addr}>")),
        (Some(name), None) => Some(name.to_owned()),
        (None, Some(addr)) => Some(addr.to_owned()),
        (None, None) => None,
    }
}

fn push_header(headers: &mut Vec<(String, String)>, name: &str, value: Option<String>) {
    if let Some(value) = value.filter(|v| !v.trim().is_empty()) {
        headers.push((name.to_owned(), value));
    }
}

/// Split a body for display, normalising the CRLF line endings mail carries
/// (a trailing `\r` renders as a stray glyph in a terminal cell) and
/// expanding tabs, which a ratatui `Paragraph` would otherwise draw as one
/// blank cell.
fn body_lines(text: &str) -> Vec<String> {
    text.split('\n')
        .map(|line| line.trim_end_matches('\r').replace('\t', "    "))
        .collect()
}

fn attachment_line(attachment: &Attachment) -> String {
    let name = attachment
        .filename
        .clone()
        .filter(|f| !f.trim().is_empty())
        .unwrap_or_else(|| format!("part {}", attachment.part_id));
    let kind = attachment.content_type.as_deref().unwrap_or("unknown type");
    match attachment.size {
        Some(size) => format!("{name} ({kind}, {size} bytes)"),
        None => format!("{name} ({kind})"),
    }
}

fn non_empty(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.trim().is_empty())
}
