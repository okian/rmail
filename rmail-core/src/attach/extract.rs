//! Turning an attachment's bytes into text.
//!
//! # Every extractor here parses hostile input
//!
//! An attachment is a file a stranger sent. PDF alone is a format with decades
//! of accumulated ambiguity and a parser stack that panics on roughly a hundred
//! distinct malformed shapes; a zip is a container that can claim a
//! decompressed size it has no intention of producing. So the rules are the
//! same for all of them: bounded output, bounded work, no panic reaching the
//! caller, and a failure that is recorded rather than retried for ever.
//!
//! # Format is decided by content, then by name
//!
//! The declared MIME type of a mail attachment is a guess made by whatever sent
//! it, and `application/octet-stream` is the most common guess of all. Magic
//! bytes are checked first because they are the only part of this that the
//! sender did not choose freely; the filename extension is the fallback, and
//! the declared type is the last resort.
//!
//! # What "no text" means
//!
//! A scanned PDF, an image, a spreadsheet of numbers: all legitimately extract
//! to nothing. That is [`Status::Empty`], and it is a different fact from
//! [`Status::Failed`] — the first is a candidate for OCR and the second is a
//! bug. Collapsing them would mean either retrying scans for ever or never
//! noticing that an extractor broke.

use std::io::{Cursor, Read};

use crate::error::Error;

/// Longest text one attachment contributes to the index.
///
/// Past this the marginal search value is nil and the cost is not: the text is
/// chunked, embedded, tokenized into FTS and hashed. A 400-page manual is a
/// real thing to receive and a poor thing to embed in its entirety.
pub const MAX_TEXT_BYTES: usize = 2 * 1024 * 1024;

/// Largest decompressed size a container attachment may produce.
///
/// A zip declares its uncompressed size in the header and is under no
/// obligation to be telling the truth. The bound is enforced on the *read*, not
/// on the claim, which is the difference between a limit and a suggestion.
const MAX_UNZIPPED_BYTES: u64 = 64 * 1024 * 1024;

/// Largest number of entries read from a container.
///
/// A zip with a million empty entries costs nothing to build and a great deal
/// to walk.
const MAX_ZIP_ENTRIES: usize = 4096;

/// Most cells read from one spreadsheet.
///
/// Not a text bound — the text bound does not help here, because a workbook can
/// cost gigabytes while producing five bytes. Two hundred thousand cells is far
/// past any spreadsheet somebody expects to be searched by its contents.
const MAX_CELLS: usize = 200_000;

/// Most pages read from one PDF.
///
/// `pdf-extract` materialises a `String` for every page before anything here
/// can bound it: two thousand pages sharing one 200 KB content stream, in a
/// 0.49 MB file, produced 381 MB of text and 478 MB of resident memory. The
/// page count is checked against the file's own structure first, which costs a
/// scan rather than an allocation.
const MAX_PDF_PAGES: usize = 1500;

/// How long one attachment may be worked on.
///
/// A PDF with a pathological content stream can occupy a core indefinitely, and
/// the work happens on a blocking thread that cannot be cancelled. A deadline
/// enforced by abandoning the *result* is not a real deadline — the thread runs
/// on — but it does keep one attachment from stalling the pipeline behind it.
pub const EXTRACT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// What became of one attachment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Text was extracted.
    Ok,
    /// The format was read and holds no text. A scanned PDF, an image, a
    /// spreadsheet of bare numbers. A candidate for OCR, not a fault.
    Empty,
    /// Larger than `max_attachment_mb`.
    TooLarge,
    /// No extractor for this format.
    Unsupported,
    /// Password-protected, so the bytes are unreadable without a secret nobody
    /// has offered.
    Encrypted,
    /// The extractor errored or panicked on it. The one status worth another
    /// attempt, because a later build may have a fixed extractor.
    Failed,
    /// The extractor exceeded its deadline and the result was abandoned.
    ///
    /// Deliberately *not* `Failed`. A file that takes a minute takes a minute
    /// every time, and a retryable timeout is a job that re-arms itself for
    /// ever — measured at sixty seconds of CPU and 478 MB of resident memory
    /// per pass, from a half-megabyte attachment.
    Timeout,
}

