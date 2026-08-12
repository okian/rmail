//! Backend selection and fallback in isolation, then the whole pipeline
//! driven end to end through a deterministic [`TestBackend`] — never the real
//! Vision or Tesseract, so nothing here depends on either being installed or
//! entitled to run on the machine executing the suite. The one exception is
//! [`sips_rasterizes_and_a_test_backend_recognizes_a_scanned_pdf`], which
//! calls the real, always-present `sips` system tool to rasterize but still
//! recognizes through [`TestBackend`] — and
//! [`the_real_backend_chain_actually_recognizes_a_rendered_page`], `#[ignore]`d
//! because it is the one test in this module that calls the real Vision
//! framework.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use rusqlite::OptionalExtension;

use super::*;
use crate::attach::extract::tests::pdf_bytes;
use crate::attach::{extract_attachments_with_ocr, stored, Provenance, Status};
use crate::config::IndexExtractConfig;
use crate::repo;
use crate::storage::Database;

static COUNTER: AtomicU32 = AtomicU32::new(0);

// ---------------------------------------------------------------------------
// Backend chain: `recognize_with` / `run_chain`, no DB, no files.
// ---------------------------------------------------------------------------

fn output(text: &str) -> OcrOutput {
    OcrOutput {
        text: text.to_owned(),
        regions: vec![OcrRegion {
            text: text.to_owned(),
            confidence: Some(0.91),
            bbox: (0.1, 0.2, 0.5, 0.1),
        }],
        confidence: Some(0.91),
    }
}

/// Unwrap a [`ChainOutcome::Recognized`], failing the test with the actual
/// variant otherwise — every backend-chain test below expects this shape
/// except the ones specifically asserting `Unavailable`/`Failed`.
fn assert_recognized(outcome: ChainOutcome) -> (OcrEngine, OcrOutput) {
    match outcome {
        ChainOutcome::Recognized(engine, output) => (engine, output),
        other => unreachable!("expected ChainOutcome::Recognized, got {other:?}"),
    }
}

#[tokio::test]
async fn the_first_backend_that_produces_a_result_wins() {
    let backends: Vec<Box<dyn OcrBackend>> = vec![Box::new(TestBackend::ok(
        OcrEngine::AppleVision,
        output("hello"),
    ))];
    let outcome = recognize_with(backends, b"irrelevant".to_vec(), vec!["eng".to_owned()])
        .await
        .unwrap();
    let (engine, produced) = assert_recognized(outcome);
    assert_eq!(engine, OcrEngine::AppleVision);
    assert_eq!(produced.text, "hello");
}

#[tokio::test]
async fn an_unavailable_backend_falls_through_to_the_next() {
    let backends: Vec<Box<dyn OcrBackend>> = vec![
        Box::new(TestBackend::unavailable(OcrEngine::AppleVision)),
        Box::new(TestBackend::ok(OcrEngine::Tesseract, output("fallback"))),
    ];
    let outcome = recognize_with(backends, Vec::new(), Vec::new())
        .await
        .unwrap();
    let (engine, produced) = assert_recognized(outcome);
    assert_eq!(engine, OcrEngine::Tesseract);
    assert_eq!(produced.text, "fallback");
}

#[tokio::test]
async fn a_failed_backend_still_falls_through_to_the_next() {
    // Unlike `Unavailable`, `Failed` means the backend genuinely ran and this
    // input beat it — still worth trying the next backend rather than giving
    // up, because Vision and Tesseract fail on different inputs.
    let backends: Vec<Box<dyn OcrBackend>> = vec![
        Box::new(TestBackend::failing(OcrEngine::AppleVision)),
        Box::new(TestBackend::ok(OcrEngine::Tesseract, output("recovered"))),
    ];
    let outcome = recognize_with(backends, Vec::new(), Vec::new())
        .await
        .unwrap();
    let (engine, produced) = assert_recognized(outcome);
    assert_eq!(engine, OcrEngine::Tesseract);
    assert_eq!(produced.text, "recovered");
}

#[tokio::test]
async fn every_backend_unavailable_is_unavailable_not_an_error() {
    let backends: Vec<Box<dyn OcrBackend>> = vec![
        Box::new(TestBackend::unavailable(OcrEngine::AppleVision)),
        Box::new(TestBackend::unavailable(OcrEngine::Tesseract)),
    ];
    let outcome = recognize_with(backends, Vec::new(), Vec::new())
        .await
        .unwrap();
    assert_eq!(outcome, ChainOutcome::Unavailable);
}

