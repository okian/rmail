//! Fixtures per kind, the tokenize/rehydrate round trip (reordered and
//! mangled echoes), and the single test that matters most: that no raw PII
//! ever reaches the literal bytes a `Provider` puts on the wire.

use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::ai::provider::{ClaudeProvider, Provider};
use crate::config::{AiConfig, AiRetry};

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn privacy() -> AiPrivacy {
    AiPrivacy::default()
}

/// The redacted content of a single-message request, panicking (via
/// `unreachable!`, not `panic!` — see `provider/tests.rs` for why) if it
/// short-circuited to `redacted_skip` instead.
fn redacted_content(text: &str) -> String {
    redacted_with_tokens(text).0
}

fn redacted_with_tokens(text: &str) -> (String, TokenMap) {
    let request = ChatRequest::new("claude-haiku-4-5", 100).user(text);
    match guard(&request, &privacy()) {
        GuardedRequest::Redacted {
            request, tokens, ..
        } => (request.messages[0].content.clone(), tokens),
        GuardedRequest::RedactedSkip => {
            unreachable!("expected {text:?} to survive redaction with content left over")
        }
    }
}

/// The kinds found in some text under the default privacy config, for terse
/// assertions — mirrors `index::entities::tests::kinds`.
fn kinds_found(text: &str) -> Vec<RedactionKind> {
    scan(text, &enabled_kinds(&privacy()))
        .into_iter()
        .map(|h| h.kind)
        .collect()
}

// ---------------------------------------------------------------------------
// Reuse of index::entities (email, phone, iban)
// ---------------------------------------------------------------------------

#[test]
fn emails_are_tokenized_via_entity_extraction() {
    let content = redacted_content("write to Ada@Example.COM about it");
    assert_eq!(content, "write to ⟦EMAIL_1⟧ about it");
}

#[test]
fn phones_are_tokenized_via_entity_extraction() {
    let content = redacted_content("call +1 555-010-1234 today");
    assert_eq!(content, "call ⟦PHONE_1⟧ today");
}

#[test]
fn ibans_are_tokenized_via_entity_extraction() {
    let content = redacted_content("transfer to GB82 WEST 1234 5698 7654 32 please");
    assert_eq!(content, "transfer to ⟦IBAN_1⟧ please");
}

#[test]
fn a_bad_iban_checksum_is_not_claimed() {
    // Same discipline `index::entities` applies: one digit changed and the
    // checksum fails, so nothing is claimed.
    assert!(!kinds_found("GB82 WEST 1234 5698 7654 33").contains(&RedactionKind::Iban));
}

#[test]
fn urls_amounts_dates_and_references_are_never_redacted() {
    // Not PII, and reusing `index::entities` for them would redact useful,
    // harmless context out of every request for no privacy benefit.
    let content = redacted_content(
        "see https://example.com/orders/42, total £1,299.00, due 2024-03-01, order REF-1234",
    );
    assert_eq!(
        content,
        "see https://example.com/orders/42, total £1,299.00, due 2024-03-01, order REF-1234"
    );
}

#[test]
fn an_email_embedded_in_a_url_is_tokenized_as_a_whole() {
    // `index::entities::scan` resolves the URL-vs-email overlap in the
    // URL's favor (see `an_email_inside_a_url_is_only_a_url` in
    // `index::entities::tests`), so this module never sees a shadowed
    // `EntityKind::Email` mention for an address embedded in a link — the
    // single most common shape being an unsubscribe/manage-preferences URL
    // with the recipient's own address in the path or query. Without the
    // `url_embeds_email` check in `scan`, the raw address would go out on
    // the wire untouched inside an otherwise-ordinary-looking link.
    let content =
        redacted_content("manage your preferences: https://x.com/u/jane.doe@example.com/prefs");
    assert_eq!(content, "manage your preferences: ⟦EMAIL_1⟧");
    assert!(!content.contains("jane.doe@example.com"));
}

#[test]
fn a_percent_encoded_address_in_a_url_is_tokenized_too() {
    let content = redacted_content("unsubscribe: https://x.com/u?e=jane.doe%40example.com");
    assert_eq!(content, "unsubscribe: ⟦EMAIL_1⟧");
}

#[test]
fn a_url_with_no_embedded_address_is_left_alone() {
    // The whole-URL tokenization in `an_email_embedded_in_a_url_is_tokenized_as_a_whole`
    // must not fire on an ordinary link just because it is a link.
    let content = redacted_content("see https://example.com/orders/42 for details");
    assert_eq!(content, "see https://example.com/orders/42 for details");
}

// ---------------------------------------------------------------------------
// Card: Luhn
// ---------------------------------------------------------------------------

