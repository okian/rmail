//! OCR: the second chance for an attachment that legitimately carries no text.
//!
//! [`extract`](crate::attach::extract) already draws the distinction that
//! matters here: [`extract::Status::Empty`](crate::attach::extract::Status)
//! means the format was read successfully and genuinely has no text layer —
//! a scanned page, a photo, a spreadsheet of bare numbers. Only the first two
//! are worth a second look, because the text is *there*, just as pixels
//! instead of characters. This module is that second look: it recognizes text
//! in an image and reports where on the image each run of it was.
//!
//! # Two backends, one interface
//!
//! [`OcrBackend`] is implemented three times: [`VisionBackend`] calls Apple's
//! Vision framework directly and exists only on macOS; [`TesseractBackend`]
//! shells out to the `tesseract` binary and exists everywhere, because it is
//! the only OCR path available at all off macOS and the only one that
//! survives Vision erroring on an input it does not like; [`TestBackend`] is
//! a deterministic double with no dependency on either. All three are equally
//! real implementors of the trait — nothing here branches on "am I in a
//! test," so a production bug in backend selection or fallback is exactly as
//! visible to a test using [`TestBackend`] as it would be with the real
//! thing installed.
//!
//! [`TesseractBackend`] shells out rather than linking a `tesseract-sys`-style
//! crate: the latter needs `libtesseract`/`liblept` present at *build* time,
//! and this machine — like most a user will run rmail on — does not have
//! them. A hard compile-time dependency on a C library nobody has installed
//! would break every build, which is a strange way to implement a feature
//! whose entire premise is "opt-in." A missing `tesseract` binary instead
//! surfaces as [`BackendError::Unavailable`] at call time, exactly like a
//! missing Vision framework does off macOS — both are "not installed," not
//! "broken."
//!
//! # Images decide by content, PDFs decide by what extraction already found
//!
//! An image attachment is recognized by its magic bytes ([`is_image`]),
//! matching `extract::detect`'s reasoning: a sender's declared MIME type is a
//! guess, and `application/octet-stream` is the commonest guess of all. A
//! text-less PDF is not detected independently — it is simply a PDF whose
//! native extraction already reported `Empty`, which is exactly what a scan
//! with no text layer produces.
//!
//! # Only page one of a scanned PDF
//!
//! Rasterizing a PDF page needs a PDF *renderer*, and this crate's only PDF
//! dependency — `pdf-extract` — never produces an image, only text.
//! [`rasterize_pdf_first_page`] gets one by shelling out to `sips`, the
//! image-conversion tool every macOS install ships at `/usr/bin/sips`: it
//! reads a PDF's page geometry and rasterizes page one to PNG at a target
//! resolution, all through a stable, long-shipping system tool rather than a
//! hand-rolled binding to CoreGraphics's C API — which this task judged not
//! worth the risk of a subtly wrong bitmap layout or color space that no test
//! in this environment could actually catch (a wrong stride or byte order
//! does not fail to compile; it fails to OCR, silently, in production).
//!
//! Only the first page is rasterized. A scanned receipt or single-page letter
//! — the overwhelming majority of scanned mail attachments — is fully
//! covered; a multi-page scan gets its first page indexed and nothing past
//! it. Extending this to every page is real, separate scope (more `sips`
//! calls, more `attachment_ocr_regions.page` values, a page budget analogous
//! to `extract::MAX_PDF_PAGES`) and is left for a follow-up rather than
//! bolted on here unverified. Off macOS, or if `sips` is missing, a scanned
//! PDF is simply left exactly where native extraction already put it —
//! `Empty`, not a new kind of failure.
//!
//! # Every backend gets a real chance, and a hang cannot wedge the pool
//!
//! [`run_chain`] does not stop at the first backend that *runs* — it stops at
//! the first one that finds text. A backend that completes but finds nothing
//! is remembered and the next backend in the chain is still tried, so a
//! degraded Vision installation that always comes back empty does not
//! silently shadow a working Tesseract fallback forever (see
//! [`ChainOutcome`]). And because `tesseract`/`sips` are subprocesses that
//! `Command::output()`/`.status()` would wait on indefinitely,
//! [`run_with_deadline`] polls and kills rather than blocking forever — a
//! hung child cannot pin an `OCR_SLOTS` permit past its own deadline the way
//! it would with the standard library's blocking wait alone.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::error::Error;

#[cfg(target_os = "macos")]
mod vision;

mod tesseract;

