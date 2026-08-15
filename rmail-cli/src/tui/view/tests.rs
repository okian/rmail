//! Render smoke tests, against ratatui's `TestBackend` — a real render into
//! a real buffer, with no terminal involved, which is what lets these run in
//! the container the gate uses.
//!
//! `panic!` in a branch that cannot happen reads better here than the
//! `unreachable!` dance, and this module is test-only — the same exemption
//! `tui::model::tests` and `tui::overlays::tests` take.
#![allow(clippy::panic)]

use ratatui::backend::TestBackend;
use ratatui::Terminal;

use super::*;
use crate::keymap::Key;
use crate::tui::model::{update, Account, Folder, InputFor, MessageRow, Msg, OpenMessage, PickFor};

/// Render `model` and flatten the buffer into one string per row.
fn draw(model: &Model, width: u16, height: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| render(model, frame)).unwrap();
    let buffer = terminal.backend().buffer().clone();
    (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol().to_owned())
                .collect::<String>()
        })
        .collect()
}

fn screen(model: &Model) -> String {
    draw(model, 120, 30).join("\n")
}

/// Drive one key through the state machine, so the render tests see the
/// states a user can actually get the TUI into rather than ones assembled by
/// hand.
fn press(model: &mut Model, key: Key) {
    update(model, Msg::Key(key));
}

fn loaded() -> Model {
    let mut model = Model::new();
    model.account = Some(Account {
        id: 1,
        name: "personal".to_owned(),
        username: Some("me@example.com".to_owned()),
    });
    model.folders = vec![
        Folder {
            id: 1,
            name: "INBOX".to_owned(),
            message_count: 12,
        },
        Folder {
            id: 2,
            name: "Archive".to_owned(),
            message_count: 340,
        },
    ];
    model.open_folder = Some(1);
    model.messages = vec![
        MessageRow {
            id: 10,
            subject: "Quarterly invoice".to_owned(),
            from: "Alice".to_owned(),
            from_addr: Some("alice@example.com".to_owned()),
            date: Some(1_700_000_000),
            flags: Vec::new(),
            has_attachments: true,
        },
        MessageRow {
            id: 11,
            subject: "Lunch?".to_owned(),
            from: "Bob".to_owned(),
            from_addr: Some("bob@example.com".to_owned()),
            date: Some(1_700_000_100),
            flags: vec![SEEN.to_owned(), FLAGGED.to_owned()],
            has_attachments: false,
        },
    ];
    model.status = "2 message(s)".to_owned();
    model
}

#[test]
fn an_empty_model_renders_without_panicking() {
    // The very first frame, before any response has arrived. Every list is
    // empty and every `Option` is `None`; this is the frame that must not be
    // an index-out-of-bounds.
    let rendered = screen(&Model::new());
    assert!(rendered.contains("folders"));
    assert!(rendered.contains("connecting"));
}

#[test]
fn the_three_panes_show_folders_the_list_and_a_preview() {
    let rendered = screen(&loaded());
    assert!(rendered.contains("INBOX"), "{rendered}");
    assert!(rendered.contains("Archive"));
    assert!(rendered.contains("Quarterly invoice"));
    assert!(rendered.contains("Alice"));
    assert!(rendered.contains("preview"));
    assert!(
        rendered.contains("alice@example.com") || rendered.contains("Alice"),
        "the preview describes the highlighted row"
    );
    assert!(
        rendered.contains("2 message(s)"),
        "the status line is drawn"
    );
}

#[test]
fn each_row_carries_a_date_and_an_undated_row_does_not_shear_the_column() {
    // 1_700_000_000 is 2023-11-14T22:13:20Z. Which calendar day that lands on
    // depends on the reader's zone, so the assertion is on the month — true
    // for every zone from UTC-12 to UTC+14.
    let mut model = loaded();
    let rendered = screen(&model);
    assert!(rendered.contains("Nov"), "no date column: {rendered}");

    model.messages[0].date = None;
    let rendered = screen(&model);
    assert!(
        rendered.contains("Quarterly invoice"),
        "an undated row still renders: {rendered}"
    );
    assert_eq!(
        short_date(None).len(),
        short_date(Some(1_700_000_000)).len(),
        "the placeholder is exactly as wide as a real date"
    );
}

#[test]
fn unread_flagged_and_attachment_markers_are_drawn() {
    let rendered = screen(&loaded());
    assert!(rendered.contains('●'), "unread marker: {rendered}");
    assert!(rendered.contains('★'), "flagged marker");
    assert!(rendered.contains('@'), "attachment marker");
}

