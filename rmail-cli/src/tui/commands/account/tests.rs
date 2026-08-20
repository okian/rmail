//! Task 97's account verbs: the two ways an account is added, the switch that
//! needs no restart, and the one verb here that refuses to guess.
//!
//! `panic!` in a branch that cannot happen reads better here than the
//! `unreachable!` dance, and this module is test-only — the same exemption
//! `tui::model::tests` takes.
#![allow(clippy::panic)]

use rmail_core::command::{self, Resolution};
use rmail_core::keymap::Key;
use rmail_proto::v1::{
    Account as ProtoAccount, AutoconfigureResponse, BeginOAuthResponse, DiscoveredServer,
    ListAccountsResponse,
};

use super::*;
use crate::tui::html::{self, CommandOpener};
use crate::tui::model::{
    update, wire, Account, Folder, MessageRow, Model, Msg, Overlay, ReportEvent, Screen,
};
use crate::tui::report::{ReportFill, ReportTone};

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

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
        selection: vec![10, 11],
        rule_draft: None,
    }
}

fn empty() -> Target {
    Target {
        account_id: 0,
        mailbox_id: None,
        message_id: None,
        selection: Vec::new(),
        rule_draft: None,
    }
}

fn asked(line: &str, target: &Target) -> Answer {
    match answer(&invocation(line), target, 5) {
        Some(answer) => answer,
        None => panic!("{line:?} has no answer"),
    }
}

fn request_on(line: &str, target: &Target) -> Request {
    match asked(line, target) {
        Answer::Rows(request) | Answer::Fact(request) => *request,
        other => panic!("{line:?} is not a request: {other:?}"),
    }
}

fn request(line: &str) -> Request {
    request_on(line, &screen())
}

fn refusal(line: &str, target: &Target) -> String {
    match asked(line, target) {
        Answer::Refused(why) => why,
        other => panic!("{line:?} was not refused: {other:?}"),
    }
}

/// A model with two accounts listed and the first one open.
fn loaded() -> Model {
    let mut model = Model::new();
    model.accounts = vec![
        Account {
            id: 7,
            name: "personal".to_owned(),
            username: Some("me@example.com".to_owned()),
        },
        Account {
            id: 8,
            name: "work".to_owned(),
            username: Some("me@work.example".to_owned()),
        },
    ];
    model.account = model.accounts.first().cloned();
    model.folders = vec![Folder {
        id: 1,
        name: "INBOX".to_owned(),
        message_count: 2,
    }];
    model.open_folder = Some(1);
    model.messages = (10..12)
        .map(|id| MessageRow {
            id,
            subject: format!("subject {id}"),
            from: "Alice".to_owned(),
            from_addr: Some("alice@example.com".to_owned()),
            date: Some(1_700_000_000 + id),
            flags: Vec::new(),
            has_attachments: false,
        })
        .collect();
    model
}

