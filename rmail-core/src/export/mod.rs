//! Multi-format export: turning a query or a thread into an mbox, a Maildir,
//! a directory of `.eml` files, or a JSON document (task 82; prd.md's
//! "Multi-Format Export").
//!
//! # The archive is the raw bytes, not a re-render
//!
//! Every format here carries `messages.raw` — the exact RFC822 octets the
//! IMAP server delivered and task 9 stored — and nothing else. No format
//! rebuilds a message from `subject`/`from_addr`/`body_text`, because a
//! rebuilt message is a *different* message: header order, folding,
//! `Content-Transfer-Encoding`, DKIM signatures and MIME boundaries all
//! change, and every one of those is something an archive is later asked to
//! prove. Line endings are likewise left alone — a message that arrived
//! CRLF is exported CRLF, not normalized to LF for the reader's convenience.
//!
//! Two formats add bytes, and both additions are reversible:
//!
//! - **mbox** prepends a `From_` separator line per message and applies
//!   *mboxrd* quoting (`>From ` for any line matching `^>*From `) so a line
//!   in the body can never be mistaken for a separator. [`mbox`]'s own docs
//!   spell out the exact inverse; `export::tests` reverses it and asserts
//!   byte equality with the original.
//! - **JSON** carries the raw bytes base64-encoded under `raw_rfc822_base64`
//!   alongside parsed metadata, which is decode-and-compare reversible.
//!
//! Maildir and `.eml` write the stored bytes verbatim, so their round trip is
//! `assert_eq!`.
//!
//! # A set, not a ranked page
//!
//! [`Selection::Query`] runs the operator grammar
//! ([`crate::query::parse`]) and the lexical index, **not** the ranking
//! pipeline. An export is an archive: it must contain every message the query
//! selects, in a deterministic order, with no relevance cutoff and no
//! server-side page cap. Reusing `SearchService`'s pipeline would have given
//! the opposite — the top *N* by score, re-ordered by a reranker, silently
//! truncated — which is a fine answer to "what am I looking for" and a
//! corrupt answer to "archive this".
//!
//! Concretely, [`select`] compiles the query's operators through
//! [`crate::retrieve::filtermask`] (the same compiler five retrievers share,
//! so `-in:Spam` means here what it means in search) and its free text through
//! [`crate::retrieve::lexical`]'s own `MATCH` builder (so `~`-forced-semantic
//! terms, negation and phrase quoting behave identically). Both are reused
//! rather than re-derived precisely so an export and a search can never
//! disagree about what a query *means*.
//!
//! # Streaming, and what bounds memory
//!
//! Ids are pulled from SQLite a page at a time by keyset pagination
//! (`id > last`), and each message's raw blob is loaded, framed and emitted
//! immediately before the next one is touched. Peak memory is therefore one
//! page of `i64` ids plus one message's bytes — not the mailbox, and not the
//! archive. That is what lets a 40 GB mbox come out of a daemon with a
//! bounded heap.
//!
//! Chunks go to a [`ChunkSink`], which is what makes the same code path
//! usable from a gRPC handler (an `mpsc::Sender` with real backpressure) and
//! from a test (a `Vec`).
//!
//! # `--with-ai` attaches; it never calls a model
//!
//! [`ExportOptions::with_ai`] joins the AI artifacts that tasks 48/49/55
//! already stored — `ai_summaries` rows and applied tags — onto each JSON
//! record, batched one query per page rather than one per message. It does
//! not summarize anything: nothing in this module reaches
//! [`crate::ai`](crate::ai) or a provider, so an export costs nothing, needs
//! no budget, and returns identical bytes twice. That is also why task 82
//! depends on 9 and 39 and not on the AI passes.
//!
//! # Writing the stream back out
//!
//! [`write`] is the other half: it applies a chunk stream to a filesystem
//! destination, and it validates every `path` it is handed (relative, no
//! `..`, no absolute prefix) rather than trusting the sender. The `mail`
//! CLI is a gRPC client talking to a daemon over a socket; "the server said
//! to write there" is not a reason to write outside the directory the user
//! named.

use std::collections::BTreeMap;

use tokio_util::sync::CancellationToken;

use crate::error::Error;
use crate::repo;
use crate::storage::Database;