#[tokio::test]
async fn every_backend_failing_is_recorded_as_failed_not_unavailable() {
    // Distinct from every-`Unavailable`: at least one backend actually ran
    // and broke, which is worth `attach::retryable` sweeping up once a fixed
    // build ships — an environment gap is not.
    let backends: Vec<Box<dyn OcrBackend>> = vec![
        Box::new(TestBackend::failing(OcrEngine::AppleVision)),
        Box::new(TestBackend::failing(OcrEngine::Tesseract)),
    ];
    let outcome = recognize_with(backends, Vec::new(), Vec::new())
        .await
        .unwrap();
    match outcome {
        ChainOutcome::Failed(engine, _reason) => assert_eq!(engine, OcrEngine::Tesseract),
        other => unreachable!("expected Failed, got {other:?}"),
    }
}

#[tokio::test]
async fn a_backend_finding_no_text_is_a_result_not_an_absence() {
    // The distinction the whole pipeline leans on: "OCR ran and found
    // nothing" (`Recognized` with empty text) is not the same fact as "OCR
    // could not run at all" (`Unavailable`) — see `attach::apply_ocr`.
    let backends: Vec<Box<dyn OcrBackend>> = vec![Box::new(TestBackend::ok(
        OcrEngine::AppleVision,
        OcrOutput::default(),
    ))];
    let outcome = recognize_with(backends, Vec::new(), Vec::new())
        .await
        .unwrap();
    let (engine, produced) = assert_recognized(outcome);
    assert_eq!(engine, OcrEngine::AppleVision);
    assert!(produced.text.is_empty());
    assert!(produced.regions.is_empty());
}

#[tokio::test]
async fn an_empty_result_still_lets_a_later_backend_try() {
    // The core fix this task's review caught: a backend that *completes* but
    // finds nothing must not shadow a working backend behind it in the
    // chain — otherwise a degraded Vision installation that always comes
    // back empty permanently hides a perfectly good Tesseract fallback.
    let backends: Vec<Box<dyn OcrBackend>> = vec![
        Box::new(TestBackend::ok(
            OcrEngine::AppleVision,
            OcrOutput::default(),
        )),
        Box::new(TestBackend::ok(
            OcrEngine::Tesseract,
            output("tesseract found it"),
        )),
    ];
    let outcome = recognize_with(backends, Vec::new(), Vec::new())
        .await
        .unwrap();
    let (engine, produced) = assert_recognized(outcome);
    assert_eq!(engine, OcrEngine::Tesseract);
    assert_eq!(produced.text, "tesseract found it");
}

#[tokio::test]
async fn every_backend_empty_falls_back_to_the_first_ones_answer() {
    let backends: Vec<Box<dyn OcrBackend>> = vec![
        Box::new(TestBackend::ok(
            OcrEngine::AppleVision,
            OcrOutput::default(),
        )),
        Box::new(TestBackend::ok(OcrEngine::Tesseract, OcrOutput::default())),
    ];
    let outcome = recognize_with(backends, Vec::new(), Vec::new())
        .await
        .unwrap();
    let (engine, produced) = assert_recognized(outcome);
    assert_eq!(engine, OcrEngine::AppleVision);
    assert!(produced.text.is_empty());
}

#[cfg(target_os = "macos")]
#[test]
fn default_backends_try_vision_before_tesseract_on_macos() {
    let backends = default_backends();
    assert_eq!(backends.len(), 2);
    assert_eq!(backends[0].engine(), OcrEngine::AppleVision);
    assert_eq!(backends[1].engine(), OcrEngine::Tesseract);
}

#[cfg(not(target_os = "macos"))]
#[test]
fn default_backends_is_tesseract_only_off_macos() {
    let backends = default_backends();
    assert_eq!(backends.len(), 1);
    assert_eq!(backends[0].engine(), OcrEngine::Tesseract);
}

// ---------------------------------------------------------------------------
// `is_image`: content-based detection.
// ---------------------------------------------------------------------------

#[test]
fn common_image_formats_are_detected_by_magic_bytes() {
    assert!(is_image(b"\x89PNG\r\n\x1a\nrest-of-file"));
    assert!(is_image(&[0xFF, 0xD8, 0xFF, 0xE0, b'r', b'e', b's', b't']));
    assert!(is_image(b"GIF89arest"));
    assert!(is_image(b"II*\0rest-of-a-little-endian-tiff"));
    assert!(is_image(b"MM\0*rest-of-a-big-endian-tiff"));
    let mut webp = b"RIFF".to_vec();
    webp.extend_from_slice(&[0, 0, 0, 0]);
    webp.extend_from_slice(b"WEBPrest");
    assert!(is_image(&webp));
    let mut heic = vec![0, 0, 0, 24];
    heic.extend_from_slice(b"ftypheic");
    heic.extend_from_slice(b"rest");
    assert!(is_image(&heic));
}

