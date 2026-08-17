//! `mail contact`, `mail subs` and `mail ask` — the three `AnalyticsService`
//! reports task 72 adds (prd.md features 59, 60 and 61).
//!
//! `mail contact` and `mail subs` are prd.md's own spelling. Its third,
//! `mail ask "…"`, was already taken by feature 43 — `AiService/AskMailbox`
//! answers a question about the *contents* of messages, and two verbs spelled
//! the same reaching different services would be worse than either name. So
//! the natural-language analytics question is `mail stats ask`, which is the
//! namespace `stats_cli`'s own module docs created for exactly these three
//! reports.
//!
//! # What costs money, and what does not
//!
//! - `mail contact <addr>` is arithmetic. `--insight` adds one model call.
//! - `mail subs` is headers and behaviour. `--classify` adds one model call
//!   for the senders neither could classify.
//! - `mail ask` is one model call for the SQL, plus a second for the
//!   narrative unless `--json` (which suppresses it, since a pipeline has no
//!   use for a paragraph).
//!
//! # `mail subs` will not unsubscribe you
//!
//! There is deliberately no `--unsubscribe` flag. What the report shows is
//! what the *sender* says its unsubscribe method is, and that header is
//! attacker-authored: the daemon does not fetch it, and neither does this.
//! The URL is printed so a human can look at it and decide.
//!
//! # `--since` takes a duration, not a timestamp
//!
//! As everywhere else in this CLI. The daemon still receives absolute bounds,
//! for the reason `stats_cli` gives: a report has to name the window it
//! summarized, and a relative one would mean something different by the time
//! it was rendered.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use clap::Args;
use rmail_proto::v1::analytics_cell::Value as CellValue;
use rmail_proto::v1::analytics_service_client::AnalyticsServiceClient;
use rmail_proto::v1::{
    AskAnalyticsRequest, AskAnalyticsResponse, GetContactInsightRequest, GetContactInsightResponse,
    ListSubscriptionsRequest, ListSubscriptionsResponse, SubscriptionClass, SubscriptionSender,
    SubscriptionSource,
};
use serde::Serialize;

/// `mail contact <address> [flags]`.
#[derive(Debug, Args)]
pub struct ContactArgs {
    /// The correspondent's email address.
    address: String,
    /// Also ask Claude for a one-paragraph relationship briefing and next
    /// actions. Costs one model call.
    #[arg(long)]
    insight: bool,
    /// Restrict to one account.
    #[arg(long)]
    account: Option<i64>,
    /// How far back to look, e.g. `90d`, `52w`. Default: one year.
    #[arg(long)]
    since: Option<String>,
    /// End the window here instead of now, as unix seconds.
    #[arg(long)]
    until: Option<i64>,
    /// How many recurring subject terms to show. Default 8, max 50.
    #[arg(long)]
    topics: Option<u32>,
    /// One JSON document instead of the rendered report.
    #[arg(long)]
    json: bool,
}

/// `mail subs [flags]`.
#[derive(Debug, Args)]
pub struct SubsArgs {
    /// Restrict to one account.
    #[arg(long)]
    account: Option<i64>,
    /// How far back to look, e.g. `90d`, `26w`. Default: 180 days.
    #[arg(long)]
    since: Option<String>,
    /// End the window here instead of now, as unix seconds.
    #[arg(long)]
    until: Option<i64>,
    /// Maximum senders. Default 50, max 500.
    #[arg(long)]
    limit: Option<u32>,
    /// Only senders worth leaving.
    #[arg(long)]
    candidates: bool,
    /// Ask Claude about senders the headers and behaviour could not classify.
    /// Costs one model call.
    #[arg(long)]
    classify: bool,
    /// One JSON document instead of the rendered table.
    #[arg(long)]
    json: bool,
}

/// `mail ask "<question>" [flags]`.
#[derive(Debug, Args)]
pub struct AskArgs {
    /// The question, in plain English.
    question: String,
    /// Restrict to one account. Required when several are configured, because
    /// the call has to be charged to one AI budget.
    #[arg(long)]
    account: Option<i64>,
    /// One JSON document instead of the rendered table. Implies no narrative:
    /// a pipeline has no use for a paragraph, and it would be a second model
    /// call.
    #[arg(long)]
    json: bool,
    /// Print the SQL that ran and the parameters it was given.
    #[arg(long)]
    explain: bool,
}

