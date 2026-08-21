//! Render smoke tests, against ratatui's `TestBackend` — a real render into
//! a real buffer, with no terminal involved, which is what lets these run in
//! the container the gate uses.
//!
//! `panic!` in a branch that cannot happen reads better here than the
//! `unreachable!` dance, and this module is test-only — the same exemption
//! `tui::model::tests` and `tui::overlays::tests` take.
#![allow(clippy::panic)]

use std::collections::VecDeque;

use ratatui::backend::TestBackend;
use ratatui::style::Color;
use ratatui::Terminal;

use super::*;
use crate::keymap::{Key, Mode};
use crate::tui::model::{
    update, Account, Confirmed, Folder, InputFor, Level, MessageRow, Msg, OpenMessage, PickFor,
    ReplyEvent,
};
use crate::tui::overlays::UndoToast;
use crate::tui::theme::ThemeName;

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
            has_note: false,
            to: None,
            tags: Vec::new(),
            ai: None,
        },
        MessageRow {
            id: 11,
            subject: "Lunch?".to_owned(),
            from: "Bob".to_owned(),
            from_addr: Some("bob@example.com".to_owned()),
            date: Some(1_700_000_100),
            flags: vec![SEEN.to_owned(), FLAGGED.to_owned()],
            has_attachments: false,
            has_note: false,
            to: None,
            tags: Vec::new(),
            ai: None,
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
fn a_zoomed_card_replaces_the_panes_with_a_named_placeholder() {
    let mut model = loaded();
    let before = screen(&model);
    assert!(before.contains("Quarterly invoice"), "{before}");

    press(&mut model, Key::Char('Z'));
    let after = screen(&model);
    // The pane title's em-dash form, not a bare "list"/"zoomed" — both of
    // those also appear in the status line's own "list zoomed" message, so
    // a looser assertion would pass even if the placeholder's title itself
    // were deleted.
    assert!(after.contains("list — zoomed"), "{after}");
    assert!(
        !after.contains("Quarterly invoice"),
        "the placeholder replaces the panes rather than sharing the frame with them: {after}"
    );
}

#[test]
fn the_zoomed_placeholder_names_whichever_card_was_just_zoomed() {
    // `Z` always targets `card_focus` (`toggle_zoom`'s own doc), so this is
    // indirectly a test of `card_focus` too — but what the placeholder
    // itself reads off is `model.zoom`, and that is what this asserts.
    let mut model = loaded();
    model.card_focus = Card::Reader;
    press(&mut model, Key::Char('Z'));
    assert_eq!(model.zoom, Some(Card::Reader));
    let rendered = screen(&model);
    // Not a bare "reader" — the status line's own "reader zoomed" message
    // would satisfy that without the placeholder's title doing anything.
    assert!(rendered.contains("reader — zoomed"), "{rendered}");
}

#[test]
fn the_status_line_still_renders_while_a_card_is_zoomed() {
    // Not "2 message(s)" — `toggle_zoom` writes its own status message, so
    // that one is legitimately gone the moment `Z` is pressed. What must
    // survive is the chrome that has nothing to do with zoom at all: the
    // mode indicator and the account/folder scope.
    let mut model = loaded();
    press(&mut model, Key::Char('Z'));
    let rendered = screen(&model);
    assert!(rendered.contains("NORMAL"), "{rendered}");
    assert!(
        rendered.contains("personal/INBOX"),
        "chrome around the deck must survive a zoomed card, only the deck itself changes: {rendered}"
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
    // Task 92 turned the marker into a fixed zone, so it is a glyph and a count
    // rather than the sentence `2 in flight`: a zone wide enough for the words
    // is a zone taken permanently from the message beside it. `tui::status`'
    // own tests cover the zone; this one covers the marker being *conditional*.
    let mut model = loaded();
    assert!(!screen(&model).contains('⧗'));

    model.inflight = 2;
    assert!(
        screen(&model).contains("⧗2"),
        "the user can see work is happening — and keep using the UI anyway"
    );
}

#[test]
fn the_help_overlay_lists_the_bindings_that_are_actually_in_force() {
    // Read out of the keymap rather than from a table beside it: task 83's
    // hand-written list was right while the bindings were a `match` nobody
    // could change, and became a list that lies the moment `keys.toml` could
    // rebind anything. Checked at the data layer, not the rendered buffer:
    // task 102 made this list genuinely scrollable, so a wide enough spread
    // of bindings — this one runs the full alphabet, on purpose — is no
    // longer all on screen at once at any one terminal size, which is a
    // rendering-and-viewport question with nothing to do with what this
    // test actually claims ("the data is keymap-driven"). The claim past
    // the fold is exactly what `pressing_g_on_the_key_reference_scrolls_to_a_
    // binding_the_first_screen_never_shows` below covers instead, at the
    // one place a scroll position is meaningful to assert about at all —
    // the rendered buffer.
    let model = loaded();
    let pane = HelpPane::new(Mode::Normal, &model.keymap);
    let rows = format!("{:?}", pane.rows);

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
            rows.contains(chords),
            "help is missing the chord {chords:?}: {rows}"
        );
        assert!(
            rows.contains(description),
            "help is missing {description:?}: {rows}"
        );
    }
}

#[test]
fn pressing_g_on_the_key_reference_scrolls_to_a_binding_the_first_screen_never_shows() {
    // `<c-c>` (quit) is not on the unscrolled first screen at all — the
    // exact regression this pins: task 102's whole "scrollable" acceptance
    // clause means the list widget's own viewport, not this function,
    // decides what is visible, and only `G` jumping the row cursor to the
    // end proves that mechanism is actually wired up rather than merely
    // plausible from reading the code.
    let mut model = loaded();
    press(&mut model, Key::Char('?'));
    let unscrolled = draw(&model, 120, 24).join("\n");
    assert!(
        !unscrolled.contains("<c-c>"),
        "the repro needs quit to actually start off screen: {unscrolled}"
    );

    press(&mut model, Key::Char('G'));
    let scrolled = draw(&model, 120, 24).join("\n");
    assert!(scrolled.contains("<c-c>"), "{scrolled}");
    assert!(scrolled.contains("quit"), "{scrolled}");
}

#[test]
fn a_rebound_key_shows_up_in_the_help() {
    let mut model = loaded();
    model.keymap =
        crate::keymap::file::parse("[normal]\n\"<c-d>\" = \"cursor.down\"\n", "keys.toml").unwrap();
    model.set_overlay(Overlay::Help(Box::new(HelpPane::new(
        Mode::Normal,
        &model.keymap,
    ))));
    let rendered = draw(&model, 120, 44).join("\n");
    assert!(
        rendered.contains("j / <c-d> / <down>"),
        "the help lists the built-in bindings and not the user's:\n{rendered}"
    );
}

