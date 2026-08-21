#![allow(clippy::panic)]

use super::*;
use crate::tui::model::AiFacts;

fn row(id: i64) -> MessageRow {
    MessageRow {
        id,
        subject: format!("subject {id}"),
        from: "Alice".to_owned(),
        from_addr: Some("alice@example.com".to_owned()),
        date: Some(1_700_000_000 + id),
        flags: Vec::new(),
        has_attachments: false,
        has_note: false,
        to: None,
        tags: Vec::new(),
        ai: None,
    }
}

fn supported(raw: &str) -> Predicate {
    match classify(raw) {
        Classification::Supported(predicate) => predicate,
        Classification::Unsupported(op) => {
            panic!("expected {raw:?} to be supported, got Unsupported({op:?})")
        }
    }
}

fn unsupported(raw: &str) -> String {
    match classify(raw) {
        Classification::Unsupported(op) => op,
        Classification::Supported(_) => panic!("expected {raw:?} to be unsupported"),
    }
}

#[test]
fn an_empty_filter_is_a_no_op_identity() {
    let predicate = supported("");
    assert!(predicate.matches(&row(1)));
    let mut other = row(2);
    other.subject = "anything at all".to_owned();
    other.flags = vec!["\\Flagged".to_owned()];
    assert!(predicate.matches(&other));
}

#[test]
fn a_blank_filter_is_also_a_no_op_identity() {
    // Whitespace-only input degrades to no tokens at all, the same as "".
    assert!(supported("   ").matches(&row(1)));
}

// ---- from: ----

#[test]
fn from_matches_the_display_name_case_insensitively() {
    let predicate = supported("from:ALICE");
    assert!(predicate.matches(&row(1)));
}

#[test]
fn from_matches_the_address_when_the_name_does_not_contain_it() {
    let predicate = supported("from:example.com");
    assert!(predicate.matches(&row(1)));
}

#[test]
fn from_excludes_a_row_that_matches_neither_name_nor_address() {
    let predicate = supported("from:bob");
    assert!(!predicate.matches(&row(1)));
}

// ---- to: ----

#[test]
fn to_matches_the_stored_recipient_string() {
    let predicate = supported("to:team@example.com");
    let mut r = row(1);
    r.to = Some("team@example.com, other@example.com".to_owned());
    assert!(predicate.matches(&r));
}

#[test]
fn to_never_matches_a_row_with_no_recipient_loaded() {
    let predicate = supported("to:team@example.com");
    assert!(!predicate.matches(&row(1)));
}

// ---- subject: ----

#[test]
fn subject_matches_a_substring_case_insensitively() {
    let predicate = supported("subject:INVOICE");
    let mut r = row(1);
    r.subject = "Q3 invoice — net 30".to_owned();
    assert!(predicate.matches(&r));
}

#[test]
fn subject_excludes_a_non_matching_row() {
    let predicate = supported("subject:invoice");
    let mut r = row(1);
    r.subject = "lunch tomorrow?".to_owned();
    assert!(!predicate.matches(&r));
}

// ---- is: ----

#[test]
fn is_unread_matches_a_row_with_no_seen_flag() {
    let predicate = supported("is:unread");
    assert!(predicate.matches(&row(1)));
    let mut seen = row(2);
    seen.flags = vec![SEEN.to_owned()];
    assert!(!predicate.matches(&seen));
}

#[test]
fn is_read_matches_a_row_carrying_seen() {
    let predicate = supported("is:read");
    let mut r = row(1);
    r.flags = vec![SEEN.to_owned()];
    assert!(predicate.matches(&r));
    assert!(!predicate.matches(&row(2)));
}

#[test]
fn is_flagged_matches_the_flagged_imap_flag() {
    let predicate = supported("is:flagged");
    let mut r = row(1);
    r.flags = vec![FLAGGED.to_owned()];
    assert!(predicate.matches(&r));
    assert!(!predicate.matches(&row(2)));
}

#[test]
fn is_replied_matches_the_answered_imap_flag() {
    let predicate = supported("is:replied");
    let mut r = row(1);
    r.flags = vec![ANSWERED.to_owned()];
    assert!(predicate.matches(&r));
    assert!(!predicate.matches(&row(2)));
}

