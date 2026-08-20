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
    Account as ProtoAccount, AiProviderKind, Alert, AnalyticsCell, AskAnalyticsResponse,
    Attachment, AuditEntry, AuthStatusResponse, AutoconfigureResponse, BeginOAuthResponse,
    BudgetClass, BulkTagResponse, CallStatus, CellType, Citation as ProtoCitation,
    CompleteOAuthResponse, CreateDraftRequest, DayUsage, Draft, DraftAddress, DraftNudgeResponse,
    DraftReplyContext, DraftRevision, EvalMetrics, EvalReport, EvaluationStats, ExportDone,
    ExportInvoicesResponse, ExtractEventsResponse, ExtractInvoiceResponse, ExtractLinksResponse,
    ExtractStructuredResponse, ExtractTablesResponse, ExtractTasksResponse, ExtractionSource,
    FieldOrigin, FieldProvenance, FindResult, FinderStatusResponse, FolderStatus, Followup,
    FollowupState, ForwardMessageResponse, FullMessage, GenerateDigestResponse,
    GetAiProviderResponse, GetContactInsightResponse, GetResponseTimesResponse, GetSpendResponse,
    HookEvent, IndexDrift, IndexGcReport, IndexKind, IndexProgress, IndexStatusResponse,
    InjectionSeverity, InvoiceMoney, InvoicePaymentStatus, InvoiceText, ItemKind, LinkKind,
    ListAccountsResponse, ListDraftRevisionsResponse, ListDraftsResponse, ListEntitiesResponse,
    ListHooksResponse, ListNotesResponse, ListRulesResponse, ListSavedSearchesResponse,
    ListSmartFoldersResponse, ListSubscriptionsResponse, ListTagRulesResponse, ListTagsResponse,
    ListTokensResponse, Message as ProtoMessage, MessageOutcome, MintTokenResponse, Note,
    NoteAuthor, NoteEvent, NotificationState, NotificationTier, OutboxEntry, OutboxState,
    PreflightCheckResponse, PreflightDegradation, PreflightFindingKind, PreflightSeverity,
    QueryPlan, RankExplanation, RefreshTokenResponse, RenderedDraft, ResponseStats, RetrievalTrace,
    RewriteLength, RewriteTone, SavedSearch, ScanInjectionResponse, ScoreMessageResponse,
    SearchAttachmentsResponse, SearchEntitiesResponse, SearchHit, SetAiProviderResponse,
    SetBudgetResponse, SmartFolder, SmartFolderEvaluation, SubscriptionClass,
    SuggestSendTimeResponse, Summary, SyncFolderResponse, SyncStatusResponse,
    SynthesizeRuleResponse, TableCell, TagRuleMode, TagSource, TagSuggestion, TagSyncMode,
    TestConnectionResponse, TestHookResponse, UsageStats, WebhookDelivery, WebhookDeliveryState,
    WebhookDestination, WebhookEvent, WebhookSecretSource,
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
#[must_use]
pub fn when(unix_seconds: i64) -> String {
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

// ---------------------------------------------------------------------------
// the tag and rule reports (task 95)
// ---------------------------------------------------------------------------

/// `TagService.ListTags` as a table.
///
/// A tag with no messages is drawn muted rather than hidden: it is a tag
/// somebody created and has not used, and hiding it would make `:tag new`
/// look as though it had done nothing.
#[must_use]
pub fn tag_rows(response: &ListTagsResponse) -> Vec<ReportRow> {
    response
        .tags
        .iter()
        .map(|counted| {
            let tag = counted.tag.clone().unwrap_or_default();
            ReportRow::new([
                tag.name,
                counted.message_count.to_string(),
                tag_sync(tag.sync_mode),
                tag.color.unwrap_or_else(|| "—".to_owned()),
            ])
            .toned(if counted.message_count == 0 {
                ReportTone::Muted
            } else {
                ReportTone::Plain
            })
        })
        .collect()
}

/// What a `TagSyncMode` discriminant means.
fn tag_sync(mode: i32) -> String {
    match TagSyncMode::try_from(mode) {
        Ok(TagSyncMode::Local) => "local".to_owned(),
        Ok(TagSyncMode::Imap) => "imap".to_owned(),
        Ok(TagSyncMode::Auto) => "auto".to_owned(),
        // A mode this build does not know, rendered rather than guessed at — the
        // same rule `FinderKind::Unknown` follows.
        Ok(TagSyncMode::Unspecified) | Err(_) => format!("mode {mode}"),
    }
}

/// One `AddTag` outcome as a row.
///
/// Per message rather than a count, because the interesting answer is the one a
/// count hides: a tag that applied to four of five and failed on the fifth.
#[must_use]
pub fn tag_applied_row(message_id: i64, name: &str, source: &str) -> ReportRow {
    ReportRow::new([message_id.to_string(), name.to_owned(), source.to_owned()])
        .toned(ReportTone::Ok)
}

/// One failed application as a row.
#[must_use]
pub fn tag_failed_row(message_id: i64, name: &str, why: &str) -> ReportRow {
    ReportRow::new([message_id.to_string(), name.to_owned(), why.to_owned()]).toned(ReportTone::Bad)
}

/// What a `TagSource` discriminant means, for the outcome column.
#[must_use]
pub fn tag_source(source: i32) -> String {
    match TagSource::try_from(source) {
        Ok(TagSource::User) => "applied".to_owned(),
        Ok(TagSource::Rule) => "applied by a rule".to_owned(),
        Ok(TagSource::Ai) => "applied by the model".to_owned(),
        Ok(TagSource::Imap) => "applied from IMAP".to_owned(),
        Ok(TagSource::Unspecified) | Err(_) => "applied".to_owned(),
    }
}

/// `TagService.BulkTag` as a table.
#[must_use]
pub fn tag_bulk_rows(response: &BulkTagResponse) -> Vec<ReportRow> {
    vec![
        ReportRow::new([
            "messages selected".to_owned(),
            response.message_count.to_string(),
        ]),
        // Applied can be lower than selected without anything being wrong: a
        // message that already carried the tag is not tagged twice. Said as its
        // own row so the gap is visible rather than looking like a partial
        // failure.
        ReportRow::new(["tags applied".to_owned(), response.applied.to_string()]).toned(
            if response.applied == 0 {
                ReportTone::Muted
            } else {
                ReportTone::Ok
            },
        ),
    ]
}

/// One streamed `TagSuggestion` as a row that can be accepted or rejected.
///
/// Both actions ride on the row, because a suggestion list where accepting is
/// inline and rejecting is not would make the safe answer the awkward one.
#[must_use]
pub fn tag_suggestion_row(suggestion: &TagSuggestion) -> ReportRow {
    let tag = suggestion.tag.clone().unwrap_or_default();
    let confidence = format!("{:.0}%", suggestion.confidence * 100.0);
    let row = ReportRow::new([tag.name, confidence, suggestion.rationale.clone()]).toned(
        // Low confidence is not a fault — it is the reason the suggestion is
        // pending rather than applied — so it is muted, not warned about.
        if suggestion.confidence >= 0.8 {
            ReportTone::Ok
        } else {
            ReportTone::Muted
        },
    );
    let id = suggestion.message_tag_id;
    match (
        resolve_invocation("accept", id),
        resolve_invocation("reject", id),
    ) {
        (Some(accept), Some(reject)) => row.running(accept).rejecting(reject),
        // Unreachable: both verbs are declared, and
        // `command::tests::every_real_verb_is_reachable_by_typing_its_own_path`
        // is what keeps them so. A row that does nothing beats a panic in a
        // client holding a terminal in raw mode.
        _ => row,
    }
}

/// The `:tag accept <id>` / `:tag reject <id>` invocation a suggestion row runs.
///
/// Parsed rather than built field by field, so the row runs exactly what typing
/// that line runs — including the capability task 90's gate reads.
///
/// Bang'd, which is that gate being deliberately skipped for these two rows.
/// Both verbs mutate, so without it every answer on a suggestion list would open
/// a modal — and a screen whose whole purpose is answering a stream of small,
/// reversible guesses cannot ask about each one. The gesture *is* the consent:
/// the row says which tag and why, and the border says Enter accepts and `n`
/// rejects. `:tag reject` is the undoing direction anyway, so the gate would be
/// asking hardest about the safest answer.
fn resolve_invocation(which: &str, id: i64) -> Option<command::Invocation> {
    match command::parse(&format!("tag {which} {id}!")) {
        Ok(command::Resolution::Invocation(invocation)) => Some(*invocation),
        _ => None,
    }
}

/// `TagService.ListTagRules` as a table.
#[must_use]
pub fn tag_rule_rows(response: &ListTagRulesResponse) -> Vec<ReportRow> {
    response
        .rules
        .iter()
        .map(|rule| {
            let auto = matches!(TagRuleMode::try_from(rule.mode), Ok(TagRuleMode::Auto));
            ReportRow::new([
                rule.name.clone(),
                rule.tag_name.clone(),
                if auto { "auto" } else { "suggest" }.to_owned(),
                format!("{:.0}%", rule.min_conf * 100.0),
                if rule.enabled { "on" } else { "off" }.to_owned(),
            ])
            // `auto` is the mode in which a model's guess changes the mailbox
            // with nobody looking. Not a fault — somebody asked for it — but the
            // one row on this screen worth finding at a glance.
            .toned(if !rule.enabled {
                ReportTone::Muted
            } else if auto {
                ReportTone::Warn
            } else {
                ReportTone::Plain
            })
        })
        .collect()
}

/// `RuleService.ListRules` as a table.
#[must_use]
pub fn rule_rows(response: &ListRulesResponse) -> Vec<ReportRow> {
    response
        .rules
        .iter()
        .map(|rule| {
            ReportRow::new([
                rule.name.clone(),
                if rule.enabled { "on" } else { "off" }.to_owned(),
                when(rule.updated_at),
            ])
            .toned(if rule.enabled {
                ReportTone::Plain
            } else {
                ReportTone::Muted
            })
        })
        .collect()
}

/// A `MessageOutcome` table, plus the evaluation's own statistics.
///
/// The stats go first, because the question somebody asks of a dry run is "how
/// much did this match" and a hundred rows above the answer is a hundred rows
/// they have to scroll past to find it.
#[must_use]
pub fn rule_outcome_rows(
    outcomes: &[MessageOutcome],
    stats: Option<&EvaluationStats>,
    window_days: Option<u32>,
) -> Vec<ReportRow> {
    let mut rows = Vec::new();
    if let Some(stats) = stats {
        let mut summary = format!(
            "{} of {} matched · {} model call(s)",
            stats.matches, stats.messages, stats.model_calls
        );
        if let Some(days) = window_days {
            summary.push_str(&format!(" · {days} day(s)"));
        }
        rows.push(
            ReportRow::new([
                "— summary —".to_owned(),
                summary,
                String::new(),
                String::new(),
            ])
            .toned(if stats.errors > 0 {
                ReportTone::Warn
            } else {
                ReportTone::Ok
            }),
        );
        if stats.errors > 0 {
            rows.push(
                ReportRow::new([
                    "— errors —".to_owned(),
                    format!("{} message(s) could not be evaluated", stats.errors),
                    String::new(),
                    String::new(),
                ])
                .toned(ReportTone::Bad),
            );
        }
    }
    rows.extend(outcomes.iter().map(|outcome| {
        let matched: Vec<String> = outcome
            .rules
            .iter()
            .filter(|rule| rule.matched)
            .map(|rule| rule.rule.clone())
            .collect();
        let row = ReportRow::new([
            outcome.message_id.to_string(),
            outcome.from.clone(),
            outcome.subject.clone(),
            if matched.is_empty() {
                "—".to_owned()
            } else {
                matched.join(", ")
            },
        ]);
        if outcome.error.is_empty() {
            row.toned(if matched.is_empty() {
                ReportTone::Muted
            } else {
                ReportTone::Plain
            })
        } else {
            ReportRow::new([
                outcome.message_id.to_string(),
                outcome.from.clone(),
                outcome.subject.clone(),
                format!("failed: {}", outcome.error),
            ])
            .toned(ReportTone::Bad)
        }
    }));
    rows
}

/// `RuleService.SynthesizeRule` as a table: what it drafted, then its dry run.
///
/// The dropped-`claude_is` note is drawn as its own row and coloured, because it
/// is the one thing about a drafted rule somebody has to know: the model asked
/// for a criterion the daemon refused to include, so the rule that will actually
/// run is *narrower* than what was asked for.
#[must_use]
pub fn rule_draft_rows(response: &SynthesizeRuleResponse) -> Vec<ReportRow> {
    let mut rows = vec![ReportRow::new([
        "— drafted —".to_owned(),
        response.name.clone(),
        String::new(),
        String::new(),
    ])
    .toned(ReportTone::Ok)];
    if !response.claude_is_dropped.is_empty() {
        rows.push(
            ReportRow::new([
                "— dropped —".to_owned(),
                response.claude_is_dropped.clone(),
                String::new(),
                String::new(),
            ])
            .toned(ReportTone::Warn),
        );
    }
    if !response.notes.is_empty() {
        rows.push(
            ReportRow::new([
                "— notes —".to_owned(),
                response.notes.clone(),
                String::new(),
                String::new(),
            ])
            .toned(ReportTone::Muted),
        );
    }
    rows.extend(rule_outcome_rows(
        &response.dry_run,
        response.stats.as_ref(),
        Some(response.window_days),
    ));
    rows
}

// ---------------------------------------------------------------------------
// reply, drafts, send and follow-ups (task 100)
// ---------------------------------------------------------------------------

/// Addresses, joined the way a report cell shows a recipient list. Display
/// names are dropped: a report column is not the place to disambiguate two
/// people sharing a name, and the address alone is what every other id-taking
/// verb here expects typed back at it.
#[must_use]
pub fn addr_list(addrs: &[DraftAddress]) -> String {
    if addrs.is_empty() {
        return "(none)".to_owned();
    }
    addrs
        .iter()
        .map(|addr| addr.address.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// `ComposeService.ListDrafts` as a table: one row per draft.
#[must_use]
pub fn draft_list_rows(response: &ListDraftsResponse) -> Vec<ReportRow> {
    response
        .drafts
        .iter()
        .map(|draft| {
            ReportRow::new([
                draft.id.to_string(),
                addr_list(&draft.to),
                if draft.subject.trim().is_empty() {
                    NO_SUBJECT.to_owned()
                } else {
                    draft.subject.clone()
                },
                when(draft.updated_at),
            ])
        })
        .collect()
}

/// One draft's own fields, field per row — what `:draft show`, `:draft edit`
/// and `:draft revert` all answer with, since all three end with "here is the
/// draft as it now stands".
#[must_use]
pub fn draft_fields(draft: &Draft) -> Vec<ReportRow> {
    vec![
        ReportRow::new(["id".to_owned(), draft.id.to_string()]),
        ReportRow::new([
            "from".to_owned(),
            draft
                .from
                .as_ref()
                .map_or_else(|| "(none)".to_owned(), |a| a.address.clone()),
        ]),
        ReportRow::new(["to".to_owned(), addr_list(&draft.to)]),
        ReportRow::new(["cc".to_owned(), addr_list(&draft.cc)]),
        ReportRow::new([
            "subject".to_owned(),
            if draft.subject.trim().is_empty() {
                NO_SUBJECT.to_owned()
            } else {
                draft.subject.clone()
            },
        ]),
        ReportRow::new(["updated".to_owned(), when(draft.updated_at)]),
        ReportRow::new(["body".to_owned(), preview(&draft.body_text)]),
    ]
}

/// One drafted revision's fields — `:draft rewrite`'s answer.
#[must_use]
pub fn draft_revision_fields(rev: &DraftRevision) -> Vec<ReportRow> {
    vec![
        ReportRow::new(["draft".to_owned(), rev.draft_id.to_string()]),
        ReportRow::new(["seq".to_owned(), rev.seq.to_string()]),
        ReportRow::new(["label".to_owned(), rev.label.clone()]),
        ReportRow::new([
            "subject".to_owned(),
            if rev.subject.trim().is_empty() {
                NO_SUBJECT.to_owned()
            } else {
                rev.subject.clone()
            },
        ]),
        ReportRow::new(["body".to_owned(), preview(&rev.body_text)]),
        ReportRow::new([
            "model".to_owned(),
            rev.model.clone().unwrap_or_else(|| "(original)".to_owned()),
        ]),
    ]
}

/// `ComposeService.ListDraftRevisions` as a table: one row per revision, seq
/// ascending as the RPC already orders them.
#[must_use]
pub fn draft_revision_rows(response: &ListDraftRevisionsResponse) -> Vec<ReportRow> {
    response
        .revisions
        .iter()
        .map(|rev| {
            ReportRow::new([
                rev.seq.to_string(),
                rev.label.clone(),
                if rev.subject.trim().is_empty() {
                    NO_SUBJECT.to_owned()
                } else {
                    rev.subject.clone()
                },
                rev.model.clone().unwrap_or_else(|| "(original)".to_owned()),
            ])
        })
        .collect()
}

/// `ComposeService.RenderDraft`'s answer. The MIME bytes themselves stay off
/// screen — `:draft render`'s job is to confirm who this would actually reach
/// and that it produced a message at all, which `:draft show` already covers
/// the prose half of.
#[must_use]
pub fn rendered_draft_fields(rendered: &RenderedDraft) -> Vec<ReportRow> {
    vec![
        ReportRow::new(["message-id".to_owned(), rendered.message_id.clone()]),
        ReportRow::new([
            "recipients".to_owned(),
            if rendered.envelope_recipients.is_empty() {
                "(none)".to_owned()
            } else {
                rendered.envelope_recipients.join(", ")
            },
        ]),
        ReportRow::new(["size".to_owned(), format!("{} bytes", rendered.mime.len())]),
    ]
}

/// `SendSchedulerService.SuggestSendTime`'s answer.
#[must_use]
pub fn suggest_send_time_fields(response: &SuggestSendTimeResponse) -> Vec<ReportRow> {
    vec![
        ReportRow::new(["when".to_owned(), response.display.clone()]),
        ReportRow::new(["zone".to_owned(), response.tz.clone()]),
        ReportRow::new(["why".to_owned(), response.rationale.clone()]),
    ]
}

/// A follow-up's state, the way `:followup list` and `:waiting` print it.
fn followup_state(state: FollowupState) -> &'static str {
    match state {
        FollowupState::Armed => "armed",
        FollowupState::Fired => "fired",
        FollowupState::Dismissed => "dismissed",
        FollowupState::Unspecified => "unknown",
    }
}

/// `SendSchedulerService.ListFollowups` and `ListWaitingOn` both answer with
/// this same row shape — see `commands::followup_columns`.
#[must_use]
pub fn followup_rows(followups: &[Followup]) -> Vec<ReportRow> {
    followups
        .iter()
        .map(|f| {
            let row = ReportRow::new([
                f.id.to_string(),
                f.message_id.clone(),
                when(f.remind_at),
                followup_state(f.state()).to_owned(),
                f.note.clone().unwrap_or_default(),
            ]);
            match f.state() {
                FollowupState::Fired => row.toned(ReportTone::Warn),
                FollowupState::Dismissed => row.toned(ReportTone::Muted),
                FollowupState::Armed | FollowupState::Unspecified => row,
            }
        })
        .collect()
}

/// `SendSchedulerService.DraftNudge`'s answer: a subject, a model, and the
/// chase message itself — split on its own line breaks so a report row never
/// has to wrap prose it was never designed to.
#[must_use]
pub fn draft_nudge_fields(response: &DraftNudgeResponse) -> Vec<ReportRow> {
    let mut rows = vec![
        ReportRow::new(["subject".to_owned(), response.subject.clone()]),
        ReportRow::new(["model".to_owned(), response.model.clone()]),
    ];
    rows.extend(
        response
            .body
            .lines()
            .take(MAX_NUDGE_LINES)
            .map(|line| ReportRow::new(["".to_owned(), line.to_owned()])),
    );
    rows
}

/// How many lines of a drafted nudge's body the report shows before it stops
/// rather than growing a report past what a screen can hold.
const MAX_NUDGE_LINES: usize = 20;

/// A preflight finding's severity, as `:preflight` prints it.
fn preflight_severity(severity: PreflightSeverity) -> &'static str {
    match severity {
        PreflightSeverity::Notice => "notice",
        PreflightSeverity::Warn => "warn",
        PreflightSeverity::Block => "block",
        PreflightSeverity::Unspecified => "—",
    }
}

/// A preflight finding's kind, as `:preflight` prints it.
fn preflight_kind(kind: PreflightFindingKind) -> &'static str {
    match kind {
        PreflightFindingKind::MissingAttachment => "missing attachment",
        PreflightFindingKind::UnfilledPlaceholder => "unfilled placeholder",
        PreflightFindingKind::ApparentSecret => "apparent secret",
        PreflightFindingKind::RecipientNotOnThread => "recipient not on thread",
        PreflightFindingKind::DuplicateRecipient => "duplicate recipient",
        PreflightFindingKind::LargeRecipientList => "large recipient list",
        PreflightFindingKind::ToneClash => "tone clash",
        PreflightFindingKind::Unspecified => "unknown",
    }
}

