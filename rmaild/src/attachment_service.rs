//! The `AttachmentService` gRPC implementation: one RPC, `AskAttachment`,
//! which is a thin adapter over [`rmail_core::attach::ask`].
//!
//! # Why there is nothing here
//!
//! The same split `ai_service`'s `AskMailbox` documents, for the same reason.
//! Every property this feature has to guarantee — that a `forbidden`/
//! `local_only` folder's document never reaches a provider, that a citation
//! names a passage this daemon actually packed, that grounding is the
//! daemon's verdict rather than the model's — is provable in `rmail-core`
//! without a gRPC server. A transport layer that could weaken one of them by
//! accident is a transport layer that would have to be re-audited every time
//! it changed, so this file converts
//! [`AskEvent`](rmail_core::attach::ask::AskEvent)s to wire
//! [`AskAttachmentChunk`](rmail_proto::v1::AskAttachmentChunk)s and does
//! nothing else.
//!
//! # Why the service is registered even when it cannot answer
//!
//! `AttachmentService` is added to the server unconditionally — the
//! reflection set and the fail-closed scope table must see every RPC
//! regardless of runtime wiring — and declines with `FAILED_PRECONDITION`
//! when AI is off on this daemon. The convention `AiService`/`IndexService`
//! already follow.
#![allow(clippy::result_large_err)] // see mail_service.rs's note on `Result<_, Status>`

use std::pin::Pin;
use std::sync::Arc;

use futures::StreamExt;
use rmail_core::ai::provider::{StopReason, Usage};
use rmail_core::ai::{PolicyEngine, Provider, RateLimiter};
use rmail_core::attach::ask::{
    AskAttachmentRequest as CoreAskRequest, AskEvent, AskOutcome, AttachAskEngine,
    AttachmentCitation, RetrievalTrace,
};
use rmail_core::attach::search::AttachmentSearch;
use rmail_core::config::{AiAsk, AiLimits, AiPrivacy};
use rmail_core::extract::{
    invoice, tables, Claim, DocKind, ExtractEngine, InvoiceFilter, Money, Origin, PaymentStatus,
    Provenance, StoredInvoice, Table, TableReport,
};
use rmail_core::{Database, Error};
use rmail_proto::v1::attachment_service_server::AttachmentService;
use rmail_proto::v1::{
    ask_attachment_chunk, AskAttachmentChunk, AskAttachmentDone, AskAttachmentRequest,
    AttachmentCitation as ProtoCitation, AttachmentRetrievalTrace as ProtoTrace,
    AttachmentUsage as ProtoUsage, CellSource as ProtoCellSource, CellType as ProtoCellType,
    ExportInvoicesRequest, ExportInvoicesResponse, ExtractInvoiceRequest, ExtractInvoiceResponse,
    ExtractTablesRequest, ExtractTablesResponse, ExtractedInvoice, FieldOrigin, FieldProvenance,
    InvoiceCandidate, InvoiceDate, InvoiceDocKind, InvoiceExportFormat, InvoiceLineItem,
    InvoiceMoney, InvoicePaymentStatus, InvoiceText, Table as ProtoTable, TableCell as ProtoCell,
    TableColumn as ProtoColumn, TableOrigin as ProtoOrigin, TableRow as ProtoRow,
};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};

/// The `AttachmentService` handler.
///
/// Cheap to clone: a `Database` handle plus `Arc`s.
#[derive(Clone)]
pub struct AttachmentApi {
    engine: Arc<AttachAskEngine>,
    /// Whether the AI subsystem is actually active on this daemon
    /// (`ai.enabled = true` and a provider was built). Checked before any
    /// retrieval runs, so a disabled daemon declines in microseconds rather
    /// than after ranking a corpus for a call it was never going to make.
    enabled: bool,
    /// The structured-extraction engine behind `ExtractTables`. `None` leaves
    /// that RPC declining with `FAILED_PRECONDITION`, the convention this
    /// service already follows for `AskAttachment` on an AI-less daemon — but
    /// note the two are wired independently: table extraction's *native*
    /// routes need no provider at all, so the engine is built even when
    /// `enabled` is false.
    extract: Option<ExtractEngine>,
    /// Cancelled when the daemon shuts down, so open answers stop with it
    /// rather than holding shutdown open.
    shutdown: CancellationToken,
}