#[test]
fn a_luhn_valid_card_number_is_tokenized() {
    let content = redacted_content("my card is 4111 1111 1111 1111 ok");
    assert_eq!(content, "my card is ⟦CARD_1⟧ ok");
}

#[test]
fn a_luhn_invalid_digit_run_is_not_a_card() {
    // Last digit changed from the valid fixture above: fails the checksum.
    assert!(!kinds_found("my card is 4111 1111 1111 1112 ok").contains(&RedactionKind::Card));
}

#[test]
fn luhn_rejects_an_all_zero_run() {
    assert!(!luhn_valid("0000000000000000"));
}

#[test]
fn a_non_breaking_space_grouped_card_is_still_tokenized() {
    // U+00A0: what a pasted web page's card-number formatting is full of,
    // and visually indistinguishable from a plain space in a mail client.
    let content = redacted_content("my card is 4111\u{a0}1111\u{a0}1111\u{a0}1111 ok");
    assert_eq!(content, "my card is ⟦CARD_1⟧ ok");
}

// ---------------------------------------------------------------------------
// SSN: issued-range validity
// ---------------------------------------------------------------------------

#[test]
fn a_plausible_ssn_is_tokenized() {
    let content = redacted_content("ssn 123-45-6789 on file");
    assert_eq!(content, "ssn ⟦SSN_1⟧ on file");
}

#[test]
fn an_ssn_shaped_number_with_an_unissued_area_is_not_claimed() {
    // Area 666 was never issued by the SSA.
    assert!(!kinds_found("ssn 666-12-3456 on file").contains(&RedactionKind::Ssn));
    // Area 000 and area >= 900 are likewise never issued.
    assert!(!kinds_found("000-12-3456").contains(&RedactionKind::Ssn));
    assert!(!kinds_found("900-12-3456").contains(&RedactionKind::Ssn));
}

// ---------------------------------------------------------------------------
// Secrets
// ---------------------------------------------------------------------------

#[test]
fn an_anthropic_style_key_is_tokenized() {
    let content = redacted_content("key: sk-ant-abcdefghijklmnopqrstuvwxyz123456 please rotate");
    assert_eq!(content, "key: ⟦SECRET_1⟧ please rotate");
}

#[test]
fn an_aws_access_key_id_is_tokenized() {
    let content = redacted_content("access key AKIAIOSFODNN7EXAMPLE is exposed");
    assert_eq!(content, "access key ⟦SECRET_1⟧ is exposed");
}

#[test]
fn a_jwt_is_tokenized() {
    let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.\
               dozjgNryP4J3jVmNHl0w5N_XgL0n3I9PlFUP0THsR8U";
    let content = redacted_content(&format!("token: {jwt}"));
    assert_eq!(content, "token: ⟦SECRET_1⟧");
}

#[test]
fn a_labeled_secret_redacts_only_the_value_not_the_label() {
    let content = redacted_content("api_key: abcdef123456 is in the .env file");
    assert_eq!(content, "api_key: ⟦SECRET_1⟧ is in the .env file");
}

// ---------------------------------------------------------------------------
// OTP: anchored on a trigger word
// ---------------------------------------------------------------------------

#[test]
fn a_verification_code_is_tokenized() {
    let content = redacted_content("your verification code is 483920, expires soon");
    assert_eq!(content, "your verification code is ⟦OTP_1⟧, expires soon");
}

#[test]
fn a_bare_digit_run_with_no_trigger_word_is_not_an_otp() {
    // Otherwise every order count, year and partial phone number in a
    // mailbox becomes a false "one-time code".
    assert!(!kinds_found("we shipped 483920 units").contains(&RedactionKind::Otp));
}

#[test]
fn a_linking_verb_and_a_colon_together_still_match() {
    // The earlier connector treated "is" and ":" as alternatives — it could
    // consume one or the other but never both, so this ordinary phrasing
    // (both at once) did not match.
    let content = redacted_content("Your verification code is: 483920");
    assert_eq!(content, "Your verification code is: ⟦OTP_1⟧");
}

#[test]
fn a_bare_code_or_pin_trigger_is_enough() {
    assert_eq!(
        redacted_content("Your code is 123456"),
        "Your code is ⟦OTP_1⟧"
    );
    assert_eq!(redacted_content("Your PIN is 4821"), "Your PIN is ⟦OTP_1⟧");
    assert_eq!(
        redacted_content("Your PIN code is 4821"),
        "Your PIN code is ⟦OTP_1⟧"
    );
}

#[test]
fn a_markdown_bolded_code_is_still_recognized() {
    // What an HTML-derived body's plain-text extraction commonly leaves a
    // "highlighted" code looking like. The token span covers only the
    // digits — the surrounding `**` are ordinary punctuation, not part of
    // the code — so they remain around the token rather than being
    // swallowed by it.
    let content = redacted_content("Your code is **483920**");
    assert_eq!(content, "Your code is **⟦OTP_1⟧**");
}

