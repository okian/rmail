//! Task 96's AI policy, safety and audit verbs.
//!
//! The claim the whole task turns on is here rather than in `tui::form`:
//! `:ai budget set` typed bare must *not* issue a partial `SetBudget`, because
//! that RPC replaces a scope's whole budget and a partial one is a budget with
//! caps silently deleted. Two tests state the two halves —
//! `a_bare_budget_set_applies_every_cap_the_daemon_reported` and
//! `a_banged_budget_set_sends_only_what_was_typed` — and they are the reason the
//! form exists at all.
//!
//! `wire::budget_rows`' tone ladder is asserted here too, next to the verb that
//! opens that report, so the acceptance's own verify filter selects it.
//!
//! `panic!` in a branch that cannot happen reads better here than the
//! `unreachable!` dance, and this module is test-only — the same exemption
//! `tui::model::tests` takes.
#![allow(clippy::panic)]

use rmail_core::command::{self, Resolution};
use rmail_core::keymap::{Key, Mode};
use rmail_proto::v1::{
    BudgetCaps, BudgetClass, BudgetSpend, BudgetWindowCaps, ClassSpend, GetSpendResponse,
};

use super::*;
use crate::tui::commands::FormRequest;
use crate::tui::model::{
    update, wire, Account, Folder, FormEvent, MessageRow, Model, Msg, Overlay,
};
use crate::tui::report::ReportTone;

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

fn form_request(line: &str) -> FormRequest {
    match asked(line, &screen()) {
        Answer::Form(request) => *request,
        other => panic!("{line:?} is not a form: {other:?}"),
    }
}

fn refusal(line: &str, target: &Target) -> String {
    match asked(line, target) {
        Answer::Refused(why) => why,
        other => panic!("{line:?} was not refused: {other:?}"),
    }
}

fn loaded() -> Model {
    let mut model = Model::new();
    model.account = Some(Account {
        id: 7,
        name: "personal".to_owned(),
        username: Some("me@example.com".to_owned()),
    });
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
            has_note: false,
            to: None,
            tags: Vec::new(),
            ai: None,
        })
        .collect();
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

/// A `GetSpend` answer with `caps` in force for the `all` class.
fn spend(caps: BudgetCaps) -> GetSpendResponse {
    GetSpendResponse {
        account_id: 0,
        day: "2026-08-20".to_owned(),
        month: "2026-08".to_owned(),
        all: Some(ClassSpend {
            class: BudgetClass::All as i32,
            daily: Some(BudgetSpend {
                usd: 1.5,
                tokens: 1_000,
            }),
            monthly: Some(BudgetSpend {
                usd: 4.0,
                tokens: 9_000,
            }),
            caps: Some(caps),
            stored: true,
        }),
        bulk: None,
    }
}

/// Daily caps only, which is the shape every threshold test needs.
fn daily(soft_usd: Option<f64>, hard_usd: Option<f64>) -> BudgetCaps {
    BudgetCaps {
        daily: Some(BudgetWindowCaps {
            soft_usd,
            hard_usd,
            soft_tokens: None,
            hard_tokens: None,
        }),
        monthly: None,
    }
}

// ---------------------------------------------------------------------------
// scope
// ---------------------------------------------------------------------------

#[test]
fn a_policy_verb_never_takes_its_scope_from_whatever_is_on_screen() {
    // Zero is the *global* budget on these RPCs, not "no account" — the
    // opposite of what it means to the daemon verbs. So an account being loaded
    // must not silently redirect a spending cap at it: `screen()` has account 7
    // and both of these still report the global scope.
    assert_eq!(
        request("ai budget status").cmd,
        Cmd::BudgetStatus {
            generation: 5,
            account_id: 0,
        }
    );
    assert_eq!(
        request("ai provider status").cmd,
        Cmd::ProviderStatus {
            generation: 5,
            account_id: 0,
        }
    );
    assert_eq!(
        request("ai budget status --account 3").cmd,
        Cmd::BudgetStatus {
            generation: 5,
            account_id: 3,
        }
    );
}