#[test]
fn is_pinned_and_is_muted_never_match_anything_yet() {
    // Mirrors `retrieve::filtermask`'s own `RawEffect::Never` for these two —
    // there is no real predicate for either server-side yet, so a row that
    // could plausibly represent "pinned" (via a made-up flag) still gets no.
    let mut r = row(1);
    r.flags = vec!["pinned".to_owned(), "muted".to_owned()];
    assert!(!supported("is:pinned").matches(&r));
    assert!(!supported("is:muted").matches(&r));
}

#[test]
fn negating_is_pinned_or_is_muted_matches_every_row_since_the_positive_never_does() {
    // `hit != negated`: the positive is `false` for every row (see the test
    // above), so negating either one flips to "always matches" — not "still
    // excludes what it would have matched", since there is nothing it ever
    // matches to exclude. Worth pinning explicitly: a future `matches_is`
    // refactor that made `Pinned`/`Muted` genuinely `true` for some row would
    // silently flip `-is:pinned` from "everything" to "everything except
    // that row" with no test here noticing the direction changed.
    let mut r = row(1);
    r.flags = vec!["pinned".to_owned(), "muted".to_owned()];
    assert!(supported("-is:pinned").matches(&r));
    assert!(supported("-is:muted").matches(&r));
    assert!(supported("-is:pinned").matches(&row(2)));
    assert!(supported("-is:muted").matches(&row(2)));
}

#[test]
fn is_other_recognizes_the_documented_synonyms() {
    for (value, flag) in [
        ("draft", "\\Draft"),
        ("deleted", "\\Deleted"),
        ("recent", "\\Recent"),
        ("answered", ANSWERED),
    ] {
        let predicate = supported(&format!("is:{value}"));
        let mut r = row(1);
        r.flags = vec![flag.to_owned()];
        assert!(predicate.matches(&r), "is:{value} should match {flag}");
    }
}

#[test]
fn is_other_falls_back_to_a_literal_custom_keyword() {
    let predicate = supported("is:vip");
    let mut r = row(1);
    r.flags = vec!["vip".to_owned()];
    assert!(predicate.matches(&r));
    assert!(!predicate.matches(&row(2)));
}

// ---- has: ----

#[test]
fn has_attachment_reads_the_row_flag() {
    let predicate = supported("has:attachment");
    let mut r = row(1);
    r.has_attachments = true;
    assert!(predicate.matches(&r));
    assert!(!predicate.matches(&row(2)));
}

#[test]
fn has_note_reads_the_row_flag() {
    let predicate = supported("has:note");
    let mut r = row(1);
    r.has_note = true;
    assert!(predicate.matches(&r));
    assert!(!predicate.matches(&row(2)));
}

#[test]
fn has_tag_is_true_whenever_any_tag_is_applied() {
    let predicate = supported("has:tag");
    let mut r = row(1);
    r.tags = vec!["work".to_owned()];
    assert!(predicate.matches(&r));
    assert!(!predicate.matches(&row(2)));
}

#[test]
fn has_an_unrecognized_value_never_matches() {
    let predicate = supported("has:calendar");
    let mut r = row(1);
    r.has_attachments = true;
    r.has_note = true;
    r.tags = vec!["work".to_owned()];
    assert!(!predicate.matches(&r));
}

#[test]
fn negating_has_an_unrecognized_value_matches_every_row_too() {
    // Same `Never`-effect-negates-to-"everything" shape as
    // `negating_is_pinned_or_is_muted_matches_every_row_...` above, for
    // `HasTarget::Other`'s identical `false` regardless of row state.
    let predicate = supported("-has:calendar");
    let mut r = row(1);
    r.has_attachments = true;
    r.has_note = true;
    r.tags = vec!["work".to_owned()];
    assert!(predicate.matches(&r));
    assert!(predicate.matches(&row(2)));
}

// ---- tag: ----

#[test]
fn tag_matches_an_exact_applied_tag_case_insensitively() {
    let predicate = supported("tag:Work");
    let mut r = row(1);
    r.tags = vec!["work".to_owned()];
    assert!(predicate.matches(&r));
}

