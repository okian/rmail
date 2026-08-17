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
    CellType, ExportInvoicesRequest, ExtractEventsRequest, ExtractInvoiceRequest,
    ExtractLinksRequest, ExtractStructuredRequest, ExtractTablesRequest, ExtractTasksRequest,
    ExtractedInvoice, ExtractedLink, ExtractionSink, ExtractionSource, FieldOrigin,
    FieldProvenance, InvoiceDocKind, InvoiceExportFormat, InvoiceMoney, InvoicePaymentStatus,
    InvoiceText, LinkKind, TableOrigin,
};
use serde::Serialize;

/// `mail attach <action>`.
#[derive(Debug, Subcommand)]
pub enum AttachAction {
    /// Read one attachment's tables as typed rows
    /// (`AttachmentService.ExtractTables`).
    Tables(TablesArgs),
    /// Detect and read an invoice or receipt
    /// (`AttachmentService.ExtractInvoice`).
    Invoice(InvoiceArgs),
}

/// `mail attach invoice <message-id> [part-id]`.
#[derive(Debug, Args)]
pub struct InvoiceArgs {
    /// The message id.
    message_id: i64,
    /// One attachment's MIME part id. Omitted, the daemon detects across the
    /// message's document attachments and falls back to the body.
    part_id: Option<String>,
    /// Also let a model read the document for fields it does not label —
    /// most usefully the line items and a vendor no line names. Costs a model
    /// call; it never overrides a figure the document states in words.
    #[arg(long)]
    use_model: bool,
    /// One JSON document.
    #[arg(long)]
    json: bool,
}

/// `mail invoices [--export csv]`.
#[derive(Debug, Args)]
pub struct InvoicesArgs {
    /// Restrict to one account.
    #[arg(long)]
    account: Option<i64>,
    /// Restrict to one message.
    #[arg(long)]
    message: Option<i64>,
    /// Only vendors containing this, case-insensitively.
    #[arg(long)]
    vendor: Option<String>,
    /// Only invoices issued on or after this day (`YYYY-MM-DD`).
    #[arg(long)]
    since: Option<String>,
    /// Only invoices issued on or before this day (`YYYY-MM-DD`).
    #[arg(long)]
    until: Option<String>,
    /// Page size.
    #[arg(long, default_value_t = 50)]
    limit: i64,
    /// Render as `csv` on stdout instead of a table. The CSV names each row's
    /// model-inferred fields in its own column — see `extract_cli`'s module
    /// docs on why that is not optional.
    #[arg(long)]
    export: Option<String>,
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
    /// Structured data against a JSON schema
    /// (`ExtractService.ExtractStructured`).
    ///
    /// prd.md #4 sketches this as `mail extract <id> --schema invoice`; the
    /// verb had already become a subcommand group by the time it was built
    /// (`mail extract events|tasks`, task 75), and clap cannot tell a bare id
    /// from a subcommand name. `data` is the third sibling, and the flag is
    /// spelled exactly as the PRD does.
    Data(StructuredArgs),
}

