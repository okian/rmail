//! The guards that stop `mail index` deleting an index nobody asked it to.
//!
//! Each of these returns before `client(socket)` is ever called, which is what
//! makes it safe to drive the real entry point with a socket path that cannot
//! be dialed: a regression that moved the guard after the connect would hang
//! here rather than pass. End-to-end coverage against a live daemon lives in
//! `rmaild/tests/index_service.rs`, for the reason
//! `rmail-cli/src/search_cli/tests.rs` documents — this crate is bin-only, so
//! an exec-the-binary suite is a separate `rmail-cli/tests/` target.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;

/// A path nothing can be listening on. Reaching a connect attempt at all is
/// the failure these tests are written to catch.
const UNDIALABLE: &str = "/nonexistent/rmail-index-test.sock";

#[tokio::test]
async fn rebuild_without_all_or_kind_refuses_rather_than_wiping_everything() {
    let err = run(
        Path::new(UNDIALABLE),
        IndexAction::Rebuild {
            all: false,
            kinds: Vec::new(),
            yes: true,
            max_jobs: 0,
        },
    )
    .await
    .expect_err("a bare rebuild must not default to wiping every stage");
    assert!(
        err.to_string().contains("--all"),
        "and it says how to ask for it on purpose: {err}"
    );
}

#[tokio::test]
async fn rebuild_rejects_all_and_kind_together() {
    let err = run(
        Path::new(UNDIALABLE),
        IndexAction::Rebuild {
            all: true,
            kinds: vec![KindArg::Lexical],
            yes: true,
            max_jobs: 0,
        },
    )
    .await
    .expect_err("--all and --kind contradict each other");
    assert!(err.to_string().contains("contradict"));
}

#[tokio::test]
async fn rebuild_without_yes_refuses_when_there_is_no_terminal_to_ask_on() {
    // The test harness has no tty, which is the same situation a CI job is in.
    // Guessing "yes" for a script that never passed `--yes` would make the flag
    // decorative on exactly the runs where it matters most.
    let err = run(
        Path::new(UNDIALABLE),
        IndexAction::Rebuild {
            all: true,
            kinds: Vec::new(),
            yes: false,
            max_jobs: 0,
        },
    )
    .await
    .expect_err("a non-interactive rebuild without --yes must refuse");
    assert!(err.to_string().contains("--yes"), "{err}");
}

#[tokio::test]
async fn embed_without_backfill_refuses_rather_than_guessing_a_mode() {
    let err = run(
        Path::new(UNDIALABLE),
        IndexAction::Embed {
            backfill: false,
            max_jobs: 0,
        },
    )
    .await
    .expect_err("the bare verb has no meaning yet");
    assert!(err.to_string().contains("--backfill"));
}

#[test]
fn a_stage_name_survives_the_round_trip_to_the_wire_and_back() {
    for arg in [
        KindArg::Extract,
        KindArg::Lexical,
        KindArg::Entities,
        KindArg::Semantic,
    ] {
        let wire = arg.to_proto();
        assert_ne!(
            wire,
            ProtoKind::Unspecified,
            "a CLI stage must never widen to `every stage` on the wire"
        );
        // `kind_name` is what `mail index status` prints; it has to agree with
        // the enum the daemon reports back, not with a second spelling of it.
        assert_eq!(kind_name(wire as i32), format!("{arg:?}").to_lowercase());
    }
}

#[test]
fn an_unknown_stage_on_the_wire_prints_as_itself_rather_than_panicking() {
    // A daemon a version ahead of this CLI can report a stage this build has
    // no name for. Status is a diagnostic; refusing to print it would be worse
    // than printing the number.
    assert_eq!(kind_name(4242), "kind_4242");
}

#[test]
fn an_empty_stage_list_stays_empty_which_the_daemon_reads_as_every_stage() {
    assert!(kinds(&[]).is_empty());
    assert_eq!(
        kinds(&[KindArg::Semantic]),
        vec![ProtoKind::Semantic as i32]
    );
}