#[test]
fn tag_without_a_glob_suffix_matches_the_exact_name_only() {
    // Mirrors `retrieve::filtermask::tag_predicate_sql` exactly: no trailing
    // `/*` means an exact match, full stop — a child tag does NOT match.
    let predicate = supported("tag:project");
    let mut child = row(1);
    child.tags = vec!["project/acme".to_owned()];
    assert!(!predicate.matches(&child));

    let mut exact = row(2);
    exact.tags = vec!["project".to_owned()];
    assert!(predicate.matches(&exact));
}

#[test]
fn tag_with_a_glob_suffix_matches_the_tag_itself_and_its_children() {
    let predicate = supported("tag:project/*");
    let mut itself = row(1);
    itself.tags = vec!["project".to_owned()];
    assert!(
        predicate.matches(&itself),
        "the named tag itself should match"
    );

    let mut child = row(2);
    child.tags = vec!["project/acme".to_owned()];
    assert!(predicate.matches(&child), "a direct child should match");

    let mut grandchild = row(3);
    grandchild.tags = vec!["project/acme/q3".to_owned()];
    assert!(
        predicate.matches(&grandchild),
        "a deeper descendant should match"
    );
}

#[test]
fn tag_with_a_glob_suffix_does_not_match_an_unrelated_tag() {
    let predicate = supported("tag:project/*");
    let mut r = row(1);
    r.tags = vec!["projectile".to_owned()];
    assert!(!predicate.matches(&r));
}

#[test]
fn tag_does_not_match_an_unrelated_prefix() {
    let predicate = supported("tag:project");
    let mut r = row(1);
    r.tags = vec!["projectile".to_owned()];
    assert!(!predicate.matches(&r));
}

#[test]
fn tag_excludes_a_row_with_no_matching_tag() {
    let predicate = supported("tag:work");
    assert!(!predicate.matches(&row(1)));
}

// ---- ai: ----

fn ai(f: impl FnOnce(&mut AiFacts)) -> Option<AiFacts> {
    let mut facts = AiFacts::default();
    f(&mut facts);
    Some(facts)
}

#[test]
fn ai_needs_reply_flag_matches_true_and_excludes_false_or_absent() {
    let predicate = supported("ai:needs-reply");
    let mut yes = row(1);
    yes.ai = ai(|a| a.needs_reply = Some(true));
    assert!(predicate.matches(&yes));

    let mut no = row(2);
    no.ai = ai(|a| a.needs_reply = Some(false));
    assert!(!predicate.matches(&no));

    assert!(
        !predicate.matches(&row(3)),
        "no ai data at all never matches"
    );
}

#[test]
fn ai_needs_reply_accepts_the_underscore_spelling_too() {
    let predicate = supported("ai:needs_reply");
    let mut r = row(1);
    r.ai = ai(|a| a.needs_reply = Some(true));
    assert!(predicate.matches(&r));
}

#[test]
fn ai_equals_matches_category_sentiment_and_priority() {
    type Setter = fn(&mut AiFacts, &str);
    let cases: [(&str, &str, Setter); 3] = [
        ("category", "invoice", |a, v| {
            a.category = Some(v.to_owned())
        }),
        ("sentiment", "negative", |a, v| {
            a.sentiment = Some(v.to_owned())
        }),
        ("priority", "high", |a, v| a.priority = Some(v.to_owned())),
    ];
    for (key, value, set) in cases {
        let predicate = supported(&format!("ai:{key}:{value}"));
        let mut r = row(1);
        r.ai = ai(|a| set(a, value));
        assert!(predicate.matches(&r), "ai:{key}:{value} should match");
        assert!(
            !predicate.matches(&row(2)),
            "ai:{key}:{value} should exclude no-data row"
        );
    }
}

#[test]
fn ai_equals_is_case_insensitive_on_the_value() {
    let predicate = supported("ai:category:INVOICE");
    let mut r = row(1);
    r.ai = ai(|a| a.category = Some("invoice".to_owned()));
    assert!(predicate.matches(&r));
}

#[test]
fn ai_equals_an_unrecognized_key_never_matches() {
    let predicate = supported("ai:mood:great");
    let mut r = row(1);
    r.ai = ai(|a| a.category = Some("great".to_owned()));
    assert!(!predicate.matches(&r));
}

