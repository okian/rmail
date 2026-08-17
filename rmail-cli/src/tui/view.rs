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
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};
use ratatui::Frame;

use crate::keymap::{Action, Mode};

use super::model::{Focus, Level, Model, Overlay, Screen, FLAGGED, SEEN};
use super::overlays::{
    self, AskPane, AskPhase, FinderPane, OutboxPane, PalettePane, QuickAction, QuickPane,
    SearchFocus, SearchPane, UndoToast,
};
use super::theme::Theme;

/// Draw one frame.
pub fn render(model: &Model, frame: &mut Frame) {
    let area = frame.area();
    // The toast gets a row of its own rather than sharing the status line: it
    // is a countdown with an offer attached, and an offer that scrolls away
    // behind the next "3 messages" is not an offer.
    let toast_height = u16::from(model.toast.is_some());
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(toast_height),
            Constraint::Length(1),
        ])
        .split(area);

    match model.screen {
        Screen::List => render_panes(model, frame, rows[0]),
        Screen::Viewer => render_main_with_panel(model, frame, rows[0], render_viewer),
    }
    if let Some(toast) = model.toast.as_ref() {
        render_toast(&model.theme, toast, frame, rows[1]);
    }
    render_status(model, frame, rows[2]);

    match &model.overlay {
        Some(Overlay::Help) => render_help(model, frame, area),
        Some(Overlay::Pick { what, idx, .. }) => render_pick(model, frame, area, *what, *idx),
        Some(Overlay::Confirm { prompt, .. }) => {
            render_modal(&model.theme, frame, area, "confirm", prompt);
        }
        Some(Overlay::Input { prompt, buffer, .. }) => {
            render_modal(&model.theme, frame, area, prompt, &format!("{buffer}▏"));
        }
        Some(Overlay::Search(pane)) => render_search(&model.theme, pane, frame, area),
        Some(Overlay::Finder(pane)) => render_finder(&model.theme, pane, frame, area),
        Some(Overlay::Palette(pane)) => render_palette(&model.theme, pane, frame, area),
        Some(Overlay::Ask(pane)) => render_ask(&model.theme, pane, frame, area),
        Some(Overlay::Outbox(pane)) => render_outbox(&model.theme, pane, frame, area),
        Some(Overlay::Quick(pane)) => render_quick(&model.theme, pane, frame, area),
        None => {}
    }
}

fn render_panes(model: &Model, frame: &mut Frame, area: Rect) {
    render_main_with_panel(model, frame, area, |model, frame, area| {
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
    });
}

