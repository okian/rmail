//! The lexical retriever: field-weighted BM25 over `fts_messages`, gated by
//! the query's hard filters (prd.md, Stage 1's "Lexical BM25" row).
//!
//! # A retriever, not a search box
//!
//! [`index::fts::FtsIndex`](crate::index::fts::FtsIndex) already does field
//! weighting and BM25 scoring — this module's job is everything a raw
//! `MATCH` string cannot express on its own: turning a [`ParsedQuery`]'s
//! already-classified terms, phrases, and filters into that string
//! *structurally* rather than by concatenating user text, applying the
//! query's hard filters as a candidate mask baked into the same SQL (so a
//! masked top-N is actually the top N that pass the mask, not N unmasked
//! results filtered down to fewer), and adding the proximity bonus an
//! unquoted multi-term query earns when its words land close together.
//!
//! # Building the `MATCH` expression is the injection boundary
//!
//! A raw FTS5 query is a small language — `AND`/`OR`/`NOT`, `NEAR(...)`,
//! `col:`, `*` prefixes, parentheses — and [`ParsedQuery`]'s terms and
//! phrases are user text that has not been through that language's parser.
//! Concatenating a term like `NEAR(x,1)` or a bare `OR` straight into a
//! `MATCH` string would let it restructure the query the same way
//! concatenating a value into a SQL string lets it restructure a statement.
//! [`quote_fts_literal`] is the fix: every term and phrase is wrapped in an
//! FTS5 quoted string (doubling any embedded `"`, the language's own escape),
//! which removes it from consideration as syntax entirely — a quoted
//! string's *content* is still tokenized for matching, but the query
//! *parser* never looks inside it for `AND`/`OR`/`NOT`/`NEAR`/parentheses.
//! `metacharacters_cannot_change_the_query_shape` in the tests proves this
//! against the real FTS5 engine, not just against the string this module
//! builds. Quoting is not a syntax-error firewall on its own, though: FTS5's
//! own string lexer chokes on an embedded NUL the same way it would outside
//! a quoted literal, so that still surfaces as a query error — just never as
//! a *restructured* one, and [`crate::index::fts::malformed_query`] is what
//! keeps that error `InvalidArgument` (the user's typo) rather than
//! `Internal` (this module's fault) on every path through this file.
//!
//! # The hard-filter mask and the proximity bonus are both applied inside
//! # the ranked query, never after it
//!
//! Filtering a bm25-ranked page of N results down to the ones that also pass
//! `from:`/`is:`/... can return fewer than N candidates even when N exist —
//! or drop a candidate ranked N+1 that would have made the cut with the
//! non-passing ones removed first. The same failure mode applies to the
//! proximity bonus: boosting a score *after* the page is already cut can
//! never promote a candidate that fell just outside the unboosted top-N.
//! So both are compiled into the single query [`LexicalRetriever::search_ranked`]
//! runs: a hard filter becomes a SQL predicate ANDed (via a correlated
//! `EXISTS`, so the query planner can use `messages`' primary-key index
//! instead of a full scan) into the same row set the `MATCH` and `ORDER BY`
//! see, and the proximity bonus is a `CASE` multiplier inside the same
//! `ORDER BY` expression rather than a second pass over an already-fetched
//! page. Every filter value reaches SQL as a bound parameter
//! ([`rusqlite::types::Value`]), never interpolated — the injection concern
//! here is only ever the `MATCH` string, never the filter values.
//!
//! # Negation is NULL-safe
//!
//! A hard filter's SQL predicate can be `NULL` for a row where the column it
//! reads is `NULL` (no Cc header, no thread yet, no known size) — sixteen of
//! `messages`' columns are nullable. Un-negated that is harmless (`WHERE
//! NULL` just doesn't select the row, same as `WHERE FALSE`), but SQL's
//! three-valued logic makes `NOT NULL` evaluate to `NULL` too, not `TRUE` —
//! so a naive `NOT (predicate)` would silently *exclude* every row with a
//! `NULL` in the relevant column from a negated filter, which is backwards:
//! "not from alice" should include a message with no `From` header, not drop
//! it. [`build_negated_clause`] wraps every predicate in `COALESCE(_, 0)`
//! before negating so an unknown answer reads as "doesn't match" and its
//! negation reads as "matches" — see `negating_a_filter_does_not_drop...`
//! in the tests for the row this fixes.
//!
//! # What a hard filter does when this build cannot evaluate it
//!
//! Not every [`Operator`] the parser recognizes is backed by a table today —
//! `tag:`, `note:`, `ai:`, and `has:note`/`has:tag` name subsystems that land
//! in later tasks (55, 56, 57). Silently ignoring one of those would let
//! `tag:work invoice` return every `invoice` hit regardless of tag, which
//! breaks the "hard filters gate everything" contract just as badly as
//! misapplying a filter that *is* backed. But the honest answer is not
//! "unknown" either: with no tags table, *zero* messages currently have any
//! tag, so `tag:work` provably excludes everything, and this module makes it
//! do exactly that (see [`RawEffect::Never`]) rather than pretend the
//! constraint does not exist. Negating one of these
//! (`-tag:newsletter`) inverts correctly for the same reason: "not tagged
//! newsletter" is true of every message when nothing is tagged anything, so
//! it degrades to no constraint instead of excluding everything.
//!
//! `is:` is a partial exception: `pinned`/`muted` name concepts with no
//! backing data at all (same as `tag:`/`note:`), but a value outside the six
//! documented flags is not automatically unbacked — `flags` is a general
//! IMAP keyword table, and [`crate::message::fetch::flag_to_string`] already
//! persists `\Draft`, `\Deleted`, `\Recent`, and arbitrary custom keywords
//! verbatim. [`is_other_flag`] routes those through the same `flags` lookup
//! `is:unread`/`is:read`/... use, rather than lumping every undocumented
//! value into "no data" and silently returning nothing for `is:draft`.
//!
//! A relative date (`before:last-week`) is a different situation again:
//! `date`/`internaldate` are real columns, but resolving "last-week" to a
//! timestamp is the corpus-aware NL date grammar prd.md assigns to a later
//! Stage 0 step, not this one. Here that is genuine uncertainty rather than
//! a provable "never matches," so an unresolvable date value is skipped
//! (logged, not silently swallowed) rather than guessed at in either
//! direction — see [`RawEffect::Unknown`].