#[test]
fn an_account_that_is_not_a_number_is_refused_rather_than_defaulted() {
    // Defaulting would write a cap against the global budget while the caller
    // believed they had scoped it to one account — a change to the wrong thing,
    // reported as a change to the right one.
    for line in [
        "ai budget status --account nine",
        "ai budget set --account nine",
        "ai provider status --account nine",
        "ai provider set local --account nine",
        "ai audit --account nine",
    ] {
        let why = refusal(line, &screen());
        assert!(why.contains("--account"), "{line}: {why}");
    }
}

#[test]
fn a_policy_verb_needs_nothing_from_the_screen() {
    // Unlike every tag and rule verb, these do not refuse on an empty model:
    // the global budget and the daemon-wide provider are real scopes with no
    // account behind them, and refusing until one loads would make the daemon's
    // own settings unreachable on a fresh install.
    for line in ["ai budget status", "ai provider status", "ai audit"] {
        match answer(&invocation(line), &empty(), 5) {
            Some(Answer::Rows(_)) => {}
            other => panic!("{line}: {other:?}"),
        }
    }
}

// ---------------------------------------------------------------------------
// the form, and the bang that skips it
// ---------------------------------------------------------------------------

#[test]
fn a_bare_budget_set_reads_before_it_writes() {
    let request = form_request("ai budget set");
    assert_eq!(
        request.cmd,
        Cmd::BudgetForm {
            generation: 5,
            account_id: 0,
            class: Class::All,
        }
    );
    assert_eq!(request.fields.len(), CAPS.len());
    assert!(
        request.fields.iter().all(|field| field.value.is_empty()),
        "a form opens with no values in it: {:?}",
        request.fields
    );
}

#[test]
fn every_field_names_a_flag_its_verb_declares() {
    // A field naming something `ai budget set` does not declare would build a
    // line `command::parse` rejects — so the form would open, accept typing and
    // then refuse to apply, which is the worst of the three outcomes.
    let verb = command::verb_at(&["ai", "budget", "set"]).expect("ai budget set is declared");
    for field in fields() {
        assert!(
            verb.flags.iter().any(|flag| flag.name == field.flag),
            "--{} is not declared by :ai budget set",
            field.flag
        );
    }
}

#[test]
fn the_bulk_switch_picks_the_sub_budget_in_both_directions() {
    assert_eq!(
        form_request("ai budget set --bulk").cmd,
        Cmd::BudgetForm {
            generation: 5,
            account_id: 0,
            class: Class::Bulk,
        }
    );
    let Cmd::BudgetSet { class, .. } = request("ai budget set --bulk --daily-hard-usd=5!").cmd
    else {
        panic!("expected a set");
    };
    assert_eq!(class, Class::Bulk);
}

#[test]
fn a_banged_budget_set_sends_only_what_was_typed() {
    // The other half of the acceptance, and the reason the form exists: with a
    // bang this is the CLI's replace-semantics verbatim — one cap on the line
    // means one cap in force and every other one cleared. The bang is the
    // opting out, which is why it has to be typed.
    assert_eq!(
        request("ai budget set --daily-hard-usd=5!").cmd,
        Cmd::BudgetSet {
            account_id: 0,
            class: Class::All,
            caps: vec![("daily-hard-usd".to_owned(), "5".to_owned())],
        }
    );
}

#[test]
fn a_cap_that_is_not_a_number_is_refused_where_it_was_typed() {
    for line in [
        "ai budget set --daily-hard-usd=five!",
        "ai budget set --daily-soft-tokens=lots!",
        // Negative is refused for the same reason: the enforcer compares `>=`,
        // so a negative cap blocks everything, which nobody types on purpose.
        "ai budget set --monthly-hard-usd=-1!",
    ] {
        let why = refusal(line, &screen());
        assert!(why.contains("a number, at least zero"), "{line}: {why}");
    }
}