/// Why the model half of a preflight check did not contribute, when
/// [`PreflightCheckResponse::degradation_detail`] itself is empty.
fn preflight_degradation(degradation: PreflightDegradation) -> &'static str {
    match degradation {
        PreflightDegradation::Disabled => "disabled by policy",
        PreflightDegradation::Refused => "refused by policy or budget",
        PreflightDegradation::Unavailable => "the model was unavailable",
        PreflightDegradation::TimedOut => "timed out",
        PreflightDegradation::Cancelled => "cancelled",
        PreflightDegradation::Unreadable => "the model's answer could not be read",
        PreflightDegradation::NothingToReview => "the redaction firewall left nothing to review",
        PreflightDegradation::Unspecified => "the full check ran",
    }
}

/// `SendSchedulerService.PreflightCheck`'s answer: one row per finding, then
/// a summary row naming whether `ScheduleSend` would refuse this draft and,
/// when the model half did not run, why not — silence there would read as
/// "nothing to say" rather than "half of this check did not happen".
#[must_use]
pub fn preflight_rows(response: &PreflightCheckResponse) -> Vec<ReportRow> {
    let mut rows: Vec<ReportRow> = response
        .findings
        .iter()
        .map(|finding| {
            let row = ReportRow::new([
                preflight_severity(finding.severity()).to_owned(),
                preflight_kind(finding.kind()).to_owned(),
                finding.detail.clone(),
                if finding.from_model { "model" } else { "check" }.to_owned(),
            ]);
            match finding.severity() {
                PreflightSeverity::Block => row.toned(ReportTone::Bad),
                PreflightSeverity::Warn => row.toned(ReportTone::Warn),
                PreflightSeverity::Notice | PreflightSeverity::Unspecified => row,
            }
        })
        .collect();
    if rows.is_empty() {
        rows.push(
            ReportRow::new([
                "—".to_owned(),
                "nothing found".to_owned(),
                String::new(),
                String::new(),
            ])
            .toned(ReportTone::Ok),
        );
    }
    if response.blocks {
        rows.push(
            ReportRow::new([
                "verdict".to_owned(),
                "would block the send".to_owned(),
                String::new(),
                String::new(),
            ])
            .toned(ReportTone::Bad),
        );
    }
    if response.degradation() != PreflightDegradation::Unspecified {
        let why = response
            .degradation_detail
            .as_deref()
            .filter(|d| !d.is_empty())
            .map_or_else(
                || preflight_degradation(response.degradation()).to_owned(),
                str::to_owned,
            );
        rows.push(ReportRow::new([
            "model check".to_owned(),
            why,
            String::new(),
            String::new(),
        ]));
    }
    rows
}

/// The first line of a body, or as much of it as fits — what a report field
/// meant for a whole message's prose actually has room for.
fn preview(body: &str) -> String {
    let first_line = body
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("");
    let truncated: String = first_line.chars().take(BODY_PREVIEW_CHARS).collect();
    if first_line.chars().count() > BODY_PREVIEW_CHARS || body.lines().count() > 1 {
        format!("{truncated}…")
    } else {
        truncated
    }
}

/// How much of a body [`preview`] shows.
const BODY_PREVIEW_CHARS: usize = 90;

/// The register `:draft rewrite --tone` names, as the proto enum.
///
/// Parsed through `rmail_core::compose::reply::Tone` rather than matched
/// against a second copy of its six spellings: `commands::answer` already
/// refuses anything `Tone::parse` does not recognise before a
/// [`Cmd::DraftRewrite`] can carry it this far, so `None` (an unset flag, or
/// — defensively — an unparseable one that should not reach here) falling
/// back to [`RewriteTone::AsIs`] is the same "no change" `Tone::AsIs` already
/// means, not a silent misread of real input. One parser, shared with
/// `commands::answer`, is what keeps a seventh tone from being reachable on
/// one side and not the other.
#[must_use]
pub fn rewrite_tone(name: Option<&str>) -> RewriteTone {
    match name.and_then(rmail_core::compose::reply::Tone::parse) {
        Some(rmail_core::compose::reply::Tone::AsIs) | None => RewriteTone::AsIs,
        Some(rmail_core::compose::reply::Tone::Formal) => RewriteTone::Formal,
        Some(rmail_core::compose::reply::Tone::Casual) => RewriteTone::Casual,
        Some(rmail_core::compose::reply::Tone::Warmer) => RewriteTone::Warmer,
        Some(rmail_core::compose::reply::Tone::Firmer) => RewriteTone::Firmer,
        Some(rmail_core::compose::reply::Tone::MirrorRecipient) => RewriteTone::Mirror,
    }
}

/// `:draft rewrite --shorter`/`--longer`. `commands::answer` already refuses
/// both at once, so at most one of these is ever `true`.
#[must_use]
pub fn rewrite_length(shorter: bool, longer: bool) -> RewriteLength {
    match (shorter, longer) {
        (true, _) => RewriteLength::Shorter,
        (_, true) => RewriteLength::Longer,
        (false, false) => RewriteLength::AsIs,
    }
}

/// What `ComposeService.DraftReply` read before it wrote anything, the way
/// [`ask_trace`] formats `AiService.AskMailbox`'s own retrieval trace.
#[must_use]
pub fn draft_reply_context(proto: &DraftReplyContext) -> String {
    let mut line = format!("{} thread message(s)", proto.thread_messages);
    if proto.withheld_by_policy > 0 {
        line.push_str(&format!(
            " · {} withheld by policy",
            proto.withheld_by_policy
        ));
    }
    if proto.voice_samples > 0 {
        line.push_str(&format!(" · {} voice sample(s)", proto.voice_samples));
    }
    if !proto.model.is_empty() {
        line.push_str(&format!(" · {}", proto.model));
    }
    line
}

// ---------------------------------------------------------------------------
// AI policy, safety and audit (task 96)
// ---------------------------------------------------------------------------

/// A dollar figure at the precision the budget surface uses.
///
/// Four places rather than [`usd`]'s two, and deliberately not the same
/// function: a single triage call costs a fraction of a cent, and a budget
/// report that rounded it to `$0.00` would say nothing was spent by an account
/// that has been spending all day. The status bar's zone has no room for four
/// places and does not need them — it answers "am I near the cap", not "what
/// exactly has this cost".
fn budget_usd(amount: f64) -> String {
    format!("${amount:.4}")
}

/// A cap, or `-` when that dimension is uncapped.
///
/// Uncapped is not zero — zero forbids all spending — so an absent cap can
/// never be printed as a number. `mail ai budget status` draws it the same way.
fn cap_usd(value: Option<f64>) -> String {
    value.map_or_else(|| "-".to_owned(), budget_usd)
}

/// A token cap, or `-` when uncapped. See [`cap_usd`].
fn cap_tokens(value: Option<i64>) -> String {
    value.map_or_else(|| "-".to_owned(), |value| value.to_string())
}

