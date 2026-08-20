//! Task 97's token verbs, and the one thing about them that has to be right: a
//! minted secret is shown once, and once means it is not recoverable from
//! anything the model still holds afterwards.
//!
//! `panic!` in a branch that cannot happen reads better here than the
//! `unreachable!` dance, and this module is test-only — the same exemption
//! `tui::model::tests` takes.
#![allow(clippy::panic)]

use rmail_core::command::{self, Resolution};
use rmail_core::keymap::Key;
use rmail_proto::v1::{ListTokensResponse, MintTokenResponse, TokenInfo};

use super::*;
use crate::tui::model::{update, wire, Account, Model, Msg, Overlay, ReportEvent};
use crate::tui::report::{ReportFill, ReportTone};

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

/// The secret this suite chases through the model. Distinctive on purpose: the
/// test that matters most asserts it appears *nowhere*, so it has to be a string
/// nothing else could produce.
const SECRET: &str = "rmail_tok_ZZQQ-not-recoverable-42";

fn invocation(line: &str) -> Invocation {
    match command::parse(line) {
        Ok(Resolution::Invocation(invocation)) => *invocation,
        other => panic!("{line:?} does not parse to an invocation: {other:?}"),
    }
}

fn screen() -> Target {
    Target {
        account_id: 7,
        mailbox_id: Some(1),
        message_id: Some(10),
        selection: vec![10],
        rule_draft: None,
    }
}

fn asked(line: &str) -> Answer {
    match answer(&invocation(line), &screen(), 5) {
        Some(answer) => answer,
        None => panic!("{line:?} has no answer"),
    }
}

fn request(line: &str) -> Request {
    match asked(line) {
        Answer::Rows(request) | Answer::Fact(request) => *request,
        other => panic!("{line:?} is not a request: {other:?}"),
    }
}

fn refusal(line: &str) -> String {
    match asked(line) {
        Answer::Refused(why) => why,
        other => panic!("{line:?} was not refused: {other:?}"),
    }
}

fn loaded() -> Model {
    let mut model = Model::new();
    model.accounts = vec![Account {
        id: 7,
        name: "personal".to_owned(),
        username: Some("me@example.com".to_owned()),
    }];
    model.account = model.accounts.first().cloned();
    model
}

fn run(model: &mut Model, line: &str) -> Vec<Cmd> {
    update(model, Msg::Key(Key::Char(':')));
    for c in line.chars() {
        update(model, Msg::Key(Key::Char(c)));
    }
    update(model, Msg::Key(Key::Enter))
        .into_iter()
        .filter(|cmd| !matches!(cmd, Cmd::SaveHistory { .. }))
        .collect()
}

fn minted() -> MintTokenResponse {
    MintTokenResponse {
        id: 4,
        token: SECRET.to_owned(),
        name: "ci".to_owned(),
        scopes: vec!["mail.read".to_owned()],
        created_at: 1_700_000_000,
        expires_at: Some(1_800_000_000),
    }
}

// ---------------------------------------------------------------------------
// the table
// ---------------------------------------------------------------------------

#[test]
fn minting_needs_a_label_and_at_least_one_scope() {
    assert!(refusal("token create").contains("label it"));
    let why = refusal("token create --name=ci");
    assert!(why.contains("--scope"), "{why}");
    // The daemon refuses a scopeless token too, and saying so now is the same
    // answer sooner: one could never do anything, which is almost certainly not
    // what was meant.
    assert_eq!(
        request("token create --name=ci --scope=mail.read,ai.invoke").cmd,
        Cmd::TokenCreate {
            generation: 5,
            name: "ci".to_owned(),
            scopes: vec!["mail.read".to_owned(), "ai.invoke".to_owned()],
            ttl_secs: None,
        }
    );
}