/// Run `mail contact`.
///
/// # Errors
///
/// Anything that stops the command completing: an unparseable duration, no
/// daemon, a failed RPC, an unwritable stdout.
pub async fn contact(socket: &Path, args: ContactArgs) -> Result<()> {
    let since = args
        .since
        .as_deref()
        .map(parse_duration)
        .transpose()
        .context("--since")?;
    let since_abs = since.map_or(0, |seconds| now().saturating_sub(seconds));

    let channel = rmail_core::connect_uds(socket)
        .await
        .with_context(|| format!("connecting to rmaild at {}", socket.display()))?;
    let mut client = AnalyticsServiceClient::new(channel);
    let insight = client
        .get_contact_insight(GetContactInsightRequest {
            account_id: args.account.unwrap_or(0),
            address: args.address.clone(),
            since: since_abs,
            until: args.until.unwrap_or(0),
            topic_limit: args.topics.unwrap_or(0),
            // The flag is `--insight` ("also brief it"), the field is
            // `metrics_only` ("do not"). Inverted here rather than in the
            // proto because proto3 has no field presence for a bool, so the
            // zero value has to be the mode that costs nothing.
            metrics_only: !args.insight,
        })
        .await
        .context("GetContactInsight RPC failed")?
        .into_inner();

    let mut out = std::io::stdout().lock();
    if args.json {
        let line = serde_json::to_string(&contact_json(&insight))?;
        writeln!(out, "{line}")?;
        return Ok(());
    }
    print_contact(&mut out, &insight)
}

/// Run `mail subs`.
///
/// # Errors
///
/// As [`contact`].
pub async fn subs(socket: &Path, args: SubsArgs) -> Result<()> {
    let since = args
        .since
        .as_deref()
        .map(parse_duration)
        .transpose()
        .context("--since")?;
    let since_abs = since.map_or(0, |seconds| now().saturating_sub(seconds));

    let channel = rmail_core::connect_uds(socket)
        .await
        .with_context(|| format!("connecting to rmaild at {}", socket.display()))?;
    let mut client = AnalyticsServiceClient::new(channel);
    let report = client
        .list_subscriptions(ListSubscriptionsRequest {
            account_id: args.account.unwrap_or(0),
            since: since_abs,
            until: args.until.unwrap_or(0),
            limit: args.limit.unwrap_or(0),
            candidates_only: args.candidates,
            classify_unknown: args.classify,
        })
        .await
        .context("ListSubscriptions RPC failed")?
        .into_inner();

    let mut out = std::io::stdout().lock();
    if args.json {
        let line = serde_json::to_string(&subs_json(&report))?;
        writeln!(out, "{line}")?;
        return Ok(());
    }
    print_subs(&mut out, &report)
}