#[test]
fn zero_is_a_cap_and_not_an_absent_one() {
    // `--daily-hard-usd 0` forbids all spending; omitting it forbids none. A
    // validator treating zero as missing would make the strictest budget
    // anybody can set unexpressible.
    assert_eq!(
        request("ai budget set --daily-hard-usd=0!").cmd,
        Cmd::BudgetSet {
            account_id: 0,
            class: Class::All,
            caps: vec![("daily-hard-usd".to_owned(), "0".to_owned())],
        }
    );
}

// ---------------------------------------------------------------------------
// provider
// ---------------------------------------------------------------------------

#[test]
fn a_backend_is_named_or_the_verb_refuses() {
    for (text, expected) in [
        ("claude", Provider::Claude),
        ("local", Provider::Local),
        ("clear", Provider::Inherit),
        ("inherit", Provider::Inherit),
    ] {
        assert_eq!(
            request(&format!("ai provider set {text}")).cmd,
            Cmd::ProviderSet {
                account_id: 0,
                provider: expected,
            },
            "{text}"
        );
    }
    assert!(refusal("ai provider set", &screen()).contains("name a backend"));
    let why = refusal("ai provider set openai", &screen());
    assert!(why.contains("claude, local, clear"), "{why}");
}

// ---------------------------------------------------------------------------
// safety
// ---------------------------------------------------------------------------

#[test]
fn a_scan_and_a_confirmation_both_need_a_message() {
    assert_eq!(
        request("ai scan").cmd,
        Cmd::ScanInjection {
            generation: 5,
            message_id: 10,
        }
    );
    assert_eq!(
        request("ai confirm").cmd,
        Cmd::ConfirmInjection {
            generation: 5,
            message_id: 10,
            confirm: Confirm::Release,
        }
    );
    assert_eq!(
        request("ai confirm --revoke").cmd,
        Cmd::ConfirmInjection {
            generation: 5,
            message_id: 10,
            confirm: Confirm::Revoke,
        }
    );
    let no_message = Target {
        message_id: None,
        ..screen()
    };
    for line in ["ai scan", "ai confirm"] {
        assert!(refusal(line, &no_message).contains("message"), "{line}");
    }
}

#[test]
fn a_scan_and_a_confirmation_share_one_column_layout() {
    // `ConfirmInjection` answers with the same `ScanInjectionResponse` a scan
    // does — confirming is a state change reported as a fresh scan — so two
    // layouts would be two chances to disagree about what a column means.
    assert_eq!(request("ai confirm").columns, request("ai scan").columns);
}

// ---------------------------------------------------------------------------
// the ledger
// ---------------------------------------------------------------------------

#[test]
fn the_ledger_filters_are_all_reachable_from_one_line() {
    assert_eq!(
        request("ai audit").cmd,
        Cmd::AuditQuery {
            generation: 5,
            account_id: 0,
            model: None,
            failed_only: false,
            whole_ledger: false,
        }
    );
    assert_eq!(
        request("ai audit --account 3 --model claude-opus-4 --failed --all").cmd,
        Cmd::AuditQuery {
            generation: 5,
            account_id: 3,
            model: Some("claude-opus-4".to_owned()),
            failed_only: true,
            whole_ledger: true,
        }
    );
}

// ---------------------------------------------------------------------------
// spend against caps: colour and glyph
// ---------------------------------------------------------------------------