#[test]
fn a_bmp_is_only_matched_when_its_own_size_field_agrees() {
    let mut bmp = b"BM".to_vec();
    bmp.extend_from_slice(&[0, 0, 0, 0]); // size field, patched below
    bmp.extend_from_slice(b"rest-of-a-bitmap-header-and-pixels");
    let total = bmp.len() as u32;
    bmp[2..6].copy_from_slice(&total.to_le_bytes());
    assert!(is_image(&bmp));

    // Two bytes of "BM" inside an arbitrary binary attachment must not read
    // as a bitmap — the size-field check is exactly what tells them apart.
    let not_a_bmp = b"BMprobablynotabitmapatallactually".to_vec();
    assert!(!is_image(&not_a_bmp));
}

#[test]
fn a_pdf_and_arbitrary_bytes_are_not_images() {
    assert!(!is_image(b"%PDF-1.4\nnot an image"));
    assert!(!is_image(b"just some ordinary bytes"));
    assert!(!is_image(b""));
}

// ---------------------------------------------------------------------------
// The whole pipeline: `extract_attachments_with_ocr`, a real (temp-file)
// SQLite database, a deterministic backend.
// ---------------------------------------------------------------------------

struct Fixture {
    db: Database,
    account_id: i64,
    mailbox_id: i64,
    path: PathBuf,
}

impl Fixture {
    async fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-ocr-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", path.display())));
        }
        let db = Database::open(&path).unwrap();
        let (account_id, mailbox_id) = db
            .write(|c| {
                let account_id = repo::insert_account(
                    c,
                    &repo::NewAccount {
                        name: "Personal".to_owned(),
                        ..Default::default()
                    },
                )?;
                let mailbox_id = repo::insert_mailbox(
                    c,
                    &repo::NewMailbox {
                        account_id,
                        name: "INBOX".to_owned(),
                        ..Default::default()
                    },
                )?;
                Ok((account_id, mailbox_id))
            })
            .await
            .unwrap();
        Self {
            db,
            account_id,
            mailbox_id,
            path,
        }
    }

    async fn insert(&self, raw: Vec<u8>) -> i64 {
        let (account_id, mailbox_id) = (self.account_id, self.mailbox_id);
        self.db
            .write(move |c| {
                repo::insert_message(
                    c,
                    &repo::NewMessage {
                        account_id,
                        mailbox_id,
                        uid: 1,
                        uidvalidity: 1,
                        raw: Some(raw),
                        ..Default::default()
                    },
                )
            })
            .await
            .unwrap()
    }

    fn indexed_text(&self, message_id: i64, part: &str) -> Option<String> {
        let key = format!("attachment:{part}");
        self.db
            .with_read(move |c| {
                c.query_row(
                    "SELECT text FROM index_content WHERE message_id = ?1 AND part = ?2",
                    rusqlite::params![message_id, key],
                    |r| r.get(0),
                )
                .optional()
            })
            .unwrap()
    }

    fn regions(&self, message_id: i64, part_id: &str) -> Vec<(String, f64, f64, f64, f64)> {
        self.db
            .with_read(move |c| {
                let mut stmt = c.prepare(
                    "SELECT text, x, y, w, h FROM attachment_ocr_regions
                     WHERE message_id = ?1 AND part_id = ?2 ORDER BY seq",
                )?;
                let rows = stmt
                    .query_map(rusqlite::params![message_id, part_id], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .unwrap()
    }

    fn region_confidence(&self, message_id: i64, part_id: &str) -> Option<f64> {
        self.db
            .with_read(move |c| {
                c.query_row(
                    "SELECT confidence FROM attachment_ocr_regions
                     WHERE message_id = ?1 AND part_id = ?2 ORDER BY seq LIMIT 1",
                    rusqlite::params![message_id, part_id],
                    |r| r.get::<_, Option<f64>>(0),
                )
            })
            .unwrap()
    }

    fn pages(&self, message_id: i64, part_id: &str) -> i64 {
        self.db
            .with_read(move |c| {
                c.query_row(
                    "SELECT count(*) FROM attachment_pages WHERE message_id = ?1 AND part_id = ?2",
                    rusqlite::params![message_id, part_id],
                    |r| r.get(0),
                )
            })
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.path.display())));
        }
    }
}