/// Run `mail ask`.
///
/// # Errors
///
/// As [`contact`].
pub async fn ask(socket: &Path, args: AskArgs) -> Result<()> {
    let channel = rmail_core::connect_uds(socket)
        .await
        .with_context(|| format!("connecting to rmaild at {}", socket.display()))?;
    let mut client = AnalyticsServiceClient::new(channel);
    let answer = client
        .ask_analytics(AskAnalyticsRequest {
            account_id: args.account.unwrap_or(0),
            question: args.question.clone(),
            narrate: !args.json,
        })
        .await
        .context("AskAnalytics RPC failed")?
        .into_inner();

    let mut out = std::io::stdout().lock();
    if args.json {
        let line = serde_json::to_string(&ask_json(&answer))?;
        writeln!(out, "{line}")?;
        return Ok(());
    }
    print_answer(&mut out, &answer, args.explain)
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn print_contact(out: &mut impl Write, insight: &GetContactInsightResponse) -> Result<()> {
    // Every string below is either mail text (a display name, a subject term,
    // an address) or model prose written from it, and it is about to be
    // written to a terminal. `terminal_safe` drops the two families that make
    // that dangerous — bidi overrides that reorder a line, and C0/C1 runs that
    // repaint the screen — using the same definition the TUI and `mail ask`
    // already share. See `main::terminal_safe`.
    let label = if insight.name.is_empty() {
        safe(&insight.address)
    } else {
        format!("{} <{}>", safe(&insight.name), safe(&insight.address))
    };
    writeln!(out, "{label}")?;
    writeln!(
        out,
        "  window        {} .. {}",
        insight.since, insight.until
    )?;
    if let Some(volume) = &insight.volume {
        writeln!(
            out,
            "  volume        {} in / {} out over {} threads ({}% written by you)",
            volume.inbound,
            volume.outbound,
            volume.threads,
            (volume.direction_ratio * 100.0).round() as i64
        )?;
    }
    if let Some(ours) = &insight.ours {
        if ours.samples > 0 {
            writeln!(
                out,
                "  you reply     p50 {}  p90 {}  ({} replies)",
                duration(ours.p50_seconds),
                duration(ours.p90_seconds),
                ours.samples
            )?;
        }
    }
    if let Some(theirs) = &insight.theirs {
        if theirs.samples > 0 {
            writeln!(
                out,
                "  they reply    p50 {}  p90 {}  ({} replies)",
                duration(theirs.p50_seconds),
                duration(theirs.p90_seconds),
                theirs.samples
            )?;
        }
    }
    if insight.symmetry > 0.0 {
        let verdict = if insight.symmetry >= 1.0 {
            "you are the faster side"
        } else {
            "they are the faster side"
        };
        writeln!(out, "  symmetry      {:.2}  ({verdict})", insight.symmetry)?;
    }
    writeln!(
        out,
        "  waiting       {} unanswered, {} overdue",
        insight.awaiting_reply, insight.overdue
    )?;
    if let Some(cadence) = &insight.cadence {
        writeln!(
            out,
            "  cadence       {:.1}/week, median gap {}",
            cadence.messages_per_week,
            duration(cadence.median_gap_seconds)
        )?;
    }
    if let Some(decay) = &insight.decay {
        let mut flags: Vec<&str> = Vec::new();
        if decay.dormant {
            flags.push("dormant");
        }
        if decay.declining {
            flags.push("declining");
        }
        writeln!(
            out,
            "  decay         silent for {}, {} recent vs {} earlier{}",
            duration(decay.silence_seconds),
            decay.recent_messages,
            decay.prior_messages,
            if flags.is_empty() {
                String::new()
            } else {
                format!("  [{}]", flags.join(", "))
            }
        )?;
    }
    if !insight.topics.is_empty() {
        let terms: Vec<String> = insight
            .topics
            .iter()
            .map(|topic| format!("{} ({})", safe(&topic.term), topic.messages))
            .collect();
        writeln!(out, "  topics        {}", terms.join(", "))?;
    }
    if !insight.briefing.is_empty() {
        writeln!(out)?;
        writeln!(out, "{}", safe(&insight.briefing))?;
    }
    if !insight.next_actions.is_empty() {
        writeln!(out)?;
        for action in &insight.next_actions {
            writeln!(out, "  - {}", safe(action))?;
        }
    }
    if insight.accounts.len() > 1 {
        writeln!(
            out,
            "\nnote: this correspondence spans {} accounts; pass --account to brief one",
            insight.accounts.len()
        )?;
    }
    Ok(())
}

fn print_subs(out: &mut impl Write, report: &ListSubscriptionsResponse) -> Result<()> {
    writeln!(
        out,
        "{} senders  {} .. {}  ({} shown, {} header probes)",
        report.total_senders,
        report.since,
        report.until,
        report.senders.len(),
        report.headers_read
    )?;
    if report.model_classified > 0 {
        writeln!(
            out,
            "{} classified by {}",
            report.model_classified, report.model
        )?;
    }
    for sender in &report.senders {
        writeln!(
            out,
            "{}{}  {}",
            if sender.candidate { "* " } else { "  " },
            safe(&sender.address),
            class_label(sender.sender_class)
        )?;
        writeln!(
            out,
            "      {} messages, {}% read, {} replies from you  [{}]",
            sender.messages,
            (sender.read_rate * 100.0).round() as i64,
            sender.your_replies,
            source_label(sender.source)
        )?;
        if !sender.signals.is_empty() {
            let signals: Vec<String> = sender.signals.iter().map(|s| safe(s)).collect();
            writeln!(out, "      signals: {}", signals.join(", "))?;
        }
        if let Some(unsubscribe) = &sender.unsubscribe {
            let one_click = if unsubscribe.one_click {
                " (one-click)"
            } else {
                ""
            };
            if !unsubscribe.http_url.is_empty() {
                writeln!(
                    out,
                    "      unsubscribe: {}{one_click}",
                    safe(&unsubscribe.http_url)
                )?;
            }
            if !unsubscribe.mailto.is_empty() {
                writeln!(out, "      unsubscribe: mail {}", safe(&unsubscribe.mailto))?;
            }
        }
    }
    if report.senders.iter().any(|s| s.unsubscribe.is_some()) {
        writeln!(
            out,
            "\nrmail does not act on these: the address above is what the sender's own \
             List-Unsubscribe header says, and it is the sender's text. Open it yourself."
        )?;
    }
    Ok(())
}

fn print_answer(out: &mut impl Write, answer: &AskAnalyticsResponse, explain: bool) -> Result<()> {
    if !answer.notes.is_empty() {
        writeln!(out, "{}", safe(&answer.notes))?;
        writeln!(out)?;
    }
    if explain {
        writeln!(out, "sql: {}", safe(&answer.sql))?;
        if !answer.params.is_empty() {
            let params: Vec<String> = answer.params.iter().map(|p| safe(p)).collect();
            writeln!(out, "params: {}", params.join(", "))?;
        }
        writeln!(out)?;
    }
    if answer.rows.is_empty() {
        writeln!(out, "(no rows)")?;
    } else {
        // Column-aligned: a result set with no alignment is a wall of text,
        // and every cell here is already bounded in length by the daemon.
        let mut table: Vec<Vec<String>> = vec![answer.columns.iter().map(|c| safe(c)).collect()];
        for row in &answer.rows {
            table.push(
                row.cells
                    .iter()
                    .map(|cell| safe(&render_cell(cell)))
                    .collect(),
            );
        }
        let widths = column_widths(&table);
        for line in &table {
            let rendered: Vec<String> = line
                .iter()
                .enumerate()
                .map(|(index, cell)| {
                    let width = widths.get(index).copied().unwrap_or(0);
                    format!("{cell:<width$}")
                })
                .collect();
            writeln!(out, "{}", rendered.join("  ").trim_end())?;
        }
    }
    if answer.truncated {
        writeln!(out, "\n(more rows existed than the row cap allows)")?;
    }
    if !answer.narrative.is_empty() {
        writeln!(out)?;
        writeln!(out, "{}", safe(&answer.narrative))?;
    }
    Ok(())
}

/// The widest cell per column, so the table lines up.
fn column_widths(table: &[Vec<String>]) -> Vec<usize> {
    let mut widths: Vec<usize> = Vec::new();
    for row in table {
        for (index, cell) in row.iter().enumerate() {
            let width = cell.chars().count();
            match widths.get_mut(index) {
                Some(existing) => *existing = (*existing).max(width),
                None => widths.push(width),
            }
        }
    }
    widths
}

fn render_cell(cell: &rmail_proto::v1::AnalyticsCell) -> String {
    match &cell.value {
        Some(CellValue::NullValue(_)) | None => String::new(),
        Some(CellValue::IntegerValue(v)) => v.to_string(),
        Some(CellValue::RealValue(v)) => format!("{v}"),
        Some(CellValue::TextValue(v)) => v.clone(),
        Some(CellValue::Unsupported(_)) => "<binary>".to_owned(),
    }
}

fn class_label(value: i32) -> &'static str {
    match SubscriptionClass::try_from(value) {
        Ok(SubscriptionClass::Newsletter) => "newsletter",
        Ok(SubscriptionClass::Transactional) => "transactional",
        Ok(SubscriptionClass::Automated) => "automated",
        Ok(SubscriptionClass::Personal) => "personal",
        _ => "unknown",
    }
}

