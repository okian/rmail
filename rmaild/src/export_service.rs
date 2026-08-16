//! The `ExportService` gRPC implementation: request decoding, and the bridge
//! from [`rmail_core::export`]'s [`ChunkSink`] to a tonic server stream.
//!
//! Everything about *what* an export contains — which messages a query
//! selects, the mbox framing, the Maildir layout, the JSON schema — lives in
//! `rmail-core::export`. This file decodes an `ExportRequest` into that
//! module's own types, refuses what it cannot decode with the right code, and
//! forwards chunks.
//!
//! # Backpressure is the point of the sink trait
//!
//! [`ProtoSink`] awaits on a bounded channel, so a client that reads slowly
//! throttles the SQLite scan behind it. The alternative — draining the export
//! into a buffer and streaming from that — would let one `Export` call for a
//! 40 GB mailbox take the daemon's heap with it, which is precisely what
//! `rmail_core::export`'s streaming design exists to prevent, and it would be
//! undone here by a single `collect()`.
//!
//! # A cancelled export never ends `OK`
//!
//! An archive is the one kind of stream where a silent truncation is
//! unrecoverable: the client writes what it received, the file looks fine,
//! and the missing messages are noticed years later. Cancellation therefore
//! goes out as an error frame through [`crate::stream`], the same rule every
//! streaming RPC here follows, and the core exporter refuses to return a
//! successful summary for a cancelled run.

#![allow(clippy::result_large_err)]

use std::pin::Pin;

use rmail_core::export::{
    Chunk, ChunkSink, ExportOptions, ExportSummary, Exporter, Format, Selection, SinkClosed,
};
use rmail_core::{Database, Error};
use rmail_proto::v1::export_service_server::ExportService;
use rmail_proto::v1::{export_request, ExportChunk, ExportDone, ExportFormat, ExportRequest};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};
use tracing::Instrument;

/// How many chunks may sit between the exporter and a client before the scan
/// blocks.
///
/// Eight 256 KiB chunks is 2 MiB in flight — enough that a client which
/// hiccups does not stall the scan on every frame, small enough that the
/// daemon's buffering is a constant rather than a function of mailbox size.
const STREAM_BUFFER: usize = 8;

/// The `ExportService` handler.
#[derive(Debug, Clone)]
pub struct ExportApi {
    exporter: Exporter,
    /// Cancelled when the daemon shuts down, so an in-flight export stops
    /// with it rather than holding shutdown open.
    shutdown: CancellationToken,
}

impl ExportApi {
    /// Build a handler over an open database.
    #[must_use]
    pub fn new(db: Database, shutdown: CancellationToken) -> Self {
        Self {
            exporter: Exporter::new(db),
            shutdown,
        }
    }
}

#[tonic::async_trait]
impl ExportService for ExportApi {
    type ExportStream =
        Pin<Box<dyn tokio_stream::Stream<Item = Result<ExportChunk, Status>> + Send + 'static>>;

    #[tracing::instrument(skip(self, request))]
    async fn export(
        &self,
        request: Request<ExportRequest>,
    ) -> Result<Response<Self::ExportStream>, Status> {
        let req = request.into_inner();
        let selection = decode_selection(req.selection)?;
        let options = decode_options(req.format, req.with_ai, req.limit)?;

        let cancel = self.shutdown.child_token();
        // Prepared *before* the response is returned, so a refused request —
        // a thread that does not exist, `with_ai` on a byte format — arrives
        // as the call's own status rather than as the first frame of an
        // otherwise-successful stream. A client cannot tell a refusal from a
        // truncation once the stream has started, and for an archive that
        // distinction is the difference between "try again" and "this file is
        // missing mail".
        let prepared = self
            .exporter
            .prepare(&selection, &options, &cancel)
            .await
            .map_err(Status::from)?;
        let (tx, rx) = tokio::sync::mpsc::channel(STREAM_BUFFER);

        tokio::spawn(
            async move {
                let mut sink = ProtoSink {
                    tx: tx.clone(),
                    cancel: cancel.clone(),
                };
                match prepared.run(&cancel, &mut sink).await {
                    Ok(summary) => {
                        tracing::debug!(
                            messages = summary.messages,
                            bytes = summary.bytes,
                            skipped_without_raw = summary.skipped_without_raw,
                            complete = summary.complete,
                            "export finished"
                        );
                        // The sentinel, and only for a run that actually
                        // finished. `complete == false` means the consumer
                        // hung up — there is nobody to tell, and claiming
                        // completeness into a dead channel would be the one
                        // way this frame could ever lie.
                        if summary.complete {
                            let _ = send(&tx, &cancel, Ok(done_frame(&summary))).await;
                        }
                    }
                    // `send` (not a bare `tx.send`) so a cancelled export
                    // still ends with `crate::stream`'s terminal frame rather
                    // than racing it.
                    Err(error) => {
                        let _ = send(&tx, &cancel, Err(Status::from(error))).await;
                    }
                }
            }
            .instrument(tracing::Span::current()),
        );

        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }
}

/// The wire sink: converts each core chunk to its proto form and forwards it
/// under the stream's cancellation token.
struct ProtoSink {
    tx: tokio::sync::mpsc::Sender<Result<ExportChunk, Status>>,
    cancel: CancellationToken,
}

#[tonic::async_trait]
impl ChunkSink for ProtoSink {
    async fn accept(&mut self, chunk: Chunk) -> Result<(), SinkClosed> {
        match send(&self.tx, &self.cancel, Ok(to_proto(chunk))).await {
            std::ops::ControlFlow::Continue(()) => Ok(()),
            std::ops::ControlFlow::Break(()) => Err(SinkClosed),
        }
    }
}