#[test]
fn the_ttl_spellings_are_the_ones_the_config_file_uses() {
    // A second duration grammar for one flag is the drift `parity` exists to
    // prevent, so this goes through `config::parse_human_duration` — the same
    // function `mail token create --ttl` uses.
    let ttl = |line: &str| match request(line).cmd {
        Cmd::TokenCreate { ttl_secs, .. } => ttl_secs,
        other => panic!("{line}: {other:?}"),
    };
    assert_eq!(
        ttl("token create --name=ci --scope=admin --ttl=24h"),
        Some(86_400)
    );
    assert_eq!(
        ttl("token create --name=ci --scope=admin --ttl=90d"),
        Some(90 * 86_400)
    );
    assert_eq!(ttl("token create --name=ci --scope=admin"), None);
    let why = refusal("token create --name=ci --scope=admin --ttl=soon");
    assert!(why.contains("--ttl"), "{why}");
}

#[test]
fn zero_is_not_a_spelling_of_never_expires() {
    // It is `INVALID_ARGUMENT` at the daemon, and leaving `--ttl` off is how
    // "never" is said. A client that quietly turned one into the other would be
    // minting a permanent token for a line that asked for a temporary one.
    let why = refusal("token create --name=ci --scope=admin --ttl=0s");
    assert!(why.contains("a positive duration"), "{why}");
}

#[test]
fn revoking_takes_an_id_and_does_not_ask() {
    // The safe direction: nothing is lost that was not already unrecoverable,
    // and re-revoking is explicitly not an error on this RPC. Asking here would
    // ask hardest about the answer somebody reaching for it in a hurry needs.
    let request = request("token revoke 4");
    assert_eq!(request.cmd, Cmd::TokenRevoke { token_id: 4 });
    assert!(request.confirm.is_none());
    assert!(refusal("token revoke").contains(":token list"));
}

#[test]
fn listing_is_metadata_and_is_re_runnable() {
    let request = request("token list");
    assert_eq!(request.cmd, Cmd::TokenList { generation: 5 });
    assert!(
        !request.once,
        "`ListTokens` returns no secret, so `r` is fine"
    );
}

// ---------------------------------------------------------------------------
// what the rows say
// ---------------------------------------------------------------------------

#[test]
fn a_revoked_token_stays_in_the_listing_drawn_dim() {
    // Knowing a token existed and was revoked is part of an audit trail; hiding
    // it would throw that away.
    let response = ListTokensResponse {
        tokens: vec![
            TokenInfo {
                id: 1,
                name: "ci".to_owned(),
                scopes: vec!["mail.read".to_owned()],
                created_at: 1_700_000_000,
                last_used_at: Some(1_700_000_500),
                expires_at: None,
                revoked: false,
            },
            TokenInfo {
                id: 2,
                name: "old".to_owned(),
                scopes: Vec::new(),
                created_at: 1_700_000_000,
                last_used_at: None,
                expires_at: Some(1_700_001_000),
                revoked: true,
            },
        ],
    };
    let rows = wire::token_rows(&response);
    assert_eq!(rows[0].tone, ReportTone::Ok);
    assert_eq!(rows[0].cells[2], "active");
    assert_eq!(rows[1].tone, ReportTone::Muted);
    assert_eq!(rows[1].cells[2], "revoked");
    // "never" for no expiry, and "unknown" rather than "never" for a token the
    // daemon reports no last-use for: a token never used and a daemon that does
    // not record it are different facts.
    assert_eq!(rows[0].cells[4], "never");
    assert_eq!(rows[1].cells[3], "unknown");
    // An empty scope list is a word, not a blank cell — a blank reads as a
    // rendering fault.
    assert_eq!(rows[1].cells[5], "none reported");
}

#[test]
fn the_minted_row_says_the_secret_cannot_be_shown_again() {
    // A reader who does not know will close the pane, and there is no second
    // chance to tell them.
    let rows = wire::minted_rows(&minted());
    let secret = rows
        .iter()
        .find(|row| row.cells[0] == "token")
        .expect("the secret is a row");
    assert_eq!(secret.cells[1], SECRET);
    let marker = rows.last().expect("a marker row");
    assert_eq!(marker.tone, ReportTone::Bad);
    assert!(
        marker.cells[1].contains("cannot be shown again"),
        "{:?}",
        marker.cells
    );
    assert!(
        marker.cells[1].contains(":token revoke 4"),
        "and how to undo it: {:?}",
        marker.cells
    );
}

