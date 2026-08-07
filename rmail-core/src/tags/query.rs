//! A minimal, hard-filter-only SQL compiler for `BulkTag`'s `query`
//! selector (prd.md CLI, as `mail tag-bulk --query "from:stripe" --account
//! <id> finance/receipt` — see `rmail_cli::tag_cli`'s module docs for why
//! this crate's CLI spells the bulk form as its own subcommand rather than
//! prd.md's literal `mail tag --bulk search:"…" <tag>`).
//!
//! Deliberately **not** a reuse of [`crate::retrieve::filtermask`]/
//! [`crate::retrieve::lexical`]'s own compilers: both are `pub(crate)` to
//! `retrieve` and built for the full ranking pipeline (relative-date
//! resolution via `QueryPlanner`, dense/entity/fuzzy fan-out, an embedder,
//! a corpus vocabulary). Selecting the message set a bulk tag applies to
//! needs none of that — only "which messages does this filter-only query
//! name" — so this is a small, self-contained compiler over the same
//! [`Operator`] enum ([`crate::query::parse`]), covering the operators a
//! bulk selection realistically needs.
//!
//! An operator outside that subset — and every free-text term/phrase — is
//! silently not applied, the same "degrade rather than error" rule
//! `query::parse` itself follows for a value that doesn't fit an operator's
//! shape: `mail tag --bulk search:"from:stripe invoice" finance/receipt`
//! bulk-selects by `from:` alone; `invoice` is simply not a bulk-selection
//! constraint this compiler understands (bulk-tagging needs an enumerable
//! set, not a ranked one, so there is no sound way to fold free text in
//! without either running the full lexical retriever — the exact coupling
//! this module exists to avoid — or silently dropping it, which is what
//! this already does, just made explicit here).
//!
//! # Two consumers, one compiler (task 35)
//!
//! [`crate::smart_folder`] resolves a deterministic smart folder's
//! membership through this same compiler and this same
//! [`select_message_ids`] statement, rather than growing a third copy of
//! "turn an operator query into a set of message ids" beside this one and
//! [`crate::retrieve::filtermask`]'s. The two consumers differ only in what
//! they do about the silent degradation described above, which is why
//! [`compile_detailed`] reports what it dropped alongside the finished
//! SQL: bulk-tagging accepts a dropped constraint (the caller named the
//! tag and can see the count before committing), while a smart folder
//! rejects one at create time — a *persistent* saved predicate whose ranked
//! half was silently discarded resolves to a strictly larger set than the
//! user described, on every future sync, with nobody watching.

use rusqlite::types::Value;
use rusqlite::Connection;

use crate::query::parse::{self, HasTarget, IsFlag, Operator};
use crate::retrieve::filtermask::tag_predicate_sql;

/// A compiled hard-filter query, plus what the compiler could not express.
///
/// The two `dropped_*` counts are the whole reason this type exists —
/// see the module docs' "Two consumers, one compiler" section.
pub(crate) struct Compiled {
    /// A `WHERE`-clause fragment over `messages`, already including the
    /// `account_id` scope.
    pub where_sql: String,
    /// Bound parameters, in the order the `?` placeholders appear.
    pub params: Vec<Value>,
    /// How many parsed operators became SQL.
    pub applied: usize,
    /// Parsed operators this compiler does not back, dropped from the
    /// predicate.
    pub dropped_operators: Vec<Operator>,
    /// Free-text terms and phrases, which a hard-filter-only compiler has
    /// nowhere to put (they rank, they do not gate).
    pub dropped_free_text: Vec<String>,
}

/// Compile `raw` into a [`Compiled`] predicate over `messages`, scoped to
/// `account_id`, plus everything in `raw` the compiler could not express.
#[must_use]
pub(crate) fn compile_detailed(account_id: i64, raw: &str) -> Compiled {
    let parsed = parse::parse(raw);
    let mut clauses = vec!["account_id = ?".to_owned()];
    let mut params = vec![Value::Integer(account_id)];
    let mut applied = 0usize;
    let mut dropped_operators = Vec::new();

    for filter in &parsed.filters {
        let Some((sql, filter_params)) = classify(&filter.op) else {
            dropped_operators.push(filter.op.clone());
            continue;
        };
        applied += 1;
        clauses.push(if filter.negated {
            // NULL-safe negation — see `retrieve::lexical::build_negated_clause`
            // for the identical three-valued-logic hazard this closes; every
            // predicate `classify` builds is `EXISTS`-based or a plain
            // column comparison, both of which have the same "NULL reads as
            // no-match" pitfall under a naive `NOT (...)`.
            format!("NOT COALESCE(({sql}), 0)")
        } else {
            format!("({sql})")
        });
        params.extend(filter_params);
    }

    let dropped_free_text = parsed
        .terms
        .iter()
        .map(|term| term.text.clone())
        .chain(parsed.phrases.iter().map(|phrase| phrase.text.clone()))
        .collect();

    Compiled {
        where_sql: clauses.join(" AND "),
        params,
        applied,
        dropped_operators,
        dropped_free_text,
    }
}

