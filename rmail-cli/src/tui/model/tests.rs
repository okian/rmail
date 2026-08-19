//! Headless tests for the TUI's state machine.
//!
//! Every one of these drives [`update`] directly. No terminal, no daemon, no
//! runtime — which is the point of the model/terminal split, and the reason
//! these can cover the error paths (no archive folder, no sender to reply to,
//! a failed RPC) that a hand-driven TUI never reaches.
//!
//! `panic!` in a match arm that cannot happen reads better here than the
//! `unreachable!` dance, and this module is test-only — the same exemption
//! `tag_cli::tests` takes (`clippy.toml` carves out `unwrap`/`expect` in
//! tests but not `panic`).
#![allow(clippy::panic)]

use std::time::Instant;

use super::*;

fn account() -> Account {
    Account {
        id: 7,
        name: "personal".to_owned(),
        username: Some("me@example.com".to_owned()),
    }
}

fn folders() -> Vec<Folder> {
    vec![
        Folder {
            id: 1,
            name: "INBOX".to_owned(),
            message_count: 3,
        },
        Folder {
            id: 2,
            name: "Archive".to_owned(),
            message_count: 9,
        },
        Folder {
            id: 3,
            name: "Sent".to_owned(),
            message_count: 4,
        },
    ]
}

fn row(id: i64) -> MessageRow {
    MessageRow {
        id,
        subject: format!("subject {id}"),
        from: "Alice".to_owned(),
        from_addr: Some("alice@example.com".to_owned()),
        date: Some(1_700_000_000 + id),
        flags: Vec::new(),
        has_attachments: false,
    }
}

/// A model with an account, folders and three messages already loaded — the
/// state most action tests want to start from.
fn loaded() -> Model {
    let mut model = Model::new();
    model.account = Some(account());
    model.folders = folders();
    model.folder_idx = 0;
    model.open_folder = Some(1);
    model.messages = vec![row(10), row(11), row(12)];
    model
}

fn press(model: &mut Model, key: Key) -> Vec<Cmd> {
    update(model, Msg::Key(key))
}

fn keys(model: &mut Model, sequence: &str) -> Vec<Cmd> {
    let mut cmds = Vec::new();
    for c in sequence.chars() {
        cmds.extend(press(model, Key::Char(c)));
    }
    cmds
}

// ---------------------------------------------------------------------------
// startup
// ---------------------------------------------------------------------------

#[test]
fn boot_loads_accounts_then_folders_then_messages() {
    let mut model = Model::new();

    assert_eq!(update(&mut model, Msg::Boot), vec![Cmd::LoadAccounts]);
    assert_eq!(model.inflight, 1, "the request is counted, not awaited");

    let cmds = update(&mut model, Msg::Accounts(Ok(vec![account()])));
    assert_eq!(
        cmds,
        vec![
            Cmd::LoadFolders { account_id: 7 },
            Cmd::Watch { account_id: 7 },
            Cmd::LoadOutbox { account_id: 7 },
            // Task 92's heartbeat, counted for neither of the reasons `Watch`
            // is not: nobody asked for it and it never finishes.
            Cmd::Heartbeat { account_id: 7 },
        ],
        "the event stream starts as soon as there is an account to watch, and the \
         outbox is listed once so a live undo window is visible without asking"
    );

    let cmds = update(&mut model, Msg::Folders(Ok(folders())));
    assert_eq!(cmds, vec![Cmd::LoadMessages { mailbox_id: 1 }]);
    assert_eq!(
        model.folder_idx, 0,
        "INBOX is selected, not merely the first folder"
    );

    let cmds = update(
        &mut model,
        Msg::Messages {
            mailbox_id: 1,
            result: Ok(vec![row(10), row(11)]),
        },
    );
    assert!(cmds.is_empty());
    assert_eq!(model.messages.len(), 2);

    // The outbox listing startup asked for, so the counter can be checked
    // against every request that actually went out.
    update(
        &mut model,
        Msg::Outbox {
            now: 1_000,
            result: Ok(Vec::new()),
        },
    );
    assert_eq!(model.inflight, 0, "every request has been accounted for");
}

#[test]
fn inbox_is_selected_case_insensitively_and_wherever_it_sits() {
    let mut model = Model::new();
    model.account = Some(account());
    let folders = vec![
        Folder {
            id: 5,
            name: "Drafts".to_owned(),
            message_count: 0,
        },
        Folder {
            id: 6,
            name: "Inbox".to_owned(),
            message_count: 2,
        },
    ];
    let cmds = update(&mut model, Msg::Folders(Ok(folders)));
    assert_eq!(model.folder_idx, 1);
    assert_eq!(cmds, vec![Cmd::LoadMessages { mailbox_id: 6 }]);
}

#[test]
fn the_first_frame_does_not_wait_for_any_data() {
    // prd.md budgets 200 ms for TUI startup. What that budget covers is the
    // path to the first painted frame: build the model, draw it. Anything
    // that had to wait for the daemon would blow it on a cold mailbox, so the
    // assertion that matters is that the first frame needs nothing.
    let started = Instant::now();
    let model = Model::new();
    let backend = ratatui::backend::TestBackend::new(120, 40);
    let mut terminal = ratatui::Terminal::new(backend).unwrap();
    terminal
        .draw(|frame| crate::tui::view::render(&model, frame))
        .unwrap();
    let elapsed = started.elapsed();

    assert!(model.account.is_none() && model.messages.is_empty());
    assert!(
        elapsed < std::time::Duration::from_millis(200),
        "first frame took {elapsed:?}, budget is 200ms"
    );
}

#[test]
fn no_accounts_is_reported_rather_than_leaving_the_status_on_connecting() {
    let mut model = Model::new();
    let cmds = update(&mut model, Msg::Accounts(Ok(Vec::new())));
    assert!(cmds.is_empty());
    assert_eq!(model.level, Level::Error);
    assert!(model.status.contains("no accounts"), "{}", model.status);
}

#[test]
fn account_flag_selects_that_account_rather_than_the_first() {
    let other = Account {
        id: 9,
        name: "work".to_owned(),
        username: Some("work@example.com".to_owned()),
    };
    let mut model = Model::for_account(Some(9));
    let cmds = update(&mut model, Msg::Accounts(Ok(vec![account(), other])));

    assert_eq!(model.current_account().map(|a| a.id), Some(9));
    assert_eq!(
        cmds,
        vec![
            Cmd::LoadFolders { account_id: 9 },
            Cmd::Watch { account_id: 9 },
            Cmd::LoadOutbox { account_id: 9 },
            // Task 92's heartbeat, counted for neither of the reasons `Watch`
            // is not: nobody asked for it and it never finishes.
            Cmd::Heartbeat { account_id: 9 },
        ]
    );
}

#[test]
fn an_account_flag_naming_nothing_is_an_error_not_a_silent_fallback() {
    let mut model = Model::for_account(Some(404));
    let cmds = update(&mut model, Msg::Accounts(Ok(vec![account()])));
    assert!(
        cmds.is_empty(),
        "opening the wrong mailbox is worse than none"
    );
    assert!(model.current_account().is_none());
    assert_eq!(model.level, Level::Error);
    assert!(model.status.contains("no account 404"), "{}", model.status);
}

#[test]
fn a_failed_load_shows_the_daemons_message_and_clears_the_inflight_count() {
    let mut model = Model::new();
    update(&mut model, Msg::Boot);
    assert_eq!(model.inflight, 1);

    let cmds = update(
        &mut model,
        Msg::Accounts(Err("connection refused".to_owned())),
    );
    assert!(cmds.is_empty());
    assert_eq!(model.inflight, 0, "a failure must not strand the counter");
    assert_eq!(model.level, Level::Error);
    assert!(model.status.contains("connection refused"));
}

// ---------------------------------------------------------------------------
// navigation
// ---------------------------------------------------------------------------

#[test]
fn j_and_k_move_the_message_cursor_and_stop_at_the_ends() {
    let mut model = loaded();
    assert_eq!(model.message_idx, 0);

    press(&mut model, Key::Char('j'));
    assert_eq!(model.message_idx, 1);
    press(&mut model, Key::Char('j'));
    press(&mut model, Key::Char('j'));
    assert_eq!(model.message_idx, 2, "clamped at the last row, not wrapped");

    press(&mut model, Key::Char('k'));
    assert_eq!(model.message_idx, 1);
    press(&mut model, Key::Char('k'));
    press(&mut model, Key::Char('k'));
    assert_eq!(model.message_idx, 0, "clamped at the first row");
}

#[test]
fn arrow_keys_are_synonyms_for_j_and_k() {
    let mut model = loaded();
    press(&mut model, Key::Down);
    assert_eq!(model.message_idx, 1);
    press(&mut model, Key::Up);
    assert_eq!(model.message_idx, 0);
}

#[test]
fn gg_jumps_to_the_top_and_capital_g_to_the_bottom() {
    let mut model = loaded();

    press(&mut model, Key::Char('G'));
    assert_eq!(model.message_idx, 2);

    let cmds = keys(&mut model, "gg");
    assert!(cmds.is_empty());
    assert_eq!(model.message_idx, 0);
    assert!(
        model.pending.is_empty(),
        "the chord is consumed, not left half-typed"
    );
}

#[test]
fn a_single_g_is_inert_until_its_partner_arrives() {
    let mut model = loaded();
    press(&mut model, Key::Char('G'));
    assert_eq!(model.message_idx, 2);

    press(&mut model, Key::Char('g'));
    assert_eq!(
        model.pending.keys(),
        [Key::Char('g')],
        "the g is held, waiting for its partner"
    );
    assert_eq!(model.message_idx, 2, "a lone g moves nothing");
}

#[test]
fn partial_g_does_not_swallow_the_next_key() {
    // The bug this guards: holding the half-typed `g` and then treating the
    // following key as "not the partner, so ignore it" silently eats a
    // keystroke. `g` then `k` must move up, and `g` then `q` must quit.
    let mut model = loaded();
    model.message_idx = 2;

    press(&mut model, Key::Char('g'));
    press(&mut model, Key::Char('k'));
    assert!(model.pending.is_empty(), "the pending g was cleared");
    assert_eq!(model.message_idx, 1, "the k was handled, not swallowed");

    press(&mut model, Key::Char('g'));
    press(&mut model, Key::Char('q'));
    assert!(model.quit, "q after a partial g still quits");
}

#[test]
fn a_partial_g_does_not_survive_to_pair_with_a_later_g() {
    let mut model = loaded();
    model.message_idx = 2;

    press(&mut model, Key::Char('g'));
    press(&mut model, Key::Char('j')); // clears the pending g
    press(&mut model, Key::Char('g')); // starts a fresh chord
    assert_eq!(
        model.message_idx, 2,
        "g-j-g must not jump: the first g is long gone"
    );
}

#[test]
fn tab_moves_focus_between_the_panes_and_j_then_moves_the_other_cursor() {
    let mut model = loaded();
    assert_eq!(model.focus, Focus::Messages);

    press(&mut model, Key::Tab);
    assert_eq!(model.focus, Focus::Folders);
    press(&mut model, Key::Char('j'));
    assert_eq!(model.folder_idx, 1);
    assert_eq!(model.message_idx, 0, "the message cursor did not move");

    press(&mut model, Key::Tab);
    assert_eq!(model.focus, Focus::Messages);
}

#[test]
fn gg_and_g_apply_to_whichever_pane_has_focus() {
    let mut model = loaded();
    model.focus = Focus::Folders;

    press(&mut model, Key::Char('G'));
    assert_eq!(model.folder_idx, 2);
    keys(&mut model, "gg");
    assert_eq!(model.folder_idx, 0);
    assert_eq!(model.message_idx, 0);
}

#[test]
fn enter_on_a_folder_loads_it_and_moves_focus_to_the_messages() {
    let mut model = loaded();
    model.focus = Focus::Folders;
    model.folder_idx = 1;

    let cmds = press(&mut model, Key::Enter);
    assert_eq!(cmds, vec![Cmd::LoadMessages { mailbox_id: 2 }]);
    assert_eq!(model.focus, Focus::Messages);
    assert_eq!(model.open_folder, Some(2));
    assert!(model.messages.is_empty(), "the stale list is cleared");
}

#[test]
fn a_listing_for_a_folder_the_user_already_left_is_discarded() {
    let mut model = loaded();
    model.focus = Focus::Folders;
    model.folder_idx = 1;
    press(&mut model, Key::Enter); // now waiting on folder 2

    // Folder 1's slow listing finally lands. Applying it would drag the pane
    // back to a folder nobody is looking at.
    let cmds = update(
        &mut model,
        Msg::Messages {
            mailbox_id: 1,
            result: Ok(vec![row(99)]),
        },
    );
    assert!(cmds.is_empty());
    assert!(model.messages.is_empty(), "the stale page was dropped");

    update(
        &mut model,
        Msg::Messages {
            mailbox_id: 2,
            result: Ok(vec![row(50)]),
        },
    );
    assert_eq!(model.messages.len(), 1);
}

#[test]
fn enter_on_a_message_opens_the_viewer_once_the_body_arrives() {
    let mut model = loaded();
    model.message_idx = 1;

    let cmds = press(&mut model, Key::Enter);
    assert_eq!(cmds, vec![Cmd::Open { message_id: 11 }]);
    assert_eq!(
        model.screen,
        Screen::List,
        "the screen does not change until the body is here"
    );

    update(
        &mut model,
        Msg::Opened {
            message_id: 11,
            result: Ok(OpenMessage {
                id: 11,
                body: vec!["line one".to_owned(), "line two".to_owned()],
                ..OpenMessage::default()
            }),
        },
    );
    assert_eq!(model.screen, Screen::Viewer);
    assert_eq!(model.scroll, 0);
}

#[test]
fn a_slow_open_for_a_message_the_user_left_behind_is_discarded() {
    let mut model = loaded();
    press(&mut model, Key::Enter); // asks for message 10

    // The user changes their mind and opens a different one.
    press(&mut model, Key::Char('j'));
    let cmds = press(&mut model, Key::Enter);
    assert_eq!(cmds, vec![Cmd::Open { message_id: 11 }]);

    // Message 10's body finally lands. Showing it would open a viewer on a
    // message the user is no longer asking for.
    update(
        &mut model,
        Msg::Opened {
            message_id: 10,
            result: Ok(OpenMessage {
                id: 10,
                ..OpenMessage::default()
            }),
        },
    );
    assert_eq!(model.screen, Screen::List);
    assert!(model.open.is_none());

    update(
        &mut model,
        Msg::Opened {
            message_id: 11,
            result: Ok(OpenMessage {
                id: 11,
                ..OpenMessage::default()
            }),
        },
    );
    assert_eq!(model.screen, Screen::Viewer);
    assert_eq!(model.open.as_ref().map(|o| o.id), Some(11));
}

