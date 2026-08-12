//! The message projection a rule is evaluated against.
//!
//! [`MessageFacts`] is deliberately a *snapshot*, loaded once per message per
//! evaluation, rather than a handle the predicates query through. Two reasons,
//! both load-bearing:
//!
//! - A rule set is many predicates over one message. Re-reading `messages`
//!   and `flags` per predicate would turn one evaluation into a dozen round
//!   trips through the blocking pool for data that cannot change underneath
//!   a single evaluation in any way the result should depend on.
//! - Evaluation is a pure function of the snapshot, which is what lets
//!   [`super::eval`] be tested without a database at all and what makes a
//!   backtest's per-message verdict reproducible from the same inputs.
//!
//! # Headers are loaded only when a rule asks for one
//!
//! `messages.raw` is the whole RFC822 message — for a mail with attachments,
//! megabytes of it. Loading that for every message just in case some rule
//! has a `header.*` predicate would dominate the cost of evaluating rules
//! that have none. [`load_facts`] therefore takes `need_headers` and only
//! selects (and scans) `raw` when the compiled rule set actually contains a
//! header predicate.
//!
//! # Why a hand-rolled header scan
//!
//! [`scan_headers`] reads the header block only — everything up to the first
//! blank line — unfolds continuation lines per RFC 5322 §2.2.3, and splits on
//! the first colon. It deliberately does not build a `mail_parser::Message`:
//! that parses the entire MIME tree, decodes bodies, and allocates for every
//! part, all to answer "what is the List-Id". The scan is bounded twice over
//! — by [`MAX_HEADER_BYTES`] and by a cap on how many headers it will keep —
//! because `raw` is attacker-controlled bytes, and a message with a hundred
//! thousand headers is a thing a mailbox can receive.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::OptionalExtension;

use crate::error::Error;
use crate::storage::Database;

/// How much of `messages.raw` the header scan will read before giving up on
/// finding the end of the header block. Generous for real mail (RFC 5321
/// suggests 1000 octets per line and real headers run to a few kilobytes),
/// small enough that a malformed message with no blank line at all cannot
/// make this scan the whole body.
pub const MAX_HEADER_BYTES: usize = 256 * 1024;

/// How many distinct header *fields* are retained. A message may legitimately
/// repeat `Received` dozens of times; a hundred thousand of anything is an
/// attack, not mail.
const MAX_HEADERS: usize = 512;

/// One message, projected into everything a rule can predicate on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MessageFacts {
    /// The local stable message id (`messages.id`).
    ///
    /// This — not the RFC822 `Message-ID` header — is what the classification
    /// cache is keyed by. The header is optional, forgeable, and not unique
    /// in practice; the row id is present for every message, is what every
    /// other table in this schema references, and is what makes
    /// `ON DELETE CASCADE` clean up a deleted message's cached verdicts.
    pub message_id: i64,
    /// Owning account.
    pub account_id: i64,
    /// Owning mailbox row.
    pub mailbox_id: i64,
    /// Owning mailbox name, as IMAP names it.
    pub mailbox: String,
    /// The RFC822 `Message-ID` header, when the message carries one. Reported
    /// in backtest output so a human can correlate a verdict with the mail in
    /// another client; never used as a key.
    pub rfc_message_id: Option<String>,
    /// The sender rendered as `Display Name <addr@example.com>` — what a
    /// `from` predicate matches against.
    pub from: String,
    /// The sender's bare address, for the reply draft action.
    pub from_addr: Option<String>,
    /// The sender's display name, for the reply draft action.
    pub from_name: Option<String>,
    /// The subject, or the empty string when absent (so a `subject` regex
    /// runs against something rather than being skipped).
    pub subject: String,
    /// The extracted plain-text body, or the empty string.
    pub body: String,
    /// RFC822 size in bytes, falling back to the raw blob's own length when
    /// the server did not report one.
    pub size: u64,
    /// `Date`, falling back to INTERNALDATE, falling back to 0.
    pub date: i64,
    /// The message's IMAP flags/keywords.
    pub flags: BTreeSet<String>,
    /// Header name (lowercased) to every value it appeared with, in order.
    /// Empty unless the evaluation asked for headers.
    pub headers: BTreeMap<String, Vec<String>>,
}

impl MessageFacts {
    /// Every value of `name` (matched case-insensitively).
    #[must_use]
    pub fn header_values(&self, name: &str) -> &[String] {
        self.headers
            .get(&name.trim().to_ascii_lowercase())
            .map_or(&[][..], Vec::as_slice)
    }

    /// Render this message the way a model sees it — the same text used for
    /// a `claude_is` classification and frozen into a few-shot example, so
    /// the two can never drift.
    ///
    /// `max_body_chars` bounds the body, matching what
    /// `ai.privacy.max_body_chars` does for the rest of the AI pipeline.
    #[must_use]
    pub fn render_for_model(&self, max_body_chars: usize) -> String {
        let from = if self.from.is_empty() {
            "(unknown sender)"
        } else {
            &self.from
        };
        let subject = if self.subject.is_empty() {
            "(no subject)"
        } else {
            &self.subject
        };
        let (body, truncated) = match self.body.char_indices().nth(max_body_chars) {
            Some((idx, _)) => (&self.body[..idx], true),
            None => (self.body.as_str(), false),
        };
        let mut out = format!("From: {from}\nSubject: {subject}\n\n{body}");
        if truncated {
            out.push_str("\n\n[body truncated]");
        }
        out
    }
}