/// One recognized run of text and where it sits on the image.
#[derive(Debug, Clone, PartialEq)]
pub struct OcrRegion {
    /// The recognized text, generally one line.
    pub text: String,
    /// The backend's confidence for this region, `0.0..=1.0`. `None` when the
    /// backend has nothing to report for an individual region (Tesseract
    /// emits this per word; a region with no words has nothing to average).
    pub confidence: Option<f32>,
    /// Normalized `(x, y, width, height)`, top-left origin, each in
    /// `0.0..=1.0` of the image's own dimensions — independent of whatever
    /// pixel size the image actually was.
    ///
    /// A backend is trusted to compute these but not to *clamp* them —
    /// `attach::apply_ocr` clamps every value into range before it reaches
    /// storage, because `attachment_ocr_regions`' `CHECK` constraints abort
    /// the whole message's persist on a single out-of-range value (verified:
    /// a box a hair past an image edge, which Vision does produce, is enough
    /// to trigger it), and a UI's chosen units should never be able to take
    /// an entire attachment's extraction down with it.
    pub bbox: (f32, f32, f32, f32),
}

/// What one OCR pass over one image produced.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct OcrOutput {
    /// Every region's text, joined in reading order (top to bottom).
    pub text: String,
    /// Every recognized region, in the same order as `text`.
    pub regions: Vec<OcrRegion>,
    /// The mean confidence across `regions`, or `None` when there were none.
    pub confidence: Option<f32>,
}

/// Which engine produced an [`OcrOutput`] — the fact that becomes
/// `attachment_extractions.extractor` and, in coarser form,
/// `attachment_extractions.provenance = 'ocr'`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OcrEngine {
    /// Apple's on-device Vision framework (`VNRecognizeTextRequest`).
    AppleVision,
    /// The `tesseract` binary, invoked as a subprocess.
    Tesseract,
}

impl OcrEngine {
    /// The identity recorded in `attachment_extractions.extractor`.
    ///
    /// Versioned the same way every other extractor identity in this crate
    /// is (see `extract::Format::extractor`): so a later build with a
    /// materially different recognizer can be told apart from this one by
    /// `attach::retryable`'s "extractor this build no longer uses" sweep.
    #[must_use]
    pub fn extractor_id(self) -> &'static str {
        match self {
            Self::AppleVision => "apple-vision/1",
            Self::Tesseract => "tesseract-cli/1",
        }
    }
}

/// Why a backend produced no [`OcrOutput`].
#[derive(Debug, Clone)]
pub enum BackendError {
    /// This backend cannot run in this environment at all: the framework
    /// does not exist on this platform, or the external binary is not
    /// installed. Not a bug — an operator's environment, worth a debug log
    /// and a fallback, not a warning.
    Unavailable(String),
    /// The backend is available and ran, but this particular input beat it
    /// (or it timed out, which this crate treats the same way: a hung
    /// backend is not usably different from a broken one).
    Failed(String),
}

/// A backend able to recognize text in an image's encoded bytes.
///
/// Implemented once per real engine and once as [`TestBackend`] — see the
/// module docs for why all three are first-class rather than the real ones
/// existing and the test double being bolted on.
///
/// `recognize` is synchronous and can be slow (an on-device vision model or a
/// subprocess taking real wall-clock time): every caller in this crate runs
/// it inside [`isolate`], which is the `spawn_blocking` boundary. A
/// `recognize` implementation must never be called directly from async code.
pub trait OcrBackend: Send + Sync {
    /// Which engine this is.
    fn engine(&self) -> OcrEngine;

    /// Recognize text in `image`, whatever raster format it arrived in (PNG,
    /// JPEG, TIFF, ...). `langs` is `index.extract.ocr_langs`.
    ///
    /// # Errors
    ///
    /// [`BackendError::Unavailable`] if this backend cannot run at all right
    /// now; [`BackendError::Failed`] if it ran and this input beat it. Never
    /// for "found no text" — that is a valid, empty [`OcrOutput`].
    fn recognize(&self, image: &[u8], langs: &[String]) -> Result<OcrOutput, BackendError>;
}

/// A factory for a fresh backend chain, so each attempt gets backends with no
/// state left over from the last one.
///
/// A plain `Vec<Box<dyn OcrBackend>>` would work for a single call, but
/// `extract_attachments` may run OCR once per attachment in a message with
/// several of them, and `Box<dyn OcrBackend>` is not `Clone` — there is no
/// value to hand out a second time. A factory sidesteps that by never trying
/// to reuse one: production's factory is "call `default_backends` again,"
/// which is exactly as cheap as building the `Vec` once, and a test's factory
/// can hand out a fresh [`TestBackend`] per attachment just as easily as a
/// shared one.
pub(crate) type BackendFactory = std::sync::Arc<dyn Fn() -> Vec<Box<dyn OcrBackend>> + Send + Sync>;

/// A deterministic backend for tests.
///
/// Never touches Vision or Tesseract, so the pipeline's behavior — which
/// backend wins, what happens when one is unavailable or fails, what lands in
/// `attachment_extractions` and `attachment_ocr_regions` — can be asserted
/// without either the real framework's entitlements or the real binary
/// installed on the machine running the test. Constructed with the
/// [`OcrEngine`] it should claim to be, so a test can exercise "Vision found
/// this" and "Tesseract found this" without needing two distinct types.
#[derive(Clone)]
pub struct TestBackend {
    engine: OcrEngine,
    result: TestResult,
}

