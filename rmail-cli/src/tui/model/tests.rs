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
            Cmd::Watch { account_id: 7 }
        ],
        "the event stream starts as soon as there is an account to watch"
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
            Cmd::Watch { account_id: 9 }
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

use crate::keymap::{Keymap, Mode, MAX_COUNT};

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