/// What the `:` line is complaining about.
///
/// The command pane's own error line, not the status line: `complain` leaves the
/// pane open with the reason in it precisely so the line that caused it can be
/// fixed rather than retyped, and a test reading the status line would be reading
/// the "command — type a verb" the pane opened with.
fn complaint(model: &Model) -> String {
    match model.overlay.as_ref() {
        Some(Overlay::Command(pane)) => pane.error.clone().unwrap_or_default(),
        _ => model.status.clone(),
    }
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

fn proto_account(id: i64) -> ProtoAccount {
    ProtoAccount {
        id,
        name: format!("account {id}"),
        imap_server: Some("imap.example.com".to_owned()),
        imap_port: Some(993),
        username: Some("me@example.com".to_owned()),
        smtp_server: Some("smtp.example.com".to_owned()),
        smtp_port: Some(587),
        credential_kind: "keychain".to_owned(),
        credential_ref: Some("rmail".to_owned()),
        created_at: 1_700_000_000,
        updated_at: 1_700_000_001,
    }
}

fn discovered(source: &str, existing: i64, username: &str) -> AutoconfigureResponse {
    AutoconfigureResponse {
        source: source.to_owned(),
        imap: Some(DiscoveredServer {
            host: "imap.example.com".to_owned(),
            port: 993,
            security: "tls".to_owned(),
            username: username.to_owned(),
        }),
        smtp: Some(DiscoveredServer {
            host: "smtp.example.com".to_owned(),
            port: 587,
            security: "starttls".to_owned(),
            username: username.to_owned(),
        }),
        toml: "[[accounts]]\nname = \"you@example.com\"\n".to_owned(),
        login_validated: false,
        validation_detail: String::new(),
        existing_account_id: existing,
        warnings: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// which account a verb acts on
// ---------------------------------------------------------------------------

#[test]
fn a_read_falls_back_to_the_account_on_screen() {
    // "This account" is what somebody means when they are looking at it, and
    // typing an id they can see in the header would be busywork.
    assert_eq!(
        request("account show").cmd,
        Cmd::AccountShow {
            generation: 5,
            account_id: 7,
        }
    );
    assert_eq!(
        request("account show 8").cmd,
        Cmd::AccountShow {
            generation: 5,
            account_id: 8,
        }
    );
    let why = refusal("account show", &empty());
    assert!(why.contains(":account list"), "{why}");
}

#[test]
fn account_rm_never_falls_back_to_whatever_is_open() {
    // The one verb where guessing is unrecoverable: it cascades to every
    // message stored for the account. A line that deleted the account on screen
    // because its id was left off is a line nobody should be able to type by
    // accident.
    let why = refusal("account rm", &screen());
    assert!(why.contains("which account"), "{why}");
    assert_eq!(
        request("account rm 8").cmd,
        Cmd::AccountDelete { account_id: 8 }
    );
}

#[test]
fn account_rm_is_the_only_account_verb_that_asks() {
    // Task 89 settled that a typed `:` line is already the deliberate act, so
    // gating every mutating verb would make the question meaningless. What is
    // left is the per-verb judgement about the expensive, hard-to-undo ones.
    let asks = |line: &str| match asked(line, &screen()) {
        Answer::Rows(request) | Answer::Fact(request) => request.confirm.is_some(),
        other => panic!("{line}: {other:?}"),
    };
    assert!(asks("account rm 8"));
    for line in [
        "account list",
        "account show",
        "account add you@example.com",
        "account new you@example.com",
        "account login --oauth=google --client-id=x",
        "account refresh",
        "account test",
    ] {
        assert!(!asks(line), "{line} asks");
    }
}

#[test]
fn a_positional_that_is_not_a_number_is_refused_rather_than_ignored() {
    // Silently falling back to the account on screen would act on a different
    // account than the one that was named — a change to the wrong thing,
    // reported as a change to the right one.
    let why = refusal("account show eight", &screen());
    assert!(why.contains("not an account id"), "{why}");
}

// ---------------------------------------------------------------------------
// adding: propose, then apply
// ---------------------------------------------------------------------------

#[test]
fn account_add_discovers_and_writes_nothing() {
    assert_eq!(
        request("account add you@example.com").cmd,
        Cmd::AccountDiscover {
            generation: 5,
            email: "you@example.com".to_owned(),
            credential: None,
            allow_model: false,
        }
    );
    // The model step is opt-in: it costs money, and a proposal is a guess even
    // after the daemon validates it.
    let Cmd::AccountDiscover { allow_model, .. } = request("account add you@example.com --ai").cmd
    else {
        panic!("expected a discovery");
    };
    assert!(allow_model);
    assert!(refusal("account add", &screen()).contains("address"));
}

#[test]
fn a_credential_reference_travels_and_a_password_never_does() {
    for (flag, expected) in [
        (
            "--password-command=pass",
            Credential::Command("pass".to_owned()),
        ),
        (
            "--password-env=RMAIL_PW",
            Credential::Env("RMAIL_PW".to_owned()),
        ),
        ("--keychain=rmail", Credential::Keychain("rmail".to_owned())),
    ] {
        let Cmd::AccountDiscover { credential, .. } =
            request(&format!("account add you@example.com {flag}")).cmd
        else {
            panic!("expected a discovery");
        };
        assert_eq!(credential, Some(expected), "{flag}");
    }
}

#[test]
fn two_credentials_are_refused_rather_than_one_being_dropped() {
    // `CredentialRef` is a oneof: a request carrying two would have one of them
    // silently dropped at the wire seam, and the account would authenticate
    // with something other than what was asked for.
    let why = refusal(
        "account add you@example.com --keychain=rmail --password-env=PW",
        &screen(),
    );
    assert!(why.contains("one credential at a time"), "{why}");
    let why = refusal(
        "account new you@example.com --keychain=rmail --oauth=google",
        &screen(),
    );
    assert!(why.contains("one credential at a time"), "{why}");
}

#[test]
fn a_port_that_is_not_a_port_is_refused_where_it_was_typed() {
    for line in [
        "account new x --imap-port=nine",
        "account new x --imap-port=0",
        "account new x --smtp-port=70000",
    ] {
        let why = refusal(line, &screen());
        assert!(why.contains("a port, 1 to 65535"), "{line}: {why}");
    }
    let Cmd::AccountCreate { settings, .. } = request("account new x --imap-port=993").cmd else {
        panic!("expected a create");
    };
    assert_eq!(settings, vec![("imap-port".to_owned(), "993".to_owned())]);
}

#[test]
fn the_apply_row_is_a_line_somebody_could_have_typed() {
    // The whole reason `:account new` is a verb: a row's action *is* an
    // `Invocation` (task 90), so applying a proposal means there is a `:` line
    // that applies it — and the settings on it are visible before it runs.
    let rows = wire::autoconfigure_rows("you@example.com", &discovered("ispdb", 0, "you"));
    let apply = rows
        .iter()
        .find(|row| row.cells.first().is_some_and(|cell| cell == "apply"))
        .expect("an apply row");
    let applied = apply.on_enter.clone().expect("it runs something");
    assert_eq!(applied.verb, vec!["account", "new"]);
    assert_eq!(applied.positionals, vec!["you@example.com".to_owned()]);
    let flag = |name: &str| {
        applied
            .flags
            .iter()
            .find(|flag| flag.name == name)
            .and_then(|flag| flag.value.clone())
    };
    assert_eq!(flag("imap-server"), Some("imap.example.com".to_owned()));
    assert_eq!(flag("imap-port"), Some("993".to_owned()));
    assert_eq!(flag("username"), Some("you".to_owned()));
    assert_eq!(flag("smtp-port"), Some("587".to_owned()));
    // Not bang'd: creating an account is the mutation task 90's gate should ask
    // about, and a proposal that may have come from a model is exactly when
    // `[y/N]` in front of the settings is worth having.
    assert!(!applied.bang);
}

#[test]
fn a_discovered_value_with_a_space_in_it_survives_being_put_on_a_line() {
    // A username comes out of an autoconfig document fetched over the network,
    // so it is untrusted text. Unquoted, one with a space in it would split into
    // two tokens and ask the verb about something nobody typed.
    let rows = wire::autoconfigure_rows("you@example.com", &discovered("ispdb", 0, "ada lovelace"));
    let apply = rows
        .iter()
        .find(|row| row.cells.first().is_some_and(|cell| cell == "apply"))
        .expect("an apply row");
    let applied = apply.on_enter.clone().expect("it runs something");
    assert_eq!(
        applied
            .flags
            .iter()
            .find(|flag| flag.name == "username")
            .and_then(|flag| flag.value.clone()),
        Some("ada lovelace".to_owned())
    );
}

#[test]
fn no_apply_row_when_that_address_already_has_an_account() {
    // `Create` would make a second one. The report says which account exists,
    // and the TOML block is still there for somebody who really does want two.
    let rows = wire::autoconfigure_rows("you@example.com", &discovered("ispdb", 3, "you"));
    assert!(
        !rows
            .iter()
            .any(|row| row.cells.first().is_some_and(|cell| cell == "apply")),
        "{rows:?}"
    );
    let existing = rows
        .iter()
        .find(|row| row.cells.first().is_some_and(|cell| cell == "existing"))
        .expect("an existing row");
    assert_eq!(existing.tone, ReportTone::Warn);
    assert!(existing.cells[1].contains("nothing was changed"));
}

#[test]
fn a_proposal_that_came_from_a_model_says_so_and_is_drawn_as_a_warning() {
    let rows = wire::autoconfigure_rows("you@example.com", &discovered("model", 0, "you"));
    let source = rows.first().expect("a source row");
    assert_eq!(source.tone, ReportTone::Warn);
    assert!(source.cells[1].contains("a guess"), "{:?}", source.cells);
    // And an ordinary probe is not drawn as one, or the warning would mean
    // nothing.
    let rows = wire::autoconfigure_rows("you@example.com", &discovered("srv", 0, "you"));
    assert_eq!(rows.first().map(|row| row.tone), Some(ReportTone::Plain));
}

#[test]
fn the_toml_row_carries_the_verb_that_opens_the_block() {
    let rows = wire::autoconfigure_rows("you@example.com", &discovered("ispdb", 0, "you"));
    let toml = rows
        .iter()
        .find(|row| row.cells.first().is_some_and(|cell| cell == "toml"))
        .expect("a toml row");
    let opened = toml.on_enter.clone().expect("it runs something");
    assert_eq!(opened.verb, vec!["account", "toml"]);
    assert!(
        opened.bang,
        "opening a file this process wrote is not a question"
    );
}

#[test]
fn the_block_outlives_the_report_it_was_shown_in() {
    // Somebody reads the proposal, closes it, thinks, and then wants the block.
    // That is why it is session state and not a field on the pane.
    let mut model = loaded();
    let cmds = run(&mut model, "account add you@example.com");
    assert!(
        matches!(cmds.first(), Some(Cmd::AccountDiscover { .. })),
        "{cmds:?}"
    );
    let why = {
        let mut fresh = loaded();
        // Nothing discovered yet: the verb says how to discover something
        // rather than doing nothing.
        run(&mut fresh, "account toml");
        complaint(&fresh)
    };
    assert!(why.contains(":account add"), "{why}");

    update(&mut model, Msg::AccountToml("[[accounts]]\n".to_owned()));
    update(&mut model, Msg::Key(Key::Esc));
    assert!(model.overlay.is_none());
    let cmds = run(&mut model, "account toml");
    assert_eq!(
        cmds,
        vec![Cmd::OpenText {
            text: "[[accounts]]\n".to_owned(),
            extension: "toml".to_owned(),
            label: "the [[accounts]] block".to_owned(),
        }]
    );
}

// ---------------------------------------------------------------------------
// listing, and switching
// ---------------------------------------------------------------------------

#[test]
fn a_listing_row_carries_the_switch() {
    // Read the list, move to a row, press Enter. Every row carries it,
    // including the open one — `use_account` answers that with "already looking
    // at it" rather than a reload, which beats a row that does nothing.
    let response = ListAccountsResponse {
        accounts: vec![proto_account(7), proto_account(8)],
    };
    let rows = wire::account_rows(&response, 7);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].tone, ReportTone::Ok, "the open account is marked");
    assert_eq!(rows[1].tone, ReportTone::Plain);
    for (row, id) in rows.iter().zip([7, 8]) {
        let switch = row.on_enter.clone().expect("it runs something");
        assert_eq!(switch.verb, vec!["account", "use"]);
        assert_eq!(switch.positionals, vec![id.to_string()]);
        assert!(switch.bang, "switching back is the whole undo");
    }
}

#[test]
fn switching_accounts_clears_what_belonged_to_the_old_one() {
    // Everything on screen belongs to the account it came from, so leaving any
    // of it would put one account's rows under a header naming another.
    let mut model = loaded();
    model.scroll = 12;
    model.message_idx = 1;
    let cmds = run(&mut model, "account use 8");
    assert_eq!(model.account.map(|account| account.id), Some(8));
    assert!(model.folders.is_empty());
    assert!(model.messages.is_empty());
    assert_eq!(model.open_folder, None);
    assert_eq!(model.folder_idx, 0);
    assert_eq!(model.message_idx, 0);
    assert_eq!(model.scroll, 0);
    assert_eq!(model.screen, Screen::List);
    // And it loads the new one exactly the way the first account loads, which is
    // what keeps "start looking at this account" one path rather than two.
    assert_eq!(
        cmds,
        vec![
            Cmd::LoadFolders { account_id: 8 },
            Cmd::Watch { account_id: 8 },
            Cmd::LoadOutbox { account_id: 8 },
            Cmd::Heartbeat { account_id: 8 },
        ]
    );
}

#[test]
fn switching_to_the_account_already_open_does_nothing_at_all() {
    // Somebody asking for the account they are on wants nothing to happen, and
    // throwing away their cursor and their open message to fetch the same rows
    // again would be the opposite of nothing.
    let mut model = loaded();
    model.message_idx = 1;
    let cmds = run(&mut model, "account use 7");
    assert!(cmds.is_empty(), "{cmds:?}");
    assert_eq!(model.message_idx, 1);
    assert!(!model.folders.is_empty());
    assert!(
        model.status.contains("already looking at"),
        "{}",
        model.status
    );
}

#[test]
fn an_id_the_daemon_has_never_listed_is_refused_rather_than_sent() {
    // A folder listing for an account that does not exist answers NOT_FOUND two
    // round trips later, by which point the screen has already been cleared.
    let mut model = loaded();
    let cmds = run(&mut model, "account use 99");
    assert!(cmds.is_empty(), "{cmds:?}");
    assert_eq!(model.account.map(|account| account.id), Some(7));
    assert!(!model.folders.is_empty(), "nothing was cleared");

    let mut fresh = Model::new();
    run(&mut fresh, "account use 1");
    let why = complaint(&fresh);
    assert!(why.contains(":account list"), "{why}");
}

#[test]
fn a_listing_row_switches_and_drops_the_selection_it_was_pressed_from() {
    // The row path rather than a typed line, and deliberately: opening `:` over
    // a visual selection prefills `'<,'>`, and `:'<,'>account use 8` is refused
    // — correctly, since a range names a set of messages and this verb acts on
    // none. So the only way `use_account` runs with a selection up is from a
    // row, which is exactly why it has to clear it: the selection is a set of
    // rows in a list that is about to be replaced.
    let mut model = loaded();
    let cmds = run(&mut model, "account list");
    let generation = match cmds.first() {
        Some(Cmd::AccountList { generation, .. }) => *generation,
        other => panic!("expected a listing: {other:?}"),
    };
    let response = ListAccountsResponse {
        accounts: vec![proto_account(7), proto_account(8)],
    };
    update(
        &mut model,
        Msg::Report {
            generation,
            event: ReportEvent::Frame {
                fill: ReportFill::Replace,
                rows: wire::account_rows(&response, 7),
                complete: true,
            },
        },
    );
    model.visual = Some(0);
    assert!(model.is_selecting(), "the selection is up");
    // The second row is account 8.
    update(&mut model, Msg::Key(Key::Char('j')));
    let cmds: Vec<Cmd> = update(&mut model, Msg::Key(Key::Enter))
        .into_iter()
        .filter(|cmd| !matches!(cmd, Cmd::SaveHistory { .. }))
        .collect();
    assert_eq!(
        model.account.as_ref().map(|account| account.id),
        Some(8),
        "{cmds:?}"
    );
    assert_eq!(model.visual, None, "the selection went with the list");
    assert!(
        cmds.contains(&Cmd::LoadFolders { account_id: 8 }),
        "{cmds:?}"
    );
}

#[test]
fn account_use_needs_a_number() {
    let mut model = loaded();
    run(&mut model, "account use");
    let why = complaint(&model);
    assert!(why.contains("needs an id"), "{why}");
    let mut model = loaded();
    run(&mut model, "account use work");
    let why = complaint(&model);
    assert!(why.contains("not an account id"), "{why}");
}

#[test]
fn the_accounts_the_daemon_listed_are_kept_even_when_none_can_be_chosen() {
    // A session that could not choose an account is exactly the session that
    // needs to be able to list them.
    let mut model = Model::new();
    update(
        &mut model,
        Msg::Accounts(Ok(vec![Account {
            id: 7,
            name: "personal".to_owned(),
            username: None,
        }])),
    );
    assert_eq!(model.accounts.len(), 1);
    assert_eq!(model.account.map(|account| account.id), Some(7));
}

// ---------------------------------------------------------------------------
// the OAuth flow
// ---------------------------------------------------------------------------

#[test]
fn login_needs_a_provider_and_a_client_id() {
    assert!(refusal("account login", &screen()).contains("which provider"));
    let why = refusal("account login --oauth=google", &screen());
    assert!(why.contains("--client-id"), "{why}");
    assert_eq!(
        request("account login --oauth=google --client-id=abc").cmd,
        Cmd::AccountLogin {
            generation: 5,
            account_id: 7,
            provider: "google".to_owned(),
            client_id: "abc".to_owned(),
            client_secret_command: None,
            scopes: Vec::new(),
            open_browser: true,
        }
    );
}

#[test]
fn scopes_are_read_from_both_spellings() {
    // `mail account login --scope a,b` and `--scope a --scope b` are both
    // accepted there, and a TUI taking only one of them would be the surface
    // where a documented form did nothing.
    let Cmd::AccountLogin { scopes, .. } =
        request("account login --oauth=google --client-id=x --scope=a,b --scope=c").cmd
    else {
        panic!("expected a login");
    };
    assert_eq!(scopes, vec!["a".to_owned(), "b".to_owned(), "c".to_owned()]);
}

#[test]
fn the_browser_is_launched_unless_it_was_asked_not_to_be() {
    let Cmd::AccountLogin { open_browser, .. } =
        request("account login --oauth=google --client-id=x --no-browser").cmd
    else {
        panic!("expected a login");
    };
    assert!(!open_browser);
}

#[test]
fn the_url_is_on_screen_whether_or_not_a_browser_opens_it() {
    // A browser that does not launch, or launches somewhere the user is not
    // signed in, leaves the URL as the only way to finish — and a flow whose URL
    // scrolled past unread cannot be recovered.
    let rows = wire::oauth_started_rows(&BeginOAuthResponse {
        flow_id: "f1".to_owned(),
        authorization_url: "https://accounts.google.com/o/oauth2/auth?x=1".to_owned(),
        redirect_uri: "http://127.0.0.1:53101/".to_owned(),
        expires_at: 1_700_000_600,
    });
    assert_eq!(
        rows.first().map(|row| row.cells[1].clone()),
        Some("https://accounts.google.com/o/oauth2/auth?x=1".to_owned())
    );
    assert!(
        rows.iter()
            .any(|row| row.cells[1].contains("waiting") || row.cells[0].contains("waiting")),
        "the report says it is waiting: {rows:?}"
    );
}

#[test]
fn the_open_path_refuses_anything_that_is_not_an_https_url() {
    // The URL comes from the daemon, so a `file://` here would mean something
    // had already gone wrong — but "the value is safe today" is not the property
    // worth relying on when handing an argument to whatever handler the platform
    // has registered for its scheme.
    //
    // A program name that does not exist is enough to prove nothing was
    // spawned: validation happens before the opener is reached, so a refusal is
    // a refusal and not a failed launch.
    let opener = CommandOpener::new("/nonexistent/rmail-should-never-run");
    for url in [
        "file:///etc/passwd",
        "http://example.com/",
        "javascript:alert(1)",
        "https://example.com/ and something",
    ] {
        let error = html::open_url(url, &opener).expect_err(url);
        assert!(
            format!("{error:#}").contains("only https URLs"),
            "{url}: {error:#}"
        );
    }
}

// ---------------------------------------------------------------------------
// the rest of the table
// ---------------------------------------------------------------------------

#[test]
fn the_remaining_verbs_reach_their_own_rpc() {
    assert_eq!(
        request("account list").cmd,
        Cmd::AccountList {
            generation: 5,
            open: 7,
        }
    );
    assert_eq!(
        request("account test").cmd,
        Cmd::AccountTest {
            generation: 5,
            account_id: 7,
        }
    );
    assert_eq!(
        request("account refresh").cmd,
        Cmd::AccountRefresh {
            generation: 5,
            account_id: 7,
            force: false,
        }
    );
    let Cmd::AccountRefresh { force, .. } = request("account refresh --force").cmd else {
        panic!("expected a refresh");
    };
    assert!(force);
}

#[test]
fn every_one_account_table_shares_one_column_layout() {
    // Four verbs answering about one account: four layouts would be four chances
    // to disagree about what a column means.
    let show = request("account show").columns;
    for line in [
        "account test",
        "account refresh",
        "account login --oauth=google --client-id=x",
        "account add you@example.com",
    ] {
        assert_eq!(request(line).columns, show, "{line}");
    }
}

#[test]
fn an_account_with_no_credential_is_drawn_as_the_reason_a_sync_would_fail() {
    let mut account = proto_account(7);
    account.credential_kind = "none".to_owned();
    account.credential_ref = None;
    let rows = wire::account_fields(&account);
    let credential = rows
        .iter()
        .find(|row| row.cells[0] == "credential")
        .expect("a credential row");
    assert_eq!(credential.tone, ReportTone::Warn);
    assert_eq!(credential.cells[1], "none");
}
