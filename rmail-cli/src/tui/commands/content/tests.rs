//! The content family's shared helpers, and the one verb here that reaches
//! nothing.
#![allow(clippy::panic)]

use rmail_core::command::{self, Resolution};
use rmail_core::keymap::Key;

use super::*;
use crate::tui::commands::Answer;
use crate::tui::model::{update, Account, Folder, MessageRow, Model, Msg, OpenMessage, Overlay};

fn invocation(line: &str) -> Invocation {
    match command::parse(line) {
        Ok(Resolution::Invocation(invocation)) => *invocation,
        other => panic!("{line:?} does not parse to an invocation: {other:?}"),
    }
}

pub(super) fn screen() -> Target {
    Target {
        account_id: 7,
        mailbox_id: Some(1),
        message_id: Some(10),
        selection: vec![10, 11],
        rule_draft: None,
    }
}

pub(super) fn no_account() -> Target {
    Target {
        account_id: 0,
        mailbox_id: None,
        message_id: Some(10),
        selection: Vec::new(),
        rule_draft: None,
    }
}

pub(super) fn no_message() -> Target {
    Target {
        message_id: None,
        ..screen()
    }
}

pub(super) fn loaded() -> Model {
    let mut model = Model::new();
    model.accounts = vec![Account {
        id: 7,
        name: "personal".to_owned(),
        username: Some("me@example.com".to_owned()),
    }];
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
            has_attachments: true,
            has_note: false,
            to: None,
            tags: Vec::new(),
            ai: None,
        })
        .collect();
    model
}

pub(super) fn run(model: &mut Model, line: &str) -> Vec<crate::tui::model::Cmd> {
    update(model, Msg::Key(Key::Char(':')));
    for c in line.chars() {
        update(model, Msg::Key(Key::Char(c)));
    }
    update(model, Msg::Key(Key::Enter))
        .into_iter()
        .filter(|cmd| !matches!(cmd, crate::tui::model::Cmd::SaveHistory { .. }))
        .collect()
}

// ---------------------------------------------------------------------------
// the window
// ---------------------------------------------------------------------------

#[test]
fn a_window_is_a_duration_going_back_and_an_instant_ending() {
    // `--since 30d` reads better than a unix second and is what `mail stats`
    // accepts; `--until` is absolute because "until 30 days ago" is a window
    // nobody asks for.
    assert_eq!(
        since(&invocation("digest --since=30d")),
        Ok(Some(2_592_000))
    );
    assert_eq!(since(&invocation("digest")), Ok(None));
    assert_eq!(
        until(&invocation("digest --until=1700000000")),
        Ok(Some(1_700_000_000))
    );
    assert_eq!(until(&invocation("digest")), Ok(None));
}

#[test]
fn a_window_with_no_length_is_refused_rather_than_reported_on() {
    // A report over zero seconds summarizes nothing, and presenting nothing as
    // though it were a report is worse than refusing.
    let why = since(&invocation("digest --since=0s")).expect_err("refused");
    assert!(why.contains("has to have a length"), "{why}");
    let why = since(&invocation("digest --since=soon")).expect_err("refused");
    assert!(why.contains("--since"), "{why}");
    let why = until(&invocation("digest --until=yesterday")).expect_err("refused");
    assert!(why.contains("unix seconds"), "{why}");
}

#[test]
fn the_message_is_the_one_on_screen_unless_a_line_names_another() {
    assert_eq!(message(&invocation("links"), &screen()), Ok(10));
    assert_eq!(message(&invocation("links 42"), &screen()), Ok(42));
    let why = message(&invocation("links"), &no_message()).expect_err("refused");
    assert!(why.contains("no message selected"), "{why}");
    let why = message(&invocation("links twelve"), &screen()).expect_err("refused");
    assert!(why.contains("not a message id"), "{why}");
}

#[test]
fn every_verb_in_this_family_needs_the_account_on_screen() {
    // There is no `--account` and no zero-means-everything here: these verbs are
    // about one account's mail, and a session with none loaded has nothing for
    // them to report on.
    assert_eq!(account(&screen()), Ok(7));
    let why = account(&no_account()).expect_err("refused");
    assert!(why.contains("no account"), "{why}");
}

#[test]
fn a_verb_whose_positionals_are_text_never_reads_the_first_word_as_an_id() {
    // The bug this split exists for: `content::message` reads the first
    // positional as a message id, which is right for `:links 42` and wrong for
    // every verb whose positionals are what somebody wrote. Using it on those
    // refused any question or note that did not begin with a number.
    //
    // Asserted over the registry rather than a list here, so a verb added later
    // with a free-text positional is covered without an edit: every content verb
    // whose first declared positional is not id-shaped has to answer a line whose
    // first word is a word.
    for verb in command::children_of(&[]) {
        let Some(first) = verb.positionals.first() else {
            continue;
        };
        if first.name == "message_id" || first.name.ends_with("_id") || first.name == "id" {
            continue;
        }
        let path = verb.canonical();
        let line = format!("{path} chased this on Tuesday");
        let Ok(Resolution::Invocation(parsed)) = command::parse(&line) else {
            continue;
        };
        let Some(answer) = super::answer(&parsed, &screen(), 1) else {
            continue;
        };
        if let Answer::Refused(why) = answer {
            assert!(
                !why.contains("is not a message id"),
                "{line:?} read its own argument as a message id: {why}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// the one verb that reaches nothing
// ---------------------------------------------------------------------------

#[test]
fn attach_list_reads_the_open_message_rather_than_the_daemon() {
    // The parts came back with the message, so a round trip to re-fetch what the
    // preview pane is already drawing would be a second source of truth.
    let mut model = loaded();
    model.open = Some(OpenMessage {
        id: 10,
        headers: Vec::new(),
        body: Vec::new(),
        has_html: false,
        attachments: vec![
            "invoice.pdf (12 KB)".to_owned(),
            "notes.txt (1 KB)".to_owned(),
        ],
    });
    let cmds = run(&mut model, "attach list");
    assert!(cmds.is_empty(), "it reaches no daemon: {cmds:?}");
    let Some(Overlay::Report(pane)) = model.overlay_top() else {
        panic!("expected a report");
    };
    assert!(pane.complete, "nothing is outstanding");
    assert_eq!(pane.rows.len(), 2);
    assert_eq!(pane.rows[0].cells[0], "invoice.pdf (12 KB)");
    assert!(model.status.contains("2 attachments"), "{}", model.status);
}

#[test]
fn attach_list_says_so_when_there_is_nothing_attached() {
    let mut model = loaded();
    model.open = Some(OpenMessage {
        id: 10,
        headers: Vec::new(),
        body: Vec::new(),
        has_html: false,
        attachments: Vec::new(),
    });
    run(&mut model, "attach list");
    assert!(
        model.status.contains("nothing attached"),
        "{}",
        model.status
    );
    // And when no message is open at all, it says how to open one rather than
    // showing an empty table.
    let mut model = loaded();
    run(&mut model, "attach list");
    let why = match model.overlay_top() {
        Some(Overlay::Command(pane)) => pane.error.clone().unwrap_or_default(),
        _ => model.status.clone(),
    };
    assert!(why.contains("no message open"), "{why}");
}
