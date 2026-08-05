//! The lexical index: FTS5 over the extracted parts, ranked by field-weighted
//! BM25.
//!
//! # Fields are the ranking, not the storage
//!
//! `index_content` stores what a message *is made of*; this table stores what a
//! ranker needs to tell matches apart. A term in a subject is stronger evidence
//! than the same term buried in a quoted reply, and mail *from* someone is
//! stronger than mail merely addressed to them alongside forty other people.
//! Those are the only distinctions the lexical retriever can make on its own,
//! so they are the columns.
//!
//! # Contentless, deliberately
//!
//! FTS5 stores the inverted index and nothing else. Keeping a second copy of
//! every body would double the largest table in the database to buy a snippet
//! feature that can read `index_content` just as easily. `contentless_delete=1`
//! is what makes that survivable — without it a row cannot be deleted without
//! handing FTS5 the original text back, which would mean keeping the text
//! after all.
//!
//! # Sync is replace-then-insert, keyed on the message
//!
//! One FTS row per message, `rowid = messages.id`. Re-indexing deletes and
//! re-inserts rather than updating: a contentless table has no update, and the
//! delete is what stops a part that disappeared from lingering in the index as
//! a term that matches nothing. Deletion of the message itself is handled by a
//! trigger, because a virtual table takes no foreign key and mail that stays
//! searchable after it is gone is worse than mail that is missing.
//!
//! # BM25 signs
//!
//! SQLite's `bm25()` returns a *negative* number, more negative meaning a
//! better match, so that `ORDER BY bm25(...)` sorts best-first without a
//! `DESC`. Every score this module hands out is negated into the orientation a
//! caller expects — higher is better — because the alternative is a subtle sign
//! error in every consumer for the life of the project.

use crate::config::Bm25Weights;
use crate::error::Error;
use crate::index::extract::Part;
use crate::storage::Database;

/// The FTS columns, in declaration order. `bm25()` takes one weight per column
/// in exactly this order, so the two must not drift.
const COLUMNS: [&str; 7] = [
    "subject",
    "sender",
    "recipients",
    "body",
    "attachments",
    "notes",
    "summary",
];

/// Largest result page one search returns.
pub const MAX_LIMIT: i64 = 500;

/// One lexical hit.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    /// The message that matched.
    pub message_id: i64,
    /// Relevance, higher is better. Derived from BM25, whose native sign is
    /// inverted.
    pub score: f64,
}

/// The lexical index.
///
/// Cheap to clone: every clone shares one database handle.
#[derive(Debug, Clone)]
pub struct FtsIndex {
    db: Database,
    weights: Bm25Weights,
}

impl FtsIndex {
    /// Open the index with the configured field weights.
    #[must_use]
    pub fn new(db: Database, weights: Bm25Weights) -> Self {
        Self { db, weights }
    }

    /// Index one message from its extracted parts.
    ///
    /// Reads `index_content`, folds each part into the column that ranks it,
    /// and replaces the message's row. A message with no extracted text is
    /// removed from the index rather than inserted empty — an empty document
    /// matches nothing and only costs the ranker a candidate to discard.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    #[tracing::instrument(skip(self))]
    pub async fn index_message(&self, message_id: i64) -> Result<bool, Error> {
        let indexed = self
            .db
            .write(move |conn| {
                let tx = conn.transaction()?;
                let parts: Vec<(String, String)> = {
                    let mut stmt =
                        tx.prepare("SELECT part, text FROM index_content WHERE message_id = ?1")?;
                    let rows = stmt
                        .query_map([message_id], |row| Ok((row.get(0)?, row.get(1)?)))?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    rows
                };

                // Replace, never update: a contentless table has no update, and
                // deleting first is what stops a part that disappeared from
                // lingering as a term that matches nothing.
                tx.execute("DELETE FROM fts_messages WHERE rowid = ?1", [message_id])?;

                let columns = fold(&parts);
                let indexed = columns.iter().any(|text| !text.is_empty());
                if indexed {
                    // Built from `COLUMNS` rather than written out, so the
                    // insert, the `bm25()` weight list and the fold below
                    // cannot drift out of the order they all depend on.
                    tx.execute(
                        &format!(
                            "INSERT INTO fts_messages (rowid, {}) VALUES (?1, {})",
                            COLUMNS.join(", "),
                            (2..=COLUMNS.len() + 1)
                                .map(|n| format!("?{n}"))
                                .collect::<Vec<_>>()
                                .join(", "),
                        ),
                        rusqlite::params![
                            message_id, columns[0], columns[1], columns[2], columns[3], columns[4],
                            columns[5], columns[6],
                        ],
                    )?;
                }
                tx.commit()?;
                Ok(indexed)
            })
            .await?;
        tracing::debug!(indexed, "lexical index updated");
        Ok(indexed)
    }