#[derive(Clone)]
enum TestResult {
    Output(OcrOutput),
    Unavailable,
    Failed,
}

impl TestBackend {
    /// A backend that succeeds with a canned `output`.
    #[must_use]
    pub fn ok(engine: OcrEngine, output: OcrOutput) -> Self {
        Self {
            engine,
            result: TestResult::Output(output),
        }
    }

    /// A backend that reports itself unavailable, as a missing Tesseract
    /// binary or an off-macOS `VisionBackend` would.
    #[must_use]
    pub fn unavailable(engine: OcrEngine) -> Self {
        Self {
            engine,
            result: TestResult::Unavailable,
        }
    }

    /// A backend that ran and failed, for exercising the fallback path.
    #[must_use]
    pub fn failing(engine: OcrEngine) -> Self {
        Self {
            engine,
            result: TestResult::Failed,
        }
    }
}

impl OcrBackend for TestBackend {
    fn engine(&self) -> OcrEngine {
        self.engine
    }

    fn recognize(&self, _image: &[u8], _langs: &[String]) -> Result<OcrOutput, BackendError> {
        match &self.result {
            TestResult::Output(output) => Ok(output.clone()),
            TestResult::Unavailable => Err(BackendError::Unavailable("test double".to_owned())),
            TestResult::Failed => Err(BackendError::Failed("test double".to_owned())),
        }
    }
}

/// Apple's Vision framework, macOS only.
///
/// See the `vision` submodule for the FFI itself; this is only the
/// [`OcrBackend`] adapter, kept separate so the module doc above can name the
/// type without pulling every reader through the `objc2` call sequence.
#[cfg(target_os = "macos")]
pub struct VisionBackend;

#[cfg(target_os = "macos")]
impl OcrBackend for VisionBackend {
    fn engine(&self) -> OcrEngine {
        OcrEngine::AppleVision
    }

    fn recognize(&self, image: &[u8], langs: &[String]) -> Result<OcrOutput, BackendError> {
        vision::recognize(image, langs)
    }
}

/// The `tesseract` binary, invoked as a subprocess. Available on every
/// platform in the sense that the code compiles everywhere; whether it can
/// actually run depends on whether the operator installed `tesseract`, which
/// [`OcrBackend::recognize`] discovers at call time.
pub struct TesseractBackend;

impl OcrBackend for TesseractBackend {
    fn engine(&self) -> OcrEngine {
        OcrEngine::Tesseract
    }

    fn recognize(&self, image: &[u8], langs: &[String]) -> Result<OcrOutput, BackendError> {
        tesseract::recognize(image, langs)
    }
}

/// The backend chain a real, non-test caller gets: Vision first where it is
/// compiled in, because it is on-device, ships with the OS and needs no
/// operator setup; Tesseract after it, both as the only option off macOS and
/// as the fallback when Vision itself errors — or comes back empty — on a
/// particular input.
#[cfg(target_os = "macos")]
fn default_backends() -> Vec<Box<dyn OcrBackend>> {
    vec![Box::new(VisionBackend), Box::new(TesseractBackend)]
}

#[cfg(not(target_os = "macos"))]
fn default_backends() -> Vec<Box<dyn OcrBackend>> {
    vec![Box::new(TesseractBackend)]
}

/// What trying every backend in a chain, in order, produced.
#[derive(Debug, Clone, PartialEq)]
pub enum ChainOutcome {
    /// A backend produced a result — real text, or a definitive "nothing
    /// here" from a backend that actually ran (preferred over a *different*
    /// backend's [`Failed`](Self::Failed) — a completed "no text" answer is
    /// more informative than an inconclusive error from something else in
    /// the chain).
    Recognized(OcrEngine, OcrOutput),
    /// Every backend in the chain reported itself unavailable: an
    /// environment gap (no Vision, no `tesseract` on `PATH`), not a fault in
    /// this attachment. The caller's native extraction result stands
    /// unchanged — retrying without a config or environment change would
    /// find the same gap.
    Unavailable,
    /// At least one backend ran and errored (or timed out), and none
    /// produced a result at all. Recorded as `Status::Failed` under the
    /// engine that last failed, which is exactly what `attach::retryable`
    /// exists to sweep up once a fixed build ships.
    Failed(OcrEngine, String),
}

/// Run OCR over one image's encoded bytes using the default backend chain.
///
/// # Errors
///
/// Only for task-machinery failure (the blocking pool was shut down, or a
/// backend's `spawn_blocking` task itself failed to run) — never for "found
/// no text" or "every backend was unavailable/failed," both of which are a
/// [`ChainOutcome`], not an `Err`.
pub async fn recognize(bytes: Vec<u8>, langs: Vec<String>) -> Result<ChainOutcome, Error> {
    recognize_with(default_backends(), bytes, langs).await
}