#[test]
fn spend_is_drawn_against_the_cap_it_is_measured_by() {
    // The acceptance's "soft/hard color *and* glyph". The tone is the colour
    // (`tui::view::tone_style`) and `ReportTone::glyph` is the character, so
    // asserting the tone asserts both — and the glyph is what carries the
    // meaning on a monochrome terminal.
    let dollars = |rows: &[crate::tui::report::ReportRow]| {
        rows.iter()
            .find(|row| {
                row.cells
                    .get(1)
                    .is_some_and(|cell| cell.starts_with("today"))
            })
            .cloned()
            .expect("a daily dollars row")
    };

    // Spent $1.50. Under both caps.
    let row = dollars(&wire::budget_rows(&spend(daily(Some(5.0), Some(10.0)))));
    assert_eq!(row.tone, ReportTone::Ok);
    assert_eq!(row.tone.glyph(), "✓");
    assert_eq!(row.cells.last().map(String::as_str), Some("under"));

    // At or above the soft cap: the model is being downgraded, not blocked.
    let row = dollars(&wire::budget_rows(&spend(daily(Some(1.5), Some(10.0)))));
    assert_eq!(row.tone, ReportTone::Warn);
    assert_eq!(row.tone.glyph(), "!");
    assert_eq!(row.cells.last().map(String::as_str), Some("downgrading"));

    // At or above the hard cap. Hard wins, because a scope past both is
    // blocked — reporting the softer verdict would understate it.
    let row = dollars(&wire::budget_rows(&spend(daily(Some(1.0), Some(1.5)))));
    assert_eq!(row.tone, ReportTone::Bad);
    assert_eq!(row.tone.glyph(), "✗");
    assert_eq!(row.cells.last().map(String::as_str), Some("blocked"));

    // No cap at all is muted and says so: unlimited is a configuration, not a
    // warning, and drawing it as one would make a default install permanently
    // yellow.
    let row = dollars(&wire::budget_rows(&spend(daily(None, None))));
    assert_eq!(row.tone, ReportTone::Muted);
    assert_eq!(row.cells.last().map(String::as_str), Some("no cap"));
    assert_eq!(row.cells.get(4).map(String::as_str), Some("-"));
}

#[test]
fn tokens_and_dollars_are_judged_separately() {
    // The reason the report is eight rows rather than four: this scope is over
    // its token cap and under its dollar cap, and one row for both would have
    // to report one of the two answers wrongly.
    let caps = BudgetCaps {
        daily: Some(BudgetWindowCaps {
            soft_usd: None,
            hard_usd: Some(100.0),
            soft_tokens: None,
            hard_tokens: Some(500),
        }),
        monthly: None,
    };
    let rows = wire::budget_rows(&spend(caps));
    let today: Vec<_> = rows
        .iter()
        .filter(|row| {
            row.cells
                .get(1)
                .is_some_and(|cell| cell.starts_with("today"))
        })
        .collect();
    assert_eq!(today.len(), 2);
    assert_eq!(today[0].cells.get(2).map(String::as_str), Some("dollars"));
    assert_eq!(today[0].tone, ReportTone::Ok);
    assert_eq!(today[1].cells.get(2).map(String::as_str), Some("tokens"));
    assert_eq!(today[1].tone, ReportTone::Bad);
}

#[test]
fn the_class_row_says_whether_an_operator_set_the_caps() {
    // "Unset" and "set to exactly the configured default" behave identically
    // until the configuration changes, so a reader deciding whether to edit
    // them has to be able to tell which one they are looking at.
    let mut response = spend(daily(Some(5.0), None));
    let rows = wire::budget_rows(&response);
    assert_eq!(
        rows.first().map(|row| row.cells[0].clone()),
        Some("all (set)".to_owned())
    );
    if let Some(all) = response.all.as_mut() {
        all.stored = false;
    }
    let rows = wire::budget_rows(&response);
    assert_eq!(
        rows.first().map(|row| row.cells[0].clone()),
        Some("all (ai.limits)".to_owned())
    );
}

// ---------------------------------------------------------------------------
// dispatch, end to end
// ---------------------------------------------------------------------------

#[test]
fn a_bare_budget_set_opens_a_form_and_a_banged_one_does_not() {
    let mut model = loaded();
    let cmds = run(&mut model, "ai budget set");
    assert!(
        matches!(cmds.first(), Some(Cmd::BudgetForm { .. })),
        "{cmds:?}"
    );
    assert!(matches!(model.overlay_top(), Some(Overlay::Form(_))));

    let mut model = loaded();
    let cmds = run(&mut model, "ai budget set --daily-hard-usd=5!");
    assert!(
        matches!(cmds.first(), Some(Cmd::BudgetSet { .. })),
        "{cmds:?}"
    );
    assert!(!model.overlay_is_open());
}