/// `mail extract data <message-id> --schema invoice`.
#[derive(Debug, Args)]
pub struct StructuredArgs {
    /// The message id.
    message_id: i64,
    /// A built-in schema: invoice, receipt, flight, meeting, order.
    #[arg(long, default_value = "invoice")]
    schema: String,
    /// A JSON Schema of your own, read from this file, instead of a built-in.
    /// The root must be `{"type": "object"}`; the daemon bounds its size,
    /// depth and property count and rejects any keyword it cannot enforce.
    #[arg(long)]
    schema_file: Option<std::path::PathBuf>,
    /// Re-run a message already extracted under this exact schema. Without
    /// it the stored document comes back and no tokens are spent.
    #[arg(long)]
    refresh: bool,
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
// attach invoice / invoices
// ---------------------------------------------------------------------------

/// Run `mail attach invoice`.
///
/// # Errors
///
/// No daemon, a failed RPC, or an unwritable stdout.
pub async fn invoice(socket: &Path, args: InvoiceArgs) -> Result<()> {
    let channel = connect(socket).await?;
    let response = AttachmentServiceClient::new(channel)
        .extract_invoice(ExtractInvoiceRequest {
            message_id: args.message_id,
            part_id: args.part_id.clone().unwrap_or_default(),
            use_model: args.use_model,
        })
        .await
        .context("ExtractInvoice RPC failed")?
        .into_inner();

    let mut out = std::io::stdout().lock();
    let Some(extracted) = response.invoice.as_ref() else {
        writeln!(out, "no invoice was read from this message")?;
        return Ok(());
    };
    if args.json {
        writeln!(
            out,
            "{}",
            serde_json::to_string(&serde_json::json!({
                "invoice": invoice_json(extracted),
                "candidates": response.candidates.iter().map(|candidate| {
                    serde_json::json!({
                        "part_id": candidate.part_id,
                        "filename": candidate.filename,
                        "kind": doc_kind_name(candidate.kind),
                    })
                }).collect::<Vec<_>>(),
                "used_model": response.used_model,
            }))?
        )?;
        return Ok(());
    }

    writeln!(
        out,
        "{} #{}  ({}{})",
        doc_kind_name(extracted.kind),
        extracted.invoice_id,
        if extracted.part_id.is_empty() {
            "body".to_owned()
        } else {
            format!("part {}", extracted.part_id)
        },
        if extracted.inferred {
            ", some fields inferred"
        } else {
            ""
        }
    )?;
    // Every line prints its origin. A total read off a labelled line and a
    // total a model recovered from a rendered page are different facts, and
    // hiding that to make the output tidy would be the bug.
    let mut field = |label: &str, value: String, provenance: Option<&FieldProvenance>| {
        if value.is_empty() {
            return Ok(());
        }
        writeln!(out, "  {label:<10} {value}{}", origin_note(provenance))
    };
    field(
        "vendor",
        extracted
            .vendor
            .as_ref()
            .map(|text| text.value.clone())
            .unwrap_or_default(),
        extracted
            .vendor
            .as_ref()
            .and_then(|text| text.provenance.as_ref()),
    )?;
    field(
        "number",
        extracted
            .number
            .as_ref()
            .map(|text| text.value.clone())
            .unwrap_or_default(),
        extracted
            .number
            .as_ref()
            .and_then(|text| text.provenance.as_ref()),
    )?;
    for (label, money) in [
        ("subtotal", extracted.subtotal.as_ref()),
        ("tax", extracted.tax.as_ref()),
        ("total", extracted.total.as_ref()),
    ] {
        field(
            label,
            money.map(render_money).unwrap_or_default(),
            money.and_then(|money| money.provenance.as_ref()),
        )?;
    }
    for (label, date) in [
        ("issued", extracted.issued_at.as_ref()),
        ("due", extracted.due_at.as_ref()),
    ] {
        field(
            label,
            date.map(|date| render_day(date.at)).unwrap_or_default(),
            date.and_then(|date| date.provenance.as_ref()),
        )?;
    }
    field(
        "status",
        status_name(extracted.status).to_owned(),
        extracted.status_provenance.as_ref(),
    )?;

    for item in &extracted.line_items {
        writeln!(
            out,
            "  - {}{}{}  [{}]",
            item.description,
            if item.has_quantity {
                format!(" x{}", item.quantity)
            } else {
                String::new()
            },
            item.total
                .as_ref()
                .map(|money| format!("  {}", render_money(money)))
                .unwrap_or_default(),
            origin_name(item.origin)
        )?;
    }
    for warning in &extracted.warnings {
        writeln!(out, "  ! {warning}")?;
    }
    for candidate in &response.candidates {
        if candidate.kind == InvoiceDocKind::Unspecified as i32 {
            writeln!(
                out,
                "  (considered {}: not a bill)",
                if candidate.part_id.is_empty() {
                    "the body".to_owned()
                } else {
                    format!("part {}", candidate.part_id)
                }
            )?;
        }
    }
    Ok(())
}

/// Run `mail invoices`.
///
/// # Errors
///
/// An unknown `--export`, an unparseable `--since`/`--until`, no daemon, a
/// failed RPC, or an unwritable stdout.
pub async fn invoices(socket: &Path, args: InvoicesArgs) -> Result<()> {
    let format = match args.export.as_deref().map(str::trim) {
        None | Some("") => InvoiceExportFormat::Rows,
        Some("csv") => InvoiceExportFormat::Csv,
        Some(other) => anyhow::bail!("unknown --export {other:?}; the only format is csv"),
    };
    let channel = connect(socket).await?;
    let response = AttachmentServiceClient::new(channel)
        .export_invoices(ExportInvoicesRequest {
            account_id: args.account.unwrap_or_default(),
            message_id: args.message.unwrap_or_default(),
            vendor: args.vendor.clone().unwrap_or_default(),
            since: parse_day(args.since.as_deref())?,
            until: parse_day(args.until.as_deref())?,
            limit: args.limit,
            format: format as i32,
        })
        .await
        .context("ExportInvoices RPC failed")?
        .into_inner();

    let mut out = std::io::stdout().lock();
    if format == InvoiceExportFormat::Csv {
        // Written verbatim, including its CRLF line endings: this is an RFC
        // 4180 document on its way to a file, not a rendering.
        write!(out, "{}", response.csv)?;
        return Ok(());
    }
    if response.invoices.is_empty() {
        writeln!(out, "no invoices have been extracted yet")?;
        return Ok(());
    }
    for extracted in &response.invoices {
        writeln!(
            out,
            "{:<6} {:<10} {:<24} {:<16} {:<12} {}{}",
            extracted.invoice_id,
            doc_kind_name(extracted.kind),
            extracted
                .vendor
                .as_ref()
                .map(|text| text.value.clone())
                .unwrap_or_else(|| "-".to_owned()),
            extracted
                .number
                .as_ref()
                .map(|text| text.value.clone())
                .unwrap_or_else(|| "-".to_owned()),
            extracted
                .total
                .as_ref()
                .map(render_money)
                .unwrap_or_else(|| "-".to_owned()),
            extracted
                .due_at
                .as_ref()
                .map(|date| render_day(date.at))
                .unwrap_or_else(|| "-".to_owned()),
            if extracted.inferred {
                "  (inferred)"
            } else {
                ""
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

// ---------------------------------------------------------------------------
// extract data
// ---------------------------------------------------------------------------

/// Run `mail extract data`.
///
/// # Errors
///
/// An unreadable or non-JSON `--schema-file`, no daemon, a failed RPC, or an
/// unwritable stdout.
pub async fn structured(socket: &Path, args: StructuredArgs) -> Result<()> {
    // Read and parsed here so a typo in a local file is a local error, rather
    // than a round trip that comes back INVALID_ARGUMENT.
    let schema_json = match args.schema_file.as_ref() {
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            serde_json::from_str::<serde_json::Value>(&text)
                .with_context(|| format!("{} is not valid JSON", path.display()))?;
            text
        }
        None => String::new(),
    };
    let channel = connect(socket).await?;
    let response = ExtractServiceClient::new(channel)
        .extract_structured(ExtractStructuredRequest {
            message_id: args.message_id,
            schema: args.schema.clone(),
            schema_json,
            refresh: args.refresh,
        })
        .await
        .context("ExtractStructured RPC failed")?
        .into_inner();

    let mut out = std::io::stdout().lock();
    // The stored document is already JSON and is the whole point of the verb,
    // so it is printed as JSON whatever the flags — but pretty-printed, since
    // a person is reading it.
    let data: serde_json::Value =
        serde_json::from_str(&response.data).unwrap_or(serde_json::Value::Null);
    writeln!(
        out,
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "extraction_id": response.extraction_id,
            "message_id": response.message_id,
            "schema": response.schema,
            "schema_hash": response.schema_hash,
            "model": response.model,
            "created_at": response.created_at,
            "cached": response.cached,
            "data": data,
        }))?
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Invoice rendering helpers
// ---------------------------------------------------------------------------

/// `12.34 GBP`, from integer minor units. Never a float: see the proto's own
/// note on `InvoiceMoney`.
fn render_money(money: &InvoiceMoney) -> String {
    // `rmail_core::extract::Money::display`, not a fourth formatter. Rendering
    // integer minor units as a decimal is the one thing this feature says must
    // never drift, and a private copy here is exactly how it would.
    rmail_core::extract::Money {
        currency: money.currency.clone(),
        minor_units: money.minor_units,
    }
    .display()
}

fn render_day(at: i64) -> String {
    stamp(at, true)
}

/// ` [model]` for an inferred field, nothing for a parsed one.
///
/// Silence for the parsed case on purpose: the annotation has to stand out on
/// the lines where it matters, and marking every line would make it furniture.
fn origin_note(provenance: Option<&FieldProvenance>) -> String {
    match provenance.map(|p| p.origin) {
        Some(origin) if origin == FieldOrigin::Model as i32 => "  [model]".to_owned(),
        _ => String::new(),
    }
}

fn origin_name(origin: i32) -> &'static str {
    match FieldOrigin::try_from(origin) {
        Ok(FieldOrigin::Parsed) => "parsed",
        Ok(FieldOrigin::Model) => "model",
        _ => "unknown",
    }
}

fn doc_kind_name(kind: i32) -> &'static str {
    match InvoiceDocKind::try_from(kind) {
        Ok(InvoiceDocKind::Invoice) => "invoice",
        Ok(InvoiceDocKind::Receipt) => "receipt",
        _ => "unknown",
    }
}

fn status_name(status: i32) -> &'static str {
    match InvoicePaymentStatus::try_from(status) {
        Ok(InvoicePaymentStatus::Paid) => "paid",
        Ok(InvoicePaymentStatus::Unpaid) => "unpaid",
        Ok(InvoicePaymentStatus::Overdue) => "overdue",
        Ok(InvoicePaymentStatus::Refunded) => "refunded",
        Ok(InvoicePaymentStatus::Void) => "void",
        // Includes UNSPECIFIED, which means the document said nothing. An
        // empty string so the caller's `field` helper skips the line rather
        // than printing `status unknown`, which reads as a failed parse.
        _ => "",
    }
}