impl Status {
    /// The stored form. Changing one orphans every row that used it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Empty => "empty",
            Self::TooLarge => "too_large",
            Self::Unsupported => "unsupported",
            Self::Encrypted => "encrypted",
            Self::Failed => "failed",
            Self::Timeout => "timeout",
        }
    }

    /// Every status, for parsing and for exhaustive tests.
    pub const ALL: [Self; 7] = [
        Self::Ok,
        Self::Empty,
        Self::TooLarge,
        Self::Unsupported,
        Self::Encrypted,
        Self::Failed,
        Self::Timeout,
    ];

    /// Read a stored status back.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] for a value this build does not know — which means a
    /// newer build wrote it, and guessing would be worse than saying so.
    pub fn parse(value: &str) -> Result<Self, Error> {
        Self::ALL
            .into_iter()
            .find(|status| status.as_str() == value)
            .ok_or_else(|| Error::internal(format!("unknown attachment status: {value}")))
    }

    /// Whether a later pass should try again.
    ///
    /// Only a hard failure is worth retrying, and then only because the next
    /// build may have a fixed extractor. Re-running an unsupported format or an
    /// oversized file changes nothing and costs the same every time.
    #[must_use]
    pub fn is_retryable(self) -> bool {
        matches!(self, Self::Failed)
    }
}

/// The formats this build can read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Pdf,
    Docx,
    Xlsx,
    Pptx,
    Html,
    Csv,
    Text,
}

impl Format {
    /// The configuration name, matching `index.extract.formats`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pdf => "pdf",
            Self::Docx => "docx",
            Self::Xlsx => "xlsx",
            Self::Pptx => "pptx",
            Self::Html => "html",
            Self::Csv => "csv",
            Self::Text => "txt",
        }
    }

    /// Every format, so a configured name can be checked against something.
    pub const ALL: [Self; 7] = [
        Self::Pdf,
        Self::Docx,
        Self::Xlsx,
        Self::Pptx,
        Self::Html,
        Self::Csv,
        Self::Text,
    ];

    /// The format a configuration name refers to, if any.
    ///
    /// A name matching nothing is a silent no-op: the shipped default listed
    /// `"eml"`, which is not a format, and omitted `"html"`, which is — so
    /// every HTML attachment was declined by a configuration nobody had read
    /// as declining it.
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|format| format.as_str() == name)
    }

    /// The extractor's identity, recorded with the result so a later build can
    /// re-run what it would actually improve.
    #[must_use]
    pub fn extractor(self) -> &'static str {
        match self {
            Self::Pdf => "pdf-extract/0.12",
            Self::Docx | Self::Pptx => "ooxml/1",
            Self::Xlsx => "calamine/0.36",
            Self::Html => "html2text/0.12",
            Self::Csv | Self::Text => "text/1",
        }
    }
}

/// What one extraction produced.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Extracted {
    /// The text, normalized and bounded.
    pub text: String,
    /// Byte spans of each page within `text`, in order, for formats that have
    /// pages. A citation into a fifty-page contract has to name the page.
    pub pages: Vec<(usize, usize)>,
    /// Whether the text was cut at [`MAX_TEXT_BYTES`].
    pub truncated: bool,
}

