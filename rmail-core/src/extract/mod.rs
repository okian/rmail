//! Structured extraction: pulling tables, calendar items and links out of a
//! message and its attachments (prd.md #54, #65, #66; task 75).
//!
//! Three extractors that look unrelated and are not. Each one takes text a
//! stranger wrote, finds a structure inside it, and hands that structure to
//! something that will act on it — a spreadsheet a user reads as data, a
//! calendar entry that goes into their week, a link they click. That is the
//! same trust boundary three times, so the rules are shared:
//!
//! - **Bound the work explicitly.** Input size, rows, cells, links, components,
//!   nesting depth, attendees. Every bound is a named constant with a reason,
//!   and every bound has a test that reaches it.
//! - **Malformed input is a status, never a panic** and never an unbounded
//!   scan. One broken `VEVENT` costs that component, not the file.
//! - **Say where it came from.** [`tables::CellSource`], [`links::LinkSource`]
//!   and [`events::Event::source`] exist so a consumer can check a claim
//!   against the message, and so an inferred fact never passes for a read one.
//! - **Never resolve, never fetch, never execute.** See [`links`]' module docs
//!   on why a link found in mail is not a link this daemon may follow.
//!
//! # What existed before this module
//!
//! A good deal, and none of it is duplicated here:
//!
//! - [`crate::index::entities`] already finds URLs, dates, amounts, references
//!   and tracking numbers with byte spans under an entity budget. [`links`]
//!   calls it for the plain-text half of a message rather than writing a second
//!   URL regex; what it adds is the HTML half, where a link has a target *and*
//!   a display text and the gap between them is the phishing case.
//! - [`crate::attach::extract`] already turns attachment bytes into text with
//!   page spans, encoding detection and hard bounds. [`tables`] uses it as the
//!   input to its model route and never re-opens a PDF; what it adds is the
//!   native route, reading a workbook as a grid rather than as a bag of words.
//! - [`crate::attach::ocr`] already recognizes text on an image. The image
//!   table route runs it rather than inventing a second one.
//! - [`crate::hooks::run_hook`] already runs an operator's command with a
//!   payload on stdin, bounded and killable. [`events`]' pipe sink is that
//!   function, not a second process runner.
//! - [`crate::ai::gate`], [`crate::ai::injection`], [`crate::ai::redact`] and
//!   [`crate::ai::audit`] are the model plumbing. [`model`] is a thin
//!   composition of them, and it is the only place in this module that reaches
//!   a provider.
//!
//! # Where the model is, and where it is not
//!
//! Deterministic first, everywhere it is possible. A spreadsheet, a CSV, an
//! HTML table, an `.ics` and a `<a href>` are all *parsed*; only three things
//! are inferred, and each is marked as inferred on the way out:
//!
//! | inferred | why there is no deterministic route |
//! |---|---|
//! | tables in a PDF or a scan | the rows are a visual arrangement, not a structure in the file |
//! | events and tasks in a body | "Thursday at 3" is prose |
//! | a link's *purpose* | a rule can see `zoom.us`; only a reader can see which link the message is actually about |
//!
//! Note what the third row does *not* say: the deterministic classifier runs
//! first and always, and the model refines its answer. A daemon with no API key
//! still gets a ranked link picker, still gets every `.ics` event, and still
//! gets every spreadsheet table — less well, which is this project's standing
//! rule for what "AI off" means.
//!
//! ## PDF and image tables read text, not pixels
//!
//! prd.md #54 says "Claude vision" for these. What ships here is a model pass
//! over the document's *extracted text* (with `[page N]` markers) and, for an
//! image, over its OCR output. The honest reason is the same one
//! [`crate::attach::ocr`] gives for only rasterizing page one: this crate has
//! no PDF renderer, and [`crate::ai::provider`] carries no image content block
//! — adding both is real, separate scope, and bolting them on unverified is
//! how a subtly wrong bitmap ships. A ruled table survives text extraction with
//! its rows intact often enough for this route to be worth having, and
//! [`tables::TableOrigin::is_inferred`] tells a consumer exactly what it is
//! looking at either way.

pub mod events;
pub mod invoice;
pub mod links;
pub mod model;
pub mod store;
pub mod structured;
pub mod tables;

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::attach::extract::{Extracted, Format, Status};
use crate::config::ExtractConfig;
use crate::error::Error;
use crate::storage::Database;

pub use events::{
    CalendarReport, Delivery, DeliveryReport, Event, Sink, Source as EventSource, Task,
};
pub use invoice::{
    Claim, DocKind, Invoice, InvoiceReport, LineItem, Money, Origin, PaymentStatus, Provenance,
    StoredInvoice,
};
pub use links::{Link, LinkKind, LinkPart, LinkReport};
pub use model::{Ask, ExtractModel};
pub use store::InvoiceFilter;
pub use structured::{Extraction, StructuredReport};
pub use tables::{Cell, CellSource, CellType, CellValue, Column, Table, TableOrigin, TableReport};

/// How much of a document's text reaches the model on the table route.
///
/// Deliberately tighter than `ai.privacy.max_body_chars`: a table transcription
/// is decided by the pages the table is on, and the whole document would spend
/// the budget on prose.
const MAX_DOCUMENT_CHARS: usize = 24_000;

/// How much of a message body reaches the model on the calendar route.
const MAX_BODY_CHARS: usize = 8_000;

/// How many attachments the invoice detector will open when the caller named
/// no part.
///
/// Each one costs a full text extraction — a PDF parse, a workbook walk — so
/// the count is the cost of an unnamed request, and it has to be small enough
/// that a mail with forty attachments is not a way to make one RPC do forty
/// documents' work.
const MAX_CANDIDATE_PARTS: usize = 4;

/// The one place the three extractors meet the database, the message store and
/// the model.
///
/// Cheap to clone — every field is a handle. `model` is `None` on a daemon with
/// no provider configured, and every route degrades rather than failing: see
/// the module docs.
#[derive(Debug, Clone)]
pub struct ExtractEngine {
    db: Database,
    model: Option<Arc<ExtractModel>>,
    config: ExtractConfig,
}

/// One message's scope, for policy and audit.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Scope {
    account_id: i64,
    mailbox: Option<String>,
}

impl ExtractEngine {
    /// Build an engine.
    #[must_use]
    pub fn new(db: Database, model: Option<Arc<ExtractModel>>, config: ExtractConfig) -> Self {
        Self { db, model, config }
    }

    /// Whether a model route is available at all.
    #[must_use]
    pub fn has_model(&self) -> bool {
        self.model.is_some()
    }