// ---------------------------------------------------------------------------
// Address
// ---------------------------------------------------------------------------

#[test]
fn a_street_address_is_tokenized() {
    let content =
        redacted_content("please ship to 123 Main Street, Springfield, IL 62704 by Friday");
    assert_eq!(content, "please ship to ⟦ADDRESS_1⟧ by Friday");
}

#[test]
fn a_street_suffix_word_must_end_where_the_suffix_ends() {
    // Regression: the street suffix alternation originally had no trailing
    // `\b`, so it matched the *prefix* of an ordinary word — "St" inside
    // "Steps", "Pl" inside "Places", "Dr" inside "Drinks" — and truncated
    // whatever followed. None of these ordinary marketing/subject-line
    // phrases contain a postal address.
    for text in [
        "5 Easy Steps to save money",
        "3 Easy Ways to get started",
        "10 Great Places to visit",
        "2 Large Drinks included",
    ] {
        assert_eq!(
            redacted_content(text),
            text,
            "{text:?} was corrupted by a mid-word address match"
        );
    }
}

#[test]
fn a_zip_code_must_end_where_the_zip_ends() {
    // Regression: the ZIP alternative had no trailing `\b` either, so
    // "IL 627041" matched "62704" and left a dangling "1" — the trailing
    // city/state/zip group simply does not participate when it cannot
    // match cleanly, and the address ends at the street instead.
    let content = redacted_content("ship to 123 Main Street, Springfield, IL 627041");
    assert_eq!(content, "ship to ⟦ADDRESS_1⟧, Springfield, IL 627041");
}

// ---------------------------------------------------------------------------
// Names: salutation, sign-off, display name (bare and quoted), self-intro
// ---------------------------------------------------------------------------

#[test]
fn a_salutation_name_is_tokenized() {
    let content = redacted_content("Dear Jane Doe, thanks for reaching out");
    assert_eq!(content, "Dear ⟦NAME_1⟧, thanks for reaching out");
}

#[test]
fn a_sign_off_name_is_tokenized() {
    let content = redacted_content("Thanks for your help.\n\nRegards,\nJohn Smith");
    assert_eq!(content, "Thanks for your help.\n\nRegards,\n⟦NAME_1⟧");
}

#[test]
fn a_same_line_sign_off_name_is_tokenized() {
    // "Thanks, Jane" / "Best, John" with no newline at all — ubiquitous in
    // short replies, and the original pattern required `[\r\n]+` and so
    // missed all of them. Only the captured name is replaced — "Thanks, "
    // is the trigger phrase, not part of the name — the same
    // span-covers-the-identifier-not-the-sentence rule every other detector
    // in this module follows.
    assert_eq!(redacted_content("Thanks, Jane!"), "Thanks, ⟦NAME_1⟧!");
    assert_eq!(redacted_content("Best, John"), "Best, ⟦NAME_1⟧");
}

#[test]
fn a_bare_signoff_word_with_no_comma_and_no_newline_is_not_a_name() {
    // The same-line allowance is gated on a comma specifically: "Best" (or
    // any other sign-off word) followed directly by a capitalized word with
    // only a space between must not fire, or ordinary phrases like "Best
    // New York pizza" become false positives.
    assert!(!kinds_found("Best New York pizza recipe").contains(&RedactionKind::Name));
    assert!(!kinds_found("Best wishes for the trip").contains(&RedactionKind::Name));
}

#[test]
fn an_email_display_name_is_tokenized_separately_from_the_address() {
    let content = redacted_content("please contact John Doe <john.doe@example.com> for details");
    assert_eq!(
        content, "please contact ⟦NAME_1⟧ <⟦EMAIL_1⟧> for details",
        "the name and the address are two different kinds of PII and get two tokens"
    );
}

#[test]
fn a_quoted_email_display_name_is_tokenized() {
    // The RFC 5322 quoted form is required whenever a display name contains
    // a comma or period — the common case for "Last, First" or initials —
    // not an exotic one. The captured span is the name only, not the
    // surrounding quote marks — the same span-covers-the-identifier rule
    // as everywhere else in this module — so the quotes remain.
    let content =
        redacted_content("from \"Doe, Jane\" <jane.doe@example.com> — please reply directly");
    assert_eq!(
        content,
        "from \"⟦NAME_1⟧\" <⟦EMAIL_1⟧> — please reply directly"
    );
}

#[test]
fn a_self_introduction_name_is_tokenized() {
    let content = redacted_content("Hi, my name is Sarah Connor and I'm calling about the order");
    assert_eq!(
        content,
        "Hi, my name is ⟦NAME_1⟧ and I'm calling about the order"
    );
}

