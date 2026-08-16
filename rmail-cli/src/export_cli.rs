//! `mail export` — a thin gRPC-client verb over `ExportService.Export`
//! (task 82).
//!
//! # The archive is written here, by shared code
//!
//! The daemon streams framed bytes; turning them back into an mbox file, a
//! Maildir tree or a directory of `.eml` files is
//! [`rmail_core::export::write::DestinationWriter`]'s job, not this file's.
//! That module also owns the path check that keeps a server-supplied entry
//! name inside the directory the user named — a check every client of this
//! RPC needs and none should be reimplementing.
//!
//! # One blocking task, not one per chunk
//!
//! Writing files is blocking work, and blocking the runtime is not allowed.
//! Rather than hopping into `spawn_blocking` for every 256 KiB frame, the
//! writer runs inside a *single* blocking task fed by a bounded channel and
//! draining it with `blocking_recv`. The channel's bound is what gives the
//! gRPC stream backpressure when the disk is slower than the socket, and the
//! writer's own errors come back through the join handle rather than being
//! discovered after the stream has already been consumed.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use rmail_core::export::write::DestinationWriter;
use rmail_core::export::{Chunk, Format};
use rmail_proto::v1::export_service_client::ExportServiceClient;
use rmail_proto::v1::{export_request, ExportChunk, ExportFormat, ExportRequest};
use tokio_stream::StreamExt;

/// How many chunks may sit between the gRPC stream and the writer task.
///
/// Small on purpose: the point is to keep the disk and the socket coupled, so
/// a slow disk throttles the daemon's scan instead of letting this process
/// buffer an archive it has not written yet.
const WRITE_QUEUE: usize = 4;

/// `mail export <query> --format <fmt> -o <path>`.
#[derive(Debug, clap::Args)]
pub struct ExportArgs {
    /// The query to export (`from:alice has:attachment "office move"`).
    /// Every message it selects is exported — this is not a ranked page, so
    /// nothing is dropped for scoring below a cutoff. Omit it (and pass
    /// `--thread`) to export a conversation instead.
    query: Option<String>,

    /// Export one thread, oldest message first, instead of a query.
    #[arg(long, conflicts_with = "query")]
    thread: Option<i64>,

    /// Archive format.
    #[arg(long, short = 'f', default_value = "mbox",
          value_parser = ["mbox", "maildir", "eml", "json"])]
    format: String,

    /// Where to write it: a file for `mbox`/`json`, a directory for
    /// `maildir`/`eml`. `-` writes a single-document format to stdout.
    #[arg(long, short = 'o')]
    out: PathBuf,

    /// Attach the AI summaries and tags already stored for each message.
    /// Only meaningful with `--format json`; it never calls a model.
    #[arg(long)]
    with_ai: bool,

    /// Stop after this many messages (0 = no limit).
    #[arg(long, default_value_t = 0)]
    limit: i32,

    /// Overwrite an existing archive file. Without it, `mail export` refuses
    /// rather than truncating one.
    #[arg(long)]
    force: bool,
}

