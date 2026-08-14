//! Drawing — a pure function of `&Model`, and the only module that knows
//! ratatui exists.
//!
//! [`render`] takes an immutable model and a frame. It cannot change state,
//! cannot issue a request, and cannot decide anything the model has not
//! already decided; every question it might want to ask ("which row is
//! selected", "is an overlay up") is answered by a field. That is what keeps
//! the state machine in `tui::model` genuinely authoritative rather than
//! merely nominally so — behaviour that leaked in here would be behaviour no
//! `tui::model` test could reach.
//!
//! The layout is prd.md's: folders on the left, the message list in the
//! middle, the preview on the right, a status line underneath. Opening a
//! message (`Enter`) replaces all three with a full-width viewer, because a
//! third of an 80-column terminal is not enough to read mail in.

#[cfg(test)]
mod tests;

use chrono::{DateTime, Local, Utc};
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use crate::keymap::{Action, Mode};

use super::model::{Focus, Level, Model, Overlay, Screen, FLAGGED, SEEN};

/// Draw one frame.
pub fn render(model: &Model, frame: &mut Frame) {
    let area = frame.area();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);

    match model.screen {
        Screen::List => render_panes(model, frame, rows[0]),
        Screen::Viewer => render_viewer(model, frame, rows[0]),
    }
    render_status(model, frame, rows[1]);

    match &model.overlay {
        Some(Overlay::Help) => render_help(model, frame, area),
        Some(Overlay::Pick { what, idx, .. }) => render_pick(model, frame, area, *what, *idx),
        Some(Overlay::Confirm { prompt, .. }) => render_modal(frame, area, "confirm", prompt),
        Some(Overlay::Input { prompt, buffer, .. }) => {
            render_modal(frame, area, prompt, &format!("{buffer}▏"));
        }
        None => {}
    }
}

fn render_panes(model: &Model, frame: &mut Frame, area: Rect) {
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(20),
            Constraint::Percentage(40),
            Constraint::Percentage(40),
        ])
        .split(area);

    render_folders(model, frame, columns[0]);
    render_messages(model, frame, columns[1]);
    render_preview(model, frame, columns[2]);
}

