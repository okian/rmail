//! `mail stats` — mailbox analytics verbs over `AnalyticsService`.
//!
//! Currently one verb, `mail stats response-time` (task 71, prd.md feature
//! 58). It is a namespace rather than a bare command because prd.md's
//! `AnalyticsService` grows three more reports (contact insight, subscription
//! detection, natural-language analytics), and `mail stats <report>` is where
//! those belong.
//!
//! # `--since` takes a duration, not a timestamp
//!
//! `--since 30d` reads better than a unix second and is what every other
//! duration-shaped flag in this CLI accepts. The daemon still receives
//! absolute bounds — the report has to name the window it summarized, and a
//! relative one would mean something different by the time it was rendered.
//!
//! # The `--json` schema
//!
//! One object, not one line per group, because the report is a single
//! document with several sections and splitting it would lose which totals a
//! group belongs to. Hand-written rather than derived from the wire types,
//! for the reason `search_cli`'s module docs give: a proto field rename must
//! not silently reshape a documented CLI contract.
//!
//! ```json
//! {
//!   "since": 1700000000,
//!   "until": 1707776000,
//!   "by": "contact",
//!   "you": {"samples": 42, "p50_seconds": 5400, "p90_seconds": 172800, ...},
//!   "them": {"samples": 39, ...},
//!   "self_addresses": ["me@example.com"],
//!   "groups": [
//!     {
//!       "key": "alice@example.com",
//!       "label": "Alice",
//!       "you": {...}, "them": {...},
//!       "inbound": 12, "awaiting_reply": 3, "overdue": 2,
//!       "bottleneck": true, "slower_than_counterpart": false, "stalled": true
//!     }
//!   ],
//!   "trend": [{"window_start": …, "window_end": …, "stats": {…}}]
//! }
//! ```

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use clap::{Args, Subcommand, ValueEnum};
use rmail_proto::v1::analytics_service_client::AnalyticsServiceClient;
use rmail_proto::v1::{
    GetResponseTimesRequest, GetResponseTimesResponse, ResponseStats, ResponseTimeGroup,
    ResponseTimeGroupBy, ResponseTrendPoint,
};
use serde::Serialize;

/// `mail stats <report>`.
#[derive(Debug, Subcommand)]
pub enum StatsAction {
    /// How fast you answer, per contact or per folder, and where you are the
    /// bottleneck (`AnalyticsService.GetResponseTimes`).
    #[command(name = "response-time")]
    ResponseTime(ResponseTimeArgs),
    /// Answer a plain-English question about the mailbox with rows and a short
    /// narrative (`AnalyticsService.AskAnalytics`).
    ///
    /// Here rather than at `mail ask`, which feature 43 already uses for
    /// questions about message *contents* — see `analytics_cli`'s own module
    /// docs.
    Ask(crate::analytics_cli::AskArgs),
}

/// `mail stats response-time [flags]`.
#[derive(Debug, Args)]
pub struct ResponseTimeArgs {
    /// Group by contact (the default) or by folder.
    #[arg(long = "by", value_enum, default_value_t = ByArg::Contact)]
    by: ByArg,
    /// Restrict to one account.
    #[arg(long)]
    account: Option<i64>,
    /// How far back to look, e.g. `30d`, `12w`, `6h`. Default: 90 days.
    #[arg(long)]
    since: Option<String>,
    /// End the window here instead of now, as unix seconds.
    #[arg(long)]
    until: Option<i64>,
    /// Trend step, e.g. `7d`. Default: one point per week.
    #[arg(long)]
    bucket: Option<String>,
    /// Rolling span each trend point summarizes, e.g. `28d`. Must be at least
    /// `--bucket`. Default: four weeks.
    #[arg(long)]
    window: Option<String>,
    /// Maximum groups to print. Must be positive — 0 would reach the daemon
    /// as "unset" and quietly print its default instead.
    #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..))]
    limit: u32,
    /// Observations a group needs before it can be flagged a bottleneck.
    #[arg(long)]
    min_samples: Option<u32>,
    /// How many times slower than the other side your median must be to count
    /// as the bottleneck. Default: 2.
    #[arg(long)]
    bottleneck_ratio: Option<f64>,
    /// Print only the groups where you are the bottleneck.
    #[arg(long)]
    bottlenecks_only: bool,
    /// Print the rolling trend as well as the groups.
    #[arg(long)]
    trend: bool,
    /// One JSON document instead of the tables.
    #[arg(long)]
    json: bool,
}