pub mod json;
pub mod maildir;
pub mod mbox;
pub mod select;
pub mod write;

#[cfg(test)]
mod tests;

/// How many bytes of a message's framed output travel in one [`Chunk`].
///
/// 256 KiB, matching `rmaild::mail_service`'s attachment chunking, and for
/// the same reason: far enough under gRPC's 16 MiB frame cap that no
/// individual frame can approach it, large enough that a big mailbox is not
/// paying per-frame overhead millions of times.
pub const CHUNK_BYTES: usize = 256 * 1024;

/// How many message ids one keyset page pulls from SQLite.
///
/// Bounds the id buffer (`PAGE_SIZE * 8` bytes) and is also the batch size
/// for the AI-artifact join, which is one query per page rather than one per
/// message — prd.md's "batch-attaches".
const PAGE_SIZE: usize = 256;

/// The archive formats an export can produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Format {
    /// One document: messages concatenated behind `From_` lines, mboxrd
    /// quoted. See [`mbox`].
    Mbox,
    /// A Maildir tree (`tmp/`, `new/`, `cur/`), one file per message under
    /// `cur/` with flags in its `:2,` info suffix. See [`maildir`].
    Maildir,
    /// One `.eml` file per message, byte-identical to the stored raw.
    Eml,
    /// One JSON document with metadata, base64 raw, and optionally the
    /// stored AI artifacts. See [`json`].
    Json,
}

impl Format {
    /// Every format, for exhaustive iteration in tests and CLI help.
    pub const ALL: [Format; 4] = [Format::Mbox, Format::Maildir, Format::Eml, Format::Json];

    /// The lowercase name used on the CLI and in error messages.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Format::Mbox => "mbox",
            Format::Maildir => "maildir",
            Format::Eml => "eml",
            Format::Json => "json",
        }
    }

    /// Whether this format's output is one document (mbox, JSON) rather than
    /// one file per message (Maildir, `.eml`).
    ///
    /// The distinction every consumer branches on: a single-stream format's
    /// chunks all carry an empty [`Chunk::path`] and append to one output; a
    /// per-file format's chunks name the file they belong to.
    #[must_use]
    pub const fn is_single_stream(self) -> bool {
        matches!(self, Format::Mbox | Format::Json)
    }
}

impl std::fmt::Display for Format {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for Format {
    type Err = Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Format::ALL
            .into_iter()
            .find(|format| format.as_str() == s)
            .ok_or_else(|| {
                Error::invalid_argument(format!(
                    "unknown export format {s:?}; expected one of mbox, maildir, eml, json"
                ))
            })
    }
}

/// What to export.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    /// Every message a search query selects, in `messages.id` order.
    Query(String),
    /// Every message in one thread, oldest first — the order
    /// `MailService.GetThread` already promises a conversation reads in.
    Thread(i64),
}

/// How to export it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExportOptions {
    /// The archive format.
    pub format: Format,
    /// Attach the AI artifacts already stored for each message. Only
    /// [`Format::Json`] has anywhere to put them; see
    /// [`Exporter::export`]'s errors.
    pub with_ai: bool,
    /// Stop after this many *exported* messages. `None` means the whole
    /// selection.
    ///
    /// Counted against what actually lands in the archive, not against rows
    /// the selection matched: a row with no stored raw is skipped by the byte
    /// formats (see [`ExportSummary::skipped_without_raw`]) and does not
    /// consume the budget, so `--limit 10` yields ten messages or the whole
    /// selection, never eight.
    pub limit: Option<i64>,
}

impl ExportOptions {
    /// The plain options for a format: no AI attachment, no limit.
    #[must_use]
    pub const fn new(format: Format) -> Self {
        Self {
            format,
            with_ai: false,
            limit: None,
        }
    }
}

/// One frame of an export stream.
///
/// Frames arrive in order, and every frame belonging to one message arrives
/// contiguously. Appending each frame's `data` to the output named by `path`
/// reproduces the archive exactly — which is what [`write::DestinationWriter`]
/// does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// Relative path within the destination, for the per-file formats.
    /// `None` for [`Format::is_single_stream`] formats.
    pub path: Option<String>,
    /// True on the first frame of a message's bytes.
    pub start_of_message: bool,
    /// The message these bytes came from, or `None` for framing bytes
    /// belonging to no single message (JSON's opening/closing braces).
    pub message_id: Option<i64>,
    /// The bytes.
    pub data: Vec<u8>,
}