#[test]
fn ai_priority_greater_than_orders_by_rank_not_by_string() {
    let predicate = supported("ai:priority>normal");
    let mut high = row(1);
    high.ai = ai(|a| a.priority = Some("high".to_owned()));
    assert!(predicate.matches(&high), "high > normal");

    let mut low = row(2);
    low.ai = ai(|a| a.priority = Some("low".to_owned()));
    assert!(!predicate.matches(&low), "low is not > normal");

    let mut equal = row(3);
    equal.ai = ai(|a| a.priority = Some("normal".to_owned()));
    assert!(!predicate.matches(&equal), "normal is not > normal");
}

#[test]
fn ai_priority_greater_than_high_pins_criticals_own_rank() {
    // Distinct from `ai_priority_greater_than_orders_by_rank_not_by_string`:
    // that test never compares against `high` as the *threshold*, so a wrong
    // rank for `critical` (anything other than the one value strictly above
    // `high`'s own) would not fail it. This is the one case that actually
    // exercises `critical`'s position in the ordering, not just that it
    // exists.
    let predicate = supported("ai:priority>high");
    let mut critical = row(1);
    critical.ai = ai(|a| a.priority = Some("critical".to_owned()));
    assert!(predicate.matches(&critical), "critical > high");

    let mut high = row(2);
    high.ai = ai(|a| a.priority = Some("high".to_owned()));
    assert!(!predicate.matches(&high), "high is not > high");
}

#[test]
fn ai_greater_than_only_accepts_priority_as_the_key() {
    let predicate = supported("ai:category>invoice");
    let mut r = row(1);
    r.ai = ai(|a| a.category = Some("zzz".to_owned()));
    assert!(!predicate.matches(&r));
}

#[test]
fn ai_greater_than_an_unrecognized_threshold_value_never_matches() {
    let predicate = supported("ai:priority>urgent");
    let mut r = row(1);
    r.ai = ai(|a| a.priority = Some("critical".to_owned()));
    assert!(!predicate.matches(&r));
}

// ---- negation ----

#[test]
fn a_negated_operator_excludes_what_it_would_otherwise_match() {
    let predicate = supported("-from:alice");
    assert!(!predicate.matches(&row(1)));
    let mut other = row(2);
    other.from = "Bob".to_owned();
    other.from_addr = Some("bob@example.com".to_owned());
    assert!(predicate.matches(&other));
}

#[test]
fn a_negated_flag_operator_excludes_what_it_would_otherwise_match() {
    let predicate = supported("-is:unread");
    assert!(
        !predicate.matches(&row(1)),
        "row(1) is unread, so -is:unread excludes it"
    );
    let mut r = row(2);
    r.flags = vec![SEEN.to_owned()];
    assert!(predicate.matches(&r));
}

#[test]
fn a_negated_free_text_term_excludes_a_match_and_includes_a_non_match() {
    let predicate = supported("-invoice");
    let mut has_it = row(1);
    has_it.subject = "Q3 invoice".to_owned();
    assert!(!predicate.matches(&has_it));

    let mut lacks_it = row(2);
    lacks_it.subject = "lunch tomorrow".to_owned();
    assert!(predicate.matches(&lacks_it));
}

// ---- free text ----

#[test]
fn free_text_matches_the_subject() {
    let predicate = supported("invoice");
    let mut r = row(1);
    r.subject = "Q3 invoice — net 30".to_owned();
    assert!(predicate.matches(&r));
}

#[test]
fn free_text_matches_the_sender_name_or_address() {
    let predicate = supported("acme");
    let mut named = row(1);
    named.from = "Acme Billing".to_owned();
    assert!(predicate.matches(&named));

    let mut addressed = row(2);
    addressed.from = "Billing".to_owned();
    addressed.from_addr = Some("billing@acme.com".to_owned());
    assert!(predicate.matches(&addressed));
}

#[test]
fn free_text_matches_the_recipient_when_loaded() {
    let predicate = supported("acme");
    let mut r = row(1);
    r.to = Some("ops@acme.com".to_owned());
    assert!(predicate.matches(&r));
}

#[test]
fn a_quoted_phrase_matches_the_same_way_a_word_does() {
    let predicate = supported("\"net 30\"");
    let mut r = row(1);
    r.subject = "invoice, net 30 terms".to_owned();
    assert!(predicate.matches(&r));
    assert!(!predicate.matches(&row(2)));
}