#[test]
fn the_viewer_shows_headers_the_body_and_the_html_offer() {
    let mut model = loaded();
    model.screen = Screen::Viewer;
    model.open = Some(OpenMessage {
        id: 10,
        headers: vec![
            ("From".to_owned(), "Zoë <zoe@example.com>".to_owned()),
            ("Subject".to_owned(), "Invoice €10".to_owned()),
        ],
        body: vec!["Total: €10".to_owned(), String::new(), "Thanks".to_owned()],
        has_html: true,
        attachments: vec!["doc.pdf (application/pdf, 5 bytes)".to_owned()],
    });

    let rendered = screen(&model);
    assert!(rendered.contains("Zoë <zoe@example.com>"), "{rendered}");
    assert!(rendered.contains("Invoice €10"));
    assert!(rendered.contains("Total: €10"));
    assert!(rendered.contains("doc.pdf"));
    assert!(
        rendered.contains("press o to open it in a browser"),
        "the HTML alternative is offered where the user can see it"
    );
}

#[test]
fn the_busy_marker_appears_only_while_something_is_in_flight() {
    let mut model = loaded();
    assert!(!screen(&model).contains("in flight"));

    model.inflight = 2;
    assert!(
        screen(&model).contains("2 in flight"),
        "the user can see work is happening — and keep using the UI anyway"
    );
}

#[test]
fn the_help_overlay_lists_the_bindings_that_are_actually_in_force() {
    // Read out of the keymap rather than from a table beside it: task 83's
    // hand-written list was right while the bindings were a `match` nobody
    // could change, and became a list that lies the moment `keys.toml` could
    // rebind anything.
    let mut model = loaded();
    model.overlay = Some(Overlay::Help);
    let rendered = draw(&model, 120, 44).join("\n");

    for (chords, description) in [
        ("j / <down>", "down"),
        ("k / <up>", "up"),
        ("gg", "first row"),
        ("G", "last row"),
        ("<tab>", "switch between the folder and message panes"),
        ("<enter>", "open the folder or the message"),
        ("q", "back, or quit from the message list"),
        ("<c-c>", "quit from anywhere"),
        ("?", "this help"),
        ("a", "archive"),
        ("d", "delete"),
        ("s", "toggle read"),
        ("f", "toggle flagged"),
        ("r", "reply"),
        ("F", "forward"),
        ("o", "open the HTML part in a browser"),
        ("v", "visual selection"),
    ] {
        assert!(
            rendered.contains(chords),
            "help is missing the chord {chords:?}"
        );
        assert!(
            rendered.contains(description),
            "help is missing {description:?}"
        );
    }
    assert!(
        rendered.contains("mail keys set"),
        "the help says how to change what it lists"
    );
}

#[test]
fn a_rebound_key_shows_up_in_the_help() {
    let mut model = loaded();
    model.keymap =
        crate::keymap::file::parse("[normal]\n\"<c-d>\" = \"cursor.down\"\n", "keys.toml").unwrap();
    model.overlay = Some(Overlay::Help);
    let rendered = draw(&model, 120, 44).join("\n");
    assert!(
        rendered.contains("j / <c-d> / <down>"),
        "the help lists the built-in bindings and not the user's:\n{rendered}"
    );
}

#[test]
fn the_status_line_shows_the_mode_and_what_is_half_typed() {
    // A half-typed command that is invisible is indistinguishable from a
    // keyboard that has stopped responding — and the user's next move,
    // mashing keys, is the thing that makes it worse.
    let mut model = loaded();
    press(&mut model, Key::Char('3'));
    press(&mut model, Key::Char('g'));
    assert!(screen(&model).contains("3g"), "{}", screen(&model));

    let mut model = loaded();
    press(&mut model, Key::Char('v'));
    assert!(
        screen(&model).contains("-- VISUAL --"),
        "{}",
        screen(&model)
    );
}