/// `--by`'s vocabulary. Spelled out rather than reusing the proto enum so
/// `--help` prints `contact` rather than `RESPONSE_TIME_GROUP_BY_CONTACT`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ByArg {
    Contact,
    Mailbox,
}

impl ByArg {
    fn into_proto(self) -> ResponseTimeGroupBy {
        match self {
            Self::Contact => ResponseTimeGroupBy::Contact,
            Self::Mailbox => ResponseTimeGroupBy::Mailbox,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Contact => "contact",
            Self::Mailbox => "mailbox",
        }
    }
}

/// Run `mail stats <report>`.
///
/// # Errors
///
/// Anything that stops the command completing: an unparseable duration, no
/// daemon, a failed RPC, an unwritable stdout.
pub async fn run(socket: &Path, action: StatsAction) -> Result<()> {
    match action {
        StatsAction::ResponseTime(args) => response_time(socket, args).await,
        StatsAction::Ask(args) => crate::analytics_cli::ask(socket, args).await,
    }
}

async fn response_time(socket: &Path, args: ResponseTimeArgs) -> Result<()> {
    let since = args
        .since
        .as_deref()
        .map(parse_duration)
        .transpose()
        .context("--since")?;
    let bucket = args
        .bucket
        .as_deref()
        .map(parse_duration)
        .transpose()
        .context("--bucket")?;
    let window = args
        .window
        .as_deref()
        .map(parse_duration)
        .transpose()
        .context("--window")?;

    let (since_abs, until) = resolve_window(since, args.until, now());

    let channel = rmail_core::connect_uds(socket)
        .await
        .with_context(|| format!("connecting to rmaild at {}", socket.display()))?;
    let mut client = AnalyticsServiceClient::new(channel);

    let report = client
        .get_response_times(GetResponseTimesRequest {
            account_id: args.account.unwrap_or(0),
            group_by: args.by.into_proto() as i32,
            since: since_abs,
            until,
            bucket_seconds: bucket.unwrap_or(0),
            window_seconds: window.unwrap_or(0),
            limit: args.limit,
            min_samples: args.min_samples.unwrap_or(0),
            bottleneck_ratio: args.bottleneck_ratio.unwrap_or(0.0),
        })
        .await
        .context("GetResponseTimes RPC failed")?
        .into_inner();

    let mut out = std::io::stdout().lock();
    if args.json {
        let line = serde_json::to_string(&to_json(&report, args.by, args.bottlenecks_only))?;
        writeln!(out, "{line}")?;
        return Ok(());
    }
    print_report(&mut out, &report, &args)
}