#[test]
fn an_operator_and_free_text_together_conjoin() {
    let predicate = supported("from:acme invoice");
    let mut both = row(1);
    both.from_addr = Some("billing@acme.com".to_owned());
    both.subject = "October invoice".to_owned();
    assert!(predicate.matches(&both));

    let mut only_operator = row(2);
    only_operator.from_addr = Some("billing@acme.com".to_owned());
    only_operator.subject = "hello".to_owned();
    assert!(!predicate.matches(&only_operator));
}

#[test]
fn two_filters_of_the_same_operator_both_have_to_hold() {
    // `from:` conjoins like every operator — a row needs an address matching
    // both substrings to pass, which most rows cannot but some legitimately
    // can (an address containing both terms), so both directions need
    // proving: one filter alone is not enough, and both together really are.
    let predicate = supported("from:acme from:beta");
    let mut only_one = row(1);
    only_one.from_addr = Some("billing@acme.com".to_owned());
    assert!(!predicate.matches(&only_one));

    let mut both = row(2);
    both.from_addr = Some("acme-beta-billing@example.com".to_owned());
    assert!(predicate.matches(&both));
}

// ---- unsupported operators (the full grammar minus the safe seven) ----

#[test]
fn every_operator_outside_the_safe_subset_is_reported_by_name() {
    // tui.md §9 item 2 also lists `todo:` and `summary:` in the grammar, but
    // `rmail_core::query::parse::OPERATORS`/`parse_operator` do not
    // implement either one — there is no `Operator::Todo`/`Operator::Summary`
    // variant at all, so `todo:x`/`summary:x` degrade to an ordinary
    // free-text term today (proven by
    // `an_unrecognized_key_value_pair_degrades_to_free_text_rather_than_erroring`'s
    // identical path for any unregistered key) rather than classifying
    // `Unsupported`. That is a gap in the shared parser both `/` and `f` sit
    // on top of, not something `filter.rs` can close on its own without
    // adding operators to `rmail-core` that `/` search would then need to
    // consume too — out of this task's stated scope (a new
    // `rmail-cli/src/tui/filter.rs`). Left as a known, documented limitation
    // rather than worked around locally.
    for (raw, expected_key) in [
        ("cc:alice", "cc"),
        ("body:refund", "body"),
        ("filename:*.pdf", "filename"),
        ("larger:5mb", "larger"),
        ("smaller:1kb", "smaller"),
        ("before:2024-01-01", "before"),
        ("after:2024-01-01", "after"),
        ("on:2024-01-01", "on"),
        ("date:2024-01-01..2024-02-01", "date"),
        ("note:reminder", "note"),
        ("in:Archive", "in"),
        ("account:work", "account"),
        ("thread:123", "thread"),
    ] {
        assert_eq!(unsupported(raw), expected_key, "classifying {raw:?}");
    }
}

#[test]
fn a_registered_operator_whose_value_fails_its_own_shape_check_is_still_unsupported() {
    // Whether an unsafe operator is rejected must not depend on whether its
    // value happens to be well-formed — `date:last-week` is `date:`'s key
    // shaped exactly like `date:2024-01-01..2024-02-01` above, and both must
    // report the same key, not one classifying `Unsupported` and the other
    // silently degrading to a literal-text search for "date:last-week".
    for (raw, expected_key) in [
        ("date:last-week", "date"),
        ("date:2024", "date"),
        ("larger:huge", "larger"),
        ("smaller:lots", "smaller"),
    ] {
        assert_eq!(unsupported(raw), expected_key, "classifying {raw:?}");
    }
}

#[test]
fn a_safe_operators_key_with_a_malformed_value_degrades_to_free_text_like_slash_search_does() {
    // `ai:priority>` and `ai:category:` are `ai:`'s value shape failing to
    // parse (a mid-keystroke or malformed comparison) — but `ai` is one of
    // the safe seven, so unlike an unsafe key's malformed value
    // (`a_registered_operator_whose_value_fails_its_own_shape_check_...`
    // above), this must NOT classify `Unsupported`: had the value parsed,
    // this filter would have evaluated it as a real `ai:` constraint, so
    // degrading to an inert free-text search costs nothing beyond what `/`
    // search already does with the identical input. Reporting these as
    // `Unsupported("ai")` was a real bug: it told someone mid-typing
    // `ai:priority>high` — an expression the filter fully supports — to go
    // use `/` instead, at the exact keystroke that made it momentarily
    // malformed.
    for raw in ["ai:priority>", "ai:category:"] {
        let predicate = supported(raw);
        let mut r = row(1);
        r.subject = format!("{raw} literally in the subject");
        assert!(predicate.matches(&r), "classifying {raw:?}");
        assert!(!predicate.matches(&row(2)), "classifying {raw:?}");
    }
}