/// What an export produced.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExportSummary {
    /// Messages written.
    pub messages: u64,
    /// Total bytes across every chunk.
    pub bytes: u64,
    /// Messages the selection matched but that carried no stored raw RFC822.
    ///
    /// Counted rather than silently dropped, and never fabricated: an archive
    /// that invented bytes for a message whose originals were never stored
    /// would be worse than one that says it skipped it. The byte formats have
    /// nothing to write for such a message; [`Format::Json`] still emits its
    /// record, with `raw_rfc822_base64` null, and does **not** count it here
    /// (nothing was skipped).
    ///
    /// This reaches the caller — `ExportChunk.done` on the wire, a warning
    /// line from `mail export`. A count nobody is shown is the same as no
    /// count.
    pub skipped_without_raw: u64,
    /// Whether the whole selection was written.
    ///
    /// `false` means the consumer went away mid-export. It is not an error —
    /// there is nobody left to tell — but it is the difference between a
    /// summary that may be published as "this archive is complete" and one
    /// that may not, so it travels *with* the summary rather than being
    /// inferred from the absence of an error.
    pub complete: bool,
}

/// The sink half of an export: where [`Exporter::export`] puts its chunks.
///
/// A trait rather than a concrete `mpsc::Sender` so the daemon's streaming
/// handler and an in-memory caller (tests, a future `mail import` round trip)
/// drive byte-for-byte the same code path. There is no second export
/// implementation for the in-memory case to drift from.
#[async_trait::async_trait]
pub trait ChunkSink: Send {
    /// Accept one chunk.
    ///
    /// # Errors
    ///
    /// [`SinkClosed`] when the consumer has gone away — a client that
    /// hung up mid-export. [`PreparedExport::run`] treats that as a stop, not
    /// a fault, and reports it as [`ExportSummary::complete`] `== false`.
    ///
    /// # Cancellation
    ///
    /// An implementation that can block indefinitely (a bounded channel with
    /// a stopped reader) must honor the export's cancellation token itself:
    /// [`PreparedExport::run`] checks the token *before* each `accept`, which
    /// cannot interrupt one already parked. `rmaild`'s own sink selects on the
    /// token; that is why no blanket impl for `tokio::sync::mpsc::Sender` is
    /// offered here — it would look like the obvious choice and would silently
    /// ignore cancellation for as long as its consumer stayed stalled.
    async fn accept(&mut self, chunk: Chunk) -> Result<(), SinkClosed>;
}

/// The consumer of an export stream has gone away.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SinkClosed;

impl std::fmt::Display for SinkClosed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the export consumer closed the stream")
    }
}

impl std::error::Error for SinkClosed {}

#[async_trait::async_trait]
impl ChunkSink for Vec<Chunk> {
    async fn accept(&mut self, chunk: Chunk) -> Result<(), SinkClosed> {
        self.push(chunk);
        Ok(())
    }
}

/// Everything one message contributes to an archive.
///
/// Assembled per message immediately before framing and dropped immediately
/// after, which is what keeps peak memory at one message rather than one
/// mailbox.
#[derive(Debug, Clone)]
pub struct LoadedMessage {
    /// The message row, including its raw RFC822 (`None` when the row has
    /// none — see [`ExportSummary::skipped_without_raw`]).
    pub message: repo::Message,
    /// The mailbox name the message lives in, if its row is still there.
    pub mailbox: Option<String>,
    /// IMAP flags, sorted.
    pub flags: Vec<String>,
    /// Attachment metadata (never bytes — those are already inside `raw`).
    pub attachments: Vec<repo::Attachment>,
    /// Stored AI artifacts, present only when
    /// [`ExportOptions::with_ai`] is set.
    pub ai: Option<json::AiArtifacts>,
}

/// The export engine: reads the local mirror, frames it, and hands chunks to
/// a sink.
///
/// Cheap to clone; every clone shares the database handle.
#[derive(Debug, Clone)]
pub struct Exporter {
    db: Database,
}