/// Draw `main` in `area`, giving the collapsible AI panel a column of its own
/// when it is open.
///
/// A column rather than an overlay, because the panel is *about* what the
/// cursor is on: covering the message to explain the message would be an odd
/// thing to do, and the panel has to stay useful while the list is navigated
/// underneath it.
fn render_main_with_panel<F>(model: &Model, frame: &mut Frame, area: Rect, main: F)
where
    F: FnOnce(&Model, &mut Frame, Rect),
{
    if !model.ai_panel {
        main(model, frame, area);
        return;
    }
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(70), Constraint::Percentage(30)])
        .split(area);
    main(model, frame, columns[0]);
    render_ai_panel(model, frame, columns[1]);
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
        .block(pane_block(
            &model.theme,
            "folders",
            model.focus == Focus::Folders,
        ))
        .highlight_style(selected_style(&model.theme, model.focus == Focus::Folders));
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_messages(model: &Model, frame: &mut Frame, area: Rect) {
    let theme = &model.theme;
    let items: Vec<ListItem> = model
        .messages
        .iter()
        .enumerate()
        .map(|(idx, row)| {
            let unread = !row.has_flag(SEEN);
            let flagged = row.has_flag(FLAGGED);
            // Each glyph carries its own token rather than one style over the
            // whole run: today all three resolve to the same color, but the
            // glyphs (`●`/`★`/`@`) already carry the meaning on their own —
            // see `Theme::mono`, where they carry it *alone*.
            let line = Line::from(vec![
                Span::styled(if unread { "●" } else { " " }, theme.unread),
                Span::styled(if flagged { "★" } else { " " }, theme.flagged),
                Span::styled(
                    if row.has_attachments { "@" } else { " " },
                    theme.attachment,
                ),
                Span::styled(format!(" {} ", short_date(row.date)), theme.muted),
                Span::raw(format!("{:<20.20} {}", row.from, row.subject)),
            ]);
            let mut style = Style::default();
            if unread {
                style = style.add_modifier(Modifier::BOLD);
            }
            // A visual selection has to be visible on every row it covers,
            // not only the one under the cursor: a bulk archive that acted on
            // rows the user could not see would be indistinguishable from a
            // bug in the selection arithmetic. `sel_row` is patched on top of
            // (not under) the unread bold, matching the order this comment
            // describes — equivalent either way today, since neither side
            // sets a field the other does, but the order that reads as
            // "selection wins" is the one to keep if that ever stops being
            // true.
            if model.is_selected(idx) {
                style = style.patch(theme.sel_row);
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
        .block(pane_block(theme, &title, model.focus == Focus::Messages))
        .highlight_style(selected_style(theme, model.focus == Focus::Messages));
    frame.render_stateful_widget(list, area, &mut state);
}

/// The preview pane: headers of the highlighted row, without fetching a body.
///
/// A list view must not pull a body across the wire per row (see
/// `MailService.List`'s own comment), so the preview shows what the listing
/// already carries and invites `Enter` for the rest.
fn render_preview(model: &Model, frame: &mut Frame, area: Rect) {
    let theme = &model.theme;
    let lines: Vec<Line> = match model.current_message() {
        Some(row) => vec![
            header_line(theme, "From", &row.from),
            header_line(theme, "Date", &short_date(row.date)),
            header_line(theme, "Subject", &row.subject),
            header_line(
                theme,
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
        .block(pane_block(&model.theme, "preview", false))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn render_viewer(model: &Model, frame: &mut Frame, area: Rect) {
    let theme = &model.theme;
    let Some(open) = model.open.as_ref() else {
        frame.render_widget(
            Paragraph::new("nothing open").block(pane_block(theme, "message", true)),
            area,
        );
        return;
    };

    let mut lines: Vec<Line> = open
        .headers
        .iter()
        .map(|(name, value)| header_line(theme, name, value))
        .collect();
    if !open.attachments.is_empty() {
        lines.push(header_line(
            theme,
            "Attachments",
            &open.attachments.join(", "),
        ));
    }
    if open.has_html {
        lines.push(Line::styled(
            "  [HTML alternative available — press o to open it in a browser]",
            theme.accent,
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
        .block(pane_block(theme, "message", true))
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn render_status(model: &Model, frame: &mut Frame, area: Rect) {
    let theme = &model.theme;
    let level_style = match model.level {
        Level::Info => theme.ok,
        Level::Error => theme.err,
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
        Mode::Insert | Mode::Prompt => " -- INSERT --",
        Mode::Menu => " -- SELECT --",
        _ => "",
    };
    let pending = if model.pending.is_empty() {
        String::new()
    } else {
        format!(" {}", model.pending.label())
    };
    let line = Line::from(vec![
        // Sanitized here rather than at each call site: the status line is
        // the one surface every part of the TUI writes to, and task 85 is
        // what first puts third-party text into it — an SMTP server's verbatim
        // rejection (`OutboxRow::last_error`), a recipient address, a folder
        // name. One place covers every present and future caller.
        Span::styled(overlays::safe_line(&model.status), level_style),
        Span::styled(busy, theme.muted),
        Span::styled(mode, theme.mode_indicator),
        Span::styled(pending, theme.warn),
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
    let theme = &model.theme;
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
            Span::styled(format!("  {keys:<14}"), theme.emphasis),
            Span::raw(action.describe()),
        ]));
    }
    lines.push(Line::styled(
        "  rebind with `mail keys set <chord> <action>` — no restart needed",
        theme.muted,
    ));

    let area = centered(area, 72, u16::try_from(lines.len()).unwrap_or(u16::MAX) + 2);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(pane_block(theme, "keys — any of q, Esc or ? closes", true)),
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
        .block(pane_block(&model.theme, title, true))
        .highlight_style(selected_style(&model.theme, true));
    frame.render_stateful_widget(list, area, &mut state);
}

fn render_modal(theme: &Theme, frame: &mut Frame, area: Rect, title: &str, body: &str) {
    let area = centered(area, 60, 3);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(body.to_owned()).block(pane_block(theme, title, true)),
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

fn header_line<'a>(theme: &Theme, name: &str, value: &str) -> Line<'a> {
    Line::from(vec![
        Span::styled(format!("{name}: "), theme.emphasis),
        Span::raw(value.to_owned()),
    ])
}

fn pane_block<'a>(theme: &Theme, title: &str, focused: bool) -> Block<'a> {
    let style = if focused {
        theme.border_focus
    } else {
        theme.border_blur
    };
    Block::default()
        .borders(Borders::ALL)
        .border_style(style)
        .title(title.to_owned())
}

fn selected_style(theme: &Theme, focused: bool) -> Style {
    if focused {
        theme.sel_focus
    } else {
        theme.sel_blur
    }
}

// ---------------------------------------------------------------------------
// task 85's overlays
// ---------------------------------------------------------------------------

/// How many characters of a one-line cell are drawn before it is elided.
///
/// The panes are a percentage of the terminal, so this is a backstop rather
/// than the layout: it keeps one absurd subject from pushing everything after
/// it off the row, and it truncates on **characters** so it cannot split a
/// code point.
const CELL: usize = 96;

/// A line built from [`overlays::runs_from_char_positions`]-style runs, with
/// the matched pieces picked out.
///
/// Highlighting is bold plus a color rather than a background: a background
/// run through a fuzzy match's scattered single characters reads as noise.
fn highlighted<'a>(theme: &Theme, runs: Vec<(String, bool)>) -> Line<'a> {
    Line::from(
        runs.into_iter()
            .map(|(text, on)| {
                if on {
                    Span::styled(text, theme.match_hl)
                } else {
                    Span::raw(text)
                }
            })
            .collect::<Vec<_>>(),
    )
}

/// The prompt line every typing overlay shares: what has been typed, a block
/// cursor, and a hint.
fn prompt_line<'a>(theme: &Theme, sigil: &str, text: &str, hint: &str) -> Line<'a> {
    Line::from(vec![
        Span::styled(
            format!("{sigil} "),
            theme.accent.add_modifier(Modifier::BOLD),
        ),
        // The typed text is the user's own, so it needs no sanitizing for
        // safety — but it goes through the same one-line treatment as
        // everything else so a pasted control character cannot reach the
        // terminal through the search box either.
        Span::raw(overlays::safe_line(text)),
        Span::styled("▏", theme.accent),
        Span::styled(format!("  {hint}"), theme.muted),
    ])
}

fn render_search(theme: &Theme, pane: &SearchPane, frame: &mut Frame, area: Rect) {
    let area = centered_pct(area, 88, 80);
    frame.render_widget(Clear, area);

    let why_height = if pane.explain { 9 } else { 0 };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(why_height),
        ])
        .split(inner(
            theme,
            frame,
            area,
            &format!(
                "search{}",
                if pane.complete {
                    ""
                } else {
                    " — searching…"
                }
            ),
        ));

    frame.render_widget(
        Paragraph::new(prompt_line(
            theme,
            "/",
            &pane.query,
            "~ semantic · = lexical · Tab completes · Enter walks",
        )),
        rows[0],
    );

    let items: Vec<ListItem> = pane
        .hits
        .iter()
        .map(|hit| {
            ListItem::new(vec![
                Line::from(vec![
                    Span::styled(format!("{:<12} ", short_date(hit.date)), theme.muted),
                    Span::styled(
                        overlays::truncate_chars(&overlays::safe_line(&hit.subject), CELL),
                        theme.emphasis,
                    ),
                    Span::styled(format!("  {}", overlays::safe_line(&hit.from)), theme.muted),
                ]),
                // The snippet's highlights are byte ranges into its *original*
                // text; `runs_from_byte_ranges` applies them and the sanitizer
                // together, one character at a time, so a dropped control byte
                // cannot shift a highlight onto the wrong word.
                highlighted(
                    theme,
                    overlays::runs_from_byte_ranges(&hit.snippet, &hit.highlights),
                ),
            ])
        })
        .collect();
    let mut state = ListState::default();
    state.select((!pane.hits.is_empty()).then_some(pane.cursor));
    frame.render_stateful_widget(
        List::new(items).highlight_style(selected_style(theme, pane.focus == SearchFocus::Results)),
        rows[1],
        &mut state,
    );

    if pane.explain {
        render_why(theme, pane, frame, rows[2]);
    }
}

/// The `x` why-panel: the ranker's own feature breakdown for the highlighted
/// hit.
fn render_why(theme: &Theme, pane: &SearchPane, frame: &mut Frame, area: Rect) {
    let lines: Vec<Line> = match pane.explanation.as_ref() {
        None => vec![Line::styled("  explaining…", theme.muted)],
        Some(why) => {
            let mut lines = vec![Line::from(vec![
                Span::styled("  score ", theme.muted),
                Span::raw(why.score.clone()),
                Span::styled(format!("   via {}", why.sources.join(", ")), theme.muted),
            ])];
            lines.extend(why.features.iter().map(|(name, detail)| {
                Line::from(vec![
                    Span::styled(format!("  {name:<24}"), theme.accent),
                    Span::raw(detail.clone()),
                ])
            }));
            if let Some(matched) = why.matched.as_ref() {
                lines.push(Line::raw(format!(
                    "  matched: {}",
                    overlays::truncate_chars(&overlays::safe_line(matched), CELL)
                )));
            }
            if !why.claude_reason.is_empty() {
                // Model-authored, and therefore steerable by any message that
                // reached the reranker's context.
                lines.push(Line::raw(format!(
                    "  claude: {}",
                    overlays::truncate_chars(&overlays::safe_line(&why.claude_reason), CELL)
                )));
            }
            lines
        }
    };
    frame.render_widget(
        Paragraph::new(lines).block(pane_block(theme, "why — x closes", false)),
        area,
    );
}

fn render_finder(theme: &Theme, pane: &FinderPane, frame: &mut Frame, area: Rect) {
    let area = centered_pct(area, 76, 70);
    frame.render_widget(Clear, area);
    let title = match (pane.complete, pane.superseded) {
        // Superseded is not an error and not a completed scan either: the
        // results on screen are the best of a partial walk, and saying so is
        // the difference between "that is all there is" and "that is all it
        // got to".
        (true, true) => format!("find — {} so far (superseded)", pane.items.len()),
        (true, false) => format!("find — {} of {} scanned", pane.items.len(), pane.scanned),
        (false, _) => "find — searching…".to_owned(),
    };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner(theme, frame, area, &title));

    frame.render_widget(
        Paragraph::new(prompt_line(
            theme,
            "❯",
            &pane.query,
            "> cmd · # tag · @ person · / saved · : folder",
        )),
        rows[0],
    );

    let items: Vec<ListItem> = pane
        .items
        .iter()
        .map(|item| {
            ListItem::new(Line::from({
                let mut spans = vec![Span::styled(
                    format!("{:<7}", item.kind.label()),
                    theme.finder_kind,
                )];
                // `positions` are char offsets into `primary`, which is why
                // this indexes characters and never bytes.
                spans.extend(
                    highlighted(
                        theme,
                        overlays::runs_from_char_positions(&item.primary, &item.positions),
                    )
                    .spans,
                );
                if !item.secondary.trim().is_empty() {
                    spans.push(Span::styled(
                        format!(
                            "  {}",
                            overlays::truncate_chars(&overlays::safe_line(&item.secondary), CELL)
                        ),
                        theme.muted,
                    ));
                }
                spans
            }))
        })
        .collect();
    let mut state = ListState::default();
    state.select((!pane.items.is_empty()).then_some(pane.cursor));
    frame.render_stateful_widget(
        List::new(items).highlight_style(selected_style(theme, true)),
        rows[1],
        &mut state,
    );
}

fn render_palette(theme: &Theme, pane: &PalettePane, frame: &mut Frame, area: Rect) {
    let area = centered_pct(area, 70, 66);
    frame.render_widget(Clear, area);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner(
            theme,
            frame,
            area,
            &format!("commands — {} match", pane.matches.len()),
        ));

    frame.render_widget(
        Paragraph::new(prompt_line(
            theme,
            ":",
            &pane.input,
            "Enter runs · Esc closes",
        )),
        rows[0],
    );

    let items: Vec<ListItem> = pane
        .matches
        .iter()
        .map(|entry| {
            ListItem::new(Line::from(vec![
                Span::styled(format!("{:<22}", entry.action.id()), theme.emphasis),
                Span::styled(format!("{:<12}", entry.chords), theme.warn),
                Span::styled(entry.action.describe(), theme.muted),
            ]))
        })
        .collect();
    let mut state = ListState::default();
    state.select((!pane.matches.is_empty()).then_some(pane.cursor));
    frame.render_stateful_widget(
        List::new(items).highlight_style(selected_style(theme, true)),
        rows[1],
        &mut state,
    );
}

fn render_ask(theme: &Theme, pane: &AskPane, frame: &mut Frame, area: Rect) {
    let area = centered_pct(area, 88, 84);
    frame.render_widget(Clear, area);
    let citations_height = u16::try_from(pane.citations.len().min(8)).unwrap_or(8);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(citations_height),
        ])
        .split(inner(theme, frame, area, ask_title(pane)));

    frame.render_widget(
        Paragraph::new(prompt_line(
            theme,
            "?",
            &pane.question,
            match pane.phase {
                AskPhase::Asking => "Enter asks · Esc closes",
                AskPhase::Streaming => "answering…",
                AskPhase::Done => "Esc closes",
            },
        )),
        rows[0],
    );
    frame.render_widget(
        Paragraph::new(Line::styled(
            overlays::safe_line(pane.trace.as_deref().unwrap_or_default()),
            theme.muted,
        )),
        rows[1],
    );

    // Model-authored prose: neutralized, but with its paragraph breaks kept,
    // because they carry meaning in an answer in a way they do not in a row.
    let mut body: Vec<Line> = overlays::safe_prose(&pane.answer)
        .lines()
        .map(|line| Line::raw(line.to_owned()))
        .collect();
    if let Some(error) = pane.error.as_ref() {
        body.push(Line::styled(format!("({error})"), theme.err));
    }
    if pane.phase == AskPhase::Done && !pane.grounded {
        body.push(Line::styled(
            format!(
                "not grounded — the daemon found nothing in your mail behind this answer{}",
                if pane.refusal.is_empty() {
                    String::new()
                } else {
                    format!(": {}", overlays::safe_line(&pane.refusal))
                }
            ),
            theme.err,
        ));
    }
    frame.render_widget(Paragraph::new(body).wrap(Wrap { trim: false }), rows[2]);

    let items: Vec<ListItem> = pane
        .citations
        .iter()
        .map(|citation| {
            ListItem::new(Line::from(vec![
                Span::styled(format!("[{}] ", citation.label), theme.warn),
                Span::raw(overlays::truncate_chars(
                    &overlays::safe_line(&citation.subject),
                    CELL,
                )),
                Span::styled(
                    format!(
                        "  {} · {}",
                        overlays::safe_line(&citation.from_addr),
                        overlays::safe_line(&citation.mailbox)
                    ),
                    theme.muted,
                ),
            ]))
        })
        .collect();
    let mut state = ListState::default();
    state.select((!pane.citations.is_empty()).then_some(pane.cursor));
    frame.render_stateful_widget(
        List::new(items).highlight_style(selected_style(theme, true)),
        rows[3],
        &mut state,
    );
}

/// The ask pane's title says whose verdict "grounded" is, because it is the
/// daemon's — computed from the citations it actually resolved — and not a
/// claim the model made about its own answer.
fn ask_title(pane: &AskPane) -> &'static str {
    match (pane.phase, pane.grounded) {
        (AskPhase::Asking, _) => "ask your mailbox",
        (AskPhase::Streaming, _) => "ask — answering…",
        (AskPhase::Done, true) => "ask — grounded in your mail (daemon-verified)",
        (AskPhase::Done, false) => "ask — NOT grounded (daemon's verdict)",
    }
}

fn render_outbox(theme: &Theme, pane: &OutboxPane, frame: &mut Frame, area: Rect) {
    let area = centered_pct(area, 76, 60);
    frame.render_widget(Clear, area);
    let inner_area = inner(theme, frame, area, "outbox — u cancels · Esc closes");

    // Only when there is nothing else to show. A cancel reports through the
    // same message as a listing, and a refused cancel must not blank the
    // outbox the user is looking at — the status line already carries why.
    if let Some(error) = pane.error.as_ref().filter(|_| pane.rows.is_empty()) {
        frame.render_widget(
            Paragraph::new(Line::styled(overlays::safe_line(error), theme.err)),
            inner_area,
        );
        return;
    }
    if pane.loading {
        frame.render_widget(Paragraph::new("listing…"), inner_area);
        return;
    }
    if pane.rows.is_empty() {
        frame.render_widget(Paragraph::new("nothing waiting to go out"), inner_area);
        return;
    }

    let items: Vec<ListItem> = pane
        .rows
        .iter()
        .map(|row| {
            let state_style = match row.state.as_str() {
                "failed" | "uncertain" => theme.err,
                "sent" => theme.ok,
                _ => theme.warn,
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{:<10}", row.state), state_style),
                Span::styled(
                    format!("{:<12} ", short_date(Some(row.send_at))),
                    theme.muted,
                ),
                Span::raw(overlays::truncate_chars(
                    &overlays::safe_line(&row.subject),
                    CELL,
                )),
                Span::styled(format!("  → {}", overlays::safe_line(&row.to)), theme.muted),
            ]))
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(pane.cursor.min(pane.rows.len().saturating_sub(1))));
    frame.render_stateful_widget(
        List::new(items).highlight_style(selected_style(theme, true)),
        inner_area,
        &mut state,
    );
}

fn render_quick(theme: &Theme, pane: &QuickPane, frame: &mut Frame, area: Rect) {
    let height = u16::try_from(QuickAction::ALL.len()).unwrap_or(3) + 2;
    let area = centered(area, 56, height);
    frame.render_widget(Clear, area);
    let items: Vec<ListItem> = QuickAction::ALL
        .iter()
        .map(|(_, label)| ListItem::new(Line::raw(*label)))
        .collect();
    let mut state = ListState::default();
    state.select(Some(
        pane.cursor.min(QuickAction::ALL.len().saturating_sub(1)),
    ));
    frame.render_stateful_widget(
        List::new(items)
            .block(pane_block(
                theme,
                &format!(
                    "AI · {}",
                    overlays::truncate_chars(&overlays::safe_line(&pane.subject), 40)
                ),
                true,
            ))
            .highlight_style(selected_style(theme, true)),
        area,
        &mut state,
    );
}

/// The collapsible AI panel.
fn render_ai_panel(model: &Model, frame: &mut Frame, area: Rect) {
    let theme = &model.theme;
    let mut lines: Vec<Line> = Vec::new();
    match model.summary.as_ref() {
        None => lines.push(Line::styled("reading…", theme.muted)),
        Some(summary) => {
            lines.push(Line::styled(summary.status.clone(), theme.muted));
            // Every string below was written by a model that a hostile message
            // can steer, so all of it goes through the sanitizer.
            for (label, value) in [
                ("priority", summary.priority.as_ref()),
                ("tl;dr", summary.tl_dr.as_ref()),
                ("summary", summary.summary.as_ref()),
                ("reply", summary.suggested_reply.as_ref()),
            ] {
                if let Some(value) = value {
                    lines.push(Line::styled(format!("{label}:"), theme.emphasis));
                    lines.push(Line::raw(overlays::safe_prose(value)));
                }
            }
            if let Some(needs) = summary.needs_reply {
                lines.push(Line::raw(if needs {
                    "needs a reply".to_owned()
                } else {
                    "no reply needed".to_owned()
                }));
            }
            push_bullets(theme, &mut lines, "key points", &summary.key_points);
            push_bullets(theme, &mut lines, "to-do", &summary.todos);
            push_bullets(theme, &mut lines, "tags", &summary.tags);
        }
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(pane_block(theme, "AI · \\ hides · . acts", false))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn push_bullets(theme: &Theme, lines: &mut Vec<Line<'_>>, label: &str, values: &[String]) {
    if values.is_empty() {
        return;
    }
    lines.push(Line::styled(format!("{label}:"), theme.emphasis));
    lines.extend(
        values
            .iter()
            .map(|value| Line::raw(format!("· {}", overlays::safe_prose(value)))),
    );
}

/// The undo-send countdown.
fn render_toast(theme: &Theme, toast: &UndoToast, frame: &mut Frame, area: Rect) {
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                format!(
                    " sending to {} in {}s ",
                    overlays::safe_line(&toast.to),
                    toast.remaining
                ),
                theme.toast,
            ),
            Span::styled("  u undoes", theme.warn),
        ])),
        area,
    );
}

/// Draw `title`'s border around `area` and return what is left inside it.
///
/// Every overlay wants the same border and the same "what is left" arithmetic,
/// and `Block::inner` is the only thing that gets that arithmetic right when
/// the area is smaller than the border.
fn inner(theme: &Theme, frame: &mut Frame, area: Rect, title: &str) -> Rect {
    let block = pane_block(theme, title, true);
    let inside = block.inner(area);
    frame.render_widget(block, area);
    inside
}

/// A percentage-sized rectangle in the middle of `area`.
fn centered_pct(area: Rect, width_pct: u16, height_pct: u16) -> Rect {
    let width = area.width.saturating_mul(width_pct.min(100)) / 100;
    let height = area.height.saturating_mul(height_pct.min(100)) / 100;
    centered(area, width.max(1), height.max(1))
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