impl AttachmentApi {
    /// Build the handler from the daemon's own provider, policy engine,
    /// privacy settings, limits and shared AI concurrency budget.
    ///
    /// `semaphore`/`rate_limiter` must be the daemon's `AiWorkerPool`'s own,
    /// for the reason `AiApi::new` gives: one process must not exceed one
    /// configured `ai.limits` ceiling because it has several call sites.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: Database,
        provider: Arc<dyn Provider>,
        policy: Arc<PolicyEngine>,
        search: AttachmentSearch,
        privacy: AiPrivacy,
        limits: AiLimits,
        config: AiAsk,
        semaphore: Arc<Semaphore>,
        rate_limiter: Arc<RateLimiter>,
        enabled: bool,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            engine: Arc::new(AttachAskEngine::new(
                db,
                provider,
                policy,
                search,
                privacy,
                limits,
                config,
                semaphore,
                rate_limiter,
            )),
            enabled,
            extract: None,
            shutdown,
        }
    }

    /// Serve `ExtractTables` from `engine`.
    ///
    /// Separate from [`Self::new`] because the two capabilities have different
    /// preconditions: `AskAttachment` needs a provider, and table extraction's
    /// native routes do not.
    #[must_use]
    pub fn with_extract(mut self, engine: ExtractEngine) -> Self {
        self.extract = Some(engine);
        self
    }
}

#[tonic::async_trait]
impl AttachmentService for AttachmentApi {
    type AskAttachmentStream =
        Pin<Box<dyn tokio_stream::Stream<Item = Result<AskAttachmentChunk, Status>> + Send>>;

    #[tracing::instrument(skip(self, request), fields(message_id, top_k))]
    async fn ask_attachment(
        &self,
        request: Request<AskAttachmentRequest>,
    ) -> Result<Response<Self::AskAttachmentStream>, Status> {
        let req = request.into_inner();
        tracing::Span::current()
            .record("message_id", req.message_id)
            .record("top_k", req.top_k);

        if !self.enabled {
            return Err(Status::from(Error::failed_precondition(
                "AI is disabled on this daemon (ai.enabled = false, or no provider could be \
                 built), so ask-attachment cannot answer"
                    .to_owned(),
            )));
        }

        // A child of the shutdown token, so daemon shutdown ends an open
        // answer — and so dropping the response stream propagates to the
        // provider rather than merely to the relay.
        let cancel = self.shutdown.child_token();
        let stream = self
            .engine
            .ask(
                &CoreAskRequest {
                    question: req.question,
                    message_id: req.message_id,
                    part_id: req.part_id,
                    account_id: req.account_id,
                    top_k: req.top_k,
                },
                &cancel,
            )
            .await
            .map_err(Status::from)?;

        // The token is cancelled when the mapped stream is dropped, which is
        // what tonic does the instant a client disconnects. Without this the
        // engine's own `tx.closed()` race would still fire, but the upstream
        // HTTP request would only be dropped once the engine noticed.
        let guard = CancelOnDrop(cancel);
        let stream = stream.map(move |event| {
            let _ = &guard;
            event.map(to_proto_chunk).map_err(Status::from)
        });
        Ok(Response::new(Box::pin(stream)))
    }

    #[tracing::instrument(skip(self, request), fields(message_id, tables))]
    async fn extract_tables(
        &self,
        request: Request<ExtractTablesRequest>,
    ) -> Result<Response<ExtractTablesResponse>, Status> {
        let req = request.into_inner();
        tracing::Span::current().record("message_id", req.message_id);
        if req.message_id <= 0 {
            return Err(Status::from(Error::invalid_argument(
                "message_id must be a positive message id",
            )));
        }
        let Some(engine) = self.extract.as_ref() else {
            return Err(Status::from(Error::failed_precondition(
                "structured extraction is not wired on this daemon".to_owned(),
            )));
        };
        let cancel = self.shutdown.child_token();
        let report = engine
            .tables(req.message_id, &req.part_id, req.allow_model, &cancel)
            .await
            .map_err(Status::from)?;
        tracing::Span::current().record("tables", report.tables.len());
        Ok(Response::new(to_proto_tables(&report)))
    }

    #[tracing::instrument(skip(self, request), fields(message_id, inferred))]
    async fn extract_invoice(
        &self,
        request: Request<ExtractInvoiceRequest>,
    ) -> Result<Response<ExtractInvoiceResponse>, Status> {
        let req = request.into_inner();
        tracing::Span::current().record("message_id", req.message_id);
        if req.message_id <= 0 {
            return Err(Status::from(Error::invalid_argument(
                "message_id must be a positive message id",
            )));
        }
        let engine = self.extract_engine()?;
        let cancel = self.shutdown.child_token();
        // Empty means "detect across the message". An explicit empty part id
        // and an absent one are the same request, so there is no way to ask
        // for "the part called empty string" and get a NOT_FOUND for it.
        let part = req.part_id.trim();
        let part = (!part.is_empty()).then_some(part);
        let report = engine
            .invoice(req.message_id, part, req.use_model, &cancel)
            .await
            .map_err(Status::from)?;
        tracing::Span::current().record("inferred", report.stored.invoice.inferred());
        Ok(Response::new(ExtractInvoiceResponse {
            invoice: Some(to_proto_invoice(&report.stored)),
            candidates: report
                .candidates
                .iter()
                .map(|candidate| InvoiceCandidate {
                    part_id: candidate.part.clone(),
                    filename: candidate.filename.clone(),
                    kind: to_proto_kind(candidate.kind) as i32,
                })
                .collect(),
            used_model: report.used_model,
        }))
    }