    // -----------------------------------------------------------------------
    // Tables
    // -----------------------------------------------------------------------

    /// Extract tables from one attachment.
    ///
    /// Native for a workbook, delimited text or HTML; a model pass over the
    /// extracted text for a PDF, and over OCR output for an image. `use_model`
    /// false confines this to the native routes, which is what a caller who
    /// must not spend anything asks for.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] if the message or the part does not exist;
    /// [`Error::FailedPrecondition`] if the message has no stored body, or the
    /// part is a format with no text and no route to one;
    /// [`Error::InvalidArgument`] for a format nothing here can read, or a
    /// model route asked for on a daemon with no model.
    #[tracing::instrument(skip(self, cancel), fields(tables, origin))]
    pub async fn tables(
        &self,
        message_id: i64,
        part_id: &str,
        use_model: bool,
        cancel: &CancellationToken,
    ) -> Result<TableReport, Error> {
        let scope = self.scope(message_id).await?;
        let raw = self.raw(message_id).await?;
        let part_id_owned = part_id.to_owned();
        let part = tokio::task::spawn_blocking(move || attachment_part(&raw, &part_id_owned))
            .await
            .map_err(|e| Error::internal(format!("attachment decode task failed: {e}")))?
            .ok_or_else(|| {
                Error::not_found(format!("message {message_id} has no part {part_id}"))
            })?;

        let format = crate::attach::extract::detect(
            &part.bytes,
            part.filename.as_deref(),
            part.content_type.as_deref(),
        );
        let span = tracing::Span::current();
        let report = match format {
            Some(Format::Xlsx) => {
                span.record("origin", "spreadsheet");
                tables::from_xlsx(part.bytes).await?
            }
            Some(Format::Csv) => {
                span.record("origin", "csv");
                let name = part
                    .filename
                    .clone()
                    .unwrap_or_else(|| "Table 1".to_owned());
                let bytes = part.bytes;
                // On the blocking pool, like every other parser here. Decoding
                // is proportional to the attachment and `from_csv` walks every
                // byte; a 4 MB export would otherwise hold a runtime worker
                // for the duration, which CLAUDE.md forbids outright.
                //
                // Decoded here rather than through `attach::extract`, which
                // *normalizes* whitespace on the way out — correct for an
                // index, fatal for a grid: it collapses every record onto one
                // line, and a three-column CSV came back as a single row of
                // seven fields. The encoding guess is the same one, for the
                // same reason (a Windows-1252 export from an accounting
                // package is an ordinary thing to receive).
                blocking(move || {
                    let text = decode_text(&bytes);
                    if text.trim().is_empty() {
                        return Err(Error::failed_precondition(
                            "this attachment yielded no text to read tables from (status empty)"
                                .to_owned(),
                        ));
                    }
                    tables::from_csv(&name, &text)
                })
                .await?
            }
            Some(Format::Html) => {
                span.record("origin", "html");
                let bytes = part.bytes;
                // The *source*, not the stripped text: `attach::extract`'s HTML
                // route runs the markup through the same stripper the bodies
                // use, which is exactly what destroys the table.
                blocking(move || tables::from_html(&decode_text(&bytes))).await?
            }
            Some(Format::Pdf) => {
                span.record("origin", "model");
                // Checked *before* the PDF is parsed. Running the extraction
                // and then refusing meant a caller who had explicitly declined
                // the model route still paid for the parse, once per request.
                self.require_model(use_model)?;
                let (status, text) =
                    crate::attach::extract::extract(Format::Pdf, part.bytes).await?;
                Self::require_text(status, &text)?;
                self.model_tables(&scope, message_id, &paginate(&text), cancel, use_model)
                    .await?
            }
            _ if crate::attach::ocr::is_image(&part.bytes) => {
                span.record("origin", "model");
                // Same reasoning, and it matters more here: OCR is a
                // subprocess with a deadline, and a `mail.read`-only caller
                // could force one per image attachment for an answer that was
                // always going to be a refusal.
                self.require_model(use_model)?;
                let text = self.ocr_text(part.bytes).await?;
                self.model_tables(&scope, message_id, &text, cancel, use_model)
                    .await?
            }
            Some(other) => {
                return Err(Error::invalid_argument(format!(
                    "{} attachments carry no tables this build can read",
                    other.as_str()
                )))
            }
            None => {
                return Err(Error::invalid_argument(
                    "this attachment is not a format tables can be read from".to_owned(),
                ))
            }
        };
        span.record("tables", report.tables.len());
        Ok(report)
    }

    /// A format that legitimately produced nothing is a precondition failure,
    /// not an empty answer: a caller must be able to tell "this document has no
    /// tables" from "this document has no text at all".
    fn require_text(status: Status, text: &Extracted) -> Result<(), Error> {
        if status != Status::Ok || text.text.trim().is_empty() {
            return Err(Error::failed_precondition(format!(
                "this attachment yielded no text to read tables from (status {})",
                status.as_str()
            )));
        }
        Ok(())
    }

    /// Recognize text on an image, or say why not.
    async fn ocr_text(&self, bytes: Vec<u8>) -> Result<String, Error> {
        match crate::attach::ocr::recognize(bytes, self.config.ocr_langs.clone()).await? {
            crate::attach::ocr::ChainOutcome::Recognized(_, output)
                if !output.text.trim().is_empty() =>
            {
                Ok(output.text)
            }
            crate::attach::ocr::ChainOutcome::Recognized(_, _) => Err(Error::failed_precondition(
                "no text was recognized on this image".to_owned(),
            )),
            crate::attach::ocr::ChainOutcome::Unavailable => Err(Error::failed_precondition(
                "no OCR backend is available, so an image's tables cannot be read".to_owned(),
            )),
            crate::attach::ocr::ChainOutcome::Failed(_, why) => Err(Error::unavailable(format!(
                "OCR failed on this image: {why}"
            ))),
        }
    }

    /// The model route for tables.
    async fn model_tables(
        &self,
        scope: &Scope,
        message_id: i64,
        document: &str,
        cancel: &CancellationToken,
        use_model: bool,
    ) -> Result<TableReport, Error> {
        let model = self.require_model(use_model)?;
        let mut text = document.to_owned();
        if let Some((index, _)) = text.char_indices().nth(MAX_DOCUMENT_CHARS) {
            text.truncate(index);
        }
        let answer = model
            .ask(
                &Ask {
                    system: tables::TABLE_SYSTEM_PROMPT,
                    instruction: "Transcribe every table in the document below.".to_owned(),
                    untrusted: vec![("document", text)],
                    schema: tables::table_schema(),
                    max_tokens: self.config.max_tokens,
                    account_id: scope.account_id,
                    mailbox: scope.mailbox.clone(),
                    message_id: Some(message_id),
                },
                cancel,
            )
            .await?;
        tables::from_model_answer(&answer)
    }

