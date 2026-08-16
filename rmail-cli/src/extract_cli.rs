//! `mail attach tables`, `mail extract events|tasks` and `mail links` — task
//! 75's human surface (prd.md features 54, 65, 66).
//!
//! # What each verb prints, and why the provenance is not optional
//!
//! All three print a table's, an item's or a link's *source* alongside its
//! content, because in every one of these three cases the interesting failure
//! is a plausible-looking value that came from somewhere other than where a
//! reader assumes:
//!
//! - `attach tables` prints `inferred` on any table a model transcribed off a
//!   rendered page, and the A1 reference for a cell read out of a workbook. A
//!   total from a spreadsheet cell and a total a model read off a PDF are not
//!   the same fact.
//! - `extract events`/`tasks` print `ics` or `model` per item. A `DTSTART` and
//!   a model's reading of "Thursday at 3" are not the same fact either.
//! - `links` prints a `!` marker and the host the text *claimed* whenever a
//!   link's display text names a different site than its target. That is the
//!   phishing case, and hiding it to make the output tidy would be the bug.
//!
//! # `mail links` never opens anything
//!
//! prd.md #66 sketches `mail links <id> --open`. What ships is `--copy N`,
//! which prints the Nth link's target to stdout for a human to do something
//! with, and no `--open`. Handing a URL a stranger wrote to a browser from a
//! mail client is the one action in this feature with a real blast radius, and
//! the daemon has deliberately not resolved the link, so nothing here knows
//! where it goes. Printing it is the honest maximum; piping it onward is the
//! user's own decision, made with the deceptive-link marker in front of them.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use rmail_proto::v1::attachment_service_client::AttachmentServiceClient;
use rmail_proto::v1::extract_service_client::ExtractServiceClient;
use rmail_proto::v1::link_service_client::LinkServiceClient;
use rmail_proto::v1::{
    CellType, ExtractEventsRequest, ExtractLinksRequest, ExtractTablesRequest, ExtractTasksRequest,
    ExtractedLink, ExtractionSink, ExtractionSource, LinkKind, TableOrigin,
};
use serde::Serialize;

/// `mail attach <action>`.
#[derive(Debug, Subcommand)]
pub enum AttachAction {
    /// Read one attachment's tables as typed rows
    /// (`AttachmentService.ExtractTables`).
    Tables(TablesArgs),
}

/// `mail attach tables <message-id>:<part-id>`.
#[derive(Debug, Args)]
pub struct TablesArgs {
    /// The message id.
    message_id: i64,
    /// The attachment's MIME part id, as `mail search --attachments` reports
    /// it.
    part_id: String,
    /// Allow a model pass for a PDF or an image, whose tables have no
    /// structure to parse. Costs a model call; without it those formats are
    /// declined rather than silently returning nothing.
    #[arg(long)]
    allow_model: bool,
    /// One JSON document instead of the rendered tables.
    #[arg(long)]
    json: bool,
}

/// `mail extract <action>`.
#[derive(Debug, Subcommand)]
pub enum ExtractAction {
    /// Calendar events from a message and any .ics it carries
    /// (`ExtractService.ExtractEvents`).
    Events(ItemArgs),
    /// Actionable tasks from a message and any .ics it carries
    /// (`ExtractService.ExtractTasks`).
    Tasks(ItemArgs),
}

/// `mail extract events|tasks <message-id> [flags]`.
#[derive(Debug, Args)]
pub struct ItemArgs {
    /// The message id.
    message_id: i64,
    /// Also let a model read the body for items the .ics does not state.
    #[arg(long)]
    use_model: bool,
    /// Where to deliver: `ics` (print the file, the default), `command` (pipe
    /// it to `extract.command`) or `webhook` (POST it to
    /// `extract.webhook_url`). The command and the URL come from the daemon's
    /// configuration; this only chooses which of them to use.
    #[arg(long, default_value = "ics")]
    sink: String,
    /// Print the .ics rather than a summary.
    #[arg(long)]
    ics: bool,
    /// One JSON document.
    #[arg(long)]
    json: bool,
}

/// `mail links <message-id> [flags]`.
#[derive(Debug, Args)]
pub struct LinksArgs {
    /// The message id.
    message_id: i64,
    /// Let a model refine the classification and the ranking. Without it the
    /// deterministic classifier's answer stands, which is a complete answer.
    #[arg(long)]
    use_model: bool,
    /// Print only the Nth link's target (1-based, in the printed order), for
    /// piping. See this module's docs on why there is no `--open`.
    #[arg(long)]
    copy: Option<usize>,
    /// One JSON document.
    #[arg(long)]
    json: bool,
}