/// Decide a format from the bytes, the filename and the declared type, in that
/// order of trust.
///
/// Magic bytes come first because they are the only part a sender did not
/// choose freely. `application/octet-stream` is the commonest declared type for
/// an attachment of any format at all, so trusting the declaration first would
/// route most real attachments to `Unsupported`.
#[must_use]
pub fn detect(bytes: &[u8], filename: Option<&str>, content_type: Option<&str>) -> Option<Format> {
    if bytes.starts_with(b"%PDF-") {
        return Some(Format::Pdf);
    }
    if bytes.starts_with(b"PK\x03\x04") {
        // Every OOXML format is a zip, and which one it is lives inside. The
        // container's own parts decide, not the extension: an xlsx named
        // `.docx` routed to the DOCX extractor produces `Empty`, which is not
        // retryable, so it is silently unsearchable for ever. The filename is
        // only a tie-break for a zip whose parts say nothing — which is exactly
        // where the sender's guess is all there is.
        return ooxml_kind(bytes).or_else(|| match extension(filename).as_deref() {
            Some("docx" | "docm") => Some(Format::Docx),
            Some("xlsx" | "xlsm" | "xlsb") => Some(Format::Xlsx),
            Some("pptx" | "pptm") => Some(Format::Pptx),
            _ => None,
        });
    }

    match extension(filename).as_deref() {
        Some("pdf") => return Some(Format::Pdf),
        Some("html" | "htm" | "xhtml") => return Some(Format::Html),
        Some("csv" | "tsv") => return Some(Format::Csv),
        Some("txt" | "text" | "md" | "log" | "json" | "yaml" | "yml" | "toml" | "ini") => {
            return Some(Format::Text)
        }
        _ => {}
    }

    let declared = content_type.unwrap_or("").to_ascii_lowercase();
    let declared = declared.split(';').next().unwrap_or("").trim();
    match declared {
        "application/pdf" => Some(Format::Pdf),
        "text/html" | "application/xhtml+xml" => Some(Format::Html),
        "text/csv" | "text/tab-separated-values" => Some(Format::Csv),
        _ if declared.starts_with("text/") => Some(Format::Text),
        // Not a guess of last resort: an unrecognized attachment recorded as
        // `Unsupported` is a fact somebody can act on, whereas one run through
        // a text extractor produces a page of mojibake that pollutes the index
        // and matches queries at random.
        _ => None,
    }
}

/// Extract text, or say why not.
///
/// Never panics and never blocks indefinitely: the work runs on a blocking task
/// whose panic is caught, because `pdf-extract` alone has around a hundred
/// panicking call sites and an attachment is a file a stranger sent.
///
/// # Errors
///
/// Only for a task-machinery failure. A malformed or unreadable attachment is a
/// [`Status`], not an error — the pipeline must not stop on one.
pub async fn extract(format: Format, bytes: Vec<u8>) -> Result<(Status, Extracted), Error> {
    isolate(format.as_str(), EXTRACT_TIMEOUT, move || {
        run(format, &bytes)
    })
    .await
}

/// Run `work` where neither its panic nor its runtime can reach the caller.
///
/// Separated from [`extract`] so the isolation itself is testable. The two
/// things it guarantees are precisely the two that cannot be provoked through a
/// real extractor on demand, and an untested guard against a daemon-killing
/// panic is a guess.
///
/// # Errors
///
/// Only for a task-machinery failure that is neither a panic nor a timeout.
/// How many attachments may be under extraction at once.
///
/// The blocking pool is shared with every `Database` read and write in the
/// process, and an abandoned extraction is *never* cancelled — a blocking task
/// cannot be. Without a limit, timed-out extractions accumulate in that pool
/// until nothing can reach SQLite: measured, a scaled-down pool went from a
/// 4 ms read to no database access at all. The permit is held by the spawned
/// task rather than by the caller, so an abandoned thread keeps consuming its
/// slot for as long as it is really running, which is the honest accounting.
static EXTRACTION_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

