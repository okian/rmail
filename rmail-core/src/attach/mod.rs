//! The attachment text pipeline: raw bytes in, searchable text out.
//!
//! # Bytes are not stored twice
//!
//! `messages.raw` already holds the complete RFC822, so an attachment's bytes
//! are re-derived from it when needed rather than kept alongside. A mailbox is
//! mostly attachments by volume; storing them again would roughly double the
//! database to save a parse that takes microseconds.
//!
//! # Text lands where the body does
//!
//! Extracted text goes into `index_content` as `attachment:<part_id>`, next to
//! the subject and the body, so the lexical index, the entity extractor and the
//! chunker all reach it through paths they already have. Nothing downstream
//! needs to know an attachment is different from a body — only that it carries
//! a different field weight.
//!
//! # Failure is recorded, not retried
//!
//! An encrypted PDF, a format nothing here reads, a file past the size limit:
//! each legitimately yields no text. Without a row saying so they are
//! indistinguishable from "not done yet", and the pipeline would re-open the
//! same 40 MB archive on every pass for the life of the mailbox. Only a hard
//! extractor failure is worth another attempt, and then only under a build that
//! might do better.
//!
//! # OCR is a second pass, not a third format
//!
//! [`ocr`] gets a look at exactly what native extraction already gave up on
//! for the right reason: an image, which has no text-showing operators to
//! begin with, or a PDF native extraction read successfully and found
//! genuinely empty. It only runs when `index.extract.ocr` opts in, and what
//! it produces is stored with a [`Provenance`] alongside the same
//! `extractor`/`status` columns everything else uses — a search hit on OCR'd
//! text is real, but it is a guess about pixels rather than a fact read out
//! of a format, and callers that care about the difference can ask.
//!
//! # Finding an attachment, and asking it a question
//!
//! [`search`] ranks *attachments* rather than the messages that carried them,
//! fusing a per-attachment BM25 arm with the chunk-level dense arm and
//! resolving each winner to a page. [`ask`] is retrieval-augmented generation
//! over what [`search`] found, built to the same order of operations
//! [`crate::ai::rag`] documents — policy gate before any text is rendered,
//! every excerpt fenced, every citation looked up rather than believed.

pub mod ask;
pub mod extract;
pub mod ocr;
pub mod search;

use rusqlite::OptionalExtension;
use sha2::{Digest, Sha256};

use crate::config::IndexExtractConfig;
use crate::error::Error;
use crate::index::extract::Part;
use crate::storage::Database;
use extract::{Extracted, Format, Status};
use ocr::{OcrEngine, OcrRegion};

/// Where a part's stored text came from: read straight out of the format, or
/// recognized from pixels.
///
/// A ranker or a UI badge needs this distinction more often than it needs
/// the exact engine identity `extractor` already carries — "is this a guess"
/// is a coarser, more frequently-asked question than "which guesser."
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    /// Read directly from the format (PDF content stream, DOCX XML, ...).
    Native,
    /// Recognized by an OCR backend from a raster image.
    Ocr,
}

impl Provenance {
    /// The stored form. Changing one orphans every row that used it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Ocr => "ocr",
        }
    }

    /// Read a stored value back.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] for a value this build does not know.
    pub fn parse(value: &str) -> Result<Self, Error> {
        match value {
            "native" => Ok(Self::Native),
            "ocr" => Ok(Self::Ocr),
            other => Err(Error::internal(format!("unknown provenance: {other}"))),
        }
    }
}

/// What one pass over a message's attachments did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AttachReport {
    /// The message.
    pub message_id: i64,
    /// Attachments considered.
    pub attachments: usize,
    /// Attachments whose bytes were unchanged, so nothing was re-extracted.
    pub unchanged: usize,
    /// Attachments that produced text this pass.
    pub extracted: usize,
    /// Attachments that produced none, for any reason.
    pub empty: usize,
    /// Attachments recorded as `failed`.
    pub failed: usize,
    /// Rows removed because the attachment they described is gone.
    pub removed: usize,
    /// Of `extracted`, how many got their text from OCR rather than the
    /// format itself — `index.extract.ocr` doing real work, not merely being
    /// on.
    pub ocr: usize,
}