impl Exporter {
    /// Build an exporter over an open database.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Validate a request and resolve its selection, producing no bytes.
    ///
    /// Everything that can be the *caller's* fault happens here — a bad
    /// format/flag combination, a negative limit, a thread that does not
    /// exist — so a streaming handler can reject a bad request with a status
    /// on the call itself instead of as the first frame of an otherwise
    /// successful stream. That distinction is what lets a client tell "this
    /// request was refused" from "this archive is truncated", and it is why
    /// `mail export` does not leave an empty file behind for a typo'd thread
    /// id.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidArgument`] for a negative limit, or `with_ai` on a
    ///   format with nowhere to put the artifacts.
    /// - [`Error::NotFound`] if [`Selection::Thread`] names no thread.
    /// - [`Error::Cancelled`] if `cancel` has already fired.
    /// - A mapped storage error otherwise.
    #[tracing::instrument(
        skip(self, selection, cancel),
        fields(
            format = options.format.as_str(),
            with_ai = options.with_ai,
            // The *kind* of selection, never its text: a query is user
            // content, the same rule `query::plan` applies to its own `raw`.
            selection = selection_kind(selection),
        )
    )]
    pub async fn prepare(
        &self,
        selection: &Selection,
        options: &ExportOptions,
        cancel: &CancellationToken,
    ) -> Result<PreparedExport, Error> {
        if options.with_ai && options.format != Format::Json {
            return Err(Error::invalid_argument(format!(
                "--with-ai attaches AI artifacts to the JSON export; format {} carries verbatim \
                 RFC822 and has nowhere to put them",
                options.format
            )));
        }
        if options.limit.is_some_and(|limit| limit < 0) {
            return Err(Error::invalid_argument("export limit must not be negative"));
        }
        let cursor = select::Cursor::open(&self.db, selection, cancel).await?;
        Ok(PreparedExport {
            exporter: self.clone(),
            cursor,
            framer: Framer::new(options.format),
            options: options.clone(),
        })
    }

    /// [`Exporter::prepare`] followed by [`PreparedExport::run`].
    ///
    /// The convenience form, for callers with nowhere useful to put the
    /// distinction between a refused request and a failed one.
    ///
    /// # Errors
    ///
    /// As [`Exporter::prepare`] and [`PreparedExport::run`].
    pub async fn export<S: ChunkSink>(
        &self,
        selection: &Selection,
        options: &ExportOptions,
        cancel: &CancellationToken,
        sink: &mut S,
    ) -> Result<ExportSummary, Error> {
        self.prepare(selection, options, cancel)
            .await?
            .run(cancel, sink)
            .await
    }
}

/// A validated export, ready to stream. See [`Exporter::prepare`].
#[derive(Debug)]
pub struct PreparedExport {
    exporter: Exporter,
    cursor: select::Cursor,
    framer: Framer,
    options: ExportOptions,
}