use rusqlite::types::Value;
use rusqlite::ToSql;
use tokio_util::sync::CancellationToken;

use super::cancel::interruptible_read;
use super::filtermask::ai_predicate_sql;
use super::{rank_by_score, Candidate, Source};
use crate::error::Error;
use crate::index::fts::{self, FtsIndex};
use crate::query::{Filter, HasTarget, IsFlag, Mode, Operator, ParsedQuery};
use crate::storage::Database;

/// Multiplier applied to a candidate's score when its unquoted terms also
/// satisfy the proximity probe (see [`MatchExpr::proximity`]).
///
/// A multiplier rather than an additive bonus: every score this module hands
/// out is BM25-derived and non-negative (`index::fts`'s "BM25 signs" note —
/// a corpus where every configured weight has been clamped to `0.0` by
/// [`FtsIndex`]'s own sanity check is the one case where the bonus becomes a
/// no-op, since `0.0 * 1.2` is still `0.0`, which is the correct degenerate
/// behavior: there is no relevance signal left to amplify), so multiplying
/// scales the bonus to a candidate's existing relevance instead of needing a
/// constant tuned to the corpus's absolute BM25 magnitude. `1.2` is a
/// deliberately modest lift — enough to break a near-tie in favor of the
/// tighter match (prd.md: "an unquoted multi-term query still earns a
/// proximity bonus when terms appear close together"), not enough for a weak
/// proximity match to leapfrog a strongly-relevant non-proximate one.
const PROXIMITY_BONUS: f64 = 1.2;

/// Maximum token distance between terms for the unquoted-proximity probe.
///
/// Equal to FTS5's own default `NEAR` distance (used when a caller omits the
/// second `NEAR()` argument) — spelled out explicitly so the window is
/// documented here rather than left as an implicit default a reader has to
/// already know.
const PROXIMITY_WINDOW: u32 = 10;

/// Seconds in a day, for turning an inclusive calendar-day date filter into a
/// half-open `[start, end)` timestamp range.
const SECONDS_PER_DAY: i64 = 86_400;

/// Field-weighted BM25 retrieval over `fts_messages`, gated by hard filters.
///
/// Cheap to clone: both fields share a database handle.
#[derive(Debug, Clone)]
pub struct LexicalRetriever {
    fts: FtsIndex,
    db: Database,
}