    // -----------------------------------------------------------------------
    // Calendar
    // -----------------------------------------------------------------------

    /// Extract events and tasks from a message and any `.ics` it carries.
    ///
    /// The `.ics` route is deterministic and always runs. The model route runs
    /// over the body when `use_model` is set and a model is configured, and its
    /// items are marked [`events::Source::Model`].
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] if the message does not exist;
    /// [`Error::FailedPrecondition`] if it has no stored body;
    /// [`Error::InvalidArgument`] if a model route is asked for and none is
    /// configured. A malformed `.ics` part is skipped, not fatal — see
    /// [`events::parse_ics`].
    #[tracing::instrument(skip(self, cancel), fields(events, tasks))]
    pub async fn calendar(
        &self,
        message_id: i64,
        use_model: bool,
        cancel: &CancellationToken,
    ) -> Result<CalendarReport, Error> {
        let scope = self.scope(message_id).await?;
        let raw = self.raw(message_id).await?;
        let decoded = tokio::task::spawn_blocking(move || decode_message(&raw))
            .await
            .map_err(|e| Error::internal(format!("message decode task failed: {e}")))?;

        let mut report = CalendarReport {
            events: Vec::new(),
            tasks: Vec::new(),
            method: String::new(),
            skipped: 0,
        };
        for calendar in &decoded.calendars {
            match events::parse_ics(calendar) {
                Ok(parsed) => {
                    if report.method.is_empty() {
                        report.method = parsed.method;
                    }
                    report.skipped += parsed.skipped;
                    report.events.extend(parsed.events);
                    report.tasks.extend(parsed.tasks);
                }
                Err(error) => {
                    tracing::debug!(%error, message_id, "a calendar part did not parse");
                    report.skipped += 1;
                }
            }
        }
        // An `.ics` may omit `UID`, and the idempotency table keys on it.
        for event in &mut report.events {
            if event.uid.trim().is_empty() {
                event.uid = events::synthesize_uid(
                    message_id,
                    "event",
                    &event.summary,
                    Some(event.starts_at),
                );
            }
        }
        for task in &mut report.tasks {
            if task.uid.trim().is_empty() {
                task.uid = events::synthesize_uid(message_id, "task", &task.summary, task.due_at);
            }
        }

        if use_model && self.model.is_some() {
            let inferred = self
                .model_calendar(&scope, message_id, &decoded, cancel)
                .await?;
            report.skipped += inferred.skipped;
            // The `.ics` is authoritative, and an inferred item that restates
            // one is dropped. The join is on the *instant*, not on the UID: an
            // `.ics` event carries the sender's own UID and an inferred one
            // carries a hash of its summary and start, so those two identities
            // can never collide and a UID-only filter — which is what this was
            // — silently let a model put the same meeting in the calendar
            // twice. Two events at the same second in one message are the same
            // event; a message that really does contain two distinct meetings
            // starting simultaneously is not a case worth double-booking for.
            let starts: std::collections::BTreeSet<i64> =
                report.events.iter().map(|e| e.starts_at).collect();
            let uids: std::collections::BTreeSet<String> =
                report.events.iter().map(|e| e.uid.clone()).collect();
            let mut duplicates = 0usize;
            report
                .events
                .extend(inferred.events.into_iter().filter(|e| {
                    let novel = !uids.contains(&e.uid) && !starts.contains(&e.starts_at);
                    if !novel {
                        duplicates += 1;
                    }
                    novel
                }));
            // Tasks join on due date the same way, with one difference: a task
            // with no due date has no instant to join on, so it is compared by
            // its (case-folded) summary instead. "Send the deck" from the
            // `.ics` and "send the deck" from the body are one task.
            let dues: std::collections::BTreeSet<i64> =
                report.tasks.iter().filter_map(|t| t.due_at).collect();
            let summaries: std::collections::BTreeSet<String> = report
                .tasks
                .iter()
                .map(|t| t.summary.trim().to_lowercase())
                .collect();
            let uids: std::collections::BTreeSet<String> =
                report.tasks.iter().map(|t| t.uid.clone()).collect();
            report.tasks.extend(inferred.tasks.into_iter().filter(|t| {
                let novel = !uids.contains(&t.uid)
                    && !t.due_at.is_some_and(|due| dues.contains(&due))
                    && !summaries.contains(&t.summary.trim().to_lowercase());
                if !novel {
                    duplicates += 1;
                }
                novel
            }));
            if duplicates > 0 {
                tracing::debug!(
                    message_id,
                    duplicates,
                    "dropped inferred items the .ics already stated"
                );
            }
        } else if use_model {
            return Err(Error::invalid_argument(
                "no AI provider is configured, so events cannot be inferred from a body; \
                 ask for the .ics route instead"
                    .to_owned(),
            ));
        }

        let span = tracing::Span::current();
        span.record("events", report.events.len());
        span.record("tasks", report.tasks.len());
        Ok(report)
    }

    /// The model route for calendar items.
    async fn model_calendar(
        &self,
        scope: &Scope,
        message_id: i64,
        decoded: &Decoded,
        cancel: &CancellationToken,
    ) -> Result<CalendarReport, Error> {
        let model = self.require_model(true)?;
        let mut body = decoded.body.clone();
        if let Some((index, _)) = body.char_indices().nth(MAX_BODY_CHARS) {
            body.truncate(index);
        }
        let answer = model
            .ask(
                &Ask {
                    system: events::CALENDAR_SYSTEM_PROMPT,
                    instruction: format!(
                        "Today is {}. Extract the events and tasks stated in the email below.",
                        chrono::Utc::now().format("%Y-%m-%d")
                    ),
                    untrusted: vec![("subject", decoded.subject.clone()), ("email", body)],
                    schema: events::calendar_schema(),
                    max_tokens: self.config.max_tokens,
                    account_id: scope.account_id,
                    mailbox: scope.mailbox.clone(),
                    message_id: Some(message_id),
                },
                cancel,
            )
            .await?;
        events::from_model_answer(message_id, &answer)
    }

    /// Deliver extracted items to a sink, idempotently per message.
    ///
    /// # Errors
    ///
    /// Whatever [`Delivery::deliver`] returns.
    pub async fn deliver_events(
        &self,
        message_id: i64,
        events: &[Event],
        sink: &Sink,
        cancel: &CancellationToken,
    ) -> Result<DeliveryReport, Error> {
        let uids: Vec<String> = events.iter().map(|event| event.uid.clone()).collect();
        // A renderer rather than a rendered string: `deliver` needs the file
        // for *what it claimed* as well as the file for everything, and
        // handing it one string meant already-delivered events were pushed to
        // the sink a second time. See `Delivery::deliver`.
        let render = |wanted: &[String]| {
            let selected: Vec<Event> = events
                .iter()
                .filter(|event| wanted.contains(&event.uid))
                .cloned()
                .collect();
            events::events_to_ics(&selected)
        };
        Delivery {
            db: &self.db,
            message_id,
        }
        .deliver("event", &uids, &render, sink, cancel)
        .await
    }

    /// [`Self::deliver_events`] for tasks.
    ///
    /// # Errors
    ///
    /// Whatever [`Delivery::deliver`] returns.
    pub async fn deliver_tasks(
        &self,
        message_id: i64,
        tasks: &[Task],
        sink: &Sink,
        cancel: &CancellationToken,
    ) -> Result<DeliveryReport, Error> {
        let uids: Vec<String> = tasks.iter().map(|task| task.uid.clone()).collect();
        let render = |wanted: &[String]| {
            let selected: Vec<Task> = tasks
                .iter()
                .filter(|task| wanted.contains(&task.uid))
                .cloned()
                .collect();
            events::tasks_to_ics(&selected)
        };
        Delivery {
            db: &self.db,
            message_id,
        }
        .deliver("task", &uids, &render, sink, cancel)
        .await
    }

    /// The sink an operator configured, or [`Sink::Ics`] when they configured
    /// none.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] for a sink name this build does not know, or
    /// one the configuration has not been given the details for — an
    /// unconfigured webhook must fail loudly rather than silently degrade to
    /// returning a file, or a user would believe their task tracker had been
    /// updated.
    pub fn sink(&self, name: &str) -> Result<Sink, Error> {
        match name {
            "" | "ics" => Ok(Sink::Ics),
            "command" => {
                if self.config.command.trim().is_empty() {
                    return Err(Error::invalid_argument(
                        "extract.command is not configured, so there is nothing to pipe to"
                            .to_owned(),
                    ));
                }
                Ok(Sink::Command {
                    command: self.config.command.clone(),
                    args: self.config.command_args.clone(),
                })
            }
            "webhook" => {
                if self.config.webhook_url.trim().is_empty() {
                    return Err(Error::invalid_argument(
                        "extract.webhook_url is not configured, so there is nowhere to POST"
                            .to_owned(),
                    ));
                }
                Ok(Sink::Webhook {
                    url: self.config.webhook_url.clone(),
                })
            }
            other => Err(Error::invalid_argument(format!(
                "unknown extraction sink {other:?}; expected ics, command or webhook"
            ))),
        }
    }

    // -----------------------------------------------------------------------
    // Links
    // -----------------------------------------------------------------------

    /// Extract, deduplicate, classify and rank every link in a message.
    ///
    /// The deterministic classifier always runs. When `use_model` is set and a
    /// model is configured, its answer refines the classification and the
    /// score, and the refined links are marked [`links::Classifier::Model`] —
    /// a model failure is logged and the deterministic answer stands, because a
    /// picker that vanishes when an API key expires is worse than one that
    /// ranks slightly less well.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] if the message does not exist;
    /// [`Error::FailedPrecondition`] if it has no stored body.
    #[tracing::instrument(skip(self, cancel), fields(links, deceptive))]
    pub async fn links(
        &self,
        message_id: i64,
        use_model: bool,
        cancel: &CancellationToken,
    ) -> Result<LinkReport, Error> {
        let scope = self.scope(message_id).await?;
        let raw = self.raw(message_id).await?;
        let decoded = tokio::task::spawn_blocking(move || decode_message(&raw))
            .await
            .map_err(|e| Error::internal(format!("message decode task failed: {e}")))?;

        let parts: Vec<LinkPart> = decoded
            .html
            .iter()
            .enumerate()
            .map(|(index, text)| LinkPart {
                part: format!("html:{index}"),
                text: text.clone(),
                html: true,
            })
            .chain(
                decoded
                    .text
                    .iter()
                    .enumerate()
                    .map(|(index, text)| LinkPart {
                        part: format!("text:{index}"),
                        text: text.clone(),
                        html: false,
                    }),
            )
            .collect();
        let mut report = links::extract_links(&parts, &decoded.unsubscribe);

        if use_model && !report.links.is_empty() {
            if let Some(model) = self.model.as_ref() {
                match self
                    .model_links(model, &scope, message_id, &decoded.subject, &report, cancel)
                    .await
                {
                    Ok(refined) => report = refined,
                    Err(error) => {
                        // Degrade, do not fail: see this method's docs.
                        tracing::warn!(%error, message_id, "link classification fell back to rules");
                    }
                }
            }
        }

        let span = tracing::Span::current();
        span.record("links", report.links.len());
        span.record(
            "deceptive",
            report.links.iter().filter(|link| link.deceptive).count(),
        );
        Ok(report)
    }

    /// The model route for link classification.
    async fn model_links(
        &self,
        model: &ExtractModel,
        scope: &Scope,
        message_id: i64,
        subject: &str,
        report: &LinkReport,
        cancel: &CancellationToken,
    ) -> Result<LinkReport, Error> {
        let listing = links::model_listing(&report.links, links::MAX_LINKS_TO_MODEL);
        let answer = model
            .ask(
                &Ask {
                    system: links::LINK_SYSTEM_PROMPT,
                    instruction: format!(
                        "Classify each numbered link. The vocabulary is: {}.",
                        LinkKind::ALL
                            .iter()
                            .map(|kind| kind.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    untrusted: vec![("subject", subject.to_owned()), ("links", listing)],
                    schema: links::link_schema(),
                    max_tokens: self.config.max_tokens,
                    account_id: scope.account_id,
                    mailbox: scope.mailbox.clone(),
                    message_id: Some(message_id),
                },
                cancel,
            )
            .await?;
        links::apply_model_answer(report.clone(), &answer)
    }

    // -----------------------------------------------------------------------
    // Invoices and receipts (task 73)
    // -----------------------------------------------------------------------

    /// Detect and extract an invoice or receipt, and store what was found.
    ///
    /// `part_id` names one attachment; `None` detects across the message's
    /// document attachments and falls back to the body. The deterministic
    /// reader always runs; `use_model` adds a model pass whose fields are
    /// marked [`invoice::Origin::Model`] and which never overrides a value the
    /// document stated in words — see [`invoice::merge`].
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] if the message or the named part does not exist;
    /// [`Error::FailedPrecondition`] if the message has no stored body, if no
    /// part of it yielded readable text, or if nothing invoice-shaped could be
    /// read out of the text there was; [`Error::InvalidArgument`] if a model
    /// pass is asked for on a daemon with no provider.
    #[tracing::instrument(skip(self, cancel), fields(part, kind, inferred))]
    pub async fn invoice(
        &self,
        message_id: i64,
        part_id: Option<&str>,
        use_model: bool,
        cancel: &CancellationToken,
    ) -> Result<InvoiceReport, Error> {
        let scope = self.scope(message_id).await?;
        let raw = self.raw(message_id).await?;
        let wanted = part_id.map(str::to_owned);
        let (parts, decoded) =
            tokio::task::spawn_blocking(move || (attachment_parts(&raw), decode_message(&raw)))
                .await
                .map_err(|e| Error::internal(format!("message decode task failed: {e}")))?;

        let mut candidates = Vec::new();
        let (kind, part, text, tables) = match wanted {
            Some(wanted) => {
                // Resolved by index rather than out of `parts`: a caller who
                // named one attachment must not pay to have all sixty-four
                // decoded into memory, and this is the same positional
                // identity `ExtractTables` uses.
                let part = parts
                    .iter()
                    .find(|(id, _)| *id == wanted)
                    .map(|(_, part)| part)
                    .ok_or_else(|| {
                        Error::not_found(format!("message {message_id} has no part {wanted}"))
                    })?;
                let (text, tables) = self.document(part).await?;
                let kind = invoice::detect(
                    part.filename.as_deref(),
                    part.content_type.as_deref(),
                    &text,
                );
                candidates.push(invoice::Candidate {
                    part: wanted.clone(),
                    filename: part.filename.clone().unwrap_or_default(),
                    kind,
                });
                // A caller who named a part is telling this daemon what to
                // read. Declining because the detector disagreed would make
                // the explicit request weaker than the automatic one; the
                // disagreement is recorded as a warning instead.
                (kind.unwrap_or(DocKind::Invoice), wanted, text, tables)
            }
            None => {
                self.detect_invoice(&parts, &decoded, &mut candidates)
                    .await?
            }
        };

        let span = tracing::Span::current();
        span.record("part", part.as_str());
        span.record("kind", kind.as_str());

        // On the blocking pool, like every other parser in this module. The
        // line loop is `MAX_LABELS_PER_LINE` label scans over each of
        // `MAX_LINES` lines of up to `MAX_DOCUMENT_BYTES`, plus a
        // `to_lowercase()` and an `entities::scan` per line — hundreds of
        // milliseconds of CPU on an attachment a stranger chose, which
        // CLAUDE.md forbids holding a runtime worker for.
        let (document, part_owned) = (text.clone(), part.clone());
        let mut parsed = blocking(move || {
            let mut parsed = invoice::parse_document(kind, &part_owned, &document);
            if parsed.line_items.is_empty() {
                for table in &tables {
                    let items = invoice::line_items_from_table(table, parsed.currency.as_deref());
                    if !items.is_empty() {
                        parsed.line_items = items;
                        break;
                    }
                }
            }
            Ok(parsed)
        })
        .await?;
        if candidates
            .last()
            .is_some_and(|candidate| candidate.kind.is_none())
        {
            parsed.warnings.push(
                "this document does not read as an invoice or a receipt; it was extracted \
                 because it was named explicitly"
                    .to_owned(),
            );
        }

        let mut used_model = false;
        if use_model {
            let inferred = self
                .model_invoice(&scope, message_id, kind, &part, &text, cancel)
                .await?;
            parsed = invoice::merge(parsed, inferred);
            used_model = true;
        }
        span.record("inferred", parsed.inferred());

        let stored = store::save_invoice(&self.db, message_id, &parsed).await?;
        Ok(InvoiceReport {
            stored,
            candidates,
            used_model,
        })
    }

    /// Pick the part to read when the caller named none.
    ///
    /// Bounded at [`MAX_CANDIDATE_PARTS`] documents, and images are not
    /// considered: OCR is a subprocess with a deadline, and running one per
    /// image attachment on the chance that a scan is an invoice is a cost a
    /// caller has to ask for by naming the part.
    async fn detect_invoice(
        &self,
        parts: &[(String, AttachmentPart)],
        decoded: &Decoded,
        candidates: &mut Vec<invoice::Candidate>,
    ) -> Result<(DocKind, String, String, Vec<Table>), Error> {
        let mut ordered: Vec<&(String, AttachmentPart)> = parts
            .iter()
            .filter(|(_, part)| {
                crate::attach::extract::detect(
                    &part.bytes,
                    part.filename.as_deref(),
                    part.content_type.as_deref(),
                )
                .is_some()
            })
            .collect();
        // A filename that says `invoice` is a deliberate label and is read
        // first, so a five-attachment mail does not spend its budget on the
        // logo before reaching the bill.
        ordered.sort_by_key(|(_, part)| {
            let name = part.filename.clone().unwrap_or_default().to_lowercase();
            u8::from(!(name.contains("invoice") || name.contains("receipt")))
        });

        for (id, part) in ordered.into_iter().take(MAX_CANDIDATE_PARTS) {
            let (text, tables) = match self.document(part).await {
                Ok(document) => document,
                // One unreadable attachment costs that attachment, not the
                // request: a password-protected workbook next to a readable
                // PDF must not hide the PDF.
                Err(error) => {
                    tracing::debug!(%error, part = %id, "an attachment could not be read");
                    candidates.push(invoice::Candidate {
                        part: id.clone(),
                        filename: part.filename.clone().unwrap_or_default(),
                        kind: None,
                    });
                    continue;
                }
            };
            let kind = invoice::detect(
                part.filename.as_deref(),
                part.content_type.as_deref(),
                &text,
            );
            candidates.push(invoice::Candidate {
                part: id.clone(),
                filename: part.filename.clone().unwrap_or_default(),
                kind,
            });
            if let Some(kind) = kind {
                return Ok((kind, id.clone(), text, tables));
            }
        }

        // The body last, and it is a real answer rather than a fallback: a
        // hosted-billing "your receipt" mail keeps its numbers in the body and
        // attaches nothing at all.
        let kind = invoice::detect(None, None, &decoded.body);
        candidates.push(invoice::Candidate {
            part: String::new(),
            filename: String::new(),
            kind,
        });
        match kind {
            Some(kind) => Ok((kind, String::new(), decoded.body.clone(), Vec::new())),
            None => Err(Error::failed_precondition(
                "neither this message nor its attachments reads as an invoice or a receipt"
                    .to_owned(),
            )),
        }
    }

    /// One attachment's text, plus any tables it natively carries.
    ///
    /// The tables are the line-item route for a spreadsheet invoice: see
    /// [`invoice::line_items_from_table`].
    async fn document(&self, part: &AttachmentPart) -> Result<(String, Vec<Table>), Error> {
        let format = crate::attach::extract::detect(
            &part.bytes,
            part.filename.as_deref(),
            part.content_type.as_deref(),
        );
        match format {
            Some(Format::Xlsx) => {
                let report = tables::from_xlsx(part.bytes.clone()).await?;
                let (status, text) =
                    crate::attach::extract::extract(Format::Xlsx, part.bytes.clone()).await?;
                Self::require_document_text(status, &text)?;
                Ok((text.text, report.tables))
            }
            // Both routes decode the bytes directly rather than going through
            // `attach::extract`, which *normalizes* whitespace on the way out
            // — right for an index, fatal here: it collapses the whole
            // document onto one line, and an invoice is read by its lines.
            // `Total: 1,299.00` on its own line and the same words in the
            // middle of a 4 KB paragraph are not equally readable, and a
            // vendor label would swallow the rest of the file. The same
            // reasoning `tables`' CSV route already documents.
            Some(Format::Csv) => {
                let name = part
                    .filename
                    .clone()
                    .unwrap_or_else(|| "Table 1".to_owned());
                let bytes = part.bytes.clone();
                blocking(move || {
                    let text = decode_text(&bytes);
                    if text.trim().is_empty() {
                        return Err(Error::failed_precondition(
                            "this attachment yielded no text (status empty)".to_owned(),
                        ));
                    }
                    let report = tables::from_csv(&name, &text)?;
                    Ok((text, report.tables))
                })
                .await
            }
            Some(Format::Text) => {
                let bytes = part.bytes.clone();
                blocking(move || {
                    let text = decode_text(&bytes);
                    if text.trim().is_empty() {
                        return Err(Error::failed_precondition(
                            "this attachment yielded no text (status empty)".to_owned(),
                        ));
                    }
                    Ok((text, Vec::new()))
                })
                .await
            }
            Some(Format::Html) => {
                let bytes = part.bytes.clone();
                blocking(move || {
                    let source = decode_text(&bytes);
                    let report = tables::from_html(&source)?;
                    let text = crate::index::extract::strip_html(&source);
                    if text.trim().is_empty() {
                        return Err(Error::failed_precondition(
                            "this attachment yielded no text (status empty)".to_owned(),
                        ));
                    }
                    Ok((text, report.tables))
                })
                .await
            }
            Some(format) => {
                let (status, text) =
                    crate::attach::extract::extract(format, part.bytes.clone()).await?;
                Self::require_document_text(status, &text)?;
                Ok((paginate(&text), Vec::new()))
            }
            None if crate::attach::ocr::is_image(&part.bytes) => {
                Ok((self.ocr_text(part.bytes.clone()).await?, Vec::new()))
            }
            None => Err(Error::invalid_argument(
                "this attachment is not a format an invoice can be read from".to_owned(),
            )),
        }
    }

    /// [`Self::require_text`] with a message that names this feature.
    fn require_document_text(status: Status, text: &Extracted) -> Result<(), Error> {
        if status != Status::Ok || text.text.trim().is_empty() {
            return Err(Error::failed_precondition(format!(
                "this attachment yielded no text to read an invoice from (status {})",
                status.as_str()
            )));
        }
        Ok(())
    }

    /// The model route for invoices.
    async fn model_invoice(
        &self,
        scope: &Scope,
        message_id: i64,
        kind: DocKind,
        part: &str,
        document: &str,
        cancel: &CancellationToken,
    ) -> Result<Invoice, Error> {
        let model = self.require_model(true)?;
        let mut text = document.to_owned();
        if let Some((index, _)) = text.char_indices().nth(MAX_DOCUMENT_CHARS) {
            text.truncate(index);
        }
        let answer = model
            .ask(
                &Ask {
                    system: invoice::INVOICE_SYSTEM_PROMPT,
                    instruction: "Extract the invoice or receipt stated in the document below."
                        .to_owned(),
                    untrusted: vec![("document", text)],
                    schema: invoice::invoice_schema(),
                    max_tokens: self.config.max_tokens,
                    account_id: scope.account_id,
                    mailbox: scope.mailbox.clone(),
                    message_id: Some(message_id),
                },
                cancel,
            )
            .await?;
        invoice::from_model_answer(kind, part, &answer)
    }

    /// Read stored invoices.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    pub async fn list_invoices(&self, filter: &InvoiceFilter) -> Result<Vec<StoredInvoice>, Error> {
        store::list_invoices(&self.db, filter).await
    }

    // -----------------------------------------------------------------------
    // General structured extraction (task 73)
    // -----------------------------------------------------------------------

    /// Extract one message against a JSON schema, validate the answer, and
    /// store it.
    ///
    /// `schema` is a built-in's name ([`structured::BUILTINS`]) or, with
    /// `custom` supplied alongside, a caller's own schema. `refresh` re-runs a
    /// message that has already been extracted under the same schema; without
    /// it the stored document is returned and no tokens are spent.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] for a message that does not exist;
    /// [`Error::FailedPrecondition`] for one with no stored body, or on a
    /// daemon with no AI provider — there is no deterministic route here, so an
    /// empty document would be a lie rather than a degraded answer;
    /// [`Error::InvalidArgument`] for an unknown schema name or a supplied
    /// schema this build cannot enforce; [`Error::Internal`] if the answer does
    /// not satisfy the schema.
    #[tracing::instrument(skip(self, custom, cancel), fields(schema, cached))]
    pub async fn structured(
        &self,
        message_id: i64,
        schema: &str,
        custom: Option<serde_json::Value>,
        refresh: bool,
        cancel: &CancellationToken,
    ) -> Result<StructuredReport, Error> {
        let name = schema.trim();
        let (name, schema) = match custom {
            Some(custom) => {
                structured::check_schema(&custom)?;
                (structured::CUSTOM.to_owned(), custom)
            }
            None => {
                let builtin = structured::schema_for(name).ok_or_else(|| {
                    Error::invalid_argument(format!(
                        "unknown schema {name:?}; this build knows {}",
                        structured::BUILTINS
                            .iter()
                            .map(|schema| schema.name)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                })?;
                (name.to_owned(), builtin)
            }
        };
        let hash = structured::schema_hash(&schema);
        let span = tracing::Span::current();
        span.record("schema", name.as_str());

        if !refresh {
            if let Some(existing) =
                store::find_extraction(&self.db, message_id, &name, &hash).await?
            {
                span.record("cached", true);
                return Ok(StructuredReport {
                    extraction: existing,
                    cached: true,
                });
            }
        }
        span.record("cached", false);

        // Checked before anything is fetched or decoded. Running the whole
        // MIME decode and *then* refusing meant a provider-less daemon paid for
        // the parse once per request, for an answer that was always a refusal —
        // the same ordering mistake `tables()` documents fixing.
        let model = self.model.as_deref().ok_or_else(|| {
            Error::failed_precondition(
                "no AI provider is configured, so a message cannot be read against a schema"
                    .to_owned(),
            )
        })?;
        let scope = self.scope(message_id).await?;
        let raw = self.raw(message_id).await?;
        let decoded = tokio::task::spawn_blocking(move || decode_message(&raw))
            .await
            .map_err(|e| Error::internal(format!("message decode task failed: {e}")))?;

        let mut body = decoded.body.clone();
        if let Some((index, _)) = body.char_indices().nth(MAX_BODY_CHARS) {
            body.truncate(index);
        }
        let instruction = structured::builtin(&name).map_or_else(
            || "Extract the data the schema below asks for from the email.".to_owned(),
            |builtin| builtin.instruction.to_owned(),
        );
        let answer = model
            .ask(
                &Ask {
                    system: structured::STRUCTURED_SYSTEM_PROMPT,
                    instruction,
                    untrusted: vec![("subject", decoded.subject.clone()), ("email", body)],
                    schema: schema.clone(),
                    max_tokens: self.config.max_tokens,
                    account_id: scope.account_id,
                    mailbox: scope.mailbox.clone(),
                    message_id: Some(message_id),
                },
                cancel,
            )
            .await?;
        // Re-validated here even though the provider was asked to constrain
        // the answer: see `structured`'s module docs on why a schema somebody
        // else enforced is not a schema this daemon has checked.
        let data = structured::from_model_answer(&schema, &answer)?;
        let extraction = store::save_extraction(
            &self.db,
            message_id,
            &name,
            &hash,
            &self.config.model,
            &data,
        )
        .await?;
        Ok(StructuredReport {
            extraction,
            cached: false,
        })
    }

    // -----------------------------------------------------------------------
    // Shared plumbing
    // -----------------------------------------------------------------------

    /// A model, or the reason there is not one.
    fn require_model(&self, use_model: bool) -> Result<&ExtractModel, Error> {
        if !use_model {
            return Err(Error::invalid_argument(
                "this format can only be read with a model pass, which this request declined"
                    .to_owned(),
            ));
        }
        self.model.as_deref().ok_or_else(|| {
            Error::invalid_argument(
                "no AI provider is configured, so this format's structure cannot be read"
                    .to_owned(),
            )
        })
    }

    /// The account and folder a message belongs to.
    async fn scope(&self, message_id: i64) -> Result<Scope, Error> {
        let found: Option<(i64, Option<String>)> = self
            .db
            .read(move |conn| {
                conn.query_row(
                    "SELECT m.account_id, mb.name
                       FROM messages m
                       LEFT JOIN mailboxes mb ON mb.id = m.mailbox_id
                      WHERE m.id = ?1",
                    [message_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
            })
            .await?;
        found
            .map(|(account_id, mailbox)| Scope {
                account_id,
                mailbox,
            })
            .ok_or_else(|| Error::not_found(format!("message {message_id}")))
    }

    /// A message's stored raw bytes.
    async fn raw(&self, message_id: i64) -> Result<Vec<u8>, Error> {
        let raw: Option<Option<Vec<u8>>> = self
            .db
            .read(move |conn| {
                conn.query_row(
                    "SELECT raw FROM messages WHERE id = ?1",
                    [message_id],
                    |row| row.get(0),
                )
                .optional()
            })
            .await?;
        match raw {
            Some(Some(raw)) if !raw.is_empty() => Ok(raw),
            Some(_) => Err(Error::failed_precondition(format!(
                "message {message_id} has no stored body; fetch it before extracting from it"
            ))),
            None => Err(Error::not_found(format!("message {message_id}"))),
        }
    }
}

use rusqlite::OptionalExtension;

// ---------------------------------------------------------------------------
// MIME decoding
// ---------------------------------------------------------------------------

/// The pieces of a message the extractors read.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Decoded {
    subject: String,
    /// Rendered body text, for the model routes.
    body: String,
    /// Raw HTML sources, for the link scanner.
    html: Vec<String>,
    /// Plain-text bodies.
    text: Vec<String>,
    /// `text/calendar` parts' decoded text.
    calendars: Vec<String>,
    /// Raw `List-Unsubscribe` header values.
    unsubscribe: Vec<String>,
}

/// Longest single body or calendar part decoded out of a message.
const MAX_PART_BYTES: usize = 1024 * 1024;

/// Most body/calendar parts read from one message.
const MAX_PARTS: usize = 64;

/// Pull the pieces the extractors need out of a raw message.
///
/// Bounded in part count and per-part size: a message may nest `multipart/*`
/// arbitrarily and `mail_parser` will hand back every leaf.
fn decode_message(raw: &[u8]) -> Decoded {
    use mail_parser::{MessageParser, MimeHeaders};

    let mut decoded = Decoded::default();
    let Some(message) = MessageParser::default().parse(raw) else {
        return decoded;
    };
    decoded.subject = message.subject().unwrap_or_default().to_owned();
    for header in message.headers() {
        if header.name().eq_ignore_ascii_case("List-Unsubscribe") {
            // Not `as_text()`. `mail_parser` parses `List-Unsubscribe` with
            // its *address* parser — RFC 2369's `<...>` list is the same shape
            // as an angle-addr list — so the value arrives as an `Address` and
            // a text-only read finds nothing, which silently turned the one
            // authoritative unsubscribe signal in a message into no signal at
            // all. Every form is flattened here.
            header_strings(header.value(), &mut decoded.unsubscribe);
        }
    }
    for part in message.parts.iter().take(MAX_PARTS) {
        let subtype = part
            .content_type()
            .and_then(|ct| ct.subtype().map(str::to_ascii_lowercase))
            .unwrap_or_default();
        let Some(text) = part.text_contents() else {
            continue;
        };
        let text = if text.len() > MAX_PART_BYTES {
            text.get(..MAX_PART_BYTES).unwrap_or_default()
        } else {
            text
        };
        match subtype.as_str() {
            "calendar" => decoded.calendars.push(text.to_owned()),
            "html" => decoded.html.push(text.to_owned()),
            "plain" => decoded.text.push(text.to_owned()),
            _ => {}
        }
    }
    // What the model reads: the plain-text body when there is one, the stripped
    // HTML otherwise. Never the raw markup — a model given HTML spends its
    // budget on style attributes.
    decoded.body = decoded
        .text
        .first()
        .cloned()
        .or_else(|| {
            decoded
                .html
                .first()
                .map(|html| crate::index::extract::strip_html(html))
        })
        .unwrap_or_default();
    decoded
}

/// Run a CPU-bound parse on the blocking pool.
///
/// Every parser in this module walks its whole input, and an attachment is
/// megabytes a stranger chose. Holding a runtime worker for that is the thing
/// CLAUDE.md rules out; `from_xlsx` already had its own `spawn_blocking` and
/// this is the same treatment for the routes that did not.
async fn blocking<T, F>(work: F) -> Result<T, Error>
where
    F: FnOnce() -> Result<T, Error> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| Error::internal(format!("table extraction task failed: {e}")))?
}