#[test]
fn an_unfilled_form_refuses_to_replace_what_it_could_not_read() {
    // The whole hazard in one test. `SetBudget` replaces a scope's budget, so
    // applying before the read landed would clear every cap in force — and it
    // would look like a successful edit.
    let mut model = loaded();
    run(&mut model, "ai budget set");
    let apply = rows_of(&model) - 1;
    let cmds = walk_to(&mut model, apply);
    assert!(cmds.is_empty(), "{cmds:?}");
    let cmds = update(&mut model, Msg::Key(Key::Enter));
    assert!(cmds.is_empty(), "nothing may be issued: {cmds:?}");
    assert!(
        matches!(model.overlay_top(), Some(Overlay::Form(_))),
        "the form stays up"
    );
    assert!(
        model.status.contains("nothing to replace"),
        "{}",
        model.status
    );
}

#[test]
fn a_bare_budget_set_applies_every_cap_the_daemon_reported() {
    // The acceptance, end to end: a line naming one cap opens a form holding
    // *all* of them, and applying sends all of them. Sending only the typed one
    // would delete the rest — which is what makes this the reason the form is
    // here rather than a nicety.
    let mut model = loaded();
    let generation = opened(
        &mut model,
        "ai budget set --account 3 --bulk --daily-hard-usd=5",
    );
    update(
        &mut model,
        Msg::Form {
            generation,
            event: FormEvent::Fields(vec![
                ("daily-hard-usd".to_owned(), "9".to_owned()),
                ("monthly-hard-usd".to_owned(), "50".to_owned()),
                ("daily-soft-tokens".to_owned(), "1000".to_owned()),
            ]),
        },
    );
    let apply = rows_of(&model) - 1;
    walk_to(&mut model, apply);
    let cmds: Vec<Cmd> = update(&mut model, Msg::Key(Key::Enter))
        .into_iter()
        .filter(|cmd| !matches!(cmd, Cmd::SaveHistory { .. }))
        .collect();
    assert_eq!(
        cmds,
        vec![Cmd::BudgetSet {
            // The scope and the class the line named, carried through the form:
            // dropping them would replace the *global* budget for a line that
            // asked for account 3's bulk one.
            account_id: 3,
            class: Class::Bulk,
            caps: vec![
                // The typed flag wins over what the daemon reported for that
                // field — "flags pre-fill the form" — and every other cap in
                // force comes along so applying does not clear it.
                ("daily-hard-usd".to_owned(), "5".to_owned()),
                ("daily-soft-tokens".to_owned(), "1000".to_owned()),
                ("monthly-hard-usd".to_owned(), "50".to_owned()),
            ],
        }]
    );
    assert!(!model.overlay_is_open(), "applying closes the form");
}

#[test]
fn a_value_the_verb_will_not_accept_is_refused_on_the_form() {
    // Refused where it was typed, with the field still on screen and still
    // editable. Refusing after the form closed would take the value that caused
    // it away with the form, which is the opposite of useful.
    let mut model = loaded();
    let generation = opened(&mut model, "ai budget set");
    update(
        &mut model,
        Msg::Form {
            generation,
            // The first field, which is the one the cursor opens on: `CAPS`
            // orders soft before hard.
            event: FormEvent::Fields(vec![("daily-soft-usd".to_owned(), "5".to_owned())]),
        },
    );
    update(&mut model, Msg::Key(Key::Enter));
    for c in "abc".chars() {
        update(&mut model, Msg::Key(Key::Char(c)));
    }
    update(&mut model, Msg::Key(Key::Enter));
    let apply = rows_of(&model) - 1;
    walk_to(&mut model, apply);
    let cmds = update(&mut model, Msg::Key(Key::Enter));
    assert!(cmds.is_empty(), "{cmds:?}");
    let Some(Overlay::Form(pane)) = model.overlay_top() else {
        panic!("the form stays up");
    };
    assert_eq!(pane.fields[0].value, "5abc", "and keeps what was typed");
    assert!(
        pane.error
            .as_ref()
            .is_some_and(|why| why.contains("a number, at least zero")),
        "{:?}",
        pane.error
    );
}

