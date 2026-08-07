//! The versioned golden set: the `(query, judged-relevant message-ids)` file
//! prd.md's "Evaluation Harness & Metrics" section makes the ground truth for
//! relevance.
//!
//! # Judgments reference RFC `Message-ID`s, not row ids
//!
//! A golden set is *versioned* — committed, reviewed, and compared against
//! runs made weeks apart — which rules out `messages.id`. Row ids are
//! assigned by insertion order, so they are stable only until the mailbox is
//! re-synced from scratch, a `UIDVALIDITY` bump forces a refetch, or the
//! corpus is rebuilt on another machine. A golden set keyed on them would
//! keep parsing fine and start silently scoring the wrong mail, which is the
//! worst available failure mode for a file whose entire job is to be the
//! thing you trust.
//!
//! [`JudgedMessage::message_id`] is therefore the RFC 5322 `Message-ID`
//! header, which the sender assigns once and nothing downstream rewrites;
//! `messages.message_id` stores it verbatim and `idx_messages_message_id`
//! indexes it, so [`GoldenSet::resolve`]'s lookup is a covered index probe
//! per judgment rather than a scan.
//!
//! # An unresolvable judgment is an error, not a zero
//!
//! Resolution returns [`Resolved::unresolved`] rather than dropping misses,
//! and [`super::EvalThresholds`] fails the run on a non-empty list by
//! default. Treating a missing message as "present but irrelevant" would
//! subtract from NDCG for a corpus problem — mail not synced, an index not
//! built — and present it as a *ranking* regression. The distinction matters
//! most in exactly the situation the guard exists for: CI, where a fixture
//! that failed to seed and a ranker that got worse must not look alike.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::Error;
use crate::eval::metrics::{Judgments, MAX_GAIN};
use crate::storage::Database;

/// The only golden-set schema version this build understands.
///
/// Checked rather than ignored so an older binary refuses a newer file
/// outright instead of silently reading a subset of it and reporting metrics
/// over judgments it did not understand.
pub const SCHEMA_VERSION: u32 = 1;

/// Longest query string a golden entry may hold — matches
/// [`crate::saved_search::MAX_QUERY_LEN`] because both are "a query string a
/// human typed, persisted to disk".
pub const MAX_QUERY_LEN: usize = 4096;

/// A parsed golden-set file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoldenSet {
    /// Schema version; must equal [`SCHEMA_VERSION`].
    pub version: u32,
    /// Free-form name of the corpus these judgments were made against
    /// (`"fixture-v1"`, `"kian-personal-2026-08"`). Never interpreted —
    /// it exists so a report says which mailbox a number came from, since
    /// NDCG is meaningless across corpora.
    pub corpus: String,
    /// The judged queries.
    pub queries: Vec<GoldenQuery>,
}

/// One judged query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoldenQuery {
    /// Stable short name, unique within the file — how a per-query metric is
    /// reported and how a regression is attributed. Distinct from `query` so
    /// that rewording the query text does not break the history of the case
    /// it represents.
    pub name: String,
    /// The query string, exactly as a user would type it into `mail search`:
    /// free text, quoted phrases, and operators.
    pub query: String,
    /// Restrict to one account; `0`/absent = every account, matching
    /// `SearchRequest.account_id`.
    #[serde(default)]
    pub account_id: i64,
    /// The judged-relevant messages. At least one must carry a non-zero
    /// gain — see [`GoldenSet::validate`].
    pub judgments: Vec<JudgedMessage>,
}

/// One relevance judgment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JudgedMessage {
    /// RFC 5322 `Message-ID` header, angle brackets and all
    /// (`"<abc@example.com>"`). See the module docs.
    pub message_id: String,
    /// Relevance grade, `1..=`[`MAX_GAIN`]. Defaults to `1` so an ungraded
    /// golden set — the common case, where a message is simply "the right
    /// answer" — needs no `gain` key at all.
    ///
    /// `0` is rejected rather than read as "judged irrelevant". Nothing
    /// consumes explicit negatives (every metric here is defined over the
    /// relevant set), so the grade would be inert — and it cannot survive
    /// the wire: proto3 gives `Judgment.gain = 0` and an absent `gain` the
    /// identical encoding, so a `0` sent to `SearchService.Evaluate` is
    /// indistinguishable from an ungraded judgment and is read back as `1`.
    /// Refusing it at load is what keeps the file's meaning and the wire's
    /// from diverging; omit the judgment instead.
    #[serde(default = "default_gain")]
    pub gain: u32,
}

const fn default_gain() -> u32 {
    1
}

/// A golden set with its judgments mapped onto the corpus's own row ids.
#[derive(Debug, Clone, Default)]
pub struct Resolved {
    /// Row-id judgments, ready for [`crate::eval::metrics`].
    pub judgments: Judgments,
    /// `Message-ID`s in the golden set that no message in this corpus has.
    /// See the module docs for why these are surfaced rather than dropped.
    pub unresolved: Vec<String>,
}

impl GoldenSet {
    /// Parse a golden set from TOML text.
    ///
    /// # Errors
    /// [`Error::InvalidArgument`] on malformed TOML or on any violation
    /// [`GoldenSet::validate`] checks.
    pub fn from_toml(text: &str) -> Result<Self, Error> {
        let set: Self = toml::from_str(text)
            .map_err(|e| Error::InvalidArgument(format!("golden set is not valid TOML: {e}")))?;
        set.validate()?;
        Ok(set)
    }