/// Run the export.
pub async fn export(socket: &Path, args: ExportArgs) -> Result<()> {
    let format: Format = args
        .format
        .parse()
        .map_err(|e: rmail_core::Error| anyhow::anyhow!("{e}"))?;

    let selection = match (args.query, args.thread) {
        (Some(query), None) => export_request::Selection::Query(query),
        (None, Some(thread_id)) => export_request::Selection::ThreadId(thread_id),
        (None, None) => bail!("give a query to export, or --thread <id>"),
        // `conflicts_with` already refuses this; the arm exists so the match
        // is total without an unreachable panic.
        (Some(_), Some(_)) => bail!("give either a query or --thread, not both"),
    };

    let to_stdout = args.out.as_os_str() == "-";
    if to_stdout && !format.is_single_stream() {
        bail!("--format {format} writes one file per message; give a directory with -o, not `-`");
    }

    // A single-document format truncates its destination. For a tool whose
    // whole purpose is preservation, silently replacing yesterday's archive
    // with today's shorter one is not an acceptable default — and unlike the
    // directory formats (whose entry names are per-message, so re-exporting
    // overwrites each message with itself) there is no version of that which
    // is harmless.
    if !to_stdout && format.is_single_stream() && args.out.exists() && !args.force {
        bail!(
            "{} already exists; pass --force to replace it",
            args.out.display()
        );
    }

    let channel = rmail_core::connect_uds(socket)
        .await
        .with_context(|| format!("connecting to rmaild at {}", socket.display()))?;
    let mut client = ExportServiceClient::new(channel);

    let mut stream = client
        .export(ExportRequest {
            selection: Some(selection),
            format: wire_format(format) as i32,
            with_ai: args.with_ai,
            limit: args.limit,
        })
        .await
        .context("ExportService.Export")?
        .into_inner();

    let (tx, mut rx) = tokio::sync::mpsc::channel::<Chunk>(WRITE_QUEUE);
    let destination = args.out.clone();
    let writer_task = tokio::task::spawn_blocking(move || -> Result<()> {
        let mut writer = if to_stdout {
            DestinationWriter::to_writer(format, Box::new(std::io::stdout()))?
        } else {
            DestinationWriter::create(format, &destination)
                .with_context(|| format!("creating {}", destination.display()))?
        };
        while let Some(chunk) = rx.blocking_recv() {
            writer.apply(&chunk)?;
        }
        writer.finish()?;
        Ok(())
    });

    let mut messages = 0u64;
    let mut bytes = 0u64;
    let mut stream_error = None;
    let mut done = None;
    while let Some(item) = stream.next().await {
        let chunk = match item {
            Ok(chunk) => chunk,
            Err(status) => {
                stream_error = Some(status);
                break;
            }
        };
        if let Some(summary) = chunk.done {
            // The terminal frame carries no bytes and ends the archive.
            done = Some(summary);
            break;
        }
        if chunk.start_of_message {
            messages += 1;
        }
        bytes += chunk.data.len() as u64;
        if tx.send(from_proto(chunk)).await.is_err() {
            // The writer died; its error is the real one, so stop feeding it
            // and let the join below report why.
            break;
        }
    }
    // Closing the channel is what ends the writer's loop; it must happen
    // before awaiting the join or this deadlocks.
    drop(tx);
    writer_task.await.context("export writer task")??;

    // The partial archive is left on disk deliberately in both failure paths
    // below — deleting a half-written export would destroy the only copy of
    // whatever the caller did receive. Saying it is partial is what stops it
    // being mistaken for a whole one.
    if let Some(status) = stream_error {
        bail!(
            "export failed after {messages} message(s); {} is incomplete: {status}",
            describe(&args.out)
        );
    }
    // A gRPC stream that stops yielding ends OK. Without this check a daemon
    // that shut down mid-export — and whose best-effort cancellation frame did
    // not make it out — would leave a truncated archive and a zero exit code.
    let Some(done) = done else {
        bail!(
            "the export stream ended without a completion marker after {messages} message(s); \
             {} is incomplete",
            describe(&args.out)
        );
    };

    if !to_stdout {
        // stdout is the archive when `-o -`; a summary line would corrupt it.
        let mut stderr = std::io::stderr();
        let _ = writeln!(
            stderr,
            "exported {messages} message(s), {bytes} byte(s) to {}",
            describe(&args.out)
        );
    }
    // Printed even when the archive itself went to stdout: it is a warning on
    // stderr, so it cannot corrupt the document, and it is the one thing a
    // caller most needs to hear.
    if done.skipped_without_raw > 0 {
        let _ = writeln!(
            std::io::stderr(),
            "warning: {} message(s) matched but had no stored raw RFC822 and are not in the \
             archive",
            done.skipped_without_raw
        );
    }
    Ok(())
}

fn describe(out: &Path) -> String {
    if out.as_os_str() == "-" {
        "stdout".to_owned()
    } else {
        out.display().to_string()
    }
}

const fn wire_format(format: Format) -> ExportFormat {
    match format {
        Format::Mbox => ExportFormat::Mbox,
        Format::Maildir => ExportFormat::Maildir,
        Format::Eml => ExportFormat::Eml,
        Format::Json => ExportFormat::Json,
    }
}

fn from_proto(chunk: ExportChunk) -> Chunk {
    Chunk {
        path: (!chunk.path.is_empty()).then_some(chunk.path),
        start_of_message: chunk.start_of_message,
        message_id: (chunk.message_id != 0).then_some(chunk.message_id),
        data: chunk.data,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    #[test]
    fn an_empty_wire_path_decodes_to_a_single_stream_chunk() {
        let chunk = from_proto(ExportChunk {
            path: String::new(),
            start_of_message: true,
            message_id: 7,
            data: b"x".to_vec(),
            done: None,
        });
        assert_eq!(chunk.path, None);
        assert_eq!(chunk.message_id, Some(7));
    }

    #[test]
    fn a_named_wire_path_and_zero_message_id_decode_to_options() {
        let chunk = from_proto(ExportChunk {
            path: "cur/1.rmail:2,S".to_owned(),
            start_of_message: false,
            message_id: 0,
            data: Vec::new(),
            done: None,
        });
        assert_eq!(chunk.path.as_deref(), Some("cur/1.rmail:2,S"));
        assert_eq!(chunk.message_id, None);
    }

    #[test]
    fn every_format_maps_to_a_specified_wire_value() {
        for format in Format::ALL {
            assert_ne!(wire_format(format), ExportFormat::Unspecified, "{format}");
        }
    }
}
