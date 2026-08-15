use super::*;

/// Convenience: the query's single filter, panicking (test-only) with a
/// useful message if there isn't exactly one.
fn only_filter(parsed: &ParsedQuery) -> &Filter {
    assert_eq!(
        parsed.filters.len(),
        1,
        "expected exactly one filter, got {:?}",
        parsed.filters
    );
    &parsed.filters[0]
}

/// Build a plain, unnegated, default-mode, non-operator-shaped [`Term`] for
/// the common case of an ordinary word.
fn term(text: &str) -> Term {
    Term {
        text: text.to_owned(),
        negated: false,
        mode: Mode::Auto,
        looked_like_operator: false,
    }
}

/// Build the [`Term`] a degraded `key:value` token becomes: same shape as
/// [`term`], but `looked_like_operator` is `true` because the token had
/// `key:` syntax before it fell back to free text.
fn degraded(text: &str) -> Term {
    Term {
        looked_like_operator: true,
        ..term(text)
    }
}

/// [`OPERATORS`] is what a UI completes from; the `match` in
/// [`parse_operator`] is what actually parses. Two lists in one file drift,
/// so this walks every advertised name — with the example value it
/// advertises — through the real parser.
#[test]
fn operator_table_matches_the_parser() {
    for (name, example) in OPERATORS {
        assert!(
            parse_operator(name, example).is_some(),
            "OPERATORS advertises {name}:{example}, which the parser rejects",
        );
    }
    assert!(
        parse_operator("nosuchoperator", "x").is_none(),
        "the check would pass vacuously if unknown keys parsed",
    );
    // The other direction, which the loop above cannot see: a new arm in
    // `parse_operator` that never reaches OPERATORS is an operator the search
    // box will not offer. There is no way to enumerate a `match`, so this is
    // a deliberate speed bump — bump the count *and* add the row.
    assert_eq!(
        OPERATORS.len(),
        20,
        "an operator was added to or removed from the grammar; keep OPERATORS level with \
         `parse_operator`'s match arms so the search box still offers all of them",
    );
}

// ---------------------------------------------------------------------------
// Address / text operators
// ---------------------------------------------------------------------------

#[test]
fn from_operator() {
    let parsed = parse("from:alice");
    assert_eq!(only_filter(&parsed).op, Operator::From("alice".to_owned()));
    assert!(!only_filter(&parsed).negated);
    assert!(parsed.terms.is_empty());
}

#[test]
fn to_operator() {
    let parsed = parse("to:me");
    assert_eq!(only_filter(&parsed).op, Operator::To("me".to_owned()));
}

#[test]
fn cc_operator() {
    let parsed = parse("cc:team@x.com");
    assert_eq!(
        only_filter(&parsed).op,
        Operator::Cc("team@x.com".to_owned())
    );
}

#[test]
fn subject_operator() {
    let parsed = parse("subject:invoice");
    assert_eq!(
        only_filter(&parsed).op,
        Operator::Subject("invoice".to_owned())
    );
}

