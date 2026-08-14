use super::*;
use crate::error::ErrorReason;

fn mailbox_scope(mailbox_id: i64) -> PageScope {
    PageScope::new("rmail.v1.MailService/List").field("mailbox_id", mailbox_id)
}

#[test]
fn a_token_round_trips_within_its_own_scope() {
    let scope = mailbox_scope(3);
    let token = encode(&scope, Cursor::new(1_700_000_000, 42));
    assert_eq!(
        decode(&token, &scope).unwrap(),
        Some(Cursor::new(1_700_000_000, 42))
    );
}

#[test]
fn an_empty_token_means_the_first_page() {
    assert_eq!(decode("", &mailbox_scope(3)).unwrap(), None);
}

#[test]
fn a_token_cannot_be_re_aimed_at_another_mailbox() {
    // The whole point: a cursor minted while listing mailbox 3 must not
    // resume a listing of mailbox 9.
    let token = encode(&mailbox_scope(3), Cursor::new(100, 1));
    let err = decode(&token, &mailbox_scope(9)).expect_err("cross-mailbox resume must be refused");
    assert_eq!(err.reason(), ErrorReason::InvalidArgument);
}

#[test]
fn a_token_cannot_be_re_aimed_at_another_account() {
    let mine = PageScope::new("rmail.v1.SendSchedulerService/ListOutbox")
        .opt_field("account_id", Some(1))
        .opt_field("state", None::<i32>);
    let theirs = PageScope::new("rmail.v1.SendSchedulerService/ListOutbox")
        .opt_field("account_id", Some(2))
        .opt_field("state", None::<i32>);
    let token = encode(&mine, Cursor::new(100, 1));
    assert_eq!(
        decode(&token, &theirs)
            .expect_err("cross-account resume must be refused")
            .reason(),
        ErrorReason::InvalidArgument
    );
}

#[test]
fn a_token_cannot_be_re_aimed_at_another_method() {
    let list = PageScope::new("rmail.v1.MailService/List").field("mailbox_id", 3);
    let drafts = PageScope::new("rmail.v1.ComposeService/ListDrafts").field("mailbox_id", 3);
    let token = encode(&list, Cursor::new(100, 1));
    assert!(decode(&token, &drafts).is_err());
}

#[test]
fn an_absent_filter_is_not_the_same_scope_as_a_present_one() {
    // "every account" and "account 0" select different rows; a token must not
    // cross between them just because the value formats the same.
    let every = PageScope::new("m").opt_field("account_id", None::<i64>);
    let zero = PageScope::new("m").opt_field("account_id", Some(0));
    let token = encode(&every, Cursor::new(1, 1));
    assert!(decode(&token, &zero).is_err());
}

#[test]
fn field_names_are_hashed_so_values_cannot_swap() {
    let ab = PageScope::new("m").field("a", 1).field("b", 2);
    let ba = PageScope::new("m").field("a", 2).field("b", 1);
    let token = encode(&ab, Cursor::new(1, 1));
    assert!(decode(&token, &ba).is_err());
}

#[test]
fn field_framing_is_unambiguous() {
    // Without length prefixes, ("ab", "c") and ("a", "bc") would hash alike.
    let left = PageScope::new("m").field("ab", "c");
    let right = PageScope::new("m").field("a", "bc");
    let token = encode(&left, Cursor::new(1, 1));
    assert!(decode(&token, &right).is_err());
}

#[test]
fn a_malformed_token_is_invalid_argument_not_a_panic() {
    let scope = mailbox_scope(3);
    for bad in [
        "not base64 at all !!",
        "AAAA",
        // Valid base64url, right length, wrong version byte.
        &BASE64.encode([9u8; TOKEN_BYTES]),
        // Right version, one byte short.
        &BASE64.encode({
            let mut b = vec![VERSION];
            b.extend_from_slice(&[0u8; TOKEN_BYTES - 2]);
            b
        }),
    ] {
        let err = decode(bad, &scope).expect_err("malformed token must be refused");
        assert_eq!(err.reason(), ErrorReason::InvalidArgument, "for {bad:?}");
    }
}

#[test]
fn a_token_does_not_leak_its_cursor_in_the_error_message() {
    let err = decode(
        &encode(&mailbox_scope(3), Cursor::new(7, 7)),
        &mailbox_scope(9),
    )
    .expect_err("must refuse");
    let message = err.to_string();
    assert!(
        !message.contains("mailbox") || !message.contains('7'),
        "error should not narrate the token's contents: {message}"
    );
}

#[test]
fn page_size_is_capped_and_defaulted() {
    assert_eq!(clamp_page_size(0, 100), 100);
    assert_eq!(clamp_page_size(-5, 100), 100);
    assert_eq!(clamp_page_size(10, 100), 10);
    assert_eq!(clamp_page_size(MAX_PAGE_SIZE, 100), MAX_PAGE_SIZE);
    assert_eq!(clamp_page_size(MAX_PAGE_SIZE + 1, 100), MAX_PAGE_SIZE);
    assert_eq!(clamp_page_size(i64::MAX, 100), MAX_PAGE_SIZE);
    // A server default above the cap must not be a way around it.
    assert_eq!(clamp_page_size(0, 10_000), MAX_PAGE_SIZE);
}

#[test]
fn a_short_page_yields_no_next_token() {
    let scope = mailbox_scope(3);
    assert_eq!(next_token(&scope, Some(Cursor::new(1, 1)), false), None);
    assert_eq!(next_token(&scope, None, true), None);
    assert!(next_token(&scope, Some(Cursor::new(1, 1)), true).is_some());
}

#[test]
fn negative_and_extreme_cursor_values_survive() {
    // `sort` is a raw i64 column value; nothing constrains it to be positive,
    // and a two's-complement round trip is exactly where a hand-rolled
    // encoding goes wrong.
    let scope = mailbox_scope(1);
    for cursor in [
        Cursor::new(i64::MIN, i64::MIN),
        Cursor::new(-1, -1),
        Cursor::new(0, 0),
        Cursor::new(i64::MAX, i64::MAX),
    ] {
        let token = encode(&scope, cursor);
        assert_eq!(decode(&token, &scope).unwrap(), Some(cursor));
    }
}

#[test]
fn a_token_is_url_safe_and_unpadded() {
    // It travels in a gRPC metadata header for the streaming list, where a
    // non-ASCII byte would have to be base64'd a second time by the transport.
    let token = encode(&mailbox_scope(3), Cursor::new(i64::MIN, i64::MAX));
    assert!(
        token
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
        "not url-safe: {token}"
    );
}