impl PreparedExport {
    /// Stream the archive into `sink`.
    ///
    /// Returns once the whole selection has been framed, the sink hung up, or
    /// `cancel` fired. A cancelled export returns [`Error::Cancelled`] rather
    /// than a short-but-successful summary: a truncated archive that reported
    /// success would be indistinguishable from a complete one.
    ///
    /// # Errors
    ///
    /// - [`Error::InvalidArgument`] if the query's free text compiled to
    ///   something FTS5 rejects.
    /// - [`Error::Cancelled`] if `cancel` fires mid-export.
    /// - A mapped storage error otherwise.
    #[tracing::instrument(
        skip(self, sink, cancel),
        fields(
            format = self.options.format.as_str(),
            messages,
            bytes,
        )
    )]
    pub async fn run<S: ChunkSink>(
        self,
        cancel: &CancellationToken,
        sink: &mut S,
    ) -> Result<ExportSummary, Error> {
        let mut summary = ExportSummary::default();
        let result = self.stream(cancel, sink, &mut summary).await;
        // Recorded here rather than at the end of `stream`, so a run that
        // stopped early — a closed sink, a cancelled token, a storage error —
        // still leaves its counts on the span. Those are the runs whose
        // telemetry is worth having.
        let span = tracing::Span::current();
        span.record("messages", summary.messages);
        span.record("bytes", summary.bytes);
        if summary.skipped_without_raw > 0 {
            // Once, with the total — not once per row. A mailbox whose raw
            // blobs were lost wholesale would otherwise emit a warning per
            // message and bury everything else in the log.
            tracing::warn!(
                skipped = summary.skipped_without_raw,
                "messages had no stored raw RFC822 and are absent from the archive"
            );
        }
        result.map(|()| summary)
    }

    /// The body of [`PreparedExport::run`], writing its progress into
    /// `summary` as it goes so the caller can report it whichever way this
    /// returns.
    async fn stream<S: ChunkSink>(
        mut self,
        cancel: &CancellationToken,
        sink: &mut S,
        summary: &mut ExportSummary,
    ) -> Result<(), Error> {
        let db = self.exporter.db.clone();
        let options = self.options.clone();
        let cursor = &mut self.cursor;
        let framer = &mut self.framer;

        if let Some(prologue) = framer.prologue() {
            if emit(
                sink,
                cancel,
                summary,
                Chunk {
                    path: None,
                    start_of_message: false,
                    message_id: None,
                    data: prologue,
                },
            )
            .await?
            .is_closed()
            {
                return Ok(());
            }
        }

        'pages: loop {
            let page = cursor.next_page(&db, cancel).await?;
            if page.is_empty() {
                break;
            }
            // Everything that can be fetched for the whole page in one
            // statement is — flags, and (for `--with-ai`) summaries and tags.
            // That is prd.md's "batch-attaches".
            let context = PageContext::for_page(&db, &page, &options, cancel).await?;
            for id in page {
                // The raw blob is fetched one message at a time, deliberately:
                // a page of 256 ids is two kilobytes, a page of 256 raw bodies
                // is unbounded. This loop is what keeps peak memory at one
                // message rather than one page.
                let Some(loaded) = context.load(&db, id, &options, cancel).await? else {
                    // Deleted between the id scan and now (see `select`'s
                    // "not a snapshot" note). Skipping is the only correct
                    // answer — there is nothing left to archive.
                    continue;
                };
                let Some(entry) = framer.frame(&loaded)? else {
                    summary.skipped_without_raw += 1;
                    tracing::debug!(
                        message_id = loaded.message.id,
                        "message has no stored raw RFC822; excluded from the archive"
                    );
                    continue;
                };
                summary.messages += 1;
                if emit_entry(sink, cancel, summary, &entry, loaded.message.id)
                    .await?
                    .is_closed()
                {
                    return Ok(());
                }
                // The budget is spent on messages that reached the archive,
                // not on rows the selection matched — see
                // [`ExportOptions::limit`].
                if options
                    .limit
                    .is_some_and(|limit| summary.messages >= limit as u64)
                {
                    break 'pages;
                }
            }
        }

        if let Some(epilogue) = framer.epilogue() {
            if emit(
                sink,
                cancel,
                summary,
                Chunk {
                    path: None,
                    start_of_message: false,
                    message_id: None,
                    data: epilogue,
                },
            )
            .await?
            .is_closed()
            {
                return Ok(());
            }
        }

        summary.complete = true;
        Ok(())
    }
}

/// Whether the sink is still taking chunks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SinkState {
    Open,
    Closed,
}

impl SinkState {
    const fn is_closed(self) -> bool {
        matches!(self, SinkState::Closed)
    }
}

/// Send one chunk, accounting its bytes, and translate a cancelled token into
/// [`Error::Cancelled`].
///
/// The token is checked on both sides of the `accept`. Before, so a cancelled
/// export stops without doing more work; and **after**, because a sink that
/// itself selects on the token (`rmaild`'s does, so a cancelled stream can
/// emit its terminal frame) reports cancellation as [`SinkClosed`], which is
/// otherwise indistinguishable from an ordinary client hang-up. Without the
/// second check a daemon shutdown mid-export would return a summary — short,
/// and marked complete only because nothing said otherwise.
async fn emit<S: ChunkSink>(
    sink: &mut S,
    cancel: &CancellationToken,
    summary: &mut ExportSummary,
    chunk: Chunk,
) -> Result<SinkState, Error> {
    if cancel.is_cancelled() {
        return Err(Error::cancelled("export cancelled"));
    }
    let bytes = chunk.data.len() as u64;
    match sink.accept(chunk).await {
        Ok(()) => {
            summary.bytes += bytes;
            Ok(SinkState::Open)
        }
        Err(SinkClosed) if cancel.is_cancelled() => Err(Error::cancelled("export cancelled")),
        Err(SinkClosed) => Ok(SinkState::Closed),
    }
}

