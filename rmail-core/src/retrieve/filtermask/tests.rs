//! `compile`'s decision logic, tested the same way
//! `retrieve::lexical`'s `compile_filters` is: pure, no database, because
//! every retriever that embeds the compiled SQL into a live query (`structured`,
//! `recency`, `prefix`, `dense`, `entity`, `fuzzy`) proves the mask actually
//! filters correctly in its own integration tests. What belongs here is
//! whether `compile` reaches the right *decision* for a given filter shape.

use rusqlite::types::Value;

use super::*;
use crate::query::{parse, Operator};

fn other(op: Operator, negated: bool) -> HardFilter {
    HardFilter::Other(Filter { op, negated })
}

fn date(op: Operator, negated: bool, range: DateRange) -> HardFilter {
    HardFilter::Date {
        filter: Filter { op, negated },
        range,
    }
}

#[test]
fn no_filters_is_unconstrained() {
    assert!(matches!(compile(&[]), FilterMask::Unconstrained));
}

#[test]
fn an_unbacked_filter_excludes_everything_but_its_negation_does_not() {
    let tag = other(Operator::Tag("work".to_owned()), false);
    assert!(matches!(compile(&[tag]), FilterMask::ExcludesEverything));

    let not_tag = other(Operator::Tag("work".to_owned()), true);
    assert!(matches!(compile(&[not_tag]), FilterMask::Unconstrained));
}

#[test]
fn a_non_numeric_thread_id_excludes_everything() {
    let filter = other(Operator::Thread("not-a-number".to_owned()), false);
    assert!(matches!(compile(&[filter]), FilterMask::ExcludesEverything));
}

#[test]
fn a_numeric_thread_id_compiles_to_an_equality_predicate() {
    let filter = other(Operator::Thread("42".to_owned()), false);
    let FilterMask::Sql(mask) = compile(&[filter]) else {
        unreachable!("expected a compiled predicate");
    };
    assert_eq!(mask.sql, "(thread_id = ?)");
    assert_eq!(mask.params, vec![Value::Integer(42)]);
}

#[test]
fn a_resolved_date_range_binds_both_bounds() {
    let filter = date(
        Operator::On("2024-06-15".to_owned()),
        false,
        DateRange {
            start: Some(100),
            end: Some(200),
        },
    );
    let FilterMask::Sql(mask) = compile(&[filter]) else {
        unreachable!("expected a compiled predicate");
    };
    assert_eq!(
        mask.sql,
        "(COALESCE(date, internaldate) >= ? AND COALESCE(date, internaldate) < ?)"
    );
    assert_eq!(mask.params, vec![Value::Integer(100), Value::Integer(200)]);
}

#[test]
fn a_one_sided_date_range_binds_one_bound() {
    let after = date(
        Operator::After("2024-06-15".to_owned()),
        false,
        DateRange {
            start: Some(100),
            end: None,
        },
    );
    let FilterMask::Sql(mask) = compile(&[after]) else {
        unreachable!("expected a compiled predicate");
    };
    assert_eq!(mask.sql, "(COALESCE(date, internaldate) >= ?)");

    let before = date(
        Operator::Before("2024-06-15".to_owned()),
        false,
        DateRange {
            start: None,
            end: Some(200),
        },
    );
    let FilterMask::Sql(mask) = compile(&[before]) else {
        unreachable!("expected a compiled predicate");
    };
    assert_eq!(mask.sql, "(COALESCE(date, internaldate) < ?)");
}

#[test]
fn an_unresolved_date_value_contributes_no_constraint() {
    // Reachable only via `HardFilter::Other` — task 26 tried to resolve
    // `before:whenever` and gave up, so it degrades the same way lexical.rs's
    // own unresolvable-date case does: skipped, not excluding everything.
    let filter = other(Operator::Before("whenever".to_owned()), false);
    assert!(matches!(compile(&[filter]), FilterMask::Unconstrained));
}

#[test]
fn negating_a_backed_filter_wraps_it_null_safely() {
    let filter = other(Operator::Cc("legal".to_owned()), true);
    let FilterMask::Sql(mask) = compile(&[filter]) else {
        unreachable!("expected a compiled predicate");
    };
    assert_eq!(
        mask.sql, "NOT COALESCE((cc_addrs LIKE ? ESCAPE '\\'), 0)",
        "NULL-safe negation so a message with no Cc header is included, not dropped"
    );
}

#[test]
fn multiple_filters_conjoin_in_the_order_given() {
    let filters = vec![
        other(Operator::From("alice".to_owned()), false),
        other(Operator::Is(crate::query::IsFlag::Unread), false),
    ];
    let FilterMask::Sql(mask) = compile(&filters) else {
        unreachable!("expected a compiled predicate");
    };
    assert!(mask.sql.contains(" AND "));
    // `from:` binds two params (`from_addr`/`from_name`, see `like_either`)
    // plus one for `is:unread`'s flag-presence check.
    assert_eq!(mask.params.len(), 3);
}

#[test]
fn exists_clause_correlates_on_the_given_message_id_expression() {
    let mask = Mask {
        sql: "account_id = ?".to_owned(),
        params: vec![Value::Integer(1)],
    };
    assert_eq!(
        mask.exists_clause("c.message_id"),
        "EXISTS (SELECT 1 FROM messages WHERE messages.id = c.message_id AND account_id = ?)"
    );
}

#[test]
fn real_parsed_filters_round_trip_through_compile() {
    // A smoke test against the real operator parser, so this module's
    // hand-built `HardFilter`s above are not the only shape it has ever seen.
    let parsed = parse::parse("from:alice is:unread");
    let hard: Vec<HardFilter> = parsed.filters.into_iter().map(HardFilter::Other).collect();
    let FilterMask::Sql(mask) = compile(&hard) else {
        unreachable!("expected a compiled predicate");
    };
    assert!(mask.sql.contains("from_addr"));
    assert!(mask.sql.contains("flags"));
}
