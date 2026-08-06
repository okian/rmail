//! Compiling `QueryPlan::hard_filters` into one SQL predicate, shared by
//! every retriever in this task that is not [`super::lexical`].
//!
//! # Why this exists instead of six copies
//!
//! [`super::dense`], [`super::entity`], [`super::fuzzy`], [`super::prefix`],
//! [`super::recency`], and [`super::structured`] all need the same thing:
//! "only messages this query's `from:`/`is:`/`before:`/... constraints
//! allow". Writing that predicate once and reusing it is not just less code —
//! it is the difference between six retrievers that agree about what `-in:Spam`
//! means and six retrievers that each got one detail (NULL-safety on
//! negation, an unbacked `tag:` filter excluding everything, a malformed
//! `thread:` id) slightly differently. [`compile`] is that one place.
//!
//! # Why not share `retrieve::lexical`'s compiler instead
//!
//! `lexical.rs`'s `compile_filters` does almost this, and duplicating it here
//! is a real cost. It was not reused because it operates on a different
//! input: [`crate::query::Filter`], as task 25 produced it, with a
//! `before:`/`after:`/`on:`/`date:` value still a raw string — task 27 predates
//! task 26's `QueryPlan` in the dependency graph, so lexical.rs has never seen
//! a resolved date. This task's retrievers *do* — [`crate::query::HardFilter::Date`]
//! carries the absolute range task 26 already resolved, including relative
//! expressions (`before:last-week`) lexical.rs's own `day_start` cannot parse
//! at all. Reusing lexical.rs's compiler here would mean either handing it a
//! `QueryPlan` dependency it does not otherwise need, or discarding the
//! resolved range and re-deriving it from the raw string the way lexical.rs
//! does — silently losing relative-date support for five of the seven
//! retrievers for no reason. Duplicating the (short, mechanical, well-tested-
//! by-precedent) operator classification once here is the smaller risk than
//! either.
//!
//! # Hard filters, not scope
//!
//! See `retrieve`'s module docs for the integration decision this module is
//! the concrete answer to: every constraint this compiles comes from
//! [`crate::query::QueryPlan::hard_filters`]; [`crate::query::QueryPlan::scope`]
//! is never read.

use rusqlite::types::Value;

use crate::query::{DateRange, Filter, HardFilter, HasTarget, IsFlag, Operator};

/// A hard-filter mask compiled to SQL: a boolean expression over `messages`
/// (and, via `EXISTS`/`IN` subqueries, `flags`/`attachments`/`mailboxes`/
/// `accounts`), plus the parameters it binds.
///
/// `sql` is safe to embed verbatim in a query — built only from fixed,
/// trusted column/table names this module wrote, never from filter *values*,
/// which always travel through `params` as bound parameters.
pub(crate) struct Mask {
    pub(crate) sql: String,
    pub(crate) params: Vec<Value>,
}

/// The result of compiling a plan's hard filters.
pub(crate) enum FilterMask {
    /// No filters, or none of them contribute a constraint.
    Unconstrained,
    /// At least one filter's positive form provably matches nothing, so —
    /// filters conjoin — the whole query matches nothing.
    ExcludesEverything,
    /// A real SQL predicate.
    Sql(Mask),
}

impl Mask {
    /// `sql`, wrapped as a correlated `EXISTS` gate against `messages` for a
    /// query whose driving table is something other than `messages` itself
    /// (`fts_messages.rowid`, a chunk's `message_id`, an entity mention's
    /// `message_id`, ...). Mirrors `retrieve::lexical`'s own `EXISTS` mask —
    /// see that module's docs for why a correlated `EXISTS` is used instead
    /// of `rowid IN (SELECT id FROM messages WHERE ...)`: the latter plans
    /// independently of the driving query and full-scans `messages`
    /// regardless of how few candidate rows actually need checking.
    pub(crate) fn exists_clause(&self, message_id_expr: &str) -> String {
        format!(
            "EXISTS (SELECT 1 FROM messages WHERE messages.id = {message_id_expr} AND {})",
            self.sql
        )
    }
}

/// What evaluating one hard filter's *positive* form (ignoring negation)
/// resolves to, before [`compile`] applies negation. Mirrors
/// `retrieve::lexical::RawEffect` exactly — see this module's docs for why
/// the two are not the same type.
enum RawEffect {
    /// A real SQL predicate over `messages`, invertible with `NOT (...)`.
    Sql(String, Vec<Value>),
    /// The positive form is false for every message today.
    Never,
    /// This stage cannot resolve the value with confidence in either
    /// direction. Unreachable for a date-shaped operator that resolved (that
    /// is [`HardFilter::Date`]); reachable for one that did not (task 26
    /// already tried and gave up — see [`HardFilter::Other`]'s docs).
    Unknown,
}