/// [`recognize`] against an explicit backend chain — the seam
/// `attach::extract_attachments_with_ocr` uses to drive a deterministic
/// [`TestBackend`] instead of the real chain.
pub(crate) async fn recognize_with(
    backends: Vec<Box<dyn OcrBackend>>,
    bytes: Vec<u8>,
    langs: Vec<String>,
) -> Result<ChainOutcome, Error> {
    isolate(move || run_chain(&backends, &bytes, &langs)).await
}

/// OCR for a text-less PDF: rasterize page one, then recognize it exactly as
/// if it had arrived as an image attachment. See the module docs for why only
/// page one, and why `sips` rather than a linked PDF renderer.
///
/// [`ChainOutcome::Unavailable`] covers both "not macOS" and "macOS but
/// `sips` is missing or failed to rasterize this particular PDF" — in every
/// case the PDF is left exactly where native extraction already put it.
///
/// # Errors
///
/// Only for task-machinery failure, matching [`recognize`].
pub async fn recognize_pdf_page(bytes: Vec<u8>, langs: Vec<String>) -> Result<ChainOutcome, Error> {
    recognize_pdf_page_with(default_backends(), bytes, langs).await
}

/// [`recognize_pdf_page`] against an explicit backend chain, for tests.
pub(crate) async fn recognize_pdf_page_with(
    backends: Vec<Box<dyn OcrBackend>>,
    bytes: Vec<u8>,
    langs: Vec<String>,
) -> Result<ChainOutcome, Error> {
    isolate(move || {
        let Some(image) = rasterize_pdf_first_page(&bytes) else {
            return ChainOutcome::Unavailable;
        };
        run_chain(&backends, &image, &langs)
    })
    .await
}

/// Try every backend, in order. Stops early only on a backend that finds
/// real text; a backend that completes with nothing is remembered but the
/// chain keeps going, so a degraded backend that always comes back empty
/// cannot permanently shadow a working one behind it. See [`ChainOutcome`]
/// for how the three possible shapes of "nothing usable happened" are told
/// apart.
fn run_chain(backends: &[Box<dyn OcrBackend>], image: &[u8], langs: &[String]) -> ChainOutcome {
    let mut first_empty: Option<(OcrEngine, OcrOutput)> = None;
    let mut last_failure: Option<(OcrEngine, String)> = None;

    for backend in backends {
        match backend.recognize(image, langs) {
            Ok(output) if output.text.trim().is_empty() => {
                tracing::debug!(
                    engine = ?backend.engine(),
                    "ocr backend found no text; trying the next one"
                );
                if first_empty.is_none() {
                    first_empty = Some((backend.engine(), output));
                }
            }
            Ok(output) => return ChainOutcome::Recognized(backend.engine(), output),
            Err(BackendError::Unavailable(reason)) => {
                tracing::debug!(
                    engine = ?backend.engine(),
                    reason,
                    "ocr backend unavailable; trying the next one"
                );
            }
            Err(BackendError::Failed(reason)) => {
                // Still falls through to the next backend — Vision and
                // Tesseract fail on different inputs for different reasons,
                // and a fallback that stops at the first hard error defeats
                // the point of having one.
                tracing::warn!(engine = ?backend.engine(), reason, "ocr backend failed");
                last_failure = Some((backend.engine(), reason));
            }
        }
    }

    if let Some((engine, output)) = first_empty {
        return ChainOutcome::Recognized(engine, output);
    }
    if let Some((engine, reason)) = last_failure {
        return ChainOutcome::Failed(engine, reason);
    }
    ChainOutcome::Unavailable
}

/// Whether these bytes are a raster image format an OCR backend can read,
/// decided by content — matching `extract::detect`'s reasoning that a
/// sender's declared filename or MIME type is a suggestion, not a fact, and
/// `application/octet-stream` is the commonest suggestion of all.
#[must_use]
pub fn is_image(bytes: &[u8]) -> bool {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes.starts_with(&[0xFF, 0xD8, 0xFF])
        || bytes.starts_with(b"GIF87a")
        || bytes.starts_with(b"GIF89a")
        || bytes.starts_with(b"II*\0")
        || bytes.starts_with(b"MM\0*")
    {
        return true;
    }
    // WEBP: a RIFF container naming itself WEBP at byte 8, past the 4-byte
    // little-endian chunk-size field every RIFF file opens with.
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return true;
    }
    // HEIC/HEIF: an ISOBMFF file whose `ftyp` box names a brand Photos and
    // Vision actually produce. Every ISOBMFF file (including plain MP4)
    // shares this box, so the brand is checked rather than just the box name.
    if bytes.len() >= 12
        && &bytes[4..8] == b"ftyp"
        && matches!(
            &bytes[8..12],
            b"heic" | b"heix" | b"heim" | b"heis" | b"mif1" | b"msf1"
        )
    {
        return true;
    }
    // BMP: two magic bytes is a weak signal on its own, so it is paired with
    // the file-size field every BMP header carries at offset 2 actually
    // matching the bytes actually present — cheap, and it turns most
    // accidental "BM..." collisions in arbitrary binary attachments back into
    // a `false` rather than a doomed OCR attempt.
    if bytes.len() >= 6 && bytes.starts_with(b"BM") {
        let claimed = u32::from_le_bytes([bytes[2], bytes[3], bytes[4], bytes[5]]);
        if claimed as usize == bytes.len() {
            return true;
        }
    }
    false
}