fn source_label(value: i32) -> &'static str {
    match SubscriptionSource::try_from(value) {
        Ok(SubscriptionSource::Header) => "header",
        Ok(SubscriptionSource::Heuristic) => "heuristic",
        Ok(SubscriptionSource::Model) => "model",
        _ => "unspecified",
    }
}

/// Fold a string into something a terminal can be trusted to render.
///
/// One definition, shared with `mail ask`, `mail search` and the TUI: bidi
/// overrides and invisibles (which reorder or hide a line without corrupting
/// it) and C0/C1 control runs (which drive the terminal itself) are dropped.
/// Everything printed by this module is either mail text or model prose
/// written from mail text, so all of it goes through here.
fn safe(text: &str) -> String {
    crate::terminal_safe(text)
}

/// Seconds as the coarsest unit that still reads honestly.
fn duration(seconds: i64) -> String {
    if seconds <= 0 {
        return "-".to_owned();
    }
    if seconds < 90 {
        return format!("{seconds}s");
    }
    if seconds < 5_400 {
        return format!("{}m", seconds / 60);
    }
    if seconds < 172_800 {
        return format!("{}h", seconds / 3_600);
    }
    format!("{}d", seconds / 86_400)
}

// ---------------------------------------------------------------------------
// JSON
// ---------------------------------------------------------------------------

