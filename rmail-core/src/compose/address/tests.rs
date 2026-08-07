//! What [`super::Mailbox`] owes: it is the single gate through which an
//! address reaches a header or an SMTP envelope, so anything it accepts is
//! something the renderer will happily emit unescaped.

use super::*;
use crate::error::ErrorReason;

fn reason(input: &str) -> ErrorReason {
    Mailbox::parse(input)
        .expect_err(&format!("{input:?} must not parse"))
        .reason()
}

#[test]
fn a_bare_addr_spec_parses_with_no_display_name() {
    let m = Mailbox::parse("alice@example.com").unwrap();
    assert_eq!(m.address(), "alice@example.com");
    assert_eq!(m.display_name(), None);
    assert_eq!(m.domain(), "example.com");
}

#[test]
fn angle_bracket_form_separates_name_from_address() {
    let m = Mailbox::parse("Alice Example <alice@example.com>").unwrap();
    assert_eq!(m.address(), "alice@example.com");
    assert_eq!(m.display_name(), Some("Alice Example"));
}

#[test]
fn a_quoted_display_name_is_unquoted_and_unescaped() {
    let m = Mailbox::parse(r#""Doe, Jane \"JD\"" <jane@example.com>"#).unwrap();
    assert_eq!(m.display_name(), Some(r#"Doe, Jane "JD""#));
    assert_eq!(m.address(), "jane@example.com");
}

#[test]
fn the_last_angle_bracket_run_is_the_address() {
    // A `<` inside a quoted display name must not be mistaken for the start
    // of the addr-spec — `find` would take the wrong one here.
    let m = Mailbox::parse(r#""a <b" <c@example.com>"#).unwrap();
    assert_eq!(m.address(), "c@example.com");
    assert_eq!(m.display_name(), Some("a <b"));
}

#[test]
fn surrounding_whitespace_is_trimmed_everywhere() {
    let m = Mailbox::parse("   Alice   <  alice@example.com  >  ").unwrap();
    assert_eq!(m.address(), "alice@example.com");
    assert_eq!(m.display_name(), Some("Alice"));
}

#[test]
fn a_non_ascii_display_name_is_kept_verbatim() {
    // Encoding is the renderer's job; the stored form is always decoded.
    let m = Mailbox::new("cafe@example.com", Some("Café Ünicode")).unwrap();
    assert_eq!(m.display_name(), Some("Café Ünicode"));
}

#[test]
fn an_empty_display_name_becomes_none() {
    assert_eq!(
        Mailbox::new("a@example.com", Some("   "))
            .unwrap()
            .display_name(),
        None
    );
    assert_eq!(
        Mailbox::parse("   <a@example.com>").unwrap().display_name(),
        None
    );
}

#[test]
fn every_malformed_address_is_invalid_argument() {
    for input in [
        "",
        "   ",
        "no-at-sign",
        "two@at@signs.com",
        "@example.com",
        "local@",
        // Header injection: the whole reason this type exists.
        "alice@example.com>\r\nBcc: victim@example.com",
        "alice\r\n@example.com",
        // Non-ASCII addr-spec (SMTPUTF8) — see the module docs.
        "älice@example.com",
        "alice@exämple.com",
        // Domain shapes that do not resolve and would weaken the character
        // rules if allowed.
        "a@.example.com",
        "a@example.com.",
        "a@exa..mple.com",
        "a@-example.com",
        "a@example-.com",
        "a@exa mple.com",
        "a@[192.0.2.1]",
        // Specials that would end the token inside a header.
        "a,b@example.com",
        "a;b@example.com",
        r#""odd name"@example.com"#,
        // Unbalanced brackets.
        "Alice <alice@example.com",
        "Alice >alice@example.com<",
    ] {
        assert_eq!(
            reason(input),
            ErrorReason::InvalidArgument,
            "{input:?} must be rejected as INVALID_ARGUMENT"
        );
    }
}

#[test]
fn oversized_parts_are_rejected() {
    let long_local = format!("{}@example.com", "a".repeat(MAX_LOCAL + 1));
    assert_eq!(reason(&long_local), ErrorReason::InvalidArgument);

    // 255 is the cap; build a domain of labels that exceeds it.
    let long_domain = format!("a@{}.com", "b".repeat(MAX_DOMAIN));
    assert_eq!(reason(&long_domain), ErrorReason::InvalidArgument);

    let long_name = "x".repeat(MAX_DISPLAY_NAME + 1);
    let err = Mailbox::new("a@example.com", Some(&long_name)).unwrap_err();
    assert_eq!(err.reason(), ErrorReason::InvalidArgument);
}

#[test]
fn a_display_name_with_a_control_character_is_rejected_not_stripped() {
    // Silently repairing it would hide either a caller bug or an injection
    // attempt; both are worth surfacing.
    let err = Mailbox::new("a@example.com", Some("Alice\r\nBcc: victim@example.com")).unwrap_err();
    assert_eq!(err.reason(), ErrorReason::InvalidArgument);
}

#[test]
fn addresses_at_exactly_the_limits_are_accepted() {
    let local = "a".repeat(MAX_LOCAL);
    let m = Mailbox::parse(&format!("{local}@example.com")).unwrap();
    assert_eq!(m.address().len(), MAX_LOCAL + "@example.com".len());
}

#[test]
fn plus_addressing_and_the_usual_local_part_punctuation_survive() {
    for addr in [
        "alice+rmail@example.com",
        "alice.b_c@example.co.uk",
        "a!#$%&'*/=?^`{|}~-@example.com",
        "user@localhost",
    ] {
        assert_eq!(Mailbox::parse(addr).unwrap().address(), addr);
    }
}
