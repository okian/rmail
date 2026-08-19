//! Task 94's daemon verbs: every one answered, each with the request it claims,
//! and the one that asks before it runs.
//!
//! Two halves. The table is a pure function, so most of this is `answer()` in,
//! `Request` out — no `Model`, no terminal, no daemon. The dispatch half drives
//! `tui::model::update` end to end, because "the report opens", "the fact lands
//! on the status line" and "the bang skips the question" are claims about the
//! state machine rather than about the table.
//!
//! `panic!` in a branch that cannot happen reads better here than the
//! `unreachable!` dance, and this module is test-only — the same exemption
//! `tui::model::tests` takes.
#![allow(clippy::panic)]

use std::collections::BTreeSet;

use rmail_core::command::{self, Resolution};
use rmail_core::keymap::{Key, Mode};
use rmail_core::parity::Command as Capability;

use super::*;
use crate::tui::model::{
    update, Account, Confirmed, Folder, MessageRow, Model, Msg, Overlay, ReportEvent,
};
use crate::tui::report::{ReportFill, ReportRow};

// ---------------------------------------------------------------------------
// fixtures
// ---------------------------------------------------------------------------

fn invocation(line: &str) -> Invocation {
    match command::parse(line) {
        Ok(Resolution::Invocation(invocation)) => *invocation,
        other => panic!("{line:?} does not parse to an invocation: {other:?}"),
    }
}

/// A screen with everything a verb might ask for.
fn screen() -> Target {
    Target {
        account_id: 7,
        mailbox_id: Some(1),
        message_id: Some(10),
    }
}

/// A screen with nothing loaded yet.
fn empty() -> Target {
    Target {
        account_id: 0,
        mailbox_id: None,
        message_id: None,
    }
}

fn asked(line: &str, target: &Target) -> Answer {
    match answer(&invocation(line), target, 5) {
        Some(answer) => answer,
        None => panic!("{line:?} has no answer"),
    }
}

fn request(line: &str) -> Request {
    match asked(line, &screen()) {
        Answer::Rows(request) | Answer::Fact(request) => *request,
        other => panic!("{line:?} was refused: {other:?}"),
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
        message_count: 3,
    }];
    model.open_folder = Some(1);
    model.messages = (10..13)
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

/// Type `line` on the command line and run it.
fn run(model: &mut Model, line: &str) -> Vec<Cmd> {
    update(model, Msg::Key(Key::Char(':')));
    for c in line.chars() {
        update(model, Msg::Key(Key::Char(c)));
    }
    update(model, Msg::Key(Key::Enter))
}

/// The command a `:` line issued, ignoring the history write that rides along.
fn issued(cmds: &[Cmd]) -> Vec<Cmd> {
    cmds.iter()
        .filter(|cmd| !matches!(cmd, Cmd::SaveHistory { .. }))
        .cloned()
        .collect()
}

/// `verb`'s path plus one placeholder per declared positional.
///
/// A verb that requires an argument does not parse without one, and these tests
/// are about what a verb *answers*, not about the parser — which
/// `command::tests` already covers.
fn with_arguments(verb: &rmail_core::command::Verb) -> String {
    let mut line = verb.canonical();
    for positional in verb.positionals {
        line.push(' ');
        line.push_str(positional.name);
    }
    line
}

