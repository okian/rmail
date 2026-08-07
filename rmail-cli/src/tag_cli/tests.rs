//! Pure-logic unit tests for [`parse_target`] — the one piece of decision
//! logic in this file that does not need a live daemon. End-to-end coverage
//! (the compiled `mail tag`/`mail untag`/`mail tags` against a real daemon)
//! is left as a follow-up — see `rmail-cli/src/search_cli/tests.rs`'s own
//! doc comment for why this crate's bin-only shape makes that a separate
//! `rmail-cli/tests/` binary-exec suite rather than something addable here.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

#[test]
fn a_bare_integer_parses_as_a_message_target() {
    let ParsedTarget::Direct(target) = parse_target("42", true).unwrap() else {
        panic!("expected a direct target");
    };
    assert_eq!(target.of, Some(target::Of::MessageId(42)));
}

#[test]
fn a_thread_prefixed_target_parses_as_a_thread_target() {
    let ParsedTarget::Direct(target) = parse_target("thread:7", true).unwrap() else {
        panic!("expected a direct target");
    };
    assert_eq!(target.of, Some(target::Of::ThreadId(7)));
}

#[test]
fn a_thread_target_with_a_non_numeric_id_is_an_error() {
    let err = parse_target("thread:abc", true).expect_err("a non-numeric thread id must fail");
    assert!(err.to_string().contains("thread:abc"));
}

#[test]
fn a_non_numeric_non_prefixed_target_is_an_error() {
    let err = parse_target("not-a-number", true).expect_err("must fail to parse as anything");
    assert!(err.to_string().contains("not-a-number"));
}

#[test]
fn a_search_target_parses_as_bulk_when_allowed() {
    let ParsedTarget::Bulk(query) = parse_target("search:from:alice", true).unwrap() else {
        panic!("expected a bulk target");
    };
    assert_eq!(query, "from:alice");
}

#[test]
fn a_search_target_is_rejected_when_bulk_is_not_allowed() {
    // `mail untag` has no bulk form on the wire (`RemoveTag` takes a single
    // `Target`, not a selector) -- see the module docs.
    let err = parse_target("search:from:alice", false)
        .expect_err("untag must reject a bulk-shaped target");
    assert!(err.to_string().contains("bulk form"));
}

#[tokio::test]
async fn tag_rejects_a_bulk_target_and_points_at_tag_bulk_without_touching_the_network() {
    // `mail tag search:"…" <tag>` has no `--account`, so it fails fast
    // rather than guessing one -- see `tag`'s own doc comment. Calling the
    // real `tag()` entry point (rather than re-deriving its logic here) is
    // safe with a socket path that cannot possibly be dialed *because* the
    // bulk-rejection branch returns before `client(socket)` is ever called
    // — this test would hang instead of failing fast if that ordering ever
    // regressed.
    let args = TagArgs {
        target: "search:from:stripe".to_owned(),
        tags: vec!["finance/receipt".to_owned()],
    };
    let err = tag(Path::new("/nonexistent/rmail-test.sock"), args)
        .await
        .expect_err("a bulk target must be rejected with guidance, not dialed");
    assert!(err.to_string().contains("mail tag-bulk"));
}