fn print_report(
    out: &mut impl Write,
    report: &GetResponseTimesResponse,
    args: &ResponseTimeArgs,
) -> Result<()> {
    let ours = report.ours.unwrap_or_default();
    let theirs = report.theirs.unwrap_or_default();

    writeln!(
        out,
        "window      {} .. {}  ({} pairs, by {})",
        report.since,
        report.until,
        report.pairs,
        args.by.as_str()
    )?;
    writeln!(out, "you         {}", summarize(ours))?;
    writeln!(out, "them        {}", summarize(theirs))?;
    if report.skipped_out_of_order > 0 {
        writeln!(
            out,
            "skipped     {} pair(s) whose reply predates what it answers",
            report.skipped_out_of_order
        )?;
    }
    if report.self_addresses.is_empty() {
        // Without an identity every pair is discarded, so an empty report is
        // almost always this and not "you answer nothing".
        writeln!(
            out,
            "note        no address is known to be yours (set `username` on the account, or \
             sync a Sent folder); nothing can be attributed"
        )?;
    }
    writeln!(out)?;

    let groups: Vec<&ResponseTimeGroup> = report
        .groups
        .iter()
        .filter(|group| !args.bottlenecks_only || group.bottleneck)
        .collect();
    if groups.is_empty() {
        writeln!(out, "(no groups)")?;
    } else {
        writeln!(
            out,
            "{:<34}  {:>8}  {:>10}  {:>10}  {:>7}  FLAG",
            "GROUP", "REPLIES", "YOUR P50", "YOUR P90", "OVERDUE"
        )?;
        for group in &groups {
            let ours = group.ours.unwrap_or_default();
            writeln!(
                out,
                "{:<34}  {:>8}  {:>10}  {:>10}  {:>7}  {}",
                truncate(&group.label, 34),
                ours.samples,
                humanize(ours.p50_seconds),
                humanize(ours.p90_seconds),
                group.overdue,
                flag(group)
            )?;
        }
        if report.total_groups as usize > report.groups.len() {
            writeln!(
                out,
                "... {} more group(s); raise --limit",
                report.total_groups as usize - report.groups.len()
            )?;
        }
    }

    if args.trend {
        writeln!(out)?;
        writeln!(
            out,
            "{:<12}  {:>8}  {:>10}  {:>10}",
            "TREND END", "REPLIES", "P50", "P90"
        )?;
        for point in &report.trend {
            let stats = point.stats.unwrap_or_default();
            writeln!(
                out,
                "{:<12}  {:>8}  {:>10}  {:>10}",
                point.window_end,
                stats.samples,
                humanize(stats.p50_seconds),
                humanize(stats.p90_seconds)
            )?;
        }
    }
    Ok(())
}

/// Why a group is flagged, or nothing at all.
fn flag(group: &ResponseTimeGroup) -> &'static str {
    match (group.slower_than_counterpart, group.stalled) {
        (true, true) => "BOTTLENECK (slower, stalled)",
        (true, false) => "BOTTLENECK (slower)",
        (false, true) => "BOTTLENECK (stalled)",
        (false, false) => "",
    }
}

/// A one-line summary of a stats block, or an honest "no data".
fn summarize(stats: ResponseStats) -> String {
    if stats.samples == 0 {
        return "no replies in this window".to_owned();
    }
    format!(
        "n={} p50={} p90={} min={} max={}",
        stats.samples,
        humanize(stats.p50_seconds),
        humanize(stats.p90_seconds),
        humanize(stats.min_seconds),
        humanize(stats.max_seconds)
    )
}

/// Seconds as the coarsest unit that keeps a whole number in front of the
/// decimal point — a p90 of "3.2d" is read at a glance and "276480s" is not.
fn humanize(seconds: i64) -> String {
    const MINUTE: i64 = 60;
    const HOUR: i64 = 60 * MINUTE;
    const DAY: i64 = 24 * HOUR;
    let (value, unit) = if seconds.abs() >= DAY {
        (seconds as f64 / DAY as f64, "d")
    } else if seconds.abs() >= HOUR {
        (seconds as f64 / HOUR as f64, "h")
    } else if seconds.abs() >= MINUTE {
        (seconds as f64 / MINUTE as f64, "m")
    } else {
        return format!("{seconds}s");
    };
    format!("{value:.1}{unit}")
}

/// Clip a label to `width` columns, marking that it was clipped.
fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    let kept: String = text.chars().take(width.saturating_sub(1)).collect();
    format!("{kept}…")
}

/// `12h`, `30d`, `6w`, `90` (bare seconds) — the same vocabulary
/// `config::duration` uses, minus the units a report has no use for.
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
        .with_context(|| format!("`{value}` is not a duration like `30d`, `12h` or `90s`"))?;
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

