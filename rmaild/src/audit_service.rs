//! The `AuditService` gRPC implementation: read-only access to the
//! append-only AI call audit ledger (`rmail_core::ai::audit`).
//!
//! # `ExportLedger` streams pages, not one eager `Vec`
//!
//! A naive implementation would run one query for everything matching the
//! filter and replay it. That holds the whole result set in memory for the
//! life of the stream, and a client that stops reading — or a `mail ai audit
//! export` piped into something that dies — has no way to make the daemon
//! stop doing work on its behalf. Instead this walks the ledger the same way
//! `SyncApi::watch_events` walks the durable event log
//! (`rmaild/src/sync_service.rs`): a spawned task pages through
//! [`ai::query_calls`] by `id` cursor, feeding a bounded channel, and selects
//! against the daemon's shutdown token on every send so a dropped client or a
//! process shutdown stops the paging loop rather than leaving it to run to
//! completion into a channel nobody drains.
//
// `tonic::Status` is intentionally the error type throughout a gRPC service
// boundary; its size makes `result_large_err` fire on every `Result<_, Status>`
// helper, so the lint is allowed for this module.
#![allow(clippy::result_large_err)]

use std::pin::Pin;

use rmail_core::ai::{
    self, AuditFilter as CoreAuditFilter, CallStatus as CoreCallStatus, LedgerEntry,
};
use rmail_core::{Database, Error};
use rmail_proto::v1::audit_service_server::AuditService;
use rmail_proto::v1::{
    AuditEntry as ProtoAuditEntry, AuditFilter as ProtoAuditFilter, CallStatus as ProtoCallStatus,
    ExportLedgerRequest, QueryAiCallsRequest, QueryAiCallsResponse,
};
use tokio_stream::Stream;
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};
use tracing::Instrument;

/// Page size `QueryAiCalls` uses when the caller does not ask for one.
const DEFAULT_PAGE_SIZE: i64 = 50;

/// Ceiling on `QueryAiCalls`'s page size, kept comfortably under
/// `rmail_core::ai::audit`'s own internal cap so the `page_size + 1`
/// over-fetch this handler uses to compute `has_more` is never itself
/// clamped away by that inner limit.
const MAX_PAGE_SIZE: i64 = 200;

/// The batch size `ExportLedger`'s paging loop reads at a time. Independent
/// of `MAX_PAGE_SIZE` — that one bounds a single response message; this one
/// bounds how much work happens between checks of the channel/cancellation.
///
/// Currently equal to `rmail_core::ai::audit`'s own internal page-size
/// ceiling (`MAX_QUERY_LIMIT`, 500 as of this writing), which
/// [`ai::query_calls`] silently clamps every request down to regardless of
/// what is asked for. Raising this constant alone would do nothing until
/// that one also moves — they are two ends of the same batch, just owned by
/// different crates.
const EXPORT_BATCH_SIZE: i64 = 500;

/// Backpressure between the `ExportLedger` paging task and its consumer: a
/// slow client applies backpressure to the paging loop rather than the daemon
/// buffering the rest of a large export in memory on the client's behalf.
const EXPORT_STREAM_BUFFER: usize = 256;

/// The `AuditService` handler, backed by the local database.
#[derive(Clone)]
pub struct AuditApi {
    db: Database,
    /// Cancelled when the daemon shuts down, so an in-flight `ExportLedger`
    /// stream stops with it rather than holding shutdown open.
    shutdown: CancellationToken,
}

impl AuditApi {
    /// Create a handler over the given database.
    #[must_use]
    pub fn new(db: Database, shutdown: CancellationToken) -> Self {
        Self { db, shutdown }
    }
}

#[tonic::async_trait]
impl AuditService for AuditApi {
    async fn query_ai_calls(
        &self,
        request: Request<QueryAiCallsRequest>,
    ) -> Result<Response<QueryAiCallsResponse>, Status> {
        let req = request.into_inner();
        let filter = filter_from_proto(req.filter)?;
        if let Some(account_id) = filter.account_id {
            tracing::Span::current().record(rmail_core::telemetry::FIELD_ACCOUNT, account_id);
        }

        let page_size = if req.limit <= 0 {
            DEFAULT_PAGE_SIZE
        } else {
            i64::from(req.limit).min(MAX_PAGE_SIZE)
        };
        let before_id = non_negative(req.before_id, "before_id")?;

        // Over-fetch by one to learn whether another page exists without a
        // second round trip: a full `page_size + 1` rows back means there is
        // at least one more beyond what is returned.
        let mut entries = ai::query_calls(&self.db, &filter, page_size + 1, before_id).await?;
        let has_more = i64::try_from(entries.len()).unwrap_or(i64::MAX) > page_size;
        if has_more {
            entries.truncate(usize::try_from(page_size).unwrap_or(usize::MAX));
        }

        Ok(Response::new(QueryAiCallsResponse {
            entries: entries.iter().map(to_proto).collect(),
            has_more,
        }))
    }

    type ExportLedgerStream =
        Pin<Box<dyn Stream<Item = Result<ProtoAuditEntry, Status>> + Send + 'static>>;

