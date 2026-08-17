//! The `ExtractService` and `LinkService` gRPC implementations: task 75's
//! calendar/task and link surfaces (prd.md #65, #66).
//!
//! Both are thin. [`rmail_core::extract`] owns the parsers, the bounds, the
//! model fencing and the idempotency; what lives here is the translation
//! between its types and the wire, and the two decisions that are genuinely a
//! transport concern:
//!
//! - **The sink is resolved from configuration, never from the request.** The
//!   wire carries an `ExtractionSink` enum — a *shape* — and
//!   [`rmail_core::extract::ExtractEngine::sink`] turns that into the command
//!   or URL the operator configured. A caller that could name a command would
//!   have remote code execution; a caller that could name a URL would have an
//!   outbound request generator. Asking for a shape the operator has not
//!   configured is `INVALID_ARGUMENT`, deliberately not a silent degrade to
//!   returning a file: a user must not believe their task tracker was updated
//!   when it was not.
//! - **Events and tasks are delivered under separate `kind`s.** The
//!   idempotency key includes it, so extracting a message's events does not
//!   suppress its tasks.
//!
//! # Why these two services share a file
//!
//! They share a dependency — one [`rmail_core::extract::ExtractEngine`] — and
//! nothing else. Two files would duplicate its construction and the four
//! conversion helpers; the alternative, giving each its own engine, would give
//! the daemon two of everything the engine holds, including two views of the
//! model's concurrency budget. That is the same reasoning
//! [`rmail_core::ai::gate`] gives for not minting fresh semaphores.
//
// `tonic::Status` is intentionally the error type throughout a gRPC service
// boundary; its size makes `result_large_err` fire on every
// `Result<_, Status>` helper, so the lint is allowed for this module — the
// same allowance `ai_safety_service.rs`/`audit_service.rs` carry.
#![allow(clippy::result_large_err)]

use rmail_core::extract::{events, links, Event, ExtractEngine, LinkReport, Sink, Task};
use rmail_core::Error;
use rmail_proto::v1::extract_service_server::ExtractService;
use rmail_proto::v1::link_service_server::LinkService;
use rmail_proto::v1::{
    ExtractEventsRequest, ExtractEventsResponse, ExtractLinksRequest, ExtractLinksResponse,
    ExtractStructuredRequest, ExtractStructuredResponse, ExtractTasksRequest, ExtractTasksResponse,
    ExtractedEvent, ExtractedLink, ExtractedTask, ExtractionSink, ExtractionSource, LinkClassifier,
    LinkKind, LinkSource,
};
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};

/// The `ExtractService` handler.
#[derive(Debug, Clone)]
pub struct ExtractApi {
    engine: ExtractEngine,
    /// Cancelled when the daemon shuts down. A child of it reaches the model
    /// call and the sink, so a shutdown stops a webhook POST and a piped
    /// command rather than holding shutdown open behind them — the same
    /// wiring `AnalyticsApi` documents for its own scans.
    shutdown: CancellationToken,
}

impl ExtractApi {
    /// Build a handler over `engine`.
    #[must_use]
    pub fn new(engine: ExtractEngine, shutdown: CancellationToken) -> Self {
        Self { engine, shutdown }
    }
}

/// The `LinkService` handler.
#[derive(Debug, Clone)]
pub struct LinkApi {
    engine: ExtractEngine,
    shutdown: CancellationToken,
}

impl LinkApi {
    /// Build a handler over `engine`.
    #[must_use]
    pub fn new(engine: ExtractEngine, shutdown: CancellationToken) -> Self {
        Self { engine, shutdown }
    }
}