// ---------------------------------------------------------------------------
// Dedup, overlap, and per-kind counters
// ---------------------------------------------------------------------------

#[test]
fn repeated_mentions_of_the_same_value_share_one_token() {
    let (content, tokens) =
        redacted_with_tokens("contact john@example.com or, failing that, john@example.com again");
    assert_eq!(
        content,
        "contact ⟦EMAIL_1⟧ or, failing that, ⟦EMAIL_1⟧ again"
    );
    assert_eq!(tokens.len(), 1);
}

#[test]
fn distinct_values_of_the_same_kind_get_their_own_counter() {
    let content = redacted_content("cc alice@example.com and bob@example.com");
    assert_eq!(content, "cc ⟦EMAIL_1⟧ and ⟦EMAIL_2⟧");
}

#[test]
fn overlapping_hits_produce_one_token_not_two() {
    // The digit run is both a labeled "secret" and, independently,
    // Luhn-valid — the illustrative case from the module docs. Whichever
    // detector wins, the output must have exactly one token covering the
    // span, never two overlapping ones that would corrupt the text.
    let content = redacted_content("api_key: 4111111111111111");
    assert_eq!(content.matches(TOKEN_OPEN).count(), 1, "{content:?}");
    assert!(!content.contains("4111111111111111"));
}

#[test]
fn resolve_overlaps_unions_a_partial_overlap_rather_than_dropping_it() {
    // Hand-built rather than driven off real detector output: the point is
    // to prove the algorithm itself closes the gap a "drop the loser" rule
    // would leave — if the loser's span reaches past the winner's, its tail
    // must not go out raw — independent of whether today's detectors
    // happen to produce a partial overlap themselves.
    let hits = vec![
        Hit {
            kind: RedactionKind::Name,
            start: 0,
            end: 5,
            value: "Alice".to_owned(),
            key: "alice".to_owned(),
        },
        Hit {
            kind: RedactionKind::Secret,
            start: 3,
            end: 10,
            value: "ceSecret".to_owned(),
            key: "cesecret".to_owned(),
        },
    ];
    let resolved = resolve_overlaps(hits);
    assert_eq!(resolved.len(), 1, "{resolved:?}");
    assert_eq!(resolved[0].start, 0);
    assert_eq!(
        resolved[0].end, 10,
        "the union must cover both hits' spans, not just the winner's own"
    );
    assert_eq!(
        resolved[0].kind,
        RedactionKind::Name,
        "identity comes from the earlier-starting hit"
    );
}

// ---------------------------------------------------------------------------
// The system prompt is never scanned
// ---------------------------------------------------------------------------

#[test]
fn the_system_prompt_passes_through_unscanned() {
    let request = ChatRequest::new("claude-haiku-4-5", 10)
        .system("support contact: ops@example.com")
        .user("hello there, nothing sensitive");
    match guard(&request, &privacy()) {
        GuardedRequest::Redacted { request, .. } => {
            assert_eq!(
                request.system.as_deref(),
                Some("support contact: ops@example.com"),
                "the system prompt must survive byte-identical for prompt caching"
            );
        }
        GuardedRequest::RedactedSkip => unreachable!("the user message has real content"),
    }
}

// ---------------------------------------------------------------------------
// Config: the on/off switch and the five configurable kinds
// ---------------------------------------------------------------------------

#[test]
fn redact_false_passes_every_message_through_unchanged() {
    let privacy = AiPrivacy {
        redact: false,
        ..AiPrivacy::default()
    };
    let request = ChatRequest::new("m", 10).user("john@example.com, card 4111 1111 1111 1111");
    match guard(&request, &privacy) {
        GuardedRequest::Redacted {
            request, tokens, ..
        } => {
            assert_eq!(
                request.messages[0].content,
                "john@example.com, card 4111 1111 1111 1111"
            );
            assert!(tokens.is_empty());
        }
        GuardedRequest::RedactedSkip => unreachable!("redaction is off; nothing should skip"),
    }
}

#[test]
fn redact_patterns_narrows_the_five_configurable_kinds() {
    let privacy = AiPrivacy {
        redact_patterns: vec!["ssn".to_owned()],
        ..AiPrivacy::default()
    };
    let request = ChatRequest::new("m", 10).user(
        "card 4111 1111 1111 1111, iban GB82 WEST 1234 5698 7654 32, ssn 123-45-6789, \
         reach jane@example.com",
    );
    let content = match guard(&request, &privacy) {
        GuardedRequest::Redacted { request, .. } => request.messages[0].content.clone(),
        GuardedRequest::RedactedSkip => unreachable!(),
    };
    assert!(content.contains("SSN_1"), "ssn stays enabled: {content}");
    assert!(
        content.contains("EMAIL_1"),
        "email is always-on baseline: {content}"
    );
    assert!(
        content.contains("4111 1111 1111 1111"),
        "card redaction was narrowed off, raw text should remain: {content}"
    );
    assert!(
        content.contains("GB82"),
        "iban redaction was narrowed off, raw text should remain: {content}"
    );
}