#[test]
fn switching_folder_abandons_a_pending_open() {
    let mut model = loaded();
    press(&mut model, Key::Enter); // asks for message 10

    model.focus = Focus::Folders;
    model.folder_idx = 1;
    press(&mut model, Key::Enter); // loads the Archive folder instead

    update(
        &mut model,
        Msg::Opened {
            message_id: 10,
            result: Ok(OpenMessage {
                id: 10,
                ..OpenMessage::default()
            }),
        },
    );
    assert_eq!(
        model.screen,
        Screen::List,
        "the abandoned body must not open a viewer over the new folder"
    );
}

#[test]
fn the_viewer_scrolls_with_j_k_gg_and_g() {
    let mut model = loaded();
    model.screen = Screen::Viewer;
    model.open = Some(OpenMessage {
        id: 10,
        body: (0..5).map(|n| format!("line {n}")).collect(),
        ..OpenMessage::default()
    });

    press(&mut model, Key::Char('j'));
    assert_eq!(model.scroll, 1);
    press(&mut model, Key::Char('G'));
    assert_eq!(model.scroll, 4);
    keys(&mut model, "gg");
    assert_eq!(model.scroll, 0);
    press(&mut model, Key::Char('k'));
    assert_eq!(model.scroll, 0, "clamped at the top");
}

#[test]
fn q_backs_out_of_the_viewer_before_it_quits() {
    let mut model = loaded();
    model.screen = Screen::Viewer;
    model.open = Some(OpenMessage {
        id: 10,
        ..OpenMessage::default()
    });

    press(&mut model, Key::Char('q'));
    assert_eq!(model.screen, Screen::List);
    assert!(!model.quit, "the first q returns to the list");

    press(&mut model, Key::Char('q'));
    assert!(model.quit, "the second q quits");
}

#[test]
fn ctrl_c_quits_from_anywhere_including_a_modal() {
    let mut model = loaded();
    model.overlay = Some(Overlay::Help);
    press(&mut model, Key::CTRL_C);
    assert!(model.quit);
}

// ---------------------------------------------------------------------------
// help
// ---------------------------------------------------------------------------

#[test]
fn question_mark_opens_help_and_it_closes_on_q_esc_or_another_question_mark() {
    for closer in [Key::Char('q'), Key::Esc, Key::Char('?')] {
        let mut model = loaded();
        press(&mut model, Key::Char('?'));
        assert_eq!(model.overlay, Some(Overlay::Help));

        press(&mut model, closer);
        assert_eq!(model.overlay, None, "{closer:?} closed the help");
        assert!(!model.quit, "{closer:?} must not also quit");
    }
}

#[test]
fn keys_do_not_reach_the_list_while_help_is_up() {
    let mut model = loaded();
    press(&mut model, Key::Char('?'));
    let cmds = press(&mut model, Key::Char('j'));
    assert!(cmds.is_empty());
    assert_eq!(model.message_idx, 0, "j scrolled nothing behind the modal");
}

// ---------------------------------------------------------------------------
// actions
// ---------------------------------------------------------------------------

#[test]
fn archive_moves_the_message_into_the_archive_folder() {
    let mut model = loaded();
    let cmds = press(&mut model, Key::Char('a'));
    assert_eq!(
        cmds,
        vec![Cmd::Move {
            message_id: 10,
            dest_mailbox_id: 2,
            label: "archived".to_owned(),
        }]
    );

    update(
        &mut model,
        Msg::Done {
            label: "archived".to_owned(),
            result: Ok(Effect::Removed(10)),
        },
    );
    assert_eq!(
        model.messages.iter().map(|m| m.id).collect::<Vec<_>>(),
        vec![11, 12],
        "the row leaves the list once the daemon confirms, not before"
    );
}

#[test]
fn archive_without_an_archive_folder_says_so_instead_of_failing_an_rpc() {
    let mut model = loaded();
    model.folders.retain(|f| f.name != "Archive");

    let cmds = press(&mut model, Key::Char('a'));
    assert!(cmds.is_empty());
    assert_eq!(model.level, Level::Error);
    assert!(
        model.status.contains("no archive folder"),
        "{}",
        model.status
    );
}

#[test]
fn archive_finds_a_nested_archive_folder_by_its_leaf_name() {
    // Gmail reports `[Gmail]/All Mail`; Dovecot commonly reports
    // `INBOX/Archive`. A full-name match would find neither.
    for (name, id) in [("[Gmail]/All Mail", 42), ("INBOX/Archive", 43)] {
        let mut model = loaded();
        model.folders.retain(|f| f.name != "Archive");
        model.folders.push(Folder {
            id,
            name: name.to_owned(),
            message_count: 0,
        });

        let cmds = press(&mut model, Key::Char('a'));
        assert_eq!(
            cmds,
            vec![Cmd::Move {
                message_id: 10,
                dest_mailbox_id: id,
                label: "archived".to_owned(),
            }],
            "{name} was not recognised as an archive folder"
        );
    }
}

#[test]
fn archive_never_targets_the_folder_already_open() {
    let mut model = loaded();
    model.folder_idx = 1;
    model.open_folder = Some(2); // sitting in Archive itself

    let cmds = press(&mut model, Key::Char('a'));
    assert!(cmds.is_empty(), "archiving into the open folder is a no-op");
    assert!(model.status.contains("no archive folder"));
}

#[test]
fn delete_asks_first_and_only_deletes_on_y() {
    let mut model = loaded();
    let cmds = press(&mut model, Key::Char('d'));
    assert!(cmds.is_empty(), "nothing is sent before the answer");
    assert!(matches!(model.overlay, Some(Overlay::Confirm { .. })));

    let cmds = press(&mut model, Key::Char('y'));
    assert_eq!(cmds, vec![Cmd::Delete { message_id: 10 }]);
    assert_eq!(model.overlay, None);
}

#[test]
fn declining_the_delete_confirmation_sends_nothing() {
    for decline in [Key::Char('n'), Key::Esc, Key::Char('q')] {
        let mut model = loaded();
        press(&mut model, Key::Char('d'));
        let cmds = press(&mut model, decline);
        assert!(cmds.is_empty(), "{decline:?} sent a command");
        assert_eq!(model.overlay, None);
        assert!(!model.quit, "{decline:?} must not quit the TUI");
    }
}

#[test]
fn a_delete_that_fails_leaves_the_row_in_place_and_reports_why() {
    let mut model = loaded();
    press(&mut model, Key::Char('d'));
    press(&mut model, Key::Char('y'));

    update(
        &mut model,
        Msg::Done {
            label: "deleted".to_owned(),
            result: Err("mailbox is read-only".to_owned()),
        },
    );
    assert_eq!(
        model.messages.len(),
        3,
        "nothing was removed optimistically"
    );
    assert_eq!(model.level, Level::Error);
    assert!(model.status.contains("read-only"));
}

#[test]
fn s_toggles_seen_by_sending_the_whole_intended_flag_set() {
    let mut model = loaded();
    model.messages[0].flags = vec![FLAGGED.to_owned()];

    let cmds = press(&mut model, Key::Char('s'));
    assert_eq!(
        cmds,
        vec![Cmd::SetFlags {
            message_id: 10,
            // SetFlags is a wholesale replace, so \Flagged has to be resent
            // or toggling read would silently unflag the message.
            flags: vec![FLAGGED.to_owned(), SEEN.to_owned()],
            label: "marked read".to_owned(),
        }]
    );

    update(
        &mut model,
        Msg::Done {
            label: "marked read".to_owned(),
            result: Ok(Effect::Flags {
                message_id: 10,
                flags: vec![FLAGGED.to_owned(), SEEN.to_owned()],
            }),
        },
    );
    assert!(model.messages[0].has_flag(SEEN));

    let cmds = press(&mut model, Key::Char('s'));
    assert_eq!(
        cmds,
        vec![Cmd::SetFlags {
            message_id: 10,
            flags: vec![FLAGGED.to_owned()],
            label: "marked not read".to_owned(),
        }]
    );
}

#[test]
fn f_toggles_flagged() {
    let mut model = loaded();
    let cmds = press(&mut model, Key::Char('f'));
    assert_eq!(
        cmds,
        vec![Cmd::SetFlags {
            message_id: 10,
            flags: vec![FLAGGED.to_owned()],
            label: "marked flagged".to_owned(),
        }]
    );
}

#[test]
fn copy_and_move_pick_a_destination_folder_first() {
    let mut model = loaded();

    let cmds = press(&mut model, Key::Char('c'));
    assert!(cmds.is_empty());
    assert_eq!(
        model.overlay,
        Some(Overlay::Pick {
            what: PickFor::Copy,
            message_ids: vec![10],
            idx: 0
        })
    );

    press(&mut model, Key::Char('j'));
    press(&mut model, Key::Char('j'));
    let cmds = press(&mut model, Key::Enter);
    assert_eq!(
        cmds,
        vec![Cmd::Copy {
            message_id: 10,
            dest_mailbox_id: 3,
        }]
    );

    let cmds = press(&mut model, Key::Char('M'));
    assert!(cmds.is_empty());
    press(&mut model, Key::Char('j'));
    let cmds = press(&mut model, Key::Enter);
    assert_eq!(
        cmds,
        vec![Cmd::Move {
            message_id: 10,
            dest_mailbox_id: 2,
            label: "moved to Archive".to_owned(),
        }]
    );
}

#[test]
fn the_picker_acts_on_the_open_message_when_the_viewer_is_up() {
    // The picker used to re-derive its target from the *list* cursor when
    // Enter was pressed, so `M` from the viewer moved whatever row happened to
    // be highlighted behind it.
    let mut model = loaded();
    model.message_idx = 0;
    model.screen = Screen::Viewer;
    model.open = Some(OpenMessage {
        id: 12,
        ..OpenMessage::default()
    });

    press(&mut model, Key::Char('M'));
    press(&mut model, Key::Char('j'));
    let cmds = press(&mut model, Key::Enter);
    assert_eq!(
        cmds,
        vec![Cmd::Move {
            message_id: 12,
            dest_mailbox_id: 2,
            label: "moved to Archive".to_owned(),
        }]
    );
}

#[test]
fn new_mail_arriving_under_an_open_picker_does_not_change_what_it_moves() {
    // The list is live: a `Msg::Changed` reload replaces `messages` and
    // re-clamps the cursor. If the picker resolved its target on Enter, mail
    // landing between "press M" and "press Enter" would silently re-point it
    // — and `Move` has no confirmation step to catch that.
    let mut model = loaded();
    model.message_idx = 2; // message 12

    press(&mut model, Key::Char('M'));
    update(
        &mut model,
        Msg::Messages {
            mailbox_id: 1,
            result: Ok(vec![row(99)]),
        },
    );
    assert_eq!(model.message_idx, 0, "the reload moved the cursor");

    let cmds = press(&mut model, Key::Enter);
    assert_eq!(
        cmds,
        vec![Cmd::Move {
            message_id: 12,
            dest_mailbox_id: 1,
            label: "moved to INBOX".to_owned(),
        }],
        "the picker still targets the message it was opened on"
    );
}

#[test]
fn a_copy_leaves_the_source_row_alone() {
    let mut model = loaded();
    update(
        &mut model,
        Msg::Done {
            label: "copied".to_owned(),
            result: Ok(Effect::None),
        },
    );
    assert_eq!(model.messages.len(), 3);
}

#[test]
fn escaping_the_folder_picker_sends_nothing() {
    let mut model = loaded();
    press(&mut model, Key::Char('M'));
    let cmds = press(&mut model, Key::Esc);
    assert!(cmds.is_empty());
    assert_eq!(model.overlay, None);
}

#[test]
fn reply_drafts_to_the_original_sender() {
    let mut model = loaded();
    let cmds = press(&mut model, Key::Char('r'));
    assert_eq!(
        cmds,
        vec![Cmd::Draft {
            kind: DraftKind::Reply,
            account_id: 7,
            from: "me@example.com".to_owned(),
            to: "alice@example.com".to_owned(),
            message_id: 10,
        }]
    );
}

#[test]
fn reply_to_a_message_with_no_sender_address_is_refused_not_guessed() {
    let mut model = loaded();
    model.messages[0].from_addr = None;
    let cmds = press(&mut model, Key::Char('r'));
    assert!(cmds.is_empty());
    assert_eq!(model.level, Level::Error);
    assert!(
        model.status.contains("no sender address"),
        "{}",
        model.status
    );
}

#[test]
fn drafting_without_an_account_address_is_refused_rather_than_sent_as_garbage() {
    // `ComposeService` validates the From addr-spec and would reject this;
    // catching it here means the user sees why instead of an INVALID_ARGUMENT.
    let mut model = loaded();
    model.account = Some(Account {
        username: Some("not-an-address".to_owned()),
        ..account()
    });
    let cmds = press(&mut model, Key::Char('r'));
    assert!(cmds.is_empty());
    assert!(model.status.contains("no address to send from"));
}

#[test]
fn forward_asks_for_a_recipient_and_drafts_to_it() {
    let mut model = loaded();
    let cmds = press(&mut model, Key::Char('F'));
    assert!(cmds.is_empty());
    assert!(matches!(
        model.overlay,
        Some(Overlay::Input {
            what: InputFor::ForwardTo,
            ..
        })
    ));

    for c in "bob@example.com".chars() {
        press(&mut model, Key::Char(c));
    }
    press(&mut model, Key::Backspace);
    for c in "m".chars() {
        press(&mut model, Key::Char(c));
    }

    let cmds = press(&mut model, Key::Enter);
    assert_eq!(
        cmds,
        vec![Cmd::Draft {
            kind: DraftKind::Forward,
            account_id: 7,
            from: "me@example.com".to_owned(),
            to: "bob@example.com".to_owned(),
            message_id: 10,
        }]
    );
    assert_eq!(model.overlay, None);
}

#[test]
fn an_empty_forward_recipient_drafts_nothing() {
    let mut model = loaded();
    press(&mut model, Key::Char('F'));
    let cmds = press(&mut model, Key::Enter);
    assert!(cmds.is_empty());
    assert_eq!(model.overlay, None);
}