#[test]
fn every_safe_operators_key_with_an_empty_or_unterminated_quoted_value_degrades_to_free_text() {
    // The specific bug this guards against: `split_operator` sets
    // `looked_like_operator` before checking whether the unquoted value is
    // empty (see `degraded_operator_key`'s own doc comment), so `key:"` —
    // one keystroke into typing `key:"something"` — and `key:""` — a
    // complete but empty quoted value — both look identical to a malformed
    // `ai:` comparison from `degraded_operator_key`'s point of view. Every
    // one of the safe seven must degrade the same way `ai:` now does above,
    // not flash "use `/` for that" on an operator this filter fully
    // supports the instant a user opens a quote.
    for key in ["from", "to", "subject", "has", "tag", "is", "ai"] {
        for raw in [format!("{key}:\""), format!("{key}:\"\"")] {
            let predicate = supported(&raw);
            let mut r = row(1);
            r.subject = format!("{raw} literally in the subject");
            assert!(predicate.matches(&r), "classifying {raw:?}");
            // Not just "not Unsupported": an *empty* predicate would also
            // satisfy the assertion above on every row, including this one,
            // so without this second half a future change that dropped the
            // degraded clause entirely — rather than folding it into free
            // text — would flip `f subject:"` from "matches nothing" to
            // "matches every row" and stay green here.
            assert!(!predicate.matches(&row(2)), "classifying {raw:?}");
        }
    }
}

#[test]
fn is_safe_operator_key_matches_exactly_the_operators_from_operator_accepts() {
    // The drift guard `is_safe_operator_key`'s own doc comment promises:
    // parse every registered operator's own documented example value, run
    // it through `SafeOperator::from_operator`, and check that "is safe"
    // agrees with `is_safe_operator_key(key)` for every one of the twenty
    // registered keys. A `SafeOperator` variant added or removed without
    // updating `is_safe_operator_key` fails right here instead of silently
    // mis-classifying malformed input for whichever key drifted.
    for (key, example) in parse::OPERATORS {
        // Quoted, not bare: `note`'s own example ("follow up") contains a
        // space, and an unquoted `format!` would let that silently
        // tokenize into a truncated `note:follow` filter plus a stray `up`
        // free-text term rather than actually exercising the whole example
        // value — passing today by luck (the truncated form still parses
        // as *some* `Note`, which is all this particular assertion needs),
        // not by design. `unquote` strips the quotes before `parse_operator`
        // ever sees the value, so every one of the twenty round-trips
        // identically either way — this form just does not depend on that
        // being true only for the nineteen examples with no space in them.
        let parsed = parse::parse(&format!("{key}:\"{example}\""));
        let filter = parsed.filters.into_iter().next().unwrap_or_else(|| {
            panic!(
                "{key}:\"{example}\" (OPERATORS' own documented example) did not parse as an operator"
            )
        });
        let is_safe = SafeOperator::from_operator(filter.op).is_ok();
        assert_eq!(
            is_safe,
            is_safe_operator_key(key),
            "{key} disagrees between SafeOperator::from_operator and is_safe_operator_key"
        );
    }
}

#[test]
fn the_semantic_sigil_is_unsupported() {
    assert_eq!(unsupported("~invoice"), "~");
}

#[test]
fn the_lexical_sigil_is_unsupported() {
    assert_eq!(unsupported("=invoice"), "=");
}

#[test]
fn a_sigil_on_a_phrase_is_unsupported_too() {
    assert_eq!(unsupported("~\"net 30\""), "~");
}

#[test]
fn one_unsupported_operator_rejects_the_whole_input_not_just_that_clause() {
    // §10: the filter renders the *input* red, it does not silently drop
    // `before:` and narrow by `from:` alone.
    assert_eq!(unsupported("from:acme before:2024"), "before");
}