#[test]
fn an_unknown_redact_pattern_name_is_ignored_not_rejected() {
    let privacy = AiPrivacy {
        redact_patterns: vec!["quantum_flux_capacitor".to_owned()],
        ..AiPrivacy::default()
    };
    // Must not panic, and the always-on baseline still applies.
    let content = redacted_with_tokens_for("email john@example.com", &privacy);
    assert_eq!(content, "email ⟦EMAIL_1⟧");
}

fn redacted_with_tokens_for(text: &str, privacy: &AiPrivacy) -> String {
    let request = ChatRequest::new("m", 10).user(text);
    match guard(&request, privacy) {
        GuardedRequest::Redacted { request, .. } => request.messages[0].content.clone(),
        GuardedRequest::RedactedSkip => unreachable!(),
    }
}

// ---------------------------------------------------------------------------
// redacted_skip
// ---------------------------------------------------------------------------

#[test]
fn empty_after_redaction_short_circuits_to_redacted_skip() {
    let request = ChatRequest::new("m", 10).user("john@example.com");
    assert!(matches!(
        guard(&request, &privacy()),
        GuardedRequest::RedactedSkip
    ));
}

#[test]
fn content_beyond_its_pii_is_not_skipped() {
    let request = ChatRequest::new("m", 10).user("Please review, contact john@example.com");
    assert!(matches!(
        guard(&request, &privacy()),
        GuardedRequest::Redacted { .. }
    ));
}

#[test]
fn a_request_skips_only_when_every_message_is_empty_after_redaction() {
    let request = ChatRequest::new("m", 10)
        .user("john@example.com")
        .assistant("Sure, I can help with that request today")
        .user("+1 555-010-1234");
    // The assistant turn has real content, so the request as a whole is not
    // empty even though both user turns are pure PII. This asserts every
    // message's actual redacted content, not just the enum variant — a bug
    // that redacted only `messages[0]` (leaving raw PII in a later message)
    // would still pass a variant-only check.
    match guard(&request, &privacy()) {
        GuardedRequest::Redacted { request, .. } => {
            assert_eq!(request.messages[0].content, "⟦EMAIL_1⟧");
            assert_eq!(
                request.messages[1].content,
                "Sure, I can help with that request today"
            );
            assert_eq!(request.messages[2].content, "⟦PHONE_1⟧");
        }
        GuardedRequest::RedactedSkip => unreachable!("the assistant turn has real content"),
    }
}

#[test]
fn the_same_value_across_two_messages_shares_one_token() {
    // The module docs promise dedup is request-wide, not per-message — the
    // dedup map in `guard` is built once and threaded through every
    // message's `apply` call, but nothing short of driving two messages
    // through it actually proves that.
    let request = ChatRequest::new("m", 10)
        .user("my address is 123 Main Street, Springfield, IL 62704")
        .assistant("Got it, I'll note that.")
        .user("just to confirm, that's 123 Main Street, Springfield, IL 62704");
    match guard(&request, &privacy()) {
        GuardedRequest::Redacted {
            request, tokens, ..
        } => {
            assert_eq!(request.messages[0].content, "my address is ⟦ADDRESS_1⟧");
            assert_eq!(
                request.messages[2].content,
                "just to confirm, that's ⟦ADDRESS_1⟧"
            );
            assert_eq!(
                tokens.len(),
                1,
                "one address, mentioned twice, is one token"
            );
        }
        GuardedRequest::RedactedSkip => unreachable!("the assistant turn has real content"),
    }
}

// ---------------------------------------------------------------------------
// redact_preview
// ---------------------------------------------------------------------------

#[test]
fn redact_preview_matches_what_guard_would_send() {
    let text = "email me at john@example.com";
    let request = ChatRequest::new("m", 10).user(text);
    let via_guard = match guard(&request, &privacy()) {
        GuardedRequest::Redacted { request, .. } => request.messages[0].content.clone(),
        GuardedRequest::RedactedSkip => unreachable!(),
    };
    let via_preview = preview(text, &privacy());
    assert_eq!(via_guard, via_preview.redacted_text);
    assert!(!via_preview.would_skip);
}

#[test]
fn redact_preview_flags_would_skip_for_all_pii_text() {
    let result = preview("john@example.com", &privacy());
    assert!(result.would_skip);
    assert_eq!(result.redacted_text, "⟦EMAIL_1⟧");
}

