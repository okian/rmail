//! Apple's Vision framework, called directly through `objc2`/`objc2-vision`.
//!
//! `VNImageRequestHandler` is initialized from the image's raw file bytes
//! (`initWithData:options:`) rather than from a decoded bitmap: Vision
//! decodes the container itself via ImageIO, so no image-decoding dependency
//! belongs on this side of the FFI boundary, and a HEIC or a TIFF needs no
//! special handling here beyond arriving as bytes at all.
//!
//! Recognition runs synchronously (`performRequests:error:`), not through the
//! completion-handler variant: a completion handler is how Vision fits into
//! an app already running its own event loop, and there is no such loop on
//! the `spawn_blocking` thread this runs on — a plain, blocking call is the
//! correct shape here, not a simplification of a "real" async one.
//!
//! # A known gap in this environment
//!
//! The FFI sequence below was verified twice over — once by construction
//! against `objc2-vision`'s published API, and once empirically: the
//! `#[ignore]`d integration test in the parent module's `tests` submodule
//! confirms `NSData` reaches Vision byte-for-byte (`data.len()` matches the
//! input), `performRequests:error:` returns success with no `NSError`, and
//! swapping to the URL-based initializer produces the identical outcome.
//! What it could not confirm, on the sandboxed build host this task ran on,
//! is a *positive* recognition result — `VNRecognizeTextRequest.results()`
//! came back empty for a large, high-contrast, unambiguous "Hello OCR World"
//! rendered at 200 DPI, at both the `.accurate` and `.fast` recognition
//! levels. `log show` on that host returned no unified-log entries at all
//! (for *any* subsystem, over any window), which is itself the signal: this
//! is a restricted/headless macOS instance where Vision's on-device analysis
//! backend is not fully available to an unsigned CLI process, not a defect
//! reachable through this crate's own code. Re-run
//! `cargo nextest run -p rmail-core attach::ocr -- --ignored` on an ordinary
//! Mac to confirm end to end.

use objc2::rc::{autoreleasepool, Retained};
use objc2::runtime::AnyObject;
use objc2::AnyThread;
use objc2_foundation::{NSArray, NSData, NSDictionary, NSString};
use objc2_vision::{
    VNImageOption, VNImageRequestHandler, VNRecognizeTextRequest, VNRequest,
    VNRequestTextRecognitionLevel,
};

use super::{BackendError, OcrOutput, OcrRegion};

pub(super) fn recognize(image: &[u8], langs: &[String]) -> Result<OcrOutput, BackendError> {
    // `spawn_blocking` threads have no top-level autorelease pool (that is a
    // main-thread/run-loop concept this code has neither of), and Vision
    // creates plenty of autoreleased temporaries internally — ImageIO
    // wrappers, observation arrays, `NSString`s. With no pool in place the
    // ObjC runtime does not clean those up, it leaks them (and logs an
    // "autoreleased with no pool in place" warning per object). For a daemon
    // OCR'ing thousands of attachments over its lifetime that is unbounded
    // growth, not a one-time cost. Every `Retained` created below is dropped
    // (or converted to an owned Rust value) before this closure returns, so
    // nothing from inside the pool escapes it.
    autoreleasepool(|_pool| recognize_in_pool(image, langs))
}