pub(crate) async fn isolate<F>(
    label: &str,
    deadline: std::time::Duration,
    work: F,
) -> Result<(Status, Extracted), Error>
where
    F: FnOnce() -> (Status, Extracted) + Send + 'static,
{
    // Acquired before the blocking task is spawned, so waiters queue as async
    // tasks rather than as parked threads.
    let permit = EXTRACTION_SLOTS
        .acquire()
        .await
        .map_err(|_| Error::internal("the extraction pool was shut down".to_owned()))?;
    let task = tokio::task::spawn_blocking(move || {
        let result = work();
        // Released here, not when the caller stops waiting: an abandoned task
        // still occupies a blocking thread, and pretending otherwise is how
        // the pool fills up.
        drop(permit);
        result
    });
    match tokio::time::timeout(deadline, task).await {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(join)) if join.is_panic() => {
            // The reason this is a `spawn_blocking` and not an inline call. A
            // panic here is a malformed file, not a broken invariant, and
            // taking the daemon down over one attachment takes mail down with
            // it — for every account, not only the one that received it.
            tracing::warn!(format = label, "an extractor panicked on an attachment");
            Ok((Status::Failed, Extracted::default()))
        }
        Ok(Err(join)) => Err(Error::internal(format!("extraction task failed: {join}"))),
        Err(_) => {
            // The task is still running and cannot be cancelled — a blocking
            // task never can. Abandoning the *result* is not a real deadline,
            // but it is what keeps one pathological attachment from stalling
            // every attachment behind it.
            tracing::warn!(
                format = label,
                millis = deadline.as_millis(),
                "an extractor exceeded its deadline; abandoning the result"
            );
            Ok((Status::Timeout, Extracted::default()))
        }
    }
}

/// The synchronous body, for the blocking task and for tests.
fn run(format: Format, bytes: &[u8]) -> (Status, Extracted) {
    let result = match format {
        Format::Pdf => pdf(bytes),
        Format::Docx => ooxml(bytes, &["word/document.xml"], "w:p"),
        Format::Pptx => ooxml(bytes, &["ppt/slides/slide"], "a:p"),
        Format::Xlsx => xlsx(bytes),
        Format::Html => Ok(html(bytes)),
        Format::Csv | Format::Text => Ok(plain(bytes)),
    };
    match result {
        Ok(extracted) if extracted.text.trim().is_empty() => (Status::Empty, Extracted::default()),
        Ok(extracted) => (Status::Ok, extracted),
        Err(status) => (status, Extracted::default()),
    }
}

/// PDF, page by page.
fn pdf(bytes: &[u8]) -> Result<Extracted, Status> {
    // Counted before the parse, because the parse is what allocates. A PDF is
    // free to point two thousand page objects at one shared content stream, so
    // the *file* stays small while the extracted text does not — 0.49 MB in,
    // 381 MB out, at sixty seconds of CPU. This is a scan of the bytes rather
    // than a structural parse: it over-counts a file that mentions `/Type
    // /Page` in a string, which errs toward refusing, and that is the safe
    // direction.
    let pages = count_page_objects(bytes);
    if pages > MAX_PDF_PAGES {
        tracing::warn!(pages, cap = MAX_PDF_PAGES, "declining an enormous PDF");
        return Err(Status::TooLarge);
    }
    match pdf_extract::extract_text_from_mem_by_pages(bytes) {
        Ok(pages) => Ok(join_pages(pages)),
        Err(error) => {
            // Encryption is a distinct answer: the bytes are fine and the
            // format is supported, there is simply a secret nobody offered.
            // Retrying it is pointless and recording it as a failure would put
            // a bug report where a fact belongs.
            let text = error.to_string().to_ascii_lowercase();
            if text.contains("encrypt") || text.contains("password") {
                return Err(Status::Encrypted);
            }
            tracing::debug!(%error, "pdf extraction failed");
            Err(Status::Failed)
        }
    }
}

