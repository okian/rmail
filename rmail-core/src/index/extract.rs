//! The extraction stage: turning a stored message into normalized text.
//!
//! Everything searchable is derived from `index_content`, not from `messages`
//! directly, and this is what fills it. Three decisions shape the module.
//!
//! # A message is not one document
//!
//! A subject, the header line, a body, a note the user attached, and an AI
//! summary are different things. They carry different weight in ranking — a
//! term in a subject means more than the same term buried in a quoted reply —
//! and they have different lifetimes: a summary is rewritten when the model
//! changes, a body never is. Storing them as one blob would make every one of
//! those a rewrite of all of them, and would throw away the only signal the
//! ranker has about where a match came from.
//!
//! # The text is normalized, and the hash is over the normalized form
//!
//! Whitespace collapsed, control characters dropped, HTML already stripped. The
//! original is still in `messages.raw`; this is the form the indexes agree on.
//! Hashing the normalized text rather than the source is what makes the
//! re-index decision meaningful: a mail client that re-wraps a body, or a server
//! that returns `\r\n` where it once returned `\n`, has not changed anything
//! searchable, and re-embedding a hundred thousand messages because of it would
//! be a very expensive way to learn that.
//!
//! # Extraction decides what the later stages dedup on
//!
//! [`extract_message`] emits one hash for the whole message — a hash *of the
//! part hashes* — and enqueues the lexical, entity and semantic stages against
//! it. That is the value [`crate::index::IndexQueue`] compares to decide
//! whether those stages need to run, so it must change when and only when
//! something searchable changed. A per-part hash would be too fine (a stage
//! reads every part) and the raw message would be too coarse.

use std::fmt;

use rusqlite::OptionalExtension;
use sha2::{Digest, Sha256};

use crate::error::Error;
use crate::index::{IndexKind, IndexQueue, NewJob, PRIORITY_NORMAL};
use crate::repo;
use crate::storage::Database;

/// Name recorded in `index_content.extractor`, so a fixed extractor's output
/// can be told from a broken one's without re-reading the mail.
pub const EXTRACTOR: &str = "rmail/text@1";

/// Below this many characters, language detection is guessing.
///
/// A two-word subject has no detectable language, and a wrong guess picks the
/// wrong stemmer — which is worse than picking none, because the ranker then
/// silently fails to match obvious terms.
const MIN_LANG_CHARS: usize = 24;

/// Confidence below which a detected language is not recorded.
const MIN_LANG_CONFIDENCE: f64 = 0.6;

/// Which part of a message a row of extracted text came from.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Part {
    /// The subject line.
    Subject,
    /// Who sent it: display name and address.
    Sender,
    /// Who received it: To and Cc.
    Recipients,
    /// The message body.
    Body,
    /// Text extracted from an attachment, by its MIME part id.
    Attachment(String),
    /// A note the user attached to the message.
    Note,
    /// An AI-generated summary.
    Summary,
}

impl Part {
    /// The parts this stage produces, and therefore the only ones it may
    /// remove.
    ///
    /// Notes, summaries and attachment text are written by other subsystems on
    /// their own schedules. A sweep that deleted everything it did not itself
    /// produce would wipe a user's note every time the message was re-synced.
    pub const EXTRACTOR_OWNED: [Self; 4] =
        [Self::Subject, Self::Sender, Self::Recipients, Self::Body];

    /// The stable string stored in `index_content.part`.
    #[must_use]
    pub fn as_key(&self) -> String {
        match self {
            Self::Subject => "subject".to_owned(),
            Self::Sender => "sender".to_owned(),
            Self::Recipients => "recipients".to_owned(),
            Self::Body => "body".to_owned(),
            Self::Attachment(id) => format!("attachment:{id}"),
            Self::Note => "note".to_owned(),
            Self::Summary => "summary".to_owned(),
        }
    }

    /// Parse a stored key back into a part.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] for a key no version of this code wrote.
    pub fn parse(key: &str) -> Result<Self, Error> {
        Ok(match key {
            "subject" => Self::Subject,
            "sender" => Self::Sender,
            "recipients" => Self::Recipients,
            "body" => Self::Body,
            "note" => Self::Note,
            "summary" => Self::Summary,
            other => match other.strip_prefix("attachment:") {
                Some(id) => Self::Attachment(id.to_owned()),
                None => {
                    return Err(Error::internal(format!(
                        "unknown index content part: {other}"
                    )))
                }
            },
        })
    }
}