fn to_proto(chunk: Chunk) -> ExportChunk {
    ExportChunk {
        // An empty string is the wire spelling of "this format has one
        // output"; `Option` does not survive a proto3 scalar.
        path: chunk.path.unwrap_or_default(),
        start_of_message: chunk.start_of_message,
        message_id: chunk.message_id.unwrap_or(0),
        data: chunk.data,
        done: None,
    }
}

/// The terminal frame: no path, no bytes, just the counts that let a client
/// say whether what it wrote is whole.
fn done_frame(summary: &ExportSummary) -> ExportChunk {
    ExportChunk {
        path: String::new(),
        start_of_message: false,
        message_id: 0,
        data: Vec::new(),
        done: Some(ExportDone {
            messages: i64::try_from(summary.messages).unwrap_or(i64::MAX),
            bytes: i64::try_from(summary.bytes).unwrap_or(i64::MAX),
            skipped_without_raw: i64::try_from(summary.skipped_without_raw).unwrap_or(i64::MAX),
        }),
    }
}

/// Decode the request's `selection` oneof.
///
/// An unset oneof is `INVALID_ARGUMENT`, never "everything": defaulting an
/// omitted selection to the whole mailbox is the single mistake in this RPC
/// that cannot be undone once the bytes are on someone's disk.
fn decode_selection(selection: Option<export_request::Selection>) -> Result<Selection, Status> {
    match selection {
        Some(export_request::Selection::Query(query)) => Ok(Selection::Query(query)),
        Some(export_request::Selection::ThreadId(id)) => {
            if id <= 0 {
                return Err(Status::from(Error::invalid_argument(
                    "thread_id must be positive",
                )));
            }
            Ok(Selection::Thread(id))
        }
        None => Err(Status::from(Error::invalid_argument(
            "an export must name a selection: set either `query` or `thread_id`",
        ))),
    }
}

/// Decode `format`/`with_ai`/`limit`.
///
/// `with_ai` on a non-JSON format is rejected by the core exporter too; it is
/// checked there because that is where the rule lives, and the redundancy
/// costs nothing.
fn decode_options(format: i32, with_ai: bool, limit: i32) -> Result<ExportOptions, Status> {
    let format = match ExportFormat::try_from(format) {
        Ok(ExportFormat::Mbox) => Format::Mbox,
        Ok(ExportFormat::Maildir) => Format::Maildir,
        Ok(ExportFormat::Eml) => Format::Eml,
        Ok(ExportFormat::Json) => Format::Json,
        Ok(ExportFormat::Unspecified) | Err(_) => {
            return Err(Status::from(Error::invalid_argument(
                "format must be one of EXPORT_FORMAT_MBOX, EXPORT_FORMAT_MAILDIR, \
                 EXPORT_FORMAT_EML, EXPORT_FORMAT_JSON",
            )))
        }
    };
    if limit < 0 {
        return Err(Status::from(Error::invalid_argument(
            "limit must not be negative",
        )));
    }
    Ok(ExportOptions {
        format,
        with_ai,
        limit: (limit > 0).then_some(i64::from(limit)),
    })
}

/// Send one stream item, giving up if the client went away or the daemon is
/// stopping. See `rmaild::mail_service::send` — identical reasoning.
async fn send<T>(
    tx: &tokio::sync::mpsc::Sender<Result<T, Status>>,
    cancel: &CancellationToken,
    item: Result<T, Status>,
) -> std::ops::ControlFlow<()> {
    tokio::select! {
        () = cancel.cancelled() => {
            crate::stream::terminate_cancelled(tx).await;
            std::ops::ControlFlow::Break(())
        }
        sent = tx.send(item) => {
            if sent.is_ok() {
                std::ops::ControlFlow::Continue(())
            } else {
                std::ops::ControlFlow::Break(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use tonic::Code;

    #[test]
    fn an_unset_selection_is_invalid_argument() {
        let status = decode_selection(None).unwrap_err();
        assert_eq!(status.code(), Code::InvalidArgument);
    }

    #[test]
    fn a_non_positive_thread_id_is_invalid_argument() {
        for id in [0, -1] {
            let status =
                decode_selection(Some(export_request::Selection::ThreadId(id))).unwrap_err();
            assert_eq!(status.code(), Code::InvalidArgument, "thread_id {id}");
        }
    }

    #[test]
    fn an_unspecified_or_unknown_format_is_invalid_argument() {
        for format in [ExportFormat::Unspecified as i32, 99] {
            let status = decode_options(format, false, 0).unwrap_err();
            assert_eq!(status.code(), Code::InvalidArgument, "format {format}");
        }
    }

    #[test]
    fn a_zero_limit_means_no_limit_and_a_negative_one_is_refused() {
        let options = decode_options(ExportFormat::Mbox as i32, false, 0).unwrap();
        assert_eq!(options.limit, None);
        let options = decode_options(ExportFormat::Mbox as i32, false, 5).unwrap();
        assert_eq!(options.limit, Some(5));
        let status = decode_options(ExportFormat::Mbox as i32, false, -1).unwrap_err();
        assert_eq!(status.code(), Code::InvalidArgument);
    }

    #[test]
    fn every_wire_format_decodes_to_its_core_counterpart() {
        for (wire, expected) in [
            (ExportFormat::Mbox, Format::Mbox),
            (ExportFormat::Maildir, Format::Maildir),
            (ExportFormat::Eml, Format::Eml),
            (ExportFormat::Json, Format::Json),
        ] {
            let options = decode_options(wire as i32, false, 0).unwrap();
            assert_eq!(options.format, expected);
        }
    }
}