/// Where one figure sits against its caps.
///
/// The ladder `spend_health` climbs, returning a report's vocabulary instead of
/// the status bar's: hard first, because a scope past both caps is *blocked*
/// rather than downgrading, and reporting the softer verdict for it would
/// understate what is happening. A dimension with no cap at all is muted and
/// says so — unlimited is a configuration, not a warning.
fn cap_state(spent: f64, soft: Option<f64>, hard: Option<f64>) -> (&'static str, ReportTone) {
    if hard.is_some_and(|hard| spent >= hard) {
        return ("blocked", ReportTone::Bad);
    }
    if soft.is_some_and(|soft| spent >= soft) {
        return ("downgrading", ReportTone::Warn);
    }
    if soft.is_some() || hard.is_some() {
        return ("under", ReportTone::Ok);
    }
    ("no cap", ReportTone::Muted)
}

/// `AiPolicyService.GetSpend` as the `:ai budget status` table.
///
/// Eight rows: each class's daily and monthly spend, in dollars and in tokens,
/// each against the caps that dimension is actually measured by. The tone and
/// the glyph come from [`cap_state`], which is the same ladder the status bar's
/// budget indicator climbs — so the report and the bar cannot disagree about
/// whether a scope is throttled.
#[must_use]
pub fn budget_rows(response: &GetSpendResponse) -> Vec<ReportRow> {
    let mut rows = Vec::new();
    for class in [response.all.as_ref(), response.bulk.as_ref()]
        .into_iter()
        .flatten()
    {
        // Where the caps came from, on the row rather than in a note of its
        // own: "unset" and "set to exactly the configured default" behave
        // identically until the configuration changes, and a reader deciding
        // whether to edit them needs to know which one they are looking at.
        let label = format!(
            "{} ({})",
            class_label(class.class()),
            if class.stored { "set" } else { "ai.limits" }
        );
        let caps = class.caps.unwrap_or_default();
        for (window, spend, window_caps) in [
            (
                format!("today {}", response.day),
                class.daily.unwrap_or_default(),
                caps.daily.unwrap_or_default(),
            ),
            (
                format!("month {}", response.month),
                class.monthly.unwrap_or_default(),
                caps.monthly.unwrap_or_default(),
            ),
        ] {
            let (state, tone) = cap_state(spend.usd, window_caps.soft_usd, window_caps.hard_usd);
            rows.push(
                ReportRow::new([
                    label.clone(),
                    window.clone(),
                    "dollars".to_owned(),
                    budget_usd(spend.usd),
                    cap_usd(window_caps.soft_usd),
                    cap_usd(window_caps.hard_usd),
                    state.to_owned(),
                ])
                .toned(tone),
            );
            // Tokens through the same ladder, as floats only to reuse it: a
            // token count that exceeds `f64`'s exact integer range would need
            // more tokens than any provider has ever served, and the comparison
            // is `>=` against a cap in the same units either way.
            #[allow(clippy::cast_precision_loss)]
            let (state, tone) = cap_state(
                spend.tokens as f64,
                window_caps.soft_tokens.map(|cap| cap as f64),
                window_caps.hard_tokens.map(|cap| cap as f64),
            );
            rows.push(
                ReportRow::new([
                    label.clone(),
                    window,
                    "tokens".to_owned(),
                    spend.tokens.to_string(),
                    cap_tokens(window_caps.soft_tokens),
                    cap_tokens(window_caps.hard_tokens),
                    state.to_owned(),
                ])
                .toned(tone),
            );
        }
    }
    rows
}

/// The caps in force for one class, as the form's `(flag, value)` pre-fill.
///
/// Absent caps are *omitted* rather than sent as an empty string, so a field
/// this build has no answer for keeps whatever the typed line put in it. An
/// uncapped dimension therefore shows as the empty field it is, which is what
/// applying will store — see `commands::ai_policy::fields`.
///
/// Formatted with `Display` rather than a fixed precision so the value in the
/// field is the shortest text that parses back to the same number: a cap of `5`
/// reads as `5`, not `5.0000`, and a form is read by a person before it is
/// parsed by anything.
#[must_use]
pub fn budget_fields(response: &GetSpendResponse, bulk: bool) -> Vec<(String, String)> {
    let class = if bulk {
        response.bulk.as_ref()
    } else {
        response.all.as_ref()
    };
    let caps = class.and_then(|class| class.caps).unwrap_or_default();
    let daily = caps.daily.unwrap_or_default();
    let monthly = caps.monthly.unwrap_or_default();
    let mut fields = Vec::new();
    let mut usd = |flag: &str, value: Option<f64>| {
        if let Some(value) = value {
            fields.push((flag.to_owned(), value.to_string()));
        }
    };
    usd("daily-soft-usd", daily.soft_usd);
    usd("daily-hard-usd", daily.hard_usd);
    usd("monthly-soft-usd", monthly.soft_usd);
    usd("monthly-hard-usd", monthly.hard_usd);
    let mut tokens = |flag: &str, value: Option<i64>| {
        if let Some(value) = value {
            fields.push((flag.to_owned(), value.to_string()));
        }
    };
    tokens("daily-soft-tokens", daily.soft_tokens);
    tokens("daily-hard-tokens", daily.hard_tokens);
    tokens("monthly-soft-tokens", monthly.soft_tokens);
    tokens("monthly-hard-tokens", monthly.hard_tokens);
    fields
}

/// What `SetBudget` stored, as the one line the status line says.
#[must_use]
pub fn budget_stored(response: &SetBudgetResponse) -> String {
    let caps = response.caps.unwrap_or_default();
    let daily = caps.daily.unwrap_or_default();
    let monthly = caps.monthly.unwrap_or_default();
    // Every dimension, including the uncapped ones, because *clearing* a cap is
    // the outcome somebody needs to see confirmed: this RPC replaces rather than
    // merges, and a line reporting only what was set would look identical
    // whether or not the rest had just been wiped.
    format!(
        "{} {} budget stored — daily soft {}/{} hard {}/{}, monthly soft {}/{} hard {}/{}",
        class_label(response.class()),
        if response.account_id == 0 {
            "global".to_owned()
        } else {
            format!("account {}", response.account_id)
        },
        cap_usd(daily.soft_usd),
        cap_tokens(daily.soft_tokens),
        cap_usd(daily.hard_usd),
        cap_tokens(daily.hard_tokens),
        cap_usd(monthly.soft_usd),
        cap_tokens(monthly.soft_tokens),
        cap_usd(monthly.hard_usd),
        cap_tokens(monthly.hard_tokens),
    )
}

/// A budget class's wire name, spelled as `mail ai budget status` spells it.
fn class_label(class: BudgetClass) -> &'static str {
    match class {
        BudgetClass::All => "all",
        BudgetClass::Bulk => "bulk",
        BudgetClass::Unspecified => "unspecified",
    }
}

/// An AI backend, spelled as `mail ai provider status` spells it.
fn backend_label(kind: AiProviderKind) -> &'static str {
    match kind {
        AiProviderKind::Local => "local (on-device, zero egress)",
        AiProviderKind::Claude => "claude (hosted)",
        AiProviderKind::Unspecified => "none (inherits the daemon-wide setting)",
    }
}

/// `AiPolicyService.GetAiProvider` as the `:ai provider status` table.
#[must_use]
pub fn provider_rows(response: &GetAiProviderResponse) -> Vec<ReportRow> {
    let effective = response.effective();
    let mut rows = vec![
        ReportRow::new([
            "scope".to_owned(),
            if response.account_id == 0 {
                "daemon-wide (every account with no override of its own)".to_owned()
            } else {
                format!("account {}", response.account_id)
            },
        ]),
        ReportRow::new([
            "configured".to_owned(),
            backend_label(response.configured()).to_owned(),
        ]),
        ReportRow::new([
            "override".to_owned(),
            backend_label(response.account_override()).to_owned(),
        ])
        .toned(
            if response.account_override() == AiProviderKind::Unspecified {
                ReportTone::Muted
            } else {
                ReportTone::Plain
            },
        ),
        ReportRow::new(["effective".to_owned(), backend_label(effective).to_owned()])
            .toned(ReportTone::Ok),
        ReportRow::new(["ai.policy mode".to_owned(), response.policy_mode.clone()]),
        ReportRow::new([
            "network provider".to_owned(),
            if response.network_provider_built {
                "built".to_owned()
            } else {
                "not built — nothing in this daemon can dial out for AI".to_owned()
            },
        ])
        // Muted rather than warned about: an absent network provider is the
        // structural half of the local-only guarantee holding, not a fault.
        .toned(if response.network_provider_built {
            ReportTone::Plain
        } else {
            ReportTone::Muted
        }),
        ReportRow::new(["local model".to_owned(), response.local_model.clone()]),
    ];
    // Bad rather than Warn only when the local path is the one that will serve
    // the next call: a local backend nobody is routed to being unready is a
    // fact about this host, and drawing it red would cry wolf on every
    // hosted-backend install.
    let tone = match (response.local_ready, effective) {
        (true, _) => ReportTone::Ok,
        (false, AiProviderKind::Local) => ReportTone::Bad,
        (false, _) => ReportTone::Muted,
    };
    rows.push(
        ReportRow::new([
            "local ready".to_owned(),
            if response.local_ready { "yes" } else { "no" }.to_owned(),
        ])
        .toned(tone),
    );
    // Drawn whether or not the path is ready: when it is, this says where the
    // weights were found; when it is not, it is the fix.
    rows.push(ReportRow::new([
        "local detail".to_owned(),
        response.local_detail.clone(),
    ]));
    rows
}

/// What `SetAiProvider` stored, as the one line the status line says.
#[must_use]
pub fn provider_set(response: &SetAiProviderResponse) -> String {
    format!(
        "override {} → calls for {} now use {}",
        backend_label(response.provider()),
        if response.account_id == 0 {
            "every account with no override of its own".to_owned()
        } else {
            format!("account {}", response.account_id)
        },
        backend_label(response.effective()),
    )
}

/// How seriously a scan's findings are taken, spelled as the CLI spells it.
fn severity_label(severity: InjectionSeverity) -> &'static str {
    match severity {
        InjectionSeverity::Hostile => "hostile",
        InjectionSeverity::Suspicious => "suspicious",
        InjectionSeverity::Unspecified => "unknown",
    }
}

/// `AiSafetyService.ScanInjection` as the `:ai scan` table.
///
/// The `actions` row carries the invocation that changes the state it reports:
/// `<enter>` on a withheld message releases it, and on a confirmed one withdraws
/// the confirmation. Not bang'd, unlike the tag suggestions — releasing a
/// safety hold is consent to AI-decided mail mutations, and task 90's gate
/// asking `[y/N]` for exactly that is the gate earning its keep. The row is also
/// the reason `:ai confirm` is a verb: a row's action *is* an `Invocation`.
#[must_use]
pub fn injection_rows(scan: &ScanInjectionResponse) -> Vec<ReportRow> {
    if !scan.flagged {
        return vec![ReportRow::new([
            "clean".to_owned(),
            format!("message {}: no prompt-injection signals", scan.message_id),
        ])
        .toned(ReportTone::Ok)];
    }
    let severity = scan.severity();
    let mut rows = vec![
        ReportRow::new(["severity".to_owned(), severity_label(severity).to_owned()]).toned(
            match severity {
                InjectionSeverity::Hostile => ReportTone::Bad,
                InjectionSeverity::Suspicious => ReportTone::Warn,
                InjectionSeverity::Unspecified => ReportTone::Plain,
            },
        ),
        ReportRow::new(["kinds".to_owned(), scan.kinds.join(", ")]),
    ];
    let actions = ReportRow::new([
        "actions".to_owned(),
        if scan.actions_withheld {
            "withheld — a rule matching on claude_is will not act here".to_owned()
        } else if scan.confirmed_at > 0 {
            "allowed (confirmed)".to_owned()
        } else {
            "allowed (below the configured block threshold)".to_owned()
        },
    ])
    .toned(if scan.actions_withheld {
        ReportTone::Bad
    } else {
        ReportTone::Ok
    });
    rows.push(match confirm_invocation(scan.confirmed_at > 0) {
        Some(invocation) => actions.running(invocation),
        // Unreachable: `ai confirm` is declared, and
        // `command::tests::every_real_verb_is_reachable_by_typing_its_own_path`
        // is what keeps it so. A row that does nothing beats a panic in a
        // client holding a terminal in raw mode.
        None => actions,
    });
    rows.push(ReportRow::new([
        "scanned".to_owned(),
        when(scan.scanned_at),
    ]));
    rows.push(ReportRow::new([
        "confirmed".to_owned(),
        when(scan.confirmed_at),
    ]));
    for detection in &scan.detections {
        rows.push(
            ReportRow::new([
                detection.kind.clone(),
                // Already stripped of invisible and bidi-override characters by
                // the daemon, and put through `safe_line` anyway: this client
                // never renders remote text it has not bounded itself.
                detection.excerpt.clone(),
                format!("byte {}", detection.offset),
            ])
            .toned(ReportTone::Warn),
        );
    }
    rows
}

/// The `:ai confirm` invocation the `actions` row runs.
///
/// Parsed rather than built field by field, so the row runs exactly what typing
/// that line runs — including the capability task 90's gate reads.
fn confirm_invocation(confirmed: bool) -> Option<command::Invocation> {
    let line = if confirmed {
        "ai confirm --revoke"
    } else {
        "ai confirm"
    };
    match command::parse(line) {
        Ok(command::Resolution::Invocation(invocation)) => Some(*invocation),
        _ => None,
    }
}

/// `AuditService.QueryAiCalls` (and `ExportLedger`) as the `:ai audit` table.
///
/// Newest first, which is the order both RPCs send in — not re-sorted here: a
/// client that reordered a ledger would be a client whose page boundaries no
/// longer matched the cursor the daemon paginates by.
#[must_use]
pub fn audit_rows(entries: &[AuditEntry]) -> Vec<ReportRow> {
    entries.iter().map(audit_row).collect()
}