impl fmt::Display for Part {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_key())
    }
}

/// One extracted, normalized part ready to store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedPart {
    /// Which part.
    pub part: Part,
    /// Source MIME type, where there was one.
    pub mime: Option<String>,
    /// Detected language, where the text was long enough to tell.
    pub lang: Option<String>,
    /// The normalized text.
    pub text: String,
    /// Its length in characters — not bytes, because that is what a reader
    /// means by "how long is this".
    pub chars: i64,
    /// Hash over the normalized text.
    pub content_hash: Vec<u8>,
}

/// What one extraction did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractReport {
    /// The message extracted.
    pub message_id: i64,
    /// Parts whose stored text this run replaced.
    pub written: Vec<Part>,
    /// Parts that were already stored with the same hash.
    pub unchanged: Vec<Part>,
    /// Parts removed because the message no longer has them.
    pub removed: Vec<Part>,
    /// The hash the follow-on stages dedup against.
    pub content_hash: Vec<u8>,
    /// Follow-on jobs actually queued. Zero means every later stage had
    /// already indexed this exact content.
    pub follow_on: u64,
}

impl ExtractReport {
    /// Whether this run changed anything searchable.
    #[must_use]
    pub fn changed(&self) -> bool {
        !self.written.is_empty() || !self.removed.is_empty()
    }
}

/// Extract a stored message into `index_content` and queue the stages that
/// consume it.
///
/// Idempotent: re-running over an unchanged message writes nothing, removes
/// nothing, and queues nothing. That is the common case — a sync sweep
/// re-enqueues the world on every restart — so it costs a read and a hash
/// rather than a rewrite.
///
/// # Errors
///
/// - [`Error::NotFound`] if the message does not exist.
/// - A mapped storage error.
#[tracing::instrument(skip(db, queue), fields(written, unchanged))]
pub async fn extract_message(
    db: &Database,
    queue: &IndexQueue,
    message_id: i64,
    priority: i64,
) -> Result<ExtractReport, Error> {
    let message = db
        .read(move |c| repo::get_message_text(c, message_id))
        .await?
        .ok_or_else(|| Error::not_found(format!("message {message_id} not found")))?;

    // Hashing and HTML stripping are CPU work; a large body should not hold the
    // async runtime, and it must not hold the writer lock either.
    let parts = tokio::task::spawn_blocking(move || extract_parts(&message))
        .await
        .map_err(|e| Error::internal(format!("extraction task failed: {e}")))?;

    // Hash the *stored* set, not just what this run produced. The follow-on
    // stages read every part of a message — including a note the user attached
    // and a summary the AI wrote — so a hash over only this stage's output
    // would stay byte-identical when one of those appeared, the dedup would
    // drop the jobs, and the note would never be indexed at all.
    let outcome = store(db, message_id, parts).await?;
    let content_hash = outcome.content_hash;

    // Only the stages that read extracted text. `Thread` is rolled up from
    // whole conversations rather than one message, and `Extract` is this.
    let follow_on = queue
        .enqueue(
            [IndexKind::Lexical, IndexKind::Entities, IndexKind::Semantic]
                .into_iter()
                .map(|kind| {
                    NewJob::new(message_id, kind)
                        .content_hash(content_hash.clone())
                        .priority(priority)
                })
                .collect(),
            None,
        )
        .await?;

    let span = tracing::Span::current();
    span.record("written", outcome.written.len());
    span.record("unchanged", outcome.unchanged.len());
    tracing::debug!(
        written = outcome.written.len(),
        unchanged = outcome.unchanged.len(),
        removed = outcome.removed.len(),
        follow_on,
        "message extracted"
    );

    Ok(ExtractReport {
        message_id,
        written: outcome.written,
        unchanged: outcome.unchanged,
        removed: outcome.removed,
        content_hash,
        follow_on,
    })
}

/// Run the extraction stage for a leased job.
///
/// The shape a worker uses: extract, then report the outcome so the queue can
/// record what was indexed. Separated from [`extract_message`] so the
/// extraction can be driven and tested without a lease.
///
/// # Errors
///
/// As [`extract_message`].
pub async fn run_job(
    db: &Database,
    queue: &IndexQueue,
    lease: &crate::index::Lease,
) -> Result<ExtractReport, Error> {
    extract_message(db, queue, lease.message_id, PRIORITY_NORMAL).await
}