#[test]
fn typing_into_the_forward_prompt_does_not_trigger_action_keys() {
    // `d` is delete and `q` is quit in the list. Inside a text prompt they
    // are letters, and an address containing either must not fire them.
    let mut model = loaded();
    press(&mut model, Key::Char('F'));
    for c in "dq@example.com".chars() {
        press(&mut model, Key::Char(c));
    }
    assert!(!model.quit);
    let typed = match &model.overlay {
        Some(Overlay::Input { buffer, .. }) => buffer.clone(),
        other => format!("the prompt closed: {other:?}"),
    };
    assert_eq!(typed, "dq@example.com");
}

#[test]
fn open_html_needs_an_open_message_that_actually_has_html() {
    let mut model = loaded();

    let cmds = press(&mut model, Key::Char('o'));
    assert!(cmds.is_empty(), "nothing is open yet");
    assert!(model.status.contains("open the message first"));

    model.screen = Screen::Viewer;
    model.open = Some(OpenMessage {
        id: 10,
        has_html: false,
        ..OpenMessage::default()
    });
    let cmds = press(&mut model, Key::Char('o'));
    assert!(cmds.is_empty());
    assert!(model.status.contains("no HTML part"));

    model.open = Some(OpenMessage {
        id: 10,
        has_html: true,
        ..OpenMessage::default()
    });
    let cmds = press(&mut model, Key::Char('o'));
    assert_eq!(cmds, vec![Cmd::OpenHtml { message_id: 10 }]);
}

#[test]
fn actions_in_the_viewer_apply_to_the_open_message_not_the_list_cursor() {
    let mut model = loaded();
    model.message_idx = 0;
    model.screen = Screen::Viewer;
    model.open = Some(OpenMessage {
        id: 12,
        ..OpenMessage::default()
    });

    let cmds = press(&mut model, Key::Char('a'));
    assert_eq!(
        cmds,
        vec![Cmd::Move {
            message_id: 12,
            dest_mailbox_id: 2,
            label: "archived".to_owned(),
        }]
    );
}

#[test]
fn removing_the_open_message_closes_the_viewer() {
    let mut model = loaded();
    model.screen = Screen::Viewer;
    model.open = Some(OpenMessage {
        id: 11,
        ..OpenMessage::default()
    });

    update(
        &mut model,
        Msg::Done {
            label: "deleted".to_owned(),
            result: Ok(Effect::Removed(11)),
        },
    );
    assert_eq!(model.screen, Screen::List);
    assert!(model.open.is_none());
}

#[test]
fn actions_on_an_empty_folder_report_rather_than_panic() {
    let mut model = loaded();
    model.messages.clear();
    model.message_idx = 0;

    for key in ['a', 'd', 's', 'f', 'c', 'M', 'r', 'F'] {
        let mut model = model.clone();
        let cmds = press(&mut model, Key::Char(key));
        assert!(cmds.is_empty(), "{key} issued a command with no message");
        assert_eq!(model.level, Level::Error, "{key} reported nothing");
    }
    let cmds = press(&mut model, Key::Enter);
    assert!(cmds.is_empty());
}

#[test]
fn the_cursor_stays_inside_the_list_when_the_last_row_is_removed() {
    let mut model = loaded();
    model.message_idx = 2;

    update(
        &mut model,
        Msg::Done {
            label: "archived".to_owned(),
            result: Ok(Effect::Removed(12)),
        },
    );
    assert_eq!(model.message_idx, 1);
    assert!(model.current_message().is_some());
}

// ---------------------------------------------------------------------------
// the event stream
// ---------------------------------------------------------------------------

#[test]
fn a_change_event_reloads_the_open_folder() {
    let mut model = loaded();
    let cmds = update(&mut model, Msg::Changed);
    assert_eq!(cmds, vec![Cmd::LoadMessages { mailbox_id: 1 }]);
    assert_eq!(model.inflight, 1);
}

#[test]
fn a_change_event_before_any_folder_is_open_does_nothing() {
    let mut model = Model::new();
    assert!(update(&mut model, Msg::Changed).is_empty());
}

#[test]
fn losing_the_event_stream_is_reported_and_does_not_disturb_the_inflight_count() {
    // This crate installs no tracing subscriber, so a swallowed stream error
    // would leave the user with a TUI that has quietly stopped noticing new
    // mail and a status line that reads perfectly normal.
    let mut model = loaded();
    model.inflight = 2;

    let cmds = update(
        &mut model,
        Msg::LiveUpdatesStopped("event retention gap".to_owned()),
    );
    assert!(cmds.is_empty());
    assert_eq!(model.level, Level::Error);
    assert!(
        model.status.contains("live updates stopped"),
        "{}",
        model.status
    );
    assert!(model.status.contains("retention gap"));
    assert_eq!(
        model.inflight, 2,
        "nobody asked for the stream, so nothing was counted for it"
    );
}

// ---------------------------------------------------------------------------
// flag arithmetic
// ---------------------------------------------------------------------------

#[test]
fn the_intended_flag_set_is_order_independent_and_deduplicated() {
    let row = MessageRow {
        flags: vec![SEEN.to_owned(), FLAGGED.to_owned(), SEEN.to_owned()],
        ..row(1)
    };
    assert_eq!(
        row.flags_with(FLAGGED, false),
        vec![SEEN.to_owned()],
        "the duplicate \\Seen collapses and \\Flagged is removed"
    );
    assert_eq!(
        row.flags_with("\\Draft", true),
        vec!["\\Draft".to_owned(), FLAGGED.to_owned(), SEEN.to_owned()]
    );
    assert_eq!(
        row.flags_with(SEEN, true),
        vec![FLAGGED.to_owned(), SEEN.to_owned()],
        "asking for a flag that is already there is not a toggle"
    );
}

// ---------------------------------------------------------------------------
// the keymap engine, as the model sees it (task 84)
// ---------------------------------------------------------------------------

use crate::keymap::{Chord, Keymap, Mode, MAX_COUNT};

/// A model in each mode the TUI can be in, with the keys that got it there.
/// Table-driven so a mode added later cannot quietly skip the escape checks.
fn in_every_mode() -> Vec<(Mode, Model)> {
    let viewer = {
        let mut model = loaded();
        model.screen = Screen::Viewer;
        model.open = Some(OpenMessage {
            id: 10,
            ..OpenMessage::default()
        });
        model
    };
    let mut modes = vec![(Mode::Normal, loaded()), (Mode::Viewer, viewer)];
    for (mode, key) in [
        (Mode::Visual, 'v'),
        (Mode::Insert, 'F'),
        (Mode::Pick, 'c'),
        (Mode::Confirm, 'd'),
        (Mode::Help, '?'),
        // The manual reuses `Mode::Help` rather than adding a mode, so it
        // appears twice here — deliberately: it is a *screen* rather than an
        // overlay, and the Ctrl-C/Esc checks below have to cover both ways
        // into that layer.
        (Mode::Help, 'K'),
    ] {
        let mut model = loaded();
        press(&mut model, Key::Char(key));
        assert_eq!(model.mode(), mode, "{key} did not open {mode:?}");
        modes.push((mode, model));
    }
    modes
}

#[test]
fn ctrl_c_quits_from_every_mode_there_is() {
    for (mode, mut model) in in_every_mode() {
        press(&mut model, Key::CTRL_C);
        assert!(model.quit, "Ctrl-C did not quit from {mode:?}");
    }
}

#[test]
fn esc_always_makes_progress_out_and_never_quits() {
    // The other half of "no mode is a trap": Esc leaves whatever is innermost
    // — and from the list, where there is nothing left to leave, it does
    // nothing at all rather than dropping the user out of the TUI.
    for (mode, mut model) in in_every_mode() {
        press(&mut model, Key::Esc);
        assert!(!model.quit, "Esc quit from {mode:?}");
        assert_eq!(
            model.mode(),
            Mode::Normal,
            "Esc did not get out of {mode:?}"
        );
    }
}

#[test]
fn esc_gets_out_even_from_inside_a_half_typed_chord() {
    let mut model = loaded();
    model.screen = Screen::Viewer;
    model.open = Some(OpenMessage {
        id: 10,
        ..OpenMessage::default()
    });
    keys(&mut model, "3g");
    assert!(!model.pending.is_empty(), "something is half-typed");

    press(&mut model, Key::Esc);
    assert!(model.pending.is_empty(), "the fragment is gone");
    assert_eq!(model.screen, Screen::List, "and Esc still did its job");
}

#[test]
fn an_unbound_key_does_nothing_and_leaves_nothing_behind() {
    let mut model = loaded();
    let cmds = keys(&mut model, "zZ");
    assert!(cmds.is_empty());
    assert!(model.pending.is_empty(), "an unbound key is not pending");
    assert_eq!(model.message_idx, 0);
    assert!(!model.quit);
    // And the next key still works, which is the whole point.
    press(&mut model, Key::Char('j'));
    assert_eq!(model.message_idx, 1);
}

#[test]
fn a_count_repeats_a_motion() {
    let mut model = loaded();
    model.messages = (0..10).map(row).collect();

    let cmds = keys(&mut model, "3j");
    assert!(cmds.is_empty(), "a motion issues no work");
    assert_eq!(model.message_idx, 3);

    keys(&mut model, "2k");
    assert_eq!(model.message_idx, 1);
}

#[test]
fn a_count_names_a_row_for_gg_and_g() {
    let mut model = loaded();
    model.messages = (0..10).map(row).collect();

    keys(&mut model, "4G");
    assert_eq!(model.message_idx, 3, "4G is the fourth row, 1-based");
    keys(&mut model, "2gg");
    assert_eq!(model.message_idx, 1);
    keys(&mut model, "G");
    assert_eq!(model.message_idx, 9, "a bare G is still the last row");
}

#[test]
fn an_enormous_count_clamps_instead_of_running_away() {
    let mut model = loaded();
    let cmds = keys(&mut model, "99999999j");
    assert!(cmds.is_empty());
    assert_eq!(
        model.message_idx, 2,
        "the cursor stops at the last row however big the count is"
    );
}

#[test]
fn a_count_never_multiplies_the_work_an_action_does() {
    // The bound that matters most: a count repeats *cursor arithmetic*, which
    // is O(1) and clamped. It must never turn one keystroke into 999 RPCs.
    let mut model = loaded();
    let cmds = keys(&mut model, "999d");
    assert!(cmds.is_empty(), "the confirmation comes first");
    let cmds = press(&mut model, Key::Char('y'));
    assert_eq!(
        cmds,
        vec![Cmd::Delete { message_id: 10 }],
        "one message was selected, so exactly one delete goes out"
    );

    let mut model = loaded();
    let cmds = keys(&mut model, "999a");
    assert_eq!(
        cmds.len(),
        1,
        "a counted archive is still one archive: {cmds:?}"
    );
}

#[test]
fn holding_a_digit_key_cannot_make_the_model_grow() {
    let mut model = loaded();
    for _ in 0..5_000 {
        let cmds = press(&mut model, Key::Char('9'));
        assert!(cmds.is_empty());
        assert!(
            model
                .pending
                .count()
                .is_some_and(|count| count <= MAX_COUNT),
            "the count ran past {MAX_COUNT}"
        );
        assert!(
            model.pending.label().len() <= 5,
            "the status indicator grew with the key being held: {:?}",
            model.pending.label()
        );
    }
    // And the TUI is still usable afterwards.
    press(&mut model, Key::Esc);
    assert!(model.pending.is_empty());
    press(&mut model, Key::Char('j'));
    assert_eq!(model.message_idx, 1);
}

#[test]
fn a_text_prompt_stops_accepting_characters_at_its_limit() {
    let mut model = loaded();
    press(&mut model, Key::Char('F'));
    for _ in 0..(MAX_INPUT * 2) {
        press(&mut model, Key::Char('a'));
    }
    let typed = match &model.overlay {
        Some(Overlay::Input { buffer, .. }) => buffer.clone(),
        other => panic!("the prompt closed: {other:?}"),
    };
    assert_eq!(
        typed.chars().count(),
        MAX_INPUT,
        "a key held against a prompt grew the buffer without limit"
    );
}

#[test]
fn a_pending_chord_does_not_survive_a_mode_change_it_did_not_cause() {
    // A slow `Get` landing opens the viewer while a `g` is half-typed. The
    // fragment was typed against the list; carrying it over would resolve a
    // chord the user never started.
    let mut model = loaded();
    press(&mut model, Key::Enter);
    press(&mut model, Key::Char('g'));
    assert!(!model.pending.is_empty());

    update(
        &mut model,
        Msg::Opened {
            message_id: 10,
            result: Ok(OpenMessage {
                id: 10,
                body: (0..5).map(|n| format!("line {n}")).collect(),
                ..OpenMessage::default()
            }),
        },
    );
    assert_eq!(model.screen, Screen::Viewer);
    assert!(model.pending.is_empty(), "the fragment did not follow");
}

#[test]
fn the_folder_picker_navigates_with_the_same_bindings_as_the_list() {
    let mut model = loaded();
    press(&mut model, Key::Char('c'));
    keys(&mut model, "G");
    let cmds = press(&mut model, Key::Enter);
    assert_eq!(
        cmds,
        vec![Cmd::Copy {
            message_id: 10,
            dest_mailbox_id: 3,
        }],
        "G reached the last folder"
    );

    press(&mut model, Key::Char('c'));
    keys(&mut model, "2j");
    keys(&mut model, "gg");
    let cmds = press(&mut model, Key::Enter);
    assert_eq!(
        cmds,
        vec![Cmd::Copy {
            message_id: 10,
            dest_mailbox_id: 1,
        }],
        "gg came back to the first"
    );
}

// ---------------------------------------------------------------------------
// rebinding and hot reload
// ---------------------------------------------------------------------------

fn keymap_from(toml: &str) -> Keymap {
    match crate::keymap::file::parse(toml, "keys.toml") {
        Ok(keymap) => keymap,
        Err(error) => panic!("{toml:?} should have parsed: {error}"),
    }
}