/// One ledger row.
#[must_use]
pub fn audit_row(entry: &AuditEntry) -> ReportRow {
    let tokens = entry.input_tokens
        + entry.output_tokens
        + entry.cache_creation_input_tokens
        + entry.cache_read_input_tokens;
    let failed = entry.status() == CallStatus::Error;
    ReportRow::new([
        when(entry.created_at),
        entry.model.clone(),
        entry.pass.clone().unwrap_or_else(|| "-".to_owned()),
        tokens.to_string(),
        budget_usd(entry.cost_usd),
        match (&entry.error, failed) {
            (Some(error), _) => error.clone(),
            (None, true) => "error".to_owned(),
            (None, false) => format!("ok · {} ms · {}", entry.latency_ms, entry.redaction_level),
        },
    ])
    .toned(if failed {
        ReportTone::Bad
    } else {
        ReportTone::Plain
    })
}

// ---------------------------------------------------------------------------
// accounts and tokens (task 97)
// ---------------------------------------------------------------------------

/// A credential source as the listing names it, and never the credential.
///
/// `Account::credential_kind` is one of `none|command|env|keychain|oauth` and
/// `credential_ref` is the command, the variable name or the service — which is
/// *how to obtain* the password, so it is safe to draw. The password itself never
/// crosses this API at all.
fn credential_label(kind: &str, reference: Option<&str>) -> String {
    match reference {
        Some(reference) if !reference.is_empty() => format!("{kind} {reference}"),
        _ => kind.to_owned(),
    }
}

/// A host and port as one cell, or `-` when the account has none stored.
fn endpoint(host: Option<&String>, port: Option<u32>) -> String {
    match (host, port) {
        (Some(host), Some(port)) => format!("{host}:{port}"),
        (Some(host), None) => host.clone(),
        (None, _) => "-".to_owned(),
    }
}

/// `AccountService.List` as the `:account list` table.
///
/// The row for the account on screen carries `:account use <id>`, which is what
/// makes the listing the way somebody switches: a row's action *is* an
/// `Invocation`, so the gesture and the typed line are the same thing. Every
/// row carries it, including the open one — `use_account` answers that with
/// "already looking at it" rather than a reload, which is a better outcome than
/// a row that does nothing when pressed.
#[must_use]
pub fn account_rows(response: &ListAccountsResponse, open: i64) -> Vec<ReportRow> {
    response
        .accounts
        .iter()
        .map(|account| {
            let row = ReportRow::new([
                account.id.to_string(),
                account.name.clone(),
                account.username.clone().unwrap_or_else(|| "-".to_owned()),
                endpoint(account.imap_server.as_ref(), account.imap_port),
                credential_label(&account.credential_kind, account.credential_ref.as_deref()),
            ])
            .toned(if open == account.id {
                ReportTone::Ok
            } else {
                ReportTone::Plain
            });
            match use_invocation(account.id) {
                Some(invocation) => row.running(invocation),
                // Unreachable: `account use` is declared, and
                // `command::tests::every_real_verb_is_reachable_by_typing_its_own_path`
                // is what keeps it so. A row that does nothing beats a panic in
                // a client holding a terminal in raw mode.
                None => row,
            }
        })
        .collect()
}

/// The `:account use <id>` invocation a listing row runs.
///
/// Parsed rather than built field by field, so the row runs exactly what typing
/// that line runs. Bang'd for the reason the tag suggestions are: the gesture is
/// the consent — the row says which account, and the border says Enter switches —
/// and switching is entirely reversible by switching back.
fn use_invocation(account_id: i64) -> Option<command::Invocation> {
    match command::parse(&format!("account use {account_id}!")) {
        Ok(command::Resolution::Invocation(invocation)) => Some(*invocation),
        _ => None,
    }
}

/// One account's settings as the `:account show` table.
#[must_use]
pub fn account_fields(account: &ProtoAccount) -> Vec<ReportRow> {
    vec![
        ReportRow::new(["id".to_owned(), account.id.to_string()]),
        ReportRow::new(["name".to_owned(), account.name.clone()]),
        ReportRow::new([
            "login".to_owned(),
            account.username.clone().unwrap_or_else(|| "-".to_owned()),
        ]),
        ReportRow::new([
            "imap".to_owned(),
            endpoint(account.imap_server.as_ref(), account.imap_port),
        ]),
        ReportRow::new([
            "smtp".to_owned(),
            endpoint(account.smtp_server.as_ref(), account.smtp_port),
        ]),
        ReportRow::new([
            "credential".to_owned(),
            credential_label(&account.credential_kind, account.credential_ref.as_deref()),
        ])
        .toned(if account.credential_kind == "none" {
            // Not an error — an account can exist before its credential does —
            // but it is why a sync would fail, so it is not drawn as ordinary
            // data either.
            ReportTone::Warn
        } else {
            ReportTone::Plain
        }),
        ReportRow::new(["created".to_owned(), when(account.created_at)]),
        ReportRow::new(["updated".to_owned(), when(account.updated_at)]),
    ]
}

/// What `AccountService.Create` stored, as the one line the status line says.
#[must_use]
pub fn account_created(account: &ProtoAccount) -> String {
    format!(
        "account {} stored — :account use {} to look at it, :sync now to fill it",
        account.name, account.id
    )
}

/// `AccountService.TestConnection` as a table.
#[must_use]
pub fn account_test_rows(response: &TestConnectionResponse) -> Vec<ReportRow> {
    vec![
        ReportRow::new([
            "login".to_owned(),
            if response.ok { "ok" } else { "failed" }.to_owned(),
        ])
        .toned(if response.ok {
            ReportTone::Ok
        } else {
            ReportTone::Bad
        }),
        // The detail is the answer when it failed and the capability list when
        // it did not, so it is drawn either way.
        ReportRow::new(["detail".to_owned(), response.detail.clone()]),
    ]
}

/// `AccountService.Autoconfigure` as the `:account add` table.
///
/// Three things a reader needs, in the order they need them: whether this is
/// safe to apply, what it says, and how to apply it.
///
/// The last two rows are the affordances the proposal exists for. One writes the
/// `[[accounts]]` block to a private file and opens it, which is how a block gets
/// copied into `rmail.toml` without this client growing a clipboard dependency.
/// The other carries a `:account new …` line built flag by flag from what was
/// discovered — so applying the proposal is a `:` line somebody could have typed,
/// and the settings on it are visible before it runs.
#[must_use]
pub fn autoconfigure_rows(email: &str, response: &AutoconfigureResponse) -> Vec<ReportRow> {
    let mut rows = vec![ReportRow::new([
        "source".to_owned(),
        match response.source.as_str() {
            // Named rather than passed through, because "model" is the one
            // source a reader must treat differently: it is a guess, validated
            // but still a guess, and the proto says so.
            "model" => "model — a guess, validated but not discovered".to_owned(),
            other => other.to_owned(),
        },
    ])
    .toned(if response.source == "model" {
        ReportTone::Warn
    } else {
        ReportTone::Plain
    })];
    for (label, server) in [
        ("imap", response.imap.as_ref()),
        ("smtp", response.smtp.as_ref()),
    ] {
        let Some(server) = server else {
            continue;
        };
        rows.push(ReportRow::new([
            label.to_owned(),
            format!(
                "{}:{} {} · login {}",
                server.host, server.port, server.security, server.username
            ),
        ]));
    }
    rows.push(
        ReportRow::new([
            "verified".to_owned(),
            if response.login_validated {
                "yes — a real IMAP login succeeded".to_owned()
            } else if response.validation_detail.is_empty() {
                "no credential given, so nothing was checked".to_owned()
            } else {
                response.validation_detail.clone()
            },
        ])
        .toned(if response.login_validated {
            ReportTone::Ok
        } else {
            // Muted rather than warned about: no credential means no check was
            // asked for, and drawing that red would cry wolf on the ordinary
            // case. A check that *failed* puts its reason in the same cell.
            ReportTone::Muted
        }),
    );
    if response.existing_account_id != 0 {
        rows.push(
            ReportRow::new([
                "existing".to_owned(),
                format!(
                    "account {} is already configured for this address — nothing was changed",
                    response.existing_account_id
                ),
            ])
            .toned(ReportTone::Warn),
        );
    }
    for warning in &response.warnings {
        rows.push(ReportRow::new(["warning".to_owned(), warning.clone()]).toned(ReportTone::Warn));
    }
    if !response.toml.is_empty() {
        let row = ReportRow::new([
            "toml".to_owned(),
            "open the [[accounts]] block, to paste into rmail.toml".to_owned(),
        ]);
        rows.push(match toml_invocation() {
            Some(invocation) => row.running(invocation),
            // Unreachable: `toml` is declared. A row that does nothing beats a
            // panic in a client holding a terminal in raw mode.
            None => row,
        });
    }
    // Offered only when there is no account for this address yet: `Create` would
    // otherwise make a second one, and the report already says which account
    // exists. The TOML row is still there either way, which is the answer for
    // somebody who really does want two.
    if response.existing_account_id == 0 {
        if let Some(invocation) = new_account_invocation(email, response) {
            rows.push(
                ReportRow::new([
                    "apply".to_owned(),
                    format!(":{}", invocation.verb.join(" ")),
                ])
                .toned(ReportTone::Ok)
                .running(invocation),
            );
        }
    }
    rows
}

/// The `:account new …` invocation the `apply` row runs, built from what was
/// discovered.
///
/// Every value goes through `command::quoted`, which is not decoration: a
/// username comes out of an autoconfig document fetched over the network, so it
/// is untrusted text, and one containing a space pasted onto a line unquoted
/// would split into two tokens and ask the verb about something nobody typed.
///
/// Not bang'd. Creating an account is a mutation task 90's gate should ask about,
/// and this is the row where asking is right: the proposal may have come from a
/// model, and `[y/N]` in front of it is the one moment a reader is looking at
/// both the settings and the question.
fn new_account_invocation(
    email: &str,
    response: &AutoconfigureResponse,
) -> Option<command::Invocation> {
    let imap = response.imap.as_ref()?;
    let mut line = format!("account new {}", command::quoted(email));
    line.push_str(&format!(
        " --imap-server={} --imap-port={} --username={}",
        command::quoted(&imap.host),
        imap.port,
        command::quoted(&imap.username),
    ));
    if let Some(smtp) = response.smtp.as_ref() {
        line.push_str(&format!(
            " --smtp-server={} --smtp-port={}",
            command::quoted(&smtp.host),
            smtp.port,
        ));
    }
    match command::parse(&line) {
        Ok(command::Resolution::Invocation(invocation)) => Some(*invocation),
        _ => None,
    }
}

/// The `:toml` invocation a block row runs.
///
/// A verb rather than a row-only gesture, for the reachability rule this client
/// holds everywhere: a report row's action *is* an `Invocation`, so an
/// affordance that only a row could reach would be one nobody could type — and
/// the verb registry is also the command index, so it would document nothing
/// either.
///
/// Bang'd: opening a file this process wrote, read-only, in the platform's own
/// handler is not a thing to ask about.
pub fn toml_invocation() -> Option<command::Invocation> {
    match command::parse("toml!") {
        Ok(command::Resolution::Invocation(invocation)) => Some(*invocation),
        _ => None,
    }
}

/// `AccountService.BeginOAuth` as the first frame of the `:account login` flow.
///
/// The URL is drawn even when the client is about to hand it to a browser: a
/// browser that does not launch, or launches somewhere the user is not logged
/// in, leaves the URL as the only way to finish — and a flow whose URL scrolled
/// past unread is a flow that cannot be recovered.
#[must_use]
pub fn oauth_started_rows(response: &BeginOAuthResponse) -> Vec<ReportRow> {
    vec![
        ReportRow::new([
            "open".to_owned(),
            // Bounded by the report's own cell cap, and safe-lined by
            // `ReportRow::new`, like every other remote string here.
            response.authorization_url.clone(),
        ])
        .toned(ReportTone::Ok),
        ReportRow::new(["redirect".to_owned(), response.redirect_uri.clone()]),
        ReportRow::new([
            "expires".to_owned(),
            format!("{} — the port is released then", when(response.expires_at)),
        ]),
        ReportRow::new([
            "waiting".to_owned(),
            "for the browser to come back…".to_owned(),
        ])
        .toned(ReportTone::Muted),
    ]
}

/// The row that says the browser did not launch.
///
/// Appended to the first frame rather than replacing it, so the URL stays where
/// it was: the launch failing is exactly when the URL is the only way to finish.
#[must_use]
pub fn oauth_no_browser_row() -> ReportRow {
    ReportRow::new([
        "browser".to_owned(),
        "could not be launched — open the URL above by hand".to_owned(),
    ])
    .toned(ReportTone::Warn)
}

/// `AccountService.CompleteOAuth` as the terminal frame of the same flow.
#[must_use]
pub fn oauth_done_rows(response: &CompleteOAuthResponse) -> Vec<ReportRow> {
    vec![
        ReportRow::new(["account".to_owned(), response.account_id.to_string()]),
        ReportRow::new(["provider".to_owned(), response.provider.clone()]),
        ReportRow::new(["granted".to_owned(), scope_list(&response.scopes)]),
        ReportRow::new([
            "access token".to_owned(),
            format!("expires {}", when(response.expires_at)),
        ]),
        ReportRow::new([
            "refresh token".to_owned(),
            "stored in the Keychain by the daemon — it never crossed this process".to_owned(),
        ])
        .toned(ReportTone::Ok),
    ]
}

/// `AccountService.RefreshToken` as a table.
#[must_use]
pub fn refresh_rows(response: &RefreshTokenResponse) -> Vec<ReportRow> {
    vec![
        ReportRow::new([
            "refreshed".to_owned(),
            if response.refreshed {
                "yes — this went to the provider".to_owned()
            } else {
                "no — the stored token was still good".to_owned()
            },
        ])
        .toned(if response.refreshed {
            ReportTone::Ok
        } else {
            ReportTone::Muted
        }),
        ReportRow::new(["provider".to_owned(), response.provider.clone()]),
        ReportRow::new(["expires".to_owned(), when(response.expires_at)]),
        ReportRow::new(["scopes".to_owned(), scope_list(&response.scopes)]),
    ]
}

/// A scope list, or a word saying there were none.
///
/// An empty cell reads as a rendering fault; "none" reads as the answer.
fn scope_list(scopes: &[String]) -> String {
    if scopes.is_empty() {
        return "none reported".to_owned();
    }
    scopes.join(", ")
}