#[test]
fn a_selected_row_the_cursor_is_not_on_is_still_marked() {
    // The row under the cursor is highlighted either way, so that one proves
    // nothing. What matters is the *rest* of the selection: a bulk archive
    // acting on rows the user could not see would be indistinguishable from a
    // bug in the selection arithmetic.
    //
    // Column 30 is inside the message pane (it starts at 20% of 120 columns);
    // row 0 is the pane's border, so row 1 is the first message.
    let background = |model: &Model| {
        let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
        terminal.draw(|frame| render(model, frame)).unwrap();
        terminal.backend().buffer()[(30, 1)].style().bg
    };

    // Same cursor position in both, so the only difference is the selection.
    let mut plain = loaded();
    press(&mut plain, Key::Char('j'));

    let mut selected = loaded();
    press(&mut selected, Key::Char('v'));
    press(&mut selected, Key::Char('j'));
    assert_eq!(selected.selection(), Some((0, 1)));

    assert_ne!(
        background(&selected),
        background(&plain),
        "a selected row the cursor is not on looks exactly like an unselected one"
    );
}

#[test]
fn the_folder_picker_overlay_lists_the_folders() {
    let mut model = loaded();
    model.overlay = Some(Overlay::Pick {
        what: PickFor::Move,
        message_ids: vec![10],
        idx: 1,
    });
    let rendered = screen(&model);
    assert!(rendered.contains("move to which folder?"), "{rendered}");
    assert!(rendered.contains("Archive"));
}

#[test]
fn the_confirm_and_input_overlays_show_their_prompts() {
    let mut model = loaded();
    model.overlay = Some(Overlay::Confirm {
        prompt: "delete permanently (expunges on the server)? [y/N]".to_owned(),
        message_ids: vec![10],
    });
    assert!(screen(&model).contains("expunges on the server"));

    model.overlay = Some(Overlay::Input {
        prompt: "forward to".to_owned(),
        buffer: "bob@example.com".to_owned(),
        what: InputFor::ForwardTo,
        message_id: 10,
    });
    let rendered = screen(&model);
    assert!(rendered.contains("forward to"));
    assert!(rendered.contains("bob@example.com"), "{rendered}");
}

#[test]
fn a_terminal_smaller_than_the_overlays_still_renders() {
    // `centered` clamps rather than subtracting into an underflow; ratatui's
    // own layout would panic on a `Rect` that does not fit its parent.
    let mut model = loaded();
    model.overlay = Some(Overlay::Help);
    let rendered = draw(&model, 20, 5);
    assert_eq!(rendered.len(), 5);

    model.overlay = Some(Overlay::Pick {
        what: PickFor::Copy,
        message_ids: vec![10],
        idx: 0,
    });
    assert_eq!(draw(&model, 12, 3).len(), 3);
}

#[test]
fn a_body_far_longer_than_the_pane_scrolls_rather_than_overflowing() {
    let mut model = loaded();
    model.screen = Screen::Viewer;
    model.open = Some(OpenMessage {
        id: 10,
        headers: Vec::new(),
        body: (0..500).map(|n| format!("line {n}")).collect(),
        has_html: false,
        attachments: Vec::new(),
    });

    let top = screen(&model);
    assert!(top.contains("line 0"));

    model.scroll = 400;
    let scrolled = screen(&model);
    assert!(scrolled.contains("line 400"), "{scrolled}");
    assert!(!scrolled.contains("line 0 "), "the top scrolled away");
}

// ---------------------------------------------------------------------------
// task 85's overlays
// ---------------------------------------------------------------------------

use crate::tui::model::{AskEvent, Cmd, SearchEvent};
use crate::tui::overlays::{AiSummary, Citation, Hit, OutboxRow};

/// The generation of the last streaming command in `cmds` — how a test
/// addresses the query the model is currently running.
fn generation(cmds: &[Cmd]) -> u64 {
    for cmd in cmds.iter().rev() {
        match cmd {
            Cmd::Search { generation, .. }
            | Cmd::Find { generation, .. }
            | Cmd::Ask { generation, .. } => return *generation,
            _ => {}
        }
    }
    panic!("no streaming command in {cmds:?}");
}

fn type_in(model: &mut Model, text: &str) -> Vec<Cmd> {
    let mut cmds = Vec::new();
    for c in text.chars() {
        cmds.extend(update(model, Msg::Key(Key::Char(c))));
    }
    cmds
}

