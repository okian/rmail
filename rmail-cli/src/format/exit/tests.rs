//! The exit-code table is a contract; these pin it.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use anyhow::Context as _;
use tonic::{Code, Status};

use super::*;

/// Two numbers can never be the same number, and none may collide with the
/// shell's own reserved range.
#[test]
fn every_exit_code_is_distinct_and_outside_the_shell_reserved_range() {
    let mut seen = std::collections::BTreeMap::new();
    for code in ExitCode::ALL {
        let n = code.code();
        assert!(
            n < 126,
            "{} is {n}; 126, 127 and >=128 belong to the shell",
            code.name()
        );
        if let Some(other) = seen.insert(n, code.name()) {
            panic!("{n} is both {other} and {}", code.name());
        }
    }
    assert_eq!(
        seen.len(),
        ExitCode::ALL.len(),
        "the table lost an entry to a duplicate"
    );
}

/// The numbers themselves are the contract — a script says `if [ $? -eq 5 ]`,
/// not `if [ $? -eq $PERMISSION_DENIED ]`. Reordering the enum would silently
/// renumber everything below the insertion point, so the values are pinned
/// literally rather than derived from the same enum that is under test.
#[test]
fn the_documented_numbers_are_the_numbers() {
    for (code, expected, name) in [
        (ExitCode::Success, 0u8, "success"),
        (ExitCode::Failure, 1, "failure"),
        (ExitCode::Usage, 2, "usage"),
        (ExitCode::Unavailable, 3, "unavailable"),
        (ExitCode::Unauthenticated, 4, "unauthenticated"),
        (ExitCode::PermissionDenied, 5, "permission_denied"),
        (ExitCode::NotFound, 6, "not_found"),
        (ExitCode::AlreadyExists, 7, "already_exists"),
        (ExitCode::InvalidArgument, 8, "invalid_argument"),
        (ExitCode::FailedPrecondition, 9, "failed_precondition"),
        (ExitCode::DeadlineExceeded, 10, "deadline_exceeded"),
        (ExitCode::ResourceExhausted, 11, "resource_exhausted"),
        (ExitCode::Unimplemented, 12, "unimplemented"),
        (ExitCode::Cancelled, 13, "cancelled"),
        (ExitCode::Internal, 14, "internal"),
    ] {
        assert_eq!(code.code(), expected, "{name} moved");
        assert_eq!(code.name(), name);
    }
}

/// The two the task calls out by name: a missing message and a refused scope
/// must be different numbers, and neither may be the generic failure.
#[test]
fn not_found_and_permission_denied_are_distinct_documented_codes() {
    let missing = ExitCode::of_status(Code::NotFound);
    let refused = ExitCode::of_status(Code::PermissionDenied);
    assert_ne!(missing, refused);
    assert_ne!(missing, ExitCode::Failure);
    assert_ne!(refused, ExitCode::Failure);
    assert_eq!(missing.code(), 6);
    assert_eq!(refused.code(), 5);
}

/// Every gRPC code maps somewhere, and the mapping is not the identity on
/// "everything is internal".
#[test]
fn every_grpc_code_maps_and_the_mapping_discriminates() {
    let codes = [
        Code::Ok,
        Code::Cancelled,
        Code::Unknown,
        Code::InvalidArgument,
        Code::DeadlineExceeded,
        Code::NotFound,
        Code::AlreadyExists,
        Code::PermissionDenied,
        Code::ResourceExhausted,
        Code::FailedPrecondition,
        Code::Aborted,
        Code::OutOfRange,
        Code::Unimplemented,
        Code::Internal,
        Code::Unavailable,
        Code::DataLoss,
        Code::Unauthenticated,
    ];
    let distinct: std::collections::BTreeSet<u8> = codes
        .iter()
        .map(|c| ExitCode::of_status(*c).code())
        .collect();
    assert!(
        distinct.len() >= 12,
        "only {} distinct exit codes across 17 gRPC codes — the mapping has collapsed",
        distinct.len()
    );
    assert_eq!(ExitCode::of_status(Code::Ok), ExitCode::Success);
}

/// The classifier has to look *through* the `.context(...)` every call site in
/// this crate attaches, or every RPC failure in the binary exits 1.
#[test]
fn a_status_is_found_underneath_the_context_a_call_site_attaches() {
    let err: anyhow::Error = Result::<(), Status>::Err(Status::permission_denied("nope"))
        .context("MailService/Get RPC failed")
        .context("while doing the thing")
        .expect_err("constructed as an error");
    assert_eq!(ExitCode::of(&err), ExitCode::PermissionDenied);

    let err: anyhow::Error = Result::<(), Status>::Err(Status::not_found("no such message"))
        .context("MailService/Get RPC failed")
        .expect_err("constructed as an error");
    assert_eq!(ExitCode::of(&err), ExitCode::NotFound);
}

/// An unclassifiable local failure is 1, not a wrong-but-specific code.
#[test]
fn an_unclassified_local_failure_is_the_generic_one() {
    let err = anyhow::anyhow!("the editor exited without writing anything");
    assert_eq!(ExitCode::of(&err), ExitCode::Failure);
}

/// A locally-detected precondition carries its own code without pretending a
/// server said so.
#[test]
fn a_classified_local_failure_keeps_its_code_through_context() {
    let err = Classified::new(ExitCode::FailedPrecondition, "rmaild is not running")
        .context("connecting to rmaild");
    assert_eq!(ExitCode::of(&err), ExitCode::FailedPrecondition);
    assert!(
        format!("{err:#}").contains("rmaild is not running"),
        "the message must survive: {err:#}"
    );
}

/// A missing socket file is a not-found; a refused connection is unavailable.
/// Both come back as `std::io::Error` from the connector, and a script retries
/// one of them.
#[test]
fn io_errors_are_classified_by_kind() {
    let err = anyhow::Error::new(std::io::Error::new(
        std::io::ErrorKind::ConnectionRefused,
        "refused",
    ))
    .context("connecting to rmaild");
    assert_eq!(ExitCode::of(&err), ExitCode::Unavailable);

    let err = anyhow::Error::new(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        "no such file",
    ))
    .context("reading keys.toml");
    assert_eq!(ExitCode::of(&err), ExitCode::NotFound);
}

/// A `Status` nested below an io error still wins, because the search is over
/// the whole chain in order and the RPC failure is the more specific fact.
#[test]
fn the_first_recognisable_cause_in_the_chain_decides() {
    let err: anyhow::Error = Result::<(), Status>::Err(Status::resource_exhausted("budget"))
        .context("AiService/Process RPC failed")
        .expect_err("constructed as an error");
    assert_eq!(ExitCode::of(&err), ExitCode::ResourceExhausted);
}
