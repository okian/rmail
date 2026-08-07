//! Render smoke tests, against ratatui's `TestBackend` — a real render into
//! a real buffer, with no terminal involved, which is what lets these run in
//! the container the gate uses.

use ratatui::backend::TestBackend;
use ratatui::Terminal;

use super::*;
use crate::tui::model::{Account, Folder, InputFor, MessageRow, OpenMessage, PickFor};

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
fn the_help_overlay_lists_every_binding_the_task_promises() {
    let mut model = loaded();
    model.overlay = Some(Overlay::Help);
    let rendered = draw(&model, 120, 40).join("\n");

    for expected in [
        "j / k", "gg / G", "Enter", "q", "?", "a", "d", "s", "f", "r / F", "o",
    ] {
        assert!(rendered.contains(expected), "help is missing {expected:?}");
    }
}

#[test]
fn the_folder_picker_overlay_lists_the_folders() {
    let mut model = loaded();
    model.overlay = Some(Overlay::Pick {
        what: PickFor::Move,
        message_id: 10,
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
        message_id: 10,
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
        message_id: 10,
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
