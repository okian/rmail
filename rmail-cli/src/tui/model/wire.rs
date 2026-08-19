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
    Account as ProtoAccount, Attachment, AuthStatusResponse, Citation as ProtoCitation,
    CreateDraftRequest, DayUsage, DraftAddress, FindResult, FinderStatusResponse, FolderStatus,
    FullMessage, GetSpendResponse, IndexDrift, IndexGcReport, IndexKind, IndexProgress,
    IndexStatusResponse, ItemKind, ListEntitiesResponse, Message as ProtoMessage, OutboxEntry,
    OutboxState, RankExplanation, RetrievalTrace, SearchHit, Summary, SyncFolderResponse,
    SyncStatusResponse, UsageStats,
};

use rmail_core::command;

use crate::tui::overlays::{
    valid_byte_ranges, AiSummary, Citation, Explanation, FinderItem, FinderKind, Hit, OutboxRow,
};
use crate::tui::report::{ReportRow, ReportTone};
use crate::tui::status::{Health, HealthState};

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

/// The `:auth status` report's rows.
///
/// Two settings, each with the state it is in, and — when there is a password
/// to remove — a row `<enter>` clears it from. The remedial action rides on the
/// row rather than being a second verb the reader has to know: a status screen
/// that reports a problem and cannot act on it sends them back to the shell.
///
/// The tone is the honest reading of each fact rather than "configured is
/// good": a password gate is a choice, so *having* one is [`ReportTone::Ok`]
/// and not having one is [`ReportTone::Muted`] — a default, not a fault.
/// Requiring local callers to log in is the stricter setting, hence
/// [`ReportTone::Warn`] when it is on: that is the state in which a client
/// needs `mail auth login` before anything works, which is the one thing this
/// report exists to be able to say.
pub fn auth_status_rows(response: &AuthStatusResponse) -> Vec<ReportRow> {
    let password = if response.password_configured {
        let row =
            ReportRow::new(["password", "configured — Enter removes it"]).toned(ReportTone::Ok);
        match clear_password() {
            Some(invocation) => row.running(invocation),
            None => row,
        }
    } else {
        ReportRow::new(["password", "not configured"]).toned(ReportTone::Muted)
    };
    // The one combination that is not a choice but a lock-out: local callers
    // are told to log in and there is no password to log in *with*, so
    // `LoginPassword` answers `UNAUTHENTICATED` to every caller and the only
    // way back in is editing the config file. That is a fault, and the row
    // says so rather than reporting the stricter half as merely strict.
    let local = match (response.local_login_required, response.password_configured) {
        (true, false) => ReportRow::new([
            "local login",
            "required, but no password is set — nothing can log in",
        ])
        .toned(ReportTone::Bad),
        (true, true) => ReportRow::new([
            "local login",
            "required — a socket peer must log in as well",
        ])
        .toned(ReportTone::Warn),
        (false, _) => ReportRow::new(["local login", "not required — a socket peer is trusted"])
            .toned(ReportTone::Muted),
    };
    vec![password, local]
}