/// `AdminService.ListTokens` as the `:token list` table. Metadata only — this
/// RPC never returns a secret or its hash.
#[must_use]
pub fn token_rows(response: &ListTokensResponse) -> Vec<ReportRow> {
    response
        .tokens
        .iter()
        .map(|token| {
            ReportRow::new([
                token.id.to_string(),
                token.name.clone(),
                if token.revoked { "revoked" } else { "active" }.to_owned(),
                token.last_used_at.map_or_else(
                    // Distinct from `when(0)`'s "never", which would be true of
                    // both: a token that has never been used and a daemon that
                    // does not record it are different facts.
                    || "unknown".to_owned(),
                    when,
                ),
                token.expires_at.map_or_else(|| "never".to_owned(), when),
                scope_list(&token.scopes),
            ])
            .toned(if token.revoked {
                ReportTone::Muted
            } else {
                ReportTone::Ok
            })
        })
        .collect()
}

/// `AdminService.MintToken` as the one table that carries a secret.
///
/// The secret is a row and nothing else — not the status line, not the history,
/// not a field on the model — so closing the pane is what makes it unrecoverable,
/// which is exactly what the daemon has already made it: only an argon2id hash is
/// persisted, so `ListTokens` cannot show it and neither can anything else.
///
/// The marker row says so in as many words, immediately after it. A reader who
/// does not know will close the pane, and there is no second chance to tell them.
#[must_use]
pub fn minted_rows(response: &MintTokenResponse) -> Vec<ReportRow> {
    vec![
        ReportRow::new(["id".to_owned(), response.id.to_string()]),
        ReportRow::new(["name".to_owned(), response.name.clone()]),
        ReportRow::new(["scopes".to_owned(), scope_list(&response.scopes)]),
        ReportRow::new([
            "expires".to_owned(),
            response.expires_at.map_or_else(|| "never".to_owned(), when),
        ]),
        ReportRow::new(["token".to_owned(), response.token.clone()]).toned(ReportTone::Ok),
        ReportRow::new([
            "keep it".to_owned(),
            format!(
                "this cannot be shown again — only revoked, with :token revoke {}",
                response.id
            ),
        ])
        .toned(ReportTone::Bad),
    ]
}

// ---------------------------------------------------------------------------
// automation and notifications (task 98)
// ---------------------------------------------------------------------------

/// A webhook event's wire string, or the enum name for one this build does not
/// know.
///
/// The same vocabulary `HookEvent` uses, deliberately — the protos say so — which
/// is why one function serves both.
fn event_label(event: i32) -> String {
    match WebhookEvent::try_from(event) {
        Ok(WebhookEvent::OnNewMessage) => "on_new_message".to_owned(),
        Ok(WebhookEvent::OnLabel) => "on_label".to_owned(),
        Ok(WebhookEvent::OnMove) => "on_move".to_owned(),
        Ok(WebhookEvent::OnRuleMatch) => "on_rule_match".to_owned(),
        Ok(WebhookEvent::OnSyncError) => "on_sync_error".to_owned(),
        // Rendered as its own number rather than dropped: an event this build
        // has no name for is a newer daemon's, and losing the row entirely would
        // hide a subscription that is real.
        Ok(WebhookEvent::Unspecified) | Err(_) => format!("event {event}"),
    }
}

/// The wire enum for one of `commands::automation::EVENTS`' strings.
///
/// `None` for anything else, which the caller drops. Only reachable with a name
/// the answer table already checked against the same list, so this is the belt to
/// that braces — and dropping is the safe direction: an unrecognised event
/// silently mapped to `UNSPECIFIED` would register a subscription to something
/// nobody asked for.
#[must_use]
pub fn webhook_event(name: &str) -> Option<WebhookEvent> {
    match name {
        "on_new_message" => Some(WebhookEvent::OnNewMessage),
        "on_label" => Some(WebhookEvent::OnLabel),
        "on_move" => Some(WebhookEvent::OnMove),
        "on_rule_match" => Some(WebhookEvent::OnRuleMatch),
        "on_sync_error" => Some(WebhookEvent::OnSyncError),
        _ => None,
    }
}

/// Where a signing key comes from, as the listing names it — a reference, never
/// the key.
fn signing_label(source: i32, reference: &str) -> String {
    let kind = match WebhookSecretSource::try_from(source) {
        Ok(WebhookSecretSource::Env) => "env",
        Ok(WebhookSecretSource::Command) => "command",
        Ok(WebhookSecretSource::Keychain) => "keychain",
        // Honest about what a receiver can verify rather than pretending a
        // constant is a signature — the proto's own words.
        Ok(WebhookSecretSource::Unspecified) | Err(_) => return "unsigned".to_owned(),
    };
    if reference.is_empty() {
        kind.to_owned()
    } else {
        format!("{kind} {reference}")
    }
}

/// `WebhookService.List` (and the single-destination echoes) as a table.
#[must_use]
pub fn destination_rows(destinations: &[WebhookDestination]) -> Vec<ReportRow> {
    destinations.iter().map(destination_row).collect()
}

/// One destination's row.
#[must_use]
pub fn destination_row(destination: &WebhookDestination) -> ReportRow {
    let events = if destination.events.is_empty() {
        // Not a blank cell: a destination that subscribes to nothing is a real
        // and useful configuration — it receives an explicit `:forward` and no
        // firehose — and drawing it empty would read as a rendering fault.
        "forward only".to_owned()
    } else {
        destination
            .events
            .iter()
            .map(|event| event_label(*event))
            .collect::<Vec<_>>()
            .join(",")
    };
    ReportRow::new([
        destination.name.clone(),
        destination.url.clone(),
        if destination.enabled {
            "enabled"
        } else {
            "disabled"
        }
        .to_owned(),
        events,
        if destination.include_body {
            "body included".to_owned()
        } else {
            "notification".to_owned()
        },
        signing_label(destination.secret_source, &destination.secret_reference),
    ])
    .toned(match (destination.enabled, destination.include_body) {
        // A destination entitled to message bodies is the one configuration on
        // this screen worth finding at a glance: it ships the mail itself,
        // redacted, to a third party on every matching message.
        (true, true) => ReportTone::Warn,
        (true, false) => ReportTone::Ok,
        (false, _) => ReportTone::Muted,
    })
}

/// A delivery's state, as the queue names it.
fn delivery_state(state: i32) -> (&'static str, ReportTone) {
    match WebhookDeliveryState::try_from(state) {
        Ok(WebhookDeliveryState::Pending) => ("pending", ReportTone::Plain),
        Ok(WebhookDeliveryState::Delivered) => ("delivered", ReportTone::Ok),
        Ok(WebhookDeliveryState::Failed) => ("failed", ReportTone::Bad),
        Ok(WebhookDeliveryState::Unspecified) | Err(_) => ("unknown", ReportTone::Muted),
    }
}

/// `WebhookService.ListDeliveries` as a table.
#[must_use]
pub fn delivery_rows(deliveries: &[WebhookDelivery]) -> Vec<ReportRow> {
    deliveries.iter().map(delivery_row).collect()
}

/// One delivery's row.
///
/// A failed row carries `:webhook replay <id>` — the only way out of the terminal
/// state, and deliberately something a human does, which is exactly what a row
/// action is. Not bang'd: replaying POSTs the same mail content to a third party
/// again, so task 90's gate asking first is the gate doing its job.
#[must_use]
pub fn delivery_row(delivery: &WebhookDelivery) -> ReportRow {
    let (state, tone) = delivery_state(delivery.state);
    let last = if !delivery.last_error.is_empty() {
        delivery.last_error.clone()
    } else if delivery.delivered_at > 0 {
        when(delivery.delivered_at)
    } else if delivery.next_attempt_at > 0 {
        format!("next {}", when(delivery.next_attempt_at))
    } else if delivery.last_status != 0 {
        format!("HTTP {}", delivery.last_status)
    } else {
        // Distinct from a 500 and not to be read as one: nothing answered at
        // all.
        "no answer yet".to_owned()
    };
    let row = ReportRow::new([
        delivery.id.to_string(),
        delivery.destination_name.clone(),
        delivery.event.clone(),
        state.to_owned(),
        format!("{}/{}", delivery.attempts, delivery.max_attempts),
        last,
    ])
    .toned(tone);
    if WebhookDeliveryState::try_from(delivery.state) != Ok(WebhookDeliveryState::Failed) {
        return row;
    }
    match replay_invocation(delivery.id) {
        Some(invocation) => row.running(invocation),
        None => row,
    }
}

/// The `:webhook replay <id>` invocation a failed row runs.
fn replay_invocation(delivery_id: i64) -> Option<command::Invocation> {
    match command::parse(&format!("webhook replay {delivery_id}")) {
        Ok(command::Resolution::Invocation(invocation)) => Some(*invocation),
        _ => None,
    }
}

/// What `WebhookService.Forward` queued, as the one line the status line says.
///
/// "Queued", never "sent" — and it says so louder when no dispatcher is running:
/// a client reporting a send on a daemon with `webhooks.enabled = false` would be
/// the lie the response's own `dispatcher_running` field exists to prevent.
#[must_use]
pub fn forwarded(response: &ForwardMessageResponse) -> String {
    let id = response.delivery.as_ref().map_or(0, |delivery| delivery.id);
    if response.dispatcher_running {
        format!("queued as delivery {id} — it goes out on the next dispatch tick")
    } else {
        format!(
            "queued as delivery {id}, but no dispatcher is running \
             (webhooks.enabled) — it is durably queued, not sent"
        )
    }
}

/// A hook's event, as `HookService` names it.
fn hook_event_label(event: i32) -> String {
    match HookEvent::try_from(event) {
        Ok(HookEvent::OnNewMessage) => "on_new_message".to_owned(),
        Ok(HookEvent::OnLabel) => "on_label".to_owned(),
        Ok(HookEvent::OnMove) => "on_move".to_owned(),
        Ok(HookEvent::OnRuleMatch) => "on_rule_match".to_owned(),
        Ok(HookEvent::OnSyncError) => "on_sync_error".to_owned(),
        Ok(HookEvent::Unspecified) | Err(_) => format!("event {event}"),
    }
}

/// `HookService.ListHooks` as a table.
#[must_use]
pub fn hook_rows(response: &ListHooksResponse) -> Vec<ReportRow> {
    response
        .hooks
        .iter()
        .map(|hook| {
            let command = if hook.args.is_empty() {
                hook.command.clone()
            } else {
                format!("{} {}", hook.command, hook.args.join(" "))
            };
            ReportRow::new([
                hook.name.clone(),
                hook_event_label(hook.event),
                if hook.enabled { "enabled" } else { "disabled" }.to_owned(),
                format!("{} ms", hook.timeout_ms),
                command,
            ])
            .toned(if hook.enabled {
                ReportTone::Ok
            } else {
                ReportTone::Muted
            })
        })
        .collect()
}

/// `HookService.TestHook` as a table.
///
/// Four outcomes the proto distinguishes and this does too, because they are
/// different operational facts: it exited with a code, it was killed for
/// exceeding its timeout, the daemon shut down under it, or it could not be
/// spawned at all.
#[must_use]
pub fn hook_test_rows(response: &TestHookResponse) -> Vec<ReportRow> {
    let (outcome, tone) = if response.timed_out {
        (
            "killed — it exceeded its timeout".to_owned(),
            ReportTone::Bad,
        )
    } else if response.cancelled {
        (
            "cancelled — the daemon shut down mid-run".to_owned(),
            ReportTone::Warn,
        )
    } else {
        match response.exit_code {
            Some(0) => ("exit 0".to_owned(), ReportTone::Ok),
            Some(code) => (format!("exit {code}"), ReportTone::Bad),
            None => (
                "never ran — the command could not be spawned".to_owned(),
                ReportTone::Bad,
            ),
        }
    };
    let mut rows = vec![
        ReportRow::new(["outcome".to_owned(), outcome]).toned(tone),
        ReportRow::new(["took".to_owned(), format!("{} ms", response.duration_ms)]),
    ];
    for (label, text) in [("stdout", &response.stdout), ("stderr", &response.stderr)] {
        if text.trim().is_empty() {
            continue;
        }
        // A line per line, for the reason a config block is drawn that way: this
        // is output somebody is reading, and folded into one cell it would be
        // elided at the column width.
        for line in text.lines() {
            rows.push(ReportRow::new([label.to_owned(), line.to_owned()]));
        }
    }
    rows
}

/// A notification tier, as the config file spells it.
fn tier_label(tier: i32) -> &'static str {
    match NotificationTier::try_from(tier) {
        Ok(NotificationTier::Low) => "low",
        Ok(NotificationTier::Normal) => "normal",
        Ok(NotificationTier::High) => "high",
        Ok(NotificationTier::Critical) => "critical",
        Ok(NotificationTier::Unspecified) | Err(_) => "unscored",
    }
}

/// How loud a tier is, for a row's tone.
fn tier_tone(tier: i32) -> ReportTone {
    match NotificationTier::try_from(tier) {
        Ok(NotificationTier::Critical) => ReportTone::Bad,
        Ok(NotificationTier::High) => ReportTone::Warn,
        Ok(NotificationTier::Normal) => ReportTone::Plain,
        Ok(NotificationTier::Low) => ReportTone::Muted,
        Ok(NotificationTier::Unspecified) | Err(_) => ReportTone::Muted,
    }
}

/// One `Alert` as a row of the live `:notify list` report.
#[must_use]
pub fn alert_row(alert: &Alert) -> ReportRow {
    ReportRow::new([
        when(alert.delivered_at),
        tier_label(alert.tier).to_owned(),
        alert.account.clone(),
        alert.from.clone().unwrap_or_else(|| "-".to_owned()),
        alert
            .subject
            .clone()
            .unwrap_or_else(|| NO_SUBJECT.to_owned()),
        alert.reason.clone(),
    ])
    .toned(tier_tone(alert.tier))
}