/// Smallest attachment `is_image` will still send to OCR.
///
/// `decode_parts` (in `attach::mod`) sees every MIME part `mail_parser`
/// reports as an attachment, which includes inline images — a signature
/// logo, a tracking pixel. Those are legitimately images by content and
/// would otherwise cost a full `spawn_blocking` + Vision/Tesseract pass for
/// zero plausible text. 4 KiB is comfortably below any scanned page (even a
/// small, heavily-compressed JPEG of a receipt) and comfortably above a
/// typical email-signature icon or 1×1 tracking pixel.
pub const MIN_OCR_BYTES: usize = 4 * 1024;

/// How many OCR calls may run at once.
///
/// Smaller than `extract::EXTRACTION_SLOTS`: a Vision or Tesseract call pins
/// a CPU core (and, for Tesseract, a subprocess) for the whole recognition
/// pass, not a bounded parse. Admitting as many of these as ordinary text
/// extraction would starve the blocking pool that every `Database` read and
/// write also shares — the same failure mode `extract::isolate`'s own limit
/// exists to avoid, at a smaller number because each call here costs more.
static OCR_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

/// How long one OCR attempt — recognition, or rasterization followed by
/// recognition — may run before its result is abandoned.
///
/// Generous relative to `extract::EXTRACT_TIMEOUT`: a full vision pass over a
/// dense page is a heavier operation than parsing a text layer that is
/// already there, and a deadline that fires on ordinary slow-but-working
/// input would turn a real result into a permanently empty one for no reason.
/// Larger than [`SUBPROCESS_DEADLINE`] so a subprocess's own deadline fires
/// first in the common case — this one is the last-resort net for the part
/// of the work (Vision, in particular) that has no kill switch of its own.
const OCR_TIMEOUT: Duration = Duration::from_secs(90);

/// How long a single `tesseract`/`sips` invocation may run before
/// [`run_with_deadline`] kills it.
const SUBPROCESS_DEADLINE: Duration = Duration::from_secs(60);

/// Run `work` where neither its panic nor a runaway runtime reaches the
/// caller — the same guarantee `extract::isolate` makes, reimplemented here
/// rather than shared because the two return different shapes and each is
/// small enough that the duplication costs less than the generalization
/// would.
///
/// # Errors
///
/// Only for task-machinery failure that is neither a panic nor a timeout.
async fn isolate<F>(work: F) -> Result<ChainOutcome, Error>
where
    F: FnOnce() -> ChainOutcome + Send + 'static,
{
    let permit = OCR_SLOTS
        .acquire()
        .await
        .map_err(|_| Error::internal("the OCR pool was shut down".to_owned()))?;
    let task = tokio::task::spawn_blocking(move || {
        let result = work();
        // Released here, not when the caller stops waiting — an abandoned
        // task still occupies a blocking thread until it actually returns.
        // `run_with_deadline` is what keeps "until it actually returns" from
        // being unbounded for the subprocess-shaped half of this work.
        drop(permit);
        result
    });
    match tokio::time::timeout(OCR_TIMEOUT, task).await {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(join)) if join.is_panic() => {
            tracing::warn!("an ocr backend panicked; treating this attachment as un-ocr'd");
            Ok(ChainOutcome::Unavailable)
        }
        Ok(Err(join)) => Err(Error::internal(format!("ocr task failed: {join}"))),
        Err(_) => {
            // Not `Failed`: there is no single engine to blame by the time
            // this fires (the chain may have been partway through either
            // backend, or rasterizing), and treating an unattributed timeout
            // as an environment gap — rather than manufacturing a retryable
            // failure row for an engine that may not even have been running
            // — is the safer direction. `run_with_deadline`'s own,
            // engine-attributed timeout below is what usually fires first.
            tracing::warn!(
                seconds = OCR_TIMEOUT.as_secs(),
                "ocr exceeded its deadline; abandoning the result"
            );
            Ok(ChainOutcome::Unavailable)
        }
    }
}

/// Target resolution for PDF-page rasterization.
///
/// `sips`'s default PDF import is one pixel per point — 72 DPI, the same
/// units a PDF's `MediaBox` is already in — which is coarse enough that an
/// invoice's fine print rasterizes into exactly the blur an OCR engine
/// guesses at. 200 DPI is the low end of what scanning guidance generally
/// recommends for OCR and keeps a US Letter page under 1700×2200 pixels,
/// comfortably inside what either backend handles in seconds rather than
/// minutes.
const TARGET_DPI: f64 = 200.0;