    #[tracing::instrument(skip(self, request), fields(invoices))]
    async fn export_invoices(
        &self,
        request: Request<ExportInvoicesRequest>,
    ) -> Result<Response<ExportInvoicesResponse>, Status> {
        let req = request.into_inner();
        // A negative id is a client bug, and letting it through would answer
        // "no invoices" — which reads as "you have none" rather than "you
        // asked wrong".
        for (name, value) in [
            ("account_id", req.account_id),
            ("message_id", req.message_id),
        ] {
            if value < 0 {
                return Err(Status::from(Error::invalid_argument(format!(
                    "{name} must be a positive id or 0 for every one"
                ))));
            }
        }
        let engine = self.extract_engine()?;
        let vendor = req.vendor.trim();
        let filter = InvoiceFilter {
            account_id: (req.account_id > 0).then_some(req.account_id),
            message_id: (req.message_id > 0).then_some(req.message_id),
            vendor: (!vendor.is_empty()).then(|| vendor.to_owned()),
            since: (req.since > 0).then_some(req.since),
            until: (req.until > 0).then_some(req.until),
            limit: req.limit,
        };
        let rows = engine.list_invoices(&filter).await.map_err(Status::from)?;
        tracing::Span::current().record("invoices", rows.len());
        let csv = match InvoiceExportFormat::try_from(req.format) {
            Ok(InvoiceExportFormat::Csv) => invoice::to_csv(&rows),
            Ok(InvoiceExportFormat::Unspecified | InvoiceExportFormat::Rows) => String::new(),
            // A number this build has no variant for is a newer client. Named
            // rather than defaulted, which would silently return rows to a
            // caller that asked for a file.
            Err(_) => {
                return Err(Status::from(Error::invalid_argument(format!(
                    "format {} is not one this daemon knows",
                    req.format
                ))))
            }
        };
        Ok(Response::new(ExportInvoicesResponse {
            invoices: rows.iter().map(to_proto_invoice).collect(),
            csv,
        }))
    }
}

impl AttachmentApi {
    /// The extraction engine, or the reason there is not one.
    fn extract_engine(&self) -> Result<&ExtractEngine, Status> {
        self.extract.as_ref().ok_or_else(|| {
            Status::from(Error::failed_precondition(
                "structured extraction is not wired on this daemon".to_owned(),
            ))
        })
    }
}

// ---------------------------------------------------------------------------
// ExtractInvoice / ExportInvoices conversions
// ---------------------------------------------------------------------------