// ---------------------------------------------------------------------------
// Tokenize / rehydrate round trip
// ---------------------------------------------------------------------------

#[test]
fn tokenize_rehydrate_round_trip() {
    let (content, tokens) = redacted_with_tokens("email john@example.com or call +1 555-010-1234");
    assert_eq!(content, "email ⟦EMAIL_1⟧ or call ⟦PHONE_1⟧");

    let model_response = "You can reach them at ⟦EMAIL_1⟧ or ⟦PHONE_1⟧.";
    let rehydrated = rehydrate(model_response, &tokens);
    assert_eq!(
        rehydrated,
        "You can reach them at john@example.com or +1 555-010-1234."
    );
}

#[test]
fn rehydrate_handles_tokens_echoed_in_a_different_order() {
    let (_, tokens) = redacted_with_tokens("emails: alice@example.com and bob@example.com");
    // The model's answer names bob (EMAIL_2) before alice (EMAIL_1) — the
    // reverse of how they appeared in the outbound message.
    let model_response = "First ⟦EMAIL_2⟧, then ⟦EMAIL_1⟧.";
    let rehydrated = rehydrate(model_response, &tokens);
    assert_eq!(rehydrated, "First bob@example.com, then alice@example.com.");
}

#[test]
fn rehydrate_leaves_a_mangled_token_verbatim_rather_than_guessing() {
    let (_, tokens) = redacted_with_tokens("contact alice@example.com");

    // Missing the closing bracket entirely — not token-shaped, so the
    // pattern never matches it and it passes through untouched.
    let truncated = rehydrate("reach them at ⟦EMAIL_1", &tokens);
    assert_eq!(truncated, "reach them at ⟦EMAIL_1");

    // Token-shaped (matches the `⟦[A-Z]+_[0-9]+⟧` pattern) but not a token
    // this map minted — a typo in the tag. Left exactly as written rather
    // than fuzzy-matched to the closest real token, because a wrong
    // substitution would attribute one person's PII to another.
    let typoed = rehydrate("reach them at ⟦EMAILL_1⟧", &tokens);
    assert_eq!(typoed, "reach them at ⟦EMAILL_1⟧");
}

#[test]
fn rehydrate_is_a_no_op_with_an_empty_token_map() {
    let tokens = TokenMap::default();
    assert_eq!(rehydrate("hello ⟦EMAIL_1⟧", &tokens), "hello ⟦EMAIL_1⟧");
}

#[test]
fn rehydrate_replaces_every_occurrence_of_a_repeated_token() {
    // The docs' claim is "however many times it appears" — nothing else in
    // this file drives a token through more than one occurrence.
    let (_, tokens) = redacted_with_tokens("contact alice@example.com");
    let model_response = "Email ⟦EMAIL_1⟧, or email ⟦EMAIL_1⟧ again if that bounces.";
    let rehydrated = rehydrate(model_response, &tokens);
    assert_eq!(
        rehydrated,
        "Email alice@example.com, or email alice@example.com again if that bounces."
    );
}

#[test]
fn token_map_debug_does_not_print_values() {
    let (_, tokens) = redacted_with_tokens("contact alice@example.com");
    let debug = format!("{tokens:?}");
    assert!(!debug.contains("alice"), "{debug}");
    assert!(debug.contains("1 token"), "{debug}");
}

#[test]
fn guarded_request_debug_does_not_print_message_content() {
    let request = ChatRequest::new("m", 10).user("contact alice@example.com and share more");
    let debug = format!("{:?}", guard(&request, &privacy()));
    assert!(!debug.contains("alice"), "{debug}");
    assert!(!debug.contains("EMAIL_1"), "{debug}");
}

#[test]
fn guarded_request_debug_does_not_print_raw_content_even_with_redaction_disabled() {
    // The scenario the custom `Debug` impl exists for: with `redact =
    // false`, `GuardedRequest::Redacted.request` genuinely holds the raw,
    // unredacted message — the one case a naive derived `Debug` would leak.
    let privacy = AiPrivacy {
        redact: false,
        ..AiPrivacy::default()
    };
    let request = ChatRequest::new("m", 10).user("contact alice@example.com directly");
    let debug = format!("{:?}", guard(&request, &privacy));
    assert!(!debug.contains("alice"), "{debug}");
}

// ---------------------------------------------------------------------------
// Adversarial input: a sender writing a token-shaped string
// ---------------------------------------------------------------------------

#[test]
fn a_preexisting_token_shaped_string_is_neutralized() {
    // A sender who writes the literal text `⟦EMAIL_1⟧` must not have it
    // collide with a token this pass mints, be echoed back and resolved to
    // someone else's real value by `rehydrate`, or be treated as "no
    // content" by `has_residual_content`. The text is still visible to the
    // model — just no longer shaped like this module's own tokens.
    let content =
        redacted_content("here is a token: ⟦EMAIL_1⟧ and my real address is john@example.com");
    assert_eq!(
        content,
        "here is a token: [EMAIL_1] and my real address is ⟦EMAIL_1⟧"
    );
}