    /// Remove a message from the index.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    pub async fn remove_message(&self, message_id: i64) -> Result<bool, Error> {
        let removed = self
            .db
            .write(move |conn| {
                conn.execute("DELETE FROM fts_messages WHERE rowid = ?1", [message_id])
            })
            .await?;
        Ok(removed > 0)
    }

    /// Search, best match first.
    ///
    /// `query` is FTS5 match syntax: bare terms, `"quoted phrases"`, `AND`/`OR`/
    /// `NOT`, `col:term`, and `term*` prefixes.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] if the query is not valid FTS5 syntax — that
    /// is a user typing, not a server fault, and it must not read as one.
    /// Otherwise a mapped storage error.
    #[tracing::instrument(skip(self), fields(hits))]
    pub async fn search(&self, query: &str, limit: i64) -> Result<Vec<Hit>, Error> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            return Ok(Vec::new());
        }
        let page = if limit <= 0 {
            MAX_LIMIT
        } else {
            limit.min(MAX_LIMIT)
        };
        let sql = format!(
            "SELECT rowid, bm25(fts_messages, {}) FROM fts_messages
             WHERE fts_messages MATCH ?1
             ORDER BY bm25(fts_messages, {}) LIMIT ?2",
            self.weight_list(),
            self.weight_list(),
        );
        let query = trimmed.to_owned();
        let hits = self
            .db
            .read(move |conn| {
                let mut stmt = conn.prepare(&sql)?;
                let rows = stmt
                    .query_map(rusqlite::params![query, page], |row| {
                        Ok(Hit {
                            message_id: row.get(0)?,
                            // BM25 is negative-is-better so `ORDER BY` needs no
                            // `DESC`; every score that leaves this module is in
                            // the orientation a caller expects.
                            score: -row.get::<_, f64>(1)?,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<Hit>>>()?;
                Ok(rows)
            })
            .await
            .map_err(malformed_query)?;
        tracing::Span::current().record("hits", hits.len());
        Ok(hits)
    }

    /// How many messages are in the index.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    pub async fn len(&self) -> Result<i64, Error> {
        Ok(self
            .db
            .read(|conn| conn.query_row("SELECT count(*) FROM fts_messages", [], |row| row.get(0)))
            .await?)
    }

    /// Whether the index holds no messages.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    pub async fn is_empty(&self) -> Result<bool, Error> {
        Ok(self.len().await? == 0)
    }

    /// The weights as a `bm25()` argument list, in column order.
    ///
    /// `pub(crate)` rather than private: `retrieve::lexical` builds its own
    /// `bm25(fts_messages, ...)` SQL when a hard-filter mask has to be baked
    /// into the same query this module's own [`FtsIndex::search`] cannot
    /// express, and re-deriving the weight list there would risk exactly the
    /// column-order drift this module's docs warn about. Reusing this method
    /// keeps there being exactly one place that turns [`Bm25Weights`] into a
    /// `bm25()` argument list.
    pub(crate) fn weight_list(&self) -> String {
        self.column_weights()
            .iter()
            .map(|w| format!("{w}"))
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Weights in the same order as [`COLUMNS`].
    fn column_weights(&self) -> [f64; 7] {
        let w = &self.weights;
        // Negative or non-finite weights would make `bm25()` order results in
        // ways no configuration author intended; clamp rather than trust.
        [
            sane(w.subject),
            sane(w.from),
            sane(w.to),
            sane(w.body),
            sane(w.attachments),
            sane(w.notes),
            sane(w.ai_summary),
        ]
    }
}

/// A weight that cannot invert or poison the ranking.
fn sane(weight: f64) -> f64 {
    if weight.is_finite() && weight >= 0.0 {
        weight
    } else {
        0.0
    }
}

/// Fold extracted parts into the seven ranking columns.
///
/// Several parts can land in one column — every attachment's text shares the
/// `attachments` column, because a ranker has no reason to care *which*
/// attachment a term came from, only that it came from one.
fn fold(parts: &[(String, String)]) -> [String; 7] {
    let mut columns: [String; 7] = Default::default();
    for (key, text) in parts {
        let Ok(part) = Part::parse(key) else {
            // A part written by a newer build. Skipping it loses a term;
            // failing the whole index would lose the message.
            tracing::warn!(part = %key, "skipping an index part this build does not know");
            continue;
        };
        let column = match part {
            Part::Subject => 0,
            Part::Sender => 1,
            Part::Recipients => 2,
            Part::Body => 3,
            Part::Attachment(_) => 4,
            Part::Note => 5,
            Part::Summary => 6,
        };
        if !columns[column].is_empty() {
            columns[column].push(' ');
        }
        columns[column].push_str(text);
    }
    columns
}

/// Distinguish a user's typo from a server fault.
///
/// FTS5 rejects a bad query as a plain `SQLITE_ERROR`, and its wording varies
/// with the mistake — "fts5: syntax error", "unterminated string", "unknown
/// special query". Matching those strings would mean chasing a list that grows
/// with every SQLite release, so this matches the *code* instead: everything
/// else in this statement is under our control, so a generic error from it is
/// the query. Corruption and I/O have codes of their own and keep their
/// meaning, because telling a user their search was malformed when their disk
/// is failing sends them somewhere useless.
///
/// `pub(crate)` rather than private: `retrieve::lexical` runs its own SQL
/// (a hard-filter mask or a proximity bonus baked into the ranking, neither
/// of which [`FtsIndex::search`] can express) whose `MATCH` argument is built
/// the same structural way this module's is, and a bug in that construction
/// should be reported to the caller the same way — `InvalidArgument`, not a
/// generic `Internal` for one call site and a client-safe reason for the
/// other.
pub(crate) fn malformed_query(error: crate::storage::StorageError) -> Error {
    use rusqlite::ffi::ErrorCode;

    if let crate::storage::StorageError::Sqlite(rusqlite::Error::SqliteFailure(inner, _)) = &error {
        if inner.code == ErrorCode::Unknown {
            return Error::invalid_argument(format!("malformed search query: {error}"));
        }
    }
    Error::from(error)
}

#[cfg(test)]
mod tests;