/// `text` cut to at most `limit` bytes, on a character boundary.
///
/// `&text[..limit]` panics when byte `limit` lands inside a multi-byte
/// character, and every input in this module is bytes a stranger chose: an
/// `&amp;` followed by an em dash was enough to abort a link scan, and one
/// 8 KB `.ics` content line with an `é` at the cut was enough to abort a
/// calendar parse. One definition, because two would drift and the failure
/// mode is a panic on the request path. The same discipline
/// `ai::injection::bounded` uses.
pub(crate) fn clamp_bytes(text: &str, limit: usize) -> &str {
    if text.len() <= limit {
        return text;
    }
    let mut cut = limit;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text.get(..cut).unwrap_or_default()
}

/// Flatten one header value into the strings it carries, whichever form
/// `mail_parser` chose for it.
fn header_strings(value: &mail_parser::HeaderValue<'_>, out: &mut Vec<String>) {
    use mail_parser::{Address, HeaderValue};

    match value {
        HeaderValue::Text(text) => out.push(text.as_ref().to_owned()),
        HeaderValue::TextList(list) => {
            out.extend(list.iter().map(|text| text.as_ref().to_owned()));
        }
        HeaderValue::Address(Address::List(addresses)) => out.extend(
            addresses
                .iter()
                .filter_map(|addr| addr.address.as_ref().map(|a| a.as_ref().to_owned())),
        ),
        HeaderValue::Address(Address::Group(groups)) => {
            for group in groups {
                out.extend(
                    group
                        .addresses
                        .iter()
                        .filter_map(|addr| addr.address.as_ref().map(|a| a.as_ref().to_owned())),
                );
            }
        }
        _ => {}
    }
}