    async fn export_ledger(
        &self,
        request: Request<ExportLedgerRequest>,
    ) -> Result<Response<Self::ExportLedgerStream>, Status> {
        let filter = filter_from_proto(request.into_inner().filter)?;
        let db = self.db.clone();
        let cancel = rpc_cancel(&self.shutdown);

        let (tx, rx) = tokio::sync::mpsc::channel(EXPORT_STREAM_BUFFER);
        tokio::spawn(
            async move {
                tracing::debug!(?filter, "ledger export starting");
                let mut before_id: Option<i64> = None;
                let mut sent_total: u64 = 0;
                loop {
                    let page =
                        match ai::query_calls(&db, &filter, EXPORT_BATCH_SIZE, before_id).await {
                            Ok(page) => page,
                            Err(error) => {
                                tracing::warn!(%error, sent_total, "ledger export failed mid-stream");
                                let _ =
                                    send(&tx, &cancel, Err(Status::from(error))).await;
                                return;
                            }
                        };
                    if page.is_empty() {
                        tracing::debug!(sent_total, "ledger export finished");
                        return;
                    }
                    // `query_calls` returns newest-first; the last entry in a
                    // page is the smallest id seen so far, and therefore the
                    // correct cursor to resume just after.
                    before_id = page.last().map(|entry| entry.id);

                    for entry in &page {
                        if send(&tx, &cancel, Ok(to_proto(entry))).await.is_break() {
                            tracing::debug!(
                                sent_total,
                                "ledger export stopped early (client disconnected or daemon shutting down)"
                            );
                            return;
                        }
                        sent_total += 1;
                    }
                }
            }
            .instrument(tracing::Span::current()),
        );

        Ok(Response::new(Box::pin(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        )))
    }
}

/// Send one `ExportLedger` stream item, or stop if the client disconnected or
/// the daemon is shutting down. Used for both ledger rows and the terminal
/// error, so a full channel plus a stalled client can't leave the error send
/// blocked forever after shutdown has already fired — see `send` in
/// `rmaild/src/sync_service.rs` for the identical pattern this mirrors.
async fn send(
    tx: &tokio::sync::mpsc::Sender<Result<ProtoAuditEntry, Status>>,
    cancel: &CancellationToken,
    item: Result<ProtoAuditEntry, Status>,
) -> std::ops::ControlFlow<()> {
    tokio::select! {
        () = cancel.cancelled() => std::ops::ControlFlow::Break(()),
        sent = tx.send(item) => {
            if sent.is_ok() {
                std::ops::ControlFlow::Continue(())
            } else {
                // Receiver dropped — the client disconnected or stopped
                // reading.
                std::ops::ControlFlow::Break(())
            }
        }
    }
}

/// The cancellation token an RPC's background work runs under: a child of
/// the daemon's shutdown token, so nothing outlives the process stopping.
fn rpc_cancel(shutdown: &CancellationToken) -> CancellationToken {
    shutdown.child_token()
}

/// Reject a negative cursor rather than silently treating it as "unset" —
/// matches `SyncService::watch_events`'s handling of `since_seq < 0`
/// (`rmaild/src/sync_service.rs`). Zero and absent both mean "start from the
/// newest entry."
fn non_negative(value: Option<i64>, field: &str) -> Result<Option<i64>, Status> {
    match value {
        Some(v) if v < 0 => Err(Status::from(Error::invalid_argument(format!(
            "{field} must not be negative"
        )))),
        Some(0) | None => Ok(None),
        Some(v) => Ok(Some(v)),
    }
}

/// Convert the optional proto filter into the domain filter.
fn filter_from_proto(filter: Option<ProtoAuditFilter>) -> Result<CoreAuditFilter, Status> {
    let Some(filter) = filter else {
        return Ok(CoreAuditFilter::default());
    };
    Ok(CoreAuditFilter {
        account_id: filter.account_id,
        message_id: filter.message_id,
        model: filter.model,
        since: filter.since,
        until: filter.until,
        status: status_from_proto(filter.status)?,
    })
}

/// Parse the filter's `status`, if present and not `CALL_STATUS_UNSPECIFIED`.
fn status_from_proto(status: Option<i32>) -> Result<Option<CoreCallStatus>, Status> {
    match status {
        None => Ok(None),
        Some(raw) => match ProtoCallStatus::try_from(raw) {
            Ok(ProtoCallStatus::Unspecified) => Ok(None),
            Ok(ProtoCallStatus::Ok) => Ok(Some(CoreCallStatus::Ok)),
            Ok(ProtoCallStatus::Error) => Ok(Some(CoreCallStatus::Error)),
            Err(_) => Err(Status::from(Error::invalid_argument(format!(
                "unknown status filter value {raw}"
            )))),
        },
    }
}

/// Project a domain ledger entry onto its proto representation.
fn to_proto(entry: &LedgerEntry) -> ProtoAuditEntry {
    ProtoAuditEntry {
        id: entry.id,
        created_at: entry.created_at,
        account_id: entry.account_id,
        message_id: entry.message_id,
        request_id: entry.request_id.clone(),
        model: entry.model.clone(),
        pass: entry.pass.clone(),
        input_tokens: i64::from(entry.usage.input_tokens),
        output_tokens: i64::from(entry.usage.output_tokens),
        cache_creation_input_tokens: i64::from(entry.usage.cache_creation_input_tokens),
        cache_read_input_tokens: i64::from(entry.usage.cache_read_input_tokens),
        cost_usd: entry.cost_usd,
        redaction_level: entry.redaction_level.clone(),
        latency_ms: entry.latency_ms,
        payload_sha256: entry.payload_sha256.clone(),
        status: status_to_proto(entry.status) as i32,
        error: entry.error.clone(),
    }
}

/// Project a domain call status onto its proto representation.
fn status_to_proto(status: CoreCallStatus) -> ProtoCallStatus {
    match status {
        CoreCallStatus::Ok => ProtoCallStatus::Ok,
        CoreCallStatus::Error => ProtoCallStatus::Error,
    }
}