/// A multipart RFC822 message carrying one attachment.
fn message_with(filename: &str, content_type: &str, bytes: &[u8]) -> Vec<u8> {
    use std::fmt::Write;
    let mut out = String::from(
        "From: ada@example.com\r\n\
         To: bob@example.com\r\n\
         Subject: With an attachment\r\n\
         MIME-Version: 1.0\r\n\
         Content-Type: multipart/mixed; boundary=\"BOUND\"\r\n\r\n\
         --BOUND\r\n\
         Content-Type: text/plain; charset=utf-8\r\n\r\n\
         Please see the attached.\r\n\
         --BOUND\r\n",
    );
    let _ = write!(
        out,
        "Content-Type: {content_type}\r\n\
         Content-Disposition: attachment; filename=\"{filename}\"\r\n\
         Content-Transfer-Encoding: base64\r\n\r\n\
         {}\r\n",
        base64(bytes)
    );
    out.push_str("--BOUND--\r\n");
    out.into_bytes()
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for (n, block) in bytes.chunks(3).enumerate() {
        if n > 0 && n % 19 == 0 {
            out.push_str("\r\n");
        }
        let b = [
            *block.first().unwrap_or(&0),
            *block.get(1).unwrap_or(&0),
            *block.get(2).unwrap_or(&0),
        ];
        let triple = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for i in 0..4 {
            if i <= block.len() {
                let index = ((triple >> (18 - i * 6)) & 0x3f) as usize;
                out.push(char::from(ALPHABET[index]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Bytes that pass [`is_image`] as a PNG without needing to be a real,
/// decodable one — [`TestBackend`] never looks at the pixels, only this
/// crate's own routing does, and routing only checks the magic bytes.
///
/// Padded past [`MIN_OCR_BYTES`]: that floor exists precisely to skip tiny
/// images (a signature logo, a tracking pixel), and a fixture smaller than it
/// would never reach OCR at all regardless of what a given test is actually
/// trying to exercise. [`tiny_png_like`] is the fixture for tests that
/// deliberately want to be under the floor.
fn png_like(marker: &str) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.extend_from_slice(marker.as_bytes());
    bytes.resize(MIN_OCR_BYTES + 64, 0);
    bytes
}

/// Bytes that pass [`is_image`] but are deliberately smaller than
/// [`MIN_OCR_BYTES`] — the shape of an inline signature icon or a tracking
/// pixel, not a scanned page.
fn tiny_png_like(marker: &str) -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.extend_from_slice(marker.as_bytes());
    bytes
}

fn config_with_ocr(enabled: bool) -> IndexExtractConfig {
    IndexExtractConfig {
        ocr: enabled,
        ..IndexExtractConfig::default()
    }
}

fn factory_for(backend: TestBackend) -> BackendFactory {
    // The factory clones a fresh backend per call, mirroring how
    // `default_backends()` builds a fresh `Vec` on every real attempt.
    std::sync::Arc::new(move || {
        let backend: Box<dyn OcrBackend> = Box::new(backend.clone());
        vec![backend]
    })
}

#[tokio::test]
async fn a_fixture_image_is_ocrd_and_stored_with_ocr_provenance() {
    let fixture = Fixture::open().await;
    let raw = message_with("scan.png", "image/png", &png_like("scan-1"));
    let message_id = fixture.insert(raw).await;

    let report = extract_attachments_with_ocr(
        &fixture.db,
        &config_with_ocr(true),
        message_id,
        factory_for(TestBackend::ok(
            OcrEngine::AppleVision,
            output("Invoice #4471 — Total: $128.00"),
        )),
    )
    .await
    .unwrap();

    assert_eq!(report.attachments, 1);
    assert_eq!(report.extracted, 1);
    assert_eq!(report.ocr, 1, "the extraction counted as OCR's doing");

    let rows = stored(&fixture.db, message_id).await.unwrap();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.status, Status::Ok);
    assert_eq!(row.provenance, Provenance::Ocr, "provenance flag is set");
    assert_eq!(row.extractor, OcrEngine::AppleVision.extractor_id());
    assert!(row.confidence.unwrap() > 0.9);

    assert_eq!(
        fixture.indexed_text(message_id, "0").as_deref(),
        Some("Invoice #4471 — Total: $128.00")
    );
    assert_eq!(fixture.pages(message_id, "0"), 1);

    let regions = fixture.regions(message_id, "0");
    assert_eq!(regions.len(), 1);
    assert_eq!(regions[0].0, "Invoice #4471 — Total: $128.00");
    assert!((regions[0].1 - 0.1).abs() < 1e-6, "x normalized and stored");
}

#[tokio::test]
async fn ocr_text_is_normalized_and_bounded_like_every_other_extractor() {
    let fixture = Fixture::open().await;
    let raw = message_with("scan.png", "image/png", &png_like("normalize"));
    let message_id = fixture.insert(raw).await;

    // A decomposed "é" (e + combining acute accent, U+0065 U+0301) rather
    // than the precomposed U+00E9 — exactly the shape `extract::normalize`
    // exists to fix, here proving OCR's output is not exempt from going
    // through it. Without this, a query typed with the composed form would
    // not match text indexed with the decomposed one.
    let decomposed = "caf\u{0065}\u{0301}";

    extract_attachments_with_ocr(
        &fixture.db,
        &config_with_ocr(true),
        message_id,
        factory_for(TestBackend::ok(OcrEngine::AppleVision, output(decomposed))),
    )
    .await
    .unwrap();

    let stored_text = fixture.indexed_text(message_id, "0").unwrap();
    assert_eq!(
        stored_text, "caf\u{00e9}",
        "ocr text must be nfc-normalized like native text"
    );
}

#[tokio::test]
async fn an_out_of_range_region_is_clamped_rather_than_aborting_the_persist() {
    let fixture = Fixture::open().await;
    let raw = message_with("scan.png", "image/png", &png_like("clamp"));
    let message_id = fixture.insert(raw).await;

    let mut out_of_range = output("Edge Text");
    // A box a hair past the image's edge — exactly what Vision does produce
    // near a boundary — and a confidence over 1.0. `attachment_ocr_regions`'
    // `CHECK` constraints reject any of these individually and would abort
    // the whole message's persist transaction if `attach::apply_ocr` did not
    // clamp first.
    out_of_range.regions[0].bbox = (-0.001, 1.0005, 1.2, -0.3);
    out_of_range.regions[0].confidence = Some(1.5);
    out_of_range.confidence = Some(1.5);

    let report = extract_attachments_with_ocr(
        &fixture.db,
        &config_with_ocr(true),
        message_id,
        factory_for(TestBackend::ok(OcrEngine::AppleVision, out_of_range)),
    )
    .await
    .unwrap();
    assert_eq!(report.ocr, 1, "persist must not have aborted");

    let regions = fixture.regions(message_id, "0");
    assert_eq!(regions.len(), 1);
    let (_, x, y, w, h) = regions[0];
    assert!((0.0..=1.0).contains(&x), "x clamped: {x}");
    assert!((0.0..=1.0).contains(&y), "y clamped: {y}");
    assert!((0.0..=1.0).contains(&w), "w clamped: {w}");
    assert!((0.0..=1.0).contains(&h), "h clamped: {h}");
    let confidence = fixture.region_confidence(message_id, "0").unwrap();
    assert!(
        (0.0..=1.0).contains(&confidence),
        "confidence clamped: {confidence}"
    );

    let rows = stored(&fixture.db, message_id).await.unwrap();
    let overall = rows[0].confidence.unwrap();
    assert!(
        (0.0..=1.0).contains(&overall),
        "overall confidence clamped: {overall}"
    );
}

#[tokio::test]
async fn vision_unavailable_falls_back_to_tesseract_and_that_is_what_is_recorded() {
    let fixture = Fixture::open().await;
    let raw = message_with("scan.png", "image/png", &png_like("scan-2"));
    let message_id = fixture.insert(raw).await;

    let backends: BackendFactory = std::sync::Arc::new(|| {
        vec![
            Box::new(TestBackend::unavailable(OcrEngine::AppleVision)) as Box<dyn OcrBackend>,
            Box::new(TestBackend::ok(
                OcrEngine::Tesseract,
                output("from tesseract"),
            )),
        ]
    });

    extract_attachments_with_ocr(&fixture.db, &config_with_ocr(true), message_id, backends)
        .await
        .unwrap();

    let rows = stored(&fixture.db, message_id).await.unwrap();
    assert_eq!(rows[0].extractor, OcrEngine::Tesseract.extractor_id());
    assert_eq!(rows[0].provenance, Provenance::Ocr);
}

#[tokio::test]
async fn ocr_disabled_leaves_an_image_exactly_as_native_extraction_found_it() {
    let fixture = Fixture::open().await;
    let raw = message_with("scan.png", "image/png", &png_like("scan-3"));
    let message_id = fixture.insert(raw).await;

    // No backend is ever constructed in this test, on purpose: with OCR off,
    // the pipeline must not even look at a backend chain.
    let report = extract_attachments_with_ocr(
        &fixture.db,
        &config_with_ocr(false),
        message_id,
        std::sync::Arc::new(Vec::new),
    )
    .await
    .unwrap();

    assert_eq!(report.ocr, 0);
    let rows = stored(&fixture.db, message_id).await.unwrap();
    assert_eq!(rows[0].status, Status::Unsupported);
    assert_eq!(rows[0].provenance, Provenance::Native);
    assert_eq!(rows[0].extractor, "none");
    assert!(fixture.indexed_text(message_id, "0").is_none());
}

#[tokio::test]
async fn an_image_under_the_size_floor_is_not_sent_to_ocr() {
    let fixture = Fixture::open().await;
    let raw = message_with("pixel.png", "image/png", &tiny_png_like("px"));
    let message_id = fixture.insert(raw).await;

    // No backend is ever constructed: under `MIN_OCR_BYTES`, OCR must not
    // even be attempted — a full Vision/Tesseract pass over a signature icon
    // or tracking pixel would be pure cost for no plausible text.
    let report = extract_attachments_with_ocr(
        &fixture.db,
        &config_with_ocr(true),
        message_id,
        std::sync::Arc::new(Vec::new),
    )
    .await
    .unwrap();

    assert_eq!(report.ocr, 0);
    let rows = stored(&fixture.db, message_id).await.unwrap();
    assert_eq!(rows[0].provenance, Provenance::Native);
}

#[tokio::test]
async fn every_backend_unavailable_leaves_the_native_result_in_place() {
    let fixture = Fixture::open().await;
    let raw = message_with("scan.png", "image/png", &png_like("scan-4"));
    let message_id = fixture.insert(raw).await;

    let backends: BackendFactory = std::sync::Arc::new(|| {
        vec![
            Box::new(TestBackend::unavailable(OcrEngine::AppleVision)) as Box<dyn OcrBackend>,
            Box::new(TestBackend::unavailable(OcrEngine::Tesseract)),
        ]
    });

    let report =
        extract_attachments_with_ocr(&fixture.db, &config_with_ocr(true), message_id, backends)
            .await
            .unwrap();

    assert_eq!(report.ocr, 0);
    let rows = stored(&fixture.db, message_id).await.unwrap();
    assert_eq!(rows[0].status, Status::Unsupported);
    assert_eq!(rows[0].provenance, Provenance::Native);
}

#[tokio::test]
async fn every_backend_failing_is_stored_as_a_retryable_failure() {
    let fixture = Fixture::open().await;
    let raw = message_with("scan.png", "image/png", &png_like("scan-fail"));
    let message_id = fixture.insert(raw).await;

    let backends: BackendFactory = std::sync::Arc::new(|| {
        vec![
            Box::new(TestBackend::failing(OcrEngine::AppleVision)) as Box<dyn OcrBackend>,
            Box::new(TestBackend::failing(OcrEngine::Tesseract)),
        ]
    });

    extract_attachments_with_ocr(&fixture.db, &config_with_ocr(true), message_id, backends)
        .await
        .unwrap();

    let rows = stored(&fixture.db, message_id).await.unwrap();
    assert_eq!(rows[0].status, Status::Failed);
    assert_eq!(rows[0].provenance, Provenance::Ocr);
    assert_eq!(rows[0].extractor, OcrEngine::Tesseract.extractor_id());
}

#[tokio::test]
async fn ocr_finding_nothing_is_recorded_empty_with_ocr_provenance() {
    let fixture = Fixture::open().await;
    let raw = message_with("blank.png", "image/png", &png_like("blank"));
    let message_id = fixture.insert(raw).await;

    extract_attachments_with_ocr(
        &fixture.db,
        &config_with_ocr(true),
        message_id,
        factory_for(TestBackend::ok(
            OcrEngine::AppleVision,
            OcrOutput::default(),
        )),
    )
    .await
    .unwrap();

    let rows = stored(&fixture.db, message_id).await.unwrap();
    // "OCR ran and found nothing" is still distinguishable from "OCR was
    // never attempted": the extractor identity says which one happened.
    assert_eq!(rows[0].status, Status::Empty);
    assert_eq!(rows[0].provenance, Provenance::Ocr);
    assert_eq!(rows[0].extractor, OcrEngine::AppleVision.extractor_id());
    assert!(fixture.indexed_text(message_id, "0").is_none());
}

#[tokio::test]
async fn a_pdf_that_already_has_text_is_never_sent_to_ocr() {
    let fixture = Fixture::open().await;
    let raw = message_with("letter.pdf", "application/pdf", &pdf_bytes(&["Dear Ada,"]));
    let message_id = fixture.insert(raw).await;

    // No backend is ever constructed: a PDF with a real text layer must not
    // even reach the OCR decision.
    extract_attachments_with_ocr(
        &fixture.db,
        &config_with_ocr(true),
        message_id,
        std::sync::Arc::new(Vec::new),
    )
    .await
    .unwrap();

    let rows = stored(&fixture.db, message_id).await.unwrap();
    assert_eq!(rows[0].status, Status::Ok);
    assert_eq!(rows[0].provenance, Provenance::Native);
    assert_eq!(
        rows[0].extractor,
        crate::attach::extract::Format::Pdf.extractor()
    );
    assert_eq!(
        rows[0].confidence, None,
        "native text carries no ocr confidence"
    );
}

#[tokio::test]
async fn a_text_less_pdf_is_routed_to_ocr() {
    let fixture = Fixture::open().await;
    // A PDF whose only page has an empty content stream: `pdf-extract` reads
    // it successfully and finds nothing, exactly what a scanned page with no
    // text layer produces — `extract::Status::Empty`, the routing signal
    // `attach::ocr_route` watches for.
    let raw = message_with("scan.pdf", "application/pdf", &pdf_bytes(&[""]));
    let message_id = fixture.insert(raw).await;

    let report = extract_attachments_with_ocr(
        &fixture.db,
        &config_with_ocr(true),
        message_id,
        factory_for(TestBackend::ok(
            OcrEngine::AppleVision,
            output("Recognized from a rasterized page"),
        )),
    )
    .await
    .unwrap();

    // Turning page one into an image needs `sips` (macOS) or `pdftoppm`
    // (elsewhere), and the container this suite runs in has neither. Asserting
    // OCR ran unconditionally would be asserting the presence of one of those
    // tools, not anything about this crate — so assert the contract that
    // actually holds on each side, both of which are documented behaviour and
    // both of which are worth pinning:
    if crate::attach::ocr::pdf_rasterizer_available() {
        assert_eq!(report.ocr, 1);
        let rows = stored(&fixture.db, message_id).await.unwrap();
        assert_eq!(rows[0].status, Status::Ok);
        assert_eq!(rows[0].provenance, Provenance::Ocr);
    } else {
        // `ChainOutcome::Unavailable`: an environment gap, not a fact about
        // this attachment, so native extraction's own answer stands and the
        // row is *not* marked as having been OCR'd.
        assert_eq!(report.ocr, 0);
        let rows = stored(&fixture.db, message_id).await.unwrap();
        assert_eq!(rows[0].provenance, Provenance::Native);
        assert_eq!(rows[0].confidence, None);
    }
}

/// The routing decision on its own, with no subprocess anywhere near it.
///
/// This is the half of the test above that must hold on every machine: a PDF
/// whose native extraction came back empty is a scanned page, and it is the
/// job of `ocr_route` to say so. Whether the host can then rasterize it is a
/// separate question with a separate answer.
#[test]
fn a_pdf_with_no_text_layer_routes_to_ocr_whatever_the_host_can_rasterize() {
    use crate::attach::extract::Format;

    assert_eq!(
        crate::attach::ocr_route(Some(Format::Pdf), Status::Empty, b"%PDF-1.4"),
        Some(crate::attach::OcrRoute::PdfFirstPage),
        "an empty text layer is the scanned-page signal"
    );
    assert_eq!(
        crate::attach::ocr_route(Some(Format::Pdf), Status::Ok, b"%PDF-1.4"),
        None,
        "a PDF that already yielded text is never re-read by OCR"
    );
}

#[tokio::test]
async fn turning_ocr_on_reconsiders_an_already_empty_image() {
    let fixture = Fixture::open().await;
    let raw = message_with("scan.png", "image/png", &png_like("toggle-on"));
    let message_id = fixture.insert(raw).await;

    // First pass with OCR off: the image is recorded as native `Unsupported`.
    let first = extract_attachments_with_ocr(
        &fixture.db,
        &config_with_ocr(false),
        message_id,
        std::sync::Arc::new(Vec::new),
    )
    .await
    .unwrap();
    assert_eq!(first.unchanged, 0);
    let rows = stored(&fixture.db, message_id).await.unwrap();
    assert_eq!(rows[0].provenance, Provenance::Native);

    // Second pass, identical bytes, OCR now on. Without `decision_hash`
    // folding in `config.ocr`, the stored content hash would still match and
    // this pass would count as `unchanged`, permanently skipping OCR for an
    // image that never itself changed.
    let second = extract_attachments_with_ocr(
        &fixture.db,
        &config_with_ocr(true),
        message_id,
        factory_for(TestBackend::ok(
            OcrEngine::AppleVision,
            output("found on reconsideration"),
        )),
    )
    .await
    .unwrap();
    assert_eq!(
        second.unchanged, 0,
        "the config change must force reconsideration"
    );
    assert_eq!(second.ocr, 1);
    let rows = stored(&fixture.db, message_id).await.unwrap();
    assert_eq!(rows[0].provenance, Provenance::Ocr);
    assert_eq!(
        fixture.indexed_text(message_id, "0").as_deref(),
        Some("found on reconsideration")
    );
}

#[tokio::test]
async fn turning_ocr_off_clears_previously_stored_regions() {
    let fixture = Fixture::open().await;
    let raw = message_with("scan.png", "image/png", &png_like("toggle-off"));
    let message_id = fixture.insert(raw).await;

    extract_attachments_with_ocr(
        &fixture.db,
        &config_with_ocr(true),
        message_id,
        factory_for(TestBackend::ok(
            OcrEngine::AppleVision,
            output("will be cleared"),
        )),
    )
    .await
    .unwrap();
    assert_eq!(fixture.regions(message_id, "0").len(), 1);

    extract_attachments_with_ocr(
        &fixture.db,
        &config_with_ocr(false),
        message_id,
        std::sync::Arc::new(Vec::new),
    )
    .await
    .unwrap();

    assert!(
        fixture.regions(message_id, "0").is_empty(),
        "stale ocr regions must not survive a config flip back to native"
    );
    let rows = stored(&fixture.db, message_id).await.unwrap();
    assert_eq!(rows[0].provenance, Provenance::Native);
}

/// The one test in this module that calls the real, always-present `sips`
/// tool — not Vision or Tesseract, so it stays unconditional rather than
/// `#[ignore]`d, but `#[cfg(target_os = "macos")]` because `sips` (and this
/// crate's PDF rasterization path) only exists there. Recognition itself
/// still goes through [`TestBackend`], so what this actually proves is that
/// `rasterize_pdf_first_page` produces bytes real enough for the pipeline to
/// treat as an image at all.
#[cfg(target_os = "macos")]
#[tokio::test]
async fn sips_rasterizes_and_a_test_backend_recognizes_a_scanned_pdf() {
    let fixture = Fixture::open().await;
    let raw = message_with("scan.pdf", "application/pdf", &pdf_bytes(&[""]));
    let message_id = fixture.insert(raw).await;

    extract_attachments_with_ocr(
        &fixture.db,
        &config_with_ocr(true),
        message_id,
        factory_for(TestBackend::ok(
            OcrEngine::AppleVision,
            output("page one, rasterized"),
        )),
    )
    .await
    .unwrap();

    assert_eq!(
        fixture.indexed_text(message_id, "0").as_deref(),
        Some("page one, rasterized")
    );
}

/// Calls the real Apple Vision framework — not run as part of the gate.
///
/// `pdf_bytes` produces a genuine, if minimal, one-page PDF with real text
/// drawn on it; rasterizing it with `sips` and feeding the result through the
/// real default backend chain is the actual end-to-end path a scanned
/// attachment takes in production. Run manually with
/// `cargo nextest run -p rmail-core attach::ocr -- --ignored` on a macOS
/// machine to confirm the Vision integration itself (as opposed to this
/// crate's plumbing around it, which every other test here already covers).
///
/// On the sandboxed host this task was built on, this test fails: Vision
/// returns zero observations for an unambiguous rendered page, with no
/// `NSError` and no unified-log trace at all — see `vision`'s module docs
/// for what was ruled out (byte-exact `NSData`, the URL-based initializer
/// producing the same outcome) before concluding this is that host's
/// restricted Vision access, not a bug in the FFI sequence below. Confirm on
/// an ordinary Mac before relying on this test as a green signal.
#[cfg(target_os = "macos")]
#[tokio::test]
#[ignore = "calls the real Vision framework; run manually, not part of the gate"]
async fn the_real_backend_chain_actually_recognizes_a_rendered_page() {
    let pdf = pdf_bytes(&["Hello OCR World 12345"]);
    let rasterized = super::rasterize_pdf_first_page(&pdf).expect("sips must be on PATH");
    let outcome = recognize(rasterized, vec!["eng".to_owned()]).await.unwrap();
    let ChainOutcome::Recognized(_engine, produced) = outcome else {
        unreachable!("expected a recognized result, got {outcome:?}");
    };
    assert!(
        produced.text.contains("OCR") || produced.text.contains("Hello"),
        "expected recognizable text, got: {:?}",
        produced.text
    );
    assert!(!produced.regions.is_empty());
}
