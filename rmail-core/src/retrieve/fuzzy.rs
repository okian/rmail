//! The fuzzy retriever: prd.md's "typos, partial words, subject/sender/
//! contact" row, scored by [`nucleo_matcher`]'s skim/fzf-style subsequence
//! matcher.
//!
//! # Scope: subject and sender, not the whole message
//!
//! prd.md's own column for this row names exactly three fields — "subject/
//! sender/contact" — not body text: fuzzy subsequence scoring answers "does
//! this short string plausibly contain my typo-ridden query in order," which
//! is a question worth asking of a subject line or a display name and a much
//! weaker one to ask of a multi-paragraph body, where an unrelated sentence
//! can accidentally contain a query's characters in order purely by chance.
//! [`super::lexical`] and [`super::dense`] already cover full-body recall;
//! this retriever's value is specifically typo/partial-word tolerance on the
//! short, human-authored fields a user is most likely to half-remember.
//!
//! # A bounded, recency-ordered scan instead of a trigram index
//!
//! prd.md's backend column for this row is "nucleo (subsequence) + trigram
//! index." This build has no trigram-tokenized shadow table for
//! subject/sender text, and deliberately does not add one: a trigram
//! prefilter's job is narrowing a full-mailbox scan to a bounded candidate
//! set before the O(query × candidate) subsequence scorer runs — but
//! building one means a new FTS5 virtual table, a migration, *and* hooking
//! its population into task 18/19's index-write pipeline, which is indexing-
//! subsystem work this task does not own (task 28 depends on the index
//! tasks, it does not extend them). [`nucleo_matcher`] is fast enough to
//! score tens of thousands of short strings directly — prd.md's own Part III
//! perf budget for the identical algorithm over the *whole* mailbox's finder
//! entries is "< 50 ms full ranked on 100k+ entries" — so [`MAX_SCAN_ROWS`]
//! (a plain recency-ordered `LIMIT`, prioritizing the mail most likely to be
//! "the thing I'm looking for") buys the same worst-case bound a trigram
//! index would, without the index.
//!
//! # Smart-case, per prd.md
//!
//! [`CaseMatching::Smart`] matches prd.md's Part III fuzzy-match spec
//! verbatim ("case-insensitive with smart-case (any uppercase → case-
//! sensitive)"), the closest thing Part I's own fuzzy row has to a stated
//! algorithm — Part III's hand-rolled scorer is a different task's
//! (59's) larger, cross-kind finder, not duplicated here; [`nucleo_matcher`]
//! gives this retriever the same subsequence-with-bonuses behavior without
//! re-deriving its DP.
//!
//! # `Pattern::new`, not `Pattern::parse`
//!
//! `Pattern::parse` is nucleo's fzf-compatible entry point — it reads `!`/
//! `^`/`'`/`$` at atom boundaries as invert/prefix/substring/postfix
//! operators, on top of the subsequence match itself. That is the wrong
//! grammar for `plan`'s free text: `foo !bar` under `Pattern::parse` stops
//! meaning "contains foo and bar" and starts meaning "contains foo, does
//! *not* contain bar" — a second, silently different negation syntax
//! layered on top of the one [`fuzzy_query_text`] already normalized away
//! (`query::parse`'s own `-bar`, honored by filtering negated terms out
//! before they ever reach this module). [`Pattern::new`] with
//! [`AtomKind::Fuzzy`] is documented to skip that special-character
//! treatment entirely, so a literal `!`/`^`/`'`/`$` typed by a user (a
//! ticket number, a price, a contraction) fuzzy-matches as literal
//! characters instead of being read as syntax.

use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher};
use tokio_util::sync::CancellationToken;

use super::cancel::interruptible_read;
use super::filtermask::{self, FilterMask};
use super::{rank_by_score, Candidate, Source};
use crate::error::Error;
use crate::index::fts::MAX_LIMIT;
use crate::query::{Mode, QueryPlan, TermOrigin};
use crate::storage::Database;

/// Largest number of candidate rows fetched for scoring, most-recent first.
/// See the module docs on why this — not a trigram index — is this build's
/// bound on the O(query × candidate) subsequence pass.
const MAX_SCAN_ROWS: i64 = 5_000;

/// One row's searchable text and the message it came from.
struct Haystack {
    message_id: i64,
    text: String,
}

impl AsRef<str> for Haystack {
    fn as_ref(&self) -> &str {
        &self.text
    }
}

/// nucleo subsequence retrieval over subject/sender fields.
#[derive(Debug, Clone)]
pub struct FuzzyRetriever {
    db: Database,
}

