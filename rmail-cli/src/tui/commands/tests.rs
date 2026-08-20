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
        selection: vec![10],
        rule_draft: None,
    }
}

/// A screen with nothing loaded yet.
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
///
/// An id-shaped positional gets a *number*, not its own name. Every id-taking
/// verb here parses its argument and refuses what is not a number, so a
/// placeholder of `"delivery_id"` made those verbs answer `Refused` in every
/// sweep below — which quietly excluded them from what the sweeps claim to
/// cover. `account rm`'s confirmation went unasserted that way until task 98
/// noticed.
fn with_arguments(verb: &rmail_core::command::Verb) -> String {
    let mut line = verb.canonical();
    for positional in verb.positionals {
        line.push(' ');
        if positional.name == "id" || positional.name.ends_with("_id") {
            line.push('1');
        } else {
            line.push_str(positional.name);
        }
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
        "account add",
        "account list",
        "account login",
        "account new",
        "account refresh",
        "account rm",
        "account show",
        "account test",
        "auth clear",
        "auth status",
        "ai audit",
        "ai budget set",
        "ai budget status",
        "ai confirm",
        "ai cost",
        "ai pause",
        "ai process",
        "ai provider set",
        "ai provider status",
        "ai resume",
        "ai retry",
        "ai scan",
        "ai status",
        "draft delete",
        "draft edit",
        "draft list",
        "draft render",
        "draft revert",
        "draft revisions",
        "draft rewrite",
        "draft show",
        "finder rebuild",
        "finder status",
        "followup dismiss",
        "followup list",
        "followup new",
        "forward",
        "hook list",
        "hook test",
        "index entities",
        "index gc",
        "index rebuild",
        "index reindex",
        "index run",
        "index start",
        "index status",
        "index stop",
        "index verify",
        "rule add",
        "rule backtest",
        "rule correct",
        "rule list",
        "rule new",
        "rule run",
        "notify list",
        "notify score",
        "nudge",
        "outbox edit",
        "outbox reschedule",
        "outbox retry",
        "outbox send-now",
        "outbox suggest",
        "preflight",
        "send",
        "sync now",
        "sync pause",
        "sync resume",
        "sync status",
        "tag accept",
        "tag add",
        "tag bulk",
        "tag list",
        "tag new",
        "tag reject",
        "tag rm",
        "tag rules",
        "tag rules set",
        "tag suggest",
        "token create",
        "token list",
        "token revoke",
        "waiting",
        "webhook add",
        "webhook deliveries",
        "webhook disable",
        "webhook enable",
        "webhook list",
        "webhook replay",
        "webhook rm",
        // -- task 99 ---------------------------------------------------------
        "attach ask",
        "attach invoice",
        "attach invoices",
        "attach search",
        "attach tables",
        "contact",
        "digest",
        "export",
        "extract data",
        "extract events",
        "extract tasks",
        "folder compile",
        "folder eval",
        "folder list",
        "folder members",
        "folder new",
        "folder rm",
        "links",
        "note add",
        "note edit",
        "note list",
        "note rm",
        "note watch",
        "saved edit",
        "saved list",
        "saved rm",
        "saved run",
        "saved save",
        "search attachments",
        "search compile",
        "search entities",
        "search eval",
        "stats ask",
        "stats response-time",
        "subs",
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

/// Task 100's own version of the test above — every `ComposeService` and
/// `SendSchedulerService` capability task 100 owns is reachable through
/// `commands::answer`, with the handful that are not named and explained
/// rather than silently passing because nothing checked them.
///
/// `ComposeCreateDraft`, `SendSchedulerCancelScheduled` and
/// `SendSchedulerListOutbox` predate this task and are reached through an
/// `Action` (`r`/`f`/`O`/`u`), not through this table, so `answer` was never
/// going to find them — task 94's own coverage test has the identical
/// exemption for capabilities `run_action` reaches. `SendSchedulerWatchOutbox`
/// and `SendSchedulerTrackFollowup` are the two `command::explicit`'s own
/// comments name as deliberately unwired.
#[test]
fn every_capability_task_100_owns_is_reachable_by_a_verb_or_named_as_an_exception() {
    let wanted = [
        Capability::ComposeDraftReply,
        Capability::ComposeGetDraft,
        Capability::ComposeListDrafts,
        Capability::ComposeUpdateDraft,
        Capability::ComposeDeleteDraft,
        Capability::ComposeRenderDraft,
        Capability::ComposeRewriteDraft,
        Capability::ComposeListDraftRevisions,
        Capability::ComposeSelectDraftRevision,
        Capability::SendSchedulerScheduleSend,
        Capability::SendSchedulerRetryFailed,
        Capability::SendSchedulerRescheduleSend,
        Capability::SendSchedulerUpdateScheduledBody,
        Capability::SendSchedulerSendNow,
        Capability::SendSchedulerSuggestSendTime,
        Capability::SendSchedulerCreateFollowup,
        Capability::SendSchedulerListFollowups,
        Capability::SendSchedulerDismissFollowup,
        Capability::SendSchedulerListWaitingOn,
        Capability::SendSchedulerDraftNudge,
        Capability::SendSchedulerPreflightCheck,
    ];
    for capability in wanted {
        // `reply`'s own capability is included above even though `reply` is
        // hand-written in `run_invocation` and never reaches `answer` at
        // all — so it is checked by declaration (some real verb names it),
        // not by `answer` returning `Some`, which is the one capability here
        // that would otherwise look silently unreached.
        let declared = command::children_of(&[])
            .into_iter()
            .any(|verb| verb.capability == Some(capability));
        assert!(
            declared,
            "{} is a capability task 100 owns and no verb declares it",
            capability.name()
        );
        if capability == Capability::ComposeDraftReply {
            continue;
        }
        let answered = command::children_of(&[]).into_iter().any(|verb| {
            verb.capability == Some(capability)
                && answer(&invocation(&with_arguments(verb)), &screen(), 1).is_some()
        });
        assert!(
            answered,
            "{} is declared but no verb answers for it through commands::answer",
            capability.name()
        );
    }
}

/// The two exclusions the test above takes on faith, checked directly: they
/// really are absent from the registry, not merely unreached by a verb that
/// still exists.
#[test]
fn watch_outbox_and_track_followup_are_not_reachable_by_any_verb() {
    for capability in [
        Capability::SendSchedulerWatchOutbox,
        Capability::SendSchedulerTrackFollowup,
    ] {
        let declared = command::children_of(&[])
            .into_iter()
            .any(|verb| verb.capability == Some(capability));
        assert!(
            !declared,
            "{} was declared unreachable by design; a verb now reaches it, so \
             the comment in command::explicit and every_capability_task_100_owns_… \
             above are both stale",
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
    // Built by hand, and deliberately *not* a declared verb. The claim is about
    // this table's own fallthrough — a verb reaching a capability it has no arm
    // for — and pinning it to whichever RPC happened to be unimplemented broke
    // the moment that RPC arrived: this was `:ai budget status` until task 96
    // answered it. A path the registry does not declare cannot be overtaken by
    // a later task, and tests the same thing.
    let unanswered = Invocation {
        range: None,
        verb: vec!["ai".to_owned(), "budget".to_owned(), "forecast".to_owned()],
        capability: Some(Capability::AiPolicyGetSpend),
        action: None,
        positionals: Vec::new(),
        flags: Vec::new(),
        bang: false,
    };
    assert!(answer(&unanswered, &screen(), 1).is_none());
    assert!(matches!(asked("sync status", &empty()), Answer::Refused(_)));
}

// ---------------------------------------------------------------------------
// the few verbs that ask
// ---------------------------------------------------------------------------

#[test]
fn only_the_verbs_that_cannot_be_undone_ask_when_typed() {
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
    asks.sort();
    assert_eq!(
        asks,
        // What unites these five, and nothing else: each destroys something no
        // later command can put back. `index rebuild` throws away an index that
        // costs hours to build; `account rm` cascades to every message stored
        // for the account; `webhook rm` deletes the record of what already left
        // this machine; `draft delete` takes a draft and its revision history
        // with it; `outbox send-now` spends the undo window that is the only
        // thing standing between a scheduled send and an unrecallable one.
        //
        // Every other mutating verb here is either reversible or is itself the
        // undo — and task 89 settled that a `:` line typed in full is already the
        // deliberate act a confirmation asks for, so gating them all would make
        // the question meaningless by asking it twenty times.
        //
        // Two of these (`draft delete`, `outbox send-now`) were being asserted
        // by nobody until task 98 fixed `with_arguments`: their placeholder
        // argument was not a number, so the sweep saw a refusal instead of the
        // question. See that function.
        [
            "account rm",
            "draft delete",
            "index rebuild",
            "outbox send-now",
            "webhook rm",
        ],
        "the confirmation is a per-verb judgement about what cannot be undone, \
         not a restatement of `effect().is_mutating()`"
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

/// `run_invocation`'s daemon-verb routing has to happen *before* its generic
/// "no verb here has ever declared a flag" refusal, or every flag task 100's
/// daemon-routed verbs declare would be swallowed on the way past — this
/// build's own flag-carrying verbs are the only ones that can catch a
/// regression of that ordering, since none of task 94's declare one.
#[test]
fn a_flag_on_a_daemon_verb_reaches_it_through_the_real_dispatch_not_just_answer() {
    let mut model = loaded();
    let cmds = issued(&run(&mut model, "waiting --overdue"));
    assert_eq!(
        cmds,
        vec![Cmd::Waiting {
            generation: 1,
            account_id: 7,
            overdue: true,
        }],
        "the flag reached the daemon verb rather than being refused as \
         unwired on the way past: {cmds:?}"
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

// ---------------------------------------------------------------------------
// drafts, send, the outbox and follow-ups (task 100)
// ---------------------------------------------------------------------------

#[test]
fn the_draft_verbs_issue_the_rpc_they_name() {
    assert_eq!(
        request("draft show 5").cmd,
        Cmd::DraftShow {
            generation: 5,
            draft_id: 5,
        }
    );
    assert_eq!(
        request("draft delete 5!").cmd,
        Cmd::DraftDelete { draft_id: 5 }
    );
    assert_eq!(
        request("draft render 5").cmd,
        Cmd::DraftRender {
            generation: 5,
            draft_id: 5,
        }
    );
    assert_eq!(
        request("draft revisions 5").cmd,
        Cmd::DraftRevisions {
            generation: 5,
            draft_id: 5,
        }
    );
}

#[test]
fn draft_list_carries_the_account_on_screen() {
    assert_eq!(
        request("draft list").cmd,
        Cmd::DraftList {
            generation: 5,
            account_id: 7,
        }
    );
    match asked("draft list", &empty()) {
        Answer::Refused(why) => assert!(why.contains("account"), "{why}"),
        other => panic!("expected a refusal with no account, found {other:?}"),
    }
}

#[test]
fn draft_edit_needs_a_body() {
    assert_eq!(
        request(r#"draft edit 5 --body="new text""#).cmd,
        Cmd::DraftEdit {
            generation: 5,
            draft_id: 5,
            body: "new text".to_owned(),
        }
    );
    match asked("draft edit 5", &screen()) {
        Answer::Refused(why) => assert!(why.contains("body"), "{why}"),
        other => panic!("expected a refusal with no body, found {other:?}"),
    }
}

#[test]
fn draft_revert_defaults_its_revision_to_the_original() {
    assert_eq!(
        request("draft revert 5").cmd,
        Cmd::DraftRevert {
            generation: 5,
            draft_id: 5,
            seq: 0,
        }
    );
    assert_eq!(
        request("draft revert 5 2").cmd,
        Cmd::DraftRevert {
            generation: 5,
            draft_id: 5,
            seq: 2,
        }
    );
}

#[test]
fn draft_revert_refuses_a_malformed_revision_rather_than_defaulting_to_the_original() {
    // Distinct from no second argument at all, which is what the test above
    // covers: a typo here must not silently mean the same thing an absent
    // one does.
    match asked("draft revert 5 tow", &screen()) {
        Answer::Refused(why) => assert!(why.contains("tow"), "{why}"),
        other => panic!("expected a refusal, found {other:?}"),
    }
}

#[test]
fn draft_rewrite_reads_its_own_flags() {
    assert_eq!(
        request("draft rewrite 5 --tone=formal --shorter --instruction=\"drop the joke\"").cmd,
        Cmd::DraftRewrite {
            generation: 5,
            draft_id: 5,
            tone: Some("formal".to_owned()),
            shorter: true,
            longer: false,
            instruction: "drop the joke".to_owned(),
        }
    );
    // Bare, it is refused rather than sent — see
    // `draft_rewrite_bare_is_refused_the_same_as_mail_draft_rewrite` below.
}

#[test]
fn draft_rewrite_refuses_a_tone_it_does_not_know() {
    match asked("draft rewrite 5 --tone=sarcastic", &screen()) {
        Answer::Refused(why) => assert!(why.contains("sarcastic"), "{why}"),
        other => panic!("expected a refusal, found {other:?}"),
    }
}

#[test]
fn draft_rewrite_refuses_shorter_and_longer_together() {
    match asked("draft rewrite 5 --shorter --longer", &screen()) {
        Answer::Refused(why) => assert!(why.contains("shorter") && why.contains("longer"), "{why}"),
        other => panic!("expected a refusal, found {other:?}"),
    }
}

#[test]
fn draft_rewrite_bare_is_refused_the_same_as_mail_draft_rewrite() {
    // `mail draft rewrite` refuses this client-side too, and for the stated
    // reason a round trip to be told the command asked for nothing is one
    // that did not need making — this mirrors that guard rather than leaving
    // it to a server round trip the CLI already avoids.
    match asked("draft rewrite 5", &screen()) {
        Answer::Refused(why) => assert!(why.contains("nothing to do"), "{why}"),
        other => panic!("expected a refusal, found {other:?}"),
    }
}

#[test]
fn an_id_taking_draft_verb_refuses_with_no_id() {
    // One representative rather than all seven: `id_positional` is the same
    // function under each of them, so this is a claim about that function,
    // not about `draft show` specifically.
    match asked("draft show", &screen()) {
        Answer::Refused(why) => assert!(why.contains("draft"), "{why}"),
        other => panic!("expected a refusal, found {other:?}"),
    }
    match asked("draft show notanumber", &screen()) {
        Answer::Refused(_) => {}
        other => panic!("a non-numeric id is refused the same as a missing one: {other:?}"),
    }
}

#[test]
fn send_carries_its_flags_and_needs_a_draft_and_an_account() {
    assert_eq!(
        request("send --draft=5 --at=\"tomorrow 9am\" --undo=60").cmd,
        Cmd::ScheduleSend {
            account_id: 7,
            draft_id: 5,
            at: "tomorrow 9am".to_owned(),
            undo: Some(60),
        }
    );
    match asked("send", &screen()) {
        Answer::Refused(why) => assert!(why.contains("draft"), "{why}"),
        other => panic!("expected a refusal with no draft named, found {other:?}"),
    }
    match asked("send --draft=5", &empty()) {
        Answer::Refused(why) => assert!(why.contains("account"), "{why}"),
        other => panic!("expected a refusal with no account, found {other:?}"),
    }
}

#[test]
fn send_refuses_an_undo_window_that_would_not_lengthen_anything() {
    // The proto: undo "can only lengthen". Zero, negative or unparseable all
    // have to be refused rather than silently falling back to the account
    // default the way a bare `--undo` with no value never even reaches this
    // arm (that is a parse-time `MissingFlagValue`) — this is the one case
    // the parser lets through that still has to be caught here.
    for line in [
        "send --draft=5 --undo=0",
        "send --draft=5 --undo=-30",
        "send --draft=5 --undo=abc",
    ] {
        match asked(line, &screen()) {
            Answer::Refused(why) => assert!(why.contains("positive"), "{line}: {why}"),
            other => panic!("{line}: expected a refusal, found {other:?}"),
        }
    }
}

#[test]
fn the_outbox_mutation_verbs_issue_the_rpc_they_name() {
    assert_eq!(
        request("outbox retry 9").cmd,
        Cmd::RetryFailed { outbox_id: 9 }
    );
    assert_eq!(
        request("outbox reschedule 9 --at=\"in 1h\"").cmd,
        Cmd::RescheduleSend {
            outbox_id: 9,
            at: "in 1h".to_owned(),
        }
    );
    assert_eq!(
        request(r#"outbox edit 9 --body="revised""#).cmd,
        Cmd::UpdateScheduledBody {
            outbox_id: 9,
            body: "revised".to_owned(),
        }
    );
    assert_eq!(
        request("outbox send-now 9!").cmd,
        Cmd::SendNow { outbox_id: 9 }
    );
    assert_eq!(
        request("outbox suggest").cmd,
        Cmd::SuggestSendTime {
            generation: 5,
            account_id: 7,
        }
    );
}

#[test]
fn outbox_reschedule_and_edit_need_their_value_flag() {
    match asked("outbox reschedule 9", &screen()) {
        Answer::Refused(why) => assert!(why.contains("time")),
        other => panic!("expected a refusal with no --at, found {other:?}"),
    }
    match asked("outbox edit 9", &screen()) {
        Answer::Refused(why) => assert!(why.contains("body")),
        other => panic!("expected a refusal with no --body, found {other:?}"),
    }
}

#[test]
fn draft_delete_and_outbox_send_now_ask_before_running() {
    // The other two verbs here that ask when typed in full, and — like
    // `index rebuild` — not because they mutate (every verb in this section
    // mutates): both discard something a report cannot show back
    // afterwards, a body typed once (`send-now`, skipping the rest of a
    // wait a person chose) or a stored draft outright.
    for line in ["draft delete 5", "outbox send-now 9"] {
        match asked(line, &screen()) {
            Answer::Fact(request) => {
                assert!(request.confirm.is_some(), "{line} should ask");
            }
            other => panic!("{line}: expected a fact, found {other:?}"),
        }
        let banged = format!("{line}!");
        match asked(&banged, &screen()) {
            Answer::Fact(request) => {
                assert!(request.confirm.is_none(), "{banged} should not ask");
            }
            other => panic!("{banged}: expected a fact, found {other:?}"),
        }
    }
}

#[test]
fn a_banged_draft_delete_deletes_without_asking() {
    let mut model = loaded();
    let cmds = issued(&run(&mut model, "draft delete 5!"));
    assert_eq!(cmds, vec![Cmd::DraftDelete { draft_id: 5 }]);
    assert!(model.overlay.is_none());
}

#[test]
fn the_followup_verbs_issue_the_rpc_they_name() {
    assert_eq!(
        request("followup list").cmd,
        Cmd::FollowupList {
            generation: 5,
            account_id: 7,
        }
    );
    assert_eq!(
        request("followup dismiss 3").cmd,
        Cmd::FollowupDismiss { id: 3 }
    );
    assert_eq!(
        request("nudge 3").cmd,
        Cmd::DraftNudge {
            generation: 5,
            id: 3
        }
    );
    assert_eq!(
        request("preflight 5").cmd,
        Cmd::PreflightCheck {
            generation: 5,
            account_id: 7,
            draft_id: 5,
        }
    );
}

#[test]
fn followup_new_reads_its_flags_and_needs_a_message() {
    assert_eq!(
        request("followup new --in=3d --note=\"circle back\"").cmd,
        Cmd::FollowupNew {
            message_id: 10,
            remind_in: "3d".to_owned(),
            note: "circle back".to_owned(),
        }
    );
    match asked("followup new", &empty()) {
        Answer::Refused(why) => assert!(why.contains("message")),
        other => panic!("expected a refusal with no message, found {other:?}"),
    }
}

#[test]
fn waiting_reads_its_overdue_flag_and_shares_followups_row_shape() {
    assert_eq!(
        request("waiting").cmd,
        Cmd::Waiting {
            generation: 5,
            account_id: 7,
            overdue: false,
        }
    );
    assert_eq!(
        request("waiting --overdue").cmd,
        Cmd::Waiting {
            generation: 5,
            account_id: 7,
            overdue: true,
        }
    );
    assert_eq!(
        request("waiting").columns,
        request("followup list").columns,
        "one RPC's rows, `Followup`, and one row shape for both — see \
         commands::followup_columns"
    );
}

#[test]
fn followup_list_and_waiting_need_an_account_too() {
    for line in ["followup list", "waiting"] {
        match asked(line, &empty()) {
            Answer::Refused(why) => assert!(why.contains("account"), "{line}: {why}"),
            other => panic!("{line}: expected a refusal with no account, found {other:?}"),
        }
    }
}

#[test]
fn preflight_needs_an_account_before_a_draft_id() {
    match asked("preflight 5", &empty()) {
        Answer::Refused(why) => assert!(why.contains("account"), "{why}"),
        other => panic!("expected a refusal with no account, found {other:?}"),
    }
}