fn render_folders(model: &Model, frame: &mut Frame, area: Rect) {
    let items: Vec<ListItem> = model
        .folders
        .iter()
        .map(|folder| ListItem::new(format!("{:<16}{:>5}", folder.name, folder.message_count)))
        .collect();
    let mut state = ListState::default();
    state.select(if model.folders.is_empty() {
        None
    } else {
        Some(model.folder_idx)
    });

    let list = List::new(items)
        .block(pane("folders", model.focus == Focus::Folders))
        .highlight_style(selected_style(model.focus == Focus::Folders));
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_messages(model: &Model, frame: &mut Frame, area: Rect) {
    let items: Vec<ListItem> = model
        .messages
        .iter()
        .enumerate()
        .map(|(idx, row)| {
            let unread = !row.has_flag(SEEN);
            let marks = format!(
                "{}{}{}",
                if unread { '●' } else { ' ' },
                if row.has_flag(FLAGGED) { '★' } else { ' ' },
                if row.has_attachments { '@' } else { ' ' },
            );
            let line = Line::from(vec![
                Span::styled(marks, Style::default().fg(Color::Yellow)),
                Span::styled(
                    format!(" {} ", short_date(row.date)),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw(format!("{:<20.20} {}", row.from, row.subject)),
            ]);
            let mut style = Style::default();
            if unread {
                style = style.add_modifier(Modifier::BOLD);
            }
            // A visual selection has to be visible on every row it covers,
            // not only the one under the cursor: a bulk archive that acted on
            // rows the user could not see would be indistinguishable from a
            // bug in the selection arithmetic.
            if model.is_selected(idx) {
                style = style.bg(Color::DarkGray).fg(Color::White);
            }
            ListItem::new(line).style(style)
        })
        .collect();

    let mut state = ListState::default();
    state.select(if model.messages.is_empty() {
        None
    } else {
        Some(model.message_idx)
    });

    let title = model
        .current_folder()
        .map_or_else(|| "messages".to_owned(), |f| f.name.clone());
    let list = List::new(items)
        .block(pane(&title, model.focus == Focus::Messages))
        .highlight_style(selected_style(model.focus == Focus::Messages));
    frame.render_stateful_widget(list, area, &mut state);
}

/// The preview pane: headers of the highlighted row, without fetching a body.
///
/// A list view must not pull a body across the wire per row (see
/// `MailService.List`'s own comment), so the preview shows what the listing
/// already carries and invites `Enter` for the rest.
fn render_preview(model: &Model, frame: &mut Frame, area: Rect) {
    let lines: Vec<Line> = match model.current_message() {
        Some(row) => vec![
            header_line("From", &row.from),
            header_line("Date", &short_date(row.date)),
            header_line("Subject", &row.subject),
            header_line(
                "Flags",
                &if row.flags.is_empty() {
                    "(none)".to_owned()
                } else {
                    row.flags.join(" ")
                },
            ),
            Line::raw(""),
            Line::raw("Enter to open · r reply · a archive · ? help"),
        ],
        None => vec![Line::raw("no message selected")],
    };
    let paragraph = Paragraph::new(lines)
        .block(pane("preview", false))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn render_viewer(model: &Model, frame: &mut Frame, area: Rect) {
    let Some(open) = model.open.as_ref() else {
        frame.render_widget(
            Paragraph::new("nothing open").block(pane("message", true)),
            area,
        );
        return;
    };

    let mut lines: Vec<Line> = open
        .headers
        .iter()
        .map(|(name, value)| header_line(name, value))
        .collect();
    if !open.attachments.is_empty() {
        lines.push(header_line("Attachments", &open.attachments.join(", ")));
    }
    if open.has_html {
        lines.push(Line::styled(
            "  [HTML alternative available — press o to open it in a browser]",
            Style::default().fg(Color::Cyan),
        ));
    }
    lines.push(Line::raw(""));

    // Scrolling by *slicing the body* rather than by `Paragraph::scroll`.
    //
    // `Paragraph::scroll` counts rendered lines, and this paragraph wraps
    // (`Wrap`) — so on any message with lines longer than the pane, a scroll
    // offset the model computed from `body.len()` addresses the wrong place,
    // and `G` cannot reach the end at all because the model's clamp is a
    // count of logical lines while the widget wants visual ones. Slicing
    // keeps `Model::scroll` a logical line index, exactly what `j`/`k`/`gg`/`G`
    // manipulate, with no second coordinate system to keep in step. The
    // headers stay pinned above as a side benefit.
    let from = model.scroll.min(open.body.len().saturating_sub(1));
    lines.extend(open.body[from..].iter().map(|line| Line::raw(line.clone())));

    let paragraph = Paragraph::new(lines)
        .block(pane("message", true))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn render_status(model: &Model, frame: &mut Frame, area: Rect) {
    let color = match model.level {
        Level::Info => Color::Green,
        Level::Error => Color::Red,
    };
    // The busy marker is the whole point of tracking `inflight`: the user can
    // see that something is in flight *and* keep using the UI while it is.
    let busy = if model.inflight > 0 {
        format!(" [{} in flight]", model.inflight)
    } else {
        String::new()
    };
    // The mode, and what has been typed towards a binding but not resolved.
    // vim shows both for the same reason: a half-typed `3g` that is invisible
    // is indistinguishable from a keyboard that has stopped responding, and
    // the user's next move — mashing keys — is the one thing that makes it
    // worse.
    let mode = match model.mode() {
        Mode::Visual => " -- VISUAL --",
        Mode::Insert => " -- INSERT --",
        _ => "",
    };
    let pending = if model.pending.is_empty() {
        String::new()
    } else {
        format!(" {}", model.pending.label())
    };
    let line = Line::from(vec![
        Span::styled(model.status.clone(), Style::default().fg(color)),
        Span::styled(busy, Style::default().fg(Color::DarkGray)),
        Span::styled(
            mode,
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(pending, Style::default().fg(Color::Yellow)),
    ]);
    frame.render_widget(Paragraph::new(line), area);
}

/// The key reference, read out of the keymap rather than written alongside it.
///
/// Task 83 kept this as a hand-maintained table, which was right while the
/// bindings were a `match` nobody could change. Once `keys.toml` can rebind
/// anything, a hand-maintained list is a list that lies to exactly the users
/// who customised something — so the rows are generated from the bindings in
/// force, and a rebind shows up here the moment it is loaded.
///
/// Normal mode's chain, because that is the screen `?` is pressed from; the
/// viewer and the overlays inherit or restate it.
fn render_help(model: &Model, frame: &mut Frame, area: Rect) {
    let mut lines: Vec<Line> = Vec::new();
    for action in Action::ALL {
        let chords = model.keymap.chords_for(Mode::Normal, *action);
        if chords.is_empty() {
            continue;
        }
        let keys = chords
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" / ");
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {keys:<14}"),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(action.describe()),
        ]));
    }
    lines.push(Line::styled(
        "  rebind with `mail keys set <chord> <action>` — no restart needed",
        Style::default().fg(Color::DarkGray),
    ));

    let area = centered(area, 72, u16::try_from(lines.len()).unwrap_or(u16::MAX) + 2);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(pane("keys — any of q, Esc or ? closes", true)),
        area,
    );
}