/// Split one framed message across [`CHUNK_BYTES`]-sized chunks.
///
/// A zero-byte body still emits exactly one chunk, so `start_of_message` is
/// never lost for a message whose framing happened to produce nothing — the
/// same rule `mail_service`'s attachment chunking follows.
async fn emit_entry<S: ChunkSink>(
    sink: &mut S,
    cancel: &CancellationToken,
    summary: &mut ExportSummary,
    entry: &FramedEntry,
    message_id: i64,
) -> Result<SinkState, Error> {
    let mut offset = 0;
    let mut first = true;
    loop {
        let end = (offset + CHUNK_BYTES).min(entry.body.len());
        let chunk = Chunk {
            path: entry.path.clone(),
            start_of_message: first,
            message_id: Some(message_id),
            data: entry.body[offset..end].to_vec(),
        };
        if emit(sink, cancel, summary, chunk).await?.is_closed() {
            return Ok(SinkState::Closed);
        }
        first = false;
        offset = end;
        if offset >= entry.body.len() {
            return Ok(SinkState::Open);
        }
    }
}

/// One message's framed bytes and the file they belong to.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FramedEntry {
    /// `None` for the single-stream formats.
    path: Option<String>,
    body: Vec<u8>,
}

/// The per-format byte layer.
///
/// One enum rather than a `dyn` trait: there are exactly four formats, they
/// are named in the proto, and a closed set is what lets the compiler catch a
/// fifth one being added without a framing rule.
#[derive(Debug)]
enum Framer {
    Mbox,
    Maildir,
    Eml,
    Json(json::JsonFramer),
}

impl Framer {
    fn new(format: Format) -> Self {
        match format {
            Format::Mbox => Framer::Mbox,
            Format::Maildir => Framer::Maildir,
            Format::Eml => Framer::Eml,
            Format::Json => Framer::Json(json::JsonFramer::default()),
        }
    }

    /// Bytes that open the archive, before any message.
    fn prologue(&self) -> Option<Vec<u8>> {
        match self {
            Framer::Json(framer) => Some(framer.prologue()),
            _ => None,
        }
    }

    /// Frame one message, or `None` when the format has nothing to write for
    /// it (a byte format and a row with no stored raw).
    fn frame(&mut self, loaded: &LoadedMessage) -> Result<Option<FramedEntry>, Error> {
        match self {
            Framer::Mbox => Ok(loaded.message.raw.as_deref().map(|raw| FramedEntry {
                path: None,
                body: mbox::frame(&loaded.message, raw),
            })),
            Framer::Maildir => Ok(loaded.message.raw.as_deref().map(|raw| FramedEntry {
                path: Some(maildir::entry_path(&loaded.message, &loaded.flags)),
                body: raw.to_vec(),
            })),
            Framer::Eml => Ok(loaded.message.raw.as_deref().map(|raw| FramedEntry {
                path: Some(eml_entry_path(&loaded.message)),
                body: raw.to_vec(),
            })),
            // JSON exports the record whether or not the raw survived: the
            // metadata is still true, and `raw_rfc822_base64` being null says
            // exactly what is missing.
            Framer::Json(framer) => Ok(Some(FramedEntry {
                path: None,
                body: framer.frame(loaded)?,
            })),
        }
    }

    /// Bytes that close the archive, after the last message.
    fn epilogue(&self) -> Option<Vec<u8>> {
        match self {
            Framer::Json(framer) => Some(framer.epilogue()),
            _ => None,
        }
    }
}

