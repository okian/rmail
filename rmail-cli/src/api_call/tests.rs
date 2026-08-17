//! Method resolution and error classification for `mail api call`.
//!
//! The end-to-end half — a real daemon, a real reflection exchange, a real
//! scope refusal — is `rmail-cli/tests/api_call.rs`, which execs the compiled
//! binary. What is testable in isolation is the resolver and the mapping from
//! the bridge's errors onto this binary's exit codes, and both are here.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use rmail_core::parity::Command;
use rmaild::mcp::descriptor::catalog;

use super::*;

/// Every spelling a person plausibly types resolves to the same method.
#[test]
fn every_documented_spelling_of_a_method_resolves() {
    let catalog = catalog().unwrap();
    for spelling in [
        "MailService.Get",
        "rmail.v1.MailService.Get",
        "rmail.v1.MailService/Get",
        "/rmail.v1.MailService/Get",
    ] {
        let method = resolve(catalog, spelling).unwrap_or_else(|e| panic!("{spelling}: {e:#}"));
        assert_eq!(method.path, "/rmail.v1.MailService/Get");
        assert_eq!(method.input_type, "rmail.v1.GetMessageRequest");
    }
}

/// Every capability the parity registry declares is reachable by the spelling
/// prd.md uses (`Service.Method`). This is what makes `api call` an honest
/// escape hatch rather than one that covers most of the surface — and it is
/// the reason `api call` has no capability row of its own.
#[test]
fn every_capability_in_the_registry_is_reachable_by_name() {
    let catalog = catalog().unwrap();
    let mut unreachable = Vec::new();
    for command in Command::ALL {
        let (service, method) = command
            .rpc()
            .trim_start_matches('/')
            .rsplit_once('/')
            .expect("a capability's rpc is /pkg.Service/Method");
        let short = service.rsplit_once('.').map_or(service, |(_, tail)| tail);
        if resolve(catalog, &format!("{short}.{method}")).is_err() {
            unreachable.push(command.rpc());
        }
    }
    assert!(
        unreachable.is_empty(),
        "`mail api call` cannot reach these declared capabilities: {unreachable:?}"
    );
}

/// A method that does not exist is `NOT_FOUND` and points at the verb that
/// lists what does — not a generic failure a script cannot distinguish from a
/// broken daemon.
#[test]
fn an_unknown_method_is_not_found_and_names_the_listing_verb() {
    let catalog = catalog().unwrap();
    let error = resolve(catalog, "MailService.Teleport").expect_err("no such method");
    assert_eq!(ExitCode::of(&error), ExitCode::NotFound);
    assert!(
        format!("{error:#}").contains("mail api reflect"),
        "{error:#}"
    );
}

/// Something that is not a method name at all is a *usage* error, which is a
/// different fix from "the daemon does not have it".
#[test]
fn a_malformed_method_name_is_a_usage_error() {
    let catalog = catalog().unwrap();
    for spelling in ["List", "", "MailService.", ".List", "/"] {
        let error = resolve(catalog, spelling)
            .err()
            .unwrap_or_else(|| panic!("`{spelling}` must not resolve"));
        assert_eq!(
            ExitCode::of(&error),
            ExitCode::Usage,
            "`{spelling}`: {error:#}"
        );
    }
}

/// A hostile method name reaches the error message, and therefore a terminal.
#[test]
fn a_hostile_method_name_is_sanitized_before_it_reaches_the_error() {
    let catalog = catalog().unwrap();
    let error = resolve(catalog, "MailService.\u{1b}[2JGet\u{202e}x").expect_err("no such method");
    let rendered = format!("{error:#}");
    assert!(!rendered.contains('\u{1b}'), "{rendered:?}");
    assert!(!rendered.contains('\u{202e}'), "{rendered:?}");
}

/// The bridge's errors carry different fixes, so they must carry different
/// exit codes. A refusal must never look like a bad argument, and vice versa.
#[test]
fn the_bridge_errors_map_to_distinct_exit_codes() {
    use rmaild::mcp::McpError;
    let cases = [
        (
            McpError::InvalidArguments("no such field".to_owned()),
            ExitCode::InvalidArgument,
        ),
        (
            McpError::Denied {
                tool: "delete_message".to_owned(),
                requires: "mail.write".to_owned(),
            },
            ExitCode::PermissionDenied,
        ),
        (McpError::Cancelled, ExitCode::Cancelled),
        (
            McpError::Unavailable("no channel".to_owned()),
            ExitCode::Unavailable,
        ),
        (
            McpError::Rpc(Box::new(tonic::Status::not_found("no such message"))),
            ExitCode::NotFound,
        ),
        (
            McpError::Rpc(Box::new(tonic::Status::permission_denied("nope"))),
            ExitCode::PermissionDenied,
        ),
        (McpError::Wire("truncated".to_owned()), ExitCode::Internal),
    ];
    for (error, expected) in cases {
        let rendered = format!("{error}");
        let mapped = mcp_error(error);
        assert_eq!(ExitCode::of(&mapped), expected, "{rendered}");
    }
}

/// The daemon's own message reaches the operator (it is the only thing that
/// says *which* field was wrong) but cannot drive their terminal.
#[test]
fn a_hostile_daemon_message_is_sanitized_on_its_way_out() {
    use rmaild::mcp::McpError;
    let mapped = mcp_error(McpError::InvalidArguments(
        "field \u{1b}[31m\u{202e}subject".to_owned(),
    ));
    let rendered = format!("{mapped:#}");
    assert!(!rendered.contains('\u{1b}'), "{rendered:?}");
    assert!(!rendered.contains('\u{202e}'), "{rendered:?}");
    assert!(rendered.contains("subject"), "{rendered:?}");
}