fn to_proto_invoice(stored: &StoredInvoice) -> ExtractedInvoice {
    let invoice = &stored.invoice;
    ExtractedInvoice {
        invoice_id: stored.invoice_id,
        message_id: stored.message_id,
        part_id: invoice.part.clone(),
        kind: to_proto_kind(Some(invoice.kind)) as i32,
        vendor: invoice.vendor.as_ref().map(|claim| InvoiceText {
            value: claim.value.clone(),
            provenance: Some(to_proto_provenance(&claim.provenance)),
        }),
        number: invoice.number.as_ref().map(|claim| InvoiceText {
            value: claim.value.clone(),
            provenance: Some(to_proto_provenance(&claim.provenance)),
        }),
        currency: invoice.currency.clone().unwrap_or_default(),
        subtotal: invoice.subtotal.as_ref().map(to_proto_money),
        tax: invoice.tax.as_ref().map(to_proto_money),
        total: invoice.total.as_ref().map(to_proto_money),
        issued_at: invoice.issued_at.as_ref().map(to_proto_date),
        due_at: invoice.due_at.as_ref().map(to_proto_date),
        status: invoice
            .status
            .as_ref()
            .map_or(InvoicePaymentStatus::Unspecified, |claim| {
                match claim.value {
                    PaymentStatus::Paid => InvoicePaymentStatus::Paid,
                    PaymentStatus::Unpaid => InvoicePaymentStatus::Unpaid,
                    PaymentStatus::Overdue => InvoicePaymentStatus::Overdue,
                    PaymentStatus::Refunded => InvoicePaymentStatus::Refunded,
                    PaymentStatus::Void => InvoicePaymentStatus::Void,
                }
            }) as i32,
        // The status enum has no message to hang a provenance off, so it gets
        // its own field rather than losing it: a status a model inferred and
        // one the document stamped are not the same claim either.
        status_provenance: invoice
            .status
            .as_ref()
            .map(|claim| to_proto_provenance(&claim.provenance)),
        line_items: invoice
            .line_items
            .iter()
            .map(|item| InvoiceLineItem {
                description: item.description.clone(),
                quantity: item.quantity.unwrap_or_default(),
                has_quantity: item.quantity.is_some(),
                unit_price: item.unit_price.as_ref().map(|money| InvoiceMoney {
                    currency: money.currency.clone(),
                    minor_units: money.minor_units,
                    provenance: Some(to_proto_provenance(&Provenance {
                        part: invoice.part.clone(),
                        origin: item.origin,
                        ..Provenance::default()
                    })),
                }),
                total: item.total.as_ref().map(|money| InvoiceMoney {
                    currency: money.currency.clone(),
                    minor_units: money.minor_units,
                    provenance: Some(to_proto_provenance(&Provenance {
                        part: invoice.part.clone(),
                        origin: item.origin,
                        ..Provenance::default()
                    })),
                }),
                origin: to_proto_origin(item.origin) as i32,
            })
            .collect(),
        warnings: invoice.warnings.clone(),
        inferred: invoice.inferred(),
        extracted_at: stored.extracted_at,
    }
}

fn to_proto_money(claim: &Claim<Money>) -> InvoiceMoney {
    InvoiceMoney {
        currency: claim.value.currency.clone(),
        minor_units: claim.value.minor_units,
        provenance: Some(to_proto_provenance(&claim.provenance)),
    }
}

fn to_proto_date(claim: &Claim<i64>) -> InvoiceDate {
    InvoiceDate {
        at: claim.value,
        provenance: Some(to_proto_provenance(&claim.provenance)),
    }
}

fn to_proto_provenance(provenance: &Provenance) -> FieldProvenance {
    FieldProvenance {
        part: provenance.part.clone(),
        page: provenance.page.unwrap_or_default(),
        span_start: i64::try_from(provenance.span_start).unwrap_or(i64::MAX),
        span_end: i64::try_from(provenance.span_end).unwrap_or(i64::MAX),
        origin: to_proto_origin(provenance.origin) as i32,
    }
}

fn to_proto_origin(origin: Origin) -> FieldOrigin {
    match origin {
        Origin::Parsed => FieldOrigin::Parsed,
        Origin::Model => FieldOrigin::Model,
    }
}

fn to_proto_kind(kind: Option<DocKind>) -> InvoiceDocKind {
    match kind {
        Some(DocKind::Invoice) => InvoiceDocKind::Invoice,
        Some(DocKind::Receipt) => InvoiceDocKind::Receipt,
        None => InvoiceDocKind::Unspecified,
    }
}

// ---------------------------------------------------------------------------
// ExtractTables conversions
// ---------------------------------------------------------------------------

fn to_proto_tables(report: &TableReport) -> ExtractTablesResponse {
    ExtractTablesResponse {
        tables: report.tables.iter().map(to_proto_table).collect(),
        dropped_tables: clamp_u32(report.dropped_tables),
        cell_budget_exhausted: report.cell_budget_exhausted,
    }
}

fn to_proto_table(table: &Table) -> ProtoTable {
    ProtoTable {
        name: table.name.clone(),
        columns: table
            .columns
            .iter()
            .map(|column| ProtoColumn {
                header: column.header.clone(),
                r#type: to_proto_cell_type(column.kind) as i32,
            })
            .collect(),
        rows: table
            .rows
            .iter()
            .map(|row| ProtoRow {
                cells: row.iter().map(to_proto_cell).collect(),
            })
            .collect(),
        origin: match table.origin {
            tables::TableOrigin::Spreadsheet => ProtoOrigin::Spreadsheet,
            tables::TableOrigin::Csv => ProtoOrigin::Csv,
            tables::TableOrigin::Html => ProtoOrigin::Html,
            tables::TableOrigin::Model => ProtoOrigin::Model,
        } as i32,
        // Sent explicitly rather than left for a client to derive from
        // `origin`: a client that forgot would treat a transcription as a
        // parse, which is the one mistake this field exists to prevent.
        inferred: table.inferred(),
        truncated: table.truncated,
    }
}