/// Longest edge, in pixels, a rasterized page may request.
///
/// A PDF's `MediaBox` is whatever its author claimed, and a poster-sized one
/// scaled to 200 DPI would ask `sips` to allocate a bitmap with no relation
/// to what a scanned mail attachment actually looks like. Capped rather than
/// rejected outright — a legitimately large page still gets OCR'd, just not
/// at full target resolution. The cap is applied to *one* scale factor
/// shared by both axes (see `rasterize_pdf_first_page`), not per axis — a
/// squashed, wrong-aspect-ratio page is exactly the input OCR does worst on.
const MAX_RASTER_EDGE: f64 = 4000.0;

/// Rasterize page one of a PDF to PNG bytes via `sips`, macOS's system image
/// tool — see the module docs for why a subprocess call to a long-shipping
/// Apple tool rather than a linked CoreGraphics binding.
///
/// Returns `None` — not an error — for every reason page one could not be
/// rasterized: `sips` missing, the PDF unreadable, or a genuine `sips`
/// failure. Each is logged at the point it is discovered; none of them are a
/// bug in this crate, so none of them propagate as one.
#[cfg(target_os = "macos")]
fn rasterize_pdf_first_page(bytes: &[u8]) -> Option<Vec<u8>> {
    let Some(input) = ScratchFile::write(bytes, "pdf") else {
        tracing::debug!("could not write a scratch file for pdf rasterization");
        return None;
    };
    // `pdf_page_points` already logs why, for every failure mode it has.
    let (points_w, points_h) = pdf_page_points(input.path())?;
    if points_w <= 0.0 || points_h <= 0.0 {
        tracing::debug!(points_w, points_h, "pdf reported a non-positive page size");
        return None;
    }
    // One scale factor for both axes, derived from whichever axis would hit
    // the cap first — see `MAX_RASTER_EDGE`'s doc for why this must not be
    // computed per axis.
    let uncapped_scale = TARGET_DPI / 72.0;
    let longest_edge_at_target = points_w.max(points_h) * uncapped_scale;
    let scale = if longest_edge_at_target > MAX_RASTER_EDGE {
        uncapped_scale * (MAX_RASTER_EDGE / longest_edge_at_target)
    } else {
        uncapped_scale
    };
    let target_w = (points_w * scale).round() as u32;
    let target_h = (points_h * scale).round() as u32;
    if target_w == 0 || target_h == 0 {
        return None;
    }

    let output = input.path().with_extension("png");
    let mut command = Command::new("sips");
    command
        .args(["-s", "format", "png"])
        .args(["-z", &target_h.to_string(), &target_w.to_string()])
        .arg(input.path())
        .arg("--out")
        .arg(&output);
    // Tracked for cleanup from here on regardless of outcome — `sips`, not
    // this process, creates this path, and every branch below that returns
    // must not leave it behind.
    let cleanup = ScratchFile::adopt(output.clone());
    match run_with_deadline(command, SUBPROCESS_DEADLINE) {
        Ok(result) if result.status.success() => {}
        Ok(result) => {
            tracing::debug!(status = %result.status, "sips exited non-zero rasterizing a pdf page");
            return None;
        }
        Err(SubprocessError::NotFound) => {
            tracing::debug!("sips is not on PATH; pdf ocr is unavailable");
            return None;
        }
        Err(SubprocessError::TimedOut) => {
            tracing::warn!(
                seconds = SUBPROCESS_DEADLINE.as_secs(),
                "sips timed out rasterizing a pdf page"
            );
            return None;
        }
        Err(SubprocessError::Other(reason)) => {
            tracing::debug!(reason, "failed to run sips");
            return None;
        }
    }
    let png = match std::fs::read(&output) {
        Ok(png) => png,
        Err(error) => {
            tracing::debug!(%error, "sips reported success but its output could not be read");
            return None;
        }
    };
    drop(cleanup);
    Some(png)
}