/// The filename one message gets in an `.eml` export: `<id>-<slug>.eml`.
///
/// The id prefix is what makes the name unique — two messages with identical
/// subjects (a mailing list is nothing but that) must not collide, and
/// `messages.id` is the only thing guaranteed distinct. The slug is a
/// courtesy for whoever opens the directory, and is dropped entirely when the
/// subject has no characters that survive sanitizing.
fn eml_entry_path(message: &repo::Message) -> String {
    let slug = slugify(message.subject.as_deref().unwrap_or_default());
    if slug.is_empty() {
        format!("{}.eml", message.id)
    } else {
        format!("{}-{slug}.eml", message.id)
    }
}

/// How many characters of a subject survive into a filename.
const SLUG_MAX: usize = 60;

/// Reduce a subject to `[a-z0-9-]`, collapsing every run of anything else to
/// a single `-`.
///
/// Deliberately aggressive. A subject is attacker-controlled text that is
/// about to become a path component: allowing anything beyond this set means
/// reasoning about `..`, `/`, NUL, leading dashes, Unicode normalization
/// collisions on macOS, and reserved names on other platforms. An allowlist
/// of three character classes has none of those questions.
fn slugify(subject: &str) -> String {
    let mut out = String::new();
    let mut pending_sep = false;
    for ch in subject.chars() {
        if ch.is_ascii_alphanumeric() {
            if pending_sep && !out.is_empty() {
                out.push('-');
            }
            pending_sep = false;
            out.extend(ch.to_lowercase());
            if out.chars().count() >= SLUG_MAX {
                break;
            }
        } else {
            pending_sep = true;
        }
    }
    out
}

/// The span field for a selection: its *kind*, never its text.
const fn selection_kind(selection: &Selection) -> &'static str {
    match selection {
        Selection::Query(_) => "query",
        Selection::Thread(_) => "thread",
    }
}

/// Everything one page of ids needs that is cheaper to fetch for the whole
/// page than per message.
///
/// This holds only small per-message facts (flag strings, tag names, summary
/// text) — never a raw blob. That separation is the whole reason it exists:
/// batching the *metadata* is prd.md's "batch-attaches", while batching the
/// bodies would mean 256 messages resident at once and a heap that scales
/// with `PAGE_SIZE * the largest attachment in the mailbox`.
#[derive(Debug, Default)]
struct PageContext {
    flags: BTreeMap<i64, Vec<String>>,
    /// Mailbox id → name. Loaded only for [`Format::Json`], the one format
    /// with a field for it.
    mailboxes: BTreeMap<i64, String>,
    /// Stored AI artifacts, loaded only under [`ExportOptions::with_ai`].
    ai: BTreeMap<i64, json::AiArtifacts>,
}

impl PageContext {
    /// Fetch every batchable fact for one page.
    async fn for_page(
        db: &Database,
        ids: &[i64],
        options: &ExportOptions,
        cancel: &CancellationToken,
    ) -> Result<Self, Error> {
        let json = options.format == Format::Json;
        Ok(Self {
            // The byte formats need flags too — Maildir encodes them in the
            // filename — so this is unconditional.
            flags: select::flags_for(db, ids, cancel).await?,
            mailboxes: if json {
                select::mailbox_names(db, cancel).await?
            } else {
                BTreeMap::new()
            },
            ai: if options.with_ai {
                json::load_artifacts(db, ids, cancel).await?
            } else {
                BTreeMap::new()
            },
        })
    }

    /// Assemble one message, fetching its row (raw blob included) now.
    ///
    /// `None` when the row disappeared between the id scan and this read.
    async fn load(
        &self,
        db: &Database,
        id: i64,
        options: &ExportOptions,
        cancel: &CancellationToken,
    ) -> Result<Option<LoadedMessage>, Error> {
        let Some(message) = select::load_message(db, id, cancel).await? else {
            return Ok(None);
        };
        // Attachment metadata is only ever rendered by the JSON format; the
        // byte formats already carry every part inside `raw`.
        let attachments = if options.format == Format::Json {
            select::attachments_for(db, id, cancel).await?
        } else {
            Vec::new()
        };
        Ok(Some(LoadedMessage {
            mailbox: self.mailboxes.get(&message.mailbox_id).cloned(),
            flags: self.flags.get(&id).cloned().unwrap_or_default(),
            attachments,
            ai: options
                .with_ai
                .then(|| self.ai.get(&id).cloned().unwrap_or_default()),
            message,
        }))
    }
}