/// Hand-written rather than derived from the wire types, for the reason
/// `search_cli`'s module docs give: a proto field rename must not silently
/// reshape a documented CLI contract.
#[derive(Debug, Serialize)]
struct ContactJson {
    address: String,
    name: String,
    since: i64,
    until: i64,
    inbound: u64,
    outbound: u64,
    threads: u64,
    direction_ratio: f64,
    symmetry: f64,
    awaiting_reply: u64,
    overdue: u64,
    messages_per_week: f64,
    median_gap_seconds: i64,
    silence_seconds: i64,
    dormant: bool,
    declining: bool,
    topics: Vec<TopicJson>,
    briefing: String,
    next_actions: Vec<String>,
    model: String,
    accounts: Vec<i64>,
}

#[derive(Debug, Serialize)]
struct TopicJson {
    term: String,
    messages: u64,
}

fn contact_json(insight: &GetContactInsightResponse) -> ContactJson {
    let volume = insight.volume.unwrap_or_default();
    let cadence = insight.cadence.unwrap_or_default();
    let decay = insight.decay.unwrap_or_default();
    ContactJson {
        address: insight.address.clone(),
        name: insight.name.clone(),
        since: insight.since,
        until: insight.until,
        inbound: volume.inbound,
        outbound: volume.outbound,
        threads: volume.threads,
        direction_ratio: volume.direction_ratio,
        symmetry: insight.symmetry,
        awaiting_reply: insight.awaiting_reply,
        overdue: insight.overdue,
        messages_per_week: cadence.messages_per_week,
        median_gap_seconds: cadence.median_gap_seconds,
        silence_seconds: decay.silence_seconds,
        dormant: decay.dormant,
        declining: decay.declining,
        topics: insight
            .topics
            .iter()
            .map(|topic| TopicJson {
                term: topic.term.clone(),
                messages: topic.messages,
            })
            .collect(),
        briefing: insight.briefing.clone(),
        next_actions: insight.next_actions.clone(),
        model: insight.model.clone(),
        accounts: insight.accounts.clone(),
    }
}

#[derive(Debug, Serialize)]
struct SubsJson {
    since: i64,
    until: i64,
    total_senders: u32,
    headers_read: u32,
    model_classified: u32,
    model: String,
    senders: Vec<SenderJson>,
}

#[derive(Debug, Serialize)]
struct SenderJson {
    account_id: i64,
    address: String,
    name: String,
    messages: u64,
    read_messages: u64,
    read_rate: f64,
    median_gap_seconds: i64,
    your_replies: u64,
    class: String,
    source: String,
    signals: Vec<String>,
    unsubscribe_url: String,
    unsubscribe_mailto: String,
    one_click: bool,
    headers_read: bool,
    candidate: bool,
}