#[tonic::async_trait]
impl ExtractService for ExtractApi {
    #[tracing::instrument(skip(self, request))]
    async fn extract_events(
        &self,
        request: Request<ExtractEventsRequest>,
    ) -> Result<Response<ExtractEventsResponse>, Status> {
        let cancel = self.shutdown.child_token();
        let request = request.into_inner();
        let message_id = validate_id(request.message_id)?;
        let sink = self.sink(request.sink)?;

        let report = self
            .engine
            .calendar(message_id, request.use_model, &cancel)
            .await
            .map_err(Status::from)?;
        let delivery = self
            .engine
            .deliver_events(message_id, &report.events, &sink, &cancel)
            .await
            .map_err(Status::from)?;

        Ok(Response::new(ExtractEventsResponse {
            events: report.events.iter().map(event_to_proto).collect(),
            method: report.method,
            skipped: count(report.skipped),
            ics: delivery.ics,
            delivered: count(delivery.delivered),
            already_delivered: count(delivery.skipped),
            sink_output: delivery.output,
        }))
    }

    #[tracing::instrument(skip(self, request))]
    async fn extract_tasks(
        &self,
        request: Request<ExtractTasksRequest>,
    ) -> Result<Response<ExtractTasksResponse>, Status> {
        let cancel = self.shutdown.child_token();
        let request = request.into_inner();
        let message_id = validate_id(request.message_id)?;
        let sink = self.sink(request.sink)?;

        let report = self
            .engine
            .calendar(message_id, request.use_model, &cancel)
            .await
            .map_err(Status::from)?;
        let delivery = self
            .engine
            .deliver_tasks(message_id, &report.tasks, &sink, &cancel)
            .await
            .map_err(Status::from)?;

        Ok(Response::new(ExtractTasksResponse {
            tasks: report.tasks.iter().map(task_to_proto).collect(),
            skipped: count(report.skipped),
            ics: delivery.ics,
            delivered: count(delivery.delivered),
            already_delivered: count(delivery.skipped),
            sink_output: delivery.output,
        }))
    }

    #[tracing::instrument(skip(self, request), fields(schema, cached))]
    async fn extract_structured(
        &self,
        request: Request<ExtractStructuredRequest>,
    ) -> Result<Response<ExtractStructuredResponse>, Status> {
        let cancel = self.shutdown.child_token();
        let request = request.into_inner();
        let message_id = validate_id(request.message_id)?;

        // A caller-supplied schema is parsed here rather than in the engine so
        // "that is not JSON" answers INVALID_ARGUMENT — it is the caller's own
        // input — where a schema-shaped-but-unenforceable one is the engine's
        // judgement to make.
        let custom = {
            let text = request.schema_json.trim();
            if text.is_empty() {
                None
            } else {
                Some(serde_json::from_str(text).map_err(|e| {
                    Status::from(Error::invalid_argument(format!(
                        "schema_json is not valid JSON: {e}"
                    )))
                })?)
            }
        };
        let report = self
            .engine
            .structured(
                message_id,
                &request.schema,
                custom,
                request.refresh,
                &cancel,
            )
            .await
            .map_err(Status::from)?;

        let span = tracing::Span::current();
        span.record("schema", report.extraction.schema_name.as_str());
        span.record("cached", report.cached);
        Ok(Response::new(ExtractStructuredResponse {
            extraction_id: report.extraction.extraction_id,
            message_id: report.extraction.message_id,
            schema: report.extraction.schema_name,
            schema_hash: report.extraction.schema_hash,
            data: report.extraction.data,
            model: report.extraction.model,
            created_at: report.extraction.created_at,
            cached: report.cached,
        }))
    }
}

impl ExtractApi {
    /// Turn the wire's sink *shape* into the operator's configured sink.
    ///
    /// See the module docs on why the request carries a shape and not a
    /// destination.
    fn sink(&self, sink: i32) -> Result<Sink, Status> {
        let name = match ExtractionSink::try_from(sink) {
            Ok(ExtractionSink::Unspecified | ExtractionSink::Ics) => "ics",
            Ok(ExtractionSink::Command) => "command",
            Ok(ExtractionSink::Webhook) => "webhook",
            // A number this build has no variant for: a newer client talking
            // to an older daemon. Named rather than defaulted to `ics`, which
            // would silently return a file to a caller that asked for a
            // webhook.
            Err(_) => {
                return Err(Status::from(Error::invalid_argument(format!(
                    "sink {sink} is not a shape this daemon knows"
                ))))
            }
        };
        self.engine.sink(name).map_err(Status::from)
    }
}