impl LexicalRetriever {
    /// Build a retriever over an already-open lexical index.
    ///
    /// Takes `db` separately from `fts` (rather than reaching into
    /// [`FtsIndex`] for one) because the hard-filter mask queries `messages`/
    /// `flags`/`attachments`/`mailboxes`/`accounts` directly — tables
    /// [`FtsIndex`] has no reason to know about.
    #[must_use]
    pub fn new(fts: FtsIndex, db: Database) -> Self {
        Self { fts, db }
    }

    /// Retrieve up to `limit` messages, best match first, with a
    /// source-local BM25 score and 1-based rank.
    ///
    /// `limit <= 0` means "the server default" and is clamped to
    /// [`fts::MAX_LIMIT`], mirroring [`FtsIndex::search`]'s own contract so a
    /// caller sees one limit convention across the lexical surface.
    ///
    /// Returns an empty list, not an error, when the query has nothing this
    /// retriever can rank: no free-text terms/phrases at all (or none left
    /// once punctuation/emoji-only tokens and `~`-forced-semantic ones are
    /// set aside — see [`MatchExpr::build`]), or a hard filter that provably
    /// excludes every message (see the module docs). Also returns an empty
    /// list — rather than a partial or stale one — when `cancel` fires before
    /// the scan completes: a cancelled read means a newer query superseded
    /// this one, not a fault (see [`super::cancel`]).
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] if the built `MATCH` expression is not
    /// valid FTS5 syntax after quoting (an embedded NUL byte is the only
    /// known way user text can still do this — see the module docs).
    /// Otherwise a mapped storage error.
    #[tracing::instrument(
        skip(self, query, cancel),
        fields(
            terms = query.terms.len(),
            phrases = query.phrases.len(),
            filters = query.filters.len(),
            page,
            masked,
            proximity,
            hits
        )
    )]
    pub async fn retrieve(
        &self,
        query: &ParsedQuery,
        limit: i64,
        cancel: &CancellationToken,
    ) -> Result<Vec<Candidate>, Error> {
        let mask = match compile_filters(&query.filters) {
            FilterMask::ExcludesEverything => {
                tracing::debug!("a hard filter provably excludes every message; skipping the scan");
                return Ok(Vec::new());
            }
            FilterMask::Unconstrained => None,
            FilterMask::Sql(mask) => Some(mask),
        };
        let Some(expr) = MatchExpr::build(query) else {
            return Ok(Vec::new());
        };
        let page = clamp_limit(limit);

        let span = tracing::Span::current();
        span.record("page", page);
        span.record("masked", mask.is_some());
        span.record("proximity", expr.proximity.is_some());

        // Always routed through `search_ranked` — including the common case
        // `FtsIndex::search` alone could serve — rather than branching
        // between the two: `search_ranked` produces byte-identical SQL to
        // `FtsIndex::search` when there is no mask and no proximity probe
        // (same `MATCH`/`ORDER BY bm25(...)`/`LIMIT`, just built locally
        // instead of delegated), and unlike `FtsIndex::search` it runs
        // through `cancel::interruptible_read`. Lexical is the one retriever
        // every query runs regardless of intent — task 28's "a query-
        // generation token cancels superseded scans" would be true for six
        // sources and silently false for the busiest one if this path kept
        // going through `Database::read` instead.
        let scored = self
            .search_ranked(&expr, mask.as_ref(), page, cancel)
            .await?;

        let candidates = rank_by_score(Source::Lexical, scored);
        span.record("hits", candidates.len());
        Ok(candidates)
    }

    /// Run `expr` — optionally gated by `mask`, optionally boosted by
    /// `expr.proximity` — and return up to `limit` `(message_id, score)`
    /// pairs, best first, honoring `cancel`.
    #[tracing::instrument(skip(self, expr, mask, cancel))]
    async fn search_ranked(
        &self,
        expr: &MatchExpr,
        mask: Option<&Mask>,
        limit: i64,
        cancel: &CancellationToken,
    ) -> Result<Vec<(i64, f64)>, Error> {
        let weights = self.fts.weight_list();
        // `bm25()` is negative-is-better (see `index::fts`'s "BM25 signs"
        // note), so multiplying by a bonus > 1 makes a boosted row *more*
        // negative — sorting it earlier under a plain ascending `ORDER BY`,
        // with no sign gymnastics needed here. The `?` for the probe sits
        // inside the `SELECT` list, which is why it is bound *first* below,
        // ahead of the main `MATCH` argument that follows it in the SQL
        // text.
        let score_expr = if expr.proximity.is_some() {
            format!(
                "bm25(fts_messages, {weights}) * \
                 (CASE WHEN rowid IN (SELECT rowid FROM fts_messages WHERE fts_messages MATCH ?) \
                       THEN {PROXIMITY_BONUS} ELSE 1.0 END)"
            )
        } else {
            format!("bm25(fts_messages, {weights})")
        };
        // Aliased so `ORDER BY score` doesn't have to repeat (and re-bind)
        // the `CASE` expression a second time.
        let mut sql = format!(
            "SELECT rowid, {score_expr} AS score FROM fts_messages WHERE fts_messages MATCH ?"
        );
        if let Some(mask) = mask {
            // A correlated `EXISTS` rather than `rowid IN (SELECT id FROM
            // messages WHERE ...)`: the latter's subquery is planned
            // independently of the `MATCH`, so it full-scans `messages` on
            // every masked search regardless of how few rows the `MATCH`
            // itself returns. `EXISTS`, correlated on `fts_messages.rowid`,
            // lets the planner use `messages`' `INTEGER PRIMARY KEY` and run
            // it once per `MATCH` hit instead.
            sql.push_str(&format!(
                " AND EXISTS (SELECT 1 FROM messages WHERE messages.id = fts_messages.rowid AND {})",
                mask.sql
            ));
        }
        sql.push_str(" ORDER BY score LIMIT ?");

        let proximity = expr.proximity.clone();
        let full = expr.full.clone();
        let mask_params = mask.map(|m| m.params.clone()).unwrap_or_default();
        let hits = interruptible_read(&self.db, cancel, move |conn| {
            let mut stmt = conn.prepare(&sql)?;
            let mut params: Vec<&dyn ToSql> = Vec::with_capacity(mask_params.len() + 3);
            // Binding order must match the `?` placeholders' order of
            // *appearance in the SQL text*, not any semantic ordering:
            // the proximity probe's `?` (if present) is textually first
            // (inside the `SELECT` list), then the main `MATCH`
            // argument, then the mask's own parameters (inside the
            // `EXISTS` subquery), then `LIMIT`.
            if let Some(near) = &proximity {
                params.push(near);
            }
            params.push(&full);
            for value in &mask_params {
                params.push(value);
            }
            params.push(&limit);
            let rows = stmt
                .query_map(params.as_slice(), |row| {
                    // Same sign flip as `FtsIndex::search`: bm25() is
                    // negative-is-better, every score leaving this module
                    // is higher-is-better.
                    Ok((row.get::<_, i64>(0)?, -row.get::<_, f64>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
        .map_err(fts::malformed_query)?;
        Ok(hits.unwrap_or_default())
    }
}

/// `limit <= 0` means "server default"; otherwise capped at
/// [`fts::MAX_LIMIT`] — identical clamping to [`FtsIndex::search`] so the two
/// share one limit contract.
fn clamp_limit(limit: i64) -> i64 {
    if limit <= 0 {
        fts::MAX_LIMIT
    } else {
        limit.min(fts::MAX_LIMIT)
    }
}

/// The `MATCH` expression built from a [`ParsedQuery`]'s terms and phrases.
struct MatchExpr {
    /// Required terms/phrases ANDed together, with negated ones excluded via
    /// `NOT (...)`. The expression the ranked search actually runs.
    full: String,
    /// `NEAR(...)` over the bare (unquoted) non-negated terms, present only
    /// when there are at least two of them — with fewer than two there is
    /// nothing to be "near". Phrases are excluded: a quoted phrase already
    /// gets exact adjacency from being one FTS5 phrase literal, so it needs
    /// no separate proximity probe. This is evaluated as an *additional*
    /// condition on rows the main query already matched (see
    /// [`LexicalRetriever::search_ranked`]), so it never needs to repeat the
    /// full required expression itself.
    proximity: Option<String>,
}

impl MatchExpr {
    /// Build the match expression, or `None` if the query has no free-text
    /// terms/phrases eligible for lexical matching — pure hard-filter
    /// queries, queries where every term is `~`-forced semantic, and queries
    /// where every term/phrase is punctuation/emoji with no token the
    /// `unicode61` tokenizer would ever produce (see
    /// [`has_indexable_content`]) all land here, and the module docs cover
    /// why that means "nothing to rank" and not an error.
    fn build(query: &ParsedQuery) -> Option<Self> {
        let mut required = Vec::new();
        let mut excluded = Vec::new();
        let mut bare = Vec::new();

        for term in &query.terms {
            // `~` means "bypass exact lexical matching" for this token (see
            // `query::parse::Mode`'s doc comment) — this retriever has
            // nothing to do with it either way, negated or not.
            if term.mode == Mode::Semantic || !has_indexable_content(&term.text) {
                continue;
            }
            let literal = quote_fts_literal(&term.text);
            if term.negated {
                excluded.push(literal);
            } else {
                bare.push(literal.clone());
                required.push(literal);
            }
        }
        for phrase in &query.phrases {
            if phrase.mode == Mode::Semantic || !has_indexable_content(&phrase.text) {
                continue;
            }
            let literal = quote_fts_literal(&phrase.text);
            if phrase.negated {
                excluded.push(literal);
            } else {
                required.push(literal);
            }
        }

        if required.is_empty() {
            return None;
        }

        let positive = required.join(" AND ");
        let full = if excluded.is_empty() {
            positive
        } else {
            format!("({positive}) NOT ({})", excluded.join(" OR "))
        };
        let proximity =
            (bare.len() >= 2).then(|| format!("NEAR({}, {PROXIMITY_WINDOW})", bare.join(" ")));

        Some(Self { full, proximity })
    }
}

/// Whether `text` contains at least one alphanumeric character — a cheap
/// proxy for "the `unicode61` tokenizer will produce at least one token from
/// this."
///
/// A term/phrase that fails this check would, quoted, match zero documents
/// ever; ANDing it into [`MatchExpr::build`]'s `required` list would zero out
/// the *entire* query for a reason no user typing `budget 🎉` or `report -`
/// intended (a lone `-` survives `query::parse` as literal text — see its
/// "bare modifier" rule — rather than being dropped as empty).
pub(crate) fn has_indexable_content(text: &str) -> bool {
    text.chars().any(char::is_alphanumeric)
}

/// Wrap `text` as a single FTS5 quoted-string literal, doubling any embedded
/// `"` (FTS5's own escape for a literal quote inside a quoted string).
///
/// This is the entire injection defense: once `text` is inside FTS5 quotes,
/// the query *parser* stops looking at it for `AND`/`OR`/`NOT`/`NEAR`/
/// parentheses/`*` — those only mean something outside a quoted string. The
/// content is still run through the table's tokenizer for matching (so
/// `foo"bar` becomes an adjacent two-token phrase match on `foo` and `bar`,
/// since the embedded quote is not a word character to `unicode61`), but it
/// can never restructure the boolean query the way concatenating it
/// unescaped would.
///
/// `pub(crate)` rather than private: `features::extract` builds its own,
/// narrower required-terms `MATCH` expression (no proximity probe — see that
/// module's docs) for the per-field `bm25()` breakdown prd.md's Stage 3
/// wants, and reuses this exact quoting rather than re-deriving the same
/// injection defense a second time.
pub(crate) fn quote_fts_literal(text: &str) -> String {
    format!("\"{}\"", text.replace('"', "\"\""))
}

/// A hard-filter mask compiled to SQL: a boolean expression over `messages`
/// (and, via `EXISTS`/`IN` subqueries, `flags`/`attachments`/`mailboxes`/
/// `accounts`), plus the parameters it binds.
///
/// `sql` is safe to embed verbatim in a query — it is built only from fixed,
/// trusted column/table names this module wrote, never from filter *values*,
/// which always travel through `params` as bound parameters.
struct Mask {
    sql: String,
    params: Vec<Value>,
}

/// The result of compiling a query's hard filters (see [`compile_filters`]).
enum FilterMask {
    /// No filters, or none of them contribute a constraint (every one
    /// present was a negated [`RawEffect::Never`] or an
    /// [`RawEffect::Unknown`]).
    Unconstrained,
    /// At least one filter's positive form provably matches nothing, so —
    /// filters conjoin — the whole query matches nothing regardless of what
    /// the free-text terms would otherwise find.
    ExcludesEverything,
    /// A real SQL predicate.
    Sql(Mask),
}

/// What evaluating one hard filter's *positive* form (ignoring its `negated`
/// flag) resolves to, before [`compile_filters`] applies negation.
enum RawEffect {
    /// A real SQL predicate over `messages`, invertible with `NOT (...)`.
    Sql(String, Vec<Value>),
    /// The positive form is false for every message today — either the value
    /// cannot denote any real row (a `thread:` id that is not an integer,
    /// which no `threads.id` can ever equal; an `ai:` key/value
    /// [`ai_predicate_sql`] does not recognize) or the operator names a
    /// subsystem this build has no table for yet (`tag:`, `note:`,
    /// `has:note`, `has:tag`, `is:pinned`, `is:muted`). See the module docs
    /// for why this degrades to "excludes everything" rather than "no
    /// constraint".
    Never,
    /// The operator is backed by real columns, but this stage cannot resolve
    /// *this* value with confidence in either direction (a `before:`/
    /// `after:`/`on:`/`date:` value that is not a plain ISO date — relative
    /// date resolution is a later Stage 0 step). Contributes no constraint,
    /// logged rather than silently dropped.
    Unknown,
}

/// Compile `filters` into a [`FilterMask`] in one pass: [`classify`] runs
/// exactly once per filter (rather than once to check for a doomed query and
/// again to build the mask), and a positive [`RawEffect::Never`] short-
/// circuits immediately — a query already known to match nothing does not
/// need its remaining filters classified at all, let alone reach the
/// database.
fn compile_filters(filters: &[Filter]) -> FilterMask {
    let mut clauses = Vec::new();
    let mut params = Vec::new();

    for filter in filters {
        match classify(&filter.op) {
            RawEffect::Sql(sql, sql_params) => {
                clauses.push(if filter.negated {
                    build_negated_clause(&sql)
                } else {
                    format!("({sql})")
                });
                params.extend(sql_params);
            }
            RawEffect::Never if !filter.negated => return FilterMask::ExcludesEverything,
            RawEffect::Never => {
                tracing::debug!(
                    operator = operator_kind(&filter.op),
                    "negation of an operator with no backing data yet is vacuously true; \
                     contributes no constraint"
                );
            }
            RawEffect::Unknown => {
                tracing::debug!(
                    operator = operator_kind(&filter.op),
                    "operator value cannot be resolved at this stage; not applied as a gate"
                );
            }
        }
    }

    if clauses.is_empty() {
        FilterMask::Unconstrained
    } else {
        FilterMask::Sql(Mask {
            sql: clauses.join(" AND "),
            params,
        })
    }
}

/// Negate `sql` NULL-safely: `NOT (sql)` is `NULL`, not `TRUE`, whenever
/// `sql` itself evaluates to `NULL` (a column the predicate reads is `NULL`
/// for this row) — and a `NULL` in a `WHERE`/`AND` context excludes the row
/// just as surely as `FALSE` would. That is backwards for a negated filter:
/// `-cc:legal` must *include* a message with no Cc header, not drop it for
/// having an unanswerable predicate. Wrapping in `COALESCE(_, 0)` first
/// resolves the unknown to "does not match" *before* negating, so its
/// negation correctly reads as "matches". Predicates built from `EXISTS`
/// (flags, attachments) never evaluate to `NULL` in the first place, so this
/// is a no-op for them and only changes behavior for the column-comparison
/// predicates (`from:`/`to:`/`cc:`/`subject:`/`body:`/`larger:`/`smaller:`/
/// `before:`/`after:`/`on:`/`date:`/`thread:`) where it matters.
fn build_negated_clause(sql: &str) -> String {
    format!("NOT COALESCE(({sql}), 0)")
}

/// A short, value-free label for `op`'s operator kind, safe to log.
///
/// `retrieve`'s `#[tracing::instrument(skip(query))]` already keeps a
/// query's terms/phrases out of traces; filter *values* deserve the same
/// treatment (a `note:` or `from:` filter's value can be mailbox content —
/// someone's address, a note someone wrote), even though naming *which
/// operator* degraded is genuinely useful for debugging a search that
/// returned nothing.
fn operator_kind(op: &Operator) -> &'static str {
    match op {
        Operator::From(_) => "from",
        Operator::To(_) => "to",
        Operator::Cc(_) => "cc",
        Operator::Subject(_) => "subject",
        Operator::Body(_) => "body",
        Operator::Has(_) => "has",
        Operator::Filename(_) => "filename",
        Operator::Larger(_) => "larger",
        Operator::Smaller(_) => "smaller",
        Operator::Before(_) => "before",
        Operator::After(_) => "after",
        Operator::On(_) => "on",
        Operator::DateRange(_, _) => "date",
        Operator::Is(_) => "is",
        Operator::Tag(_) => "tag",
        Operator::Note(_) => "note",
        Operator::In(_) => "in",
        Operator::Account(_) => "account",
        Operator::Thread(_) => "thread",
        Operator::Ai(_) => "ai",
    }
}

/// Classify one operator's positive form. See [`RawEffect`] for what each
/// outcome means; [`compile_filters`] is what applies `Filter::negated` on
/// top.
fn classify(op: &Operator) -> RawEffect {
    match op {
        Operator::From(value) => like_either("from_addr", "from_name", value),
        Operator::To(value) => like_one("to_addrs", value),
        Operator::Cc(value) => like_one("cc_addrs", value),
        Operator::Subject(value) => like_one("subject", value),
        Operator::Body(value) => like_one("body_text", value),
        Operator::Has(HasTarget::Attachment) => {
            RawEffect::Sql("has_attachments = 1".to_owned(), Vec::new())
        }
        Operator::Has(HasTarget::Note | HasTarget::Tag | HasTarget::Other(_)) => RawEffect::Never,
        Operator::Filename(pattern) => RawEffect::Sql(
            "EXISTS (SELECT 1 FROM attachments WHERE attachments.message_id = messages.id \
             AND lower(attachments.filename) GLOB ?)"
                .to_owned(),
            vec![Value::Text(pattern.to_ascii_lowercase())],
        ),
        Operator::Larger(bytes) => size_cmp(">", *bytes),
        Operator::Smaller(bytes) => size_cmp("<", *bytes),
        Operator::Before(raw) => match day_start(raw) {
            Some(ts) => RawEffect::Sql(
                "COALESCE(date, internaldate) < ?".to_owned(),
                vec![Value::Integer(ts)],
            ),
            None => RawEffect::Unknown,
        },
        Operator::After(raw) => match day_start(raw) {
            Some(ts) => RawEffect::Sql(
                "COALESCE(date, internaldate) >= ?".to_owned(),
                vec![Value::Integer(ts)],
            ),
            None => RawEffect::Unknown,
        },
        Operator::On(raw) => match day_start(raw) {
            Some(ts) => RawEffect::Sql(
                "COALESCE(date, internaldate) >= ? AND COALESCE(date, internaldate) < ?".to_owned(),
                vec![Value::Integer(ts), Value::Integer(ts + SECONDS_PER_DAY)],
            ),
            None => RawEffect::Unknown,
        },
        Operator::DateRange(start, end) => match (day_start(start), day_start(end)) {
            (Some(start_ts), Some(end_ts)) => RawEffect::Sql(
                "COALESCE(date, internaldate) >= ? AND COALESCE(date, internaldate) < ?".to_owned(),
                vec![
                    Value::Integer(start_ts),
                    // Inclusive of the end day: the upper bound is the start
                    // of the *next* day.
                    Value::Integer(end_ts + SECONDS_PER_DAY),
                ],
            ),
            _ => RawEffect::Unknown,
        },
        Operator::Is(IsFlag::Unread) => flag_predicate("\\Seen", false),
        Operator::Is(IsFlag::Read) => flag_predicate("\\Seen", true),
        Operator::Is(IsFlag::Flagged) => flag_predicate("\\Flagged", true),
        Operator::Is(IsFlag::Replied) => flag_predicate("\\Answered", true),
        Operator::Is(IsFlag::Pinned | IsFlag::Muted) => RawEffect::Never,
        Operator::Is(IsFlag::Other(value)) => is_other_flag(value),
        // `tag:`/`note:` have no backing table yet (tasks 55/56); `ai:` is
        // backed by `ai_summaries` (task 48) and resolved through the same
        // classifier `retrieve::filtermask` uses — see [`RawEffect::Never`]'s
        // docs and [`ai_predicate_sql`] for why this is shared rather than a
        // second, independently-drifting copy.
        Operator::Tag(_) | Operator::Note(_) => RawEffect::Never,
        Operator::Ai(predicate) => match ai_predicate_sql(predicate) {
            Some((sql, params)) => RawEffect::Sql(sql, params),
            None => RawEffect::Never,
        },
        Operator::In(name) => RawEffect::Sql(
            "mailbox_id IN (SELECT id FROM mailboxes WHERE name = ? COLLATE NOCASE)".to_owned(),
            vec![Value::Text(name.clone())],
        ),
        Operator::Account(name) => RawEffect::Sql(
            "account_id IN (SELECT id FROM accounts WHERE name = ? COLLATE NOCASE)".to_owned(),
            vec![Value::Text(name.clone())],
        ),
        Operator::Thread(id) => match id.trim().parse::<i64>() {
            Ok(id) => RawEffect::Sql("thread_id = ?".to_owned(), vec![Value::Integer(id)]),
            // `threads.id` is always an integer; a value that cannot parse as
            // one can never equal any thread's id — provably `Never`, not
            // `Unknown`.
            Err(_) => RawEffect::Never,
        },
    }
}

/// `size {cmp} ?`, with `bytes` saturated into `i64`'s range rather than
/// wrapped — `messages.size` is an `INTEGER` column, and a `larger:` value
/// past `i64::MAX` should still mean "bigger than anything ever will be",
/// not wrap into a small or negative bound.
fn size_cmp(cmp: &str, bytes: u64) -> RawEffect {
    let bound = i64::try_from(bytes).unwrap_or(i64::MAX);
    RawEffect::Sql(format!("size {cmp} ?"), vec![Value::Integer(bound)])
}

/// `EXISTS`/`NOT EXISTS` against `flags` for one IMAP flag string.
/// `present = true` asks "does this flag exist"; `false` asks "is it
/// absent" (used for `is:unread`, which is the absence of `\Seen`).
fn flag_predicate(flag: &str, present: bool) -> RawEffect {
    let exists =
        "EXISTS (SELECT 1 FROM flags WHERE flags.message_id = messages.id AND flags.flag = ?)";
    let sql = if present {
        exists.to_owned()
    } else {
        format!("NOT {exists}")
    };
    RawEffect::Sql(sql, vec![Value::Text(flag.to_owned())])
}

/// `is:<value>` for a value outside the six documented flags. A handful of
/// raw IMAP system flags this grammar does not name directly (`\Draft`,
/// `\Deleted`, `\Recent`) are recognized case-insensitively, `answered` is
/// accepted as a synonym for `is:replied`'s `\Answered`, and anything else is
/// treated as a literal custom IMAP keyword (`Flag::Custom` in
/// `message::fetch`), matched case-sensitively the way `flag_to_string`
/// stores it — `flags` is a general keyword table, not a closed enum, so a
/// value this grammar doesn't special-case is not automatically unbacked
/// data (contrast `is:pinned`/`is:muted`, which really do name nothing any
/// table tracks yet).
fn is_other_flag(value: &str) -> RawEffect {
    let flag = match value.to_ascii_lowercase().as_str() {
        "draft" => "\\Draft".to_owned(),
        "deleted" => "\\Deleted".to_owned(),
        "recent" => "\\Recent".to_owned(),
        "answered" => "\\Answered".to_owned(),
        _ => value.to_owned(),
    };
    flag_predicate(&flag, true)
}

/// `col LIKE ? OR col2 LIKE ?`, both columns matched against the same
/// escaped substring pattern.
fn like_either(col_a: &str, col_b: &str, value: &str) -> RawEffect {
    let pattern = like_pattern(value);
    RawEffect::Sql(
        format!("({col_a} LIKE ? ESCAPE '\\' OR {col_b} LIKE ? ESCAPE '\\')"),
        vec![Value::Text(pattern.clone()), Value::Text(pattern)],
    )
}

/// `col LIKE ?`, against an escaped substring pattern.
fn like_one(col: &str, value: &str) -> RawEffect {
    RawEffect::Sql(
        format!("{col} LIKE ? ESCAPE '\\'"),
        vec![Value::Text(like_pattern(value))],
    )
}

/// Turn `value` into a `%...%` substring pattern with SQL `LIKE` wildcards
/// (`%`, `_`) and the escape character itself escaped, so a user searching
/// for a literal `%` or `_` (a discount code, a variable name) gets a literal
/// match instead of an accidental wildcard.
fn like_pattern(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

/// Parse `raw` as a plain `YYYY-MM-DD` date and return the Unix-seconds
/// timestamp of that day's start, UTC. `None` for anything else — relative
/// expressions (`last-week`) and other formats are a later Stage 0 step's
/// job (see the module docs), not this one's to guess at.
fn day_start(raw: &str) -> Option<i64> {
    let date = chrono::NaiveDate::parse_from_str(raw.trim(), "%Y-%m-%d").ok()?;
    let midnight = date.and_hms_opt(0, 0, 0)?;
    Some(midnight.and_utc().timestamp())
}

#[cfg(test)]
mod tests;