fn recognize_in_pool(image: &[u8], langs: &[String]) -> Result<OcrOutput, BackendError> {
    let data = NSData::with_bytes(image);
    let options = NSDictionary::<VNImageOption, AnyObject>::new();
    let handler = VNImageRequestHandler::initWithData_options(
        VNImageRequestHandler::alloc(),
        &data,
        &options,
    );

    let request = VNRecognizeTextRequest::new();
    request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
    request.setUsesLanguageCorrection(true);
    if let Some(codes) = bcp47_languages(langs) {
        let owned: Vec<Retained<NSString>> =
            codes.iter().map(|code| NSString::from_str(code)).collect();
        let refs: Vec<&NSString> = owned.iter().map(AsRef::as_ref).collect();
        let array = NSArray::from_slice(&refs);
        request.setRecognitionLanguages(&array);
    }

    let request_ref: &VNRequest = &request;
    let requests = NSArray::from_slice(&[request_ref]);
    handler
        .performRequests_error(&requests)
        .map_err(|error| BackendError::Failed(error.localizedDescription().to_string()))?;

    let Some(observations) = request.results() else {
        return Ok(OcrOutput::default());
    };

    let mut lines: Vec<(f32, OcrRegion)> = Vec::new();
    for observation in observations.iter() {
        let candidates = observation.topCandidates(1);
        let Some(top) = candidates.iter().next() else {
            continue;
        };
        let text = top.string().to_string();
        if text.trim().is_empty() {
            continue;
        }
        let confidence = top.confidence();
        // SAFETY: `boundingBox` reads a plain `CGRect` property; the
        // binding marks it unsafe only because it is FFI, not because it has
        // an unencoded precondition.
        let bbox = unsafe { observation.boundingBox() };
        // Vision's origin is the image's *lower*-left corner; every other
        // consumer of a box in this crate — and a reader looking at a page —
        // thinks top-left. Flipped once, here, rather than by every
        // downstream consumer of `attachment_ocr_regions`.
        let y_top = 1.0 - (bbox.origin.y + bbox.size.height);
        lines.push((
            y_top as f32,
            OcrRegion {
                text,
                confidence: Some(confidence),
                bbox: (
                    bbox.origin.x as f32,
                    y_top as f32,
                    bbox.size.width as f32,
                    bbox.size.height as f32,
                ),
            },
        ));
    }
    // Vision documents no ordering over `results()`; sorted top-to-bottom so
    // the joined text reads the way the page does.
    lines.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    let regions: Vec<OcrRegion> = lines.into_iter().map(|(_, region)| region).collect();
    let text = regions
        .iter()
        .map(|region| region.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let confidences: Vec<f32> = regions
        .iter()
        .filter_map(|region| region.confidence)
        .collect();
    let confidence = if confidences.is_empty() {
        None
    } else {
        Some(confidences.iter().sum::<f32>() / confidences.len() as f32)
    };

    Ok(OcrOutput {
        text,
        regions,
        confidence,
    })
}

/// Map `index.extract.ocr_langs`' tesseract-style codes (`"eng"`, the
/// config's own default — see `TesseractBackend`, which is where this
/// convention actually comes from) to the BCP-47 tags
/// `VNRecognizeTextRequest.recognitionLanguages` requires (`"en-US"`).
///
/// Only common languages are mapped. An unmapped code is dropped rather than
/// passed through malformed, and `None` when nothing mapped at all — in
/// which case the caller leaves `recognitionLanguages` unset entirely, and
/// Vision falls back to its own on-device language detection, which
/// degrades far better than a rejected or silently-ignored bad tag would.
fn bcp47_languages(langs: &[String]) -> Option<Vec<&'static str>> {
    let mapped: Vec<&'static str> = langs
        .iter()
        .filter_map(|code| bcp47_language(code))
        .collect();
    (!mapped.is_empty()).then_some(mapped)
}

fn bcp47_language(code: &str) -> Option<&'static str> {
    Some(match code.to_ascii_lowercase().as_str() {
        "eng" => "en-US",
        "fra" | "fre" => "fr-FR",
        "deu" | "ger" => "de-DE",
        "spa" => "es-ES",
        "ita" => "it-IT",
        "por" => "pt-BR",
        "nld" | "dut" => "nl-NL",
        "chi_sim" | "zho" | "chi" => "zh-Hans",
        "chi_tra" => "zh-Hant",
        "jpn" => "ja-JP",
        "kor" => "ko-KR",
        "rus" => "ru-RU",
        "ukr" => "uk-UA",
        "pol" => "pl-PL",
        "swe" => "sv-SE",
        _ => return None,
    })
}