/// `NotificationService.ScoreMessage` as a table.
///
/// The interesting answer is usually not the tier but *why nothing happened*, so
/// the state, the threshold it was measured against and whether the account has
/// notifications on at all are all rows rather than something a reader has to
/// infer from a tier.
#[must_use]
pub fn score_rows(response: &ScoreMessageResponse) -> Vec<ReportRow> {
    let (state, tone) = match NotificationState::try_from(response.state) {
        Ok(NotificationState::Pending) => ("pending", ReportTone::Plain),
        Ok(NotificationState::Delivered) => ("delivered", ReportTone::Ok),
        Ok(NotificationState::Suppressed) => ("suppressed", ReportTone::Muted),
        // "We chose not to" and "we could not" are different facts, which is why
        // the proto keeps them apart and why only one of them is drawn as a
        // failure.
        Ok(NotificationState::Failed) => ("failed", ReportTone::Bad),
        Ok(NotificationState::Queued) => ("queued — scoring now", ReportTone::Plain),
        Ok(NotificationState::Unspecified) | Err(_) => ("unknown", ReportTone::Muted),
    };
    let mut rows = vec![ReportRow::new(["state".to_owned(), state.to_owned()]).toned(tone)];
    rows.push(
        ReportRow::new([
            "tier".to_owned(),
            response.tier.map_or_else(
                || "not scored yet".to_owned(),
                |tier| tier_label(tier).to_owned(),
            ),
        ])
        .toned(response.tier.map_or(ReportTone::Muted, tier_tone)),
    );
    if let Some(reason) = response.reason.as_ref() {
        rows.push(ReportRow::new(["why".to_owned(), reason.clone()]));
    }
    if !response.suppressed_reason.is_empty() {
        rows.push(
            ReportRow::new(["suppressed".to_owned(), response.suppressed_reason.clone()])
                .toned(ReportTone::Muted),
        );
    }
    rows.push(ReportRow::new([
        "threshold".to_owned(),
        response.effective_threshold.clone(),
    ]));
    rows.push(
        ReportRow::new([
            "account".to_owned(),
            if response.account_enabled {
                "notifications on".to_owned()
            } else {
                "notifications off for this account".to_owned()
            },
        ])
        .toned(if response.account_enabled {
            ReportTone::Plain
        } else {
            ReportTone::Muted
        }),
    );
    rows.push(
        ReportRow::new([
            "would notify".to_owned(),
            if response.would_notify { "yes" } else { "no" }.to_owned(),
        ])
        .toned(if response.would_notify {
            ReportTone::Ok
        } else {
            ReportTone::Muted
        }),
    );
    rows
}

// ---------------------------------------------------------------------------
// content, export and analytics (task 99)
// ---------------------------------------------------------------------------

/// A duration in seconds, as a report draws one.
///
/// Rounded to a unit somebody reads rather than printed exactly: a p50 of
/// `19_847` seconds is `5h 30m`, and the number nobody can hold in their head is
/// the one that makes the column useless.
#[must_use]
pub fn duration(seconds: i64) -> String {
    if seconds <= 0 {
        return "-".to_owned();
    }
    let (days, rest) = (seconds / 86_400, seconds % 86_400);
    let (hours, rest) = (rest / 3_600, rest % 3_600);
    let minutes = rest / 60;
    if days > 0 {
        return format!("{days}d {hours}h");
    }
    if hours > 0 {
        return format!("{hours}h {minutes}m");
    }
    if minutes > 0 {
        return format!("{minutes}m");
    }
    format!("{seconds}s")
}

/// `ExportService.Export`'s terminal frame as a table.
///
/// The bytes went to disk; what a reader wants is what landed and what did not.
/// `skipped_without_raw` is a row rather than a footnote: a message whose raw
/// bytes this daemon never stored cannot be exported, and an archive quietly
/// short by forty messages is worse than one that says so.
#[must_use]
pub fn export_rows(to: &str, done: &ExportDone) -> Vec<ReportRow> {
    let mut rows = vec![
        ReportRow::new(["written to".to_owned(), to.to_owned()]),
        ReportRow::new(["messages".to_owned(), done.messages.to_string()]).toned(ReportTone::Ok),
        ReportRow::new(["bytes".to_owned(), done.bytes.to_string()]),
    ];
    if done.skipped_without_raw > 0 {
        rows.push(
            ReportRow::new([
                "skipped".to_owned(),
                format!(
                    "{} had no stored raw message and could not be exported",
                    done.skipped_without_raw
                ),
            ])
            .toned(ReportTone::Warn),
        );
    }
    rows
}

/// One `ResponseStats` figure, or `-` when there are no samples.
///
/// Zero samples is not a p50 of zero: a contact who has never been replied to has
/// no median reply time, and printing `0s` would read as the fastest possible
/// answer instead of no answer at all.
fn stat(stats: Option<&ResponseStats>, pick: fn(&ResponseStats) -> i64) -> String {
    match stats {
        Some(stats) if stats.samples > 0 => duration(pick(stats)),
        _ => "-".to_owned(),
    }
}

/// `AnalyticsService.GetResponseTimes` as a table.
///
/// A row per group, and the note column is the point of the report: `bottleneck`
/// means the reader is the slow side, `stalled` means the thread has gone quiet
/// on their turn. Both are the daemon's own verdicts rather than a comparison
/// this client re-derives — the thresholds live in the request.
#[must_use]
pub fn response_time_rows(response: &GetResponseTimesResponse) -> Vec<ReportRow> {
    let mut rows = Vec::new();
    // The overall figures first, labelled, so a reader has something to compare
    // a group against without doing arithmetic.
    rows.push(
        ReportRow::new([
            "— everyone —".to_owned(),
            stat(response.ours.as_ref(), |s| s.p50_seconds),
            stat(response.ours.as_ref(), |s| s.p90_seconds),
            stat(response.theirs.as_ref(), |s| s.p50_seconds),
            String::new(),
            format!("{} pair(s)", response.pairs),
        ])
        .toned(ReportTone::Muted),
    );
    for group in &response.groups {
        let mut note = Vec::new();
        if group.bottleneck {
            note.push("you are the delay");
        }
        if group.slower_than_counterpart {
            note.push("slower than them");
        }
        if group.stalled {
            note.push("stalled");
        }
        rows.push(
            ReportRow::new([
                if group.label.is_empty() {
                    group.key.clone()
                } else {
                    group.label.clone()
                },
                stat(group.ours.as_ref(), |s| s.p50_seconds),
                stat(group.ours.as_ref(), |s| s.p90_seconds),
                stat(group.theirs.as_ref(), |s| s.p50_seconds),
                if group.overdue > 0 {
                    format!("{} ({} late)", group.awaiting_reply, group.overdue)
                } else {
                    group.awaiting_reply.to_string()
                },
                note.join(", "),
            ])
            .toned(if group.overdue > 0 || group.bottleneck {
                ReportTone::Warn
            } else if group.stalled {
                ReportTone::Muted
            } else {
                ReportTone::Plain
            }),
        );
    }
    rows
}

/// `AnalyticsService.GenerateDigest` as a table whose rows open their source.
///
/// A digest line cites the messages it is about, and `<enter>` on the row opens
/// the first of them. That is the acceptance's own requirement and it is why the
/// rows carry an invocation at all: a summary a reader cannot get behind is a
/// summary they have to take on trust.
#[must_use]
pub fn digest_rows(response: &GenerateDigestResponse) -> Vec<ReportRow> {
    if response.empty {
        return vec![ReportRow::new([
            "nothing".to_owned(),
            format!(
                "no mail worth summarizing in this window ({} considered)",
                response.considered
            ),
        ])
        .toned(ReportTone::Muted)];
    }
    let mut rows = Vec::new();
    for section in &response.sections {
        for (index, line) in section.lines.iter().enumerate() {
            let row = ReportRow::new([
                // The heading once per section rather than on every line: a
                // column repeating the same word down the screen is a column
                // carrying no information.
                if index == 0 {
                    section.heading.clone()
                } else {
                    String::new()
                },
                line.text.clone(),
            ]);
            rows.push(
                match line.message_ids.first().and_then(|id| open_invocation(*id)) {
                    Some(invocation) => row.running(invocation),
                    None => row,
                },
            );
        }
    }
    if response.withheld_by_policy > 0 {
        rows.push(
            ReportRow::new([
                "withheld".to_owned(),
                format!(
                    "{} message(s) were kept out of this digest by policy",
                    response.withheld_by_policy
                ),
            ])
            .toned(ReportTone::Warn),
        );
    }
    rows
}

/// The `:message open <id>` invocation a citing row runs.
///
/// Bang'd: opening a message is what `<enter>` does everywhere else in this
/// client, and there is nothing to confirm about reading mail.
fn open_invocation(message_id: i64) -> Option<command::Invocation> {
    match command::parse(&format!("message open {message_id}!")) {
        Ok(command::Resolution::Invocation(invocation)) => Some(*invocation),
        _ => None,
    }
}

/// `AnalyticsService.GetContactInsight` as a table.
#[must_use]
pub fn contact_rows(response: &GetContactInsightResponse) -> Vec<ReportRow> {
    let volume = response.volume.unwrap_or_default();
    let cadence = response.cadence.unwrap_or_default();
    let decay = response.decay.unwrap_or_default();
    let mut rows = vec![
        ReportRow::new([
            "who".to_owned(),
            if response.name.is_empty() {
                response.address.clone()
            } else {
                format!("{} <{}>", response.name, response.address)
            },
        ]),
        ReportRow::new([
            "volume".to_owned(),
            format!(
                "{} in, {} out, {} thread(s)",
                volume.inbound, volume.outbound, volume.threads
            ),
        ]),
        ReportRow::new([
            "your p50".to_owned(),
            stat(response.ours.as_ref(), |s| s.p50_seconds),
        ]),
        ReportRow::new([
            "their p50".to_owned(),
            stat(response.theirs.as_ref(), |s| s.p50_seconds),
        ]),
        ReportRow::new([
            "cadence".to_owned(),
            format!(
                "{} typical gap · {:.1}/week",
                duration(cadence.median_gap_seconds),
                cadence.messages_per_week
            ),
        ]),
    ];
    rows.push(
        ReportRow::new([
            "awaiting".to_owned(),
            format!(
                "{} reply(s), {} late",
                response.awaiting_reply, response.overdue
            ),
        ])
        .toned(if response.overdue > 0 {
            ReportTone::Warn
        } else {
            ReportTone::Plain
        }),
    );
    // Dormant and declining are the two facts a relationship report exists to
    // surface, and they are the daemon's verdicts rather than a threshold this
    // client re-derives.
    if decay.dormant || decay.declining {
        rows.push(
            ReportRow::new([
                "trend".to_owned(),
                format!(
                    "{} — silent for {}",
                    if decay.dormant {
                        "dormant"
                    } else {
                        "declining"
                    },
                    duration(decay.silence_seconds)
                ),
            ])
            .toned(ReportTone::Warn),
        );
    }
    for topic in &response.topics {
        rows.push(ReportRow::new([
            "topic".to_owned(),
            format!("{} ({})", topic.term, topic.messages),
        ]));
    }
    if !response.briefing.is_empty() {
        for line in response.briefing.lines() {
            rows.push(ReportRow::new(["briefing".to_owned(), line.to_owned()]));
        }
    }
    for action in &response.next_actions {
        rows.push(ReportRow::new(["next".to_owned(), action.clone()]).toned(ReportTone::Ok));
    }
    rows
}

/// A subscription's class, as the report names it.
fn subscription_class(class: i32) -> &'static str {
    match SubscriptionClass::try_from(class) {
        Ok(SubscriptionClass::Newsletter) => "newsletter",
        Ok(SubscriptionClass::Transactional) => "transactional",
        Ok(SubscriptionClass::Automated) => "automated",
        Ok(SubscriptionClass::Personal) => "personal",
        Ok(SubscriptionClass::Unknown) => "unknown",
        Ok(SubscriptionClass::Unspecified) | Err(_) => "unclassified",
    }
}

/// `AnalyticsService.ListSubscriptions` as a table.
#[must_use]
pub fn subscription_rows(response: &ListSubscriptionsResponse) -> Vec<ReportRow> {
    response
        .senders
        .iter()
        .map(|sender| {
            let unsubscribe = match sender.unsubscribe.as_ref() {
                // One-click is the difference between "there is a way out" and
                // "there is a way out that works", so it is said rather than
                // implied.
                Some(link) if link.one_click => "one click".to_owned(),
                Some(link) if !link.http_url.is_empty() => "link".to_owned(),
                Some(link) if !link.mailto.is_empty() => "by mail".to_owned(),
                _ => "none offered".to_owned(),
            };
            ReportRow::new([
                if sender.name.is_empty() {
                    sender.address.clone()
                } else {
                    format!("{} <{}>", sender.name, sender.address)
                },
                subscription_class(sender.sender_class).to_owned(),
                sender.messages.to_string(),
                format!("{:.0}%", sender.read_rate * 100.0),
                unsubscribe,
            ])
            .toned(if sender.candidate {
                // The whole point of the report: mail arriving that nobody reads.
                ReportTone::Warn
            } else {
                ReportTone::Plain
            })
        })
        .collect()
}

/// One `AnalyticsCell` as text.
fn analytics_cell(cell: &AnalyticsCell) -> String {
    use rmail_proto::v1::analytics_cell::Value;
    match cell.value.as_ref() {
        Some(Value::NullValue(_)) | None => String::new(),
        Some(Value::IntegerValue(value)) => value.to_string(),
        Some(Value::RealValue(value)) => format!("{value:.2}"),
        Some(Value::TextValue(value)) => value.clone(),
        // A column type this projection cannot carry. Named rather than blank:
        // an empty cell reads as a null, and "the daemon could not put this on
        // the wire" is a different fact.
        Some(Value::Unsupported(_)) => "(unsupported)".to_owned(),
    }
}