/// One attachment's outcome, as stored.
///
/// Not `Eq`: `confidence` is an OCR engine's own floating-point score, which
/// has no meaningful total order to derive.
#[derive(Debug, Clone, PartialEq)]
pub struct AttachmentText {
    /// The MIME part id.
    pub part_id: String,
    /// What became of it.
    pub status: Status,
    /// Which extractor ran.
    pub extractor: String,
    /// Decoded size.
    pub bytes: i64,
    /// Extracted characters.
    pub chars: i64,
    /// Pages, for formats that have them.
    pub pages: Option<i64>,
    /// Whether the text was read from the format or recognized by OCR.
    pub provenance: Provenance,
    /// The OCR engine's confidence, `0.0..=1.0`. `None` for native text, or
    /// for an OCR pass that found nothing to be confident about.
    pub confidence: Option<f64>,
}

/// Extract text from every attachment of a message.
///
/// Idempotent: an attachment whose bytes hash to what was recorded last time is
/// skipped entirely, which matters because the indexing queue redelivers on
/// lease expiry and re-parsing a PDF is the most expensive no-op available.
///
/// # Errors
///
/// [`Error::FailedPrecondition`] if the message has no stored raw bytes — it
/// was recorded from an IMAP envelope without a body fetch, so there is nothing
/// to extract from. Otherwise a mapped storage error.
pub async fn extract_attachments(
    db: &Database,
    config: &IndexExtractConfig,
    message_id: i64,
) -> Result<AttachReport, Error> {
    extract_attachments_inner(db, config, message_id, None).await
}

/// [`extract_attachments`] against an explicit OCR backend chain.
///
/// The only caller is this crate's own test suite: it drives OCR through a
/// deterministic [`ocr::TestBackend`] rather than the real Vision/Tesseract
/// chain `extract_attachments` uses, so "does an image get OCR'd, and is the
/// result stored with the right provenance" can be asserted without either
/// backend's real dependencies (Vision's macOS entitlements, an installed
/// `tesseract` binary) being present on the machine running the test.
#[cfg(test)]
pub(crate) async fn extract_attachments_with_ocr(
    db: &Database,
    config: &IndexExtractConfig,
    message_id: i64,
    backends: ocr::BackendFactory,
) -> Result<AttachReport, Error> {
    extract_attachments_inner(db, config, message_id, Some(backends)).await
}