#[test]
fn a_forged_token_cannot_game_the_redacted_skip_decision() {
    // Before neutralization, this message's only "content" was a
    // token-shaped string, which `has_residual_content` does not count as
    // real — an attacker-controlled body consisting only of a forged token
    // must not use that to dodge AI processing via `redacted_skip`.
    let request = ChatRequest::new("m", 10).user("⟦EMAIL_1⟧");
    match guard(&request, &privacy()) {
        GuardedRequest::Redacted { request, .. } => {
            assert_eq!(request.messages[0].content, "[EMAIL_1]");
        }
        GuardedRequest::RedactedSkip => {
            unreachable!("a forged token is ordinary text, not empty content")
        }
    }
}

// ---------------------------------------------------------------------------
// Scan budget
// ---------------------------------------------------------------------------

#[test]
fn a_body_past_the_scan_budget_is_truncated_before_it_is_sent() {
    let huge = format!(
        "{} contact leak@example.com",
        "a".repeat(MAX_SCAN_BYTES + 10)
    );
    let content = redacted_content(&huge);
    assert!(content.len() <= MAX_SCAN_BYTES, "{}", content.len());
    assert!(!content.contains("leak@example.com"));
}

#[test]
fn the_scan_budget_truncates_on_a_char_boundary_not_a_byte_offset() {
    // `bounded`'s walk-back-to-a-char-boundary logic is the one place in
    // this module doing arithmetic on a raw byte index. An all-ASCII
    // fixture can never exercise it — every byte offset is already a char
    // boundary. '中' is three UTF-8 bytes and, unlike a symbol, counts as
    // alphanumeric (so the fixture does not also trip `redacted_skip`).
    // `MAX_SCAN_BYTES` (262144) is not a multiple of three — 262144 = 3 ×
    // 87381 + 1 — so the naive cutoff lands one byte inside the 87382nd
    // character rather than on its first byte, forcing the walk-back loop
    // to actually walk back by exactly one character (three bytes) to
    // 262143, the nearest valid boundary. A panic on a bad slice index is
    // the failure this guards against; the exact expected length is the
    // assertion that the walk-back landed where the arithmetic above says
    // it must, not just somewhere safe by luck.
    let huge = "中".repeat(MAX_SCAN_BYTES);
    let content = redacted_content(&huge);
    assert_eq!(content.len(), 262_143);
}

// ---------------------------------------------------------------------------
// Every pattern actually compiles
// ---------------------------------------------------------------------------

#[test]
fn every_pattern_compiles() {
    for (name, pattern) in [
        ("card", CARD.as_ref()),
        ("ssn", SSN.as_ref()),
        ("secret", SECRET.as_ref()),
        ("otp", OTP.as_ref()),
        ("address", ADDRESS.as_ref()),
        ("name", NAME.as_ref()),
        ("token_pattern", TOKEN_PATTERN.as_ref()),
    ] {
        assert!(pattern.is_some(), "the {name} pattern did not compile");
    }
}

// ---------------------------------------------------------------------------
// The single most important test: no raw PII on the wire
// ---------------------------------------------------------------------------

/// A single-shot HTTP server that captures exactly one request's raw JSON
/// body and answers with a scripted response. Real bytes over a loopback
/// socket rather than a mock of the `Provider` trait — the point of the
/// test below is what a `ClaudeProvider::complete` call actually put on the
/// wire, not what the `ChatRequest` struct looked like before it was
/// serialized.
struct WireCapture {
    endpoint: String,
    body: Arc<Mutex<Option<String>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for WireCapture {
    fn drop(&mut self) {
        // A `JoinHandle` does not abort on drop, so without this every test
        // leaves an accept loop and a bound port running for the life of
        // the process — the same reasoning `provider/tests.rs`'s own
        // `Server::drop` gives.
        self.task.abort();
    }
}

impl WireCapture {
    async fn start(response_text: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let body = Arc::new(Mutex::new(None));
        let recorder = Arc::clone(&body);
        let response = canned_response(response_text);
        let task = tokio::spawn(async move {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut raw = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = stream.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    return;
                }
                raw.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&raw).to_string();
                let Some(at) = text.find("\r\n\r\n") else {
                    continue;
                };
                let length = text
                    .lines()
                    .find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.trim()
                            .eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().to_owned())
                    })
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(0);
                if raw.len() < at + 4 + length {
                    continue;
                }
                let body_text = String::from_utf8_lossy(&raw[at + 4..at + 4 + length]).to_string();
                if let Ok(mut recorded) = recorder.lock() {
                    *recorded = Some(body_text);
                }
                let _ = stream.write_all(response.as_bytes()).await;
                let _ = stream.flush().await;
                return;
            }
        });
        Self {
            endpoint: format!("http://{addr}/v1/messages"),
            body,
            task,
        }
    }

    fn captured_body(&self) -> String {
        self.body
            .lock()
            .map(|g| g.clone().unwrap_or_default())
            .unwrap_or_default()
    }
}