fn to_proto_cell(cell: &tables::Cell) -> ProtoCell {
    let (number, boolean, date) = match &cell.value {
        tables::CellValue::Number(value) => (*value, false, 0),
        tables::CellValue::Bool(value) => (0.0, *value, 0),
        tables::CellValue::Date(value) => (0.0, false, *value),
        tables::CellValue::Text(_) | tables::CellValue::Empty => (0.0, false, 0),
    };
    ProtoCell {
        text: cell.text.clone(),
        r#type: to_proto_cell_type(cell.value.kind()) as i32,
        number,
        boolean,
        date,
        source: Some(ProtoCellSource {
            sheet: cell.source.sheet.clone(),
            page: cell.source.page.unwrap_or(0),
            row: clamp_u32(cell.source.row),
            col: clamp_u32(cell.source.col),
            reference: cell.source.reference.clone(),
        }),
    }
}

fn to_proto_cell_type(kind: tables::CellType) -> ProtoCellType {
    match kind {
        tables::CellType::Empty => ProtoCellType::Empty,
        tables::CellType::Text => ProtoCellType::Text,
        tables::CellType::Number => ProtoCellType::Number,
        tables::CellType::Bool => ProtoCellType::Bool,
        tables::CellType::Date => ProtoCellType::Date,
    }
}

/// Cancels its token when dropped — see the call site.
struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// One core [`AskEvent`] as a wire [`AskAttachmentChunk`].
fn to_proto_chunk(event: AskEvent) -> AskAttachmentChunk {
    let body = match event {
        AskEvent::Trace(trace) => ask_attachment_chunk::Body::Trace(to_proto_trace(trace)),
        AskEvent::Token(token) => ask_attachment_chunk::Body::Token(token),
        AskEvent::Citation(citation) => {
            ask_attachment_chunk::Body::Citation(to_proto_citation(citation))
        }
        AskEvent::Usage(usage) => ask_attachment_chunk::Body::Usage(to_proto_usage(usage)),
        AskEvent::Done(outcome) => ask_attachment_chunk::Body::Done(to_proto_done(outcome)),
    };
    AskAttachmentChunk { body: Some(body) }
}

fn to_proto_trace(trace: RetrievalTrace) -> ProtoTrace {
    ProtoTrace {
        retrieved: clamp_u32(trace.retrieved),
        attachments: clamp_u32(trace.attachments),
        passages: clamp_u32(trace.passages),
        withheld_by_policy: clamp_u32(trace.withheld_by_policy),
        dropped_for_budget: clamp_u32(trace.dropped_for_budget),
        context_tokens: clamp_u32(trace.context_tokens),
        model: trace.model,
    }
}

fn to_proto_citation(citation: AttachmentCitation) -> ProtoCitation {
    ProtoCitation {
        label: citation.label,
        message_id: citation.message_id,
        message_uid: citation.message_uid,
        account_id: citation.account_id,
        mailbox: citation.mailbox,
        part_id: citation.part_id,
        filename: citation.filename,
        page: citation.page,
        span_start: citation.span_start,
        span_end: citation.span_end,
        quote: citation.quote,
    }
}

fn to_proto_usage(usage: Usage) -> ProtoUsage {
    ProtoUsage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_creation_input_tokens: usage.cache_creation_input_tokens,
        cache_read_input_tokens: usage.cache_read_input_tokens,
    }
}

fn to_proto_done(outcome: AskOutcome) -> AskAttachmentDone {
    AskAttachmentDone {
        grounded: outcome.grounded,
        // Empty exactly when grounded, per the proto's own contract — the
        // refusal text is the engine's, so a client never has to compose one.
        refusal: outcome
            .refusal
            .map(|refusal| refusal.message().to_owned())
            .unwrap_or_default(),
        stop_reason: outcome
            .stop_reason
            .map(|reason| stop_reason_str(reason).to_owned())
            .unwrap_or_default(),
    }
}

/// The wire spelling of a stop reason. Duplicated from `ai_service`'s own
/// rather than exported from it: it is five string literals, and the two
/// services' wire vocabularies are independent contracts that happen to
/// agree today.
fn stop_reason_str(reason: StopReason) -> &'static str {
    match reason {
        StopReason::EndTurn => "end_turn",
        StopReason::MaxTokens => "max_tokens",
        StopReason::StopSequence => "stop_sequence",
        StopReason::ToolUse => "tool_use",
        StopReason::PauseTurn => "pause_turn",
    }
}

/// A count as a wire `uint32`. Saturating rather than wrapping: these are
/// display counters, and a wrapped one would be a lie rather than a large
/// number.
fn clamp_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}