#[test]
fn the_key_references_filter_line_shows_a_caret_while_typing() {
    // Every other typing surface in this TUI (search, finder, the `:` line,
    // ask, even the manual's own `/` search) renders through `prompt_line`:
    // an accent sigil, the typed text, and a block cursor. This overlay's
    // filter had fallen out of step with all of them — flat muted text with
    // no cursor and no visible sign that keystrokes are now going into the
    // filter rather than to the row actions below.
    let mut model = loaded();
    press(&mut model, Key::Char('?'));
    press(&mut model, Key::Char('/'));
    press(&mut model, Key::Char('a'));
    press(&mut model, Key::Char('r'));
    let rendered = draw(&model, 120, 24).join("\n");
    // `prompt_line` renders the sigil as `"{sigil} "`, matching every other
    // typing surface — not `"/ar"` glued together.
    assert!(rendered.contains("/ ar"), "{rendered}");
    assert!(rendered.contains('▏'), "no caret while typing:\n{rendered}");
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
    model.set_overlay(Overlay::Pick {
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
    model.set_overlay(Overlay::Confirm {
        prompt: "delete permanently (expunges on the server)? [y/N]".to_owned(),
        then: Confirmed::Delete(vec![10]),
    });
    assert!(screen(&model).contains("expunges on the server"));

    model.set_overlay(Overlay::Input {
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
    model.set_overlay(Overlay::Help(Box::new(HelpPane::new(
        Mode::Normal,
        &model.keymap,
    ))));
    let rendered = draw(&model, 20, 5);
    assert_eq!(rendered.len(), 5);

    model.set_overlay(Overlay::Pick {
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
            | Cmd::Ask { generation, .. }
            | Cmd::DraftReply { generation, .. } => return *generation,
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
fn the_reply_pane_renders_its_context_prose_and_the_drafted_id() {
    let mut model = loaded();
    press(&mut model, Key::Char(':'));
    let generation = generation(&{
        type_in(&mut model, "reply --ai push to tuesday");
        update(&mut model, Msg::Key(Key::Enter))
    });
    for event in [
        ReplyEvent::Context("2 thread message(s)".to_owned()),
        ReplyEvent::Token("Sounds good, see you then.".to_owned()),
        ReplyEvent::Drafted {
            draft_id: 42,
            to: "alice@example.com".to_owned(),
        },
        ReplyEvent::Done,
    ] {
        update(&mut model, Msg::Reply { generation, event });
    }

    let screen = screen(&model);
    assert!(screen.contains("2 thread message(s)"), "{screen}");
    assert!(screen.contains("Sounds good, see you then."), "{screen}");
    assert!(
        screen.contains("draft 42") && screen.contains("alice@example.com"),
        "the drafted id and recipient are the pane's own next-step hint: {screen}"
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

    press(&mut model, Key::Char(' '));
    press(&mut model, Key::Char('a'));
    press(&mut model, Key::Char('p'));
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
fn the_command_line_renders_verbs_bindings_and_help_together() {
    let mut model = loaded();
    press(&mut model, Key::ctrl('k'));
    type_in(&mut model, "archive");
    let screen = screen(&model);
    assert!(screen.contains("message archive"), "{screen}");
    assert!(screen.contains("archive"), "{screen}");
}

#[test]
fn the_fallback_marker_appears_only_while_the_fallback_is_live() {
    // The marker says "Enter runs this row". Typing the verb out in full
    // means Enter runs the *line*, so pointing at a row would be pointing at
    // something Enter is not going to do.
    let mut model = loaded();
    press(&mut model, Key::ctrl('k'));
    type_in(&mut model, "message arch");
    assert!(
        screen(&model).contains("> message archive"),
        "{}",
        screen(&model)
    );

    // `archive` on its own is not a verb — `message archive` is — so the
    // line only stops being a fallback once the whole path is typed.
    type_in(&mut model, "ive");
    let full = screen(&model);
    assert!(full.contains("message archive"), "{full}");
    assert!(
        !full.contains("> message archive"),
        "the line names the verb itself now: {full}"
    );
}

#[test]
fn a_parse_error_is_shown_in_the_command_line_in_red() {
    let dark = Theme::dark();
    let mut model = loaded();
    press(&mut model, Key::ctrl('k'));
    type_in(&mut model, "message copy \"unterminated");
    press(&mut model, Key::Enter);
    let screen = screen(&model);
    assert!(screen.contains("unterminated quote"), "{screen}");
    assert!(
        !chars_matching(&model, 120, 30, dark.err).is_empty(),
        "the complaint is in the error style: {screen}"
    );
    assert!(model.overlay_is_open(), "and the overlay stays open");
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

// ---------------------------------------------------------------------------
// task 87's theme — dark is behavior-preserving, mono carries no color-only
// meaning, and every `Color::` literal lives in `theme.rs` and nowhere else.
// ---------------------------------------------------------------------------

/// Every character in the frame whose foreground, background and added
/// modifiers this crate's tokens ever set are realized by that cell — the
/// field-by-field check `a_snippet_highlight_is_styled_without_changing_the_text`
/// established above, generalized so each theme assertion does not re-walk
/// the buffer by hand.
///
/// A field `style` leaves unset (`None`, or the empty [`Modifier`] set) is
/// treated as "no constraint," not "must be unset" — this checks that a
/// token is *realized*, not that the cell carries nothing else:
///
/// - `Cell::style()` reconstructs an *every-buffer-cell* style from the
///   cell's own always-concrete `fg`/`bg` (a cell that was never explicitly
///   colored still reports `Some(Color::Reset)`, never `None`), so comparing
///   `cell.style().bg == style.bg` for a token that never calls `.bg()`
///   would compare `Some(Reset)` against `None` and always fail — nothing to
///   do with whether the coloring this test actually cares about is present.
/// - Modifiers compose across layers rather than replacing each other:
///   `render_messages` sets `Modifier::BOLD` at the *row* level for an
///   unread message, and `Style::patch` unions rather than overwrites, so
///   that row's unread-marker glyph is genuinely `theme.unread` (its own
///   token) **and** bold (the row's) at once. A token that itself carries no
///   modifier is a claim about color, not a claim that nothing else styles
///   the cell — so this checks `contains`, not equality, the same
///   loosening `fg`/`bg` already get.
///
/// Never whole-`Style` equality either: a widget can set fields no token
/// here touches at all (`underline_color`, `sub_modifier`).
///
/// Two more things this deliberately does *not* try to fix, so a caller does
/// not mistake the looseness above for "matches anything close enough":
///
/// - `style` must set at least one of `fg`/`bg`/`add_modifier`, or every
///   cell in the frame would match trivially (three of [`Theme::mono`]'s
///   fields — `ok`, `unread`, `flagged`, `attachment` — are exactly
///   [`Style::default()`] by design; asking this function for one of those
///   is almost certainly not the check a caller meant to write).
/// - Rows are newline-joined rather than concatenated flat, so a match
///   spanning the tail of one row and the head of the next cannot be
///   mistaken for one contiguous run — but a caller still cannot assume two
///   *tokens* sharing a color (`warn` and `unread` are both plain yellow in
///   `dark`) won't both satisfy the same query; picking assertions that
///   land on a row/column no sibling token could plausibly reach is still
///   the caller's job, the same way it already is for `screen()`.
fn chars_matching(model: &Model, width: u16, height: u16, style: Style) -> String {
    assert!(
        style.fg.is_some() || style.bg.is_some() || !style.add_modifier.is_empty(),
        "chars_matching(.., Style::default()) matches every cell in the frame; \
         pass a style that actually constrains something"
    );
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal.draw(|frame| render(model, frame)).unwrap();
    let buffer = terminal.backend().buffer().clone();
    let mut rows = Vec::with_capacity(usize::from(buffer.area.height));
    for y in 0..buffer.area.height {
        let mut row = String::new();
        for x in 0..buffer.area.width {
            let cell = &buffer[(x, y)];
            let cell_style = cell.style();
            let fg_matches = style.fg.is_none() || cell_style.fg == style.fg;
            let bg_matches = style.bg.is_none() || cell_style.bg == style.bg;
            let modifier_matches = cell_style.add_modifier.contains(style.add_modifier);
            if fg_matches && bg_matches && modifier_matches {
                row.push_str(cell.symbol());
            }
        }
        rows.push(row);
    }
    rows.join("\n")
}

#[test]
fn dark_theme_pane_borders_match_the_historical_colors() {
    let dark = Theme::dark();
    let model = loaded(); // focus defaults to Focus::Messages
    let rendered = draw(&model, 120, 30);
    let screen = rendered.join("\n");

    // The focused pane's border/title ("messages" is `render_messages`'s
    // fallback title when no folder name is current — `loaded` opens folder
    // 1, so the real title "INBOX" is what is actually focus-colored).
    let focused = chars_matching(&model, 120, 30, dark.border_focus);
    assert!(
        focused.contains("INBOX"),
        "the focused pane's border/title is not styled `border_focus`: {screen}"
    );

    let blurred = chars_matching(&model, 120, 30, dark.border_blur);
    assert!(
        blurred.contains("folders") && blurred.contains("preview"),
        "the unfocused panes are not styled `border_blur`: {screen}"
    );
}

#[test]
fn dark_theme_message_marks_and_visual_selection_match_the_historical_colors() {
    let dark = Theme::dark();
    // Row 10 (index 0) is unread and has an attachment; row 11 (index 1) is
    // flagged. `chars_matching` on each glyph's own token is what proves the
    // three-span split (task 87's refactor) draws identically to the single
    // combined span it replaced.
    //
    // The cursor's own row is excluded from each check: `List::highlight_style`
    // (`theme.sel_focus`) legitimately overrides a row's own span styling for
    // whichever row the cursor is on — that is what makes a highlighted row
    // look highlighted — so this moves the cursor off the row a given
    // assertion is about, rather than asserting through an overlay that is
    // correctly there.
    let mut cursor_on_row1 = loaded();
    cursor_on_row1.message_idx = 1;
    assert!(chars_matching(&cursor_on_row1, 120, 30, dark.unread).contains('●'));
    assert!(chars_matching(&cursor_on_row1, 120, 30, dark.attachment).contains('@'));

    let mut cursor_on_row0 = loaded();
    cursor_on_row0.message_idx = 0;
    assert!(chars_matching(&cursor_on_row0, 120, 30, dark.flagged).contains('★'));

    // The visual selection spans rows 0..=1; row 0 is not the cursor here, so
    // it is `sel_row` alone, unpatched by any highlight.
    let mut selecting = loaded();
    selecting.visual = Some(0);
    selecting.message_idx = 1;
    let selected_row = chars_matching(&selecting, 120, 30, dark.sel_row);
    assert!(
        selected_row.contains("Alice"),
        "a visual selection must repaint every row it covers with `sel_row`, \
         including the one the cursor is not on: {}",
        screen(&selecting)
    );
}

#[test]
fn dark_theme_viewer_html_notice_matches_the_historical_color() {
    let dark = Theme::dark();
    let mut model = loaded();
    model.screen = Screen::Viewer;
    model.open = Some(OpenMessage {
        id: 10,
        headers: vec![("From".to_owned(), "alice@example.com".to_owned())],
        body: vec!["hello".to_owned()],
        has_html: true,
        attachments: Vec::new(),
    });
    let accented = chars_matching(&model, 120, 30, dark.accent);
    assert!(
        accented.contains("HTML"),
        "the HTML-available notice is not styled `accent`: {}",
        screen(&model)
    );
}

#[test]
fn dark_theme_status_line_matches_the_historical_colors_for_each_level() {
    let dark = Theme::dark();
    let mut model = loaded();

    model.level = Level::Info;
    model.status = "synced".to_owned();
    assert!(chars_matching(&model, 120, 30, dark.ok).contains("synced"));

    model.level = Level::Error;
    model.status = "disconnected".to_owned();
    assert!(chars_matching(&model, 120, 30, dark.err).contains("disconnected"));

    // Visual mode's `-- VISUAL --` indicator, and a half-typed `3g`.
    model.visual = Some(model.message_idx);
    let mode = chars_matching(&model, 120, 30, dark.mode_indicator);
    assert!(mode.contains("VISUAL"), "{mode:?}");
    model.pending.clear();
    press(&mut model, Key::Char('3'));
    press(&mut model, Key::Char('g'));
    let pending = chars_matching(&model, 120, 30, dark.warn);
    assert!(pending.contains("3g"), "{pending:?}");
}

#[test]
fn dark_theme_help_overlay_matches_the_historical_colors() {
    let dark = Theme::dark();
    let mut model = loaded();
    model.set_overlay(Overlay::Help(Box::new(HelpPane::new(
        Mode::Normal,
        &model.keymap,
    ))));
    let emphasized = chars_matching(&model, 120, 30, dark.emphasis);
    // Every bound chord is rendered in `emphasis` — `a` (archive) is a
    // built-in default binding, so it is always present.
    assert!(emphasized.contains('a'), "{}", screen(&model));
}

#[test]
fn dark_theme_pick_confirm_and_input_overlays_border_matches_the_historical_color() {
    let dark = Theme::dark();
    let mut model = loaded();
    // The messages pane behind every overlay here is *also* `border_focus`
    // (its own border stays on screen, uncovered, around the small centered
    // modal) — so each check below is on the overlay's own *title*, which
    // only exists inside the overlay's border, rather than on
    // `!chars_matching(border_focus).is_empty()`, which the ambient pane
    // border would already satisfy regardless of how the overlay drew.
    for (overlay, title) in [
        (
            Overlay::Pick {
                what: PickFor::Move,
                message_ids: vec![10],
                idx: 0,
            },
            "move to which folder?",
        ),
        (
            Overlay::Confirm {
                prompt: "delete? [y/N]".to_owned(),
                then: Confirmed::Delete(vec![10]),
            },
            "confirm",
        ),
        (
            Overlay::Input {
                prompt: "forward to".to_owned(),
                buffer: String::new(),
                what: InputFor::ForwardTo,
                message_id: 10,
            },
            "forward to",
        ),
    ] {
        model.set_overlay(overlay);
        // Every overlay border is drawn `focused` — an overlay is definitionally
        // the thing with the keyboard's attention.
        let border = chars_matching(&model, 120, 30, dark.border_focus);
        assert!(
            border.contains(title),
            "expected {title:?} styled `border_focus`, got {border:?}: {}",
            screen(&model)
        );
    }
}

#[test]
fn dark_theme_finder_kind_label_matches_the_historical_color() {
    let dark = Theme::dark();
    let mut model = loaded();
    // Two items, cursor left on the first (default `cursor: 0`) — the
    // *second* row's kind label is what this checks, so `List::highlight_style`
    // patching the first row is beside the point.
    model.set_overlay(Overlay::Finder(Box::new(FinderPane {
        query: ">arch".to_owned(),
        items: vec![
            overlays::FinderItem {
                kind: overlays::FinderKind::Mailbox,
                ref_id: 2,
                primary: "Archive".to_owned(),
                secondary: String::new(),
                positions: Vec::new(),
                mailbox_id: 0,
            },
            overlays::FinderItem {
                kind: overlays::FinderKind::Mailbox,
                ref_id: 1,
                primary: "INBOX".to_owned(),
                secondary: String::new(),
                positions: Vec::new(),
                mailbox_id: 0,
            },
        ],
        complete: true,
        ..FinderPane::default()
    })));
    let kind_label = chars_matching(&model, 120, 30, dark.finder_kind);
    assert!(kind_label.contains("folder"), "{}", screen(&model));
}

#[test]
fn dark_theme_command_chords_match_the_historical_color() {
    let dark = Theme::dark();
    let mut model = loaded();
    press(&mut model, Key::ctrl('k'));
    // "message" prefix-matches every `message *` verb (archive, copy,
    // delete, …), alphabetically tie-broken by path (`command_matches`'s own
    // rule) — so row 0 is `message archive`, chord `a`.
    //
    // No cursor is moved off it first, and none needs to be: task 89's list
    // draws no selected row at all. That is precisely what makes this
    // measurable — `List::highlight_style` overrides a row's own span styling
    // wholesale, so under task 85's palette this assertion could only be made
    // about a row the cursor was *not* on.
    type_in(&mut model, "message");
    // `warn` (plain yellow) is what the chord column has always used —
    // distinct from `match_hl` (yellow **and bold**), which is a fuzzy-match
    // highlight, not a key binding.
    let chords = chars_matching(&model, 120, 30, dark.warn);
    assert!(
        chords.contains('a'),
        "expected message archive's chord `a`, styled `warn`, got {chords:?}: {}",
        screen(&model)
    );
}

#[test]
fn dark_theme_ask_pane_matches_the_historical_colors() {
    let dark = Theme::dark();
    let mut model = loaded();
    press(&mut model, Key::Char('A'));
    let generation = generation(&{
        type_in(&mut model, "who sent the invoice?");
        update(&mut model, Msg::Key(Key::Enter))
    });
    // Two citations, cursor left on the first (default `cursor: 0`) — this
    // checks the *second* citation's `[2]` label, so the cursor's own
    // `highlight_style` overlay is not what is being measured.
    for event in [
        AskEvent::Token("Alice did [1], cc Bob [2].".to_owned()),
        AskEvent::Cite(Box::new(Citation {
            label: 1,
            message_id: 10,
            subject: "Quarterly invoice".to_owned(),
            from_addr: "alice@example.com".to_owned(),
            mailbox: "INBOX".to_owned(),
            quote: "sending the invoice today".to_owned(),
        })),
        AskEvent::Cite(Box::new(Citation {
            label: 2,
            message_id: 11,
            subject: "Lunch?".to_owned(),
            from_addr: "bob@example.com".to_owned(),
            mailbox: "INBOX".to_owned(),
            quote: "I saw it too".to_owned(),
        })),
        AskEvent::Done {
            grounded: true,
            refusal: String::new(),
        },
    ] {
        update(&mut model, Msg::Ask { generation, event });
    }
    let citation_label = chars_matching(&model, 120, 30, dark.warn);
    assert!(citation_label.contains('['), "{}", screen(&model));
}

#[test]
fn dark_theme_outbox_state_colors_match_the_historical_colors() {
    let dark = Theme::dark();
    let mut model = loaded();
    press(&mut model, Key::Char('O'));
    update(
        &mut model,
        Msg::Outbox {
            now: 1_000,
            result: Ok(vec![
                OutboxRow {
                    id: 1,
                    to: "bob@example.com".to_owned(),
                    subject: "queued".to_owned(),
                    state: "scheduled".to_owned(),
                    send_at: 1_010,
                    undo_deadline: None,
                    last_error: None,
                },
                OutboxRow {
                    id: 2,
                    to: "carol@example.com".to_owned(),
                    subject: "went out".to_owned(),
                    state: "sent".to_owned(),
                    send_at: 900,
                    undo_deadline: None,
                    last_error: None,
                },
                OutboxRow {
                    id: 3,
                    to: "dave@example.com".to_owned(),
                    subject: "bounced".to_owned(),
                    state: "failed".to_owned(),
                    send_at: 800,
                    undo_deadline: None,
                    last_error: Some("550".to_owned()),
                },
            ]),
        },
    );
    // The cursor starts on row 0 ("scheduled"); checked with the cursor moved
    // to row 1 so `List::highlight_style` is not patched over the state this
    // assertion is about ("sent"/"failed" are unaffected either way, since
    // the cursor is never on them here).
    assert!(chars_matching(&model, 120, 30, dark.ok).contains("sent"));
    assert!(chars_matching(&model, 120, 30, dark.err).contains("failed"));
    press(&mut model, Key::Char('j'));
    assert!(
        chars_matching(&model, 120, 30, dark.warn).contains("scheduled"),
        "{}",
        screen(&model)
    );
}

#[test]
fn dark_theme_undo_toast_matches_the_historical_colors() {
    let dark = Theme::dark();
    let mut model = loaded();
    model.toasts = VecDeque::from([Toast::Undo(UndoToast {
        outbox_id: 1,
        to: "bob@example.com".to_owned(),
        deadline: 1_030,
        remaining: 30,
    })]);
    let band = chars_matching(&model, 120, 30, dark.toast);
    assert!(band.contains("bob@example.com"), "{}", screen(&model));
    let hint = chars_matching(&model, 120, 30, dark.warn);
    assert!(hint.contains("undoes"), "{}", screen(&model));
}

/// `Theme::mono` exists to prove every marker survives with no color at
/// all — this is the render-level half of `theme::tests`'s field-level
/// `mono_sets_no_foreground_or_background_anywhere`.
#[test]
fn mono_theme_still_shows_every_mail_marker_by_glyph_alone() {
    let mut model = loaded();
    model.theme = Theme::mono();
    let rendered = screen(&model);
    assert!(rendered.contains('●'), "unread glyph missing: {rendered}");
    assert!(rendered.contains('★'), "flagged glyph missing: {rendered}");
    assert!(
        rendered.contains('@'),
        "attachment glyph missing: {rendered}"
    );
}

#[test]
fn mono_theme_still_renders_without_panicking_for_every_overlay() {
    // Not a style assertion — `Theme::mono`'s fields are exercised above and
    // in `theme::tests`. This is the same "does it panic" backstop
    // `a_terminal_far_too_small_for_an_overlay_still_renders` runs for size;
    // here the axis is "every field present" rather than "every size".
    let mut model = loaded();
    model.theme = Theme::mono();
    for overlay in [
        Overlay::Help(Box::new(HelpPane::new(Mode::Normal, &model.keymap))),
        Overlay::Pick {
            what: PickFor::Copy,
            message_ids: vec![10],
            idx: 0,
        },
        Overlay::Confirm {
            prompt: "y/N".to_owned(),
            then: Confirmed::Delete(vec![10]),
        },
    ] {
        model.set_overlay(overlay);
        draw(&model, 120, 30);
    }
}

#[test]
fn a_theme_name_round_trips_through_a_loaded_model() {
    // `ThemeName` is not yet wired to a `:set theme` command (task 89), but
    // it is already the vocabulary the daemon-agnostic parts of that command
    // will resolve against, and this pins the one property that matters:
    // every built-in is reachable from its id.
    for name in ThemeName::ALL {
        let mut model = loaded();
        model.theme = name.resolve();
        // Only a panic-freedom check — the per-theme color values are
        // `theme::tests`'s job, not `view`'s.
        screen(&model);
    }
}

/// The whole point of task 87: after this refactor, nothing outside
/// `theme.rs` may name `ratatui::style::Color` directly. A call site that
/// reached past the token system back to a literal is exactly the drift this
/// module exists to prevent, and it is cheaper to catch here than to notice
/// on a light terminal.
#[test]
fn no_color_literal_escapes_the_theme_module() {
    for (path, source) in [
        ("view.rs", include_str!("../view.rs")),
        ("overlays.rs", include_str!("../overlays.rs")),
        // `manual.rs` is the same shape as `overlays.rs`: it holds what a run
        // *means* (`manual::Ink`) and never what it looks like, so the mapping
        // to a token stays in one place — `view::ink_style`.
        ("manual.rs", include_str!("../manual.rs")),
    ] {
        assert!(
            !source.contains("Color::"),
            "{path} names `Color::` directly — route it through `Theme` instead"
        );
    }
}

// ---------------------------------------------------------------------------
// the manual (task 103)
// ---------------------------------------------------------------------------

/// A model showing the manual, reached with the key that opens it.
fn manual_model() -> Model {
    let mut model = loaded();
    press(&mut model, Key::Char('K'));
    model
}

/// The manual is longer than a terminal, and every assertion below is about
/// something further down the page than a 30-row frame reaches. Tall enough
/// for the whole front page, so nothing here depends on where the viewport
/// happens to have scrolled to.
const TALL: u16 = 120;

fn tall(model: &Model) -> String {
    draw(model, 120, TALL).join("\n")
}

#[test]
fn the_manual_takes_the_whole_screen_and_names_the_page_it_is_on() {
    let mut model = manual_model();
    model.ai_panel = true; // the panel is about a message; the manual is not
    let rendered = tall(&model);
    assert!(rendered.contains("manual · Start here"), "{rendered}");
    assert!(
        !rendered.contains("\\ hides"),
        "the AI panel's own chrome is absent — it is about a message, and the \
         manual is not: {rendered}"
    );
    assert!(
        !rendered.contains("Quarterly invoice"),
        "and the message list is not behind it: {rendered}"
    );
}

#[test]
fn the_manual_renders_the_chords_that_are_actually_bound() {
    // The same property the `?` overlay has, reached through a different path:
    // the page says `{{keys:message.archive}}` and the renderer resolves it
    // against the keymap in force.
    let mut model = manual_model();
    assert!(
        !tall(&model).contains("Z / a"),
        "not yet — nothing is bound to Z"
    );

    model.keymap =
        crate::keymap::file::parse("[normal]\nZ = \"message.archive\"\n", "keys.toml").unwrap();
    let rebound = tall(&model);
    assert!(
        rebound.contains("Z / a"),
        "a rebind reaches the manual with no page edited:\n{rebound}"
    );
}

/// Move the row cursor down until it is on a followable row.
fn walk_to_a_link(model: &mut Model) {
    for _ in 0..400 {
        let state = model.manual.as_ref().expect("the manual is open");
        if manual::doc(&state.at, &model.keymap)
            .lines
            .get(state.cursor)
            .and_then(manual::DocLine::link)
            .is_some()
        {
            return;
        }
        press(model, Key::Char('j'));
    }
    panic!("no row on this page carries a link");
}

#[test]
fn a_generated_reference_page_renders_its_table() {
    let mut model = manual_model();
    crate::tui::model::open_manual_at(&mut model, "modes");
    let rendered = screen(&model);
    assert!(rendered.contains("normal"), "{rendered}");
    assert!(
        rendered.contains("→"),
        "the layer chain is drawn: {rendered}"
    );
}

#[test]
fn the_manual_search_line_shows_what_is_being_typed_and_which_scope() {
    let mut model = manual_model();
    press(&mut model, Key::Char('/'));
    for c in "invoice".chars() {
        press(&mut model, Key::Char(c));
    }
    let rendered = screen(&model);
    assert!(rendered.contains("/invoice"), "{rendered}");

    let mut model = manual_model();
    press(&mut model, Key::Char('g'));
    press(&mut model, Key::Char('/'));
    for c in "invoice".chars() {
        press(&mut model, Key::Char(c));
    }
    assert!(screen(&model).contains("g/invoice"));
}

#[test]
fn a_matched_word_in_the_manual_is_styled_and_nothing_is_spliced_into_the_text() {
    let mut model = manual_model();
    press(&mut model, Key::Char('/'));
    for c in "archive".chars() {
        press(&mut model, Key::Char(c));
    }
    press(&mut model, Key::Enter);

    // Read off a row the cursor is *not* on. `List::highlight_style` patches
    // `sel_focus` — which sets an explicit `fg` — over every span of the
    // selected row, so on that one row a `Chord`, a link, a `Match` and a
    // `Broken` marker all render identically. Submitting a search lands the
    // cursor on the first match, so that match is precisely the one whose own
    // ink is overridden; asserting without moving off it would prove nothing
    // about the styling. (`chars_matching`'s own docs make the same point for
    // the message list's unread rows.)
    press(&mut model, Key::Char('G'));
    let matched = chars_matching(&model, 120, TALL, model.theme.match_hl);
    assert!(
        matched.to_lowercase().contains("archive"),
        "the match is the styled run, got {matched:?}"
    );
    // The highlight is *style*, never markup spliced into the text — splicing
    // is what re-introduces the escaping bugs offsets exist to avoid — and it
    // splits runs rather than lines, so not one character moves. Compared
    // against the same frame with the highlight dropped rather than against
    // `before`: submitting a search also *moves the cursor to the match*,
    // which is a change to the frame that has nothing to do with styling.
    let highlighted = tall(&model);
    model.manual.as_mut().expect("the manual is open").highlight = None;
    assert_eq!(
        highlighted,
        tall(&model),
        "highlighting changed what the page says, not just how it looks"
    );
    assert!(
        !highlighted.contains('*') && !highlighted.contains("[["),
        "no markup was spliced in: {highlighted}"
    );
}

#[test]
fn the_pane_title_says_when_there_is_somewhere_to_go_back_to() {
    let mut model = manual_model();
    // Row 0 is the pane's top border, which carries the title. Asserted on
    // that row alone rather than on the whole frame: the status line names
    // `<c-o>` too, and a whole-frame match would pass without the title
    // saying anything.
    assert!(!draw(&model, 120, TALL)[0].contains("<c-o> back"));
    walk_to_a_link(&mut model);
    press(&mut model, Key::Enter);
    let title = draw(&model, 120, TALL)[0].clone();
    assert!(
        title.contains("<c-o> back"),
        "following a link says so in the title: {title:?}"
    );
}

#[test]
fn every_built_in_theme_draws_the_manual() {
    for name in ThemeName::ALL {
        let mut model = manual_model();
        model.theme = name.resolve();
        press(&mut model, Key::Char('/'));
        for c in "the".chars() {
            press(&mut model, Key::Char(c));
        }
        press(&mut model, Key::Enter);
        // Panic-freedom across every page, prompt state and theme; the
        // per-theme values are `theme::tests`' job.
        for anchor in manual::PAGES.iter().map(|page| page.anchor) {
            crate::tui::model::open_manual_at(&mut model, anchor);
            draw(&model, 120, 44);
            draw(&model, 40, 10);
        }
    }
}

// ---------------------------------------------------------------------------
// responsive layout and the toast queue (task 93)
// ---------------------------------------------------------------------------

#[test]
fn three_panes_render_at_and_above_the_preview_breakpoint() {
    let model = loaded();
    for width in [100, 120] {
        let rendered = draw(&model, width, 30).join("\n");
        assert!(rendered.contains("folders"), "width {width}: {rendered}");
        assert!(rendered.contains("preview"), "width {width}: {rendered}");
    }
}

#[test]
fn the_preview_column_drops_just_below_its_own_breakpoint() {
    let model = loaded();
    for width in [99, 60] {
        let rendered = draw(&model, width, 30).join("\n");
        assert!(rendered.contains("folders"), "width {width}: {rendered}");
        assert!(!rendered.contains("preview"), "width {width}: {rendered}");
    }
}

#[test]
fn the_folder_column_drops_below_its_own_breakpoint_leaving_messages_alone() {
    let model = loaded();
    let rendered = draw(&model, 59, 30).join("\n");
    assert!(!rendered.contains("folders"), "{rendered}");
    assert!(!rendered.contains("preview"), "{rendered}");
    assert!(
        rendered.contains("Quarterly invoice"),
        "the message list is still there: {rendered}"
    );
}

#[test]
fn tab_and_h_still_move_focus_with_the_folder_column_off_screen() {
    // `render` never special-cases a narrow terminal for focus — it is the
    // same `Focus::Folders`/`Focus::Messages` toggle either way. Proof it
    // does not panic or wedge with the column not on screen at all.
    let mut model = loaded();
    press(&mut model, Key::Char('h'));
    assert_eq!(model.focus, Focus::Folders);
    draw(&model, 50, 30);
    press(&mut model, Key::Tab);
    assert_eq!(
        model.focus,
        Focus::Messages,
        "<tab> toggles too, not just l"
    );
    draw(&model, 50, 30);
    press(&mut model, Key::Tab);
    assert_eq!(model.focus, Focus::Folders);
    press(&mut model, Key::Char('l'));
    assert_eq!(model.focus, Focus::Messages);
    draw(&model, 50, 30);
}

#[test]
fn the_status_line_names_focus_only_when_folders_are_off_screen_and_focused() {
    let mut model = loaded();
    press(&mut model, Key::Char('h'));
    assert_eq!(model.focus, Focus::Folders);

    // Wide enough that folders are drawn: the border highlight already says
    // where focus is, so the status line stays quiet.
    assert!(
        !draw(&model, 120, 30).join("\n").contains("focus: folders"),
        "the folder pane is on screen and shows its own focus"
    );
    // Narrow enough to drop the column: nothing on screen shows focus
    // without this.
    let narrow = draw(&model, 50, 30).join("\n");
    assert!(
        narrow.contains("focus: folders"),
        "the only pane drawn is unfocused and says nothing about why: {narrow}"
    );

    // Focus on messages needs no hint at any width — the one pane drawn is
    // exactly the one with the cursor.
    press(&mut model, Key::Char('l'));
    assert!(!draw(&model, 50, 30).join("\n").contains("focus: folders"));
}

#[test]
fn the_focus_hint_survives_a_long_status_line() {
    // ` ap` both opens the AI panel and installs a 63-column status
    // ("AI panel — cached analysis only; `.` offers the calls that cost").
    // At 59 columns that status alone overruns the row; the hint must still
    // reach the screen rather than being truncated off the right edge of an
    // unbounded, unwrapped status span.
    let mut model = loaded();
    press(&mut model, Key::Char(' '));
    press(&mut model, Key::Char('a'));
    press(&mut model, Key::Char('p'));
    press(&mut model, Key::Char('h'));
    assert_eq!(model.focus, Focus::Folders);
    assert!(model.ai_panel);
    assert!(
        model.status.len() > 59,
        "the repro needs a status that alone overruns the row: {:?}",
        model.status
    );
    let rendered = draw(&model, 59, 30).join("\n");
    assert!(
        rendered.contains("focus: folders"),
        "the hint has its own reserved column, not whatever status leaves over: {rendered}"
    );
}

#[test]
fn the_focus_hint_accounts_for_the_open_ai_panel_when_measuring_width() {
    // `panes_width` must subtract the AI panel's share before comparing
    // against `FOLDER_BREAKPOINT` — a terminal wide enough on its own, but
    // not once the open panel's column is taken out, is exactly the case
    // `render_panes` collapses to 1-pane and this hint exists for.
    let mut model = loaded();
    model.ai_panel = true;
    model.ai_panel_width_pct = 30;
    press(&mut model, Key::Char('h'));
    assert_eq!(model.focus, Focus::Folders);

    // 100 columns: 70% for the panes (70) is still >= FOLDER_BREAKPOINT
    // (60), so folders are drawn and the hint stays quiet.
    assert!(
        !draw(&model, 100, 30).join("\n").contains("focus: folders"),
        "70 columns for the panes still fits the folder column"
    );
    // 80 columns: 70% (56) is now under FOLDER_BREAKPOINT even though 80 on
    // its own is not. Proves the panel's share was actually subtracted.
    let narrowed_by_panel = draw(&model, 80, 30).join("\n");
    assert!(
        narrowed_by_panel.contains("focus: folders"),
        "56 columns for the panes should read as narrow, even though the \
         terminal itself is 80: {narrowed_by_panel}"
    );
}

#[test]
fn the_focus_hint_is_eligible_while_a_card_is_zoomed_even_at_a_width_that_would_show_folders() {
    // 120 columns is wide enough that folders are normally drawn (see
    // `the_status_line_names_focus_only_when_folders_are_off_screen_and_focused`)
    // — but a zoomed card covers the whole deck regardless of width, so the
    // folder pane is exactly as invisible as it is at a narrow terminal, and
    // the hint that exists for that case must fire here too.
    let mut model = loaded();
    press(&mut model, Key::Char('h'));
    assert_eq!(model.focus, Focus::Folders);
    assert!(
        !draw(&model, 120, 30).join("\n").contains("focus: folders"),
        "sanity: folders are drawn and the hint is quiet before any zoom"
    );

    press(&mut model, Key::Char('Z'));
    let rendered = draw(&model, 120, 30).join("\n");
    assert!(
        rendered.contains("focus: folders"),
        "the zoomed card hides folders exactly as a narrow terminal would: {rendered}"
    );
}

#[test]
fn the_toast_row_shows_one_entry_and_a_badge_for_the_rest() {
    let mut model = loaded();
    model.toasts = VecDeque::from([
        Toast::Priority {
            text: "3 messages need a reply".to_owned(),
        },
        Toast::Completion {
            text: "reindex complete".to_owned(),
        },
    ]);
    let rendered = screen(&model);
    assert!(
        rendered.contains("3 messages need a reply"),
        "the ranked-highest toast is shown: {rendered}"
    );
    assert!(
        !rendered.contains("reindex complete"),
        "only the shown toast draws: {rendered}"
    );
    assert!(rendered.contains("+1"), "the rest are a badge: {rendered}");
}

#[test]
fn a_priority_toast_is_shown_over_a_completion_regardless_of_push_order() {
    // The mirror of the test above: `Completion` pushed *after* `Priority`
    // this time, so a selection that just picked the front of the queue
    // would show the wrong one here even though it happened to pass above.
    let mut model = loaded();
    model.toasts = VecDeque::from([
        Toast::Completion {
            text: "reindex complete".to_owned(),
        },
        Toast::Priority {
            text: "3 messages need a reply".to_owned(),
        },
    ]);
    let rendered = screen(&model);
    assert!(
        rendered.contains("3 messages need a reply"),
        "priority is ranked to interrupt, regardless of arrival order: {rendered}"
    );
    assert!(!rendered.contains("reindex complete"), "{rendered}");
}

#[test]
fn the_newest_completion_is_shown_not_the_oldest() {
    let mut model = loaded();
    model.toasts = VecDeque::from([
        Toast::Completion {
            text: "export finished".to_owned(),
        },
        Toast::Completion {
            text: "reindex complete".to_owned(),
        },
    ]);
    let rendered = screen(&model);
    assert!(
        rendered.contains("reindex complete"),
        "the most recent notice is the useful one to show: {rendered}"
    );
    assert!(!rendered.contains("export finished"), "{rendered}");
}

#[test]
fn an_undo_toast_is_always_shown_first_even_queued_behind_others() {
    let mut model = loaded();
    model.toasts = VecDeque::from([
        Toast::Completion {
            text: "reindex complete".to_owned(),
        },
        Toast::Undo(UndoToast {
            outbox_id: 1,
            to: "bob@example.com".to_owned(),
            deadline: 1_030,
            remaining: 30,
        }),
    ]);
    let rendered = screen(&model);
    assert!(
        rendered.contains("bob@example.com"),
        "the undo offer is what a person needs to see: {rendered}"
    );
    assert!(rendered.contains("+1"), "{rendered}");
}

#[test]
fn the_ai_panel_header_shows_when_the_summary_is_pinned() {
    let mut model = loaded();
    assert!(!screen(&model).contains("AI ·"), "not open yet");

    // Opened by ` ap`, following the cursor: not pinned, and the header must
    // not claim otherwise.
    press(&mut model, Key::Char(' '));
    press(&mut model, Key::Char('a'));
    press(&mut model, Key::Char('p'));
    let unpinned = screen(&model);
    assert!(unpinned.contains("AI ·"), "the panel is open: {unpinned}");
    assert!(
        !unpinned.contains("pinned"),
        "following the cursor is not pinned: {unpinned}"
    );

    // Opened by the . menu, on a specific message: pinned.
    press(&mut model, Key::Char('.'));
    press(&mut model, Key::Enter);
    let pinned = screen(&model);
    assert!(
        pinned.contains("pinned"),
        "the panel header names the pin: {pinned}"
    );
}

#[test]
fn set_folder_width_actually_moves_the_message_columns_left_edge() {
    // Row 0 specifically, not the whole screen: the folders *list* can
    // itself contain "INBOX" as a row's text once the cursor scrolls, which
    // would make a whole-screen search ambiguous about which column it
    // found. Row 0 is border/title only, and the message pane's title is
    // the open folder's name (see `render_messages`), so this is
    // unambiguously that pane's left edge.
    let narrow = {
        let mut model = loaded();
        model.folder_width_pct = 10;
        model
    };
    let wide = {
        let mut model = loaded();
        model.folder_width_pct = 50;
        model
    };
    let narrow_x = draw(&narrow, 120, 30)[0]
        .find("INBOX")
        .unwrap_or_else(|| panic!("no INBOX title: {:?}", draw(&narrow, 120, 30)));
    let wide_x = draw(&wide, 120, 30)[0]
        .find("INBOX")
        .unwrap_or_else(|| panic!("no INBOX title: {:?}", draw(&wide, 120, 30)));
    assert!(
        wide_x > narrow_x,
        "a wider folder column should push the message list right: {narrow_x} vs {wide_x}"
    );
}

#[test]
fn set_preview_width_actually_moves_the_preview_columns_left_edge() {
    let narrow = {
        let mut model = loaded();
        model.preview_width_pct = 15;
        model
    };
    let wide = {
        let mut model = loaded();
        model.preview_width_pct = 55;
        model
    };
    let narrow_x = draw(&narrow, 120, 30)[0]
        .find("preview")
        .unwrap_or_else(|| panic!("no preview title: {:?}", draw(&narrow, 120, 30)));
    let wide_x = draw(&wide, 120, 30)[0]
        .find("preview")
        .unwrap_or_else(|| panic!("no preview title: {:?}", draw(&wide, 120, 30)));
    assert!(
        wide_x < narrow_x,
        "a wider preview column should push its own left edge left: {narrow_x} vs {wide_x}"
    );
}

#[test]
fn set_ai_panel_width_actually_moves_the_panels_left_edge() {
    let narrow = {
        let mut model = loaded();
        model.ai_panel = true;
        model.ai_panel_width_pct = 15;
        model
    };
    let wide = {
        let mut model = loaded();
        model.ai_panel = true;
        model.ai_panel_width_pct = 60;
        model
    };
    let narrow_x = draw(&narrow, 120, 30)[0]
        .find("AI ·")
        .unwrap_or_else(|| panic!("no AI panel title: {:?}", draw(&narrow, 120, 30)));
    let wide_x = draw(&wide, 120, 30)[0]
        .find("AI ·")
        .unwrap_or_else(|| panic!("no AI panel title: {:?}", draw(&wide, 120, 30)));
    assert!(
        wide_x < narrow_x,
        "a wider AI panel should push its own left edge left: {narrow_x} vs {wide_x}"
    );
}

// ---------------------------------------------------------------------------
// overlay stack z-order (task 108: rendering half — the mechanism itself is
// `tui::overlays::tests`; this is the one module allowed to call `render`)
// ---------------------------------------------------------------------------

#[test]
fn a_stacked_overlay_renders_over_the_one_underneath_without_erasing_it() {
    // tui.md §2.2.2's own example, drawn: confirm over the quick menu. Both
    // must be visible at once — the confirm as a small floating dialog, the
    // quick menu still readable around it — which is what "painted over, not
    // replaced" (§4.4) means for something the eye can check directly.
    let mut model = loaded();
    model.push_overlay(Overlay::Quick(QuickPane {
        message_id: 10,
        subject: "Q3 invoice".to_owned(),
        cursor: 0,
    }));
    model.push_overlay(Overlay::Confirm {
        prompt: "really? [y/N]".to_owned(),
        then: Confirmed::Delete(vec![10]),
    });

    let rendered = screen(&model);
    assert!(
        rendered.contains("really?"),
        "the topmost overlay (confirm) must be visible:\n{rendered}"
    );
    assert!(
        rendered.contains("Summarize") || rendered.contains("Q3 invoice"),
        "the overlay underneath must still be visible around the confirm's \
         small dialog box, not erased by it:\n{rendered}"
    );
}

#[test]
fn overlay_stack_render_order_is_back_to_front_by_index() {
    // Help clamps to 84%×80% of a 120×30 area — roughly (10,3) to (100,24) —
    // and Finder to 76%×70% — roughly (14,4) to (91,21) — so Finder's whole
    // footprint sits inside Help's. Pushing Finder last must make its title
    // the one actually on screen in that overlap, proving the render loop
    // walks the stack forward (index 0 first, the top drawn last, so it
    // wins any overlap) rather than in reverse or some other order. A
    // weaker assertion here (e.g. matching a glyph Help's own chrome also
    // contains) would pass whether or not the order is right — this one
    // does not: `render_help`'s footer contains no "find —" substring at
    // all, so its presence is real signal, not a coincidence of a shared
    // character.
    let mut model = loaded();
    model.push_overlay(Overlay::Help(Box::new(HelpPane::new(
        Mode::Normal,
        &model.keymap,
    ))));
    let with_help_only = screen(&model);
    assert!(with_help_only.contains("keys —"), "{with_help_only}");
    assert!(
        !with_help_only.contains("find —"),
        "sanity: Help alone must not already contain the finder's title"
    );

    model.push_overlay(Overlay::Finder(Box::default()));
    let with_finder_on_top = screen(&model);
    assert!(
        with_finder_on_top.contains("find —"),
        "the finder, pushed last, must be what actually renders on top:\n{with_finder_on_top}"
    );
}