/// What [`store`] found.
struct StoreOutcome {
    written: Vec<Part>,
    unchanged: Vec<Part>,
    removed: Vec<Part>,
    /// Hash over every part now stored for the message.
    content_hash: Vec<u8>,
}

/// Write the parts that changed, leave the ones that did not, and drop rows for
/// parts the message no longer has.
///
/// The removal matters: an edited draft that loses its body, or an extractor
/// that stops producing a part, would otherwise leave stale text in the index
/// forever — searchable, and matching nothing that exists.
async fn store(
    db: &Database,
    message_id: i64,
    parts: Vec<ExtractedPart>,
) -> Result<StoreOutcome, Error> {
    // Parts this run produces are the complete set for the *extractable* parts.
    // Notes and summaries are written by other subsystems and must survive an
    // extraction that knows nothing about them.
    let produced: Vec<String> = parts.iter().map(|p| p.part.as_key()).collect();

    let outcome = db
        .write(move |conn| {
            let tx = conn.transaction()?;
            let mut written = Vec::new();
            let mut unchanged = Vec::new();

            for part in &parts {
                let key = part.part.as_key();
                // `.optional()?` rather than `.ok()`: collapsing a real
                // storage error into "no row" would silently rewrite every part
                // on every sweep, which is the failure this module exists to
                // avoid.
                let existing: Option<(Vec<u8>, Option<String>, Option<String>)> = tx
                    .query_row(
                        "SELECT content_hash, lang, extractor FROM index_content
                         WHERE message_id = ?1 AND part = ?2",
                        rusqlite::params![message_id, &key],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .optional()?;
                // The hash covers the text alone, so an unchanged body whose
                // *extractor* or detected language moved on still needs
                // rewriting — otherwise bumping EXTRACTOR leaves every
                // untouched row claiming the old one, which defeats the point
                // of recording it.
                let same = existing.as_ref().is_some_and(|(hash, lang, extractor)| {
                    hash.as_slice() == part.content_hash.as_slice()
                        && lang.as_deref() == part.lang.as_deref()
                        && extractor.as_deref() == Some(EXTRACTOR)
                });
                if same {
                    unchanged.push(key);
                    continue;
                }
                tx.execute(
                    "INSERT INTO index_content
                         (message_id, part, mime, lang, text, chars, content_hash,
                          extracted_at, extractor)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, unixepoch(), ?8)
                     ON CONFLICT(message_id, part) DO UPDATE SET
                         mime = excluded.mime,
                         lang = excluded.lang,
                         text = excluded.text,
                         chars = excluded.chars,
                         content_hash = excluded.content_hash,
                         extracted_at = excluded.extracted_at,
                         extractor = excluded.extractor",
                    rusqlite::params![
                        message_id,
                        &key,
                        part.mime,
                        part.lang,
                        part.text,
                        part.chars,
                        part.content_hash,
                        EXTRACTOR,
                    ],
                )?;
                written.push(key);
            }

            // Drop the parts *this* stage owns that this run did not produce.
            //
            // Scoping matters in both directions. A note or a summary belongs
            // to another subsystem and is not this stage's to delete. And an
            // `attachment:` row is produced by the attachment pipeline, not
            // here — sweeping those would wipe minutes of OCR work on every
            // routine re-extract, silently, because the message hash would not
            // change and nothing downstream would re-run.
            let stale: Vec<String> = {
                let owned: Vec<String> = Part::EXTRACTOR_OWNED
                    .iter()
                    .map(Part::as_key)
                    .filter(|key| !produced.contains(key))
                    .collect();
                let mut stmt = tx.prepare(
                    "SELECT part FROM index_content WHERE message_id = ?1 AND part = ?2",
                )?;
                let mut stale = Vec::new();
                for key in owned {
                    let found: Option<String> = stmt
                        .query_row(rusqlite::params![message_id, &key], |row| row.get(0))
                        .optional()?;
                    stale.extend(found);
                }
                stale
            };
            {
                let mut delete =
                    tx.prepare("DELETE FROM index_content WHERE message_id = ?1 AND part = ?2")?;
                for key in &stale {
                    delete.execute(rusqlite::params![message_id, key])?;
                }
            }

            // Read the stored set back inside the same transaction, so the
            // hash describes exactly what a follow-on stage will find.
            let stored: Vec<(String, Vec<u8>)> = {
                let mut stmt = tx.prepare(
                    "SELECT part, content_hash FROM index_content WHERE message_id = ?1",
                )?;
                let rows = stmt
                    .query_map([message_id], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                rows
            };

            tx.commit()?;
            Ok((written, unchanged, stale, stored))
        })
        .await?;

    let (written, unchanged, removed, stored) = outcome;
    Ok(StoreOutcome {
        written: parse_keys(written)?,
        unchanged: parse_keys(unchanged)?,
        removed: parse_keys(removed)?,
        content_hash: message_hash(&stored),
    })
}

fn parse_keys(keys: Vec<String>) -> Result<Vec<Part>, Error> {
    keys.iter().map(|key| Part::parse(key)).collect()
}

/// Build the extractable parts of a stored message.
///
/// Only parts with text survive: an empty subject is not a document, and a row
/// of empty text would cost an index entry and match nothing.
fn extract_parts(message: &repo::MessageText) -> Vec<ExtractedPart> {
    let mut parts = Vec::with_capacity(3);

    if let Some(text) = message.subject.as_deref().map(normalize) {
        if !text.is_empty() {
            parts.push(build(Part::Subject, None, text));
        }
    }

    // Sender and recipients are separate parts because they rank differently:
    // the PRD weights a From hit at 4.0 and a To/Cc hit at 2.0. Merging them
    // into one line would be simpler and would throw that distinction away —
    // mail *from* someone is a stronger match than mail merely addressed to
    // them alongside forty other people.
    let sender = join(&[message.from_name.as_deref(), message.from_addr.as_deref()]);
    if !sender.is_empty() {
        parts.push(build(Part::Sender, None, sender));
    }
    let recipients = join(&[message.to_addrs.as_deref(), message.cc_addrs.as_deref()]);
    if !recipients.is_empty() {
        parts.push(build(Part::Recipients, None, recipients));
    }

    // `body_text` is already the stripped projection of an HTML-only message
    // (task 9), so falling back to stripping the HTML here covers only mail
    // stored before that projection existed.
    // The MIME label is decided by the branch that produced the text, not
    // re-derived afterwards: the two disagree for a body made only of
    // zero-width characters, which `trim` keeps and `normalize` drops.
    let body = message
        .body_text
        .as_deref()
        .map(normalize)
        .filter(|text| !text.is_empty())
        .map(|text| (text, "text/plain"))
        .or_else(|| {
            message
                .body_html
                .as_deref()
                .map(strip_html)
                .map(|text| normalize(&text))
                .filter(|text| !text.is_empty())
                .map(|text| (text, "text/html"))
        });
    if let Some((body, mime)) = body {
        parts.push(build(Part::Body, Some(mime.to_owned()), body));
    }

    parts
}

/// Join present fields into one normalized line.
///
/// A display name and an address belong together: a search for "Ada" and a
/// search for "ada@example.com" should both find the same mail, and keeping
/// them in one part means the ranker scores them once rather than twice.
fn join(fields: &[Option<&str>]) -> String {
    let mut line = String::new();
    for field in fields.iter().copied().flatten() {
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(field);
    }
    normalize(&line)
}

fn build(part: Part, mime: Option<String>, text: String) -> ExtractedPart {
    let chars = i64::try_from(text.chars().count()).unwrap_or(i64::MAX);
    let content_hash = hash(&text);
    let lang = detect_language(&text);
    ExtractedPart {
        part,
        mime,
        lang,
        text,
        chars,
        content_hash,
    }
}

/// Reduce text to the form the indexes agree on.
///
/// Control characters out, every run of whitespace to a single space, trimmed.
/// Deterministic by construction, because the content hash is over the result:
/// a mail client that re-wraps a body, or a server that switches `\r\n` for
/// `\n`, has changed nothing searchable and must not trigger a re-embed.
#[must_use]
pub fn normalize(text: &str) -> String {
    use unicode_normalization::UnicodeNormalization;

    let mut out = String::with_capacity(text.len());
    let mut pending_space = false;
    // NFC first. `café` written as `e` + combining acute and `café` written
    // with the precomposed character render identically, hash differently, and
    // tokenize differently — so a decomposed body would trigger a full re-embed
    // and then fail to match a query typed the other way. For a product whose
    // first feature is search, that is not a detail.
    for ch in text.nfc() {
        if ch.is_whitespace() {
            pending_space = !out.is_empty();
            continue;
        }
        // A zero-width non-joiner separates words in Persian, Arabic and Hindi.
        // Deleting it would weld them into a token nobody will ever type, so it
        // becomes a space rather than nothing.
        if ch == '\u{200C}' {
            pending_space = !out.is_empty();
            continue;
        }
        if is_ignorable(ch) {
            continue;
        }
        if pending_space {
            out.push(' ');
            pending_space = false;
        }
        out.push(ch);
    }
    out
}

/// Characters that carry no meaning to a reader and must not reach the index.
///
/// Invisible characters make two visually identical bodies hash differently,
/// and a soft hyphen inside a word — which mail clients insert freely for
/// hyphenation — splits it into a token the user can never search for. The
/// bidi controls are here for a second reason: they are the Trojan-Source
/// display-spoofing vector, and a search snippet is a rendering surface.
fn is_ignorable(ch: char) -> bool {
    ch.is_control()
        || matches!(ch,
            '\u{00AD}'                 // soft hyphen
            | '\u{180E}'               // Mongolian vowel separator
            | '\u{200B}'..='\u{200F}'  // zero-width space .. RLM
            | '\u{202A}'..='\u{202E}'  // bidi embedding/override
            | '\u{2060}'..='\u{2064}'  // word joiner .. invisible plus
            | '\u{2066}'..='\u{2069}'  // bidi isolates
            | '\u{FEFF}'               // zero-width no-break space / BOM
        )
}

/// The most HTML this stage will try to render.
///
/// html2text is quadratic in nesting depth: a megabyte of nested blockquotes
/// takes about a minute at this width, and it runs on `spawn_blocking`, which
/// cannot be aborted. A cap is the only thing standing between one crafted
/// message and a pinned pool thread — retried five times, because the queue
/// does not know the difference between slow and broken.
const MAX_HTML_BYTES: usize = 4 * 1024 * 1024;

/// Strip HTML to text, best effort.
///
/// `html2text::from_read` *panics* on content it cannot render at the given
/// width. This path is reached precisely when [`crate::message::parse`]'s
/// stripper already failed — its fallback is empty text, and empty text is what
/// sends the body here — so the pathological input is adversarially selected
/// for. Use the fallible API and give up rather than take the process down.
fn strip_html(html: &str) -> String {
    if html.len() > MAX_HTML_BYTES {
        tracing::warn!(
            bytes = html.len(),
            "HTML body over the extraction cap; indexing it as empty"
        );
        return String::new();
    }
    // Wide enough that html2text wraps rarely — normalization collapses the
    // breaks it does insert — but not so wide that the renderer needs a
    // ten-thousand-level nesting before it gives up. `TooNarrow` is a
    // *recoverable* answer here and an unreachable one at an absurd width,
    // which would leave the guard below untestable.
    html2text::config::plain()
        .string_from_read(html.as_bytes(), 200)
        .unwrap_or_default()
}

/// Detect the language, or decline to.
///
/// `None` is a real answer. Short text has no detectable language, and a wrong
/// guess selects the wrong stemmer — which fails to match obvious terms and
/// looks like a broken index rather than a bad guess.
fn detect_language(text: &str) -> Option<String> {
    if text.chars().count() < MIN_LANG_CHARS {
        return None;
    }
    let info = whatlang::detect(text)?;
    (info.is_reliable() && info.confidence() >= MIN_LANG_CONFIDENCE)
        .then(|| info.lang().code().to_owned())
}

/// Hash the normalized text of one part.
fn hash(text: &str) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hasher.finalize().to_vec()
}

/// The hash the follow-on stages dedup against: a hash of the part hashes.
///
/// Per-part would be too fine — a stage reads every part, so it must re-run if
/// any changed — and the raw message would be too coarse, re-running every
/// stage over a header the indexes never see. Part keys go into the hash too,
/// so a part appearing or disappearing changes it even when the surviving text
/// does not.
fn message_hash(parts: &[(String, Vec<u8>)]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    // Sorted, because the stored set is a set: two runs that leave the same
    // parts in a different row order have left the same content.
    let mut keyed: Vec<(&str, &[u8])> = parts
        .iter()
        .map(|(key, digest)| (key.as_str(), digest.as_slice()))
        .collect();
    keyed.sort_unstable();
    for (key, digest) in keyed {
        // The key goes in, not merely into the ordering: identical text under a
        // different part is a different document, because the same words in a
        // subject and in a body rank differently.
        hasher.update(key.as_bytes());
        hasher.update([0]);
        hasher.update(digest);
    }
    hasher.finalize().to_vec()
}

#[cfg(test)]
mod tests;
