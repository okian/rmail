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
    Account as ProtoAccount, Attachment, Citation as ProtoCitation, CreateDraftRequest,
    DraftAddress, FindResult, FolderStatus, FullMessage, ItemKind, Message as ProtoMessage,
    OutboxEntry, OutboxState, RankExplanation, RetrievalTrace, SearchHit, Summary,
};

use crate::tui::overlays::{
    valid_byte_ranges, AiSummary, Citation, Explanation, FinderItem, FinderKind, Hit, OutboxRow,
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

// ---------------------------------------------------------------------------
// task 85's overlays
// ---------------------------------------------------------------------------

/// Project a `SearchHit` into what the search overlay draws.
///
/// The snippet's text is carried verbatim and its highlights are validated
/// against *it*, not against a sanitized copy — sanitizing here would move
/// every byte offset after the first dropped control character. The renderer
/// applies the offsets and the sanitizer together, one character at a time;
/// see `overlays::runs_from_byte_ranges`.
pub fn hit(proto: SearchHit) -> Hit {
    let message = proto.message.unwrap_or_default();
    let snippet = proto.snippet.unwrap_or_default();
    let highlights: Vec<(u32, u32)> = snippet
        .highlights
        .iter()
        .map(|range| (range.start, range.end))
        .collect();
    Hit {
        message_id: message.id,
        subject: non_empty(message.subject).unwrap_or_else(|| NO_SUBJECT.to_owned()),
        from: message
            .from_name
            .filter(|name| !name.trim().is_empty())
            .or_else(|| message.from_addr.clone())
            .unwrap_or_else(|| "(unknown sender)".to_owned()),
        date: message.date.or(message.internaldate),
        highlights: valid_byte_ranges(&snippet.text, &highlights),
        snippet: snippet.text,
        sources: proto.sources,
    }
}

/// Project a `RankExplanation` into the why-panel's rows.
///
/// The floats are rendered here rather than carried into the model: `Model`
/// is compared with `assert_eq!` throughout its tests, and an `f64` in it
/// would cost `Eq` on every enum that reaches it to buy a renderer nothing.
pub fn explanation(message_id: i64, proto: RankExplanation) -> Explanation {
    Explanation {
        message_id,
        score: format!("{:.3}", proto.score),
        features: proto
            .features
            .into_iter()
            .map(|feature| {
                (
                    feature.name,
                    format!(
                        "value={:>8.3} weight={:>6.3} -> {:>8.3}",
                        feature.value, feature.weight, feature.weighted_contribution
                    ),
                )
            })
            .collect(),
        sources: proto.sources,
        matched: proto
            .matched
            .map(|snippet| snippet.text)
            .filter(|text| !text.trim().is_empty()),
        claude_reason: proto.claude_reason,
    }
}

/// Project a `FindResult` into a finder row.
///
/// `positions` are char offsets and stay char offsets. Converting them to
/// bytes here would be a second place for the two coordinate systems to be
/// confused, and the renderer wants characters anyway.
pub fn finder_item(proto: FindResult) -> FinderItem {
    FinderItem {
        kind: match proto.kind() {
            ItemKind::Message => FinderKind::Message,
            ItemKind::Mailbox => FinderKind::Mailbox,
            ItemKind::Contact => FinderKind::Contact,
            ItemKind::SavedSearch => FinderKind::SavedSearch,
            ItemKind::Tag => FinderKind::Tag,
            ItemKind::Command => FinderKind::Command,
            ItemKind::Unspecified => FinderKind::Unknown,
        },
        ref_id: proto.ref_id,
        primary: proto.primary_text,
        secondary: proto.secondary,
        positions: proto
            .positions
            .into_iter()
            .filter_map(|at| usize::try_from(at).ok())
            .collect(),
        mailbox_id: proto.mailbox_id,
    }
}

/// Project a `Citation` into the ask pane's source list.
pub fn citation(proto: ProtoCitation) -> Citation {
    Citation {
        label: proto.label,
        message_id: proto.message_id,
        subject: if proto.subject.trim().is_empty() {
            NO_SUBJECT.to_owned()
        } else {
            proto.subject
        },
        from_addr: proto.from_addr,
        mailbox: proto.mailbox,
        quote: proto.quote,
    }
}

/// One line summarising what retrieval found, shown while the answer streams.
pub fn ask_trace(proto: &RetrievalTrace) -> String {
    let mut line = format!(
        "retrieved {} · packed {} · ~{} context tokens",
        proto.retrieved, proto.packed, proto.context_tokens
    );
    // Both counts are things the user would otherwise experience as "the
    // answer is oddly thin" with no way to find out why.
    if proto.withheld_by_policy > 0 {
        line.push_str(&format!(
            " · {} withheld by policy",
            proto.withheld_by_policy
        ));
    }
    if proto.dropped_for_budget > 0 {
        line.push_str(&format!(
            " · {} dropped for budget",
            proto.dropped_for_budget
        ));
    }
    if !proto.model.is_empty() {
        line.push_str(&format!(" · {}", proto.model));
    }
    line
}

/// Project an `OutboxEntry` into the pseudo-folder's row.
pub fn outbox_row(proto: OutboxEntry) -> OutboxRow {
    let state = outbox_state(proto.state());
    OutboxRow {
        id: proto.id,
        to: if proto.to.is_empty() {
            "(no recipient)".to_owned()
        } else {
            proto.to.join(", ")
        },
        subject: if proto.subject.trim().is_empty() {
            NO_SUBJECT.to_owned()
        } else {
            proto.subject
        },
        state,
        send_at: proto.send_at,
        undo_deadline: proto.undo_deadline,
        last_error: proto.last_error.filter(|error| !error.trim().is_empty()),
    }
}

/// The state's name, as the pane prints it and as `undo_send` matches on it.
fn outbox_state(state: OutboxState) -> String {
    match state {
        OutboxState::Scheduled => "scheduled",
        OutboxState::Sending => "sending",
        OutboxState::Sent => "sent",
        OutboxState::Failed => "failed",
        OutboxState::Canceled => "canceled",
        OutboxState::Uncertain => "uncertain",
        OutboxState::Unspecified => "unknown",
    }
    .to_owned()
}

/// Project a `Summary` into what the AI panel draws.
pub fn summary(proto: Summary) -> AiSummary {
    AiSummary {
        message_id: proto.message_id,
        // Named rather than numbered: "no summary yet" and "AI is off for
        // this folder" look identical to a reader unless something says which.
        status: match proto.status() {
            rmail_proto::v1::SummaryStatus::Ok => "ok",
            rmail_proto::v1::SummaryStatus::Pending => "queued — check back shortly",
            rmail_proto::v1::SummaryStatus::NotQueued => "not queued",
            rmail_proto::v1::SummaryStatus::Unspecified => "unknown",
        }
        .to_owned(),
        tl_dr: non_empty(proto.tl_dr),
        summary: non_empty(proto.summary),
        key_points: proto.key_points,
        todos: proto
            .todos
            .into_iter()
            .map(|todo| match (todo.due, todo.owner) {
                (Some(due), Some(owner)) => format!("{} — {owner}, due {due}", todo.text),
                (Some(due), None) => format!("{} — due {due}", todo.text),
                (None, Some(owner)) => format!("{} — {owner}", todo.text),
                (None, None) => todo.text,
            })
            .collect(),
        tags: proto.suggested_tags,
        priority: non_empty(proto.priority),
        needs_reply: proto.needs_reply,
        suggested_reply: non_empty(proto.suggested_reply),
    }
}