/// How many page objects a PDF declares, by scanning for the marker.
///
/// Deliberately not a parse. The point is to decide whether the parse is worth
/// attempting, and a parser that allocates per page cannot answer that question
/// before it has already done the damage.
fn count_page_objects(bytes: &[u8]) -> usize {
    const MARKER: &[u8] = b"/Page";
    let mut count = 0usize;
    let mut at = 0usize;
    while let Some(found) = bytes[at..]
        .windows(MARKER.len())
        .position(|window| window == MARKER)
    {
        at += found + MARKER.len();
        // `/Pages` is the tree node, not a page. Without this, a document with
        // a deep page tree counts its own branches.
        if bytes.get(at) != Some(&b's') {
            count += 1;
        }
        if count > MAX_PDF_PAGES || at >= bytes.len() {
            break;
        }
    }
    count
}

/// DOCX and PPTX: a zip of XML, with the words in text nodes.
///
/// `prefixes` names the parts to read, matched by prefix so PPTX's numbered
/// slides are all found; `paragraph` is the element that separates one run of
/// text from the next, so words from two paragraphs do not run together into a
/// token that appears in neither.
fn ooxml(bytes: &[u8], prefixes: &[&str], paragraph: &str) -> Result<Extracted, Status> {
    use quick_xml::events::Event;

    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|error| {
        tracing::debug!(%error, "not a readable zip");
        Status::Failed
    })?;

    let mut names: Vec<String> = archive
        .file_names()
        .take(MAX_ZIP_ENTRIES)
        .filter(|name| prefixes.iter().any(|prefix| name.starts_with(prefix)))
        .map(str::to_owned)
        .collect();
    // Sorted so slide 10 does not come before slide 2 by accident of zip order —
    // and, more importantly, so the same file always extracts to the same text
    // and the content hash means what it claims.
    names.sort_by(|a, b| natural_order(a, b));

    let mut text = String::new();
    let mut budget = MAX_UNZIPPED_BYTES;
    for name in names {
        if budget == 0 {
            tracing::warn!("container exceeded its decompression budget");
            break;
        }
        let Ok(entry) = archive.by_name(&name) else {
            continue;
        };
        // Bounded on the read, not on the header's claim: a zip is free to
        // declare any uncompressed size it likes. Read as *bytes* and decoded
        // afterwards, because `read_to_string` fails outright when the cut
        // lands mid-character — which discarded an entire 80 MB
        // `word/document.xml` of ordinary multibyte text, permanently, since
        // the resulting `Empty` is not retryable.
        let mut raw = Vec::new();
        if entry.take(budget).read_to_end(&mut raw).is_err() && raw.is_empty() {
            continue;
        }
        // Charged against the budget whether the read succeeded or not. An
        // entry that inflates 64 MB and then errors used to cost nothing,
        // which made the aggregate limit per-entry in practice: 128 such
        // entries inflated 8 GB from an 8 MB attachment.
        budget = budget.saturating_sub(raw.len() as u64);
        let xml = decode(&raw);

        let mut reader = quick_xml::Reader::from_str(&xml);
        let mut buffer = Vec::new();
        loop {
            match reader.read_event_into(&mut buffer) {
                Ok(Event::Text(node)) => {
                    if let Ok(unescaped) = node.decode() {
                        text.push_str(&unescaped);
                    }
                }
                Ok(Event::End(end)) if end.name().as_ref() == paragraph.as_bytes() => {
                    text.push('\n');
                }
                Ok(Event::Eof) => break,
                // A malformed part is skipped, not fatal: a document with one
                // broken slide is still worth the other forty.
                Err(_) => break,
                _ => {}
            }
            buffer.clear();
            if text.len() > MAX_TEXT_BYTES {
                break;
            }
        }
        text.push('\n');
        if text.len() > MAX_TEXT_BYTES || budget == 0 {
            break;
        }
    }
    Ok(finish(text))
}