impl FuzzyRetriever {
    /// Build a retriever over `db`.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Retrieve up to `limit` messages whose subject/sender fuzzy-match
    /// `plan`'s free text, best match first.
    ///
    /// Returns an empty list, not an error, when `plan` has no free text to
    /// match against (a pure filter query — nothing here for a subsequence
    /// scorer to do) or a hard filter provably excludes every message.
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
        let Some(query_text) = fuzzy_query_text(plan) else {
            return Ok(Vec::new());
        };
        let (where_sql, mask_params) = match filtermask::compile(&plan.hard_filters) {
            FilterMask::ExcludesEverything => return Ok(Vec::new()),
            FilterMask::Unconstrained => (String::new(), Vec::new()),
            FilterMask::Sql(mask) => (format!(" AND {}", mask.sql), mask.params),
        };

        let page = clamp_limit(limit);
        let sql = format!(
            "SELECT id, COALESCE(subject, ''), COALESCE(from_name, ''), COALESCE(from_addr, '') \
             FROM messages WHERE 1=1{where_sql} \
             ORDER BY COALESCE(date, internaldate) DESC LIMIT ?"
        );
        let scan_cap = MAX_SCAN_ROWS;

        let scored = interruptible_read(&self.db, cancel, move |conn| {
            let mut stmt = conn.prepare(&sql)?;
            let mut bound: Vec<&dyn rusqlite::ToSql> = mask_params.iter().map(|p| p as _).collect();
            bound.push(&scan_cap);
            let haystacks = stmt
                .query_map(bound.as_slice(), |row| {
                    let subject: String = row.get(1)?;
                    let from_name: String = row.get(2)?;
                    let from_addr: String = row.get(3)?;
                    Ok(Haystack {
                        message_id: row.get(0)?,
                        text: format!("{subject} {from_name} {from_addr}"),
                    })
                })?
                .collect::<rusqlite::Result<Vec<Haystack>>>()?;
            if haystacks.len() as i64 == scan_cap {
                // The fetch exactly filled `MAX_SCAN_ROWS`, meaning there may
                // be older mail this pass never got to scan at all — the
                // recall tradeoff the module docs describe, made observable
                // rather than a silent miss.
                tracing::debug!(
                    scan_cap,
                    "fuzzy candidate scan hit its row cap; older mail may not have been considered"
                );
            }

            // CPU-bound scoring, kept inside the same blocking unit as the
            // fetch above: it runs on a bounded, already-fetched set (at most
            // `MAX_SCAN_ROWS` short strings), so it does not need its own
            // cancellation checkpoint the way an unbounded SQL scan does —
            // the fetch above is the part of this retriever's cost that
            // needed `interruptible_read`'s interrupt, not this.
            let mut matcher = Matcher::new(Config::DEFAULT);
            // See the module docs: `Pattern::new` (not `::parse`) so nucleo's
            // fzf operator syntax never reinterprets what `query::parse`
            // already decided this text means.
            let pattern = Pattern::new(
                &query_text,
                CaseMatching::Smart,
                Normalization::Smart,
                AtomKind::Fuzzy,
            );
            let mut matched = pattern.match_list(haystacks, &mut matcher);
            matched.truncate(usize::try_from(page).unwrap_or(usize::MAX));
            Ok(matched
                .into_iter()
                .map(|(haystack, score)| (haystack.message_id, f64::from(score)))
                .collect::<Vec<(i64, f64)>>())
        })
        .await?;

        let Some(scored) = scored else {
            tracing::debug!("scan cancelled; superseded by a newer query");
            return Ok(Vec::new());
        };

        let candidates = rank_by_score(Source::Fuzzy, scored);
        tracing::Span::current().record("hits", candidates.len());
        Ok(candidates)
    }
}

/// The free text to fuzzy-match: original (not spell-fixed/expanded), non-
/// negated terms plus non-negated phrases, joined with spaces — mirrors
/// `QueryPlanner::embed_query`'s own construction of "what the user actually
/// typed as prose" for the same reason: a correction or a PMI synonym is
/// evidence *about* the query, not itself something a subsequence scorer
/// should treat as typed input. `None` when there is nothing eligible (a
/// pure filter query, every term `~`-forced-semantic/negated, or — since
/// nucleo's `match_list` scores *every* candidate `0` for an empty pattern —
/// text with no alphanumeric character at all, which would otherwise hand
/// back `limit` recent messages mislabeled as ranked fuzzy hits).
fn fuzzy_query_text(plan: &QueryPlan) -> Option<String> {
    let mut parts: Vec<&str> = plan
        .lexical_terms
        .iter()
        .filter(|term| {
            matches!(term.origin, TermOrigin::Original)
                && !term.negated
                && term.mode != Mode::Semantic
        })
        .map(|term| term.text.as_str())
        .collect();
    parts.extend(
        plan.phrases
            .iter()
            .filter(|phrase| !phrase.negated && phrase.mode != Mode::Semantic)
            .map(|phrase| phrase.text.as_str()),
    );
    let joined = parts.join(" ");
    (joined.chars().any(char::is_alphanumeric)).then_some(joined)
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