#[tracing::instrument(
    name = "extract_attachments",
    skip(db, config, backends),
    fields(attachments, extracted, ocr)
)]
async fn extract_attachments_inner(
    db: &Database,
    config: &IndexExtractConfig,
    message_id: i64,
    backends: Option<ocr::BackendFactory>,
) -> Result<AttachReport, Error> {
    if !config.attachments {
        return Ok(report_for(message_id));
    }

    let raw: Option<Vec<u8>> = db
        .read(move |conn| {
            conn.query_row(
                "SELECT raw FROM messages WHERE id = ?1",
                [message_id],
                |row| row.get(0),
            )
            .optional()
            .map(Option::flatten)
        })
        .await?;
    let Some(raw) = raw else {
        return Err(Error::failed_precondition(format!(
            "message {message_id} has no stored body; fetch it before extracting attachments"
        )));
    };

    // Parsing, hashing and format detection all together on the blocking pool.
    // Each is proportional to the attachment: a 25 MB SHA-256, a 25 MB memcpy,
    // and — for a zip — a full central-directory parse, measured at 148 ms for
    // a 200k-entry archive. On a runtime thread that is 148 ms during which the
    // process serves nothing.
    let limit = u64::from(config.max_attachment_mb).saturating_mul(1024 * 1024);
    let Some(parts) = tokio::task::spawn_blocking(move || decode_parts(&raw, limit))
        .await
        .map_err(|e| Error::internal(format!("attachment decode task failed: {e}")))?
    else {
        // The raw did not parse at all. Distinguished from "parsed, no
        // attachments", because the stale sweep below would otherwise read an
        // empty part list as "every attachment is gone" and delete every row
        // this message has — including minutes of extraction work — on the
        // strength of one unparsable byte.
        tracing::warn!(
            message_id,
            "the stored raw could not be parsed; leaving its rows alone"
        );
        return Ok(report_for(message_id));
    };

    let known = read_known(db, message_id).await?;
    let allowed: std::collections::BTreeSet<&str> = config
        .formats
        .iter()
        .filter(|name| {
            // A name matching no format is a silent no-op that reads as a
            // deliberate exclusion. Saying so once per pass is noisy; saying so
            // never is how `"eml"` sat in the shipped default for a release
            // while `"html"` was missing.
            let known = Format::parse(name).is_some();
            if !known {
                tracing::warn!(
                    format = %name,
                    supported = ?Format::ALL.map(Format::as_str),
                    "index.extract.formats names something that is not a format"
                );
            }
            known
        })
        .map(String::as_str)
        .collect();
    // Folded into the hash below, so a configuration change invalidates the
    // decisions it would have changed. Without it, re-enabling a format or
    // raising the size limit left every previously-declined attachment
    // permanently declined: the bytes had not changed, so nothing was
    // reconsidered, and no repair path existed.
    let decision = decision_hash(config);

    let mut report = AttachReport {
        message_id,
        attachments: parts.len(),
        ..AttachReport::default()
    };
    let mut results: Vec<Outcome> = Vec::new();

    for part in &parts {
        // Over the bytes *and* the decisions that were made about them, so a
        // config change is a change.
        let hash = keyed_hash(&part.hash, &decision);
        if known
            .get(&part.part_id)
            .is_some_and(|previous| previous.hash == hash && !previous.status.is_retryable())
        {
            report.unchanged += 1;
            continue;
        }

        let bytes = part.bytes_len;
        let (status, extractor, text) = if part.oversized {
            // Decided during the decode, before the bytes were even copied: the
            // point of the limit is to not handle the file, and both detection
            // and hashing handle it.
            tracing::debug!(
                part = %part.part_id,
                bytes,
                "attachment is past the size limit; recorded rather than read"
            );
            (Status::TooLarge, "size-limit", Extracted::default())
        } else {
            match part.format {
                Some(format) if allowed.contains(format.as_str()) => {
                    let (status, text) = extract::extract(format, part.bytes.clone()).await?;
                    (status, format.extractor(), text)
                }
                // A recognized format the operator turned off is `unsupported`
                // as far as the index is concerned — it has no text — but the
                // extractor name records which one, so a later pass can see
                // what it would have used.
                Some(format) => (
                    Status::Unsupported,
                    format.extractor(),
                    Extracted::default(),
                ),
                None => (Status::Unsupported, "none", Extracted::default()),
            }
        };

        // OCR is a second pass over what native extraction already gave up
        // on, not a third kind of extractor — it only runs when the operator
        // opted in, and only for the two shapes the PRD names: an image
        // (`format` is `None` because no *text* extractor reads pixels), or a
        // PDF whose content stream had nothing in it. `part.oversized` is
        // excluded on purpose: the size limit means "do not read these
        // bytes," and OCR reads them the same as any other extractor would.
        let (status, extractor, text, provenance, confidence, regions) =
            if config.ocr && !part.oversized {
                match ocr_route(part.format, status, &part.bytes) {
                    Some(route) => {
                        let outcome = run_ocr(
                            route,
                            part.bytes.clone(),
                            config.ocr_langs.clone(),
                            backends.as_ref(),
                        )
                        .await?;
                        apply_ocr(outcome, status, extractor, text)
                    }
                    None => (
                        status,
                        extractor,
                        text,
                        Provenance::Native,
                        None,
                        Vec::new(),
                    ),
                }
            } else {
                (
                    status,
                    extractor,
                    text,
                    Provenance::Native,
                    None,
                    Vec::new(),
                )
            };

        match status {
            Status::Ok => report.extracted += 1,
            Status::Failed => report.failed += 1,
            _ => report.empty += 1,
        }
        if provenance == Provenance::Ocr && status == Status::Ok {
            report.ocr += 1;
        }
        results.push(Outcome {
            part_id: part.part_id.clone(),
            status,
            extractor,
            bytes,
            text,
            hash,
            provenance,
            confidence,
            regions,
        });
    }

    let present: Vec<String> = parts.into_iter().map(|part| part.part_id).collect();
    report.removed = persist(db, message_id, results, present).await?;

    let span = tracing::Span::current();
    span.record("attachments", report.attachments);
    span.record("extracted", report.extracted);
    span.record("ocr", report.ocr);
    tracing::debug!(
        attachments = report.attachments,
        unchanged = report.unchanged,
        extracted = report.extracted,
        empty = report.empty,
        failed = report.failed,
        ocr = report.ocr,
        "attachment text extracted"
    );
    Ok(report)
}

/// Which OCR path, if any, applies to a part native extraction already
/// classified — decided from `format`/`status` (what extraction found) and
/// `bytes` (what the part actually is, for the image case).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OcrRoute {
    /// An unrecognized-by-format part that is, by its own magic bytes, an
    /// image — recognized directly.
    Image,
    /// A PDF whose native extraction found no text layer: rasterize page one,
    /// then recognize that exactly as an image.
    PdfFirstPage,
}