/// `AnalyticsService.AskAnalytics` as a table.
///
/// The generated SQL comes first, because a number nobody can see the query
/// behind is a number nobody can check — which is the whole reason this RPC
/// returns it.
#[must_use]
pub fn ask_analytics_rows(response: &AskAnalyticsResponse) -> Vec<ReportRow> {
    let mut rows = Vec::new();
    for line in response.sql.lines() {
        rows.push(ReportRow::new(["sql".to_owned(), line.to_owned()]).toned(ReportTone::Muted));
    }
    if !response.notes.is_empty() {
        rows.push(ReportRow::new([
            "reading".to_owned(),
            response.notes.clone(),
        ]));
    }
    if !response.columns.is_empty() {
        let mut header = vec![String::new()];
        header.extend(response.columns.iter().cloned());
        rows.push(ReportRow::new(header).toned(ReportTone::Muted));
    }
    for row in &response.rows {
        let mut cells = vec![String::new()];
        cells.extend(row.cells.iter().map(analytics_cell));
        rows.push(ReportRow::new(cells));
    }
    if response.truncated {
        rows.push(
            ReportRow::new([
                "truncated".to_owned(),
                "there are more rows than this answer carries".to_owned(),
            ])
            .toned(ReportTone::Warn),
        );
    }
    for line in response.narrative.lines() {
        rows.push(ReportRow::new(["said".to_owned(), line.to_owned()]));
    }
    rows
}

/// A table cell's text, whatever type the extractor decided it was.
fn table_cell(cell: &TableCell) -> String {
    match CellType::try_from(cell.r#type) {
        Ok(CellType::Number) => format!("{}", cell.number),
        Ok(CellType::Bool) => if cell.boolean { "true" } else { "false" }.to_owned(),
        Ok(CellType::Date) => when(cell.date),
        // Text, empty, or a type this build does not know: the extractor also
        // supplies the raw text, and that is the honest rendering for all three.
        _ => cell.text.clone(),
    }
}

/// `AttachmentService.ExtractTables` as a table of tables.
///
/// One report row per table row, with the table's name in the first column on its
/// first row only — the same shape the digest uses, and for the same reason: a
/// column repeating one word down the screen carries no information.
///
/// A table wider than the report is truncated at draw time rather than reshaped
/// here. `Table::truncated` and `dropped_tables` are said outright instead,
/// because a spreadsheet silently short of three columns is a spreadsheet nobody
/// can trust.
#[must_use]
pub fn table_rows(response: &ExtractTablesResponse) -> Vec<ReportRow> {
    let mut rows = Vec::new();
    for table in &response.tables {
        let name = if table.name.is_empty() {
            "(unnamed)".to_owned()
        } else {
            table.name.clone()
        };
        let mut header = vec![name.clone()];
        header.extend(table.columns.iter().map(|column| column.header.clone()));
        rows.push(ReportRow::new(header).toned(ReportTone::Muted));
        for row in &table.rows {
            let mut cells = vec![String::new()];
            cells.extend(row.cells.iter().map(table_cell));
            rows.push(ReportRow::new(cells));
        }
        if table.truncated {
            rows.push(
                ReportRow::new([
                    String::new(),
                    format!("{name} was truncated — it has more rows than this carries"),
                ])
                .toned(ReportTone::Warn),
            );
        }
        if table.inferred {
            rows.push(
                ReportRow::new([
                    String::new(),
                    format!("{name} was inferred by a model, not parsed"),
                ])
                .toned(ReportTone::Warn),
            );
        }
    }
    if response.dropped_tables > 0 || response.cell_budget_exhausted {
        rows.push(
            ReportRow::new([
                "dropped".to_owned(),
                format!(
                    "{} table(s) did not fit the extraction budget",
                    response.dropped_tables
                ),
            ])
            .toned(ReportTone::Warn),
        );
    }
    rows
}

/// Money as the invoice report draws it, from minor units.
fn money(money: Option<&InvoiceMoney>) -> String {
    match money {
        None => "-".to_owned(),
        Some(money) => {
            // Minor units throughout, and divided only here: an invoice total is
            // compared and summed as an integer everywhere upstream for the reason
            // `rmail_core::ai::budget` gives about floats, and this is the one
            // place it becomes something to read.
            #[allow(clippy::cast_precision_loss)]
            let major = money.minor_units as f64 / 100.0;
            format!("{} {major:.2}", money.currency)
        }
    }
}

/// Where a field came from, so a reader can tell a parse from a guess.
fn origin(provenance: Option<&FieldProvenance>) -> String {
    match provenance.map(|p| FieldOrigin::try_from(p.origin)) {
        Some(Ok(FieldOrigin::Model)) => "model".to_owned(),
        Some(Ok(FieldOrigin::Parsed)) => "parsed".to_owned(),
        _ => String::new(),
    }
}

/// An invoice's payment status, as the report names it.
fn invoice_status(status: i32) -> (&'static str, ReportTone) {
    match InvoicePaymentStatus::try_from(status) {
        Ok(InvoicePaymentStatus::Paid) => ("paid", ReportTone::Ok),
        Ok(InvoicePaymentStatus::Unpaid) => ("unpaid", ReportTone::Plain),
        Ok(InvoicePaymentStatus::Overdue) => ("overdue", ReportTone::Bad),
        Ok(InvoicePaymentStatus::Refunded) => ("refunded", ReportTone::Muted),
        Ok(InvoicePaymentStatus::Void) => ("void", ReportTone::Muted),
        Ok(InvoicePaymentStatus::Unspecified) | Err(_) => ("unknown", ReportTone::Muted),
    }
}

/// `AttachmentService.ExtractInvoice` as a field table.
///
/// Every field carries where it came from, which is the column that matters: a
/// total a parser read out of a PDF's text layer and a total a model inferred from
/// a scan are not the same claim, and an invoice report that flattened them would
/// be inviting somebody to pay the second one.
#[must_use]
pub fn invoice_rows(response: &ExtractInvoiceResponse) -> Vec<ReportRow> {
    let Some(invoice) = response.invoice.as_ref() else {
        let mut rows = vec![ReportRow::new([
            "nothing".to_owned(),
            "no invoice or receipt was found in this message".to_owned(),
        ])
        .toned(ReportTone::Muted)];
        for candidate in &response.candidates {
            rows.push(ReportRow::new([
                "candidate".to_owned(),
                candidate.filename.clone(),
                candidate.part_id.clone(),
            ]));
        }
        return rows;
    };
    let text = |field: Option<&InvoiceText>| {
        field.map_or_else(
            || ("-".to_owned(), String::new()),
            |field| (field.value.clone(), origin(field.provenance.as_ref())),
        )
    };
    let (vendor, vendor_from) = text(invoice.vendor.as_ref());
    let (number, number_from) = text(invoice.number.as_ref());
    let (status, tone) = invoice_status(invoice.status);
    let mut rows = vec![
        ReportRow::new(["vendor".to_owned(), vendor, vendor_from]),
        ReportRow::new(["number".to_owned(), number, number_from]),
        ReportRow::new([
            "total".to_owned(),
            money(invoice.total.as_ref()),
            origin(
                invoice
                    .total
                    .as_ref()
                    .and_then(|total| total.provenance.as_ref()),
            ),
        ]),
        ReportRow::new(["tax".to_owned(), money(invoice.tax.as_ref()), String::new()]),
        ReportRow::new([
            "issued".to_owned(),
            invoice
                .issued_at
                .as_ref()
                .map_or_else(|| "-".to_owned(), |date| when(date.at)),
            String::new(),
        ]),
        ReportRow::new([
            "due".to_owned(),
            invoice
                .due_at
                .as_ref()
                .map_or_else(|| "-".to_owned(), |date| when(date.at)),
            String::new(),
        ]),
        ReportRow::new([
            "status".to_owned(),
            status.to_owned(),
            origin(invoice.status_provenance.as_ref()),
        ])
        .toned(tone),
    ];
    for item in &invoice.line_items {
        rows.push(ReportRow::new([
            "item".to_owned(),
            item.description.clone(),
            money(item.total.as_ref()),
        ]));
    }
    if invoice.inferred {
        rows.push(
            ReportRow::new([
                "inferred".to_owned(),
                "a model read this, and a model can be wrong about a number".to_owned(),
                String::new(),
            ])
            .toned(ReportTone::Warn),
        );
    }
    for warning in &invoice.warnings {
        rows.push(
            ReportRow::new(["warning".to_owned(), warning.clone(), String::new()])
                .toned(ReportTone::Warn),
        );
    }
    rows
}

/// `AttachmentService.ExportInvoices` as a table.
#[must_use]
pub fn invoices_rows(response: &ExportInvoicesResponse) -> Vec<ReportRow> {
    if !response.csv.is_empty() {
        // The CSV framing asked for a document, so the document is what is drawn
        // — one row per line, so it can be read and copied rather than elided
        // into one cell.
        return response
            .csv
            .lines()
            .map(|line| ReportRow::new([line.to_owned()]))
            .collect();
    }
    response
        .invoices
        .iter()
        .map(|invoice| {
            let (status, tone) = invoice_status(invoice.status);
            ReportRow::new([
                invoice
                    .vendor
                    .as_ref()
                    .map_or_else(|| "-".to_owned(), |field| field.value.clone()),
                invoice
                    .number
                    .as_ref()
                    .map_or_else(|| "-".to_owned(), |field| field.value.clone()),
                money(invoice.total.as_ref()),
                invoice
                    .issued_at
                    .as_ref()
                    .map_or_else(|| "-".to_owned(), |date| when(date.at)),
                invoice
                    .due_at
                    .as_ref()
                    .map_or_else(|| "-".to_owned(), |date| when(date.at)),
                status.to_owned(),
            ])
            .toned(tone)
        })
        .collect()
}

/// `SearchService.SearchAttachments` as a table whose rows open the message.
#[must_use]
pub fn attachment_hit_rows(response: &SearchAttachmentsResponse) -> Vec<ReportRow> {
    response
        .hits
        .iter()
        .map(|hit| {
            let row = ReportRow::new([
                hit.filename.clone(),
                hit.from_addr.clone(),
                if hit.subject.is_empty() {
                    NO_SUBJECT.to_owned()
                } else {
                    hit.subject.clone()
                },
                hit.page
                    .map_or_else(|| hit.part_id.clone(), |page| format!("page {page}")),
                hit.excerpt.clone(),
            ]);
            match open_invocation(hit.message_id) {
                Some(invocation) => row.running(invocation),
                None => row,
            }
        })
        .collect()
}

/// `SearchService.SearchEntities` as a table.
#[must_use]
pub fn entity_rows(response: &SearchEntitiesResponse) -> Vec<ReportRow> {
    response
        .hits
        .iter()
        .map(|hit| {
            ReportRow::new([
                hit.kind.clone(),
                hit.value.clone(),
                hit.mentions.to_string(),
                hit.messages.to_string(),
                when(hit.last_seen),
            ])
        })
        .collect()
}

/// `SearchService.CompileQuery` as a table.
///
/// The compiled query and its filters are the answer: this verb exists so a plan
/// can be read *before* it runs, and a plan whose filters were folded into one
/// cell would be a plan nobody could check.
#[must_use]
pub fn query_plan_rows(plan: &QueryPlan) -> Vec<ReportRow> {
    let mut rows = vec![
        ReportRow::new(["asked".to_owned(), plan.raw.clone()]),
        ReportRow::new(["compiled".to_owned(), plan.compiled.clone()]).toned(ReportTone::Ok),
    ];
    for filter in &plan.filters {
        rows.push(ReportRow::new(["filter".to_owned(), filter.clone()]));
    }
    if !plan.semantic_query.is_empty() {
        rows.push(ReportRow::new([
            "semantic".to_owned(),
            plan.semantic_query.clone(),
        ]));
    }
    if !plan.notes.is_empty() {
        rows.push(ReportRow::new(["reading".to_owned(), plan.notes.clone()]));
    }
    rows.push(
        ReportRow::new([
            "from".to_owned(),
            if plan.cached {
                format!("a cached compilation ({})", plan.model)
            } else {
                format!("a fresh model call ({})", plan.model)
            },
        ])
        .toned(if plan.cached {
            ReportTone::Muted
        } else {
            ReportTone::Plain
        }),
    );
    rows
}

/// `SearchService.Evaluate` as a table.
#[must_use]
pub fn eval_rows(report: &EvalReport) -> Vec<ReportRow> {
    let metrics = |metrics: Option<&EvalMetrics>| {
        metrics.map_or_else(
            || {
                [
                    "-".to_owned(),
                    "-".to_owned(),
                    "-".to_owned(),
                    "-".to_owned(),
                ]
            },
            |m| {
                [
                    format!("{:.3}", m.ndcg_at_10),
                    format!("{:.3}", m.mrr),
                    format!("{:.3}", m.recall_at_50),
                    format!("{:.3}", m.p_at_3),
                ]
            },
        )
    };
    let aggregate = metrics(report.aggregate.as_ref());
    let mut rows = vec![ReportRow::new([
        "— all queries —".to_owned(),
        aggregate[0].clone(),
        aggregate[1].clone(),
        aggregate[2].clone(),
        aggregate[3].clone(),
        format!("corpus {}", report.corpus),
    ])
    .toned(ReportTone::Ok)];
    for query in &report.per_query {
        let m = metrics(query.metrics.as_ref());
        rows.push(
            ReportRow::new([
                query.name.clone(),
                m[0].clone(),
                m[1].clone(),
                m[2].clone(),
                m[3].clone(),
                if query.unresolved.is_empty() {
                    format!("{}/{} relevant", query.relevant, query.returned)
                } else {
                    // A judgment naming a message that is not in the index makes
                    // every metric for that query a lower bound rather than a
                    // measurement, so it is not a footnote.
                    format!("{} unresolved judgment(s)", query.unresolved.len())
                },
            ])
            .toned(if query.unresolved.is_empty() {
                ReportTone::Plain
            } else {
                ReportTone::Warn
            }),
        );
    }
    rows
}

/// Where an extracted item came from.
fn extraction_source(source: i32) -> &'static str {
    match ExtractionSource::try_from(source) {
        Ok(ExtractionSource::Ics) => "ics",
        Ok(ExtractionSource::Model) => "model",
        Ok(ExtractionSource::Unspecified) | Err(_) => "?",
    }
}

