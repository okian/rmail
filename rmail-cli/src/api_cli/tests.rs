//! `mail api`'s argument surface. The behaviour that needs a daemon lives in
//! `rmail-cli/tests/api_call.rs`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use clap::CommandFactory as _;

use crate::Cli;

fn api() -> clap::Command {
    Cli::command()
        .get_subcommands()
        .find(|s| s.get_name() == "api")
        .cloned()
        .expect("`mail api` exists")
}

/// The three verbs prd.md names, and no others invented along the way.
#[test]
fn the_three_documented_verbs_exist() {
    let api = api();
    let mut names: Vec<&str> = api
        .get_subcommands()
        .map(clap::Command::get_name)
        .filter(|n| *n != "help")
        .collect();
    names.sort_unstable();
    assert_eq!(names, vec!["call", "ping", "reflect"]);
}

/// `mail api call MailService.List` with no body must mean "the empty
/// message", not "usage error" — a request message with no required fields is
/// the common case and typing `'{}'` every time is friction with no purpose.
#[test]
fn the_request_body_defaults_to_an_empty_object() {
    let matches = Cli::command()
        .try_get_matches_from(["mail", "api", "call", "MailService.List"])
        .expect("parses");
    let (_, api) = matches.subcommand().expect("api");
    let (_, call) = api.subcommand().expect("call");
    assert_eq!(
        call.get_one::<String>("body").map(String::as_str),
        Some("{}")
    );
}

/// The global transport flags reach `mail api call`, which is the verb most
/// likely to be pointed at a remote daemon.
#[test]
fn the_global_flags_reach_the_api_verbs() {
    let matches = Cli::command()
        .try_get_matches_from([
            "mail",
            "api",
            "call",
            "MailService.List",
            "{}",
            "--addr",
            "127.0.0.1:50051",
            "--insecure",
            "--token",
            "t",
            "--deadline",
            "5",
            "--format",
            "ndjson",
        ])
        .expect("the global flags are accepted after the subcommand");
    let cli = <Cli as clap::FromArgMatches>::from_arg_matches(&matches).expect("builds");
    assert_eq!(cli.addr.as_deref(), Some("127.0.0.1:50051"));
    assert!(cli.insecure);
    assert_eq!(cli.deadline, Some(5));
    assert_eq!(cli.format, crate::format::OutputFormat::Ndjson);
}

/// `--max-frames 0` would drain nothing and return an empty answer that looks
/// like an empty mailbox.
#[tokio::test]
async fn a_zero_frame_limit_is_refused_before_connecting() {
    let socket = std::env::temp_dir().join("rmail-cli-never-used.sock");
    let error = super::run(
        &socket,
        super::ApiAction::Call {
            method: "MailService.List".to_owned(),
            body: "{}".to_owned(),
            max_frames: 0,
        },
    )
    .await
    .expect_err("--max-frames 0 must be refused");
    assert_eq!(
        crate::format::ExitCode::of(&error),
        crate::format::ExitCode::Usage
    );
}

/// A malformed body is a usage error, and it is caught before any connection
/// — so `mail api call X '{oops'` says what is wrong with the JSON rather than
/// that the daemon is not running.
#[tokio::test]
async fn a_malformed_body_is_a_usage_error_reported_before_connecting() {
    let socket = std::env::temp_dir().join("rmail-cli-never-used.sock");
    assert!(!socket.exists(), "this test must not need a daemon");
    let error = super::run(
        &socket,
        super::ApiAction::Call {
            method: "MailService.List".to_owned(),
            body: "{oops".to_owned(),
            max_frames: 10,
        },
    )
    .await
    .expect_err("malformed JSON must be refused");
    assert_eq!(
        crate::format::ExitCode::of(&error),
        crate::format::ExitCode::Usage
    );
    assert!(format!("{error:#}").contains("not valid JSON"), "{error:#}");
}