/// Rasterize page one of a PDF to PNG bytes via `pdftoppm` (poppler-utils),
/// the off-macOS counterpart to the `sips` path above.
///
/// Without this the scanned-PDF route is macOS-only, which makes the whole
/// feature incoherent everywhere else: Tesseract is documented as "the only
/// option off macOS", it OCRs images perfectly well there, and yet no scanned
/// PDF could ever reach it because nothing turned page one into an image.
///
/// Same discipline as [`tesseract`]: shelled out, never linked. A missing
/// `pdftoppm` is not a build problem and not an error — it is one more reason
/// this returns `None`, which the caller reports as
/// [`ChainOutcome::Unavailable`] and which leaves native extraction's own
/// answer standing.
///
/// `-r` takes the DPI directly, so unlike `sips` there is no page-size query
/// and no scale arithmetic: poppler reads the `MediaBox` itself. The cap is
/// applied by asking for a bounded pixel size instead — `-scale-to` fits the
/// *longest* edge, which is exactly the "one factor for both axes" rule
/// [`MAX_RASTER_EDGE`] documents, enforced by poppler rather than restated.
#[cfg(not(target_os = "macos"))]
fn rasterize_pdf_first_page(bytes: &[u8]) -> Option<Vec<u8>> {
    let Some(input) = ScratchFile::write(bytes, "pdf") else {
        tracing::debug!("could not write a scratch file for pdf rasterization");
        return None;
    };
    // `-singlefile` makes pdftoppm write exactly `<prefix>.png` rather than
    // `<prefix>-1.png`, so the output path is known rather than guessed.
    let prefix = input.path().with_extension("page");
    let output = prefix.with_extension("png");
    let mut command = Command::new("pdftoppm");
    command
        .arg("-png")
        .args(["-f", "1", "-l", "1"])
        .arg("-singlefile")
        .args(["-r", &format!("{}", TARGET_DPI as u32)])
        .args(["-scale-to", &format!("{}", MAX_RASTER_EDGE as u32)])
        .arg(input.path())
        .arg(&prefix);
    // Tracked from here on regardless of outcome: pdftoppm, not this process,
    // creates that path, and every branch below that returns must not leave
    // it behind.
    let cleanup = ScratchFile::adopt(output.clone());
    match run_with_deadline(command, SUBPROCESS_DEADLINE) {
        Ok(result) if result.status.success() => {}
        Ok(result) => {
            tracing::debug!(status = %result.status, "pdftoppm exited non-zero rasterizing a pdf page");
            return None;
        }
        Err(SubprocessError::NotFound) => {
            tracing::debug!("pdftoppm is not on PATH; pdf ocr is unavailable");
            return None;
        }
        Err(SubprocessError::TimedOut) => {
            tracing::warn!(
                seconds = SUBPROCESS_DEADLINE.as_secs(),
                "pdftoppm timed out rasterizing a pdf page"
            );
            return None;
        }
        Err(SubprocessError::Other(reason)) => {
            tracing::debug!(reason, "failed to run pdftoppm");
            return None;
        }
    }
    let png = match std::fs::read(&output) {
        Ok(png) => png,
        Err(error) => {
            tracing::debug!(%error, "pdftoppm reported success but its output could not be read");
            return None;
        }
    };
    drop(cleanup);
    Some(png)
}

/// Whether a PDF page can be turned into an image on this machine at all.
///
/// Exposed for tests: the scanned-PDF route ends in
/// [`ChainOutcome::Unavailable`] on a host with no rasterizer, and a test that
/// asserted OCR ran regardless would be asserting the presence of `sips` or
/// `pdftoppm` rather than anything about this crate. The container the suite
/// runs in has neither.
#[cfg(test)]
pub(crate) fn pdf_rasterizer_available() -> bool {
    let tool = if cfg!(target_os = "macos") {
        "sips"
    } else {
        "pdftoppm"
    };
    std::process::Command::new(tool)
        .arg("--help")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok()
}

/// Ask `sips` for a PDF's own page size, in points — the units its
/// `MediaBox` is already in, and what `sips -g pixelWidth -g pixelHeight`
/// reports for a PDF specifically (its default PDF import is one pixel per
/// point).
#[cfg(target_os = "macos")]
fn pdf_page_points(path: &std::path::Path) -> Option<(f64, f64)> {
    let mut command = Command::new("sips");
    command
        .args(["-g", "pixelWidth", "-g", "pixelHeight"])
        .arg(path);
    let output = match run_with_deadline(command, SUBPROCESS_DEADLINE) {
        Ok(output) if output.status.success() => output,
        Ok(output) => {
            tracing::debug!(status = %output.status, "sips exited non-zero reading a pdf's page size");
            return None;
        }
        Err(SubprocessError::NotFound) => {
            tracing::debug!("sips is not on PATH; pdf ocr is unavailable");
            return None;
        }
        Err(SubprocessError::TimedOut) => {
            tracing::warn!(
                seconds = SUBPROCESS_DEADLINE.as_secs(),
                "sips timed out reading a pdf's page size"
            );
            return None;
        }
        Err(SubprocessError::Other(reason)) => {
            tracing::debug!(reason, "failed to run sips");
            return None;
        }
    };
    let text = String::from_utf8_lossy(&output.stdout);
    let mut width = None;
    let mut height = None;
    for line in text.lines() {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("pixelWidth:") {
            width = value.trim().parse::<f64>().ok();
        } else if let Some(value) = line.strip_prefix("pixelHeight:") {
            height = value.trim().parse::<f64>().ok();
        }
    }
    Some((width?, height?))
}

/// Why [`run_with_deadline`] did not return a completed [`std::process::Output`].
#[derive(Debug)]
enum SubprocessError {
    /// The binary is not on `PATH`.
    NotFound,
    /// It ran past its deadline and was killed.
    TimedOut,
    /// Spawning failed for some other reason, or waiting on it did.
    Other(String),
}