/// `ExtractService.ExtractEvents` as a table.
#[must_use]
pub fn event_rows(response: &ExtractEventsResponse) -> Vec<ReportRow> {
    let mut rows: Vec<ReportRow> = response
        .events
        .iter()
        .map(|event| {
            ReportRow::new([
                event.summary.clone(),
                if event.all_day {
                    format!("{} (all day)", when(event.starts_at))
                } else {
                    when(event.starts_at)
                },
                event.location.clone(),
                extraction_source(event.source).to_owned(),
            ])
            // A cancellation is the one row somebody must not skim past: it means
            // a meeting they may still have on a calendar is off.
            .toned(if event.cancelled {
                ReportTone::Bad
            } else if ExtractionSource::try_from(event.source) == Ok(ExtractionSource::Model) {
                // Inferred from prose rather than read out of an `.ics` part, so
                // the time may be wrong in a way a real invitation's cannot be.
                ReportTone::Warn
            } else {
                ReportTone::Plain
            })
        })
        .collect();
    rows.extend(delivery_note(
        response.skipped,
        response.delivered,
        response.already_delivered,
        &response.sink_output,
    ));
    rows
}

/// `ExtractService.ExtractTasks` as a table.
#[must_use]
pub fn task_rows(response: &ExtractTasksResponse) -> Vec<ReportRow> {
    let mut rows: Vec<ReportRow> = response
        .tasks
        .iter()
        .map(|task| {
            ReportRow::new([
                task.summary.clone(),
                when(task.due_at),
                task.priority.to_string(),
                extraction_source(task.source).to_owned(),
            ])
            .toned(if task.completed {
                ReportTone::Muted
            } else if ExtractionSource::try_from(task.source) == Ok(ExtractionSource::Model) {
                ReportTone::Warn
            } else {
                ReportTone::Plain
            })
        })
        .collect();
    rows.extend(delivery_note(
        response.skipped,
        response.delivered,
        response.already_delivered,
        &response.sink_output,
    ));
    rows
}

/// What an extraction's sink did, when there is anything to say.
///
/// `already_delivered` is the idempotency claim working — a second call over the
/// same message delivers nothing — and saying so is what stops it reading as a
/// failure. Drawn only when non-zero, so an ordinary extraction has no noise
/// under it.
fn delivery_note(
    skipped: u32,
    delivered: u32,
    already_delivered: u32,
    sink_output: &str,
) -> Vec<ReportRow> {
    let mut rows = Vec::new();
    if skipped > 0 {
        rows.push(
            ReportRow::new([
                "skipped".to_owned(),
                format!("{skipped} item(s) the extractor would not vouch for"),
            ])
            .toned(ReportTone::Muted),
        );
    }
    if delivered > 0 || already_delivered > 0 {
        rows.push(
            ReportRow::new([
                "delivered".to_owned(),
                format!("{delivered} sent, {already_delivered} already claimed"),
            ])
            .toned(ReportTone::Ok),
        );
    }
    for line in sink_output.lines() {
        rows.push(ReportRow::new(["sink".to_owned(), line.to_owned()]));
    }
    rows
}

/// `ExtractService.ExtractStructured` as a table.
///
/// The document is drawn a line at a time rather than as one cell: it is JSON
/// somebody is reading against a schema, and folded into a cell it would be
/// elided at the column width.
#[must_use]
pub fn structured_rows(response: &ExtractStructuredResponse) -> Vec<ReportRow> {
    let mut rows = vec![
        ReportRow::new(["schema".to_owned(), response.schema.clone()]),
        ReportRow::new([
            "from".to_owned(),
            if response.cached {
                format!("a cached extraction ({})", response.model)
            } else {
                format!("a fresh model call ({})", response.model)
            },
        ])
        .toned(if response.cached {
            ReportTone::Muted
        } else {
            ReportTone::Plain
        }),
    ];
    for line in response.data.lines() {
        rows.push(ReportRow::new(["data".to_owned(), line.to_owned()]));
    }
    rows
}

/// What kind of link the classifier decided this is.
fn link_kind(kind: i32) -> &'static str {
    match LinkKind::try_from(kind) {
        Ok(LinkKind::Unsubscribe) => "unsubscribe",
        Ok(LinkKind::Tracking) => "tracking",
        Ok(LinkKind::Meeting) => "meeting",
        Ok(LinkKind::Document) => "document",
        Ok(LinkKind::Cta) => "call to action",
        Ok(LinkKind::Other) => "other",
        Ok(LinkKind::Unspecified) | Err(_) => "?",
    }
}

/// `LinkService.ExtractLinks` as a table.
///
/// A deceptive link — one whose visible text names a different host from the one
/// it goes to — is the reason this verb exists, so it is drawn `Bad` and its
/// reason is on the row. Tracking pixels are counted rather than listed: there
/// are frequently dozens and none of them is individually interesting.
#[must_use]
pub fn link_rows(response: &ExtractLinksResponse) -> Vec<ReportRow> {
    let mut rows: Vec<ReportRow> = response
        .links
        .iter()
        .map(|link| {
            ReportRow::new([
                link_kind(link.kind).to_owned(),
                link.host.clone(),
                if link.display_text.is_empty() {
                    link.display_host.clone()
                } else {
                    link.display_text.clone()
                },
                link.reason.clone(),
            ])
            .toned(if link.deceptive {
                ReportTone::Bad
            } else if LinkKind::try_from(link.kind) == Ok(LinkKind::Tracking) {
                ReportTone::Muted
            } else {
                ReportTone::Plain
            })
        })
        .collect();
    if response.tracking_pixels > 0 {
        rows.push(
            ReportRow::new([
                "pixels".to_owned(),
                format!(
                    "{} tracking pixel(s) — images that report when you opened this",
                    response.tracking_pixels
                ),
                String::new(),
                String::new(),
            ])
            .toned(ReportTone::Warn),
        );
    }
    if response.truncated > 0 {
        rows.push(
            ReportRow::new([
                "truncated".to_owned(),
                format!("{} more link(s) than this carries", response.truncated),
                String::new(),
                String::new(),
            ])
            .toned(ReportTone::Muted),
        );
    }
    rows
}

/// A note's author, as the listing names it.
fn note_author(author: i32) -> &'static str {
    match NoteAuthor::try_from(author) {
        Ok(NoteAuthor::User) => "you",
        Ok(NoteAuthor::Ai) => "ai",
        Ok(NoteAuthor::Unspecified) | Err(_) => "?",
    }
}

/// One note as a row.
///
/// Carries `:note edit <id>` — pressing `<enter>` on a note is how it is
/// rewritten, and the row is where the id already is. Not bang'd: `note edit`
/// needs text after the id, so the row's line would be refused; what the row
/// carries is a *prefilled* invocation nothing can run silently.
#[must_use]
pub fn note_row(note: &Note) -> ReportRow {
    // Multi-line notes are folded to one line for the row, which is the same
    // rule every other remote string here follows. The body is markdown and the
    // whole of it is on the wire; a reader who needs it all opens the note.
    ReportRow::new([
        note.id.to_string(),
        note_author(note.author).to_owned(),
        when(note.created_at),
        note.body_md.clone(),
    ])
    .toned(if NoteAuthor::try_from(note.author) == Ok(NoteAuthor::Ai) {
        // An AI-written note is a different claim from one the user wrote, and a
        // listing that drew them identically would be inviting somebody to treat
        // a summary as a decision they made.
        ReportTone::Muted
    } else {
        ReportTone::Plain
    })
}

/// `NoteService.ListNotes` as a table.
#[must_use]
pub fn note_rows(response: &ListNotesResponse) -> Vec<ReportRow> {
    response.notes.iter().map(note_row).collect()
}

/// One `NoteEvent` as a row of the live listing.
///
/// A deletion is a row rather than a removal: the pane appends, and rewriting
/// history under a reader who is looking at it is worse than saying what changed.
#[must_use]
pub fn note_event_row(event: &NoteEvent) -> Option<ReportRow> {
    use rmail_proto::v1::note_event::Event;
    match event.event.as_ref()? {
        Event::Added(note) => Some(note_row(note)),
        Event::Edited(note) => Some(note_row(note).toned(ReportTone::Ok)),
        Event::Deleted(deleted) => Some(
            ReportRow::new([
                deleted.id.to_string(),
                String::new(),
                String::new(),
                "deleted".to_owned(),
            ])
            .toned(ReportTone::Bad),
        ),
    }
}

/// `SavedSearchService.ListSavedSearches` as a table.
///
/// Every row carries `:saved run <name>`, which is what makes the listing the way
/// one is run. Bang'd: running a saved search is a read, and there is nothing to
/// confirm about searching.
#[must_use]
pub fn saved_rows(response: &ListSavedSearchesResponse) -> Vec<ReportRow> {
    response
        .searches
        .iter()
        .map(|saved| {
            let row = ReportRow::new([
                saved.name.clone(),
                saved.query.clone(),
                when(saved.last_run_at),
            ]);
            match run_saved_invocation(&saved.name) {
                Some(invocation) => row.running(invocation),
                None => row,
            }
        })
        .collect()
}

/// The `:saved run <name>` invocation a listing row runs.
fn run_saved_invocation(name: &str) -> Option<command::Invocation> {
    match command::parse(&format!("saved run {}!", command::quoted(name))) {
        Ok(command::Resolution::Invocation(invocation)) => Some(*invocation),
        _ => None,
    }
}

/// One saved search, echoed back after being stored.
#[must_use]
pub fn saved_stored(saved: &SavedSearch) -> String {
    format!(
        "{} saved — :saved run {} searches it",
        saved.name, saved.name
    )
}

/// `SavedSearchService.ListSmartFolders` as a table.
///
/// Every row carries `:folder members <name>`, because "what is in it" is the
/// question a folder listing raises.
#[must_use]
pub fn smart_folder_rows(response: &ListSmartFoldersResponse) -> Vec<ReportRow> {
    response
        .folders
        .iter()
        .map(|folder| {
            let row = ReportRow::new([
                folder.name.clone(),
                folder.predicate.clone(),
                if folder.auto_tag.is_empty() {
                    "-".to_owned()
                } else {
                    folder.auto_tag.clone()
                },
                when(folder.last_evaluated_at),
            ])
            .toned(if folder.auto_tag.is_empty() {
                ReportTone::Plain
            } else {
                // A folder that tags what enters it changes mail on its own, which
                // is the one thing about a folder listing worth spotting.
                ReportTone::Warn
            });
            match members_invocation(&folder.name) {
                Some(invocation) => row.running(invocation),
                None => row,
            }
        })
        .collect()
}

/// The `:folder members <name>` invocation a listing row runs.
fn members_invocation(name: &str) -> Option<command::Invocation> {
    match command::parse(&format!("folder members {}!", command::quoted(name))) {
        Ok(command::Resolution::Invocation(invocation)) => Some(*invocation),
        _ => None,
    }
}

/// One smart folder as a field table — what `:folder new` and `:folder compile`
/// answer with.
#[must_use]
pub fn smart_folder_fields(folder: &SmartFolder, plan: Option<&QueryPlan>) -> Vec<ReportRow> {
    let mut rows = vec![
        ReportRow::new(["name".to_owned(), folder.name.clone()]),
        ReportRow::new(["predicate".to_owned(), folder.predicate.clone()]).toned(ReportTone::Ok),
    ];
    if !folder.nl_source.is_empty() {
        rows.push(ReportRow::new([
            "compiled from".to_owned(),
            folder.nl_source.clone(),
        ]));
    }
    if !folder.auto_tag.is_empty() {
        rows.push(
            ReportRow::new(["auto-tag".to_owned(), folder.auto_tag.clone()])
                .toned(ReportTone::Warn),
        );
    }
    rows.push(ReportRow::new([
        "notify".to_owned(),
        if folder.notify { "yes" } else { "no" }.to_owned(),
    ]));
    if let Some(plan) = plan {
        for filter in &plan.filters {
            rows.push(ReportRow::new(["filter".to_owned(), filter.clone()]));
        }
        if !plan.semantic_query.is_empty() {
            rows.push(ReportRow::new([
                "semantic".to_owned(),
                plan.semantic_query.clone(),
            ]));
        }
    }
    rows
}

/// `SavedSearchService.EvaluateSmartFolder` as a table.
#[must_use]
pub fn evaluation_rows(evaluation: &SmartFolderEvaluation) -> Vec<ReportRow> {
    vec![
        ReportRow::new(["members".to_owned(), evaluation.members.to_string()])
            .toned(ReportTone::Ok),
        ReportRow::new(["entered".to_owned(), evaluation.entered_count.to_string()]),
        ReportRow::new(["departed".to_owned(), evaluation.departed_count.to_string()]),
        ReportRow::new(["tagged".to_owned(), evaluation.tagged.to_string()]).toned(
            if evaluation.tagged > 0 {
                // This is the row that says mail was changed.
                ReportTone::Warn
            } else {
                ReportTone::Plain
            },
        ),
        ReportRow::new(["notified".to_owned(), evaluation.notified.to_string()]),
    ]
}

/// A `SearchHit` as a row of a saved search's results.
#[must_use]
pub fn saved_hit_row(hit: &SearchHit) -> ReportRow {
    let message = hit.message.clone().unwrap_or_default();
    let row = ReportRow::new([
        format!("{:.3}", hit.score),
        message
            .from_name
            .clone()
            .unwrap_or_else(|| message.from_addr.clone().unwrap_or_else(|| "-".to_owned())),
        message
            .subject
            .clone()
            .unwrap_or_else(|| NO_SUBJECT.to_owned()),
        when(message.date.unwrap_or(0)),
    ]);
    match open_invocation(message.id) {
        Some(invocation) => row.running(invocation),
        None => row,
    }
}

/// A `Message` as a row of a smart folder's membership.
#[must_use]
pub fn member_row(message: &ProtoMessage) -> ReportRow {
    let row = ReportRow::new([
        message.id.to_string(),
        message
            .from_name
            .clone()
            .or_else(|| message.from_addr.clone())
            .unwrap_or_else(|| "-".to_owned()),
        message
            .subject
            .clone()
            .unwrap_or_else(|| NO_SUBJECT.to_owned()),
        when(message.date.unwrap_or(0)),
    ]);
    match open_invocation(message.id) {
        Some(invocation) => row.running(invocation),
        None => row,
    }
}