#[test]
fn a_reloaded_keymap_changes_what_a_key_does() {
    let mut model = loaded();
    let cmds = update(
        &mut model,
        Msg::Keymap {
            result: Ok(keymap_from("[normal]\nx = \"message.archive\"\nj = \"\"\n")),
            announce: true,
        },
    );
    assert!(cmds.is_empty());
    assert!(model.status.contains("reloaded"), "{}", model.status);

    let cmds = press(&mut model, Key::Char('x'));
    assert_eq!(
        cmds,
        vec![Cmd::Move {
            message_id: 10,
            dest_mailbox_id: 2,
            label: "archived".to_owned(),
        }],
        "the new binding took effect without a restart"
    );

    press(&mut model, Key::Char('j'));
    assert_eq!(model.message_idx, 0, "and the unbound one stopped working");
}

#[test]
fn a_silent_load_does_not_stamp_on_the_status_line() {
    let mut model = Model::new();
    update(&mut model, Msg::Boot);
    let booting = model.status.clone();

    update(
        &mut model,
        Msg::Keymap {
            result: Ok(Keymap::defaults()),
            announce: false,
        },
    );
    assert_eq!(
        model.status, booting,
        "the load at startup must not overwrite the boot progress"
    );
}

#[test]
fn a_broken_keys_file_is_reported_and_the_working_bindings_stay() {
    let mut model = loaded();
    update(
        &mut model,
        Msg::Keymap {
            result: Ok(keymap_from("[normal]\nx = \"quit\"\n")),
            announce: true,
        },
    );

    update(
        &mut model,
        Msg::Keymap {
            result: Err("keys.toml is not valid TOML: expected `]`".to_owned()),
            announce: true,
        },
    );
    assert_eq!(model.level, Level::Error);
    assert!(model.status.contains("not valid TOML"), "{}", model.status);

    press(&mut model, Key::Char('x'));
    assert!(
        model.quit,
        "a typo mid-edit must not take the user's bindings away"
    );
}

#[test]
fn a_reload_drops_whatever_was_half_typed_under_the_old_bindings() {
    let mut model = loaded();
    press(&mut model, Key::Char('g'));
    assert!(!model.pending.is_empty());

    update(
        &mut model,
        Msg::Keymap {
            result: Ok(Keymap::defaults()),
            announce: false,
        },
    );
    assert!(model.pending.is_empty());
}

// ---------------------------------------------------------------------------
// visual mode
// ---------------------------------------------------------------------------

#[test]
fn v_starts_a_selection_that_j_and_k_extend() {
    let mut model = loaded();
    press(&mut model, Key::Char('v'));
    assert_eq!(model.mode(), Mode::Visual);
    assert_eq!(model.selection(), Some((0, 0)));

    press(&mut model, Key::Char('j'));
    assert_eq!(model.selection(), Some((0, 1)));
    assert!(model.is_selected(0) && model.is_selected(1) && !model.is_selected(2));

    // Back past the anchor: the selection is a range, not a direction.
    keys(&mut model, "kk");
    assert_eq!(model.selection(), Some((0, 0)));
}

#[test]
fn a_visual_selection_archives_every_message_in_it() {
    let mut model = loaded();
    keys(&mut model, "vj");
    let cmds = press(&mut model, Key::Char('a'));
    assert_eq!(
        cmds,
        vec![
            Cmd::Move {
                message_id: 10,
                dest_mailbox_id: 2,
                label: "archived".to_owned(),
            },
            Cmd::Move {
                message_id: 11,
                dest_mailbox_id: 2,
                label: "archived".to_owned(),
            }
        ]
    );
    assert_eq!(model.inflight, 2, "both are counted, not one");
    assert_eq!(
        model.mode(),
        Mode::Normal,
        "the selection ends with the action"
    );
}

#[test]
fn a_bulk_flag_toggle_picks_one_intent_for_the_whole_selection() {
    let mut model = loaded();
    model.messages[0].flags = vec![SEEN.to_owned()];

    keys(&mut model, "vj");
    let cmds = press(&mut model, Key::Char('s'));
    assert_eq!(
        cmds,
        vec![
            Cmd::SetFlags {
                message_id: 10,
                flags: vec![SEEN.to_owned()],
                label: "marked read".to_owned(),
            },
            Cmd::SetFlags {
                message_id: 11,
                flags: vec![SEEN.to_owned()],
                label: "marked read".to_owned(),
            }
        ],
        "a mixed selection is marked read, not toggled row by row"
    );

    // Now that every message in the selection has it, the same key clears it.
    let mut model = loaded();
    model.messages[0].flags = vec![SEEN.to_owned()];
    model.messages[1].flags = vec![SEEN.to_owned()];
    keys(&mut model, "vj");
    let cmds = press(&mut model, Key::Char('s'));
    assert_eq!(
        cmds.first(),
        Some(&Cmd::SetFlags {
            message_id: 10,
            flags: Vec::new(),
            label: "marked not read".to_owned(),
        })
    );
}

#[test]
fn a_visual_delete_asks_once_and_names_the_count() {
    let mut model = loaded();
    keys(&mut model, "vjj");
    let cmds = press(&mut model, Key::Char('d'));
    assert!(cmds.is_empty());
    let prompt = match &model.overlay {
        Some(Overlay::Confirm { prompt, .. }) => prompt.clone(),
        other => panic!("no confirmation: {other:?}"),
    };
    assert!(prompt.contains('3'), "{prompt}");

    let cmds = press(&mut model, Key::Char('y'));
    assert_eq!(
        cmds,
        vec![
            Cmd::Delete { message_id: 10 },
            Cmd::Delete { message_id: 11 },
            Cmd::Delete { message_id: 12 },
        ]
    );
}

#[test]
fn a_selection_bigger_than_the_bulk_cap_is_refused_rather_than_truncated() {
    let mut model = loaded();
    model.messages = (0..(i64::try_from(MAX_BULK).unwrap_or(i64::MAX) + 50))
        .map(row)
        .collect();

    keys(&mut model, "vG");
    let cmds = press(&mut model, Key::Char('a'));
    assert!(
        cmds.is_empty(),
        "acting on the first {MAX_BULK} of a bigger selection is worse than acting on none"
    );
    assert_eq!(model.level, Level::Error);
    assert!(
        model.status.contains(&MAX_BULK.to_string()),
        "the cap is named so the user can narrow the selection: {}",
        model.status
    );
    assert_eq!(
        model.mode(),
        Mode::Visual,
        "the selection survives the refusal"
    );
}

#[test]
fn visual_mode_refuses_the_actions_that_only_make_sense_on_one_message() {
    for key in ['r', 'F'] {
        let mut model = loaded();
        keys(&mut model, "vj");
        let cmds = press(&mut model, Key::Char(key));
        assert!(cmds.is_empty(), "{key} acted on a selection");
        assert_eq!(model.level, Level::Error);
        assert!(model.status.contains("one message"), "{}", model.status);
        assert_eq!(model.mode(), Mode::Visual, "and nothing was lost");
    }

    let mut model = loaded();
    keys(&mut model, "vj");
    let cmds = press(&mut model, Key::Enter);
    assert!(cmds.is_empty(), "Enter opened one of a selection");
}

#[test]
fn o_swaps_the_ends_of_the_selection_instead_of_opening_html() {
    // Visual mode's own layer shadows `o`; the single-message action it hides
    // is refused on a selection anyway.
    let mut model = loaded();
    keys(&mut model, "vj");
    assert_eq!((model.visual, model.message_idx), (Some(0), 1));

    press(&mut model, Key::Char('o'));
    assert_eq!(
        (model.visual, model.message_idx),
        (Some(1), 0),
        "the cursor moved to the other end"
    );
    assert_eq!(model.selection(), Some((0, 1)), "covering the same rows");
}

#[test]
fn a_selection_ends_when_its_rows_do() {
    let mut model = loaded();
    keys(&mut model, "vj");

    update(
        &mut model,
        Msg::Messages {
            mailbox_id: 1,
            result: Ok(Vec::new()),
        },
    );
    assert_eq!(model.visual, None, "a selection over no rows is not one");
    assert_eq!(model.mode(), Mode::Normal);
}

#[test]
fn visual_mode_needs_messages_to_select() {
    let mut model = loaded();
    model.messages.clear();
    press(&mut model, Key::Char('v'));
    assert_eq!(model.mode(), Mode::Normal);
    assert_eq!(model.level, Level::Error);

    let mut model = loaded();
    model.focus = Focus::Folders;
    press(&mut model, Key::Char('v'));
    assert_eq!(model.mode(), Mode::Normal);
    assert!(model.status.contains("message list"), "{}", model.status);
}