#[test]
fn body_operator_with_quoted_value() {
    let parsed = parse(r#"body:"exact phrase""#);
    assert_eq!(
        only_filter(&parsed).op,
        Operator::Body("exact phrase".to_owned())
    );
    // The space inside the quoted value must not have split this into two
    // tokens.
    assert!(parsed.terms.is_empty());
    assert!(parsed.phrases.is_empty());
}

#[test]
fn quoted_operator_value_with_embedded_colon() {
    // The *first* colon is the key/value separator; a colon inside the
    // quoted value (very common in an email subject: "Re: invoice") must not
    // be mistaken for it.
    let parsed = parse(r#"subject:"re: invoice""#);
    assert_eq!(
        only_filter(&parsed).op,
        Operator::Subject("re: invoice".to_owned())
    );
}

#[test]
fn operator_value_without_leading_quote_keeps_a_trailing_quote_character() {
    // `unquote` only strips a trailing `"` when a leading one matched — an
    // unquoted value that simply ends with a literal `"` character (a typo,
    // or someone searching for a quotation) must not lose it.
    let parsed = parse(r#"from:alice""#);
    assert_eq!(
        only_filter(&parsed).op,
        Operator::From("alice\"".to_owned())
    );
}

// ---------------------------------------------------------------------------
// has: / filename:
// ---------------------------------------------------------------------------

#[test]
fn has_attachment_operator() {
    let parsed = parse("has:attachment");
    assert_eq!(
        only_filter(&parsed).op,
        Operator::Has(HasTarget::Attachment)
    );
}

#[test]
fn has_note_and_has_tag_operators() {
    assert_eq!(
        only_filter(&parse("has:note")).op,
        Operator::Has(HasTarget::Note)
    );
    assert_eq!(
        only_filter(&parse("has:tag")).op,
        Operator::Has(HasTarget::Tag)
    );
}

#[test]
fn has_unrecognized_value_is_kept_as_other() {
    // A `has:` target outside the documented set is not an error and is not
    // dropped — it is preserved so a future grammar addition (or a typo a
    // user can be shown) round-trips instead of vanishing.
    let parsed = parse("has:calendar");
    assert_eq!(
        only_filter(&parsed).op,
        Operator::Has(HasTarget::Other("calendar".to_owned()))
    );
}

#[test]
fn filename_glob_operator() {
    let parsed = parse("filename:*.pdf");
    assert_eq!(
        only_filter(&parsed).op,
        Operator::Filename("*.pdf".to_owned())
    );
}

// ---------------------------------------------------------------------------
// larger: / smaller:
// ---------------------------------------------------------------------------

#[test]
fn larger_and_smaller_size_operators() {
    assert_eq!(
        only_filter(&parse("larger:5mb")).op,
        Operator::Larger(5_000_000)
    );
    assert_eq!(
        only_filter(&parse("smaller:1mb")).op,
        Operator::Smaller(1_000_000)
    );
}

#[test]
fn size_operator_supports_every_unit_and_bare_bytes() {
    assert_eq!(only_filter(&parse("larger:900b")).op, Operator::Larger(900));
    assert_eq!(
        only_filter(&parse("larger:500kb")).op,
        Operator::Larger(500_000)
    );
    assert_eq!(
        only_filter(&parse("larger:2gb")).op,
        Operator::Larger(2_000_000_000)
    );
    // No unit at all: a bare byte count.
    assert_eq!(
        only_filter(&parse("larger:1024")).op,
        Operator::Larger(1024)
    );
    // Fractional magnitudes round to the nearest byte.
    assert_eq!(
        only_filter(&parse("larger:1.5mb")).op,
        Operator::Larger(1_500_000)
    );
}

#[test]
fn size_operator_with_invalid_value_degrades_to_free_text() {
    // `larger:` is registered, but "huge" doesn't fit its shape — the whole
    // token becomes free text rather than failing, exactly like an
    // unregistered key.
    let parsed = parse("larger:huge");
    assert!(parsed.filters.is_empty());
    assert_eq!(parsed.terms, vec![degraded("larger:huge")]);
}

#[test]
fn size_operator_overflow_degrades_to_free_text_for_both_directions() {
    // A magnitude that doesn't fit in a `u64` byte count must not saturate:
    // saturating `larger:` still reads as "bigger than anything" by
    // accident, but saturating `smaller:` would read as `Smaller(u64::MAX)`
    // — a constraint that matches everything, the opposite of what was
    // typed. Both must degrade instead.
    let larger = parse("larger:9e99gb");
    assert!(larger.filters.is_empty());
    assert_eq!(larger.terms, vec![degraded("larger:9e99gb")]);

    let smaller = parse("smaller:9e99gb");
    assert!(smaller.filters.is_empty());
    assert_eq!(smaller.terms, vec![degraded("smaller:9e99gb")]);
}

// ---------------------------------------------------------------------------
// before: / after: / on: / date:
// ---------------------------------------------------------------------------

#[test]
fn before_after_on_operators_keep_the_raw_date_text() {
    // Resolving "last-week" to an absolute date is a later stage's job; this
    // stage only has to recognize the operator and carry the value through
    // unchanged.
    assert_eq!(
        only_filter(&parse("before:2025-01-01")).op,
        Operator::Before("2025-01-01".to_owned())
    );
    assert_eq!(
        only_filter(&parse("after:last-week")).op,
        Operator::After("last-week".to_owned())
    );
    assert_eq!(
        only_filter(&parse("on:2026-07-01")).op,
        Operator::On("2026-07-01".to_owned())
    );
}

#[test]
fn date_range_operator() {
    let parsed = parse("date:2025-06..2025-08");
    assert_eq!(
        only_filter(&parsed).op,
        Operator::DateRange("2025-06".to_owned(), "2025-08".to_owned())
    );
}

#[test]
fn date_range_without_separator_degrades_to_free_text() {
    let parsed = parse("date:2025-06-01");
    assert!(parsed.filters.is_empty());
    assert_eq!(parsed.terms, vec![degraded("date:2025-06-01")]);
}

#[test]
fn date_range_with_empty_bound_degrades_to_free_text() {
    // Open-ended ranges are not part of the documented grammar; guessing a
    // meaning for the missing side would be inventing behavior, not parsing
    // it.
    let parsed = parse("date:2025-06..");
    assert!(parsed.filters.is_empty());
    assert_eq!(parsed.terms, vec![degraded("date:2025-06..")]);
}

#[test]
fn date_range_with_a_second_separator_degrades_to_free_text() {
    // A second ".." is not "the range from 2025-06 to 2025-07..2025-08" —
    // `split_once` would otherwise silently fold the extra separator into
    // the end bound. Neither documented nor guessable, so this degrades too.
    let parsed = parse("date:2025-06..2025-07..2025-08");
    assert!(parsed.filters.is_empty());
    assert_eq!(
        parsed.terms,
        vec![degraded("date:2025-06..2025-07..2025-08")]
    );
}

// ---------------------------------------------------------------------------
// is:
// ---------------------------------------------------------------------------

#[test]
fn is_flag_operators_cover_every_documented_value() {
    let cases = [
        ("is:unread", IsFlag::Unread),
        ("is:read", IsFlag::Read),
        ("is:flagged", IsFlag::Flagged),
        ("is:pinned", IsFlag::Pinned),
        ("is:replied", IsFlag::Replied),
        ("is:muted", IsFlag::Muted),
    ];
    for (query, expected) in cases {
        assert_eq!(
            only_filter(&parse(query)).op,
            Operator::Is(expected.clone()),
            "query {query:?}"
        );
    }
}

#[test]
fn is_unrecognized_value_is_kept_as_other() {
    let parsed = parse("is:archived");
    assert_eq!(
        only_filter(&parsed).op,
        Operator::Is(IsFlag::Other("archived".to_owned()))
    );
}

// ---------------------------------------------------------------------------
// tag: / note: / in: / account: / thread:
// ---------------------------------------------------------------------------

#[test]
fn tag_operator_and_hierarchical_glob() {
    assert_eq!(
        only_filter(&parse("tag:work")).op,
        Operator::Tag("work".to_owned())
    );
    assert_eq!(
        only_filter(&parse("tag:project/*")).op,
        Operator::Tag("project/*".to_owned())
    );
}

#[test]
fn note_operator() {
    assert_eq!(
        only_filter(&parse("note:contract")).op,
        Operator::Note("contract".to_owned())
    );
}

#[test]
fn in_operator() {
    assert_eq!(
        only_filter(&parse("in:INBOX")).op,
        Operator::In("INBOX".to_owned())
    );
    assert_eq!(
        only_filter(&parse("in:Archive")).op,
        Operator::In("Archive".to_owned())
    );
}

#[test]
fn account_operator() {
    assert_eq!(
        only_filter(&parse("account:Personal")).op,
        Operator::Account("Personal".to_owned())
    );
}

#[test]
fn thread_operator() {
    assert_eq!(
        only_filter(&parse("thread:abc123")).op,
        Operator::Thread("abc123".to_owned())
    );
}

// ---------------------------------------------------------------------------
// ai:
// ---------------------------------------------------------------------------

#[test]
fn ai_bare_flag() {
    assert_eq!(
        only_filter(&parse("ai:needs-reply")).op,
        Operator::Ai(AiPredicate::Flag("needs-reply".to_owned()))
    );
}

#[test]
fn ai_greater_than_predicate() {
    assert_eq!(
        only_filter(&parse("ai:priority>high")).op,
        Operator::Ai(AiPredicate::GreaterThan(
            "priority".to_owned(),
            "high".to_owned()
        ))
    );
}

#[test]
fn ai_equals_predicate() {
    assert_eq!(
        only_filter(&parse("ai:category:invoice")).op,
        Operator::Ai(AiPredicate::Equals(
            "category".to_owned(),
            "invoice".to_owned()
        ))
    );
    assert_eq!(
        only_filter(&parse("ai:sentiment:negative")).op,
        Operator::Ai(AiPredicate::Equals(
            "sentiment".to_owned(),
            "negative".to_owned()
        ))
    );
}

#[test]
fn ai_predicate_shape_is_decided_by_whichever_separator_comes_first() {
    // `:` at index 1 precedes `>` at index 3, so this reads as
    // `Equals("a", "b>c")`, not `GreaterThan("a:b", "c")`.
    let parsed = parse("ai:a:b>c");
    assert_eq!(
        only_filter(&parsed).op,
        Operator::Ai(AiPredicate::Equals("a".to_owned(), "b>c".to_owned()))
    );
}

#[test]
fn ai_predicate_with_an_empty_side_degrades_to_free_text() {
    // Each of these has a `>` or `:` present but leaves one side empty —
    // exactly the shape produced mid-keystroke while typing
    // `ai:priority>high` one character at a time. A hard filter that can
    // never match (`Flag(">")`) would be worse than free text here.
    for query in ["ai:>", "ai::", "ai:>x", "ai:x>"] {
        let parsed = parse(query);
        assert!(
            parsed.filters.is_empty(),
            "expected {query:?} to degrade, got {:?}",
            parsed.filters
        );
        assert_eq!(parsed.terms, vec![degraded(query)], "query {query:?}");
    }
}

// ---------------------------------------------------------------------------
// Negation
// ---------------------------------------------------------------------------

#[test]
fn negated_tag_filter() {
    let parsed = parse("-tag:newsletter");
    let filter = only_filter(&parsed);
    assert_eq!(filter.op, Operator::Tag("newsletter".to_owned()));
    assert!(filter.negated);
}

#[test]
fn negated_operator_generalizes_beyond_tag() {
    // Negation is a property of `Filter`, not baked into individual
    // `Operator` variants, so it must work uniformly across operators, not
    // just the one example (`-tag:`) the grammar happens to show.
    let parsed = parse("-from:spam@example.com");
    let filter = only_filter(&parsed);
    assert_eq!(filter.op, Operator::From("spam@example.com".to_owned()));
    assert!(filter.negated);
}

#[test]
fn negated_free_text_term() {
    let parsed = parse("-excludeterm");
    assert!(parsed.filters.is_empty());
    assert_eq!(
        parsed.terms,
        vec![Term {
            negated: true,
            ..term("excludeterm")
        }]
    );
}

#[test]
fn negated_phrase() {
    let parsed = parse(r#"-"exact phrase""#);
    assert_eq!(
        parsed.phrases,
        vec![Phrase {
            text: "exact phrase".to_owned(),
            negated: true,
            mode: Mode::Auto,
        }]
    );
}

#[test]
fn bare_dash_is_a_literal_term_not_a_dangling_negation() {
    let parsed = parse("-");
    assert!(parsed.filters.is_empty());
    assert_eq!(parsed.terms, vec![term("-")]);
}

// ---------------------------------------------------------------------------
// Quoted phrases (free text)
// ---------------------------------------------------------------------------

#[test]
fn quoted_phrase_free_text() {
    let parsed = parse(r#""multi word phrase""#);
    assert!(parsed.filters.is_empty());
    assert!(parsed.terms.is_empty());
    assert_eq!(
        parsed.phrases,
        vec![Phrase {
            text: "multi word phrase".to_owned(),
            negated: false,
            mode: Mode::Auto,
        }]
    );
}

#[test]
fn quoted_phrase_with_colon_is_not_mistaken_for_an_operator() {
    // The key-charset check in `split_operator` exists precisely for this:
    // the "key" here would be `"re` (a leading `"` is not an identifier
    // character), so this must never even attempt an operator lookup and
    // must come out as an ordinary phrase.
    let parsed = parse(r#""re:invoice""#);
    assert!(parsed.filters.is_empty());
    assert_eq!(
        parsed.phrases,
        vec![Phrase {
            text: "re:invoice".to_owned(),
            negated: false,
            mode: Mode::Auto,
        }]
    );
}

#[test]
fn unterminated_quote_runs_to_end_of_input() {
    // No closing quote: the rest of the string becomes the phrase rather
    // than erroring or losing the text.
    let parsed = parse(r#"say "hello there"#);
    assert_eq!(parsed.terms, vec![term("say")]);
    assert_eq!(
        parsed.phrases,
        vec![Phrase {
            text: "hello there".to_owned(),
            negated: false,
            mode: Mode::Auto,
        }]
    );
}

#[test]
fn empty_quoted_phrase_is_dropped() {
    // `""` has no text to rank on; keeping it would put a phrase in the plan
    // that can never match anything.
    let parsed = parse(r#"foo """#);
    assert_eq!(parsed.terms, vec![term("foo")]);
    assert!(parsed.phrases.is_empty());
}

// ---------------------------------------------------------------------------
// `~` / `=` mode sigils
// ---------------------------------------------------------------------------

#[test]
fn semantic_mode_sigil_on_term() {
    let parsed = parse("~semantic");
    assert_eq!(
        parsed.terms,
        vec![Term {
            mode: Mode::Semantic,
            ..term("semantic")
        }]
    );
}

#[test]
fn lexical_mode_sigil_on_term() {
    let parsed = parse("=exact");
    assert_eq!(
        parsed.terms,
        vec![Term {
            mode: Mode::Lexical,
            ..term("exact")
        }]
    );
}

#[test]
fn semantic_mode_sigil_on_phrase() {
    let parsed = parse(r#"~"office move""#);
    assert_eq!(
        parsed.phrases,
        vec![Phrase {
            text: "office move".to_owned(),
            negated: false,
            mode: Mode::Semantic,
        }]
    );
}

#[test]
fn lexical_mode_sigil_on_phrase() {
    let parsed = parse(r#"="exact phrase""#);
    assert_eq!(
        parsed.phrases,
        vec![Phrase {
            text: "exact phrase".to_owned(),
            negated: false,
            mode: Mode::Lexical,
        }]
    );
}

#[test]
fn negation_composes_with_a_mode_sigil() {
    let parsed = parse("-~urgent");
    assert_eq!(
        parsed.terms,
        vec![Term {
            negated: true,
            mode: Mode::Semantic,
            ..term("urgent")
        }]
    );
}

#[test]
fn sigil_before_negation_is_not_recognized_as_negation() {
    // The strip order is fixed: negation first, then the sigil. `=-foo` is
    // therefore *not* "negated, forced lexical, text 'foo'" — it is a
    // forced-lexical search for the literal text "-foo", per the module
    // docs' note on composition order.
    let parsed = parse("=-foo");
    assert_eq!(
        parsed.terms,
        vec![Term {
            mode: Mode::Lexical,
            ..term("-foo")
        }]
    );
}

#[test]
fn mode_sigil_prevents_operator_parsing() {
    // A ranking-mode sigil on a hard filter is meaningless (a filter is not
    // ranked), so `~tag:work` is a semantic search for the literal text
    // "tag:work", never a (mode-less) tag filter. This is the load-bearing
    // case for the module docs' "sigils apply to free text only" rule.
    let parsed = parse("~tag:work");
    assert!(parsed.filters.is_empty());
    assert_eq!(
        parsed.terms,
        vec![Term {
            mode: Mode::Semantic,
            ..term("tag:work")
        }]
    );
}

#[test]
fn lexical_sigil_also_prevents_operator_parsing() {
    let parsed = parse("=tag:work");
    assert!(parsed.filters.is_empty());
    assert_eq!(
        parsed.terms,
        vec![Term {
            mode: Mode::Lexical,
            ..term("tag:work")
        }]
    );
}

#[test]
fn bare_sigils_are_literal_tokens() {
    let parsed = parse("~ =");
    assert_eq!(parsed.terms, vec![term("~"), term("=")]);
}

// ---------------------------------------------------------------------------
// Unknown operators degrade to free text (never an error)
// ---------------------------------------------------------------------------

#[test]
fn unregistered_operator_degrades_to_free_text() {
    // `foo:` is not a registered key. The whole token is kept, verbatim, as
    // a free-text term — this is the specific behavior the task's acceptance
    // criteria calls out by name, so it gets its own dedicated test rather
    // than being inferred from the size/date-range degradation tests above.
    let parsed = parse("foo:bar");
    assert!(
        parsed.filters.is_empty(),
        "an unregistered key must not produce a filter"
    );
    assert_eq!(parsed.terms, vec![degraded("foo:bar")]);
    assert!(parsed.phrases.is_empty());
}

#[test]
fn unregistered_operator_with_quoted_value_degrades_verbatim() {
    let parsed = parse(r#"foo:"bar baz""#);
    assert!(parsed.filters.is_empty());
    // The degraded term keeps the original token text, including the quote
    // characters — it is "search literally for what was typed", not a
    // half-unwrapped value.
    assert_eq!(parsed.terms, vec![degraded(r#"foo:"bar baz""#)]);
}

#[test]
fn registered_key_with_empty_value_degrades_to_free_text() {
    // `from:`, `has:`, and `is:` with nothing after the colon are shaped
    // like the start of an operator but carry no value for it to act on —
    // the exact shape produced while a user is still typing. These are not
    // flagged `looked_like_operator` the way `form:alice` (an unregistered
    // key with a real value) is: `split_operator` declines a token with an
    // empty value before the key is even looked up, so no operator shape was
    // ever recognized to record.
    for query in ["from:", "has:", "is:"] {
        let parsed = parse(query);
        assert!(parsed.filters.is_empty(), "query {query:?}");
        assert_eq!(parsed.terms, vec![term(query)], "query {query:?}");
    }
}

#[test]
fn url_shaped_free_text_is_not_mistaken_for_an_operator() {
    // A bare URL is the most common real-world "key:value-shaped but not a
    // registered operator" input a mailbox search box will see. It is
    // syntactically operator-shaped (key `https`, value
    // `//example.com/invoice`), so `looked_like_operator` is honestly `true`
    // — a later spell-fix stage comparing `https` against the registered
    // operator names will simply find no close match and suggest nothing.
    let parsed = parse("https://example.com/invoice");
    assert!(parsed.filters.is_empty());
    assert_eq!(parsed.terms, vec![degraded("https://example.com/invoice")]);
}

#[test]
fn degraded_operator_shaped_terms_are_flagged_but_ordinary_words_are_not() {
    let parsed = parse("form:alice invoice");
    assert!(parsed.filters.is_empty());
    assert_eq!(parsed.terms, vec![degraded("form:alice"), term("invoice")]);
}

// ---------------------------------------------------------------------------
// Whole-query shape
// ---------------------------------------------------------------------------

#[test]
fn empty_query_returns_empty_parsed_query() {
    let parsed = parse("");
    assert_eq!(parsed, ParsedQuery::default());
}

#[test]
fn whitespace_only_query_returns_empty_parsed_query() {
    let parsed = parse("   \t  ");
    assert!(parsed.filters.is_empty());
    assert!(parsed.terms.is_empty());
    assert!(parsed.phrases.is_empty());
}

#[test]
fn raw_is_preserved_verbatim() {
    let parsed = parse("  from:alice   invoice ");
    assert_eq!(parsed.raw, "  from:alice   invoice ");
}

#[test]
fn operator_keys_are_case_insensitive() {
    let parsed = parse("From:alice");
    assert_eq!(only_filter(&parsed).op, Operator::From("alice".to_owned()));
}

#[test]
fn prd_example_from_alice_invoice() {
    // The PRD's own worked example (`mail search "from:alice invoice"
    // --explain`): one hard filter, one ranked term.
    let parsed = parse("from:alice invoice");
    assert_eq!(only_filter(&parsed).op, Operator::From("alice".to_owned()));
    assert_eq!(parsed.terms, vec![term("invoice")]);
}

#[test]
fn mixed_query_combines_filters_terms_and_phrases() {
    let parsed =
        parse(r#"from:alice -tag:newsletter "office move" ~urgent project alpha filename:*.pdf"#);

    assert_eq!(
        parsed.filters,
        vec![
            Filter {
                op: Operator::From("alice".to_owned()),
                negated: false,
            },
            Filter {
                op: Operator::Tag("newsletter".to_owned()),
                negated: true,
            },
            Filter {
                op: Operator::Filename("*.pdf".to_owned()),
                negated: false,
            },
        ]
    );
    assert_eq!(
        parsed.terms,
        vec![
            Term {
                mode: Mode::Semantic,
                ..term("urgent")
            },
            term("project"),
            term("alpha"),
        ]
    );
    assert_eq!(
        parsed.phrases,
        vec![Phrase {
            text: "office move".to_owned(),
            negated: false,
            mode: Mode::Auto,
        }]
    );
}

#[test]
fn parsing_never_panics_and_always_preserves_raw() {
    // This directly encodes the task's "never an error" acceptance
    // criterion as a single property, over the inputs most likely to find a
    // gap in the pointwise tests above: degenerate quoting, bare modifiers,
    // malformed operator shapes, and non-ASCII text. `parse` returning
    // `ParsedQuery` rather than `Result` makes "does not error" trivially
    // true for the type system; what this test actually checks is that
    // nothing panics and that `raw` — the one thing every caller can fall
    // back to — always comes back unchanged.
    let pathological = [
        "",
        " ",
        "\"",
        "\"\"",
        "from:",
        "from:\"",
        "-:",
        "~:",
        "::",
        ":",
        "ai:>",
        "ai::",
        "date:..",
        "date:2025-06..2025-07..2025-08",
        "larger:9e99gb",
        "café naïve 日本語 🔥",
        "ai:🔥>x",
        "say \"unterminated",
        "-",
        "~",
        "=",
        "-~",
        "=-foo",
        "~-foo",
        "\"\"\"\"",
        "-\"",
        "~\"",
        "-~=\"a:b>c",
    ];
    for input in pathological {
        let parsed = parse(input);
        assert_eq!(parsed.raw, input, "raw must round-trip for {input:?}");
    }
}