/// Decode attachment bytes to text, guessing the encoding.
///
/// The same fallback `crate::attach::extract` uses and for the same reason:
/// mail attachments predate the consensus on UTF-8 by decades, Windows-1252
/// maps every byte to something, and a file that is 99% readable is worth
/// reading. Deliberately *not* normalized — see the CSV route's own comment.
fn decode_text(bytes: &[u8]) -> String {
    let (text, _, _) = encoding_rs::UTF_8.decode(bytes);
    if text.contains('\u{fffd}') {
        let (fallback, _, _) = encoding_rs::WINDOWS_1252.decode(bytes);
        return fallback.into_owned();
    }
    text.into_owned()
}

/// One attachment's bytes, keyed the way `attach::decode_parts` keys them.
struct AttachmentPart {
    bytes: Vec<u8>,
    filename: Option<String>,
    content_type: Option<String>,
}

/// Every attachment, with the same positional part ids
/// [`attachment_part`] resolves.
///
/// Bounded by [`MAX_PARTS`], like the body decoder: a message may nest
/// `multipart/*` arbitrarily and `mail_parser` hands back every leaf. The
/// consequence is worth stating plainly: a part past the sixty-fourth is
/// `NOT_FOUND` to the invoice reader while `ExtractTables` — which resolves one
/// part by index and never enumerates — would still open it. The bound is on
/// the side that has to hold every attachment's bytes at once, and answering
/// "no such part" is the honest failure for a message with more attachments
/// than this daemon will enumerate.
fn attachment_parts(raw: &[u8]) -> Vec<(String, AttachmentPart)> {
    use mail_parser::{MessageParser, MimeHeaders};

    let Some(message) = MessageParser::default().parse(raw) else {
        return Vec::new();
    };
    message
        .attachments()
        .take(MAX_PARTS)
        .enumerate()
        .map(|(index, part)| {
            (
                index.to_string(),
                AttachmentPart {
                    bytes: part.contents().to_vec(),
                    filename: part.attachment_name().map(str::to_owned),
                    content_type: part.content_type().map(|ct| {
                        ct.subtype().map_or_else(
                            || ct.ctype().to_owned(),
                            |sub| format!("{}/{}", ct.ctype(), sub),
                        )
                    }),
                },
            )
        })
        .collect()
}