/// Run a [`Compiled`] predicate and collect the message ids it names,
/// ascending.
///
/// The single statement both `BulkTag`'s selector and a smart folder's
/// membership go through — a caller supplies its own read wrapper (a plain
/// [`crate::storage::Database::read`], or
/// [`crate::retrieve::cancel::interruptible_read`] where a cancellation
/// token has to reach the in-flight scan), so the two share the query
/// without sharing a concurrency policy.
///
/// `ORDER BY id` is not cosmetic: a smart folder diffs consecutive
/// evaluations of this list against each other, and an unordered result
/// would make that diff depend on whatever plan SQLite happened to pick.
pub(crate) fn select_message_ids(
    conn: &Connection,
    compiled: &Compiled,
) -> rusqlite::Result<Vec<i64>> {
    select_message_ids_limited(conn, compiled, None)
}

/// [`select_message_ids`], stopping after `limit` rows.
///
/// The bound goes into the SQL rather than being applied to the returned
/// `Vec`, because the caller that wants one (a paged
/// `ListSmartFolderMembers`) is asking a folder that may hold every message
/// in the account for its first twenty — materializing all of them first
/// would make the bound cost exactly what it exists to avoid.
pub(crate) fn select_message_ids_limited(
    conn: &Connection,
    compiled: &Compiled,
    limit: Option<usize>,
) -> rusqlite::Result<Vec<i64>> {
    // `limit` is an integer this crate computed, never caller text, so
    // formatting it is not an injection surface — and it cannot be bound as
    // a parameter without disturbing the positional `?` order `compiled`
    // already owns.
    let bound = match limit {
        Some(limit) => format!(" LIMIT {limit}"),
        None => String::new(),
    };
    let sql = format!(
        "SELECT id FROM messages WHERE {} ORDER BY id{bound}",
        compiled.where_sql
    );
    let mut stmt = conn.prepare(&sql)?;
    let bind: Vec<&dyn rusqlite::ToSql> = compiled
        .params
        .iter()
        .map(|v| v as &dyn rusqlite::ToSql)
        .collect();
    let rows = stmt
        .query_map(bind.as_slice(), |row| row.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<i64>>>()?;
    Ok(rows)
}

/// Classify one operator's positive form into a SQL predicate over
/// `messages`, or `None` if this compiler does not back it (see the module
/// docs' "degrade rather than error" note).
fn classify(op: &Operator) -> Option<(String, Vec<Value>)> {
    match op {
        Operator::From(value) => Some(like_either("from_addr", "from_name", value)),
        Operator::To(value) => Some(like_one("to_addrs", value)),
        Operator::Cc(value) => Some(like_one("cc_addrs", value)),
        Operator::Subject(value) => Some(like_one("subject", value)),
        Operator::Has(HasTarget::Attachment) => {
            Some(("has_attachments = 1".to_owned(), Vec::new()))
        }
        Operator::Is(IsFlag::Unread) => Some(flag_predicate("\\Seen", false)),
        Operator::Is(IsFlag::Read) => Some(flag_predicate("\\Seen", true)),
        Operator::Is(IsFlag::Flagged) => Some(flag_predicate("\\Flagged", true)),
        Operator::Is(IsFlag::Replied) => Some(flag_predicate("\\Answered", true)),
        Operator::In(name) => Some((
            "mailbox_id IN (SELECT id FROM mailboxes WHERE name = ? COLLATE NOCASE)".to_owned(),
            vec![Value::Text(name.clone())],
        )),
        // Reused verbatim from `retrieve::filtermask` — the one place both
        // that module's own `tag:` classifier and this one turn a tag name
        // into SQL, so a future change to what `tag:` matches cannot drift
        // between search and bulk-tag selection.
        Operator::Tag(name) => Some(tag_predicate_sql(name)),
        _ => None,
    }
}

fn like_either(col_a: &str, col_b: &str, value: &str) -> (String, Vec<Value>) {
    let pattern = like_pattern(value);
    (
        format!("({col_a} LIKE ? ESCAPE '\\' OR {col_b} LIKE ? ESCAPE '\\')"),
        vec![Value::Text(pattern.clone()), Value::Text(pattern)],
    )
}

fn like_one(col: &str, value: &str) -> (String, Vec<Value>) {
    (
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

fn flag_predicate(flag: &str, present: bool) -> (String, Vec<Value>) {
    let exists =
        "EXISTS (SELECT 1 FROM flags WHERE flags.message_id = messages.id AND flags.flag = ?)";
    let sql = if present {
        exists.to_owned()
    } else {
        format!("NOT {exists}")
    };
    (sql, vec![Value::Text(flag.to_owned())])
}

#[cfg(test)]
mod tests;