pub(crate) fn ocr_route(format: Option<Format>, status: Status, bytes: &[u8]) -> Option<OcrRoute> {
    match format {
        // Only a part `extract::detect` could not name at all is a
        // candidate — a recognized format the operator turned off in
        // `index.extract.formats` is a deliberate exclusion, not something
        // OCR should override. The size floor keeps a signature logo or a
        // 1x1 tracking pixel — legitimately images by content — from paying
        // for a full Vision/Tesseract pass each; see `ocr::MIN_OCR_BYTES`.
        None if bytes.len() >= ocr::MIN_OCR_BYTES && ocr::is_image(bytes) => Some(OcrRoute::Image),
        Some(Format::Pdf) if status == Status::Empty => Some(OcrRoute::PdfFirstPage),
        _ => None,
    }
}

/// Run OCR for one part, against either the real default backend chain or an
/// injected test one.
async fn run_ocr(
    route: OcrRoute,
    bytes: Vec<u8>,
    langs: Vec<String>,
    backends: Option<&ocr::BackendFactory>,
) -> Result<ocr::ChainOutcome, Error> {
    match (route, backends) {
        (OcrRoute::Image, Some(factory)) => ocr::recognize_with(factory(), bytes, langs).await,
        (OcrRoute::Image, None) => ocr::recognize(bytes, langs).await,
        (OcrRoute::PdfFirstPage, Some(factory)) => {
            ocr::recognize_pdf_page_with(factory(), bytes, langs).await
        }
        (OcrRoute::PdfFirstPage, None) => ocr::recognize_pdf_page(bytes, langs).await,
    }
}

/// Fold an OCR attempt into the `(status, extractor, text, provenance,
/// confidence, regions)` tuple that gets stored.
#[allow(clippy::type_complexity)]
fn apply_ocr(
    outcome: ocr::ChainOutcome,
    native_status: Status,
    native_extractor: &'static str,
    native_text: Extracted,
) -> (
    Status,
    &'static str,
    Extracted,
    Provenance,
    Option<f64>,
    Vec<OcrRegion>,
) {
    match outcome {
        ocr::ChainOutcome::Unavailable => {
            // No backend could run at all: an environment gap (Vision not
            // compiled in and no `tesseract` on `PATH`, or no rasterizer for
            // a PDF), not a fact about this attachment. Native extraction's
            // own answer stands.
            (
                native_status,
                native_extractor,
                native_text,
                Provenance::Native,
                None,
                Vec::new(),
            )
        }
        ocr::ChainOutcome::Failed(engine, _reason) => {
            // A backend ran and errored (or timed out) and none did better —
            // `attach::retryable`'s "extractor this build no longer uses"
            // sweep is exactly what gives this another attempt once a fixed
            // build ships. `_reason` is already in the `tracing::warn!`
            // `run_chain` logged; not duplicated here.
            (
                Status::Failed,
                engine.extractor_id(),
                Extracted::default(),
                Provenance::Ocr,
                None,
                Vec::new(),
            )
        }
        ocr::ChainOutcome::Recognized(engine, output) => {
            // `extractor` is set to the OCR engine even when it found
            // nothing — that is what lets a later pass (and `retryable`)
            // tell "OCR ran and genuinely found nothing" apart from "OCR was
            // never attempted."
            let status = if output.text.trim().is_empty() {
                Status::Empty
            } else {
                Status::Ok
            };
            let text = if status == Status::Ok {
                // Through the same normalize-and-bound path every native
                // extractor's output goes through (`extract::finish`): OCR
                // text is not exempt from NFC normalization (a decomposed
                // vs. composed accent must not desync indexing from
                // querying) or from the bidi/control-character stripping
                // that keeps attacker-influenced content — a sender picks
                // the image — from reaching a search snippet unfiltered, and
                // it is not exempt from `MAX_TEXT_BYTES` either. The page
                // span is computed from the *normalized* text for the same
                // reason `extract::join_pages` does: an offset must describe
                // the text that is actually stored.
                let mut finished = extract::finish(output.text);
                finished.pages = vec![(0, finished.text.len())];
                finished
            } else {
                Extracted::default()
            };
            let confidence = output.confidence.map(|c| f64::from(clamp_unit(c)));
            let regions = output.regions.into_iter().map(clamp_region).collect();
            (
                status,
                engine.extractor_id(),
                text,
                Provenance::Ocr,
                confidence,
                regions,
            )
        }
    }
}