/// Load one message's facts.
///
/// # Errors
/// [`Error::NotFound`] if the message no longer exists; otherwise a mapped
/// storage error.
pub async fn load_facts(
    db: &Database,
    message_id: i64,
    need_headers: bool,
) -> Result<MessageFacts, Error> {
    let facts = db
        .read(move |conn| {
            let row = conn
                .query_row(
                    "SELECT m.account_id, m.mailbox_id, mb.name, m.message_id, m.subject,
                            m.from_addr, m.from_name, m.body_text,
                            COALESCE(m.size, LENGTH(m.raw), 0),
                            COALESCE(m.date, m.internaldate, 0)
                     FROM messages m
                     JOIN mailboxes mb ON mb.id = m.mailbox_id
                     WHERE m.id = ?1",
                    [message_id],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, Option<String>>(3)?,
                            row.get::<_, Option<String>>(4)?,
                            row.get::<_, Option<String>>(5)?,
                            row.get::<_, Option<String>>(6)?,
                            row.get::<_, Option<String>>(7)?,
                            row.get::<_, i64>(8)?,
                            row.get::<_, i64>(9)?,
                        ))
                    },
                )
                .optional()?;
            let Some((
                account_id,
                mailbox_id,
                mailbox,
                rfc_message_id,
                subject,
                from_addr,
                from_name,
                body_text,
                size,
                date,
            )) = row
            else {
                return Ok(None);
            };

            let mut stmt = conn.prepare("SELECT flag FROM flags WHERE message_id = ?1")?;
            let flags: BTreeSet<String> = stmt
                .query_map([message_id], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<_>>()?;

            let headers = if need_headers {
                let raw: Option<Vec<u8>> = conn
                    .query_row(
                        "SELECT raw FROM messages WHERE id = ?1",
                        [message_id],
                        |r| r.get(0),
                    )
                    .optional()?
                    .flatten();
                raw.as_deref().map(scan_headers).unwrap_or_default()
            } else {
                BTreeMap::new()
            };

            Ok(Some(MessageFacts {
                message_id,
                account_id,
                mailbox_id,
                mailbox,
                rfc_message_id,
                from: render_from(from_name.as_deref(), from_addr.as_deref()),
                from_addr,
                from_name,
                subject: subject.unwrap_or_default(),
                body: body_text.unwrap_or_default(),
                // A negative size is not representable in a mailbox; clamp
                // rather than error, so one nonsense row cannot fail a whole
                // backtest.
                size: u64::try_from(size).unwrap_or(0),
                date,
                flags,
                headers,
            }))
        })
        .await?;

    facts.ok_or_else(|| Error::not_found(format!("message {message_id}")))
}

/// The message ids in `account_id` whose date is at or after `since`,
/// most recent first, at most `limit` of them — the backtest/dry-run window.
///
/// # Errors
/// A mapped storage error.
pub async fn window(
    db: &Database,
    account_id: i64,
    since: i64,
    limit: usize,
) -> Result<Vec<i64>, Error> {
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    Ok(db
        .read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id FROM messages
                 WHERE account_id = ?1 AND COALESCE(date, internaldate, 0) >= ?2
                 ORDER BY COALESCE(date, internaldate, 0) DESC, id DESC
                 LIMIT ?3",
            )?;
            let ids = stmt
                .query_map(rusqlite::params![account_id, since, limit], |row| {
                    row.get::<_, i64>(0)
                })?
                .collect::<rusqlite::Result<Vec<i64>>>()?;
            Ok(ids)
        })
        .await?)
}

/// Render a sender the way a `from` predicate sees it.
#[must_use]
pub fn render_from(name: Option<&str>, addr: Option<&str>) -> String {
    match (
        name.filter(|n| !n.is_empty()),
        addr.filter(|a| !a.is_empty()),
    ) {
        (Some(name), Some(addr)) => format!("{name} <{addr}>"),
        (Some(name), None) => name.to_owned(),
        (None, Some(addr)) => format!("<{addr}>"),
        (None, None) => String::new(),
    }
}

/// Scan the RFC 5322 header block out of `raw`.
///
/// Bounded twice — see the module docs. Unfolds continuation lines (a line
/// beginning with space or tab continues the previous field). Anything that
/// is not `name: value` is skipped rather than failing the scan: real mail
/// contains malformed headers, and one of them must not cost a rule its
/// evaluation.
#[must_use]
pub fn scan_headers(raw: &[u8]) -> BTreeMap<String, Vec<String>> {
    let end = raw.len().min(MAX_HEADER_BYTES);
    // Lossy: header bytes are supposed to be ASCII, but non-conforming mail
    // exists and a rule should still get to match what it can rather than
    // see nothing at all.
    let text = String::from_utf8_lossy(&raw[..end]);

    let mut headers: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut fields = 0usize;
    let mut current: Option<(String, String)> = None;

    for line in text.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.is_empty() {
            // End of the header block. Anything after it is the body.
            break;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some((_, value)) = current.as_mut() {
                value.push(' ');
                value.push_str(line.trim());
            }
            continue;
        }
        if let Some((name, value)) = current.take() {
            headers.entry(name).or_default().push(value);
            fields += 1;
            if fields >= MAX_HEADERS {
                return headers;
            }
        }
        if let Some((name, value)) = line.split_once(':') {
            let name = name.trim();
            if !name.is_empty() {
                current = Some((name.to_ascii_lowercase(), value.trim().to_owned()));
            }
        }
    }
    if let Some((name, value)) = current {
        headers.entry(name).or_default().push(value);
    }
    headers
}