    /// Read and parse a golden set from disk.
    ///
    /// # Errors
    /// [`Error::NotFound`] if `path` does not exist, [`Error::Internal`] on
    /// any other I/O failure, or the parse/validation errors of
    /// [`GoldenSet::from_toml`].
    pub fn load(path: &Path) -> Result<Self, Error> {
        let text = std::fs::read_to_string(path).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                Error::NotFound(format!("golden set {}", path.display()))
            }
            _ => Error::Internal(format!("reading golden set {}: {e}", path.display())),
        })?;
        Self::from_toml(&text)
    }

    /// Reject a golden set that cannot produce a meaningful metric.
    ///
    /// # Errors
    /// [`Error::InvalidArgument`] when the version is unsupported, the file
    /// has no queries, a name is duplicated or empty, a query string is empty
    /// or over-long, a query has no positive-gain judgment (its NDCG would be
    /// an undefined `0/0` — see [`crate::eval::metrics`]), a `Message-ID` is
    /// empty or duplicated within one query, or a gain exceeds [`MAX_GAIN`].
    pub fn validate(&self) -> Result<(), Error> {
        if self.version != SCHEMA_VERSION {
            return Err(Error::InvalidArgument(format!(
                "golden set schema version {} is not supported (this build reads version {SCHEMA_VERSION})",
                self.version
            )));
        }
        if self.corpus.trim().is_empty() {
            return Err(Error::InvalidArgument(
                "golden set must name the corpus it was judged against".to_owned(),
            ));
        }
        if self.queries.is_empty() {
            return Err(Error::InvalidArgument(
                "golden set has no queries".to_owned(),
            ));
        }

        let mut names = HashMap::new();
        for q in &self.queries {
            let name = q.name.trim();
            if name.is_empty() {
                return Err(Error::InvalidArgument(
                    "golden query has an empty name".to_owned(),
                ));
            }
            if names.insert(name.to_owned(), ()).is_some() {
                return Err(Error::InvalidArgument(format!(
                    "golden query name {name:?} appears twice"
                )));
            }
            if q.query.trim().is_empty() {
                return Err(Error::InvalidArgument(format!(
                    "golden query {name:?} has an empty query string"
                )));
            }
            if q.query.len() > MAX_QUERY_LEN {
                return Err(Error::InvalidArgument(format!(
                    "golden query {name:?} exceeds {MAX_QUERY_LEN} bytes"
                )));
            }

            let mut seen = HashMap::new();
            for j in &q.judgments {
                if j.message_id.trim().is_empty() {
                    return Err(Error::InvalidArgument(format!(
                        "golden query {name:?} has a judgment with an empty message_id"
                    )));
                }
                if j.gain == 0 {
                    return Err(Error::InvalidArgument(format!(
                        "golden query {name:?} judges {} with gain 0; omit the judgment \
                         instead — an explicit zero is inert here and cannot survive the \
                         wire (see JudgedMessage::gain)",
                        j.message_id
                    )));
                }
                if j.gain > MAX_GAIN {
                    return Err(Error::InvalidArgument(format!(
                        "golden query {name:?} judges {} with gain {} (max {MAX_GAIN})",
                        j.message_id, j.gain
                    )));
                }
                if seen.insert(j.message_id.as_str(), ()).is_some() {
                    return Err(Error::InvalidArgument(format!(
                        "golden query {name:?} judges {} twice",
                        j.message_id
                    )));
                }
            }
            if !q.judgments.iter().any(|j| j.gain > 0) {
                return Err(Error::InvalidArgument(format!(
                    "golden query {name:?} has no relevant message; NDCG would be undefined"
                )));
            }
        }
        Ok(())
    }
}

impl GoldenQuery {
    /// Map this query's `Message-ID` judgments onto `db`'s row ids.
    ///
    /// A `Message-ID` matching several rows (the same mail delivered to two
    /// accounts, or present in both `INBOX` and `Archive`) resolves to *every*
    /// matching row, each carrying the judged gain: they are the same message
    /// by any definition the user cares about, so whichever copy the pipeline
    /// surfaces is the right answer. When `account_id` is non-zero the lookup
    /// is scoped to it, so a judgment cannot be satisfied by a copy in an
    /// account the query itself excluded.
    ///
    /// # Errors
    /// A mapped storage error.
    #[tracing::instrument(skip(self, db), fields(query = %self.name), err)]
    pub async fn resolve(&self, db: &Database) -> Result<Resolved, Error> {
        let wanted: Vec<(String, u32)> = self
            .judgments
            .iter()
            .map(|j| (j.message_id.clone(), j.gain))
            .collect();
        let account_id = self.account_id;

        let resolved = db
            .read(move |c| {
                let mut judgments = Judgments::new();
                let mut unresolved = Vec::new();
                let mut by_id = c.prepare_cached(
                    "SELECT id FROM messages WHERE message_id = ?1 AND (?2 = 0 OR account_id = ?2)",
                )?;
                for (message_id, gain) in wanted {
                    let rows = by_id
                        .query_map((&message_id, account_id), |row| row.get::<_, i64>(0))?
                        .collect::<Result<Vec<_>, _>>()?;
                    if rows.is_empty() {
                        unresolved.push(message_id);
                        continue;
                    }
                    for row_id in rows {
                        judgments.insert(row_id, gain);
                    }
                }
                Ok(Resolved {
                    judgments,
                    unresolved,
                })
            })
            .await?;

        Ok(resolved)
    }
}