// ---------------------------------------------------------------------------
// attach tables
// ---------------------------------------------------------------------------

/// Run `mail attach tables`.
///
/// # Errors
///
/// No daemon, a failed RPC, or an unwritable stdout.
pub async fn tables(socket: &Path, args: TablesArgs) -> Result<()> {
    let channel = connect(socket).await?;
    let response = AttachmentServiceClient::new(channel)
        .extract_tables(ExtractTablesRequest {
            message_id: args.message_id,
            part_id: args.part_id.clone(),
            allow_model: args.allow_model,
        })
        .await
        .context("ExtractTables RPC failed")?
        .into_inner();

    let mut out = std::io::stdout().lock();
    if args.json {
        writeln!(out, "{}", serde_json::to_string(&tables_json(&response))?)?;
        return Ok(());
    }
    if response.tables.is_empty() {
        writeln!(out, "no tables in this attachment")?;
        return Ok(());
    }
    for table in &response.tables {
        let origin = TableOrigin::try_from(table.origin).unwrap_or(TableOrigin::Unspecified);
        writeln!(
            out,
            "\n{} [{}{}{}]",
            table.name,
            origin.as_str_name().to_ascii_lowercase(),
            if table.inferred { ", inferred" } else { "" },
            if table.truncated { ", truncated" } else { "" }
        )?;
        let headers: Vec<String> = table
            .columns
            .iter()
            .map(|column| {
                let kind = CellType::try_from(column.r#type).unwrap_or(CellType::Unspecified);
                format!(
                    "{} ({})",
                    column.header,
                    kind.as_str_name().to_ascii_lowercase()
                )
            })
            .collect();
        if !headers.is_empty() {
            writeln!(out, "  {}", headers.join(" | "))?;
        }
        for row in &table.rows {
            let cells: Vec<String> = row
                .cells
                .iter()
                .map(|cell| {
                    let reference = cell
                        .source
                        .as_ref()
                        .map(|source| source.reference.clone())
                        .unwrap_or_default();
                    if reference.is_empty() {
                        cell.text.clone()
                    } else {
                        format!("{}@{reference}", cell.text)
                    }
                })
                .collect();
            writeln!(out, "  {}", cells.join(" | "))?;
        }
    }
    if response.dropped_tables > 0 || response.cell_budget_exhausted {
        writeln!(
            out,
            "\n({} table(s) dropped, cell budget {})",
            response.dropped_tables,
            if response.cell_budget_exhausted {
                "exhausted"
            } else {
                "intact"
            }
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// extract events / tasks
// ---------------------------------------------------------------------------

/// Run `mail extract events`.
///
/// # Errors
///
/// An unknown `--sink`, no daemon, a failed RPC, or an unwritable stdout.
pub async fn events(socket: &Path, args: ItemArgs) -> Result<()> {
    let sink = parse_sink(&args.sink)?;
    let channel = connect(socket).await?;
    let response = ExtractServiceClient::new(channel)
        .extract_events(ExtractEventsRequest {
            message_id: args.message_id,
            use_model: args.use_model,
            sink: sink as i32,
        })
        .await
        .context("ExtractEvents RPC failed")?
        .into_inner();

    let mut out = std::io::stdout().lock();
    if args.json {
        writeln!(
            out,
            "{}",
            serde_json::to_string(&serde_json::json!({
                "events": response.events.iter().map(|event| serde_json::json!({
                    "uid": event.uid,
                    "summary": event.summary,
                    "location": event.location,
                    "starts_at": event.starts_at,
                    "ends_at": event.ends_at,
                    "all_day": event.all_day,
                    "organizer": event.organizer,
                    "attendees": event.attendees,
                    "rrule": event.rrule,
                    "source": source_name(event.source),
                    "confidence": event.confidence,
                })).collect::<Vec<_>>(),
                "method": response.method,
                "skipped": response.skipped,
                "delivered": response.delivered,
                "already_delivered": response.already_delivered,
                "ics": response.ics,
            }))?
        )?;
        return Ok(());
    }
    if args.ics {
        write!(out, "{}", response.ics)?;
        return Ok(());
    }
    if !response.method.is_empty() {
        writeln!(out, "method: {}", response.method)?;
    }
    for event in &response.events {
        writeln!(
            out,
            "{}  {}{}  [{}]",
            stamp(event.starts_at, event.all_day),
            event.summary,
            if event.location.is_empty() {
                String::new()
            } else {
                format!(" @ {}", event.location)
            },
            source_name(event.source)
        )?;
    }
    summarize(
        &mut out,
        response.events.len(),
        response.delivered,
        response.already_delivered,
        response.skipped,
        &response.sink_output,
    )
}

/// Run `mail extract tasks`.
///
/// # Errors
///
/// An unknown `--sink`, no daemon, a failed RPC, or an unwritable stdout.
pub async fn tasks(socket: &Path, args: ItemArgs) -> Result<()> {
    let sink = parse_sink(&args.sink)?;
    let channel = connect(socket).await?;
    let response = ExtractServiceClient::new(channel)
        .extract_tasks(ExtractTasksRequest {
            message_id: args.message_id,
            use_model: args.use_model,
            sink: sink as i32,
        })
        .await
        .context("ExtractTasks RPC failed")?
        .into_inner();

    let mut out = std::io::stdout().lock();
    if args.json {
        writeln!(
            out,
            "{}",
            serde_json::to_string(&serde_json::json!({
                "tasks": response.tasks.iter().map(|task| serde_json::json!({
                    "uid": task.uid,
                    "summary": task.summary,
                    "due_at": task.due_at,
                    "priority": task.priority,
                    "completed": task.completed,
                    "source": source_name(task.source),
                    "confidence": task.confidence,
                })).collect::<Vec<_>>(),
                "skipped": response.skipped,
                "delivered": response.delivered,
                "already_delivered": response.already_delivered,
                "ics": response.ics,
            }))?
        )?;
        return Ok(());
    }
    if args.ics {
        write!(out, "{}", response.ics)?;
        return Ok(());
    }
    for task in &response.tasks {
        writeln!(
            out,
            "{}  {}  [{}]",
            if task.due_at == 0 {
                "          ".to_owned()
            } else {
                stamp(task.due_at, true)
            },
            task.summary,
            source_name(task.source)
        )?;
    }
    summarize(
        &mut out,
        response.tasks.len(),
        response.delivered,
        response.already_delivered,
        response.skipped,
        &response.sink_output,
    )
}

/// The trailer both item verbs print: what was found, what was pushed, and
/// what a previous run had already pushed.
fn summarize(
    out: &mut impl Write,
    found: usize,
    delivered: u32,
    already: u32,
    skipped: u32,
    sink_output: &str,
) -> Result<()> {
    writeln!(
        out,
        "\n{found} found, {delivered} delivered, {already} already delivered, {skipped} skipped"
    )?;
    if !sink_output.trim().is_empty() {
        writeln!(out, "sink: {}", sink_output.trim())?;
    }
    Ok(())
}

fn parse_sink(name: &str) -> Result<ExtractionSink> {
    match name {
        "ics" => Ok(ExtractionSink::Ics),
        "command" => Ok(ExtractionSink::Command),
        "webhook" => Ok(ExtractionSink::Webhook),
        other => anyhow::bail!("unknown --sink {other:?}; expected ics, command or webhook"),
    }
}

fn source_name(source: i32) -> &'static str {
    match ExtractionSource::try_from(source) {
        Ok(ExtractionSource::Ics) => "ics",
        Ok(ExtractionSource::Model) => "model",
        _ => "unknown",
    }
}

fn stamp(at: i64, date_only: bool) -> String {
    let Some(when) = chrono::DateTime::from_timestamp(at, 0) else {
        return "?".to_owned();
    };
    if date_only {
        when.format("%Y-%m-%d").to_string()
    } else {
        when.format("%Y-%m-%d %H:%M").to_string()
    }
}

// ---------------------------------------------------------------------------
// links
// ---------------------------------------------------------------------------

/// Run `mail links`.
///
/// # Errors
///
/// A `--copy` index outside the list, no daemon, a failed RPC, or an
/// unwritable stdout.
pub async fn links(socket: &Path, args: LinksArgs) -> Result<()> {
    let channel = connect(socket).await?;
    let response = LinkServiceClient::new(channel)
        .extract_links(ExtractLinksRequest {
            message_id: args.message_id,
            use_model: args.use_model,
        })
        .await
        .context("ExtractLinks RPC failed")?
        .into_inner();

    let mut out = std::io::stdout().lock();
    if let Some(index) = args.copy {
        let link = index
            .checked_sub(1)
            .and_then(|index| response.links.get(index))
            .with_context(|| {
                format!(
                    "--copy {index} is outside the {} link(s) in this message",
                    response.links.len()
                )
            })?;
        writeln!(out, "{}", link.url)?;
        return Ok(());
    }
    if args.json {
        writeln!(
            out,
            "{}",
            serde_json::to_string(&LinksJson {
                links: response.links.iter().map(link_json).collect(),
                truncated: response.truncated,
                skipped_parts: response.skipped_parts,
                tracking_pixels: response.tracking_pixels,
            })?
        )?;
        return Ok(());
    }
    for (index, link) in response.links.iter().enumerate() {
        writeln!(
            out,
            "{:>2}. {}{:<12} {:.2}  {}",
            index + 1,
            if link.deceptive { "! " } else { "  " },
            kind_name(link.kind),
            link.score,
            link.url
        )?;
        if !link.display_text.is_empty() {
            writeln!(out, "       text: {}", link.display_text)?;
        }
        if link.deceptive {
            writeln!(
                out,
                "       WARNING: {}",
                if link.display_host.is_empty() {
                    "this host is not what it appears to be (punycode or non-ASCII)".to_owned()
                } else {
                    format!(
                        "the text names {} but the link goes to {}",
                        link.display_host, link.host
                    )
                }
            )?;
        }
        writeln!(out, "       why: {}", link.reason)?;
    }
    if response.tracking_pixels > 0 || response.truncated > 0 {
        writeln!(
            out,
            "\n({} tracking pixel(s) not listed, {} link(s) dropped)",
            response.tracking_pixels, response.truncated
        )?;
    }
    Ok(())
}

fn kind_name(kind: i32) -> &'static str {
    match LinkKind::try_from(kind) {
        Ok(LinkKind::Unsubscribe) => "unsubscribe",
        Ok(LinkKind::Tracking) => "tracking",
        Ok(LinkKind::Meeting) => "meeting",
        Ok(LinkKind::Document) => "document",
        Ok(LinkKind::Cta) => "cta",
        Ok(LinkKind::Other) => "other",
        _ => "unknown",
    }
}

// ---------------------------------------------------------------------------
// JSON, hand-written for the reason `search_cli` gives: a proto field rename
// must not silently reshape a documented CLI contract.
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct LinksJson {
    links: Vec<LinkJson>,
    truncated: u32,
    skipped_parts: u32,
    tracking_pixels: u32,
}

#[derive(Debug, Serialize)]
struct LinkJson {
    url: String,
    host: String,
    display_text: String,
    display_host: String,
    deceptive: bool,
    kind: String,
    score: f64,
    reason: String,
    occurrences: u32,
}

fn link_json(link: &ExtractedLink) -> LinkJson {
    LinkJson {
        url: link.url.clone(),
        host: link.host.clone(),
        display_text: link.display_text.clone(),
        display_host: link.display_host.clone(),
        deceptive: link.deceptive,
        kind: kind_name(link.kind).to_owned(),
        score: link.score,
        reason: link.reason.clone(),
        occurrences: link.occurrences,
    }
}

fn tables_json(response: &rmail_proto::v1::ExtractTablesResponse) -> serde_json::Value {
    serde_json::json!({
        "tables": response.tables.iter().map(|table| serde_json::json!({
            "name": table.name,
            "origin": TableOrigin::try_from(table.origin)
                .unwrap_or(TableOrigin::Unspecified)
                .as_str_name()
                .to_ascii_lowercase(),
            "inferred": table.inferred,
            "truncated": table.truncated,
            "columns": table.columns.iter().map(|column| serde_json::json!({
                "header": column.header,
                "type": CellType::try_from(column.r#type)
                    .unwrap_or(CellType::Unspecified)
                    .as_str_name()
                    .to_ascii_lowercase(),
            })).collect::<Vec<_>>(),
            "rows": table.rows.iter().map(|row| row.cells.iter().map(|cell| serde_json::json!({
                "text": cell.text,
                "reference": cell.source.as_ref().map(|s| s.reference.clone()).unwrap_or_default(),
                "page": cell.source.as_ref().map_or(0, |s| s.page),
            })).collect::<Vec<_>>()).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
        "dropped_tables": response.dropped_tables,
        "cell_budget_exhausted": response.cell_budget_exhausted,
    })
}

async fn connect(socket: &Path) -> Result<tonic::transport::Channel> {
    rmail_core::connect_uds(socket)
        .await
        .with_context(|| format!("connecting to rmaild at {}", socket.display()))
}
