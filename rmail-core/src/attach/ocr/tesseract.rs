//! The `tesseract` binary, shelled out to rather than linked.
//!
//! See the parent module's docs for why: `libtesseract`/`liblept` are not
//! present at build time on this machine, or most machines rmail runs on,
//! and a hard compile-time dependency on them would break every build for a
//! feature whose entire premise is "opt-in." A subprocess call has none of
//! that risk — it either finds `tesseract` on `PATH` at OCR time or it does
//! not, and either way nothing about the build is affected.
//!
//! Output is requested as TSV (`tesseract <file> stdout -l <langs> tsv`)
//! rather than plain text, because plain text throws away exactly what this
//! module exists to keep: per-word bounding boxes and confidences. TSV is
//! tesseract's own structured format for that — one row per element at every
//! granularity from page down to word — and needs no dependency to parse: it
//! is genuinely tab-separated, unquoted, and a word's text cannot itself
//! contain the tab that delimits it.

use std::process::Command;

use super::{
    BackendError, OcrOutput, OcrRegion, ScratchFile, SubprocessError, SUBPROCESS_DEADLINE,
};

/// `tesseract`'s page-segmentation mode: "fully automatic page segmentation,
/// but no OSD [orientation/script detection]." The default a plain
/// `tesseract image out` invocation already uses; named explicitly so a
/// future change to tesseract's own default does not silently change what
/// this crate asks for.
const PSM_AUTO: &str = "3";

/// The row granularity TSV reports: 1 page, 2 block, 3 paragraph, 4 line, 5
/// word. Words are what carry text and a confidence; a line's own row (level
/// 4) never does — its box is the union of its words', which is exactly what
/// this module recomputes rather than trusts, so a truncated word list still
/// produces a consistent box.
const TSV_LEVEL_WORD: &str = "5";

pub(super) fn recognize(image: &[u8], langs: &[String]) -> Result<OcrOutput, BackendError> {
    let scratch = ScratchFile::write(image, "img").ok_or_else(|| {
        BackendError::Failed("could not write a scratch file for tesseract".to_owned())
    })?;

    let mut command = Command::new("tesseract");
    command.arg(scratch.path()).arg("stdout");
    if !langs.is_empty() {
        command.arg("-l").arg(langs.join("+"));
    }
    command.args(["--psm", PSM_AUTO, "tsv"]);

    // `run_with_deadline` rather than `Command::output()`: the latter waits
    // forever, and a `tesseract` invocation that never returns would
    // otherwise hold this attachment's `OCR_SLOTS` permit — one of only two
    // — for good. See the parent module's docs for the failure mode this
    // avoids.
    let output = match super::run_with_deadline(command, SUBPROCESS_DEADLINE) {
        Ok(output) => output,
        Err(SubprocessError::NotFound) => {
            return Err(BackendError::Unavailable(
                "tesseract is not on PATH".to_owned(),
            ))
        }
        Err(SubprocessError::TimedOut) => {
            return Err(BackendError::Failed(format!(
                "tesseract did not finish within {}s",
                SUBPROCESS_DEADLINE.as_secs()
            )))
        }
        Err(SubprocessError::Other(reason)) => {
            return Err(BackendError::Failed(format!(
                "could not run tesseract: {reason}"
            )))
        }
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(BackendError::Failed(format!(
            "tesseract exited with {}: {}",
            output.status,
            stderr.trim()
        )));
    }

    let tsv = String::from_utf8_lossy(&output.stdout);
    Ok(parse_tsv(&tsv))
}

/// One word, as tesseract's TSV reports it, in the pixel coordinates of the
/// image it ran against.
struct Word {
    block: i64,
    par: i64,
    line: i64,
    left: f64,
    top: f64,
    width: f64,
    height: f64,
    /// `0.0..=100.0`, or negative for a non-text row — filtered out before
    /// this struct is built.
    confidence: f32,
    text: String,
}