/// The absolute `(since, until)` the wire carries, from a relative `--since`.
///
/// `--since` is a duration and the wire wants a bound, so it has to be
/// anchored: to `--until` when the user gave one, otherwise to this process's
/// clock. The daemon is on the same machine, so a sub-second disagreement
/// between its clock and this one is not worth a round trip to ask for its
/// `now`.
///
/// Zero on either output means "unset" — the daemon then applies its own
/// `until = now` and `since = until - 90d`. That is why an absent `--since`
/// sends 0 rather than an epoch timestamp computed here: leaving the default
/// to the daemon keeps both bounds anchored to *one* clock.
fn resolve_window(since: Option<i64>, until: Option<i64>, now: i64) -> (i64, i64) {
    let since_abs = match (since, until) {
        (Some(span), Some(until)) => until.saturating_sub(span),
        (Some(span), None) => now.saturating_sub(span),
        (None, _) => 0,
    };
    (since_abs, until.unwrap_or(0))
}

// ---------------------------------------------------------------------------
// JSON projection
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct JsonReport {
    since: i64,
    until: i64,
    by: &'static str,
    pairs: u64,
    skipped_out_of_order: u64,
    self_addresses: Vec<String>,
    you: JsonStats,
    them: JsonStats,
    total_groups: u32,
    groups: Vec<JsonGroup>,
    trend: Vec<JsonTrend>,
}

#[derive(Debug, Serialize)]
struct JsonStats {
    samples: u64,
    p50_seconds: i64,
    p90_seconds: i64,
    mean_seconds: f64,
    min_seconds: i64,
    max_seconds: i64,
}

#[derive(Debug, Serialize)]
struct JsonGroup {
    key: String,
    label: String,
    mailbox_id: i64,
    you: JsonStats,
    them: JsonStats,
    inbound: u64,
    awaiting_reply: u64,
    overdue: u64,
    bottleneck: bool,
    slower_than_counterpart: bool,
    stalled: bool,
}

#[derive(Debug, Serialize)]
struct JsonTrend {
    window_start: i64,
    window_end: i64,
    stats: JsonStats,
}

fn to_json(report: &GetResponseTimesResponse, by: ByArg, bottlenecks_only: bool) -> JsonReport {
    JsonReport {
        since: report.since,
        until: report.until,
        by: by.as_str(),
        pairs: report.pairs,
        skipped_out_of_order: report.skipped_out_of_order,
        self_addresses: report.self_addresses.clone(),
        you: json_stats(report.ours),
        them: json_stats(report.theirs),
        total_groups: report.total_groups,
        groups: report
            .groups
            .iter()
            .filter(|group| !bottlenecks_only || group.bottleneck)
            .map(json_group)
            .collect(),
        trend: report.trend.iter().map(json_trend).collect(),
    }
}

fn json_stats(stats: Option<ResponseStats>) -> JsonStats {
    let stats = stats.unwrap_or_default();
    JsonStats {
        samples: stats.samples,
        p50_seconds: stats.p50_seconds,
        p90_seconds: stats.p90_seconds,
        mean_seconds: stats.mean_seconds,
        min_seconds: stats.min_seconds,
        max_seconds: stats.max_seconds,
    }
}

fn json_group(group: &ResponseTimeGroup) -> JsonGroup {
    JsonGroup {
        key: group.key.clone(),
        label: group.label.clone(),
        mailbox_id: group.mailbox_id,
        you: json_stats(group.ours),
        them: json_stats(group.theirs),
        inbound: group.inbound,
        awaiting_reply: group.awaiting_reply,
        overdue: group.overdue,
        bottleneck: group.bottleneck,
        slower_than_counterpart: group.slower_than_counterpart,
        stalled: group.stalled,
    }
}