/// A search overlay with one hit streamed into it.
fn with_hit(subject: &str, snippet: &str, highlights: Vec<(usize, usize)>) -> Model {
    let mut model = loaded();
    press(&mut model, Key::Char('/'));
    let generation = generation(&type_in(&mut model, "invoice"));
    update(
        &mut model,
        Msg::Search {
            generation,
            event: SearchEvent::Hit(Box::new(Hit {
                message_id: 10,
                subject: subject.to_owned(),
                from: "billing@acme.com".to_owned(),
                date: Some(1_700_000_000),
                snippet: snippet.to_owned(),
                highlights,
                sources: vec!["lexical".to_owned()],
            })),
        },
    );
    update(
        &mut model,
        Msg::Search {
            generation,
            event: SearchEvent::Done(Ok(())),
        },
    );
    model
}

#[test]
fn the_search_overlay_renders_the_query_and_the_hits_that_streamed_in() {
    let model = with_hit("Quarterly invoice", "your invoice for June", vec![(5, 12)]);
    let screen = screen(&model);
    assert!(screen.contains("invoice"), "{screen}");
    assert!(screen.contains("Quarterly invoice"), "{screen}");
    assert!(screen.contains("your invoice for June"), "{screen}");
    assert!(
        screen.contains("billing@acme.com"),
        "the sender is on the row: {screen}"
    );
}

#[test]
fn a_snippet_highlight_is_styled_without_changing_the_text() {
    // The highlight must be *style*, never markup spliced into the text —
    // splicing is what re-introduces the escaping bugs offsets exist to avoid.
    let model = with_hit("Quarterly invoice", "your invoice for June", vec![(5, 12)]);
    let mut terminal = Terminal::new(TestBackend::new(120, 30)).unwrap();
    terminal.draw(|frame| render(&model, frame)).unwrap();
    let buffer = terminal.backend().buffer().clone();

    let mut bold_run = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            let cell = &buffer[(x, y)];
            if cell.style().fg == Some(Color::Yellow)
                && cell.style().add_modifier.contains(Modifier::BOLD)
            {
                bold_run.push_str(cell.symbol());
            }
        }
    }
    assert!(
        bold_run.contains("invoice"),
        "the matched span is the styled one, got {bold_run:?}"
    );
    let screen = screen(&model);
    assert!(
        !screen.contains('*') && !screen.contains('['),
        "no markup was spliced into the snippet: {screen}"
    );
}

#[test]
fn a_hostile_subject_never_reaches_the_rendered_buffer() {
    // The end-to-end version of the unit test in `tui::overlays`: a subject
    // carrying an ANSI run and a bidi override, all the way through the
    // renderer into a real buffer.
    let model = with_hit(
        "pay \u{1b}[2Jnow \u{202e}esrever\u{202c}",
        "body \u{1b}[31mred\u{1b}[0m\u{7}",
        Vec::new(),
    );
    let screen = screen(&model);
    for bad in ['\u{1b}', '\u{7}', '\u{202e}', '\u{202c}'] {
        assert!(
            !screen.contains(bad),
            "{bad:?} reached the terminal buffer: {screen:?}"
        );
    }
    assert!(screen.contains("pay"), "and the real text survived");
}

#[test]
fn the_ask_pane_renders_its_citations_and_says_whose_verdict_grounded_is() {
    let mut model = loaded();
    press(&mut model, Key::Char('A'));
    let generation = generation(&{
        type_in(&mut model, "who owes me");
        update(&mut model, Msg::Key(Key::Enter))
    });
    for event in [
        AskEvent::Trace("retrieved 9 · packed 4".to_owned()),
        AskEvent::Token("Acme owes you [1].".to_owned()),
        AskEvent::Cite(Box::new(Citation {
            label: 1,
            message_id: 501,
            subject: "invoice 338".to_owned(),
            from_addr: "billing@acme.com".to_owned(),
            mailbox: "INBOX".to_owned(),
            quote: "Total $4,200".to_owned(),
        })),
        AskEvent::Done {
            grounded: false,
            refusal: "nothing says so".to_owned(),
        },
    ] {
        update(&mut model, Msg::Ask { generation, event });
    }

    let screen = screen(&model);
    assert!(screen.contains("retrieved 9"), "{screen}");
    assert!(screen.contains("Acme owes you [1]."), "{screen}");
    assert!(screen.contains("invoice 338"), "{screen}");
    assert!(
        screen.contains("NOT grounded") && screen.contains("daemon"),
        "an ungrounded answer is labelled as the daemon's verdict: {screen}"
    );
}