// ---------------------------------------------------------------------------
// shown once, and once means once
// ---------------------------------------------------------------------------

/// Drive `:token create` to the point where the secret is on screen.
fn mint(model: &mut Model) {
    let cmds = run(model, "token create --name=ci --scope=mail.read");
    let generation = match cmds.first() {
        Some(Cmd::TokenCreate { generation, .. }) => *generation,
        other => panic!("expected a mint: {other:?}"),
    };
    update(
        model,
        Msg::Report {
            generation,
            event: ReportEvent::Frame {
                fill: ReportFill::Replace,
                rows: wire::minted_rows(&minted()),
                complete: true,
            },
        },
    );
}

#[test]
fn the_secret_is_on_screen_and_in_nothing_else() {
    let mut model = loaded();
    mint(&mut model);
    let Some(Overlay::Report(pane)) = model.overlay.as_ref() else {
        panic!("expected a report");
    };
    assert!(
        pane.rows
            .iter()
            .any(|row| row.cells.contains(&SECRET.to_owned())),
        "the secret is in the pane"
    );
    // And nowhere else *while it is up*: not on the status line, and not in the
    // command history.
    assert!(!model.status.contains(SECRET), "{}", model.status);
    // The history holds no `:token` line at all, which is a stronger property
    // than "no secret in it" and is not this task's work: `history::is_secret`
    // has refused to record `token …` and `account login …` since task 89, and
    // the verbs it was written for arrived here. Asserted anyway, because a
    // rule nothing checks after the verbs exist is a rule that can be relaxed
    // without anyone noticing.
    assert!(
        !model
            .history
            .entries()
            .iter()
            .any(|line| line.contains("token")),
        "{:?}",
        model.history.entries()
    );
}

#[test]
fn closing_the_pane_makes_the_secret_unrecoverable() {
    // The claim in full, checked against the *whole* model rather than the
    // fields somebody thought to look at: the daemon keeps only an argon2id
    // hash, so if this client held a copy anywhere it would be the only place
    // the secret still existed — and it would outlive the one screen that said
    // it was the last look.
    let mut model = loaded();
    mint(&mut model);
    update(&mut model, Msg::Key(Key::Esc));
    assert!(model.overlay.is_none());
    let everything = format!("{model:?}");
    assert!(
        !everything.contains(SECRET),
        "the secret survived closing the report"
    );
}

#[test]
fn r_refuses_to_mint_a_second_token() {
    // `r` means "ask this verb again". For a report that *produced* something,
    // asking again produces another one — and a reader pressing `r` to refresh a
    // pane is not asking for a second token.
    let mut model = loaded();
    mint(&mut model);
    let cmds = update(&mut model, Msg::Key(Key::Char('r')));
    assert!(cmds.is_empty(), "{cmds:?}");
    assert!(model.status.contains("ran once"), "{}", model.status);
    // The rows are left alone: they were true when they arrived, and they are
    // the only copy of the secret.
    let Some(Overlay::Report(pane)) = model.overlay.as_ref() else {
        panic!("the report stays up");
    };
    assert!(pane
        .rows
        .iter()
        .any(|row| row.cells.contains(&SECRET.to_owned())));
}

#[test]
fn an_ordinary_report_is_still_re_runnable() {
    // Or `once` would be a flag that quietly broke `r` everywhere.
    let mut model = loaded();
    let cmds = run(&mut model, "token list");
    assert!(
        matches!(cmds.first(), Some(Cmd::TokenList { .. })),
        "{cmds:?}"
    );
    let cmds = update(&mut model, Msg::Key(Key::Char('r')));
    assert!(
        matches!(cmds.first(), Some(Cmd::TokenList { .. })),
        "{cmds:?}"
    );
}