/// The `:auth clear` invocation an `:auth status` row runs.
///
/// Parsed rather than constructed field by field, so the row runs exactly what
/// typing that line runs: the capability it carries — which the report's
/// confirmation gate reads to decide whether to ask first — comes from the verb
/// registry rather than from a literal here that could name the wrong one.
///
/// `None` cannot happen (`auth clear` is a declared verb, and
/// `rmail_core::command::tests::every_real_verb_is_reachable_by_typing_its_own_path`
/// is what keeps it reachable), and is still returned rather than unwrapped:
/// a row that does nothing on Enter is a degraded report, and a panic is a
/// terminal left in raw mode.
fn clear_password() -> Option<command::Invocation> {
    match command::parse("auth clear") {
        Ok(command::Resolution::Invocation(invocation)) => Some(*invocation),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// the daemon heartbeat (task 92)
// ---------------------------------------------------------------------------

/// `SyncService.Status` as an indicator.
///
/// Paused is the only state this RPC distinguishes, and it is deliberately
/// `Warn` rather than `Bad`: an operator paused it, and a bar that shouted
/// about a state somebody chose teaches people to ignore the bar.
#[must_use]
pub fn sync_health(response: &SyncStatusResponse) -> Health {
    let folders = response.folders.len();
    if response.paused {
        return Health::new(HealthState::Paused, format!("paused · {folders} folder(s)"));
    }
    Health::new(HealthState::Ok, format!("{folders} folder(s)"))
}

/// `IndexService.Status` as an indicator.
///
/// The order of the checks is the order of severity, and `dead` outranks
/// `paused`: a quarantined job is work that will never happen without
/// somebody's attention, while a paused worker is waiting for exactly that
/// attention already. Reporting the pause and hiding the dead jobs would hide
/// the reason to look.
#[must_use]
pub fn index_health(response: &IndexStatusResponse) -> Health {
    let working = response.queue_ready + response.queue_leased;
    if response.queue_dead > 0 {
        return Health::new(
            HealthState::Strained,
            format!("{} quarantined · queue {working}", response.queue_dead),
        );
    }
    if response.paused {
        return Health::new(HealthState::Paused, format!("paused · queue {working}"));
    }
    if working > 0 {
        return Health::new(HealthState::Busy, format!("queue {working}"));
    }
    Health::new(
        HealthState::Ok,
        format!("{} message(s) indexed", response.messages),
    )
}

/// `AiService.GetUsage` as an indicator.
///
/// `enabled` is checked before `paused` because the proto says the two are not
/// the same and must not be conflated: a daemon with `ai.enabled = false` never
/// spawns the dispatch loop, so `paused` stays false, and an indicator reading
/// that as "running" would be wrong in the one direction that matters — it
/// would send somebody to `resume` something no RPC can start.
#[must_use]
pub fn ai_health(stats: &UsageStats) -> Health {
    if !stats.enabled {
        return Health::new(HealthState::Off, "disabled in config");
    }
    let queue = stats.queue.as_ref();
    let dead = queue.map_or(0, |queue| queue.dead);
    let working = queue.map_or(0, |queue| queue.ready + queue.leased);
    if dead > 0 {
        return Health::new(
            HealthState::Strained,
            format!("{dead} quarantined · queue {working}"),
        );
    }
    if stats.paused {
        return Health::new(HealthState::Paused, format!("paused · queue {working}"));
    }
    let today = stats.today.as_ref().map_or(0.0, |today| today.cost_usd);
    if working > 0 {
        return Health::new(
            HealthState::Busy,
            format!("queue {working} · {}", usd(today)),
        );
    }
    Health::new(HealthState::Ok, format!("{} today", usd(today)))
}

/// `AiPolicyService.GetSpend` as an indicator.
///
/// Measured against the caps actually in force, and against the *hard* cap
/// first: at or above it the daemon blocks dispatch, which is a fault a bar has
/// to be able to show as one rather than as "nearly there". A scope with no cap
/// at all reads `Ok` and says so — unlimited is a configuration, not a warning,
/// and drawing it as one would make the zone permanently yellow on a default
/// install.
#[must_use]
pub fn spend_health(response: &GetSpendResponse) -> Health {
    let Some(all) = response.all.as_ref() else {
        return Health::new(HealthState::Unknown, "no spend reported");
    };
    let spent = all.daily.as_ref().map_or(0.0, |daily| daily.usd);
    let caps = all.caps.as_ref().and_then(|caps| caps.daily.as_ref());
    let hard = caps.and_then(|daily| daily.hard_usd);
    let soft = caps.and_then(|daily| daily.soft_usd);
    let against = |cap: f64| format!("{} of {} today", usd(spent), usd(cap));
    if let Some(hard) = hard.filter(|hard| spent >= *hard) {
        return Health::new(HealthState::Failed, format!("{} — blocked", against(hard)));
    }
    if let Some(soft) = soft.filter(|soft| spent >= *soft) {
        return Health::new(
            HealthState::Strained,
            format!("{} — downgrading", against(soft)),
        );
    }
    match hard.or(soft) {
        Some(cap) => Health::new(HealthState::Ok, against(cap)),
        None => Health::new(HealthState::Ok, format!("{} today · no cap", usd(spent))),
    }
}

/// A dollar figure, at cent precision.
///
/// Cents rather than the provider's own precision: the bar has a fixed zone,
/// and a spend of `$0.0000123` rendered in full is a number that pushes the
/// zone after it off the row to say nothing.
fn usd(amount: f64) -> String {
    format!("${amount:.2}")
}

// ---------------------------------------------------------------------------
// the daemon-observability reports (task 94)
// ---------------------------------------------------------------------------

/// A daemon timestamp the way a report draws one: local zone, fixed width, and
/// "never" for the zero the protos use to mean "not yet".
///
/// Local because these are read where the reader is, which is `view::short_date`'s
/// own reasoning for message dates; the format differs because a report column
/// has room for the year and a message list does not.
fn when(unix_seconds: i64) -> String {
    if unix_seconds == 0 {
        return "never".to_owned();
    }
    chrono::DateTime::<chrono::Utc>::from_timestamp(unix_seconds, 0).map_or_else(
        || "unreadable".to_owned(),
        |at| {
            at.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M")
                .to_string()
        },
    )
}

/// `IndexService.Status` as a table: one row per pipeline stage, then the queue.
///
/// Coverage is a percentage rather than the raw `indexed/eligible` pair, and a
/// *disabled* stage reports its state instead of a figure: the proto is explicit
/// that a disabled stage reports zero coverage precisely so the number stays
/// honest, and drawing `0%` beside `off` would read as a stage that is on and
/// failing.
#[must_use]
pub fn index_status_rows(response: &IndexStatusResponse) -> Vec<ReportRow> {
    let mut rows: Vec<ReportRow> = response
        .kinds
        .iter()
        .map(|kind| {
            let state = if kind.enabled { "on" } else { "off" };
            let coverage = if kind.enabled {
                format!("{:.0}%", kind.coverage * 100.0)
            } else {
                "—".to_owned()
            };
            let tone = if kind.quarantined > 0 {
                ReportTone::Warn
            } else if kind.enabled {
                ReportTone::Plain
            } else {
                ReportTone::Muted
            };
            ReportRow::new([
                index_kind(kind.kind),
                state.to_owned(),
                coverage,
                kind.pending.to_string(),
                kind.quarantined.to_string(),
            ])
            .toned(tone)
        })
        .collect();
    // The queue is not a stage and is not drawn as one: it is the shared
    // machinery every stage runs through, and a row pretending to be a fifth
    // stage would make the coverage column meaningless for it.
    let queue_tone = if response.queue_dead > 0 || response.paused {
        ReportTone::Warn
    } else {
        ReportTone::Ok
    };
    rows.push(
        ReportRow::new([
            "queue".to_owned(),
            if response.paused { "paused" } else { "running" }.to_owned(),
            format!("{} msgs", response.messages),
            format!("{} ready", response.queue_ready),
            format!("{} dead", response.queue_dead),
        ])
        .toned(queue_tone),
    );
    rows
}

/// The stage name an `IndexKind` discriminant means.
///
/// An unknown discriminant renders as itself rather than being guessed at: a
/// newer daemon adding a stage should show up as a row somebody can ask about,
/// not as whichever known stage sorts first.
fn index_kind(kind: i32) -> String {
    match IndexKind::try_from(kind) {
        Ok(IndexKind::Extract) => "extract".to_owned(),
        Ok(IndexKind::Lexical) => "lexical".to_owned(),
        Ok(IndexKind::Entities) => "entities".to_owned(),
        Ok(IndexKind::Semantic) => "semantic".to_owned(),
        Ok(IndexKind::Unspecified) | Err(_) => format!("kind {kind}"),
    }
}

/// One `IndexProgress` frame as a snapshot of counters.
///
/// A snapshot, so the frame *replaces*: `IndexProgress` reports running totals,
/// and appending them would draw one row per tick — a scrolling log of the same
/// five numbers rather than the five numbers.
#[must_use]
pub fn index_progress_rows(progress: &IndexProgress) -> Vec<ReportRow> {
    let counter = |name: &str, value: i64, tone: ReportTone| {
        ReportRow::new([name.to_owned(), value.to_string()]).toned(tone)
    };
    vec![
        counter("enqueued", progress.enqueued, ReportTone::Plain),
        counter("completed", progress.completed, ReportTone::Ok),
        counter(
            "failed",
            progress.failed,
            if progress.failed > 0 {
                ReportTone::Bad
            } else {
                ReportTone::Muted
            },
        ),
        counter(
            "remaining",
            progress.remaining,
            if progress.remaining > 0 {
                ReportTone::Muted
            } else {
                ReportTone::Ok
            },
        ),
        // Dropped is not failed: a job dropped because its message is gone is
        // the queue tidying up after a delete, and colouring it as a failure
        // would make an ordinary rebuild look broken.
        counter("dropped", progress.dropped, ReportTone::Muted),
    ]
}

/// `IndexService.Verify` as a table of what is adrift.
///
/// The verdict, then only the non-zero counters. A table of thirteen zeroes is a
/// table nobody reads; the verdict is what `clean` is for, and it is the
/// daemon's own rather than re-derived from the counters here.
#[must_use]
pub fn index_drift_rows(drift: &IndexDrift) -> Vec<ReportRow> {
    let checks: [(&str, i64); 12] = [
        ("content hash", drift.content_hash_drift),
        ("extract missing", drift.extract_missing),
        ("lexical missing", drift.lexical_missing),
        ("lexical orphaned", drift.lexical_orphaned),
        ("entity orphaned", drift.entity_orphaned),
        ("chunks unembedded", drift.chunks_unembedded),
        ("chunks unvectored", drift.chunks_unvectored),
        ("chunks wrong model", drift.chunks_wrong_model),
        ("chunks stale", drift.chunks_stale),
        ("vectors orphaned", drift.vectors_orphaned),
        ("message vectors stale", drift.message_vectors_stale),
        ("quarantined", drift.quarantined),
    ];
    let mut rows = vec![ReportRow::new([
        "verdict".to_owned(),
        if drift.clean { "clean" } else { "drifted" }.to_owned(),
    ])
    .toned(if drift.clean {
        ReportTone::Ok
    } else {
        ReportTone::Warn
    })];
    rows.extend(
        checks
            .iter()
            .filter(|(_, count)| *count > 0)
            .map(|(name, count)| {
                ReportRow::new([(*name).to_owned(), count.to_string()]).toned(ReportTone::Warn)
            }),
    );
    rows
}

/// `IndexService.Gc` as a table of what it reclaimed.
///
/// Every category, including the zeroes: this report is the answer to "what did
/// that just delete", and a category omitted because it was zero is
/// indistinguishable from one this client does not know about.
#[must_use]
pub fn index_gc_rows(report: &IndexGcReport) -> Vec<ReportRow> {
    [
        ("entities", report.entities),
        ("vectors", report.vectors),
        ("lexical rows", report.lexical_rows),
        ("content rows", report.content_rows),
        ("cached results", report.cache_results),
        ("cached embeddings", report.cache_embeddings),
        ("cached query plans", report.cache_query_plans),
    ]
    .iter()
    .map(|(name, count)| {
        ReportRow::new([(*name).to_owned(), count.to_string()]).toned(if *count > 0 {
            ReportTone::Plain
        } else {
            ReportTone::Muted
        })
    })
    .collect()
}

/// `IndexService.ListEntities` as a table.
#[must_use]
pub fn index_entity_rows(response: &ListEntitiesResponse) -> Vec<ReportRow> {
    response
        .entities
        .iter()
        .map(|entity| {
            ReportRow::new([
                entity.kind.clone(),
                entity.value.clone(),
                entity.mentions.to_string(),
                entity.messages.to_string(),
            ])
        })
        .collect()
}

/// `SyncService.Status` as a table: one row per folder, then the account.
#[must_use]
pub fn sync_status_rows(response: &SyncStatusResponse) -> Vec<ReportRow> {
    let mut rows: Vec<ReportRow> = response
        .folders
        .iter()
        .map(|folder| {
            ReportRow::new([
                folder.name.clone(),
                folder.message_count.to_string(),
                if folder.full_sync_done {
                    "all"
                } else {
                    "partial"
                }
                .to_owned(),
                folder.last_sync_at.map_or_else(|| "never".to_owned(), when),
            ])
            .toned(if folder.full_sync_done {
                ReportTone::Plain
            } else {
                ReportTone::Muted
            })
        })
        .collect();
    rows.push(
        ReportRow::new([
            "— account —".to_owned(),
            String::new(),
            if response.paused { "paused" } else { "syncing" }.to_owned(),
            String::new(),
        ])
        .toned(if response.paused {
            ReportTone::Warn
        } else {
            ReportTone::Ok
        }),
    );
    rows
}

/// `SyncService.SyncFolder` as a table: what the pass actually did.
///
/// A folder that failed keeps its counters — whatever it managed before the
/// error is true — and reports the failure in the strategy column, which is the
/// one a reader is already looking at to understand what the pass did.
#[must_use]
pub fn sync_now_rows(response: &SyncFolderResponse) -> Vec<ReportRow> {
    response
        .folders
        .iter()
        .map(|folder| {
            let failure = folder.error.as_deref().filter(|error| !error.is_empty());
            let row = ReportRow::new([
                folder.mailbox_name.clone(),
                match failure {
                    Some(error) => format!("failed: {error}"),
                    None => folder.strategy.clone(),
                },
                folder.new_messages.to_string(),
                folder.flag_updates.to_string(),
                folder.expunged.to_string(),
            ]);
            match failure {
                Some(_) => row.toned(ReportTone::Bad),
                None => row,
            }
        })
        .collect()
}

/// `AiService.GetUsage` as the dispatch-loop view.
#[must_use]
pub fn ai_status_rows(stats: &UsageStats) -> Vec<ReportRow> {
    let queue = stats.queue.unwrap_or_default();
    let mut rows = vec![
        ReportRow::new([
            "subsystem".to_owned(),
            if stats.enabled {
                "enabled".to_owned()
            } else {
                "disabled in config".to_owned()
            },
        ])
        .toned(if stats.enabled {
            ReportTone::Ok
        } else {
            ReportTone::Muted
        }),
        ReportRow::new([
            "dispatch".to_owned(),
            if stats.paused { "paused" } else { "running" }.to_owned(),
        ])
        .toned(if stats.paused {
            ReportTone::Warn
        } else {
            ReportTone::Ok
        }),
    ];
    for (name, count, watch) in [
        ("queue ready", queue.ready, false),
        ("queue leased", queue.leased, false),
        ("queue backing off", queue.backing_off, false),
        ("queue done", queue.done, false),
        ("queue error", queue.error, true),
        ("queue quarantined", queue.dead, true),
    ] {
        rows.push(ReportRow::new([name.to_owned(), count.to_string()]).toned(
            if watch && count > 0 {
                ReportTone::Warn
            } else {
                ReportTone::Plain
            },
        ));
    }
    rows
}

/// `AiService.GetUsage` as the spend view.
///
/// The caps come from `UsageStats` rather than from `AiPolicyService.GetSpend`:
/// this verb reaches one RPC and reports what that RPC says, and folding in a
/// second service's answer would make `:ai cost` disagree with `mail ai cost`
/// for reasons no reader could see. Task 96's `:ai budget status` is the
/// per-class view.
#[must_use]
pub fn ai_cost_rows(stats: &UsageStats) -> Vec<ReportRow> {
    let window = |name: &str, usage: Option<&DayUsage>, cap: f64| {
        let usage = usage.cloned().unwrap_or_default();
        let tokens = usage.input_tokens
            + usage.output_tokens
            + usage.cache_creation_input_tokens
            + usage.cache_read_input_tokens;
        // A cap of zero is "no cap" on this RPC, not "spend nothing", so it is
        // never compared against — reading it literally would report every
        // uncapped daemon as permanently over budget.
        let over = cap > 0.0 && usage.cost_usd >= cap;
        ReportRow::new([
            name.to_owned(),
            format!("${:.2}", usage.cost_usd),
            if cap > 0.0 {
                format!("${cap:.2}")
            } else {
                "none".to_owned()
            },
            format!("{tokens} in {} call(s)", usage.requests),
        ])
        .toned(if over {
            ReportTone::Bad
        } else {
            ReportTone::Plain
        })
    };
    vec![
        window("today", stats.today.as_ref(), stats.daily_cost_cap_usd),
        window(
            "this month",
            stats.month.as_ref(),
            stats.monthly_cost_cap_usd,
        ),
    ]
}

/// `FinderService.IndexStatus` as a table.
#[must_use]
pub fn finder_status_rows(response: &FinderStatusResponse) -> Vec<ReportRow> {
    vec![
        ReportRow::new(["entries".to_owned(), response.entries.to_string()]),
        ReportRow::new(["bytes".to_owned(), response.bytes.to_string()]),
        ReportRow::new(["pending".to_owned(), response.pending.to_string()]).toned(
            if response.pending > 0 {
                ReportTone::Muted
            } else {
                ReportTone::Ok
            },
        ),
        // Rejected entries are ones the index refused to hold — worth colouring,
        // because a finder that silently drops a tenth of the mailbox looks like
        // a finder that simply cannot find things.
        ReportRow::new(["rejected".to_owned(), response.rejected.to_string()]).toned(
            if response.rejected > 0 {
                ReportTone::Warn
            } else {
                ReportTone::Ok
            },
        ),
        ReportRow::new(["refreshed".to_owned(), when(response.refreshed_at)]),
    ]
}