/// Parse tesseract's TSV output into line-level [`OcrRegion`]s.
///
/// Words sharing `(block, par, line)` are folded into one region: tesseract
/// reports a box and a confidence per *word*, but a page of individually
/// boxed words is a worse citation unit than the line they form — matching
/// the granularity Vision's own `VNRecognizedTextObservation` reports at.
fn parse_tsv(tsv: &str) -> OcrOutput {
    let mut page_width = 0.0_f64;
    let mut page_height = 0.0_f64;
    let mut words: Vec<Word> = Vec::new();

    for line in tsv.lines().skip(1) {
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() < 12 {
            continue;
        }
        let level = fields[0];
        let Ok(left) = fields[6].parse::<f64>() else {
            continue;
        };
        let Ok(top) = fields[7].parse::<f64>() else {
            continue;
        };
        let Ok(width) = fields[8].parse::<f64>() else {
            continue;
        };
        let Ok(height) = fields[9].parse::<f64>() else {
            continue;
        };

        if level == "1" {
            // The page row: its own box is the image's own dimensions,
            // needed to normalize every word's box to `0.0..=1.0`.
            page_width = width;
            page_height = height;
            continue;
        }
        if level != TSV_LEVEL_WORD {
            continue;
        }
        let Ok(confidence) = fields[10].parse::<f32>() else {
            continue;
        };
        // -1 marks a non-text structural row; tesseract's own convention,
        // not this crate's.
        if confidence < 0.0 {
            continue;
        }
        let text = fields[11].trim();
        if text.is_empty() {
            continue;
        }
        let (Ok(block), Ok(par), Ok(word_line)) = (
            fields[2].parse::<i64>(),
            fields[3].parse::<i64>(),
            fields[4].parse::<i64>(),
        ) else {
            continue;
        };
        words.push(Word {
            block,
            par,
            line: word_line,
            left,
            top,
            width,
            height,
            confidence: confidence / 100.0,
            text: text.to_owned(),
        });
    }

    if page_width <= 0.0 || page_height <= 0.0 || words.is_empty() {
        return OcrOutput::default();
    }

    // Grouped by first appearance rather than sorted by the key: tesseract
    // already emits words in reading order, and re-sorting by
    // `(block, par, line)` numbers would undo that wherever a document's
    // logical block order (tesseract's own layout analysis) differs from a
    // naive numeric one.
    let mut regions: Vec<OcrRegion> = Vec::new();
    let mut current_key: Option<(i64, i64, i64)> = None;
    let mut line_words: Vec<&Word> = Vec::new();

    let flush = |line_words: &mut Vec<&Word>, regions: &mut Vec<OcrRegion>| {
        if line_words.is_empty() {
            return;
        }
        regions.push(region_from_words(line_words, page_width, page_height));
        line_words.clear();
    };

    for word in &words {
        let key = (word.block, word.par, word.line);
        if current_key.is_some() && current_key != Some(key) {
            flush(&mut line_words, &mut regions);
        }
        current_key = Some(key);
        line_words.push(word);
    }
    flush(&mut line_words, &mut regions);

    let text = regions
        .iter()
        .map(|r| r.text.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let confidences: Vec<f32> = regions.iter().filter_map(|r| r.confidence).collect();
    let confidence = if confidences.is_empty() {
        None
    } else {
        Some(confidences.iter().sum::<f32>() / confidences.len() as f32)
    };

    OcrOutput {
        text,
        regions,
        confidence,
    }
}

/// Fold a line's words into one region: text joined by spaces, box the union
/// of every word's, confidence their mean.
fn region_from_words(words: &[&Word], page_width: f64, page_height: f64) -> OcrRegion {
    let text = words
        .iter()
        .map(|w| w.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let left = words.iter().map(|w| w.left).fold(f64::INFINITY, f64::min);
    let top = words.iter().map(|w| w.top).fold(f64::INFINITY, f64::min);
    let right = words
        .iter()
        .map(|w| w.left + w.width)
        .fold(f64::NEG_INFINITY, f64::max);
    let bottom = words
        .iter()
        .map(|w| w.top + w.height)
        .fold(f64::NEG_INFINITY, f64::max);
    let mean_confidence = words.iter().map(|w| w.confidence).sum::<f32>() / words.len() as f32;

    OcrRegion {
        text,
        confidence: Some(mean_confidence),
        bbox: (
            (left / page_width) as f32,
            (top / page_height) as f32,
            ((right - left) / page_width) as f32,
            ((bottom - top) / page_height) as f32,
        ),
    }
}