#[tonic::async_trait]
impl LinkService for LinkApi {
    #[tracing::instrument(skip(self, request))]
    async fn extract_links(
        &self,
        request: Request<ExtractLinksRequest>,
    ) -> Result<Response<ExtractLinksResponse>, Status> {
        let cancel = self.shutdown.child_token();
        let request = request.into_inner();
        let message_id = validate_id(request.message_id)?;
        let report: LinkReport = self
            .engine
            .links(message_id, request.use_model, &cancel)
            .await
            .map_err(Status::from)?;
        Ok(Response::new(ExtractLinksResponse {
            links: report.links.iter().map(link_to_proto).collect(),
            truncated: count(report.truncated),
            skipped_parts: count(report.skipped_parts),
            tracking_pixels: count(report.tracking_pixels),
        }))
    }
}

// ---------------------------------------------------------------------------
// Conversions
// ---------------------------------------------------------------------------

fn event_to_proto(event: &Event) -> ExtractedEvent {
    ExtractedEvent {
        uid: event.uid.clone(),
        summary: event.summary.clone(),
        description: event.description.clone(),
        location: event.location.clone(),
        starts_at: event.starts_at,
        ends_at: event.ends_at.unwrap_or(0),
        all_day: event.all_day,
        organizer: event.organizer.clone(),
        attendees: event.attendees.clone(),
        rrule: event.rrule.clone(),
        source: source_to_proto(event.source) as i32,
        confidence: event.confidence,
        cancelled: event.cancelled,
    }
}

fn task_to_proto(task: &Task) -> ExtractedTask {
    ExtractedTask {
        uid: task.uid.clone(),
        summary: task.summary.clone(),
        description: task.description.clone(),
        due_at: task.due_at.unwrap_or(0),
        priority: u32::from(task.priority),
        completed: task.completed,
        source: source_to_proto(task.source) as i32,
        confidence: task.confidence,
    }
}

fn source_to_proto(source: events::Source) -> ExtractionSource {
    match source {
        events::Source::Ics => ExtractionSource::Ics,
        events::Source::Model => ExtractionSource::Model,
    }
}

fn link_to_proto(link: &links::Link) -> ExtractedLink {
    ExtractedLink {
        url: link.url.clone(),
        host: link.host.clone(),
        scheme: link.scheme.clone(),
        display_text: link.display_text.clone(),
        display_host: link.display_host.clone().unwrap_or_default(),
        deceptive: link.deceptive,
        kind: match link.kind {
            links::LinkKind::Unsubscribe => LinkKind::Unsubscribe,
            links::LinkKind::Tracking => LinkKind::Tracking,
            links::LinkKind::Meeting => LinkKind::Meeting,
            links::LinkKind::Document => LinkKind::Document,
            links::LinkKind::Cta => LinkKind::Cta,
            links::LinkKind::Other => LinkKind::Other,
        } as i32,
        classifier: match link.classifier {
            links::Classifier::Rules => LinkClassifier::Rules,
            links::Classifier::Model => LinkClassifier::Model,
        } as i32,
        score: link.score,
        reason: link.reason.clone(),
        occurrences: count(link.occurrences),
        source: Some(LinkSource {
            part: link.source.part.clone(),
            span_start: i64::try_from(link.source.span_start).unwrap_or(i64::MAX),
            span_end: i64::try_from(link.source.span_end).unwrap_or(i64::MAX),
        }),
    }
}

/// A count on the wire. Saturating rather than casting, so a nonsense value
/// never reaches a client as a wrapped-around small number.
fn count(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// Reject a non-positive message id before it reaches a query.
///
/// `messages.id` is a SQLite `INTEGER PRIMARY KEY` and is always positive, so
/// a zero or negative id is a client bug; answering `INVALID_ARGUMENT` says
/// so, where letting it through would answer `NOT_FOUND` and read as "that
/// message was deleted".
fn validate_id(message_id: i64) -> Result<i64, Status> {
    if message_id <= 0 {
        return Err(Status::from(Error::invalid_argument(
            "message_id must be a positive message id",
        )));
    }
    Ok(message_id)
}