/// UTC midnight of a `YYYY-MM-DD` day, or 0 for "no bound".
fn parse_day(day: Option<&str>) -> Result<i64> {
    let Some(day) = day.map(str::trim).filter(|day| !day.is_empty()) else {
        return Ok(0);
    };
    let parsed = chrono::NaiveDate::parse_from_str(day, "%Y-%m-%d")
        .with_context(|| format!("{day:?} is not a YYYY-MM-DD day"))?;
    parsed
        .and_hms_opt(0, 0, 0)
        .map(|at| at.and_utc().timestamp())
        .context("that day has no midnight")
}

fn invoice_json(extracted: &ExtractedInvoice) -> serde_json::Value {
    let money = |money: Option<&InvoiceMoney>| {
        money.map(|money| {
            serde_json::json!({
                "currency": money.currency,
                "minor_units": money.minor_units,
                "origin": origin_name(money.provenance.as_ref().map_or(0, |p| p.origin)),
            })
        })
    };
    let text = |text: Option<&InvoiceText>| {
        text.map(|text| {
            serde_json::json!({
                "value": text.value,
                "origin": origin_name(text.provenance.as_ref().map_or(0, |p| p.origin)),
            })
        })
    };
    serde_json::json!({
        "invoice_id": extracted.invoice_id,
        "message_id": extracted.message_id,
        "part_id": extracted.part_id,
        "kind": doc_kind_name(extracted.kind),
        "vendor": text(extracted.vendor.as_ref()),
        "number": text(extracted.number.as_ref()),
        "currency": extracted.currency,
        "subtotal": money(extracted.subtotal.as_ref()),
        "tax": money(extracted.tax.as_ref()),
        "total": money(extracted.total.as_ref()),
        "issued_at": extracted.issued_at.as_ref().map(|date| date.at),
        "due_at": extracted.due_at.as_ref().map(|date| date.at),
        "status": status_name(extracted.status),
        "line_items": extracted.line_items.iter().map(|item| serde_json::json!({
            "description": item.description,
            "quantity": item.has_quantity.then_some(item.quantity),
            "unit_price": money(item.unit_price.as_ref()),
            "total": money(item.total.as_ref()),
            "origin": origin_name(item.origin),
        })).collect::<Vec<_>>(),
        "warnings": extracted.warnings,
        "inferred": extracted.inferred,
        "extracted_at": extracted.extracted_at,
    })
}

async fn connect(socket: &Path) -> Result<tonic::transport::Channel> {
    rmail_core::connect_uds(socket)
        .await
        .with_context(|| format!("connecting to rmaild at {}", socket.display()))
}
