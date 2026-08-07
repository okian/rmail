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

use rusqlite::types::Value;

use crate::query::parse::{self, HasTarget, IsFlag, Operator};
use crate::retrieve::filtermask::tag_predicate_sql;

/// Compile `raw` into a `WHERE`-clause fragment (already including the
/// `account_id` scope) plus its bound parameters, in the order the `?`
/// placeholders appear.
#[must_use]
pub(crate) fn compile(account_id: i64, raw: &str) -> (String, Vec<Value>) {
    let parsed = parse::parse(raw);
    let mut clauses = vec!["account_id = ?".to_owned()];
    let mut params = vec![Value::Integer(account_id)];

    for filter in &parsed.filters {
        let Some((sql, filter_params)) = classify(&filter.op) else {
            continue;
        };
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
    (clauses.join(" AND "), params)
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