#[test]
fn the_reported_offender_is_whichever_classify_reaches_first_not_whichever_reads_first() {
    // Pins the exact counterexample `Classification::Unsupported`'s own doc
    // comment uses: `~` reads first in the string, but `classify` checks
    // every filter (where `before:2024` lands, a real parsed `Operator`)
    // before it ever looks at terms (where `~x`, a sigil-carrying term,
    // lands) — so this reports `"before"`. Without this test, reordering
    // the two loops in `classify` would silently flip which name a caller
    // sees and nothing here would notice.
    assert_eq!(unsupported("~x before:2024"), "before");
}

#[test]
fn a_sigil_also_rejects_the_whole_input_including_an_already_accumulated_filter() {
    // The filters loop runs first, so a query mixing a safe operator with a
    // later sigil already has a `Predicate` under construction when the
    // sigil is reached — this is the terms-loop half of "reject the whole
    // input", not just the filters-loop half `one_unsupported_operator_...`
    // above proves.
    assert_eq!(unsupported("from:acme ~invoice"), "~");
}

#[test]
fn negating_an_unsupported_operator_does_not_make_it_supported() {
    assert_eq!(unsupported("-before:2024"), "before");
}

#[test]
fn negation_composes_with_a_sigil_and_the_result_is_still_unsupported() {
    assert_eq!(unsupported("-~invoice"), "~");
}

#[test]
fn an_unrecognized_key_value_pair_degrades_to_free_text_rather_than_erroring() {
    // Not part of the documented grammar at all (§9's operator list has no
    // `urgency:`), so `query::parse` itself folds it into free text — this
    // is supported, matching that degrade-never-error rule, not a fourteenth
    // "unsupported operator".
    let predicate = supported("urgency:high");
    let mut r = row(1);
    r.subject = "urgency:high please read".to_owned();
    assert!(predicate.matches(&r));
}

// ---- unloaded_data ----

#[test]
fn a_predicate_touching_only_real_data_reports_no_unloaded_data() {
    assert!(supported("").unloaded_data().is_empty());
    assert!(supported("from:acme subject:invoice is:unread invoice")
        .unloaded_data()
        .is_empty());
    assert!(supported("has:attachment").unloaded_data().is_empty());
}

#[test]
fn tag_and_has_tag_both_report_tags_as_unloaded() {
    assert_eq!(
        supported("tag:work").unloaded_data(),
        vec![UnloadedData::Tags]
    );
    assert_eq!(
        supported("has:tag").unloaded_data(),
        vec![UnloadedData::Tags]
    );
}

#[test]
fn has_note_reports_note_as_unloaded() {
    assert_eq!(
        supported("has:note").unloaded_data(),
        vec![UnloadedData::Note]
    );
}

#[test]
fn every_ai_shape_reports_ai_as_unloaded() {
    for raw in ["ai:needs-reply", "ai:category:invoice", "ai:priority>high"] {
        assert_eq!(
            supported(raw).unloaded_data(),
            vec![UnloadedData::Ai],
            "{raw:?}"
        );
    }
}

#[test]
fn repeating_the_same_unloaded_operator_reports_it_once() {
    assert_eq!(
        supported("tag:work has:tag tag:home").unloaded_data(),
        vec![UnloadedData::Tags]
    );
}

#[test]
fn a_mixed_predicate_reports_every_distinct_kind_it_touches_in_order() {
    assert_eq!(
        supported("tag:work ai:needs-reply has:note").unloaded_data(),
        vec![UnloadedData::Tags, UnloadedData::Ai, UnloadedData::Note]
    );
}

#[test]
fn a_negated_unloaded_operator_still_reports_as_unloaded() {
    // `unloaded_data` is about which data a clause *depends on*, independent
    // of whether the clause is negated — `-tag:work` is exactly as unable to
    // tell "no row is tagged work" from "no tag data is loaded" as `tag:work`
    // is (see `a_negated_free_text_term_...` and the `is:pinned`/`is:muted`
    // tests for the same "negation of an always-false clause matches
    // everything" behavior this composes with).
    assert_eq!(
        supported("-tag:work").unloaded_data(),
        vec![UnloadedData::Tags]
    );
}
