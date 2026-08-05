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

pub mod extract;

use rusqlite::OptionalExtension;
use sha2::{Digest, Sha256};

use crate::config::IndexExtractConfig;
use crate::error::Error;
use crate::index::extract::Part;
use crate::storage::Database;
use extract::{Extracted, Format, Status};

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
}

/// One attachment's outcome, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
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
#[tracing::instrument(skip(db, config), fields(attachments, extracted))]
pub async fn extract_attachments(
    db: &Database,
    config: &IndexExtractConfig,
    message_id: i64,
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

        match status {
            Status::Ok => report.extracted += 1,
            Status::Failed => report.failed += 1,
            _ => report.empty += 1,
        }
        results.push(Outcome {
            part_id: part.part_id.clone(),
            status,
            extractor,
            bytes,
            text,
            hash,
        });
    }

    let present: Vec<String> = parts.into_iter().map(|part| part.part_id).collect();
    report.removed = persist(db, message_id, results, present).await?;

    let span = tracing::Span::current();
    span.record("attachments", report.attachments);
    span.record("extracted", report.extracted);
    tracing::debug!(
        attachments = report.attachments,
        unchanged = report.unchanged,
        extracted = report.extracted,
        empty = report.empty,
        failed = report.failed,
        "attachment text extracted"
    );
    Ok(report)
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
                "SELECT part_id, status, extractor, bytes, chars, pages
                 FROM attachment_extractions WHERE message_id = ?1 ORDER BY part_id",
            )?;
            let rows = stmt
                .query_map([message_id], |row| {
                    let status: String = row.get(1)?;
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
    let current: Vec<String> = [
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
                } else {
                    // No text is not the same as stale text. An attachment that
                    // used to extract and now does not — replaced by an
                    // encrypted version, say — must stop being searchable by
                    // what it used to say.
                    tx.execute(
                        "DELETE FROM index_content WHERE message_id = ?1 AND part = ?2",
                        rusqlite::params![message_id, &key],
                    )?;
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
                          chars, pages)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                     ON CONFLICT(message_id, part_id) DO UPDATE SET
                         status = excluded.status,
                         extractor = excluded.extractor,
                         content_hash = excluded.content_hash,
                         bytes = excluded.bytes,
                         chars = excluded.chars,
                         pages = excluded.pages,
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
                ])?;
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
                tx.execute(
                    "DELETE FROM attachment_pages WHERE message_id = ?1 AND part_id = ?2",
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