fn json_trend(point: &ResponseTrendPoint) -> JsonTrend {
    JsonTrend {
        window_start: point.window_start,
        window_end: point.window_end,
        stats: json_stats(point.stats),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn durations_parse_in_every_unit() {
        assert_eq!(parse_duration("90").unwrap(), 90);
        assert_eq!(parse_duration("90s").unwrap(), 90);
        assert_eq!(parse_duration("5m").unwrap(), 300);
        assert_eq!(parse_duration("2h").unwrap(), 7_200);
        assert_eq!(parse_duration("30d").unwrap(), 2_592_000);
        assert_eq!(parse_duration(" 4w ").unwrap(), 2_419_200);
    }

    #[test]
    fn a_bad_duration_is_an_error_not_a_silent_zero() {
        for bad in ["", "d", "-1d", "0d", "abc", "1y"] {
            assert!(
                parse_duration(bad).is_err(),
                "`{bad}` must not parse to a window"
            );
        }
    }

    #[test]
    fn an_overflowing_duration_is_refused() {
        assert!(parse_duration(&format!("{}w", i64::MAX)).is_err());
    }

    const NOW: i64 = 1_700_000_000;

    #[test]
    fn an_unset_window_is_left_entirely_to_the_daemon() {
        assert_eq!(
            resolve_window(None, None, NOW),
            (0, 0),
            "both bounds must stay unset so one clock anchors both"
        );
    }

    #[test]
    fn a_relative_since_anchors_to_until_when_there_is_one() {
        assert_eq!(resolve_window(Some(600), Some(9_000), NOW), (8_400, 9_000));
    }

    #[test]
    fn a_relative_since_anchors_to_the_clock_otherwise() {
        assert_eq!(resolve_window(Some(600), None, NOW), (NOW - 600, 0));
    }

    #[test]
    fn an_absolute_until_alone_leaves_since_to_the_daemons_default() {
        assert_eq!(resolve_window(None, Some(9_000), NOW), (0, 9_000));
    }

    #[test]
    fn an_absurd_span_clamps_instead_of_overflowing() {
        // `0 - i64::MAX` saturates to `i64::MIN + 1`, not to `i64::MIN`;
        // either way the daemon rejects it, and neither panics.
        assert_eq!(
            resolve_window(Some(i64::MAX), Some(0), NOW),
            (i64::MIN + 1, 0)
        );
    }

    #[test]
    fn humanized_seconds_pick_the_coarsest_whole_unit() {
        assert_eq!(humanize(0), "0s");
        assert_eq!(humanize(45), "45s");
        assert_eq!(humanize(90), "1.5m");
        assert_eq!(humanize(5_400), "1.5h");
        assert_eq!(humanize(129_600), "1.5d");
    }

    #[test]
    fn labels_are_clipped_without_splitting_a_character() {
        assert_eq!(truncate("short", 10), "short");
        assert_eq!(truncate("ααααααα", 4), "ααα…");
    }

    fn stats(samples: u64, p50: i64) -> Option<ResponseStats> {
        Some(ResponseStats {
            samples,
            p50_seconds: p50,
            p90_seconds: p50 * 2,
            mean_seconds: p50 as f64,
            min_seconds: p50,
            max_seconds: p50 * 2,
        })
    }

    fn group(key: &str, slower: bool, stalled: bool) -> ResponseTimeGroup {
        ResponseTimeGroup {
            key: key.to_owned(),
            label: key.to_owned(),
            mailbox_id: 0,
            ours: stats(4, 3_600),
            theirs: stats(4, 60),
            inbound: 9,
            awaiting_reply: 5,
            overdue: 4,
            bottleneck: slower || stalled,
            slower_than_counterpart: slower,
            stalled,
        }
    }

    fn report(groups: Vec<ResponseTimeGroup>) -> GetResponseTimesResponse {
        GetResponseTimesResponse {
            since: 100,
            until: 200,
            group_by: ResponseTimeGroupBy::Contact as i32,
            ours: stats(8, 3_600),
            theirs: stats(8, 60),
            total_groups: groups.len() as u32,
            groups,
            trend: vec![ResponseTrendPoint {
                window_start: 100,
                window_end: 200,
                stats: stats(8, 3_600),
            }],
            self_addresses: vec!["me@x".to_owned()],
            pairs: 16,
            skipped_out_of_order: 2,
        }
    }

    fn args() -> ResponseTimeArgs {
        ResponseTimeArgs {
            by: ByArg::Contact,
            account: None,
            since: None,
            until: None,
            bucket: None,
            window: None,
            limit: 20,
            min_samples: None,
            bottleneck_ratio: None,
            bottlenecks_only: false,
            trend: false,
            json: false,
        }
    }

    fn render(report: &GetResponseTimesResponse, args: &ResponseTimeArgs) -> String {
        let mut out: Vec<u8> = Vec::new();
        print_report(&mut out, report, args).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn the_table_names_why_a_group_is_flagged() {
        let rendered = render(
            &report(vec![
                group("slow@x", true, false),
                group("stalled@x", false, true),
                group("both@x", true, true),
                group("fine@x", false, false),
            ]),
            &args(),
        );
        assert!(rendered.contains("BOTTLENECK (slower)"));
        assert!(rendered.contains("BOTTLENECK (stalled)"));
        assert!(rendered.contains("BOTTLENECK (slower, stalled)"));
        let fine = rendered
            .lines()
            .find(|line| line.starts_with("fine@x"))
            .unwrap();
        assert!(!fine.contains("BOTTLENECK"));
    }

    #[test]
    fn bottlenecks_only_hides_the_healthy_groups() {
        let mut args = args();
        args.bottlenecks_only = true;
        let rendered = render(
            &report(vec![
                group("slow@x", true, false),
                group("fine@x", false, false),
            ]),
            &args,
        );
        assert!(rendered.contains("slow@x"));
        assert!(!rendered.contains("fine@x"));
    }

    #[test]
    fn an_empty_report_says_so_rather_than_printing_a_bare_header() {
        let mut empty = report(Vec::new());
        empty.ours = Some(ResponseStats::default());
        empty.theirs = Some(ResponseStats::default());
        let rendered = render(&empty, &args());
        assert!(rendered.contains("no replies in this window"));
        assert!(rendered.contains("(no groups)"));
    }

    #[test]
    fn a_report_with_no_known_identity_explains_itself() {
        let mut orphan = report(Vec::new());
        orphan.self_addresses.clear();
        let rendered = render(&orphan, &args());
        assert!(
            rendered.contains("no address is known to be yours"),
            "an empty report is almost always this: {rendered}"
        );
    }

    #[test]
    fn truncation_is_announced_rather_than_silent() {
        let mut truncated = report(vec![group("a@x", false, false)]);
        truncated.total_groups = 7;
        let rendered = render(&truncated, &args());
        assert!(rendered.contains("6 more group(s)"));
    }

    #[test]
    fn the_trend_prints_only_when_asked_for() {
        let data = report(vec![group("a@x", false, false)]);
        assert!(!render(&data, &args()).contains("TREND END"));
        let mut with_trend = args();
        with_trend.trend = true;
        assert!(render(&data, &with_trend).contains("TREND END"));
    }

    #[test]
    fn the_json_document_carries_every_flag_and_honours_the_filter() {
        let data = report(vec![
            group("slow@x", true, false),
            group("fine@x", false, false),
        ]);
        let all = to_json(&data, ByArg::Contact, false);
        assert_eq!(all.groups.len(), 2);
        assert_eq!(all.by, "contact");
        assert_eq!(all.you.samples, 8);
        assert_eq!(all.skipped_out_of_order, 2);
        assert_eq!(all.trend.len(), 1);

        let filtered = to_json(&data, ByArg::Mailbox, true);
        assert_eq!(filtered.groups.len(), 1);
        assert_eq!(filtered.by, "mailbox");
        assert!(filtered.groups[0].slower_than_counterpart);
        assert_eq!(filtered.groups[0].overdue, 4);
    }

    #[test]
    fn absent_wire_stats_project_to_zeroes_not_a_crash() {
        // proto3 message fields are optional on the wire; a peer that omitted
        // them must not take the CLI down.
        let mut bare = report(vec![group("a@x", false, false)]);
        bare.ours = None;
        bare.theirs = None;
        if let Some(group) = bare.groups.first_mut() {
            group.ours = None;
            group.theirs = None;
        }
        bare.trend[0].stats = None;
        let json = to_json(&bare, ByArg::Contact, false);
        assert_eq!(json.you.samples, 0);
        assert_eq!(json.groups[0].you.samples, 0);
        assert_eq!(json.trend[0].stats.samples, 0);
        assert!(render(&bare, &args()).contains("no replies in this window"));
    }
}
