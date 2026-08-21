//! Task 95's rule verbs: the draft-then-store flow, the two things that look
//! like dry runs and are not, and the correction that teaches one criterion.
//!
//! `panic!` in a branch that cannot happen reads better here than the
//! `unreachable!` dance, and this module is test-only — the same exemption
//! `tui::model::tests` takes.
#![allow(clippy::panic)]

use rmail_core::command::{self, Resolution};
use rmail_core::keymap::Key;

use super::*;
use crate::tui::model::{update, Account, Folder, MessageRow, Model, Msg, Overlay};

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
        other => panic!("{line:?} was refused: {other:?}"),
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

// ---------------------------------------------------------------------------
// listing, and drafting from words
// ---------------------------------------------------------------------------

#[test]
fn rule_list_needs_an_account() {
    assert_eq!(
        request("rule list").cmd,
        Cmd::RuleList {
            generation: 5,
            account_id: 7,
        }
    );
    assert!(refusal("rule list", &empty()).contains("account"));
}

#[test]
fn rule_new_joins_the_whole_instruction() {
    // An unquoted sentence is what somebody types, and synthesizing from only
    // its first word is the silent truncation `:helpgrep`'s own docs call out.
    assert_eq!(
        request("rule new archive newsletters from marketing").cmd,
        Cmd::RuleSynthesize {
            generation: 5,
            account_id: 7,
            instruction: "archive newsletters from marketing".to_owned(),
            days: None,
        }
    );
}

#[test]
fn rule_new_refuses_with_nothing_to_synthesize_from() {
    let why = refusal("rule new", &screen());
    assert!(why.contains("what the rule should do"), "{why}");
}

#[test]
fn a_days_window_that_is_not_a_number_is_refused_rather_than_defaulted() {
    // Defaulting would run a backtest over the daemon's window while the caller
    // believed they had asked for another — an answer about the wrong period,
    // presented as an answer.
    for line in [
        "rule new archive news --days soon",
        "rule backtest x --days soon",
    ] {
        let why = refusal(line, &screen());
        assert!(why.contains("whole number of days"), "{line}: {why}");
    }
    let Cmd::RuleSynthesize { days, .. } = request("rule new archive news --days 30").cmd else {
        panic!("expected a synthesis");
    };
    assert_eq!(days, Some(30));
}

// ---------------------------------------------------------------------------
// the draft, and storing it
// ---------------------------------------------------------------------------

#[test]
fn rule_add_refuses_until_something_has_been_drafted() {
    let why = refusal("rule add", &screen());
    assert!(why.contains(":rule new"), "{why}");
}

#[test]
fn rule_add_stores_the_draft_verbatim() {
    let drafted = Target {
        rule_draft: Some("name = \"news\"\n".to_owned()),
        ..screen()
    };
    assert_eq!(
        request_on("rule add", &drafted).cmd,
        Cmd::RuleCreate {
            account_id: 7,
            toml: "name = \"news\"\n".to_owned(),
        }
    );
}

#[test]
fn a_synthesis_leaves_a_draft_the_next_rule_add_stores() {
    // The flow the verb exists for, end to end through the state machine: draft
    // from words, read the dry run, store it. Two commands, no filesystem.
    let mut model = loaded();
    let cmds = run(&mut model, "rule new archive newsletters");
    assert!(
        matches!(cmds.first(), Some(Cmd::RuleSynthesize { .. })),
        "{cmds:?}"
    );
    assert!(
        model.rule_draft.is_none(),
        "nothing is drafted until the daemon answers"
    );

    update(&mut model, Msg::RuleDrafted("name = \"news\"\n".to_owned()));
    assert_eq!(model.rule_draft.as_deref(), Some("name = \"news\"\n"));

    // The draft outlives the report it was shown in, which is the reason it is
    // session state rather than a field on the pane.
    update(&mut model, Msg::Key(Key::Esc));
    assert!(!model.overlay_is_open());
    let cmds = run(&mut model, "rule add");
    assert_eq!(
        cmds,
        vec![Cmd::RuleCreate {
            account_id: 7,
            toml: "name = \"news\"\n".to_owned(),
        }]
    );
}

#[test]
fn a_second_draft_replaces_the_first() {
    // `:rule add` stores "the draft", and two of them would make which one it
    // meant depend on the order two reports happened to answer in.
    let mut model = loaded();
    update(&mut model, Msg::RuleDrafted("first".to_owned()));
    update(&mut model, Msg::RuleDrafted("second".to_owned()));
    assert_eq!(model.rule_draft.as_deref(), Some("second"));
}