/// Clamp a value that is supposed to already be `0.0..=1.0` into that range.
///
/// A backend is trusted to *compute* a confidence or a bounding-box
/// coordinate but not to guarantee it never drifts a hair outside `[0, 1]` —
/// verified: Vision does produce a box a fraction past an image's edge near
/// its boundary. `attachment_ocr_regions`' `CHECK` constraints enforce the
/// range at the database layer, and a `CHECK` failure aborts the whole
/// `persist` transaction — losing every other attachment's freshly-extracted
/// text in the same message over one out-of-range float is a worse outcome
/// than silently clamping it.
fn clamp_unit(value: f32) -> f32 {
    value.clamp(0.0, 1.0)
}

fn clamp_region(region: OcrRegion) -> OcrRegion {
    OcrRegion {
        confidence: region.confidence.map(clamp_unit),
        bbox: (
            clamp_unit(region.bbox.0),
            clamp_unit(region.bbox.1),
            clamp_unit(region.bbox.2),
            clamp_unit(region.bbox.3),
        ),
        ..region
    }
}

/// What was recorded for each attachment of a message.
///
/// # Errors
///
/// A mapped storage error.
pub async fn stored(db: &Database, message_id: i64) -> Result<Vec<AttachmentText>, Error> {
    Ok(db
        .read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT part_id, status, extractor, bytes, chars, pages, provenance, confidence
                 FROM attachment_extractions WHERE message_id = ?1 ORDER BY part_id",
            )?;
            let rows = stmt
                .query_map([message_id], |row| {
                    let status: String = row.get(1)?;
                    let provenance: String = row.get(6)?;
                    Ok(AttachmentText {
                        part_id: row.get(0)?,
                        // An unknown status means a newer build wrote it.
                        // `Failed` is the conservative reading: it is the only
                        // one that gets another attempt.
                        status: Status::parse(&status).unwrap_or(Status::Failed),
                        extractor: row.get(2)?,
                        bytes: row.get(3)?,
                        chars: row.get(4)?,
                        pages: row.get(5)?,
                        // An unknown value means a newer build wrote it.
                        // Unlike `status`'s fallback to `Failed`, the
                        // conservative reading here is `Ocr`, not `Native`:
                        // reading an unrecognized provenance as `Native`
                        // would tell a caller "trust this text as a fact
                        // read out of the format, no caveat needed," which
                        // *overstates* confidence if the row actually came
                        // from a future OCR path this build does not know
                        // about. Badging it `Ocr` is the direction that does
                        // not accidentally over-promise.
                        provenance: Provenance::parse(&provenance).unwrap_or_else(|error| {
                            tracing::warn!(%error, provenance, "unknown provenance value");
                            Provenance::Ocr
                        }),
                        confidence: row.get(7)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await?)
}

/// Which page of an attachment a byte offset falls on.
///
/// # Errors
///
/// A mapped storage error.
pub async fn page_at(
    db: &Database,
    message_id: i64,
    part_id: &str,
    offset: i64,
) -> Result<Option<i64>, Error> {
    let part_id = part_id.to_owned();
    Ok(db
        .read(move |conn| {
            conn.query_row(
                "SELECT page FROM attachment_pages
                 WHERE message_id = ?1 AND part_id = ?2
                   AND span_start <= ?3 AND span_end > ?3
                 ORDER BY page LIMIT 1",
                rusqlite::params![message_id, part_id, offset],
                |row| row.get(0),
            )
            .optional()
        })
        .await?)
}

/// Messages with attachments a later build should retry.
///
/// Only hard failures, and only where the extractor that failed is not one this
/// build still uses — the same extractor over the same bytes fails the same
/// way, so retrying it is a loop with extra steps.
///
/// # Errors
///
/// A mapped storage error.
pub async fn retryable(db: &Database, limit: i64) -> Result<Vec<i64>, Error> {
    let mut current: Vec<String> = [
        Format::Pdf,
        Format::Docx,
        Format::Xlsx,
        Format::Pptx,
        Format::Html,
        Format::Text,
    ]
    .iter()
    .map(|format| format.extractor().to_owned())
    .collect();
    // OCR engines are extractors too, as far as this sweep is concerned: a
    // Vision or Tesseract call that hard-failed is exactly the "next build
    // may have a fixed extractor" case this function exists for.
    current.push(OcrEngine::AppleVision.extractor_id().to_owned());
    current.push(OcrEngine::Tesseract.extractor_id().to_owned());
    Ok(db
        .read(move |conn| {
            let placeholders = (2..=current.len() + 1)
                .map(|n| format!("?{n}"))
                .collect::<Vec<_>>()
                .join(", ");
            let mut stmt = conn.prepare(&format!(
                "SELECT DISTINCT message_id FROM attachment_extractions
                 WHERE status = 'failed' AND extractor NOT IN ({placeholders})
                 ORDER BY message_id LIMIT ?1"
            ))?;
            let mut params: Vec<&dyn rusqlite::ToSql> = vec![&limit];
            for name in &current {
                params.push(name);
            }
            let rows = stmt
                .query_map(params.as_slice(), |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<i64>>>()?;
            Ok(rows)
        })
        .await?)
}

/// One attachment's result, on its way to storage.
struct Outcome {
    part_id: String,
    status: Status,
    extractor: &'static str,
    bytes: i64,
    text: Extracted,
    hash: Vec<u8>,
    provenance: Provenance,
    confidence: Option<f64>,
    /// Recognized-text bounding boxes, non-empty only for `Provenance::Ocr`.
    regions: Vec<OcrRegion>,
}

/// One attachment, decoded and classified.
struct DecodedPart {
    part_id: String,
    /// Empty for an oversized part: the bytes are deliberately not copied.
    bytes: Vec<u8>,
    /// The size as it arrived, which is what the limit is measured against and
    /// what gets recorded.
    bytes_len: i64,
    /// SHA-256 of the bytes, or of the size alone when the part was declined
    /// unread — an oversized attachment must still be recognizable as the same
    /// one on the next pass.
    hash: Vec<u8>,
    /// Whether it was past `max_attachment_mb`.
    oversized: bool,
    /// The format, decided from the bytes it actually has.
    format: Option<Format>,
}

/// Pull every attachment out of a raw message, hash it and classify it.
///
/// Returns `None` when the raw did not parse at all, which the caller must not
/// confuse with a message that parsed and has no attachments — the second means
/// "delete the rows for attachments that are gone", and the first means nothing
/// of the sort.
///
/// Everything expensive happens here, on a blocking thread: the parse, the
/// per-part copy, the hash, and — for a zip — the central-directory walk that
/// detection performs.
fn decode_parts(raw: &[u8], limit: u64) -> Option<Vec<DecodedPart>> {
    use mail_parser::{MessageParser, MimeHeaders};

    let message = MessageParser::default().parse(raw)?;
    Some(
        message
            .attachments()
            .enumerate()
            .map(|(index, part)| {
                let contents = part.contents();
                let bytes_len = contents.len() as i64;
                let oversized = contents.len() as u64 > limit;
                // The copy is skipped for a part that will not be read. A
                // 137 MB raw of oversized attachments otherwise cost its own
                // size again in copies before the limit was consulted.
                let bytes = if oversized {
                    Vec::new()
                } else {
                    contents.to_vec()
                };
                let hash = if oversized {
                    // The bytes are not read, so the hash cannot be over them.
                    // Size plus position is enough to recognize the same
                    // declined attachment next time.
                    Sha256::digest(format!("oversized:{index}:{bytes_len}").as_bytes()).to_vec()
                } else {
                    Sha256::digest(&bytes).to_vec()
                };
                let filename = part.attachment_name().map(str::to_owned);
                let content_type = part.content_type().map(|ct| {
                    ct.subtype().map_or_else(
                        || ct.ctype().to_owned(),
                        |sub| format!("{}/{}", ct.ctype(), sub),
                    )
                });
                let format = if oversized {
                    None
                } else {
                    extract::detect(&bytes, filename.as_deref(), content_type.as_deref())
                };
                DecodedPart {
                    // Positional rather than derived from a header: a
                    // `Content-ID` is optional and duplicable, and the identity
                    // has to be stable enough that a re-extract finds the same
                    // row.
                    part_id: index.to_string(),
                    bytes,
                    bytes_len,
                    hash,
                    oversized,
                    format,
                }
            })
            .collect(),
    )
}

/// An empty report for a message nothing could be done with.
fn report_for(message_id: i64) -> AttachReport {
    AttachReport {
        message_id,
        ..AttachReport::default()
    }
}

/// A hash of the configuration that decides what happens to an attachment.
///
/// Folded into each part's stored hash, so that turning a format on, or raising
/// the size limit, invalidates exactly the decisions it would have changed —
/// and nothing else.
fn decision_hash(config: &IndexExtractConfig) -> Vec<u8> {
    let mut formats: Vec<&str> = config.formats.iter().map(String::as_str).collect();
    // Sorted, so reordering the list in a config file is not a change.
    formats.sort_unstable();
    let mut hasher = Sha256::new();
    hasher.update(config.max_attachment_mb.to_le_bytes());
    hasher.update([u8::from(config.strip_html)]);
    for format in formats {
        hasher.update((format.len() as u64).to_le_bytes());
        hasher.update(format.as_bytes());
    }
    // `ocr`/`ocr_langs`: turning OCR on, or changing which languages it is
    // told to expect, has to reconsider every part OCR would now apply to —
    // without this, flipping `index.extract.ocr` from false to true left
    // every already-`Empty` scan or image permanently `Empty`, because the
    // bytes had not changed and nothing else had either.
    //
    // `ocr_langs` is *not* sorted before hashing, unlike `formats` above:
    // both backends treat the list as priority-ordered (`tesseract -l
    // eng+fra` tries `eng` first; Vision's `recognitionLanguages` is
    // documented as ranked the same way), so `["eng", "fra"]` and `["fra",
    // "eng"]` are a real behavior change, not a reordering a hash should
    // treat as a no-op.
    hasher.update([u8::from(config.ocr)]);
    for lang in &config.ocr_langs {
        hasher.update((lang.len() as u64).to_le_bytes());
        hasher.update(lang.as_bytes());
    }
    hasher.finalize().to_vec()
}

/// Combine a content hash with the decision hash.
fn keyed_hash(content: &[u8], decision: &[u8]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(content);
    hasher.update(decision);
    hasher.finalize().to_vec()
}

/// A previous extraction, for the skip decision.
struct Previous {
    hash: Vec<u8>,
    status: Status,
}

/// What was recorded last time, keyed by part.
async fn read_known(
    db: &Database,
    message_id: i64,
) -> Result<std::collections::BTreeMap<String, Previous>, Error> {
    Ok(db
        .read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT part_id, content_hash, status FROM attachment_extractions
                 WHERE message_id = ?1",
            )?;
            let rows = stmt.query_map([message_id], |row| {
                let status: String = row.get(2)?;
                Ok((
                    row.get::<_, String>(0)?,
                    Previous {
                        hash: row.get(1)?,
                        // Unknown means a newer build wrote it; treating it as
                        // retryable re-does work, which is the safe direction.
                        status: Status::parse(&status).unwrap_or(Status::Failed),
                    },
                ))
            })?;
            rows.collect::<rusqlite::Result<_>>()
        })
        .await?)
}

/// Write the results, and drop what belongs to attachments that are gone.
async fn persist(
    db: &Database,
    message_id: i64,
    results: Vec<Outcome>,
    present: Vec<String>,
) -> Result<usize, Error> {
    let removed = db
        .write(move |conn| {
            let tx = conn.transaction()?;

            for outcome in &results {
                let key = Part::Attachment(outcome.part_id.clone()).as_key();
                let chars = outcome.text.text.chars().count() as i64;
                if outcome.status == Status::Ok && !outcome.text.text.is_empty() {
                    tx.prepare_cached(
                        "INSERT INTO index_content
                             (message_id, part, text, chars, content_hash, extractor)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                         ON CONFLICT(message_id, part) DO UPDATE SET
                             text = excluded.text,
                             chars = excluded.chars,
                             content_hash = excluded.content_hash,
                             extractor = excluded.extractor",
                    )?
                    .execute(rusqlite::params![
                        message_id,
                        &key,
                        outcome.text.text,
                        chars,
                        // Over the *text*, not over the attachment's bytes.
                        // `index::extract::message_hash` folds every row's
                        // `content_hash` into the message-level re-index gate,
                        // so a build with a better extractor that produces
                        // different text from identical bytes has to move this
                        // — otherwise nothing downstream re-chunks, re-embeds
                        // or re-indexes it, ever.
                        Sha256::digest(outcome.text.text.as_bytes()).to_vec(),
                        outcome.extractor,
                    ])?;
                    // The attachment-granular lexical index (V39), written in
                    // the same transaction as the text it indexes. Anywhere
                    // else and a crash between the two leaves an attachment
                    // whose text exists and is unfindable — which no later
                    // pass repairs, because the skip above sees an unchanged
                    // content hash and does nothing.
                    search::index_part(&tx, message_id, &outcome.part_id, &outcome.text.text)?;
                } else {
                    // No text is not the same as stale text. An attachment that
                    // used to extract and now does not — replaced by an
                    // encrypted version, say — must stop being searchable by
                    // what it used to say.
                    tx.execute(
                        "DELETE FROM index_content WHERE message_id = ?1 AND part = ?2",
                        rusqlite::params![message_id, &key],
                    )?;
                    search::forget_part(&tx, message_id, &outcome.part_id)?;
                }

                tx.execute(
                    "DELETE FROM attachment_pages WHERE message_id = ?1 AND part_id = ?2",
                    rusqlite::params![message_id, outcome.part_id],
                )?;
                for (page, (start, end)) in outcome.text.pages.iter().enumerate() {
                    tx.prepare_cached(
                        "INSERT INTO attachment_pages
                             (message_id, part_id, page, span_start, span_end)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                    )?
                    .execute(rusqlite::params![
                        message_id,
                        outcome.part_id,
                        page as i64 + 1,
                        *start as i64,
                        *end as i64,
                    ])?;
                }

                tx.prepare_cached(
                    "INSERT INTO attachment_extractions
                         (message_id, part_id, status, extractor, content_hash, bytes,
                          chars, pages, provenance, confidence)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
                     ON CONFLICT(message_id, part_id) DO UPDATE SET
                         status = excluded.status,
                         extractor = excluded.extractor,
                         content_hash = excluded.content_hash,
                         bytes = excluded.bytes,
                         chars = excluded.chars,
                         pages = excluded.pages,
                         provenance = excluded.provenance,
                         confidence = excluded.confidence,
                         extracted_at = unixepoch()",
                )?
                .execute(rusqlite::params![
                    message_id,
                    outcome.part_id,
                    outcome.status.as_str(),
                    outcome.extractor,
                    outcome.hash,
                    outcome.bytes,
                    chars,
                    if outcome.text.pages.is_empty() {
                        None
                    } else {
                        Some(outcome.text.pages.len() as i64)
                    },
                    outcome.provenance.as_str(),
                    outcome.confidence,
                ])?;

                // Unconditional delete-then-insert, same reasoning as
                // `attachment_pages` just above: a part that used to OCR and
                // now reads natively (OCR turned off, or a format newly
                // recognized) must stop carrying boxes for text that is no
                // longer how its text was produced. An `ON DELETE CASCADE`
                // from `attachment_extractions` only fires on a row
                // *deletion*, and this is an upsert.
                tx.execute(
                    "DELETE FROM attachment_ocr_regions WHERE message_id = ?1 AND part_id = ?2",
                    rusqlite::params![message_id, outcome.part_id],
                )?;
                for (seq, region) in outcome.regions.iter().enumerate() {
                    tx.prepare_cached(
                        "INSERT INTO attachment_ocr_regions
                             (message_id, part_id, page, seq, text, confidence, x, y, w, h)
                         VALUES (?1, ?2, 1, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    )?
                    .execute(rusqlite::params![
                        message_id,
                        outcome.part_id,
                        seq as i64,
                        region.text,
                        region.confidence.map(f64::from),
                        f64::from(region.bbox.0),
                        f64::from(region.bbox.1),
                        f64::from(region.bbox.2),
                        f64::from(region.bbox.3),
                    ])?;
                }
            }

            // Attachments the message no longer has. A message is immutable in
            // IMAP, but a re-fetch after a `UIDVALIDITY` rebuild replaces its
            // raw, and an `attachment:` row for a part that is gone would stay
            // searchable for ever.
            // Compared in memory rather than through a `NOT IN` list. SQLite
            // caps bound parameters at 32766, and a message with forty
            // thousand parts — 5.6 MB of raw — made the whole extraction fail
            // with a parameter-count error on every retry, persisting nothing.
            let present: std::collections::BTreeSet<&str> =
                present.iter().map(String::as_str).collect();
            let stale: Vec<String> = {
                let mut stmt =
                    tx.prepare("SELECT part_id FROM attachment_extractions WHERE message_id = ?1")?;
                let rows = stmt
                    .query_map([message_id], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<String>>>()?;
                rows.into_iter()
                    .filter(|part_id| !present.contains(part_id.as_str()))
                    .collect()
            };
            for part_id in &stale {
                let key = Part::Attachment(part_id.clone()).as_key();
                tx.execute(
                    "DELETE FROM index_content WHERE message_id = ?1 AND part = ?2",
                    rusqlite::params![message_id, &key],
                )?;
                search::forget_part(&tx, message_id, part_id)?;
                tx.execute(
                    "DELETE FROM attachment_pages WHERE message_id = ?1 AND part_id = ?2",
                    rusqlite::params![message_id, part_id],
                )?;
                tx.execute(
                    "DELETE FROM attachment_ocr_regions WHERE message_id = ?1 AND part_id = ?2",
                    rusqlite::params![message_id, part_id],
                )?;
                tx.execute(
                    "DELETE FROM attachment_extractions WHERE message_id = ?1 AND part_id = ?2",
                    rusqlite::params![message_id, part_id],
                )?;
            }

            tx.commit()?;
            Ok(stale.len())
        })
        .await?;
    Ok(removed)
}

#[cfg(test)]
mod tests;