#[test]
fn an_overlays_own_action_bound_elsewhere_does_not_close_the_wrong_overlay() {
    // Every action id is bindable in every mode, so `confirm.accept` can end
    // up bound where there is no confirmation to accept. Each of these takes
    // the overlay to read what it captured; taking it first and checking
    // afterwards would close whatever *was* up and do nothing else — a modal
    // that vanishes without acting, from a key the user rebound themselves.
    for (opener, mode, foreign) in [
        ('c', Mode::Pick, ["confirm.accept", "input.submit"]),
        ('d', Mode::Confirm, ["pick.accept", "input.submit"]),
        ('F', Mode::Insert, ["pick.accept", "confirm.accept"]),
    ] {
        for action in foreign {
            let mut model = loaded();
            model.keymap = keymap_from(&format!("[{}]\nz = \"{action}\"\n", mode.id()));
            press(&mut model, Key::Char(opener));
            assert_eq!(model.mode(), mode, "{opener} did not open {mode:?}");

            let cmds = press(&mut model, Key::Char('z'));
            assert!(cmds.is_empty(), "{action} in {mode:?} issued work");
            assert_eq!(
                model.mode(),
                mode,
                "{action} closed the {mode:?} overlay it has nothing to do with"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// the manual (task 103)
// ---------------------------------------------------------------------------

/// A model on the manual's front page, reached the way a user reaches it.
fn manual_open() -> Model {
    let mut model = loaded();
    press(&mut model, Key::Char('K'));
    assert_eq!(model.screen, Screen::Manual);
    model
}

fn manual(model: &Model) -> &ManualState {
    model
        .manual
        .as_ref()
        .unwrap_or_else(|| panic!("the manual is not open"))
}

/// Move the cursor down with `j` until it is on a row carrying a link, and
/// return that row's target.
///
/// Driven with real keys rather than by assigning `cursor`, so the row cursor
/// itself (`Cursor::Manual`) is under test too.
fn walk_to_a_link(model: &mut Model) -> &'static str {
    let lines = manual_doc(model).expect("a rendered page").lines.len();
    for _ in 0..lines {
        let target = manual_doc(model)
            .and_then(|doc| doc.lines.get(manual(model).cursor).and_then(|l| l.link()));
        if let Some(target) = target {
            return target;
        }
        press(model, Key::Char('j'));
    }
    panic!("no row on this page carries a link");
}

#[test]
fn k_opens_the_manual_and_q_puts_the_list_back() {
    let mut model = manual_open();
    assert_eq!(model.mode(), Mode::Help, "it reuses the help layer");
    assert_eq!(manual(&model).at, manual::Location::start());

    press(&mut model, Key::Char('q'));
    assert_eq!(model.screen, Screen::List);
    assert!(model.manual.is_none());
}

#[test]
fn the_manual_returns_to_the_viewer_when_that_is_where_it_was_opened_from() {
    let mut model = loaded();
    press(&mut model, Key::Enter);
    update(
        &mut model,
        Msg::Opened {
            message_id: 10,
            result: Ok(OpenMessage {
                id: 10,
                ..OpenMessage::default()
            }),
        },
    );
    assert_eq!(model.screen, Screen::Viewer);

    press(&mut model, Key::Char('K'));
    assert_eq!(model.screen, Screen::Manual);
    press(&mut model, Key::Esc);
    assert_eq!(
        model.screen,
        Screen::Viewer,
        "reading the manual mid-message puts you back on the message"
    );
    assert!(model.open.is_some());
}

#[test]
fn the_manual_falls_back_to_the_list_when_the_message_it_covered_is_gone() {
    let mut model = loaded();
    press(&mut model, Key::Enter);
    update(
        &mut model,
        Msg::Opened {
            message_id: 10,
            result: Ok(OpenMessage {
                id: 10,
                ..OpenMessage::default()
            }),
        },
    );
    press(&mut model, Key::Char('K'));
    // Archived from another client while the manual was up.
    update(
        &mut model,
        Msg::Done {
            label: "archived".to_owned(),
            result: Ok(Effect::Removed(10)),
        },
    );
    assert_eq!(
        model.screen,
        Screen::Manual,
        "a message vanishing elsewhere does not close the page being read"
    );
    press(&mut model, Key::Esc);
    assert_eq!(model.screen, Screen::List);
}

#[test]
fn the_manual_state_and_the_screen_never_disagree() {
    // The two are one piece of state in two fields (`set_screen`'s own docs),
    // so this drives everything that changes a screen and asserts the pairing
    // after each one rather than trusting the call sites.
    let mut model = manual_open();
    let steps: Vec<Msg> = vec![
        Msg::Key(Key::Char('j')),
        Msg::Changed,
        Msg::Keymap {
            result: Ok(Keymap::defaults()),
            announce: true,
        },
        Msg::Opened {
            message_id: 10,
            result: Ok(OpenMessage {
                id: 10,
                ..OpenMessage::default()
            }),
        },
        Msg::Key(Key::Char('K')),
        Msg::Done {
            label: "archived".to_owned(),
            result: Ok(Effect::Removed(10)),
        },
        Msg::Key(Key::Esc),
        Msg::Folders(Ok(folders())),
        Msg::Key(Key::Char('K')),
        Msg::Key(Key::Char('q')),
        Msg::Boot,
    ];
    for step in steps {
        let label = format!("{step:?}");
        update(&mut model, step);
        assert_eq!(
            model.screen == Screen::Manual,
            model.manual.is_some(),
            "after {label}: screen={:?} manual={:?}",
            model.screen,
            model.manual.is_some()
        );
    }
}

#[test]
fn a_slow_message_open_does_not_replace_the_manual() {
    let mut model = loaded();
    press(&mut model, Key::Enter); // asks for message 10
    press(&mut model, Key::Char('K')); // changed their mind
    let cmds = update(
        &mut model,
        Msg::Opened {
            message_id: 10,
            result: Ok(OpenMessage {
                id: 10,
                ..OpenMessage::default()
            }),
        },
    );
    assert!(cmds.is_empty());
    assert_eq!(
        model.screen,
        Screen::Manual,
        "the abandoned Get must not yank a viewer open over the manual"
    );
    assert!(model.open.is_none());
}

#[test]
fn nothing_the_manual_does_needs_the_daemon() {
    // The manual has to work when the daemon will not start, so no action on
    // it may return a `Cmd` at all — that is the property, not an
    // implementation detail.
    let mut model = manual_open();
    let mut cmds = Vec::new();
    for key in [
        Key::Char('j'),
        Key::Char('k'),
        Key::Char('G'),
        Key::Char('g'),
        Key::Char('g'),
        Key::ctrl('o'),
        Key::ctrl('i'),
        Key::Char('n'),
        Key::Char('N'),
        Key::Char('/'),
        Key::Esc,
        Key::Char('g'),
        Key::Char('/'),
        Key::Esc,
        Key::Enter,
    ] {
        cmds.extend(press(&mut model, key));
    }
    assert!(cmds.is_empty(), "the manual issued work: {cmds:?}");
}

#[test]
fn enter_follows_the_link_under_the_cursor_and_ctrl_o_comes_back() {
    let mut model = manual_open();
    let target = walk_to_a_link(&mut model);
    let cursor_before = manual(&model).cursor;

    press(&mut model, Key::Enter);
    assert_eq!(manual(&model).at, manual::Location::Page(target.to_owned()));
    assert_eq!(manual(&model).cursor, 0, "a new page starts at the top");
    assert!(manual(&model).can_jump_back());

    press(&mut model, Key::ctrl('o'));
    assert_eq!(manual(&model).at, manual::Location::start());
    assert_eq!(
        manual(&model).cursor,
        cursor_before,
        "the jump list remembers where you were on each page, not just which \
         page it was"
    );
    assert!(manual(&model).can_jump_forward());

    press(&mut model, Key::ctrl('i'));
    assert_eq!(manual(&model).at, manual::Location::Page(target.to_owned()));
}

#[test]
fn tab_goes_forward_too_because_most_terminals_cannot_send_ctrl_i() {
    let mut model = manual_open();
    walk_to_a_link(&mut model);
    press(&mut model, Key::Enter);
    press(&mut model, Key::ctrl('o'));
    press(&mut model, Key::Tab);
    assert_ne!(
        manual(&model).at,
        manual::Location::start(),
        "Tab is the same byte as Ctrl-I on a terminal without the kitty \
         keyboard protocol, so it has to mean the same thing"
    );
}

#[test]
fn there_is_nothing_to_go_back_to_from_the_first_page_and_it_says_so() {
    let mut model = manual_open();
    let cmds = press(&mut model, Key::ctrl('o'));
    assert!(cmds.is_empty());
    assert_eq!(model.level, Level::Error);
    assert!(model.status.contains("back"), "{}", model.status);
    assert_eq!(manual(&model).at, manual::Location::start());
}

#[test]
fn following_a_link_forgets_what_was_ahead() {
    let mut model = manual_open();
    walk_to_a_link(&mut model);
    press(&mut model, Key::Enter);
    press(&mut model, Key::ctrl('o'));
    assert!(manual(&model).can_jump_forward());

    // A new branch: what was ahead is no longer reachable from here, exactly
    // as in a browser.
    let elsewhere = manual::PAGES
        .iter()
        .map(|page| page.anchor)
        .find(|anchor| *anchor != manual::START)
        .expect("another page");
    enter_manual(&mut model, manual::Location::Page(elsewhere.to_owned()));
    assert!(!manual(&model).can_jump_forward());
}

#[test]
fn the_jump_list_is_bounded() {
    let mut model = manual_open();
    // Alternate between two pages so every step is a real move.
    for step in 0..MAX_JUMPS * 2 {
        let anchor = if step % 2 == 0 { "keys" } else { "modes" };
        enter_manual(&mut model, manual::Location::Page(anchor.to_owned()));
    }
    assert_eq!(
        manual(&model).back.len(),
        MAX_JUMPS,
        "following links for an hour must not grow a Vec for as long as it \
         lasts"
    );
}

#[test]
fn navigating_to_the_page_already_showing_is_not_a_jump() {
    let mut model = manual_open();
    enter_manual(&mut model, manual::Location::start());
    assert!(
        !manual(&model).can_jump_back(),
        "otherwise pressing K on the front page would fill the jump list with \
         copies of it"
    );
}

#[test]
fn slash_searches_the_page_rather_than_the_mailbox() {
    let mut model = manual_open();
    press(&mut model, Key::Char('/'));
    assert!(
        model.overlay.is_none(),
        "the mailbox search overlay would cover the text it was opened to \
         search"
    );
    assert_eq!(model.mode(), Mode::Prompt, "the manual's own search line");
    keys(&mut model, "modes");
    assert_eq!(
        manual(&model).prompt.as_ref().map(|p| p.pattern.as_str()),
        Some("modes")
    );
    assert_eq!(
        manual(&model).prompt.as_ref().map(|p| p.scope),
        Some(Scope::Page)
    );
    // It previews as it is typed, before Enter.
    assert_eq!(manual(&model).pattern(), Some("modes"));
}

#[test]
fn an_in_page_search_lands_on_the_first_match_and_n_steps_through_the_rest() {
    let mut model = manual_open();
    press(&mut model, Key::Char('/'));
    keys(&mut model, "manual");
    press(&mut model, Key::Enter);

    let hits = manual_matches(&model, "manual");
    assert!(hits.len() > 1, "the front page mentions it more than once");
    assert_eq!(manual(&model).cursor, hits[0]);
    assert_eq!(manual(&model).highlight.as_deref(), Some("manual"));
    assert!(!manual(&model).typing(), "the prompt closed on Enter");

    press(&mut model, Key::Char('n'));
    assert_eq!(manual(&model).cursor, hits[1]);
    press(&mut model, Key::Char('N'));
    assert_eq!(manual(&model).cursor, hits[0]);
    // And it wraps rather than stopping dead.
    press(&mut model, Key::Char('N'));
    assert_eq!(manual(&model).cursor, hits[hits.len() - 1]);
}

#[test]
fn searching_for_something_not_on_the_page_says_so_and_points_at_helpgrep() {
    let mut model = manual_open();
    press(&mut model, Key::Char('/'));
    keys(&mut model, "zzzznope");
    press(&mut model, Key::Enter);
    assert_eq!(model.level, Level::Error);
    assert!(model.status.contains("g/"), "{}", model.status);
}

#[test]
fn n_before_anything_has_been_searched_for_says_so_rather_than_moving() {
    let mut model = manual_open();
    press(&mut model, Key::Char('n'));
    assert_eq!(manual(&model).cursor, 0);
    assert_eq!(model.level, Level::Error);
}

#[test]
fn an_empty_search_clears_rather_than_matching_everything() {
    let mut model = manual_open();
    press(&mut model, Key::Char('/'));
    keys(&mut model, "manual");
    press(&mut model, Key::Enter);
    assert!(manual(&model).highlight.is_some());

    press(&mut model, Key::Char('/'));
    press(&mut model, Key::Enter);
    assert_eq!(manual(&model).highlight, None);
}

#[test]
fn esc_leaves_the_prompt_then_the_highlight_then_the_manual() {
    let mut model = manual_open();
    press(&mut model, Key::Char('/'));
    keys(&mut model, "manual");
    press(&mut model, Key::Enter);
    press(&mut model, Key::Char('/'));
    assert!(manual(&model).typing());

    press(&mut model, Key::Esc);
    assert!(!manual(&model).typing(), "the prompt went first");
    assert!(
        manual(&model).highlight.is_some(),
        "and left the highlight alone"
    );

    press(&mut model, Key::Esc);
    assert_eq!(manual(&model).highlight, None, "then the highlight");
    assert_eq!(model.screen, Screen::Manual);

    press(&mut model, Key::Esc);
    assert_eq!(model.screen, Screen::List, "then the manual itself");
}

#[test]
fn g_slash_greps_every_page_and_a_hit_opens_it_with_the_pattern_still_showing() {
    let mut model = manual_open();
    keys(&mut model, "g/");
    assert_eq!(model.mode(), Mode::Prompt);
    assert_eq!(
        manual(&model).prompt.as_ref().map(|p| p.scope),
        Some(Scope::Manual)
    );
    keys(&mut model, "Command index");
    press(&mut model, Key::Enter);
    assert_eq!(
        manual(&model).at,
        manual::Location::Grep("Command index".to_owned())
    );

    let target = walk_to_a_link(&mut model);
    press(&mut model, Key::Enter);
    assert_eq!(manual(&model).at, manual::Location::Page(target.to_owned()));
    assert_eq!(
        manual(&model).highlight.as_deref(),
        Some("Command index"),
        "arriving with nothing highlighted would lose the one thing that made \
         the row a hit"
    );
    assert!(
        manual_matches(&model, "Command index").contains(&manual(&model).cursor),
        "and it lands on a matching line rather than the top of the page"
    );

    press(&mut model, Key::ctrl('o'));
    assert!(
        matches!(manual(&model).at, manual::Location::Grep(_)),
        "back goes to the hit list, which is what a jump list is for"
    );
}

#[test]
fn g_slash_from_the_message_list_is_the_mailbox_search_not_helpgrep() {
    // `g/` is bound in the help layer only, and the engine drops a dead
    // prefix one key at a time — so from the list `g` goes nowhere and the
    // `/` that follows reaches the mailbox search box. Pinned because the
    // alternative (a `g/` in `Mode::Normal`) would be a silent change to what
    // `/` does there.
    let mut model = loaded();
    keys(&mut model, "g/");
    assert_eq!(model.screen, Screen::List);
    assert!(
        matches!(model.overlay, Some(Overlay::Search(_))),
        "{:?}",
        model.overlay
    );
}

#[test]
fn helpgrep_reached_from_outside_the_manual_opens_the_manual_first() {
    // Reachable today through the palette, and from task 89's command line
    // once it exists: a grep prompt with nowhere to show its answer is not a
    // prompt worth raising.
    let mut model = loaded();
    press(&mut model, Key::ctrl('k'));
    // The palette resolves against action *ids*, and this one is
    // `manual.grep` — `helpgrep` is the verb spelling, which is task 89's
    // command line rather than this surface.
    keys(&mut model, "grep");
    press(&mut model, Key::Enter);
    assert_eq!(
        model.screen,
        Screen::Manual,
        "somebody who ran it still meant to search the manual"
    );
    assert!(model.overlay.is_none(), "the palette closed behind it");
    assert!(manual(&model).typing());
    assert_eq!(
        manual(&model).prompt.as_ref().map(|prompt| prompt.scope),
        Some(Scope::Manual)
    );
}

#[test]
fn enter_on_a_row_with_no_link_says_so_rather_than_doing_nothing() {
    let mut model = manual_open();
    // The front page's first line is its title, which is not a link.
    assert!(manual_doc(&model)
        .and_then(|doc| doc.lines.first().and_then(|line| line.link()))
        .is_none());
    let cmds = press(&mut model, Key::Enter);
    assert!(cmds.is_empty());
    assert_eq!(model.level, Level::Error);
    assert_eq!(manual(&model).at, manual::Location::start());
}

#[test]
fn enter_still_closes_the_key_reference_overlay() {
    // `<enter>` in `Mode::Help` moved from `Action::Cancel` to
    // `Action::MenuAccept` so that the manual could use it to follow a link.
    // The `?` overlay has no row cursor, so the behaviour there must not have
    // changed — task 102 is what replaces it with something richer.
    let mut model = loaded();
    press(&mut model, Key::Char('?'));
    assert_eq!(model.overlay, Some(Overlay::Help));
    press(&mut model, Key::Enter);
    assert_eq!(model.overlay, None);
    assert!(!model.quit);
}

#[test]
fn the_manual_opens_over_a_visual_selection_and_gives_it_back() {
    let mut model = loaded();
    press(&mut model, Key::Char('v'));
    press(&mut model, Key::Char('j'));
    assert_eq!(model.selection(), Some((0, 1)));

    press(&mut model, Key::Char('K'));
    assert_eq!(
        model.mode(),
        Mode::Help,
        "the keyboard belongs to the page being read, not to the selection \
         behind it"
    );
    press(&mut model, Key::Char('q'));
    assert_eq!(model.mode(), Mode::Visual);
    assert_eq!(model.selection(), Some((0, 1)), "the selection survived");
}

#[test]
fn the_manual_offers_no_message_to_act_on() {
    // Bound in `Mode::Normal`, so only reachable here through a rebind — the
    // natural one for somebody who wants `a`/`d` to work everywhere. Reaching
    // mail from behind a page of prose is exactly what must not happen, and
    // the *selection* case is the one that did: `targets` consults the
    // selection before it consults the screen, so `bulk_targets` resolved to
    // the rows underneath and archived them.
    for selection in [false, true] {
        let mut model = loaded();
        if selection {
            press(&mut model, Key::Char('v'));
            press(&mut model, Key::Char('j'));
            assert_eq!(model.selection(), Some((0, 1)));
        }
        press(&mut model, Key::Char('K'));
        model.keymap = keymap_from("[help]\nz = \"message.archive\"\n");

        let cmds = press(&mut model, Key::Char('z'));
        assert!(
            cmds.is_empty(),
            "archived mail from the manual (selection: {selection}): {cmds:?}"
        );
        assert_eq!(model.level, Level::Error);
        assert_eq!(model.screen, Screen::Manual);
        assert_eq!(
            model.visual.is_some(),
            selection,
            "and the selection is neither used nor thrown away"
        );
    }
}

#[test]
fn a_selection_made_on_the_list_does_not_act_from_the_viewer_either() {
    // The same root cause, on the screen it predates the manual on: a hit
    // opened from a search made mid-selection leaves the anchor set, and `a`
    // there used to archive the *list rows* rather than the message on screen.
    let mut model = loaded();
    press(&mut model, Key::Char('v'));
    press(&mut model, Key::Char('j'));
    let cmds = open_message_by_id(&mut model, 12);
    assert_eq!(cmds, vec![Cmd::Open { message_id: 12 }]);
    update(
        &mut model,
        Msg::Opened {
            message_id: 12,
            result: Ok(OpenMessage {
                id: 12,
                ..OpenMessage::default()
            }),
        },
    );
    assert_eq!(model.screen, Screen::Viewer);
    assert_eq!(
        model.mode(),
        Mode::Viewer,
        "and the mode follows the screen rather than the stale anchor"
    );
    assert_eq!(model.selection(), None, "the range is not on screen");

    let cmds = press(&mut model, Key::Char('a'));
    assert_eq!(
        cmds,
        vec![Cmd::Move {
            message_id: 12,
            dest_mailbox_id: 2,
            label: "archived".to_owned(),
        }],
        "it archives what the viewer is showing, not the rows behind it"
    );
}

#[test]
fn opening_a_folder_from_the_manual_leaves_it() {
    // `open_folder_by_id` is the finder's jump target; it sets the screen, and
    // a screen set behind the manual would be a manual nobody could see with
    // its state still allocated.
    let mut model = manual_open();
    let cmds = open_folder_by_id(&mut model, 2);
    assert_eq!(model.screen, Screen::List);
    assert!(model.manual.is_none());
    assert_eq!(cmds, vec![Cmd::LoadMessages { mailbox_id: 2 }]);
}

#[test]
fn a_page_shrinking_under_the_cursor_leaves_the_cursor_usable() {
    // Nothing re-clamps `ManualState::cursor` when the page it is on gets
    // *shorter* without a key being pressed, and the generated key reference
    // does exactly that on a `keys.toml` reload. Unclamped, `k` needed one
    // press per row of the difference before the highlighted row moved, and
    // `<enter>` reported "no link on this line" about a row that had one.
    let mut model = manual_open();
    open_manual_at(&mut model, "keys");
    press(&mut model, Key::Char('G'));
    let full = manual_doc(&model).expect("the keys page").lines.len();
    assert_eq!(manual(&model).cursor, full - 1);

    // A keymap the page renders *shorter*. `keys.toml` is additive today
    // (`keymap::file::parse` starts from the defaults), so this is built with
    // `Keymap::unbind` — the model must not assume the map it is handed is a
    // superset of the one it had, and `Msg::Keymap` replaces it wholesale.
    // Both layers keep their `<enter>`, so no section disappears.
    let mut smaller = Keymap::defaults();
    for mode in [Mode::Menu, Mode::Pick] {
        let chords: Vec<Chord> = smaller
            .layer(mode)
            .filter(|(_, action)| !matches!(action, Action::MenuAccept | Action::PickAccept))
            .map(|(chord, _)| chord.clone())
            .collect();
        for chord in chords {
            smaller.unbind(mode, &chord);
        }
    }
    update(
        &mut model,
        Msg::Keymap {
            result: Ok(smaller),
            announce: false,
        },
    );
    let shrunk = manual_doc(&model).expect("the keys page").lines.len();
    assert!(shrunk < full, "the page did shrink: {shrunk} vs {full}");
    assert_eq!(
        manual(&model).cursor_in(shrunk),
        shrunk - 1,
        "the cursor reads as the last row of the page it is actually on"
    );

    // And one `k` moves it, rather than being swallowed by the difference.
    press(&mut model, Key::Char('k'));
    let after = manual_doc(&model).expect("the keys page").lines.len();
    assert_eq!(manual(&model).cursor_in(after), after - 2);
}

#[test]
fn a_keymap_reload_changes_what_the_manual_says_about_keys() {
    // The whole reason the rendered page is not cached: a stored document is
    // one that keeps claiming the old binding.
    let mut model = manual_open();
    enter_manual(&mut model, manual::Location::Page("keys".to_owned()));
    let before = manual_doc(&model).expect("the keys page");

    update(
        &mut model,
        Msg::Keymap {
            result: Ok(keymap_from("[normal]\nZ = \"message.archive\"\n")),
            announce: true,
        },
    );
    let after = manual_doc(&model).expect("the keys page");
    assert_ne!(before, after, "the page did not follow the reload");
    let text: String = after
        .lines
        .iter()
        .map(manual::DocLine::text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains('Z'), "{text}");
}

#[test]
fn opening_the_manual_at_a_named_page_goes_straight_there() {
    // The seam tasks 89 and 102 use: `:manual <page>` and `K` on a key
    // reference row both need "open it *there*", which an action's own
    // signature cannot carry.
    let mut model = loaded();
    let cmds = open_manual_at(&mut model, "commands");
    assert!(cmds.is_empty());
    assert_eq!(
        manual(&model).at,
        manual::Location::Page("commands".to_owned())
    );
}

#[test]
fn opening_the_manual_at_an_action_id_lands_on_the_page_documenting_it() {
    // Task 102's `K` on a key-reference row has an `Action::id` and no
    // anchor, and task 89's `:manual message.archive` is the same string
    // typed by hand — so this resolves through the manual's declared
    // action-to-page mapping rather than through a second one derived here.
    let mut model = loaded();
    let cmds = open_manual_at(&mut model, "message.archive");
    assert!(cmds.is_empty());
    assert_eq!(
        manual(&model).at,
        manual::Location::Page("archive".to_owned())
    );
}

#[test]
fn opening_the_manual_at_a_verb_path_is_the_same_as_at_its_action_id() {
    // Dots and spaces are one separator everywhere else in this vocabulary;
    // a page name is the one place they could have stopped being.
    let mut model = loaded();
    open_manual_at(&mut model, "message archive");
    assert_eq!(
        manual(&model).at,
        manual::Location::Page("archive".to_owned())
    );
}

#[test]
fn a_name_that_is_both_a_page_and_an_action_opens_that_page() {
    // `manual` is the one string that is both. This pins the *outcome* and
    // says so: it cannot distinguish anchor-first from id-first, because the
    // two agree — `manual::tests`'
    // `a_page_anchor_that_is_also_a_documented_id_resolves_to_its_own_page`
    // is the check that keeps them agreeing, and it is where a future
    // collision fails.
    let mut model = loaded();
    open_manual_at(&mut model, "manual");
    assert_eq!(
        manual(&model).at,
        manual::Location::Page("manual".to_owned())
    );
}

#[test]
fn opening_the_manual_at_a_page_that_does_not_exist_is_refused_by_name() {
    let mut model = loaded();
    let cmds = open_manual_at(&mut model, "nowhere");
    assert!(cmds.is_empty());
    assert_eq!(model.screen, Screen::List, "and nothing was opened");
    assert!(model.manual.is_none());
    assert_eq!(model.level, Level::Error);
    assert!(model.status.contains("nowhere"), "{}", model.status);
}

#[test]
fn opening_the_manual_over_a_streaming_overlay_stops_the_stream() {
    // The manual is a screen; an overlay left up would cover it, and an
    // abandoned `AskMailbox` left running would be billed for an answer
    // nobody will read — `leave`'s rule, applied on this path too.
    let mut model = loaded();
    press(&mut model, Key::Char('A'));
    keys(&mut model, "what happened");
    press(&mut model, Key::Enter);
    assert_eq!(model.mode(), Mode::Menu, "the answer is streaming");

    // `manual` is not bound in the menu layer — the ask pane's own keys are —
    // so this is the rebind case, which is also the shape task 89's `:manual`
    // will take: an action reaching this path from a mode that has an overlay
    // up.
    model.keymap = keymap_from("[menu]\nK = \"manual\"\n");
    let cmds = press(&mut model, Key::Char('K'));
    assert_eq!(model.screen, Screen::Manual);
    assert!(model.overlay.is_none());
    assert!(
        cmds.contains(&Cmd::CancelStream { which: Stream::Ask }),
        "{cmds:?}"
    );
}

#[test]
fn a_jump_restores_what_was_highlighted_on_the_page_it_returns_to() {
    // Not just the cursor: grep a phrase, open a hit, follow a link out of it
    // and come back, and the hit you came for has to still be lit up.
    let mut model = manual_open();
    keys(&mut model, "g/");
    keys(&mut model, "Modes and layers");
    press(&mut model, Key::Enter);
    walk_to_a_link(&mut model);
    press(&mut model, Key::Enter);
    assert_eq!(
        manual(&model).highlight.as_deref(),
        Some("Modes and layers")
    );

    // Out to another page, which clears it, then back.
    enter_manual(
        &mut model,
        manual::Location::Page("capabilities".to_owned()),
    );
    assert_eq!(manual(&model).highlight, None);
    press(&mut model, Key::ctrl('o'));
    assert_eq!(
        manual(&model).highlight.as_deref(),
        Some("Modes and layers"),
        "the jump list restores the page's state, not only its cursor"
    );
    press(&mut model, Key::ctrl('i'));
    assert_eq!(manual(&model).highlight, None, "and forward again");
}

#[test]
fn n_and_the_jump_keys_are_silent_when_the_manual_is_not_open() {
    // `Mode::Help` is the `?` overlay's layer as well as the manual's, so
    // every binding added for the manual has to be inert there. `n` was not:
    // it painted a red "nothing searched for yet — / searches this page" over
    // the status line, about a page the reader is not on.
    for key in [
        Key::Char('n'),
        Key::Char('N'),
        Key::ctrl('o'),
        Key::ctrl('i'),
        Key::Tab,
        Key::Char('j'),
        Key::Char('k'),
        Key::Char('/'),
    ] {
        let mut model = loaded();
        press(&mut model, Key::Char('?'));
        model.info("2 message(s)");

        let cmds = press(&mut model, key);
        assert!(cmds.is_empty(), "{key:?} issued work: {cmds:?}");
        assert_eq!(model.overlay, Some(Overlay::Help), "{key:?} closed it");
        assert_eq!(
            (model.level, model.status.as_str()),
            (Level::Info, "2 message(s)"),
            "{key:?} wrote to the status line"
        );
        assert_eq!(model.message_idx, 0, "{key:?} moved the list behind it");
    }
}

#[test]
fn helpgrep_with_its_pattern_supplied_goes_straight_to_the_hits() {
    // The consumer of the `pattern` positional the verb declares — an action
    // cannot carry a string, so this is the seam task 89 dispatches through.
    let mut model = loaded();
    let cmds = open_manual_grep_for(&mut model, "  Command index  ");
    assert!(cmds.is_empty());
    assert_eq!(
        manual(&model).at,
        manual::Location::Grep("Command index".to_owned()),
        "the pattern is trimmed and used, not dropped"
    );
    // And `<c-o>` still has somewhere to go, so the hit list is not a dead end.
    assert!(manual(&model).can_jump_back());
}

#[test]
fn a_bare_helpgrep_raises_the_prompt_rather_than_listing_nothing() {
    for pattern in ["", "   "] {
        let mut model = loaded();
        open_manual_grep_for(&mut model, pattern);
        assert_eq!(model.screen, Screen::Manual);
        assert!(manual(&model).typing(), "{pattern:?}");
        assert_eq!(
            manual(&model).prompt.as_ref().map(|prompt| prompt.scope),
            Some(Scope::Manual)
        );
    }
}

#[test]
fn helpgrep_with_a_pattern_closes_an_overlay_it_was_dispatched_from() {
    // Task 89's command line is an overlay, and it will be up when it
    // dispatches. A hit list drawn behind it would be a hit list nobody sees.
    let mut model = loaded();
    press(&mut model, Key::Char('A'));
    keys(&mut model, "what happened");
    press(&mut model, Key::Enter);

    let cmds = open_manual_grep_for(&mut model, "Key reference");
    assert!(model.overlay.is_none());
    assert!(cmds.contains(&Cmd::CancelStream { which: Stream::Ask }));
    assert_eq!(
        manual(&model).at,
        manual::Location::Grep("Key reference".to_owned())
    );
}

// ---------------------------------------------------------------------------
// the `:` command line (task 89)
// ---------------------------------------------------------------------------

fn command_pane(model: &Model) -> &CommandPane {
    match model.overlay.as_ref() {
        Some(Overlay::Command(pane)) => pane,
        other => panic!("expected the command overlay, found {other:?}"),
    }
}

/// Open the command line and type `line` into it.
fn command(model: &mut Model, line: &str) -> Vec<Cmd> {
    press(model, Key::Char(':'));
    keys(model, line)
}

#[test]
fn colon_opens_the_command_line_in_prompt_mode() {
    let mut model = loaded();
    press(&mut model, Key::Char(':'));
    assert!(matches!(model.overlay, Some(Overlay::Command(_))));
    assert_eq!(
        model.mode(),
        Mode::Prompt,
        "keys are text here, so a `d` types rather than deletes"
    );
    // The proof that it is text: `d` is `message.delete` in Normal.
    keys(&mut model, "d");
    assert_eq!(command_pane(&model).input, "d");
}

#[test]
fn ctrl_k_opens_the_same_overlay_as_colon() {
    // `Action::PaletteOpen` is kept as a documented alias — renaming an
    // action id breaks a `keys.toml` somebody has already written.
    let mut model = loaded();
    press(&mut model, Key::ctrl('k'));
    assert!(matches!(model.overlay, Some(Overlay::Command(_))));
}

#[test]
fn a_verb_with_no_arguments_delegates_straight_to_run_action() {
    // The whole point of the task: the 39 behaviours the keyboard reaches
    // keep exactly one implementation. `help` opens the same overlay whether
    // it arrives as `?` or as `:help`.
    let mut by_key = loaded();
    press(&mut by_key, Key::Char('?'));

    let mut by_line = loaded();
    command(&mut by_line, "help");
    press(&mut by_line, Key::Enter);

    assert!(matches!(by_key.overlay, Some(Overlay::Help)));
    assert_eq!(by_line.overlay, by_key.overlay);
    assert_eq!(by_line.screen, by_key.screen);
}

#[test]
fn a_dotted_verb_and_a_spaced_one_are_the_same_line() {
    for line in ["message.archive", "message archive"] {
        let mut model = loaded();
        let cmds = command(&mut model, line);
        let cmds = [cmds, press(&mut model, Key::Enter)].concat();
        assert!(
            cmds.iter().any(|cmd| matches!(cmd, Cmd::Move { .. })),
            "{line:?} issued {cmds:?}"
        );
    }
}

#[test]
fn a_parse_error_keeps_the_overlay_open_with_the_text_still_there() {
    let mut model = loaded();
    command(&mut model, "message copy \"unterminated");
    assert!(press(&mut model, Key::Enter).is_empty());
    let pane = command_pane(&model);
    assert!(
        pane.error
            .as_deref()
            .is_some_and(|why| why.contains("unterminated quote")),
        "{:?}",
        pane.error
    );
    assert_eq!(
        pane.input, "message copy \"unterminated",
        "the offending text is still there to fix"
    );
}

#[test]
fn a_parse_error_clears_as_soon_as_the_line_is_edited() {
    let mut model = loaded();
    command(&mut model, "message copy \"unterminated");
    press(&mut model, Key::Enter);
    assert!(command_pane(&model).error.is_some());
    press(&mut model, Key::Backspace);
    assert!(
        command_pane(&model).error.is_none(),
        "an error about text that is being fixed is an error about text that \
         is no longer there"
    );
}

#[test]
fn an_interior_node_names_its_children_rather_than_failing() {
    // `manual` is a real verb *and* has children, so it resolves rather than
    // reporting them — which is why the case below uses `cursor`, a path no
    // verb sits at.
    let mut model = loaded();
    command(&mut model, "cursor");
    press(&mut model, Key::Enter);
    let why = command_pane(&model).error.clone().unwrap_or_default();
    assert!(why.contains("needs one of"), "{why}");
    assert!(why.contains("down"), "{why}");
    assert!(why.contains("up"), "{why}");
}

#[test]
fn a_verb_given_an_argument_it_does_not_take_is_refused_by_name() {
    let mut model = loaded();
    command(&mut model, "message archive now");
    press(&mut model, Key::Enter);
    let why = command_pane(&model).error.clone().unwrap_or_default();
    assert!(why.contains("takes no arguments"), "{why}");
    assert!(why.contains("now"), "{why}");
}

#[test]
fn opening_the_command_line_over_a_selection_prefills_the_range() {
    let mut model = loaded();
    press(&mut model, Key::Char('v'));
    assert!(model.visual.is_some());
    press(&mut model, Key::Char(':'));
    assert_eq!(command_pane(&model).input, "'<,'>");
}

#[test]
fn without_a_selection_the_command_line_opens_empty() {
    let mut model = loaded();
    press(&mut model, Key::Char(':'));
    assert_eq!(command_pane(&model).input, "");
}

#[test]
fn the_selection_range_acts_on_the_whole_selection() {
    let mut model = loaded();
    press(&mut model, Key::Char('v'));
    press(&mut model, Key::Char('j'));
    press(&mut model, Key::Char(':'));
    keys(&mut model, "message archive");
    let cmds = press(&mut model, Key::Enter);
    let moves = cmds
        .iter()
        .filter(|cmd| matches!(cmd, Cmd::Move { .. }))
        .count();
    assert_eq!(moves, 2, "both selected rows: {cmds:?}");
}

#[test]
fn a_selection_range_with_no_selection_is_refused_rather_than_narrowed() {
    let mut model = loaded();
    command(&mut model, "'<,'>message archive");
    let cmds = press(&mut model, Key::Enter);
    // The line parsed, so it is recorded — see `record_command`. Nothing
    // that touches mail is issued, which is the property under test.
    assert!(
        cmds.iter()
            .all(|cmd| matches!(cmd, Cmd::SaveHistory { .. })),
        "{cmds:?}"
    );
    let why = command_pane(&model).error.clone().unwrap_or_default();
    assert!(why.contains("visual selection"), "{why}");
}

#[test]
fn the_ranges_with_no_model_support_say_so_rather_than_acting_on_one_row() {
    // Acting on the row under the cursor when `%` was typed would be a range
    // that looked honoured and was not — the worst of the three answers.
    for (line, expected) in [("%message archive", "%"), ("20message archive", "count")] {
        let mut model = loaded();
        command(&mut model, line);
        let cmds = press(&mut model, Key::Enter);
        assert!(
            cmds.iter()
                .all(|cmd| matches!(cmd, Cmd::SaveHistory { .. })),
            "{line:?} issued {cmds:?}"
        );
        let why = command_pane(&model).error.clone().unwrap_or_default();
        assert!(why.contains(expected), "{line:?}: {why}");
    }
}

#[test]
fn a_bang_skips_the_confirmation_and_only_that() {
    let mut without = loaded();
    command(&mut without, "message delete");
    press(&mut without, Key::Enter);
    assert!(
        matches!(without.overlay, Some(Overlay::Confirm { .. })),
        "delete asks first"
    );

    let mut with = loaded();
    command(&mut with, "message delete!");
    let cmds = press(&mut with, Key::Enter);
    assert!(with.overlay.is_none(), "no question was asked");
    assert!(
        cmds.iter().any(|cmd| matches!(cmd, Cmd::Delete { .. })),
        "and the delete went out: {cmds:?}"
    );
}

#[test]
fn helpgrep_carries_its_pattern_into_the_manual() {
    // The one argument-carrying verb `run_action` cannot reach: its
    // signature takes a count, not a string.
    let mut model = loaded();
    command(&mut model, "helpgrep archive");
    press(&mut model, Key::Enter);
    assert_eq!(model.screen, Screen::Manual);
    assert_eq!(
        manual(&model).at,
        manual::Location::Grep("archive".to_owned())
    );
}

#[test]
fn manual_grep_is_the_same_verb_by_its_other_spelling() {
    let mut model = loaded();
    command(&mut model, "manual grep archive");
    press(&mut model, Key::Enter);
    assert_eq!(
        manual(&model).at,
        manual::Location::Grep("archive".to_owned())
    );
}

#[test]
fn a_bare_helpgrep_typed_on_the_command_line_raises_the_prompt() {
    let mut model = loaded();
    command(&mut model, "helpgrep");
    press(&mut model, Key::Enter);
    assert_eq!(model.screen, Screen::Manual);
    assert!(manual(&model).typing(), "the prompt is up");
}

#[test]
fn manual_opens_the_front_page_and_takes_no_page_name() {
    let mut bare = loaded();
    command(&mut bare, "manual");
    press(&mut bare, Key::Enter);
    assert_eq!(manual(&bare).at, manual::Location::start());

    // Deliberately refused rather than accepted: `manual` cannot declare a
    // positional without shadowing `manual grep` — see `command::explicit`'s
    // own docs — and accepting an argument the grammar never mentions is
    // exactly what that guard exists to prevent. `open_manual_at` is still
    // the seam for a page name; task 102's `K` is what calls it.
    let mut named = loaded();
    command(&mut named, "manual archive");
    press(&mut named, Key::Enter);
    let why = command_pane(&named).error.clone().unwrap_or_default();
    assert!(why.contains("takes no arguments"), "{why}");
}

#[test]
fn an_unquoted_multi_word_pattern_reaches_helpgrep_whole() {
    // Searching only the first word and dropping the rest would be a silent
    // truncation of what was typed.
    let mut model = loaded();
    command(&mut model, "helpgrep undo window");
    press(&mut model, Key::Enter);
    assert_eq!(
        manual(&model).at,
        manual::Location::Grep("undo window".to_owned())
    );
}

// --- completion ------------------------------------------------------------

#[test]
fn tab_completes_to_what_the_registry_can_say_for_certain() {
    let mut model = loaded();
    command(&mut model, "message arc");
    press(&mut model, Key::Tab);
    assert_eq!(command_pane(&model).input, "message archive ");
}

#[test]
fn tab_stops_at_the_shared_prefix_when_more_than_one_verb_matches() {
    // `help` and `helpgrep` are different verbs sharing a prefix, so
    // completing to either would be a keystroke that did the wrong thing
    // rather than one that did nothing. This is the case that goes through
    // `longest_common_prefix`; a single-candidate line does not.
    let mut model = loaded();
    command(&mut model, "he");
    press(&mut model, Key::Tab);
    assert_eq!(
        command_pane(&model).input,
        "help",
        "the shared prefix, and no further"
    );
}

#[test]
fn tab_opens_a_segment_that_is_already_typed_in_full() {
    // Without the separator, Tab stalls on `message` — the completer answers
    // `message` again, which is not longer than what is already there.
    let mut model = loaded();
    command(&mut model, "mess");
    press(&mut model, Key::Tab);
    assert_eq!(command_pane(&model).input, "message ");
}

#[test]
fn tab_works_on_a_line_that_opened_with_a_range() {
    // The state the command line is documented to open in. The range is a
    // separator, not part of the first word — counting it as typed text made
    // Tab dead here for every first segment.
    let mut model = loaded();
    press(&mut model, Key::Char('v'));
    press(&mut model, Key::Char(':'));
    keys(&mut model, "message arc");
    press(&mut model, Key::Tab);
    assert_eq!(command_pane(&model).input, "'<,'>message archive ");
}

#[test]
fn tab_on_a_line_ending_in_a_flag_leaves_it_alone() {
    // The registry's completer drops flags before it looks at anything, so it
    // answers about the verb — and substituting that over the flag is how
    // `:search --x` once became `search search`.
    let mut model = loaded();
    command(&mut model, "search --x");
    press(&mut model, Key::Tab);
    assert_eq!(command_pane(&model).input, "search --x");
}

#[test]
fn tab_with_nothing_to_add_leaves_the_line_alone() {
    let mut model = loaded();
    command(&mut model, "zzzznope");
    press(&mut model, Key::Tab);
    assert_eq!(command_pane(&model).input, "zzzznope");
}

// --- history ---------------------------------------------------------------

#[test]
fn a_run_line_is_recorded_and_written() {
    let mut model = loaded();
    command(&mut model, "help");
    let cmds = press(&mut model, Key::Enter);
    assert_eq!(model.history.entries(), ["help"]);
    let saved = cmds.iter().find_map(|cmd| match cmd {
        Cmd::SaveHistory { entries } => Some(entries.clone()),
        _ => None,
    });
    assert_eq!(saved.as_deref(), Some(&["help".to_owned()][..]));
}

#[test]
fn a_line_that_did_not_parse_is_not_recorded() {
    let mut model = loaded();
    command(&mut model, "message copy \"unterminated");
    let cmds = press(&mut model, Key::Enter);
    assert!(model.history.entries().is_empty());
    assert!(
        !cmds
            .iter()
            .any(|cmd| matches!(cmd, Cmd::SaveHistory { .. })),
        "and nothing was written: {cmds:?}"
    );
}

#[test]
fn a_secret_line_is_never_recorded_and_never_reaches_a_write() {
    // Driven through `record_command`, not through `<enter>`: `token` has no
    // TUI verb today, so that line never parses, and a test that typed it
    // would pass with the redaction rule deleted. `record_command` is the
    // seam every recorded line goes through, so this is where the rule can be
    // made to fail.
    let mut model = loaded();
    press(&mut model, Key::Char(':'));
    for line in [
        "token create --name claude",
        "account login --client-id abc 1",
        "webhook add --secret-env WH x",
        "'<,'>token create",
    ] {
        record_command(&mut model, line);
        assert!(
            model.history.entries().is_empty(),
            "{line:?} was recorded: {:?}",
            model.history.entries()
        );
        assert!(!model.pending_history, "{line:?} asked for a write");
    }
    // ...and an ordinary line does reach both, so the assertions above are
    // about the rule rather than about `record_command` doing nothing.
    record_command(&mut model, "message archive");
    assert_eq!(model.history.entries(), ["message archive"]);
    assert!(model.pending_history);
}

#[test]
fn up_and_down_walk_the_history() {
    let mut model = loaded();
    model.history = History::new(vec!["help".to_owned(), "manual".to_owned()]);
    press(&mut model, Key::Char(':'));

    press(&mut model, Key::Up);
    assert_eq!(command_pane(&model).input, "manual", "newest first");
    press(&mut model, Key::Up);
    assert_eq!(command_pane(&model).input, "help");
    press(&mut model, Key::Up);
    assert_eq!(
        command_pane(&model).input,
        "help",
        "and stops at the oldest"
    );

    press(&mut model, Key::Down);
    assert_eq!(command_pane(&model).input, "manual");
    press(&mut model, Key::Down);
    assert_eq!(
        command_pane(&model).input,
        "",
        "back past the newest is the line as it was typed"
    );
}

#[test]
fn the_history_walk_is_filtered_by_what_was_already_typed() {
    let mut model = loaded();
    model.history = History::new(vec![
        "message archive".to_owned(),
        "help".to_owned(),
        "message move".to_owned(),
    ]);
    press(&mut model, Key::Char(':'));
    keys(&mut model, "mess");
    press(&mut model, Key::Up);
    assert_eq!(command_pane(&model).input, "message move");
    press(&mut model, Key::Up);
    assert_eq!(
        command_pane(&model).input,
        "message archive",
        "help skipped"
    );
    press(&mut model, Key::Down);
    press(&mut model, Key::Down);
    assert_eq!(
        command_pane(&model).input,
        "mess",
        "and the seed comes back, not an empty line"
    );
}

#[test]
fn typing_after_a_history_walk_starts_a_new_one() {
    let mut model = loaded();
    model.history = History::new(vec!["help".to_owned()]);
    press(&mut model, Key::Char(':'));
    press(&mut model, Key::Up);
    assert_eq!(command_pane(&model).input, "help");
    press(&mut model, Key::Backspace);
    assert_eq!(command_pane(&model).input, "hel");
    assert!(
        command_pane(&model).browse.is_none(),
        "the line is the typist's again"
    );
}

#[test]
fn up_and_down_still_move_a_cursor_everywhere_else() {
    // The history walk is the command line's alone; `cursor.up` has to keep
    // meaning what it means in every other overlay.
    let mut model = loaded();
    press(&mut model, Key::Char('j'));
    assert_eq!(model.message_idx, 1);
    press(&mut model, Key::Up);
    assert_eq!(model.message_idx, 0);
}

#[test]
fn the_fallback_carries_the_range_and_the_bang_the_line_had() {
    // `:'<,'>arch` and `:del!` mean what they look like they mean: the
    // fallback runs the ranked verb, not a bare version of it.
    let mut ranged = loaded();
    press(&mut ranged, Key::Char('v'));
    press(&mut ranged, Key::Char('j'));
    press(&mut ranged, Key::Char(':'));
    keys(&mut ranged, "message arch");
    let cmds = press(&mut ranged, Key::Enter);
    assert_eq!(
        cmds.iter()
            .filter(|cmd| matches!(cmd, Cmd::Move { .. }))
            .count(),
        2,
        "the range survived the fallback: {cmds:?}"
    );

    let mut banged = loaded();
    command(&mut banged, "message del!");
    let cmds = press(&mut banged, Key::Enter);
    assert!(banged.overlay.is_none(), "the bang survived the fallback");
    assert!(cmds.iter().any(|cmd| matches!(cmd, Cmd::Delete { .. })));
}

#[test]
fn an_empty_line_asks_for_a_verb_rather_than_running_the_first_row() {
    // The list is every verb there is, in path order; running whichever
    // sorts first would be a bare Enter doing something nobody named.
    let mut model = loaded();
    press(&mut model, Key::Char(':'));
    let pane = command_pane(&model);
    assert!(!pane.matches.is_empty());
    assert!(!pane.fallback_is_live(), "so no row is marked");
    let cmds = press(&mut model, Key::Enter);
    assert!(cmds.is_empty(), "{cmds:?}");
    let why = command_pane(&model).error.clone().unwrap_or_default();
    assert!(why.contains("needs a verb"), "{why}");
}

#[test]
fn a_line_refused_after_parsing_is_still_recorded() {
    // It parsed, so it is exactly the line somebody wants `<up>` to bring
    // back and fix — unlike one that never left the overlay.
    let mut model = loaded();
    command(&mut model, "%message archive");
    press(&mut model, Key::Enter);
    assert!(command_pane(&model).error.is_some());
    assert_eq!(model.history.entries(), ["%message archive"]);
}

#[test]
fn colon_from_a_list_overlay_replaces_it_and_stops_what_it_was_streaming() {
    // `:` is bound in `Menu`, and a binding that does nothing in the layer it
    // was added to is not a binding.
    let mut model = loaded();
    let cmds = {
        press(&mut model, Key::Char('/'));
        keys(&mut model, "invoice")
    };
    // The *last* one: every keystroke issues a search, and a frame from a
    // superseded generation is dropped on arrival.
    let generation = cmds
        .iter()
        .rev()
        .find_map(|cmd| match cmd {
            Cmd::Search { generation, .. } => Some(*generation),
            _ => None,
        })
        .expect("the query issued a search");
    // A hit has to land before `<enter>` will leave the query line — an
    // empty result list has nothing to walk.
    update(
        &mut model,
        Msg::Search {
            generation,
            event: SearchEvent::Hit(Box::new(Hit {
                message_id: 10,
                subject: "Quarterly invoice".to_owned(),
                from: "Alice".to_owned(),
                date: None,
                snippet: "your invoice".to_owned(),
                highlights: Vec::new(),
                sources: vec!["lexical".to_owned()],
            })),
        },
    );
    press(&mut model, Key::Enter);
    assert_eq!(model.mode(), Mode::Menu, "the results are up");

    let cmds = press(&mut model, Key::Char(':'));
    assert!(matches!(model.overlay, Some(Overlay::Command(_))));
    assert!(
        cmds.iter().any(|cmd| matches!(
            cmd,
            Cmd::CancelStream {
                which: Stream::Search
            }
        )),
        "the search it replaced was cancelled: {cmds:?}"
    );
}

#[test]
fn opening_the_command_line_over_a_modal_that_is_not_a_list_is_refused() {
    // A folder picker or a confirmation owns the keyboard: replacing one with
    // a command line would answer a question nobody answered.
    //
    // Driven through `run_action` rather than by pressing `:`, and that is
    // the point: `:` is bound in `Normal` and `Menu` only, so a keypress can
    // never reach `open_command` with a `Confirm` up, and a test that pressed
    // it would be green for *any* body of `open_command` — including one that
    // replaced the overlay unconditionally. This exercises the refusal
    // itself, which is what a `keys.toml` binding `:` in `Confirm` reaches.
    for opener in [Action::CommandOpen, Action::PaletteOpen] {
        let mut model = loaded();
        press(&mut model, Key::Char('d'));
        assert!(matches!(model.overlay, Some(Overlay::Confirm { .. })));
        assert!(run_action(&mut model, opener, None).is_empty());
        assert!(
            matches!(model.overlay, Some(Overlay::Confirm { .. })),
            "{opener:?} replaced the question"
        );

        let mut model = loaded();
        press(&mut model, Key::Char('c'));
        assert!(matches!(model.overlay, Some(Overlay::Pick { .. })));
        run_action(&mut model, opener, None);
        assert!(
            matches!(model.overlay, Some(Overlay::Pick { .. })),
            "{opener:?} replaced the picker"
        );
    }
}

#[test]
fn a_selection_that_outlived_the_list_does_not_prefill_a_range() {
    // The anchor survives leaving the list, but `Model::selection` does not —
    // so a prefilled `'<,'>` in the viewer would be a range nothing could
    // honour, and the verb would quietly act on one message instead.
    let mut model = loaded();
    press(&mut model, Key::Char('v'));
    press(&mut model, Key::Char('j'));
    // Through `open_message_by_id`, which is how a selection reaches the
    // viewer at all: `<enter>` on the list refuses while one is up, and a
    // search hit is the path task 103's note describes.
    open_message_by_id(&mut model, 10);
    update(
        &mut model,
        Msg::Opened {
            message_id: 10,
            result: Ok(OpenMessage {
                id: 10,
                ..OpenMessage::default()
            }),
        },
    );
    assert_eq!(model.screen, Screen::Viewer);
    assert!(model.visual.is_some(), "the anchor outlived the list");
    assert!(!model.is_selecting(), "but the range did not");

    press(&mut model, Key::Char(':'));
    assert_eq!(command_pane(&model).input, "", "so nothing is prefilled");
}

#[test]
fn a_range_typed_in_the_viewer_is_refused_rather_than_narrowed_to_one() {
    let mut model = loaded();
    press(&mut model, Key::Char('v'));
    open_message_by_id(&mut model, 10);
    update(
        &mut model,
        Msg::Opened {
            message_id: 10,
            result: Ok(OpenMessage {
                id: 10,
                ..OpenMessage::default()
            }),
        },
    );
    assert_eq!(model.screen, Screen::Viewer);
    // Typed by hand: nothing prefills it here, which is the previous test.
    command(&mut model, "'<,'>message archive");
    let cmds = press(&mut model, Key::Enter);
    assert!(
        cmds.iter()
            .all(|cmd| matches!(cmd, Cmd::SaveHistory { .. })),
        "nothing was archived: {cmds:?}"
    );
    let why = command_pane(&model).error.clone().unwrap_or_default();
    assert!(why.contains("message list"), "{why}");
}

#[test]
fn a_range_on_a_verb_that_acts_on_no_message_is_refused() {
    // A range names a set of messages, and `help` acts on none — accepting
    // one and ignoring it is the same "looked honoured and was not" the `%`
    // refusal exists to avoid.
    let mut model = loaded();
    press(&mut model, Key::Char('v'));
    press(&mut model, Key::Char(':'));
    keys(&mut model, "help");
    press(&mut model, Key::Enter);
    assert!(model.overlay.is_some(), "the command line is still up");
    let why = command_pane(&model).error.clone().unwrap_or_default();
    assert!(why.contains("does not act on one"), "{why}");

    // ...while a verb that does act on mail takes the same range.
    let mut model = loaded();
    press(&mut model, Key::Char('v'));
    press(&mut model, Key::Char(':'));
    keys(&mut model, "message archive");
    let cmds = press(&mut model, Key::Enter);
    assert!(cmds.iter().any(|cmd| matches!(cmd, Cmd::Move { .. })));
}

#[test]
fn the_fallback_refuses_a_line_carrying_a_flag() {
    // `:message archive --force` is refused by the parser. `:arch --force`
    // must be refused too, or the abbreviation is less strict than the
    // spelling — the flag would be dropped and the archive would happen.
    let mut model = loaded();
    command(&mut model, "arch --force");
    let cmds = press(&mut model, Key::Enter);
    assert!(
        !cmds.iter().any(|cmd| matches!(cmd, Cmd::Move { .. })),
        "{cmds:?}"
    );
    let why = command_pane(&model).error.clone().unwrap_or_default();
    assert!(why.contains("flags cannot be guessed"), "{why}");
    assert!(
        model.history.entries().is_empty(),
        "and it was not recorded"
    );
}

// ---------------------------------------------------------------------------
// `:set` (task 93)
// ---------------------------------------------------------------------------

#[test]
fn set_updates_a_pane_width_and_closes_the_overlay() {
    let mut model = loaded();
    command(&mut model, "set folder-width 25");
    press(&mut model, Key::Enter);
    assert_eq!(model.folder_width_pct, 25);
    assert!(model.overlay.is_none(), "a successful :set closes the line");
    assert_eq!(model.status, "folder-width set to 25");
}

#[test]
fn set_updates_the_preview_width_and_the_ai_panel_width_independently() {
    let mut model = loaded();
    command(&mut model, "set preview-width 35");
    press(&mut model, Key::Enter);
    command(&mut model, "set ai-panel-width 45");
    press(&mut model, Key::Enter);
    assert_eq!(model.preview_width_pct, 35);
    assert_eq!(model.ai_panel_width_pct, 45);
    // Untouched by either write.
    assert_eq!(model.folder_width_pct, 20);
}

#[test]
fn set_rejects_a_non_numeric_value_and_leaves_the_line_open() {
    let mut model = loaded();
    command(&mut model, "set folder-width abc");
    press(&mut model, Key::Enter);
    assert!(matches!(model.overlay, Some(Overlay::Command(_))));
    let why = command_pane(&model).error.clone().unwrap_or_default();
    assert!(why.contains("not a whole number"), "{why}");
    assert_eq!(model.folder_width_pct, 20, "the bad value never lands");
}

#[test]
fn set_rejects_a_pane_width_outside_its_range() {
    let mut model = loaded();
    command(&mut model, "set folder-width 5");
    press(&mut model, Key::Enter);
    let why = command_pane(&model).error.clone().unwrap_or_default();
    assert!(why.contains("folder-width must be"), "{why}");
    assert_eq!(model.folder_width_pct, 20);
}

#[test]
fn set_rejects_folder_and_preview_widths_that_would_crowd_out_the_message_list() {
    // Defaults are 20/40; 55 + 40 = 95 leaves the message list 5% — below the
    // 10% `render_panes` is entitled to assume.
    let mut model = loaded();
    command(&mut model, "set folder-width 55");
    press(&mut model, Key::Enter);
    let why = command_pane(&model).error.clone().unwrap_or_default();
    assert!(why.contains("message list needs the rest"), "{why}");
    assert_eq!(model.folder_width_pct, 20, "rejected together, not clamped");
}

#[test]
fn set_rejects_preview_and_folder_widths_that_would_crowd_out_the_message_list() {
    // The other direction of the combined cap: preview-width is the one
    // that trips it this time. No single preview-width is both in its own
    // 10-60 range and, combined with the *default* 20 folder-width, over
    // the 90 cap (20 + 60 = 80) — so folder-width is widened to 40 first,
    // a valid `:set` in its own right, before 55 (also valid alone) pushes
    // the combined total to 95.
    let mut model = loaded();
    command(&mut model, "set folder-width 40");
    press(&mut model, Key::Enter);
    assert_eq!(
        model.folder_width_pct, 40,
        "the setup step itself must land"
    );
    command(&mut model, "set preview-width 55");
    press(&mut model, Key::Enter);
    let why = command_pane(&model).error.clone().unwrap_or_default();
    assert!(why.contains("message list needs the rest"), "{why}");
    assert_eq!(
        model.preview_width_pct, 40,
        "rejected together, not clamped"
    );
}

#[test]
fn set_rejects_an_ai_panel_width_outside_its_range() {
    for value in ["5", "99"] {
        let mut model = loaded();
        command(&mut model, &format!("set ai-panel-width {value}"));
        press(&mut model, Key::Enter);
        let why = command_pane(&model).error.clone().unwrap_or_default();
        assert!(why.contains("ai-panel-width must be"), "{value}: {why}");
        assert_eq!(
            model.ai_panel_width_pct, 30,
            "{value}: rejected, not clamped"
        );
    }
}

#[test]
fn set_rejects_an_unknown_option() {
    let mut model = loaded();
    command(&mut model, "set bogus 10");
    press(&mut model, Key::Enter);
    let why = command_pane(&model).error.clone().unwrap_or_default();
    assert!(why.contains("unknown option"), "{why}");
}

#[test]
fn set_names_the_option_not_the_value_when_both_are_wrong() {
    // The option is checked first: `bogus` is not a real option regardless
    // of what its value looks like, so the error must say that rather than
    // implying `bogus` were real and only "abc" were the problem.
    let mut model = loaded();
    command(&mut model, "set bogus abc");
    press(&mut model, Key::Enter);
    let why = command_pane(&model).error.clone().unwrap_or_default();
    assert!(why.contains("unknown option"), "{why}");
    assert!(!why.contains("whole number"), "{why}");
}

#[test]
fn set_with_a_missing_value_is_refused_cleanly_not_a_panic() {
    // Both positionals parse as optional (bare `set` must still resolve to
    // itself, like every other real verb), so "only one argument given" is
    // `set_option`'s job to catch, not the parser's.
    let mut model = loaded();
    command(&mut model, "set folder-width");
    press(&mut model, Key::Enter);
    assert!(matches!(model.overlay, Some(Overlay::Command(_))));
    assert!(
        command_pane(&model).error.is_some(),
        "a half-given set must surface as an error, not silently no-op"
    );
}

// ---------------------------------------------------------------------------
// the toast queue's cap (task 93)
// ---------------------------------------------------------------------------

#[test]
fn push_toast_caps_the_queue_and_drops_the_oldest_first() {
    let mut model = loaded();
    for n in 0..MAX_TOASTS {
        push_toast(
            &mut model,
            Toast::Completion {
                text: format!("job {n}"),
            },
        );
    }
    assert_eq!(model.toasts.len(), MAX_TOASTS);
    push_toast(
        &mut model,
        Toast::Completion {
            text: "job overflow".to_owned(),
        },
    );
    assert_eq!(model.toasts.len(), MAX_TOASTS, "capped, not grown");
    assert!(
        !model
            .toasts
            .iter()
            .any(|toast| matches!(toast, Toast::Completion { text } if text == "job 0")),
        "the oldest was dropped: {:?}",
        model.toasts
    );
    assert!(
        model
            .toasts
            .iter()
            .any(|toast| matches!(toast, Toast::Completion { text } if text == "job overflow")),
        "the new one landed"
    );
}

#[test]
fn push_toast_never_evicts_the_undo_toast() {
    // An active undo countdown must survive a flood of other toasts, or `u`
    // silently stops being able to cancel a send still inside its window.
    let mut model = loaded();
    set_undo_toast(
        &mut model,
        UndoToast {
            outbox_id: 1,
            to: "bob@example.com".to_owned(),
            deadline: 1_030,
            remaining: 30,
        },
    );
    for n in 0..MAX_TOASTS {
        push_toast(
            &mut model,
            Toast::Priority {
                text: format!("alert {n}"),
            },
        );
    }
    assert_eq!(model.toasts.len(), MAX_TOASTS, "still capped");
    assert!(
        undo_toast(&model).is_some(),
        "a flood of other toasts must not evict an active undo offer"
    );
}