#[test]
fn a_failed_read_leaves_the_form_un_appliable() {
    // A form that could not see the caps in force is exactly the form that must
    // not replace them.
    let mut model = loaded();
    let generation = opened(&mut model, "ai budget set");
    update(
        &mut model,
        Msg::Form {
            generation,
            event: FormEvent::Failed("the daemon is not running".to_owned()),
        },
    );
    let apply = rows_of(&model) - 1;
    walk_to(&mut model, apply);
    let cmds = update(&mut model, Msg::Key(Key::Enter));
    assert!(cmds.is_empty(), "{cmds:?}");
    assert!(matches!(model.overlay_top(), Some(Overlay::Form(_))));
}

#[test]
fn an_answer_to_a_superseded_read_is_dropped() {
    let mut model = loaded();
    let generation = opened(&mut model, "ai budget set");
    update(
        &mut model,
        Msg::Form {
            generation: generation + 1,
            event: FormEvent::Fields(vec![("daily-hard-usd".to_owned(), "9".to_owned())]),
        },
    );
    let Some(Overlay::Form(pane)) = model.overlay_top() else {
        panic!("expected a form");
    };
    assert!(!pane.ready, "a stale answer must not fill the form");
    assert!(pane.fields.iter().all(|field| field.value.is_empty()));
}

#[test]
fn a_form_is_menu_until_a_field_is_open_and_insert_while_it_is() {
    let mut model = loaded();
    run(&mut model, "ai budget set");
    assert_eq!(model.mode(), Mode::Menu);
    update(&mut model, Msg::Key(Key::Enter));
    assert_eq!(model.mode(), Mode::Insert);
    // Typing is text, not commands: `j` in a field is a `j`, and the field it
    // lands in is the one that was opened.
    for c in "12".chars() {
        update(&mut model, Msg::Key(Key::Char(c)));
    }
    update(&mut model, Msg::Key(Key::Enter));
    assert_eq!(model.mode(), Mode::Menu);
    let Some(Overlay::Form(pane)) = model.overlay_top() else {
        panic!("expected a form");
    };
    assert_eq!(
        pane.fields.first().map(|field| field.value.as_str()),
        Some("12")
    );
}

#[test]
fn esc_cancels_the_field_before_it_cancels_the_form() {
    let mut model = loaded();
    let generation = opened(&mut model, "ai budget set");
    update(
        &mut model,
        Msg::Form {
            generation,
            event: FormEvent::Fields(vec![("daily-soft-usd".to_owned(), "3".to_owned())]),
        },
    );
    update(&mut model, Msg::Key(Key::Enter));
    update(&mut model, Msg::Key(Key::Char('9')));
    update(&mut model, Msg::Key(Key::Esc));
    let Some(Overlay::Form(pane)) = model.overlay_top() else {
        panic!("the form stays up: one Esc is one layer");
    };
    assert_eq!(
        pane.fields.first().map(|field| field.value.as_str()),
        Some("3"),
        "the value is put back"
    );
    update(&mut model, Msg::Key(Key::Esc));
    assert!(!model.overlay_is_open());
}

/// Type `line`, and return the generation the form's read was stamped with.
///
/// Read back off the issued command rather than from the model, which is what
/// every other dispatch test here does: `Model::generation` is private, and a
/// test that reached into it would be asserting against a number nothing on
/// the wire ever sees.
fn opened(model: &mut Model, line: &str) -> u64 {
    let cmds = run(model, line);
    match cmds.first() {
        Some(Cmd::BudgetForm { generation, .. }) => *generation,
        other => panic!("{line:?} did not open a form: {other:?}"),
    }
}

/// How many rows the open form has.
fn rows_of(model: &Model) -> usize {
    match model.overlay_top() {
        Some(Overlay::Form(pane)) => pane.rows(),
        other => panic!("expected a form: {other:?}"),
    }
}

/// Walk the cursor down to `at`, returning whatever the last step issued.
fn walk_to(model: &mut Model, at: usize) -> Vec<Cmd> {
    let mut cmds = Vec::new();
    for _ in 0..at {
        cmds = update(model, Msg::Key(Key::Char('j')));
    }
    cmds
}