fn subs_json(report: &ListSubscriptionsResponse) -> SubsJson {
    SubsJson {
        since: report.since,
        until: report.until,
        total_senders: report.total_senders,
        headers_read: report.headers_read,
        model_classified: report.model_classified,
        model: report.model.clone(),
        senders: report.senders.iter().map(sender_json).collect(),
    }
}

fn sender_json(sender: &SubscriptionSender) -> SenderJson {
    let unsubscribe = sender.unsubscribe.clone().unwrap_or_default();
    SenderJson {
        account_id: sender.account_id,
        address: sender.address.clone(),
        name: sender.name.clone(),
        messages: sender.messages,
        read_messages: sender.read_messages,
        read_rate: sender.read_rate,
        median_gap_seconds: sender.median_gap_seconds,
        your_replies: sender.your_replies,
        class: class_label(sender.sender_class).to_owned(),
        source: source_label(sender.source).to_owned(),
        signals: sender.signals.clone(),
        unsubscribe_url: unsubscribe.http_url,
        unsubscribe_mailto: unsubscribe.mailto,
        one_click: unsubscribe.one_click,
        headers_read: sender.headers_read,
        candidate: sender.candidate,
    }
}

#[derive(Debug, Serialize)]
struct AskJson {
    question: String,
    sql: String,
    params: Vec<String>,
    notes: String,
    columns: Vec<String>,
    rows: Vec<Vec<serde_json::Value>>,
    truncated: bool,
    narrative: String,
    model: String,
}

fn ask_json(answer: &AskAnalyticsResponse) -> AskJson {
    AskJson {
        question: answer.question.clone(),
        sql: answer.sql.clone(),
        params: answer.params.clone(),
        notes: answer.notes.clone(),
        columns: answer.columns.clone(),
        rows: answer
            .rows
            .iter()
            .map(|row| row.cells.iter().map(cell_json).collect())
            .collect(),
        truncated: answer.truncated,
        narrative: answer.narrative.clone(),
        model: answer.model.clone(),
    }
}

/// A cell as JSON, keeping SQLite's own types rather than stringifying — the
/// whole point of `--json` is that something else consumes it.
fn cell_json(cell: &rmail_proto::v1::AnalyticsCell) -> serde_json::Value {
    match &cell.value {
        Some(CellValue::NullValue(_)) | None => serde_json::Value::Null,
        Some(CellValue::IntegerValue(v)) => serde_json::Value::from(*v),
        Some(CellValue::RealValue(v)) => serde_json::Number::from_f64(*v)
            .map_or(serde_json::Value::Null, serde_json::Value::Number),
        Some(CellValue::TextValue(v)) => serde_json::Value::String(v.clone()),
        Some(CellValue::Unsupported(_)) => serde_json::Value::Null,
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// `7d`, `12h`, `90s`, `4w` — the same grammar every other duration flag in
/// this CLI accepts.
fn parse_duration(value: &str) -> Result<i64> {
    let trimmed = value.trim();
    let (digits, multiplier) = match trimmed.chars().last() {
        Some('s') => (&trimmed[..trimmed.len() - 1], 1),
        Some('m') => (&trimmed[..trimmed.len() - 1], 60),
        Some('h') => (&trimmed[..trimmed.len() - 1], 3_600),
        Some('d') => (&trimmed[..trimmed.len() - 1], 86_400),
        Some('w') => (&trimmed[..trimmed.len() - 1], 7 * 86_400),
        _ => (trimmed, 1),
    };
    let count: i64 = digits
        .trim()
        .parse()
        .with_context(|| format!("`{value}` is not a duration like `7d`, `12h` or `90s`"))?;
    if count <= 0 {
        anyhow::bail!("`{value}` must be a positive duration");
    }
    count
        .checked_mul(multiplier)
        .with_context(|| format!("`{value}` is too large to express in seconds"))
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(0))
}

#[cfg(test)]
mod tests;