fn canned_response(text: &str) -> String {
    let body = serde_json::json!({
        "id": "msg_1",
        "model": "claude-haiku-4-5",
        "content": [{"type": "text", "text": text}],
        "stop_reason": "end_turn",
        "usage": {
            "input_tokens": 10, "output_tokens": 5,
            "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0,
        },
    })
    .to_string();
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n{body}",
        body.len()
    )
}

fn test_provider(server: &WireCapture) -> ClaudeProvider {
    let config = AiConfig {
        api_key_command: "printf test-key".to_owned(),
        retry: AiRetry {
            max_attempts: 1,
            base_delay_ms: 1,
            max_delay_ms: 1,
        },
        ..AiConfig::default()
    };
    ClaudeProvider::new(&config)
        .unwrap()
        .with_endpoint(&server.endpoint)
}

#[tokio::test]
async fn no_raw_pii_reaches_the_outbound_wire_body() {
    let raw = "Hi, my name is Jane Doe. Reach me at jane.doe@example.com or +1 555-010-1234. \
               My card is 4111 1111 1111 1111 and my SSN is 123-45-6789. I live at \
               123 Main Street, Springfield, IL 62704. My API key is \
               sk-ant-abcdefghijklmnopqrstuvwxyz123456.";
    let request = ChatRequest::new("claude-haiku-4-5", 200).user(raw);

    let (guarded, tokens) = match guard(&request, &privacy()) {
        GuardedRequest::Redacted {
            request, tokens, ..
        } => (request, tokens),
        GuardedRequest::RedactedSkip => unreachable!("expected real content to survive"),
    };
    assert_eq!(
        tokens.len(),
        7,
        "expected one token per PII kind present above: {tokens:?}"
    );

    // The "model's answer" echoes every token back in a scrambled order —
    // proving rehydration does not depend on the order a call happened to
    // produce them in.
    let server = WireCapture::start(
        "Summary: contacted via ⟦PHONE_1⟧ and ⟦EMAIL_1⟧ — that's ⟦NAME_1⟧. Card ⟦CARD_1⟧, \
         ssn ⟦SSN_1⟧, address ⟦ADDRESS_1⟧, key ⟦SECRET_1⟧.",
    )
    .await;
    let provider = test_provider(&server);
    let response = provider
        .complete(&guarded, &CancellationToken::new())
        .await
        .unwrap();

    // The one test that matters most: inspect the literal bytes the mock
    // server received, not the `ChatRequest` struct — a bug that redacted
    // the struct but somehow reintroduced raw text during serialization
    // would only show up here.
    let wire_body = server.captured_body();
    assert!(!wire_body.is_empty(), "the server never captured a body");
    for raw_value in [
        "Jane Doe",
        "jane.doe@example.com",
        "555-010-1234",
        "4111 1111 1111 1111",
        "4111111111111111",
        "123-45-6789",
        "123 Main Street",
        "sk-ant-abcdefghijklmnopqrstuvwxyz123456",
    ] {
        assert!(
            !wire_body.contains(raw_value),
            "raw PII {raw_value:?} leaked into the outbound payload: {wire_body}"
        );
    }
    assert!(wire_body.contains("EMAIL_1"), "{wire_body}");
    assert!(wire_body.contains("NAME_1"), "{wire_body}");

    // The response, once rehydrated, has the real values back — even though
    // the model echoed them in a different order than the request used.
    let rehydrated = rehydrate(&response.text, &tokens);
    assert!(rehydrated.contains("jane.doe@example.com"), "{rehydrated}");
    assert!(rehydrated.contains("+1 555-010-1234"), "{rehydrated}");
    assert!(rehydrated.contains("Jane Doe"), "{rehydrated}");
    assert!(rehydrated.contains("4111 1111 1111 1111"), "{rehydrated}");
    assert!(rehydrated.contains("123-45-6789"), "{rehydrated}");
    assert!(rehydrated.contains("123 Main Street"), "{rehydrated}");
    assert!(
        rehydrated.contains("sk-ant-abcdefghijklmnopqrstuvwxyz123456"),
        "{rehydrated}"
    );
    assert!(
        !rehydrated.contains(TOKEN_OPEN),
        "every token in the response should have resolved: {rehydrated}"
    );
}