fn open_pane(model: &Model) -> &crate::tui::report::ReportPane {
    match model.overlay.as_ref() {
        Some(Overlay::Report(pane)) => pane,
        other => panic!("expected a report, found {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// coverage: every declared verb is answered
// ---------------------------------------------------------------------------

/// Every verb the registry declares with a capability and no action has an
/// answer here, or the `:` line that reaches it says "no answer for it".
///
/// The drift check this module exists to make possible. A verb declared in
/// `command::explicit` for a later task is a real state and answers `None`; what
/// this refuses is the *other* direction — a verb answered here that the
/// registry does not have, which would be a table entry nobody can reach.
#[test]
fn every_answer_belongs_to_a_verb_the_registry_declares() {
    // Walk the verbs, ask each one, and collect those with an answer. Then
    // assert the set is exactly the ones this build claims — spelled out, so a
    // verb added to the table without a registry entry fails by name.
    let mut answered = BTreeSet::new();
    for verb in command::children_of(&[]) {
        if verb.action.is_some() || verb.capability.is_none() {
            continue;
        }
        // Asked with a placeholder argument per declared positional, because a
        // verb that requires one does not parse without it — and the question
        // here is "is it answered", not "does it parse bare".
        let path = verb.canonical();
        let line = with_arguments(verb);
        if answer(&invocation(&line), &screen(), 1).is_some() {
            answered.insert(path);
        }
    }
    let expected: BTreeSet<String> = [
        "auth clear",
        "auth status",
        "ai cost",
        "ai pause",
        "ai process",
        "ai resume",
        "ai retry",
        "ai status",
        "finder rebuild",
        "finder status",
        "index entities",
        "index gc",
        "index rebuild",
        "index reindex",
        "index run",
        "index start",
        "index status",
        "index stop",
        "index verify",
        "sync now",
        "sync pause",
        "sync resume",
        "sync status",
    ]
    .iter()
    .map(|verb| (*verb).to_owned())
    .collect();
    assert_eq!(answered, expected);
}

/// Every capability the acceptance names is reached by one of these verbs.
///
/// Read off `parity::Command` rather than from a list, so a capability whose
/// verb was forgotten fails here rather than being noticed by a reader.
#[test]
fn every_capability_task_94_names_is_reachable_by_a_verb() {
    let wanted = [
        Capability::IndexStatus,
        Capability::IndexReindex,
        Capability::IndexRebuild,
        Capability::IndexVerify,
        Capability::IndexGc,
        Capability::IndexSetPaused,
        Capability::IndexListEntities,
        Capability::SyncSyncFolder,
        Capability::SyncStatus,
        Capability::SyncPause,
        Capability::SyncResume,
        Capability::AiGetUsage,
        Capability::AiSetPaused,
        Capability::AiRetryFailed,
        Capability::AiAnalyzeMessage,
        Capability::FinderRebuildIndex,
        Capability::FinderIndexStatus,
    ];
    for capability in wanted {
        let reached = command::children_of(&[]).into_iter().any(|verb| {
            verb.capability == Some(capability)
                && answer(&invocation(&with_arguments(verb)), &screen(), 1).is_some()
        });
        assert!(
            reached,
            "{} is named by task 94's acceptance and no verb answers for it",
            capability.name()
        );
    }
}

// ---------------------------------------------------------------------------
// each verb's own request
// ---------------------------------------------------------------------------

#[test]
fn the_index_verbs_issue_the_rpc_they_name() {
    assert_eq!(
        request("index status").cmd,
        Cmd::IndexStatus { generation: 5 }
    );
    assert_eq!(
        request("index verify").cmd,
        Cmd::IndexVerify { generation: 5 }
    );
    assert_eq!(request("index gc").cmd, Cmd::IndexGc { generation: 5 });
    assert_eq!(
        request("index entities email").cmd,
        Cmd::IndexEntities {
            generation: 5,
            kind: "email".to_owned(),
        }
    );
    assert_eq!(
        request("index rebuild!").cmd,
        Cmd::IndexRebuild { generation: 5 }
    );
}

#[test]
fn run_drains_and_reindex_targets_the_open_folder() {
    // Two verbs over one RPC, which the CLI also spells as two — the difference
    // is the mode, and a TUI collapsing them into a flag would be the surface
    // where the spelling diverged.
    assert_eq!(
        request("index run").cmd,
        Cmd::IndexReindex {
            generation: 5,
            mode: Reindex::Drain,
            mailbox_id: None,
        }
    );
    assert_eq!(
        request("index reindex").cmd,
        Cmd::IndexReindex {
            generation: 5,
            mode: Reindex::Selection,
            mailbox_id: Some(1),
        }
    );
}

#[test]
fn start_and_stop_are_one_rpc_read_in_two_directions() {
    assert_eq!(
        request("index stop").cmd,
        Cmd::IndexSetPaused { pause: Pause::Stop }
    );
    assert_eq!(
        request("index start").cmd,
        Cmd::IndexSetPaused {
            pause: Pause::Start
        }
    );
    assert!(Pause::Stop.paused(), "stop is the paused direction");
    assert!(!Pause::Start.paused());
}

#[test]
fn the_sync_verbs_carry_the_account_on_screen() {
    assert_eq!(
        request("sync status").cmd,
        Cmd::SyncStatusReport {
            generation: 5,
            account_id: 7,
        }
    );
    assert_eq!(
        request("sync now").cmd,
        Cmd::SyncNow {
            generation: 5,
            account_id: 7,
        }
    );
    assert_eq!(
        request("sync pause").cmd,
        Cmd::SyncSetPaused {
            account_id: 7,
            pause: Pause::Stop,
        }
    );
}

#[test]
fn ai_status_and_ai_cost_are_two_views_of_one_rpc() {
    assert_eq!(
        request("ai status").cmd,
        Cmd::AiUsage {
            generation: 5,
            costs: false,
        }
    );
    assert_eq!(
        request("ai cost").cmd,
        Cmd::AiUsage {
            generation: 5,
            costs: true,
        }
    );
    assert_ne!(
        request("ai status").columns,
        request("ai cost").columns,
        "one RPC, two column layouts — which is the whole reason they are two \
         verbs rather than one"
    );
}

#[test]
fn the_finder_verbs_issue_their_own_rpcs() {
    assert_eq!(
        request("finder status").cmd,
        Cmd::FinderStatus { generation: 5 }
    );
    assert_eq!(request("finder rebuild").cmd, Cmd::FinderRebuild);
}

#[test]
fn the_three_streaming_verbs_share_one_column_layout() {
    // One shape, because the RPCs answer with the same `IndexProgress`: three
    // layouts over one message would be three chances to disagree about what
    // `remaining` means.
    let run = request("index run").columns;
    assert_eq!(request("index reindex").columns, run);
    assert_eq!(request("index rebuild!").columns, run);
}

// ---------------------------------------------------------------------------
// rows or a fact
// ---------------------------------------------------------------------------

#[test]
fn a_verb_answering_with_a_table_opens_a_report_and_one_with_a_fact_does_not() {
    for line in [
        "index status",
        "index verify",
        "index gc",
        "index entities email",
        "index run",
        "sync status",
        "sync now",
        "ai status",
        "ai cost",
        "ai process",
        "finder status",
        "auth status",
    ] {
        assert!(
            matches!(asked(line, &screen()), Answer::Rows(_)),
            "{line} answers with a table"
        );
    }
    for line in [
        "index start",
        "index stop",
        "sync pause",
        "sync resume",
        "ai pause",
        "ai resume",
        "ai retry",
        "finder rebuild",
        "auth clear",
    ] {
        assert!(
            matches!(asked(line, &screen()), Answer::Fact(_)),
            "{line} answers with a fact"
        );
    }
}

#[test]
fn a_fact_declares_no_columns_and_a_table_declares_some() {
    // The renderer tells them apart by the columns, so an empty list on a table
    // would draw a Report with no grid and a non-empty one on a fact would draw
    // a Report nothing ever fills.
    assert!(request("ai retry").columns.is_empty());
    assert!(!request("ai status").columns.is_empty());
}

#[test]
fn mutating_is_not_what_decides_between_them() {
    // `:sync now` mutates and answers with a row per folder; reducing that to
    // "synced" would throw away the one thing somebody ran it to see.
    assert!(
        crate::tui::report::mutates(&invocation("sync now")),
        "sync now reaches a mutating capability"
    );
    assert!(matches!(asked("sync now", &screen()), Answer::Rows(_)));
}

// ---------------------------------------------------------------------------
// what the screen does not have
// ---------------------------------------------------------------------------

#[test]
fn a_verb_needing_an_account_refuses_before_the_round_trip() {
    for line in ["sync status", "sync now", "sync pause", "sync resume"] {
        match asked(line, &empty()) {
            Answer::Refused(why) => assert!(why.contains("account"), "{line}: {why}"),
            other => panic!("{line} should refuse with no account: {other:?}"),
        }
    }
}

#[test]
fn entities_refuses_a_missing_kind_rather_than_asking_for_everything() {
    // The positional is optional so the verb stays typeable — the registry is
    // also the command index, and a row nobody can type documents nothing — so
    // the refusal is this side's job.
    match asked("index entities", &screen()) {
        Answer::Refused(why) => assert!(why.contains("kind"), "{why}"),
        other => panic!("expected a refusal, found {other:?}"),
    }
}

#[test]
fn reindex_refuses_with_no_folder_open_and_names_what_is_missing() {
    match asked("index reindex", &empty()) {
        Answer::Refused(why) => assert!(why.contains("folder"), "{why}"),
        other => panic!("expected a refusal, found {other:?}"),
    }
    // And `index run` does not: draining the queue is not about a folder.
    assert!(matches!(asked("index run", &empty()), Answer::Rows(_)));
}

#[test]
fn ai_process_refuses_with_no_message_selected() {
    match asked("ai process", &empty()) {
        Answer::Refused(why) => assert!(why.contains("message"), "{why}"),
        other => panic!("expected a refusal, found {other:?}"),
    }
}

#[test]
fn a_verb_this_build_has_no_answer_for_is_not_a_refusal() {
    // The two mean different things to the caller: `None` is "no answer for that
    // verb", and reporting a missing account as that would send somebody looking
    // for a feature that is present and has nothing to act on yet.
    //
    // Built by hand because the registry has no such verb yet — which is the
    // point: this is the exact shape task 95's `:tag add` will arrive in, and
    // the table has to answer `None` for it rather than guessing.
    let later_task = Invocation {
        range: None,
        verb: vec!["tag".to_owned(), "add".to_owned()],
        capability: Some(Capability::TagAddTag),
        action: None,
        positionals: Vec::new(),
        flags: Vec::new(),
        bang: false,
    };
    assert!(answer(&later_task, &screen(), 1).is_none());
    assert!(matches!(asked("sync status", &empty()), Answer::Refused(_)));
}

// ---------------------------------------------------------------------------
// the one verb that asks
// ---------------------------------------------------------------------------

#[test]
fn rebuild_is_the_only_verb_here_that_asks_when_typed() {
    let mut asks = Vec::new();
    for verb in command::children_of(&[]) {
        if verb.action.is_some() || verb.capability.is_none() {
            continue;
        }
        let path = verb.canonical();
        if let Some(Answer::Rows(request) | Answer::Fact(request)) =
            answer(&invocation(&with_arguments(verb)), &screen(), 1)
        {
            if request.confirm.is_some() {
                asks.push(path);
            }
        }
    }
    assert_eq!(
        asks,
        ["index rebuild"],
        "task 89 settled that a `:` line typed in full is already the \
         deliberate act a confirmation asks for, so gating every mutating verb \
         would make the question meaningless by asking it twenty times"
    );
}

#[test]
fn a_bang_is_what_skips_rebuilds_question() {
    let asked = match answer(&invocation("index rebuild"), &screen(), 1) {
        Some(Answer::Rows(request)) => request.confirm,
        other => panic!("expected rows, found {other:?}"),
    };
    assert!(asked.is_some(), "typed bare, it asks");
    let banged = match answer(&invocation("index rebuild!"), &screen(), 1) {
        Some(Answer::Rows(request)) => request.confirm,
        other => panic!("expected rows, found {other:?}"),
    };
    assert!(banged.is_none(), "with a bang, it does not");
}

// ---------------------------------------------------------------------------
// dispatch, through the state machine
// ---------------------------------------------------------------------------

#[test]
fn a_table_verb_opens_a_report_and_issues_its_request() {
    let mut model = loaded();
    let cmds = issued(&run(&mut model, "index status"));
    assert_eq!(cmds.len(), 1, "{cmds:?}");
    assert!(matches!(cmds.first(), Some(Cmd::IndexStatus { .. })));
    assert_eq!(open_pane(&model).invocation.verb, ["index", "status"]);
    assert_eq!(model.mode(), Mode::Menu);
    assert_eq!(
        model.inflight, 0,
        "a report's own progress is the report, so it is not also counted as \
         outstanding work"
    );
}

#[test]
fn a_fact_verb_says_so_on_the_status_line_and_counts_as_work() {
    let mut model = loaded();
    let cmds = issued(&run(&mut model, "ai retry"));
    assert_eq!(cmds, vec![Cmd::AiRetry]);
    assert!(model.overlay.is_none(), "no report for a one-line answer");
    assert!(model.status.contains("retrying"), "{}", model.status);
    assert_eq!(
        model.inflight, 1,
        "somebody asked for this one, unlike the heartbeat"
    );
}

#[test]
fn a_verb_that_declares_an_argument_is_dispatched_with_it() {
    let mut model = loaded();
    let cmds = issued(&run(&mut model, "index entities email"));
    assert_eq!(
        cmds,
        vec![Cmd::IndexEntities {
            generation: 1,
            kind: "email".to_owned(),
        }],
        "the argument reaches the request rather than being refused on the way \
         past: {cmds:?}"
    );
    assert!(
        open_pane(&model).title.contains("email"),
        "and the report says which kind it is showing: {}",
        open_pane(&model).title
    );
}

#[test]
fn a_verb_given_more_arguments_than_it_declares_is_refused() {
    let mut model = loaded();
    run(&mut model, "index entities email phone");
    match model.overlay.as_ref() {
        Some(Overlay::Command(pane)) => {
            let why = pane.error.clone().unwrap_or_default();
            assert!(why.contains("1 argument"), "{why}");
        }
        other => panic!("expected the complaint in the command line: {other:?}"),
    }
}

#[test]
fn rebuild_asks_first_and_answering_yes_runs_it_once() {
    let mut model = loaded();
    let cmds = issued(&run(&mut model, "index rebuild"));
    assert!(
        cmds.is_empty(),
        "nothing is sent until it is answered: {cmds:?}"
    );
    match model.overlay.as_ref() {
        Some(Overlay::Confirm { prompt, then }) => {
            assert!(prompt.contains("rebuild"), "{prompt}");
            match then {
                Confirmed::Invoke { invocation, over } => {
                    assert!(invocation.bang, "the gate stamps the bang");
                    assert!(
                        over.is_none(),
                        "a question asked of a typed line has no report behind \
                         it to put back"
                    );
                }
                other => panic!("expected an invocation, found {other:?}"),
            }
        }
        other => panic!("expected a confirmation, found {other:?}"),
    }
    let cmds = update(&mut model, Msg::Key(Key::Char('y')));
    assert!(
        matches!(cmds.first(), Some(Cmd::IndexRebuild { .. })),
        "{cmds:?}"
    );
    assert!(matches!(model.overlay, Some(Overlay::Report(_))));
}

#[test]
fn declining_rebuild_runs_nothing_and_leaves_no_report() {
    let mut model = loaded();
    run(&mut model, "index rebuild");
    let cmds = update(&mut model, Msg::Key(Key::Char('n')));
    assert!(cmds.is_empty());
    assert!(model.overlay.is_none());
}

#[test]
fn a_banged_rebuild_starts_without_asking() {
    let mut model = loaded();
    let cmds = issued(&run(&mut model, "index rebuild!"));
    assert!(
        matches!(cmds.first(), Some(Cmd::IndexRebuild { .. })),
        "{cmds:?}"
    );
    assert!(matches!(model.overlay, Some(Overlay::Report(_))));
}

#[test]
fn re_running_a_report_that_asked_does_not_ask_again() {
    // The question was answered to open it; asking on every `r` would make `r`
    // the wrong key to press.
    let mut model = loaded();
    run(&mut model, "index rebuild!");
    let cmds = update(&mut model, Msg::Key(Key::Char('r')));
    assert!(
        matches!(cmds.first(), Some(Cmd::IndexRebuild { .. })),
        "{cmds:?}"
    );
    assert!(matches!(model.overlay, Some(Overlay::Report(_))));
}

#[test]
fn a_refusal_reaches_the_command_line_rather_than_being_swallowed() {
    let mut model = Model::new();
    update(&mut model, Msg::Key(Key::Char(':')));
    for c in "sync status".chars() {
        update(&mut model, Msg::Key(Key::Char(c)));
    }
    // The line parses, so it is recorded in the history — a refusal is exactly
    // the line somebody wants `<up>` to bring back and fix. Nothing else is
    // issued.
    let cmds = issued(&update(&mut model, Msg::Key(Key::Enter)));
    assert!(cmds.is_empty(), "{cmds:?}");
    match model.overlay.as_ref() {
        Some(Overlay::Command(pane)) => {
            let why = pane.error.clone().unwrap_or_default();
            assert!(why.contains("account"), "{why}");
        }
        other => panic!("the command line stays up with the complaint: {other:?}"),
    }
}

#[test]
fn a_streamed_report_fills_in_and_a_stale_frame_is_dropped() {
    let mut model = loaded();
    let cmds = issued(&run(&mut model, "index run"));
    let first = match cmds.first() {
        Some(Cmd::IndexReindex { generation, .. }) => *generation,
        other => panic!("expected a reindex, found {other:?}"),
    };
    // Two frames of the same pass: the second replaces the first, because
    // `IndexProgress` reports running totals.
    for (done, remaining) in [(false, "7"), (false, "3")] {
        update(
            &mut model,
            Msg::Report {
                generation: first,
                event: ReportEvent::Frame {
                    fill: ReportFill::Replace,
                    rows: vec![ReportRow::new(["remaining", remaining])],
                    complete: done,
                },
            },
        );
    }
    assert_eq!(open_pane(&model).rows.len(), 1, "a snapshot replaces");
    assert_eq!(open_pane(&model).rows[0].cells[1], "3");

    // `r` supersedes it; the old pass's tail must not land in the new answer.
    let cmds = update(&mut model, Msg::Key(Key::Char('r')));
    let second = match cmds.first() {
        Some(Cmd::IndexReindex { generation, .. }) => *generation,
        other => panic!("expected a fresh reindex, found {other:?}"),
    };
    assert_ne!(first, second);
    update(
        &mut model,
        Msg::Report {
            generation: first,
            event: ReportEvent::Frame {
                fill: ReportFill::Replace,
                rows: vec![ReportRow::new(["remaining", "stale"])],
                complete: true,
            },
        },
    );
    assert!(open_pane(&model).rows.is_empty(), "{:?}", open_pane(&model));
    assert!(!open_pane(&model).complete);
}