#[test]
fn the_undo_toast_renders_its_countdown_above_the_status_line() {
    let mut model = loaded();
    update(
        &mut model,
        Msg::Outbox {
            now: 1_000,
            result: Ok(vec![OutboxRow {
                id: 9,
                to: "bob@example.com".to_owned(),
                subject: "the one you regret".to_owned(),
                state: "scheduled".to_owned(),
                send_at: 1_010,
                undo_deadline: Some(1_007),
                last_error: None,
            }]),
        },
    );
    let rows = draw(&model, 120, 30);
    let toast = rows
        .get(rows.len() - 2)
        .cloned()
        .unwrap_or_else(|| panic!("no toast row"));
    assert!(toast.contains("bob@example.com"), "{toast}");
    assert!(toast.contains("7s"), "the countdown is on it: {toast}");
    assert!(toast.contains("u undoes"), "{toast}");
}

#[test]
fn the_ai_panel_takes_a_column_and_leaves_the_list_visible() {
    let mut model = loaded();
    let before = screen(&model);
    assert!(before.contains("Quarterly invoice"));

    press(&mut model, Key::Char('\\'));
    update(
        &mut model,
        Msg::Summarized {
            message_id: 10,
            result: Ok(AiSummary {
                message_id: 10,
                status: "ok".to_owned(),
                tl_dr: Some("an invoice is due".to_owned()),
                key_points: vec!["due Friday".to_owned()],
                ..AiSummary::default()
            }),
        },
    );
    let after = screen(&model);
    assert!(after.contains("an invoice is due"), "{after}");
    assert!(after.contains("due Friday"), "{after}");
    assert!(
        after.contains("Quarterly invoice"),
        "the panel is a column, not a cover: {after}"
    );
}

#[test]
fn the_palette_renders_ids_bindings_and_help_together() {
    let mut model = loaded();
    press(&mut model, Key::ctrl('k'));
    type_in(&mut model, "archive");
    let screen = screen(&model);
    assert!(screen.contains("message.archive"), "{screen}");
    assert!(screen.contains("archive"), "{screen}");
}

#[test]
fn a_terminal_far_too_small_for_an_overlay_still_renders() {
    // `centered`/`centered_pct` clamp rather than panic inside ratatui's
    // layout arithmetic, and every overlay has to survive an 8x4 window.
    let model = with_hit("Quarterly invoice", "your invoice for June", Vec::new());
    for (width, height) in [(8, 4), (1, 1), (40, 6)] {
        let rows = draw(&model, width, height);
        assert_eq!(rows.len(), usize::from(height));
    }
}

#[test]
fn a_refused_cancel_leaves_the_outbox_listing_on_screen() {
    let mut model = loaded();
    press(&mut model, Key::Char('O'));
    update(
        &mut model,
        Msg::Outbox {
            now: 1_000,
            result: Ok(vec![OutboxRow {
                id: 1,
                to: "bob@example.com".to_owned(),
                subject: "still queued".to_owned(),
                state: "scheduled".to_owned(),
                send_at: 1_010,
                undo_deadline: None,
                last_error: None,
            }]),
        },
    );
    press(&mut model, Key::Char('u'));
    update(
        &mut model,
        Msg::Outbox {
            now: 1_001,
            result: Err("already claimed by a worker".to_owned()),
        },
    );

    let screen = screen(&model);
    assert!(
        screen.contains("still queued"),
        "a refused cancel must not replace the listing with its error: {screen}"
    );
    assert!(screen.contains("already claimed"), "{screen}");
}

#[test]
fn hostile_text_reaching_the_status_line_is_neutralized() {
    // `OutboxEntry.last_error` is a remote SMTP server's verbatim reply, and
    // `Enter` on an outbox row puts it in the status line — the one surface
    // every part of the TUI writes to.
    let mut model = loaded();
    press(&mut model, Key::Char('O'));
    update(
        &mut model,
        Msg::Outbox {
            now: 1_000,
            result: Ok(vec![OutboxRow {
                id: 1,
                to: "bob@example.com".to_owned(),
                subject: "rejected".to_owned(),
                state: "failed".to_owned(),
                send_at: 1_010,
                undo_deadline: None,
                last_error: Some("550 \u{1b}[2Jdenied \u{202e}esrever\u{202c}".to_owned()),
            }]),
        },
    );
    press(&mut model, Key::Enter);

    let screen = screen(&model);
    for bad in ['\u{1b}', '\u{202e}', '\u{202c}'] {
        assert!(
            !screen.contains(bad),
            "{bad:?} reached the status line: {screen:?}"
        );
    }
    assert!(screen.contains("550"), "and the real text survived");
}