/// The attachment at `part_id`, using the same positional identity
/// `crate::attach` assigns — so a part id from `SearchAttachments` names the
/// same bytes here.
fn attachment_part(raw: &[u8], part_id: &str) -> Option<AttachmentPart> {
    use mail_parser::{MessageParser, MimeHeaders};

    let message = MessageParser::default().parse(raw)?;
    let wanted: usize = part_id.parse().ok()?;
    let part = message.attachments().nth(wanted)?;
    Some(AttachmentPart {
        bytes: part.contents().to_vec(),
        filename: part.attachment_name().map(str::to_owned),
        content_type: part.content_type().map(|ct| {
            ct.subtype().map_or_else(
                || ct.ctype().to_owned(),
                |sub| format!("{}/{}", ct.ctype(), sub),
            )
        }),
    })
}

/// Insert `[page N]` markers into an extracted document, so the model can name
/// the page a table came from and the answer's provenance is checkable.
fn paginate(extracted: &Extracted) -> String {
    if extracted.pages.is_empty() {
        return extracted.text.clone();
    }
    let mut out = String::with_capacity(extracted.text.len() + extracted.pages.len() * 12);
    for (index, (start, end)) in extracted.pages.iter().enumerate() {
        let Some(page) = extracted.text.get(*start..*end) else {
            continue;
        };
        out.push_str(&format!("\n[page {}]\n", index + 1));
        out.push_str(page);
    }
    out
}