/// Compile `filters` into a [`FilterMask`].
pub(crate) fn compile(filters: &[HardFilter]) -> FilterMask {
    let mut clauses = Vec::new();
    let mut params = Vec::new();

    for hard in filters {
        let (effect, negated) = match hard {
            HardFilter::Date { filter, range } => (date_effect(range), filter.negated),
            HardFilter::Other(filter) => (classify(&filter.op), filter.negated),
        };
        match effect {
            RawEffect::Sql(sql, sql_params) => {
                clauses.push(if negated {
                    build_negated_clause(&sql)
                } else {
                    format!("({sql})")
                });
                params.extend(sql_params);
            }
            RawEffect::Never if !negated => return FilterMask::ExcludesEverything,
            RawEffect::Never => {
                tracing::debug!(
                    operator = operator_kind(hard.filter()),
                    "negation of an operator with no backing data yet is vacuously true; \
                     contributes no constraint"
                );
            }
            RawEffect::Unknown => {
                tracing::debug!(
                    operator = operator_kind(hard.filter()),
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

/// A resolved date range's effect — always [`RawEffect::Sql`], since
/// [`HardFilter::Date`] only exists once task 26 has already produced an
/// absolute range.
fn date_effect(range: &DateRange) -> RawEffect {
    match (range.start, range.end) {
        (Some(start), Some(end)) => RawEffect::Sql(
            "COALESCE(date, internaldate) >= ? AND COALESCE(date, internaldate) < ?".to_owned(),
            vec![Value::Integer(start), Value::Integer(end)],
        ),
        (Some(start), None) => RawEffect::Sql(
            "COALESCE(date, internaldate) >= ?".to_owned(),
            vec![Value::Integer(start)],
        ),
        (None, Some(end)) => RawEffect::Sql(
            "COALESCE(date, internaldate) < ?".to_owned(),
            vec![Value::Integer(end)],
        ),
        // `before:`/`after:`/`on:`/`date:` always resolve at least one bound
        // when task 26 produces `HardFilter::Date` at all (see
        // `query::plan::resolve_filters`) — both `None` is unreachable in
        // practice, but degrading to "no constraint" rather than matching
        // nothing is the same fail-open rule every other unresolvable case
        // here follows.
        (None, None) => RawEffect::Unknown,
    }
}

/// Negate `sql` NULL-safely — see `retrieve::lexical::build_negated_clause`,
/// which this is byte-for-byte identical to: the same three-valued-logic
/// hazard applies to the same nullable columns regardless of which module's
/// mask a predicate ends up in.
fn build_negated_clause(sql: &str) -> String {
    format!("NOT COALESCE(({sql}), 0)")
}

fn operator_kind(filter: &Filter) -> &'static str {
    match &filter.op {
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

/// Classify one operator's positive form (everything except a resolved
/// date, which never reaches this function — see [`date_effect`]).
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
        // A date-shaped operator lands here only when task 26 could not
        // resolve its value (see `HardFilter::Other`'s docs) — the resolved
        // case is `HardFilter::Date`, handled by `date_effect` before
        // `classify` is ever called.
        Operator::Before(_) | Operator::After(_) | Operator::On(_) | Operator::DateRange(_, _) => {
            RawEffect::Unknown
        }
        Operator::Is(IsFlag::Unread) => flag_predicate("\\Seen", false),
        Operator::Is(IsFlag::Read) => flag_predicate("\\Seen", true),
        Operator::Is(IsFlag::Flagged) => flag_predicate("\\Flagged", true),
        Operator::Is(IsFlag::Replied) => flag_predicate("\\Answered", true),
        Operator::Is(IsFlag::Pinned | IsFlag::Muted) => RawEffect::Never,
        Operator::Is(IsFlag::Other(value)) => is_other_flag(value),
        Operator::Tag(_) | Operator::Note(_) | Operator::Ai(_) => RawEffect::Never,
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
            Err(_) => RawEffect::Never,
        },
    }
}

fn size_cmp(cmp: &str, bytes: u64) -> RawEffect {
    let bound = i64::try_from(bytes).unwrap_or(i64::MAX);
    RawEffect::Sql(format!("size {cmp} ?"), vec![Value::Integer(bound)])
}

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

fn like_either(col_a: &str, col_b: &str, value: &str) -> RawEffect {
    let pattern = like_pattern(value);
    RawEffect::Sql(
        format!("({col_a} LIKE ? ESCAPE '\\' OR {col_b} LIKE ? ESCAPE '\\')"),
        vec![Value::Text(pattern.clone()), Value::Text(pattern)],
    )
}

fn like_one(col: &str, value: &str) -> RawEffect {
    RawEffect::Sql(
        format!("{col} LIKE ? ESCAPE '\\'"),
        vec![Value::Text(like_pattern(value))],
    )
}

fn like_pattern(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{escaped}%")
}

#[cfg(test)]
mod tests;
