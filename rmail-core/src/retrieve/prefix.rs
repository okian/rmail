//! The prefix/autocomplete retriever: prd.md's "as-you-type, incremental"
//! row, over `fts_messages` with FTS5's own `token*` prefix query.
//!
//! # A different question than `super::lexical` asks
//!
//! [`super::lexical`] matches complete terms — "invoice" matches documents
//! containing the whole word "invoice". A user who has typed "inv" and is
//! still typing has not finished a word lexical.rs can match at all; FTS5's
//! `"inv"*` prefix syntax is the operator built for exactly that, matching
//! any indexed term with "inv" as a prefix ("invoice", "investor",
//! "invitation", ...). Running that query for every free-text term rather
//! than only the last one typed is a deliberate choice: this retriever has no
//! way to know which term in a multi-word query is "still being typed" (the
//! caller does, from cursor position, but a `QueryPlan` carries none), so it
//! treats every eligible term as a possible in-progress word rather than
//! guessing.
//!
//! # `finder_index` is a different subsystem
//!
//! prd.md's backend column for this row lists `FTS5 prefix + finder_index`.
//! `finder_index` (task 59, Part III) is the denormalized, in-memory,
//! cross-kind — messages, mailboxes, contacts, tags, saved searches, commands
//! — table behind the `Ctrl-P` fuzzy finder, built and maintained by a change
//! feed that does not exist yet in this build (task 28 depends on 19/21/26/27,
//! not 59). This retriever is scoped to what it can do today: FTS5 prefix
//! matching over messages only, the half of that backend list this task's
//! index tasks actually provide.

use super::cancel::interruptible_read;
use super::filtermask::{self, FilterMask};
use super::{rank_by_score, Candidate, Source};
use crate::error::Error;
use crate::index::fts::{self, FtsIndex, MAX_LIMIT};
use crate::query::{Mode, QueryPlan, TermOrigin};
use crate::storage::Database;
use tokio_util::sync::CancellationToken;

/// Shortest free-text term this retriever will prefix-match. Below this, a
/// prefix query matches a large fraction of the vocabulary and stops being a
/// useful "as-you-type" signal — the same reasoning most autocomplete UIs use
/// to gate on a minimum keystroke count before suggesting anything.
const MIN_PREFIX_TERM_LEN: usize = 2;

/// FTS5 prefix retrieval, field-weighted the same way [`super::lexical`] is.
#[derive(Debug, Clone)]
pub struct PrefixRetriever {
    fts: FtsIndex,
    db: Database,
}

impl PrefixRetriever {
    /// Build a retriever sharing `fts`'s field weights and `db`'s hard-filter
    /// mask machinery.
    #[must_use]
    pub fn new(fts: FtsIndex, db: Database) -> Self {
        Self { fts, db }
    }

    /// Retrieve up to `limit` messages whose subject/sender/body/... contains
    /// a term with one of `plan`'s eligible free-text terms as a prefix.
    ///
    /// Returns an empty list, not an error, when there is nothing eligible to
    /// prefix-match (a pure filter query, or every term too short/negated/
    /// forced-semantic — see [`build_prefix_match`]) or when a hard filter
    /// provably excludes every message.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    #[tracing::instrument(skip(self, plan, cancel), fields(hits))]
    pub async fn retrieve(
        &self,
        plan: &QueryPlan,
        limit: i64,
        cancel: &CancellationToken,
    ) -> Result<Vec<Candidate>, Error> {
        let Some(match_expr) = build_prefix_match(plan) else {
            return Ok(Vec::new());
        };
        let mask = filtermask::compile(&plan.hard_filters);
        if matches!(mask, FilterMask::ExcludesEverything) {
            return Ok(Vec::new());
        }

        let page = clamp_limit(limit);
        let weights = self.fts.weight_list();
        let mut sql = format!(
            "SELECT rowid, bm25(fts_messages, {weights}) AS score FROM fts_messages \
             WHERE fts_messages MATCH ?"
        );
        let mask_params = if let FilterMask::Sql(mask) = &mask {
            sql.push_str(&format!(
                " AND {}",
                mask.exists_clause("fts_messages.rowid")
            ));
            mask.params.clone()
        } else {
            Vec::new()
        };
        sql.push_str(" ORDER BY score LIMIT ?");

        let rows = interruptible_read(&self.db, cancel, move |conn| {
            let mut stmt = conn.prepare(&sql)?;
            let mut bound: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(mask_params.len() + 2);
            bound.push(&match_expr);
            for value in &mask_params {
                bound.push(value);
            }
            bound.push(&page);
            let rows = stmt
                .query_map(bound.as_slice(), |row| {
                    // Same bm25() sign flip as `FtsIndex::search`.
                    Ok((row.get::<_, i64>(0)?, -row.get::<_, f64>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<(i64, f64)>>>()?;
            Ok(rows)
        })
        .await
        .map_err(fts::malformed_query)?;

        let Some(rows) = rows else {
            tracing::debug!("scan cancelled; superseded by a newer query");
            return Ok(Vec::new());
        };

        let candidates = rank_by_score(Source::Prefix, rows);
        tracing::Span::current().record("hits", candidates.len());
        Ok(candidates)
    }
}

/// Build an FTS5 `MATCH` expression OR-ing a prefix query over every eligible
/// free-text term, or `None` if nothing qualifies.
///
/// Eligible: the term as the user actually typed it ([`TermOrigin::Original`]
/// — a spell-corrected or PMI-expanded term is not what is "still being
/// typed"), not negated (excluding-by-prefix has no autocomplete meaning),
/// not `~`-forced-semantic (the same bypass [`super::lexical`] honors), and
/// at least [`MIN_PREFIX_TERM_LEN`] indexable characters long.
fn build_prefix_match(plan: &QueryPlan) -> Option<String> {
    let terms: Vec<String> = plan
        .lexical_terms
        .iter()
        .filter(|term| {
            matches!(term.origin, TermOrigin::Original)
                && !term.negated
                && term.mode != Mode::Semantic
                && term.text.chars().count() >= MIN_PREFIX_TERM_LEN
                && term.text.chars().any(char::is_alphanumeric)
        })
        .map(|term| quote_fts_prefix(&term.text))
        .collect();
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" OR "))
    }
}

/// Wrap `text` as an FTS5 quoted-string token-prefix query (`"text"*`) — the
/// same quoting `retrieve::lexical::quote_fts_literal` uses (doubling an
/// embedded `"`) so a term cannot restructure the query as FTS5 syntax,
/// followed immediately by the prefix operator outside the quotes (no
/// whitespace: FTS5's grammar allows `*` only directly after the string it
/// modifies).
fn quote_fts_prefix(text: &str) -> String {
    format!("\"{}\"*", text.replace('"', "\"\""))
}

fn clamp_limit(limit: i64) -> i64 {
    if limit <= 0 {
        MAX_LIMIT
    } else {
        limit.min(MAX_LIMIT)
    }
}

#[cfg(test)]
mod tests;