/// Run `command` to completion, killing it if it has not finished within
/// `deadline`.
///
/// `Command::output()`/`.status()` wait forever, which means the deadline
/// callers think they have (`OCR_TIMEOUT`, enforced by `isolate`'s
/// `tokio::time::timeout`) only stops the *async* caller from waiting — the
/// blocking thread underneath, and the child process it spawned, keep
/// running regardless, holding an `OCR_SLOTS` permit for as long as the
/// child lives. A wedged `tesseract` or `sips` would do that forever: with
/// two permits total, two hangs anywhere permanently stalls every
/// subsequent attachment's OCR. This polls `try_wait` against its own
/// deadline instead, so a hang is actually terminated rather than merely
/// stopped-waiting-for.
///
/// stdout/stderr are drained on background threads concurrently with the
/// poll loop — not left for `.output()` to gather after the fact — so a
/// chatty child cannot fill an OS pipe buffer and stall regardless of how
/// promptly this polls.
fn run_with_deadline(
    mut command: Command,
    deadline: Duration,
) -> Result<std::process::Output, SubprocessError> {
    let mut child = match command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(SubprocessError::NotFound)
        }
        Err(error) => return Err(SubprocessError::Other(format!("spawn failed: {error}"))),
    };

    let stdout_pipe = child.stdout.take();
    let stderr_pipe = child.stderr.take();
    let stdout_thread = std::thread::spawn(move || drain(stdout_pipe));
    let stderr_thread = std::thread::spawn(move || drain(stderr_pipe));

    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if start.elapsed() >= deadline {
                    // Best-effort: `kill` can race a child that exits on its
                    // own between the deadline check and here, in which case
                    // it errors harmlessly. `wait` after `kill` reaps the
                    // process so it does not linger as a zombie.
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(SubprocessError::TimedOut);
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(SubprocessError::Other(format!("wait failed: {error}"))),
        }
    };
    // Both threads finish once their pipe's write end closes, which happens
    // when the child exits — already true by this point, so `.join()` here
    // is bounded, not a second unbounded wait.
    let stdout = stdout_thread.join().unwrap_or_default();
    let stderr = stderr_thread.join().unwrap_or_default();
    Ok(std::process::Output {
        status,
        stdout,
        stderr,
    })
}

/// Read one pipe to the end, for [`run_with_deadline`]'s drain threads.
/// `None` (the child had no such pipe, which does not happen given this
/// module always requests one) reads as empty rather than panicking.
fn drain(pipe: Option<impl Read>) -> Vec<u8> {
    let mut buffer = Vec::new();
    if let Some(mut pipe) = pipe {
        let _ = pipe.read_to_end(&mut buffer);
    }
    buffer
}

/// A unique scratch file, removed on drop.
///
/// Both `sips` and `tesseract` are subprocesses that read a real file — there
/// is no way around writing one when the work is "hand bytes to an external
/// tool." This crate has no `tempfile` dependency (see
/// `storage::tests::TempDbPath` for the same hand-rolled approach), so this
/// is the production equivalent, with two properties `TempDbPath` does not
/// need: the name is unpredictable (a process-random suffix, not just a
/// counter), and the file is created with `O_CREAT | O_EXCL` so a symlink an
/// attacker pre-placed at a guessed path is refused rather than followed —
/// this crate briefly writes attachment plaintext to this path on a machine
/// that may have other local users.
struct ScratchFile {
    path: std::path::PathBuf,
}

/// Process-wide counter mixed into [`ScratchFile`]'s names, alongside a
/// per-call random component — two attachments extracted concurrently on the
/// blocking pool must never be handed the same path, and the path must not
/// be guessable ahead of the `open` call that creates it.
static SCRATCH_COUNTER: AtomicU64 = AtomicU64::new(0);

impl ScratchFile {
    /// Write `bytes` to a new scratch file with the given extension (which
    /// `sips`/`tesseract` both use only as a hint — the identity that
    /// actually matters is the magic bytes leptonica and ImageIO sniff from
    /// the content) and return a handle that deletes it on drop.
    fn write(bytes: &[u8], extension: &str) -> Option<Self> {
        use std::fs::OpenOptions;
        use std::io::Write as _;

        let n = SCRATCH_COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let random = random_suffix();
        let path =
            std::env::temp_dir().join(format!("rmail-ocr-{pid}-{n}-{random:016x}.{extension}"));

        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // Owner-only: this is attachment plaintext, briefly, on a
            // machine that may have other local users.
            options.mode(0o600);
        }
        let mut file = options.open(&path).ok()?;
        file.write_all(bytes).ok()?;
        Some(Self { path })
    }

    /// Track a path this struct did not create — an output file a subprocess
    /// wrote — so it is cleaned up the same way regardless of whether the
    /// call that produced it succeeded. The path itself is still derived
    /// from a [`write`](Self::write)d scratch file's own random name, so it
    /// is no more guessable than that one was.
    fn adopt(path: std::path::PathBuf) -> Self {
        Self { path }
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

/// A process-random `u64`, for [`ScratchFile`]'s unpredictable names.
///
/// `RandomState::new()` seeds fresh from the OS's random source on every
/// call (not once per process), which is enough unpredictability for a
/// scratch-file suffix without pulling in a `rand` dependency for it.
fn random_suffix() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish()
}

impl Drop for ScratchFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests;