#[test]
fn a_draft_never_touches_the_inflight_count() {
    // Nothing finished when a draft arrives, and `Msg::Done` would decrement a
    // counter this never incremented.
    let mut model = loaded();
    model.inflight = 2;
    update(&mut model, Msg::RuleDrafted("x".to_owned()));
    assert_eq!(model.inflight, 2);
}

// ---------------------------------------------------------------------------
// a dry run is not a backtest
// ---------------------------------------------------------------------------

#[test]
fn rule_run_evaluates_the_selection_and_backtest_replays_history() {
    assert_eq!(
        request("rule run").cmd,
        Cmd::RuleEvaluate {
            generation: 5,
            account_id: 7,
            message_ids: vec![10, 11],
            rule: None,
        }
    );
    assert_eq!(
        request("rule run --rule newsletters").cmd,
        Cmd::RuleEvaluate {
            generation: 5,
            account_id: 7,
            message_ids: vec![10, 11],
            rule: Some("newsletters".to_owned()),
        }
    );
    assert_eq!(
        request("rule backtest newsletters --days 14").cmd,
        Cmd::RuleBacktest {
            generation: 5,
            account_id: 7,
            name: "newsletters".to_owned(),
            days: Some(14),
        }
    );
}

#[test]
fn rule_run_refuses_with_nothing_selected_and_backtest_refuses_with_no_name() {
    // An account but no selection: `empty()` would refuse for the account first,
    // which is a different refusal and would make this pass for the wrong reason.
    let nothing_selected = Target {
        selection: Vec::new(),
        ..screen()
    };
    assert!(refusal("rule run", &nothing_selected).contains("message"));
    assert!(refusal("rule backtest", &screen()).contains("name a rule"));
}

#[test]
fn every_outcome_table_shares_one_column_layout() {
    // Three verbs answering with the same `MessageOutcome`: three layouts would
    // be three chances to disagree about what a column means.
    let run = request("rule run").columns;
    assert_eq!(request("rule backtest x").columns, run);
    assert_eq!(request("rule new archive news").columns, run);
}

// ---------------------------------------------------------------------------
// corrections
// ---------------------------------------------------------------------------

#[test]
fn rule_correct_records_the_criterion_and_which_way_it_should_have_gone() {
    assert_eq!(
        request("rule correct \"is a newsletter\"").cmd,
        Cmd::RuleCorrect {
            account_id: 7,
            message_id: 10,
            prompt: "is a newsletter".to_owned(),
            expected: true,
        }
    );
    assert_eq!(
        request("rule correct \"is a newsletter\" --no").cmd,
        Cmd::RuleCorrect {
            account_id: 7,
            message_id: 10,
            prompt: "is a newsletter".to_owned(),
            expected: false,
        }
    );
}

#[test]
fn rule_correct_refuses_without_a_criterion_or_a_message() {
    assert!(refusal("rule correct", &screen()).contains("criterion"));
    assert!(refusal("rule correct \"is a newsletter\"", &empty()).contains("account"));
    let no_message = Target {
        message_id: None,
        ..screen()
    };
    assert!(refusal("rule correct \"is a newsletter\"", &no_message).contains("message"));
}

// ---------------------------------------------------------------------------
// dispatch
// ---------------------------------------------------------------------------

#[test]
fn a_rule_table_opens_a_report_and_a_rule_fact_does_not() {
    let mut model = loaded();
    let cmds = run(&mut model, "rule list");
    assert!(
        matches!(cmds.first(), Some(Cmd::RuleList { .. })),
        "{cmds:?}"
    );
    assert!(matches!(model.overlay_top(), Some(Overlay::Report(_))));

    let mut model = loaded();
    update(&mut model, Msg::RuleDrafted("x".to_owned()));
    let cmds = run(&mut model, "rule add");
    assert!(
        matches!(cmds.first(), Some(Cmd::RuleCreate { .. })),
        "{cmds:?}"
    );
    assert!(!model.overlay_is_open());
}

#[test]
fn no_rule_verb_asks_before_it_runs() {
    // Every one of these mutates something, and none asks — task 89's rule that
    // a `:` line typed in full is already the deliberate act. The dangerous one,
    // `:rule add`, is guarded differently and better: it can only store a draft
    // whose dry run has already been on screen.
    for line in [
        "rule list",
        "rule new archive news",
        "rule run",
        "rule backtest x",
        "rule correct \"is news\"",
    ] {
        match asked(line, &screen()) {
            Answer::Rows(request) | Answer::Fact(request) => {
                assert!(request.confirm.is_none(), "{line} asks");
            }
            other => panic!("{line}: {other:?}"),
        }
    }
}