/// Which OOXML format a zip holds, from its own parts.
fn ooxml_kind(bytes: &[u8]) -> Option<Format> {
    let archive = zip::ZipArchive::new(Cursor::new(bytes)).ok()?;
    let names: Vec<String> = archive
        .file_names()
        .take(MAX_ZIP_ENTRIES)
        .map(str::to_owned)
        .collect();
    let has = |prefix: &str| names.iter().any(|name| name.starts_with(prefix));
    if has("word/") {
        Some(Format::Docx)
    } else if has("xl/") {
        Some(Format::Xlsx)
    } else if has("ppt/") {
        Some(Format::Pptx)
    } else {
        // A plain zip is not a document. Guessing one of the three from an
        // archive with none of their parts would run a document extractor over
        // arbitrary files and fill the index with fragments of whatever was in
        // them.
        None
    }
}

/// XLSX, cell by cell rather than sheet by sheet.
///
/// # Why not `worksheet_range`
///
/// It builds a *dense* `Range` from the sheet's declared corner, which a
/// spreadsheet is free to place anywhere. One cell at `Z4000000` — in a
/// 1,433-byte attachment — allocated 3.3 GB in two seconds. None of the four
/// limits in this module applied: the output text was five bytes, calamine
/// opens the zip itself, and it finished well inside the deadline. Rust aborts
/// on allocation failure rather than panicking, so [`isolate`] could not have
/// caught it either; the daemon simply dies.
///
/// The cell reader is sparse, so cost is proportional to cells that exist.
fn xlsx(bytes: &[u8]) -> Result<Extracted, Status> {
    use calamine::Reader;

    let mut workbook: calamine::Xlsx<_> =
        calamine::Xlsx::new(Cursor::new(bytes)).map_err(|error| {
            let text = error.to_string().to_ascii_lowercase();
            if text.contains("password") || text.contains("encrypt") {
                return Status::Encrypted;
            }
            tracing::debug!(%error, "spreadsheet could not be opened");
            Status::Failed
        })?;

    let mut text = String::new();
    let mut budget = MAX_CELLS;
    for name in workbook.sheet_names() {
        // The sheet name is indexed too: "Q3 Forecast" is often the most
        // searchable thing in a workbook, and it appears in no cell.
        text.push_str(&name);
        text.push('\n');

        let Ok(mut cells) = workbook.worksheet_cells_reader(&name) else {
            continue;
        };
        let mut row = None;
        loop {
            let cell = match cells.next_cell() {
                Ok(Some(cell)) => cell,
                Ok(None) => break,
                // A malformed sheet is skipped, not fatal: a workbook with one
                // broken tab is still worth the other twelve.
                Err(_) => break,
            };
            let Some(budget_left) = budget.checked_sub(1) else {
                tracing::warn!(sheet = %name, "spreadsheet exceeded the cell budget");
                break;
            };
            budget = budget_left;

            let value = match cell.get_value() {
                calamine::DataRef::Empty => continue,
                calamine::DataRef::String(s) => (*s).to_owned(),
                calamine::DataRef::SharedString(s) => (*s).to_owned(),
                calamine::DataRef::Float(f) => f.to_string(),
                calamine::DataRef::Int(i) => i.to_string(),
                calamine::DataRef::Bool(b) => b.to_string(),
                calamine::DataRef::DateTime(d) => d.to_string(),
                calamine::DataRef::DateTimeIso(s) | calamine::DataRef::DurationIso(s) => s.clone(),
                calamine::DataRef::Error(e) => format!("{e:?}"),
            };
            let (r, _) = cell.get_position();
            if row == Some(r) {
                // Tab-separated within a row rather than a line per cell: a row
                // is a record, and splitting it destroys the adjacency that
                // makes "invoice 4471" findable as a phrase.
                text.push('\t');
            } else {
                if row.is_some() {
                    text.push('\n');
                }
                row = Some(r);
            }
            text.push_str(&value);
            if text.len() > MAX_TEXT_BYTES {
                break;
            }
        }
        text.push('\n');
        if text.len() > MAX_TEXT_BYTES || budget == 0 {
            break;
        }
    }
    Ok(finish(text))
}