fn render_pick(
    model: &Model,
    frame: &mut Frame,
    area: Rect,
    what: super::model::PickFor,
    idx: usize,
) {
    let items: Vec<ListItem> = model
        .folders
        .iter()
        .map(|folder| ListItem::new(folder.name.clone()))
        .collect();
    let mut state = ListState::default();
    state.select(Some(idx.min(model.folders.len().saturating_sub(1))));

    let title = match what {
        super::model::PickFor::Copy => "copy to which folder?",
        super::model::PickFor::Move => "move to which folder?",
    };
    let height = u16::try_from(model.folders.len().min(12)).unwrap_or(12) + 2;
    let area = centered(area, 40, height);
    frame.render_widget(Clear, area);
    let list = List::new(items)
        .block(pane(title, true))
        .highlight_style(selected_style(true));
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_modal(frame: &mut Frame, area: Rect, title: &str, body: &str) {
    let area = centered(area, 60, 3);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(body.to_owned()).block(pane(title, true)),
        area,
    );
}

/// A message's date the way a mail client shows one: fixed width, local
/// zone (mail is read where the reader is, not where the sender was), and a
/// placeholder of the same width when the message carries no usable date, so
/// one undated message cannot shear the whole column.
fn short_date(unix_seconds: Option<i64>) -> String {
    const WIDTH: usize = "01 Jan 00:00".len();
    unix_seconds
        .and_then(|seconds| DateTime::<Utc>::from_timestamp(seconds, 0))
        .map_or_else(
            || " ".repeat(WIDTH),
            |at| at.with_timezone(&Local).format("%d %b %H:%M").to_string(),
        )
}

fn header_line<'a>(name: &str, value: &str) -> Line<'a> {
    Line::from(vec![
        Span::styled(
            format!("{name}: "),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(value.to_owned()),
    ])
}

fn pane<'a>(title: &str, focused: bool) -> Block<'a> {
    let style = if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    Block::default()
        .borders(Borders::ALL)
        .border_style(style)
        .title(title.to_owned())
}

fn selected_style(focused: bool) -> Style {
    if focused {
        Style::default()
            .bg(Color::Blue)
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().add_modifier(Modifier::REVERSED)
    }
}

/// A `width` x `height` rectangle in the middle of `area`, clamped so an
/// overlay bigger than the terminal is truncated rather than panicking inside
/// ratatui's layout arithmetic.
fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}