/// HTML, through the same stripper the message bodies use.
fn html(bytes: &[u8]) -> Extracted {
    let source = decode(bytes);
    finish(crate::index::extract::strip_html(&source))
}

/// Plain text and CSV, decoded from whatever encoding they arrived in.
fn plain(bytes: &[u8]) -> Extracted {
    finish(decode(bytes))
}

/// Decode bytes to a string, guessing the encoding.
///
/// Mail attachments predate the consensus on UTF-8 by decades; a Windows-1252
/// CSV from an accounting package is an ordinary thing to receive. Lossy rather
/// than failing, because a file that is 99% readable is worth indexing.
fn decode(bytes: &[u8]) -> String {
    let (text, _, _) = encoding_rs::UTF_8.decode(bytes);
    if text.contains('\u{fffd}') {
        // Replacement characters mean it was not UTF-8. Windows-1252 maps every
        // byte to something, so it never fails and is the right fallback for
        // the western European mail that produces most of these.
        let (fallback, _, _) = encoding_rs::WINDOWS_1252.decode(bytes);
        return fallback.into_owned();
    }
    text.into_owned()
}

/// Concatenate pages, recording where each one starts and ends.
///
/// # Each page is normalized before it is placed
///
/// The offsets have to describe the text that is *stored*, and normalization
/// collapses whitespace — which is not distributed evenly across pages. A
/// sparsely set title page followed by dense body pages is what most real PDFs
/// look like, and scaling pre-normalization offsets by a global length ratio
/// put five of six page marks on the wrong page in an ordinary six-page
/// document. A citation that confidently names the wrong page is worse than no
/// citation at all, so the offsets are built from the normalized text rather
/// than estimated back onto it.
fn join_pages(pages: Vec<String>) -> Extracted {
    let page_count = pages.len();
    let mut text = String::new();
    let mut spans = Vec::with_capacity(pages.len());
    for page in pages {
        let normalized = crate::index::extract::normalize(&page);
        if text.len().saturating_add(normalized.len()) > MAX_TEXT_BYTES {
            break;
        }
        let start = text.len();
        text.push_str(&normalized);
        spans.push((start, text.len()));
        // A separator, so two pages' words do not run together into a token
        // that appears on neither. Outside the span, because it belongs to
        // neither page.
        text.push('\n');
    }
    let truncated = spans.len() < page_count;
    Extracted {
        text,
        pages: spans,
        truncated,
    }
}

/// Normalize and bound a pageless result.
///
/// Only for the formats that have no pages. Anything with them goes through
/// [`join_pages`], which normalizes page by page so the offsets describe the
/// text that is actually stored — see that function for why estimating them
/// afterwards does not work.
fn finish(text: String) -> Extracted {
    let normalized = crate::index::extract::normalize(&text);
    let truncated = normalized.len() > MAX_TEXT_BYTES;
    let text = if truncated {
        let cut = boundary(&normalized, MAX_TEXT_BYTES);
        normalized.get(..cut).unwrap_or_default().to_owned()
    } else {
        normalized
    };
    Extracted {
        text,
        pages: Vec::new(),
        truncated,
    }
}

/// The character boundary at or before `at`.
fn boundary(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// Order names so `slide2` precedes `slide10`.
///
/// Zip entry order is whatever the writer chose, and lexical order puts slide
/// ten before slide two. Both would make the same file extract to different
/// text on different days, which the content hash would then read as a change.
fn natural_order(a: &str, b: &str) -> std::cmp::Ordering {
    let key = |s: &str| -> (String, u64) {
        let digits: String = s.chars().filter(char::is_ascii_digit).collect();
        let letters: String = s.chars().filter(|c| !c.is_ascii_digit()).collect();
        (letters, digits.parse().unwrap_or(0))
    };
    key(a).cmp(&key(b))
}

/// The extension of a filename, lowercased.
fn extension(filename: Option<&str>) -> Option<String> {
    filename?
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
}

#[cfg(test)]
pub(crate) mod tests;
