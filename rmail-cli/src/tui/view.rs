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

use super::form::FormPane;
use super::help::{self, HelpPane};
use super::manual;
use super::model::{Focus, Model, Overlay, Scope, Screen, FLAGGED, SEEN};
use super::overlays::{
    self, AskPane, AskPhase, CommandPane, FinderPane, OutboxPane, QuickAction, QuickPane,
    ReplyPane, SearchFocus, SearchPane, Toast,
};
use super::report::{ReportColumn, ReportPane, ReportTone};
use super::settings;
use super::status;
use super::theme::Theme;
use super::whichkey::{self, Band, Entry, Kind};

/// Draw one frame.
pub fn render(model: &Model, frame: &mut Frame) {
    let area = frame.area();
    // The toast gets a row of its own rather than sharing the status line: it
    // is a countdown with an offer attached, and an offer that scrolls away
    // behind the next "3 messages" is not an offer. The row's height never
    // grows past one even when several are queued — see `render_toast`.
    let toast_height = u16::from(model.shown_toast().is_some());
    // Task 91's band. Derived once and used twice — for the height and for the
    // drawing — because `whichkey::band` walks the keymap's layers and doing
    // that twice per frame to ask the same question is work for nothing.
    let band = whichkey::band(model);
    // Two rows when a binding is unreachable: the warning is a sentence, and
    // reflowing it into the entry row would push the entries off the screen in
    // exactly the state somebody needs to read them.
    let band_height = match band.as_ref() {
        None => 0,
        Some(band) => 1 + u16::from(band.warning.is_some()),
    };
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(toast_height),
            Constraint::Length(band_height),
            Constraint::Length(1),
        ])
        .split(area);

    match model.screen {
        Screen::List => render_panes(model, frame, rows[0]),
        Screen::Viewer => render_main_with_panel(model, frame, rows[0], render_viewer),
        // Full width, and deliberately without the AI panel: the manual is
        // not about a message, so there is nothing for that column to be
        // about either. The settings screen is the same case.
        Screen::Manual => render_manual(model, frame, rows[0]),
        Screen::Settings => render_settings(model, frame, rows[0]),
    }
    if model.shown_toast().is_some() {
        render_toast(model, frame, rows[1]);
    }
    if let Some(band) = band.as_ref() {
        render_band(&model.theme, band, frame, rows[2]);
    }
    render_status(model, frame, rows[3]);

    match &model.overlay {
        Some(Overlay::Help(pane)) => render_help(&model.theme, pane, frame, area),
        Some(Overlay::Pick { what, idx, .. }) => render_pick(model, frame, area, *what, *idx),
        Some(Overlay::Confirm { prompt, .. }) => {
            render_modal(&model.theme, frame, area, "confirm", prompt);
        }
        Some(Overlay::Input { prompt, buffer, .. }) => {
            render_modal(&model.theme, frame, area, prompt, &format!("{buffer}▏"));
        }
        Some(Overlay::Search(pane)) => render_search(&model.theme, pane, frame, area),
        Some(Overlay::Finder(pane)) => render_finder(&model.theme, pane, frame, area),
        Some(Overlay::Command(pane)) => render_command(&model.theme, pane, frame, area),
        Some(Overlay::Ask(pane)) => render_ask(&model.theme, pane, frame, area),
        Some(Overlay::Reply(pane)) => render_reply(&model.theme, pane, frame, area),
        Some(Overlay::Outbox(pane)) => render_outbox(&model.theme, pane, frame, area),
        Some(Overlay::Quick(pane)) => render_quick(&model.theme, pane, frame, area),
        Some(Overlay::Report(pane)) => render_report(&model.theme, pane, frame, area),
        Some(Overlay::Form(pane)) => render_form(&model.theme, pane, frame, area),
        None => {}
    }
}

/// Below this width the preview column is dropped: folders + messages,
/// 2-pane.
const PREVIEW_BREAKPOINT: u16 = 100;
/// Below this width the folder column is dropped too: messages alone,
/// 1-pane.
const FOLDER_BREAKPOINT: u16 = 60;

/// The columns the status bar's message zone keeps before an optional zone is
/// dropped to make room.
///
/// The message is the one zone that flexes, and it is also the one carrying
/// whatever just failed — so an indicator zone that squeezed it to nothing
/// would have hidden the sentence explaining why the indicator went red.
const MIN_MESSAGE: u16 = 24;

/// The list screen's three panes, collapsing as `area` narrows.
///
/// `area` is what is left *after* [`render_main_with_panel`] has already
/// taken the AI panel's share out, when it is open — so the breakpoints
/// below fire against the space the panes actually have, not the raw
/// terminal width. A 120-column terminal with the panel open at its default
/// 30% gives this closure 84 columns, already under [`PREVIEW_BREAKPOINT`].
/// [`panes_width`] duplicates this same split for [`render_status`], which
/// needs the answer from outside the panel's own column.
///
/// `Model::focus` still toggles between [`Focus::Folders`] and
/// [`Focus::Messages`] exactly as it does at full width — `update` has no
/// terminal size to special-case a narrow one with (this module's own docs:
/// render "cannot decide anything the model has not already decided"), and
/// it does not need one. Focusing folders while they are off-screen paints
/// nothing differently until the terminal widens again; [`render_status`]
/// is what tells a person that rather than leaving them to wonder why `j`
/// stopped moving the message cursor.
fn render_panes(model: &Model, frame: &mut Frame, area: Rect) {
    render_main_with_panel(model, frame, area, |model, frame, area| {
        if area.width < FOLDER_BREAKPOINT {
            render_messages(model, frame, area);
            return;
        }
        if area.width < PREVIEW_BREAKPOINT {
            let columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Percentage(model.folder_width_pct),
                    Constraint::Percentage(100 - model.folder_width_pct),
                ])
                .split(area);
            render_folders(model, frame, columns[0]);
            render_messages(model, frame, columns[1]);
            return;
        }
        let columns = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(model.folder_width_pct),
                // `set_option` enforces folder_width_pct + preview_width_pct
                // <= MAX_PANES_PCT (<= 90) as the only writer of either
                // field, so this cannot underflow — `saturating_sub` all the
                // same, because a `Rect` computed from a bad percentage
                // split must never be the thing that turns a stale or
                // hand-built `Model` into a panic.
                Constraint::Percentage(
                    100u16
                        .saturating_sub(model.folder_width_pct)
                        .saturating_sub(model.preview_width_pct),
                ),
                Constraint::Percentage(model.preview_width_pct),
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
        .constraints([
            Constraint::Percentage(100u16.saturating_sub(model.ai_panel_width_pct)),
            Constraint::Percentage(model.ai_panel_width_pct),
        ])
        .split(area);
    main(model, frame, columns[0]);
    render_ai_panel(model, frame, columns[1]);
}

/// The width [`render_panes`] actually lays its columns out in: `terminal_width`
/// minus the AI panel's share when it is open, zero otherwise. [`render_status`]
/// needs this — not the raw terminal width — to say whether [`Focus::Folders`]
/// has anywhere on screen to point at. Re-running [`render_main_with_panel`]'s
/// own split against a throwaway one-row `Rect` (rather than approximating the
/// percentage arithmetic by hand) is what keeps the two answers in agreement by
/// construction instead of by two copies of the same formula staying in sync.
fn panes_width(model: &Model, terminal_width: u16) -> u16 {
    if !model.ai_panel {
        return terminal_width;
    }
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(100u16.saturating_sub(model.ai_panel_width_pct)),
            Constraint::Percentage(model.ai_panel_width_pct),
        ])
        .split(Rect::new(0, 0, terminal_width, 1));
    columns[0].width
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

// ---------------------------------------------------------------------------
// the manual (task 103)
// ---------------------------------------------------------------------------

/// The manual: one already-wrapped line per row, with a row cursor.
///
/// A `List` rather than a `Paragraph`, and for the opposite reason to
/// [`render_viewer`]'s slicing: the manual's cursor selects a *row*, because a
/// row is what `<enter>` follows a link from. `ListState` keeps that row on
/// screen as it moves without the model needing to know the pane's height,
/// and because [`manual`] has already wrapped every line at [`manual::WRAP`]
/// there is no `Wrap` here to make the widget's line count disagree with the
/// model's.
fn render_manual(model: &Model, frame: &mut Frame, area: Rect) {
    let theme = &model.theme;
    let Some(state) = model.manual.as_ref() else {
        // Unreachable: `Screen::Manual` and `Model::manual` are set together
        // (`model::set_screen`). Drawn rather than skipped so that a future
        // edit which broke that pairing shows up as something visible instead
        // of an empty frame.
        frame.render_widget(
            Paragraph::new("the manual is not open").block(pane_block(theme, "manual", true)),
            area,
        );
        return;
    };

    let mut doc = manual::doc(&state.at, &model.keymap);
    if let Some(pattern) = state.pattern() {
        manual::highlight(&mut doc, pattern);
    }

    // The search line gets a row of its own, below the page, for the reason
    // the undo toast gets one: a prompt drawn over the text it is searching
    // hides the answer.
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(1),
            Constraint::Length(u16::from(state.typing())),
        ])
        .split(area);

    let items: Vec<ListItem> = doc
        .lines
        .iter()
        .map(|line| ListItem::new(doc_line(theme, line)))
        .collect();
    let mut list_state = ListState::default();
    list_state.select(if doc.lines.is_empty() {
        None
    } else {
        Some(state.cursor_in(doc.lines.len()))
    });

    let mut title = format!("manual · {}", doc.title);
    if state.can_jump_back() {
        title.push_str(" · <c-o> back");
    }
    if state.can_jump_forward() {
        title.push_str(" · <c-i> forward");
    }
    let list = List::new(items)
        .block(pane_block(theme, &title, true))
        .highlight_style(selected_style(theme, true));
    frame.render_stateful_widget(list, rows[0], &mut list_state);

    if let Some(prompt) = state.prompt.as_ref() {
        let sigil = match prompt.scope {
            Scope::Page => "/",
            Scope::Manual => "g/",
        };
        frame.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(sigil.to_owned(), theme.accent),
                Span::raw(overlays::safe_line(&prompt.pattern)),
                Span::styled("▏", theme.accent),
            ])),
            rows[1],
        );
    }
}

/// One rendered manual line, with each run's [`manual::Ink`] resolved to a
/// theme token. This is the only place the manual's styling is decided, which
/// is what keeps `manual.rs` free of `ratatui` entirely.
fn doc_line<'a>(theme: &Theme, line: &manual::DocLine) -> Line<'a> {
    Line::from(
        line.runs
            .iter()
            .map(|run| Span::styled(overlays::safe_line(&run.text), ink_style(theme, run.ink)))
            .collect::<Vec<_>>(),
    )
}

fn ink_style(theme: &Theme, ink: manual::Ink) -> Style {
    match ink {
        // Prose and code both take the terminal's own foreground. Code is
        // told apart by its `│` gutter rather than by colour, which is the
        // rule `theme::Theme::mono` exists to keep every surface honest
        // about — and a code block dimmed or tinted would be the *command
        // you are meant to type* rendered as an aside.
        manual::Ink::Body | manual::Ink::Code => Style::default(),
        manual::Ink::Heading | manual::Ink::Chord => theme.emphasis,
        manual::Ink::Muted => theme.muted,
        manual::Ink::Accent => theme.accent,
        manual::Ink::Match => theme.match_hl,
        manual::Ink::Broken => theme.err,
    }
}

/// Task 92's status bar: fixed-width zones, and one that flexes.
///
/// The zones are laid out with `Layout` rather than concatenated into a line,
/// because `Model::status` is unbounded — an SMTP server's verbatim rejection
/// reaches it — and a `Paragraph` that does not wrap pushes everything after a
/// long message off the row. Splitting first gives each fixed fact a width
/// nothing else can encroach on, so the mode is in the same columns whether or
/// not anything failed and a stopped indexer is visible while a
/// two-hundred-character rejection is on screen.
///
/// Zones are dropped from the right as the terminal narrows, in reverse order
/// of how much they say: the focus hint first (task 93's, and only ever a hint),
/// then the daemon indicators, then the scope. The mode, the message, the busy
/// marker and the pending keys are never dropped — those four are the ones a
/// keyboard's behaviour depends on.
fn render_status(model: &Model, frame: &mut Frame, area: Rect) {
    let theme = &model.theme;
    let bar = status::bar(model);

    let mode_width = u16::try_from(status::MODE_WIDTH).unwrap_or(u16::MAX);
    let scope_width = u16::try_from(bar.scope.chars().count() + 1).unwrap_or(u16::MAX);
    let daemon_width =
        u16::try_from(status::INDICATOR_WIDTH * bar.daemon.len()).unwrap_or(u16::MAX);
    let inflight_width = u16::try_from(bar.inflight.chars().count() + 1).unwrap_or(u16::MAX);
    let pending_width = u16::try_from(bar.pending.chars().count() + 1).unwrap_or(u16::MAX);
    // `status::bar` answers "the folder pane has focus"; whether that pane is
    // being drawn is this module's question, because it is the only one that
    // knows the width. A hint about a pane the reader can see is noise.
    let eligible = !bar.focus_hint.is_empty() && panes_width(model, area.width) < FOLDER_BREAKPOINT;
    let hint_width = if eligible {
        u16::try_from(bar.focus_hint.chars().count() + 1).unwrap_or(u16::MAX)
    } else {
        0
    };

    // The focus hint is not subject to `MIN_MESSAGE`, and that is the whole
    // point of it: it is only eligible at a width where the folder pane is *not
    // drawn*, and at that width a `Focus::Folders` state makes `j`/`k` move a
    // cursor nobody can see. That is a fact about what the keyboard is doing,
    // and those are the facts this bar never drops — so it is dropped only when
    // it does not fit at all.
    let mut fixed = mode_width + inflight_width + pending_width;
    let hint = hint_width > 0 && fixed + hint_width < area.width;
    if hint {
        fixed += hint_width;
    }
    // The two informative zones, widest first, each kept only if the message
    // still has room. Scope before daemon: which folder you are in explains
    // more of what is on screen than whether the indexer is idle.
    let scope = fixed + scope_width + MIN_MESSAGE <= area.width;
    if scope {
        fixed += scope_width;
    }
    let daemon = fixed + daemon_width + MIN_MESSAGE <= area.width;

    let mut constraints = vec![Constraint::Length(mode_width)];
    if scope {
        constraints.push(Constraint::Length(scope_width));
    }
    constraints.push(Constraint::Min(0));
    if daemon {
        constraints.push(Constraint::Length(daemon_width));
    }
    constraints.push(Constraint::Length(inflight_width));
    constraints.push(Constraint::Length(pending_width));
    if hint {
        constraints.push(Constraint::Length(hint_width));
    }
    let zones = Layout::default()
        .direction(Direction::Horizontal)
        .constraints(constraints)
        .split(area);

    let mut zone = zones.iter();
    let mut next = |frame: &mut Frame, line: Line<'static>| {
        if let Some(area) = zone.next() {
            frame.render_widget(Paragraph::new(line), *area);
        }
    };

    next(frame, Line::styled(bar.mode.clone(), theme.mode_indicator));
    if scope {
        next(
            frame,
            Line::styled(overlays::safe_line(&bar.scope), theme.muted),
        );
    }
    next(
        frame,
        Line::styled(
            // Sanitized here rather than at each call site: the status line is
            // the one surface every part of the TUI writes to, and third-party
            // text reaches it — an SMTP server's verbatim rejection
            // (`OutboxRow::last_error`), a recipient address, a folder name.
            // One place covers every present and future caller.
            overlays::safe_line(&bar.message),
            if bar.failed { theme.err } else { theme.ok },
        ),
    );
    if daemon {
        next(frame, Line::from(indicator_spans(theme, &bar.daemon)));
    }
    next(frame, Line::styled(bar.inflight.clone(), theme.muted));
    next(frame, Line::styled(bar.pending.clone(), theme.warn));
    if hint {
        next(frame, Line::styled(bar.focus_hint.clone(), theme.warn));
    }
}

/// The daemon zone: a glyph and a label per subsystem.
///
/// Both, not either: the glyph is what survives a monochrome terminal and a
/// red-green colour-blind reader, which is the same reason task 90's report rows
/// carry one. The `:` command each indicator expands into is deliberately not
/// drawn here — it is a sentence and this is a fixed zone — and reaches the
/// reader through the manual instead, which is where a command somebody has to
/// type belongs.
fn indicator_spans(theme: &Theme, indicators: &[status::Indicator]) -> Vec<Span<'static>> {
    indicators
        .iter()
        .flat_map(|indicator| {
            let style = tone_style(theme, indicator.state.tone());
            [
                Span::styled(format!(" {}", indicator.state.glyph()), style),
                Span::styled(indicator.which.label().to_owned(), style),
            ]
        })
        .collect()
}

/// The key reference, read out of the keymap rather than written alongside it.
///
/// Task 83 kept this as a hand-maintained table, which was right while the
/// bindings were a `match` nobody could change. Once `keys.toml` can rebind
/// anything, a hand-maintained list is a list that lies to exactly the users
/// who customised something — so the rows are generated from the bindings in
/// force, and a rebind shows up here the moment it is loaded.
///
/// The `?` key reference (task 102): mode-aware (draws [`HelpPane::mode`],
/// not always `Mode::Normal`), scrollable (a stock `List`, so nothing here
/// is clipped past the terminal the way an unwrapped `Paragraph` would),
/// grouped (a bold heading per [`help::Row::Group`]) and filterable
/// (the footer row doubles as the `/` filter's own input line while
/// [`HelpPane::editing`] is true).
fn render_help(theme: &Theme, pane: &HelpPane, frame: &mut Frame, area: Rect) {
    let area = centered_pct(area, 84, 80);
    frame.render_widget(Clear, area);

    let title = format!("keys — {} mode — q/Esc closes", pane.mode.id());
    let inner = inner(theme, frame, area, &title);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner);

    let items: Vec<ListItem> = pane
        .rows
        .iter()
        .map(|row| match row {
            help::Row::Group(label) => ListItem::new(Line::styled(
                label.clone(),
                theme.accent.add_modifier(Modifier::BOLD),
            )),
            help::Row::Binding {
                chords, describe, ..
            } => ListItem::new(Line::from(vec![
                Span::styled(
                    format!("  {:<14}", overlays::safe_line(chords)),
                    theme.emphasis,
                ),
                Span::raw(*describe),
            ])),
        })
        .collect();

    let mut state = ListState::default();
    if let Some(position) = help_list_position(pane) {
        state.select(Some(position));
    }
    frame.render_stateful_widget(
        List::new(items).highlight_style(selected_style(theme, true)),
        rows[0],
        &mut state,
    );

    // `prompt_line` while actually typing — the caret and the accent-styled
    // sigil are what mark every other typing surface here (search, finder,
    // the `:` line, ask) as one; this filter is the same kind of surface
    // and had fallen out of step with them, plain muted text with no cursor
    // and no visible cue that keystrokes are now going into it rather than
    // to the row actions below.
    let footer = if pane.editing {
        prompt_line(theme, "/", &pane.filter, "<enter> keeps it · esc closes")
    } else if pane.filter.is_empty() {
        // At most 65 columns: `centered_pct(area, 84, 80)` on an 80-column
        // terminal, minus the block's left and right borders, leaves
        // exactly that much — and `Paragraph` has no `.wrap()`, so anything
        // longer has its tail silently eaten rather than pinned by a test.
        // `<c-o>` (the *other* direction `<tab>` cycles) is left off the
        // hint on that budget; it is still live, just not named here.
        Line::styled(
            "<enter> runs · c rebinds · K manual · / filters · <tab> modes",
            theme.muted,
        )
    } else {
        Line::styled(
            format!(
                "filter: {} — / edits, esc closes",
                overlays::safe_line(&pane.filter)
            ),
            theme.muted,
        )
    };
    frame.render_widget(Paragraph::new(footer), rows[1]);
}

/// Where [`HelpPane::cursor`]'s binding sits among [`HelpPane::rows`] — what
/// [`ListState::select`] needs, since the list draws group headings too and
/// the cursor counts only the rows that are actually selectable.
fn help_list_position(pane: &HelpPane) -> Option<usize> {
    let mut seen = 0;
    for (position, row) in pane.rows.iter().enumerate() {
        if matches!(row, help::Row::Binding { .. }) {
            if seen == pane.cursor {
                return Some(position);
            }
            seen += 1;
        }
    }
    None
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

/// The `:` command line, and the verbs it currently matches.
fn render_command(theme: &Theme, pane: &CommandPane, frame: &mut Frame, area: Rect) {
    let area = centered_pct(area, 70, 66);
    frame.render_widget(Clear, area);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner(
            theme,
            frame,
            area,
            &format!("command — {} match", pane.matches.len()),
        ));

    // The complaint takes the hint column rather than a row of its own, so a
    // parse error does not reflow the list under it.
    // Sanitized, because a complaint quotes what was typed — a positional or a
    // flag name reaches it verbatim — and a pasted bidi override or an
    // invisible must not reach the terminal through an error message. The
    // typed line itself goes through `prompt_line`, which sanitizes; this is
    // the other half of the same rule.
    let hint = pane.error.as_deref().map_or_else(
        || "Enter runs · Tab completes · Esc closes".to_owned(),
        overlays::safe_line,
    );
    let mut line = prompt_line(theme, ":", &pane.input, &hint);
    if pane.error.is_some() {
        if let Some(last) = line.spans.last_mut() {
            last.style = theme.err;
        }
    }
    frame.render_widget(Paragraph::new(line), rows[0]);

    // A marker rather than a selected row: there is no cursor here to move,
    // and a `List::highlight_style` on row 0 would look exactly like one.
    // It appears only while the fallback is live — when the line already
    // names a verb, Enter runs *that*, and pointing at a ranked row would be
    // pointing at something Enter is not going to do.
    let fallback = pane.fallback_is_live();
    let items: Vec<ListItem> = pane
        .matches
        .iter()
        .enumerate()
        .map(|(row, entry)| {
            let marker = if fallback && row == 0 { "> " } else { "  " };
            ListItem::new(Line::from(vec![
                Span::styled(marker, theme.accent),
                Span::styled(format!("{:<22}", entry.verb), theme.emphasis),
                Span::styled(format!("{:<12}", entry.chords), theme.warn),
                Span::styled(entry.describe.clone(), theme.muted),
            ]))
        })
        .collect();
    frame.render_widget(List::new(items), rows[1]);
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

/// `:reply --ai` — simpler than [`render_ask`]: no typing phase, no
/// citations, so two rows (a context line, then the streaming prose) are all
/// this pane draws.
fn render_reply(theme: &Theme, pane: &ReplyPane, frame: &mut Frame, area: Rect) {
    let area = centered_pct(area, 88, 84);
    frame.render_widget(Clear, area);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner(theme, frame, area, reply_title(pane)));

    frame.render_widget(
        Paragraph::new(Line::styled(
            overlays::safe_line(pane.context.as_deref().unwrap_or("reading the thread…")),
            theme.muted,
        )),
        rows[0],
    );

    // Model-authored prose: neutralized, but with its paragraph breaks kept —
    // the same reason `render_ask`'s own body is.
    let mut body: Vec<Line> = overlays::safe_prose(&pane.body)
        .lines()
        .map(|line| Line::raw(line.to_owned()))
        .collect();
    if let Some(error) = pane.error.as_ref() {
        body.push(Line::styled(
            format!("({})", overlays::safe_line(error)),
            theme.err,
        ));
    }
    if let Some((id, to)) = &pane.drafted {
        body.push(Line::styled(
            format!(
                "draft {id} created for {} — `mail draft rewrite {id}` to adjust, :send --draft={id} to schedule",
                overlays::safe_line(to)
            ),
            theme.muted,
        ));
    }
    frame.render_widget(Paragraph::new(body).wrap(Wrap { trim: false }), rows[1]);
}

/// The reply pane's title says what its one visible state is: still
/// streaming, finished, or failed. The draft id and next steps live in the
/// body instead of here, for the reason [`ask_title`] keeps citations out of
/// its own title — a border title is not the place for text a stream wrote.
fn reply_title(pane: &ReplyPane) -> &'static str {
    if pane.error.is_some() {
        "reply — failed"
    } else if pane.done {
        "reply — Esc closes"
    } else {
        "reply — drafting…"
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

/// Task 90's Report: a fixed-width grid with a header row.
///
/// The columns are padded to the widths the report declared rather than to
/// what the rows happen to contain, so a streamed table does not shift
/// sideways as frames arrive — see `report`'s module docs on why that is the
/// interesting property of a report and not of a list.
fn render_report(theme: &Theme, pane: &ReportPane, frame: &mut Frame, area: Rect) {
    let area = centered_pct(area, 84, 72);
    frame.render_widget(Clear, area);
    let title = report_title(pane);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(inner(theme, frame, area, &title));

    // The header is indented by one cell — the column a row's tone glyph
    // occupies — so the headings sit above the cells they name rather than one
    // place to the left of them.
    let headers: Vec<String> = pane
        .columns
        .iter()
        .map(|column| column.header.clone())
        .collect();
    let mut header = vec![Span::styled(" ", theme.muted)];
    header.extend(report_cells(&headers, &pane.columns, theme.muted));
    frame.render_widget(Paragraph::new(Line::from(header)), rows[0]);

    // Only when there is nothing else to show, the same rule the outbox
    // follows: a report that streamed forty rows and then failed has told the
    // reader something true about those forty, and blanking them to show the
    // error would throw it away. The status line already carries why.
    if let Some(error) = pane.error.as_ref().filter(|_| pane.rows.is_empty()) {
        frame.render_widget(
            Paragraph::new(Line::styled(overlays::safe_line(error), theme.err)),
            rows[1],
        );
        return;
    }
    if pane.rows.is_empty() {
        frame.render_widget(
            Paragraph::new(if pane.complete {
                "nothing to report"
            } else {
                "asking…"
            }),
            rows[1],
        );
        return;
    }

    let items: Vec<ListItem> = pane
        .rows
        .iter()
        .map(|row| {
            let style = tone_style(theme, row.tone);
            let mut spans = vec![Span::styled(row.tone.glyph().to_owned(), style)];
            spans.extend(report_cells(&row.cells, &pane.columns, style));
            // A marker rather than a colour, for the same reason a tone
            // carries a glyph: "this row does something" has to survive a
            // monochrome terminal, and it is what tells a reader that Enter
            // here is not a no-op before they press it.
            if row.on_enter.is_some() {
                spans.push(Span::styled(" ⏎", theme.accent));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(pane.cursor.min(pane.rows.len().saturating_sub(1))));
    frame.render_stateful_widget(
        List::new(items).highlight_style(selected_style(theme, true)),
        rows[1],
        &mut state,
    );
}

/// The form overlay: a label/value row per field, then the row that applies it
/// (task 96).
///
/// The apply row is drawn as a row rather than as a button because it *is* one —
/// see [`FormPane::rows`]. It is styled by whether the form can be applied at
/// all, so "nothing to replace yet" is visible before `<enter>` says it.
fn render_form(theme: &Theme, pane: &FormPane, frame: &mut Frame, area: Rect) {
    let area = centered_pct(area, 72, 62);
    frame.render_widget(Clear, area);
    let title = form_title(pane);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner(theme, frame, area, &title));

    let mut items: Vec<ListItem> = pane
        .fields
        .iter()
        .enumerate()
        .map(|(idx, field)| {
            let editing = pane.editing.as_ref().is_some_and(|edit| edit.at == idx);
            // A caret only on the field being typed into, and only there: a
            // form showing eight of them would say every field was open.
            let value = if editing {
                format!("{}▏", field.value)
            } else if field.value.is_empty() {
                // Not blank: an empty cell reads as a rendering fault, and for
                // this RPC an empty cap is a real value — "no cap" — that
                // applying will store.
                "(none)".to_owned()
            } else {
                field.value.clone()
            };
            let style = if field.value.is_empty() && !editing {
                theme.muted
            } else {
                Style::default()
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!(" {} ", fitted(field.label, FORM_LABEL)),
                    theme.muted,
                ),
                Span::styled(overlays::safe_line(&value), style),
            ]))
        })
        .collect();
    let (apply, apply_style) = match pane.blocked() {
        Some(why) => (why, theme.muted),
        None => (
            "apply — replaces every value above".to_owned(),
            theme.accent,
        ),
    };
    items.push(ListItem::new(Line::from(vec![
        Span::styled(format!(" {} ", fitted("⏎", FORM_LABEL)), theme.muted),
        Span::styled(overlays::safe_line(&apply), apply_style),
    ])));

    let mut state = ListState::default();
    state.select(Some(pane.cursor.min(pane.rows().saturating_sub(1))));
    frame.render_stateful_widget(
        List::new(items).highlight_style(selected_style(theme, true)),
        rows[0],
        &mut state,
    );

    // The highlighted field's own hint, on its own line: eight hints drawn at
    // once is a wall of text, and the one that matters is the one under the
    // cursor.
    let hint = pane.error.as_ref().map_or_else(
        || pane.field().map_or("", |field| field.hint).to_owned(),
        |error| overlays::safe_line(error),
    );
    let style = if pane.error.is_some() {
        theme.err
    } else {
        theme.muted
    };
    frame.render_widget(Paragraph::new(Line::styled(hint, style)), rows[1]);
}

/// How wide a form's label column is.
///
/// Fixed, so the values line up under each other: `monthly soft tokens` is the
/// longest label task 96 has, and a column sized to whatever the longest label
/// happened to be would move every value the moment a field was added.
const FORM_LABEL: usize = 20;

/// A form's border.
fn form_title(pane: &FormPane) -> String {
    let state = if pane.editing.is_some() {
        "typing · <enter> keeps it · Esc puts it back"
    } else if pane.ready {
        "j/k moves · <enter> edits · Esc closes"
    } else if pane.error.is_some() {
        "failed · Esc closes"
    } else {
        "reading…"
    };
    format!("{} · {state}", overlays::safe_line(&pane.title))
}

/// The settings screen: the section list on the left, its fields on the right
/// (task 101).
///
/// Two columns rather than one long list, because fourteen sections and their
/// fields do not fit vertically and a screen that scrolled past its own table of
/// contents would make `<tab>` the only way to know where you are.
fn render_settings(model: &Model, frame: &mut Frame, area: Rect) {
    let theme = &model.theme;
    let Some(settings) = model.settings.as_ref() else {
        return;
    };
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(SETTINGS_SECTIONS), Constraint::Min(1)])
        .split(area);

    let sections: Vec<ListItem> = settings::Section::ALL
        .iter()
        .map(|section| {
            let selected = *section == settings.section;
            ListItem::new(Line::styled(
                format!(" {}", section.title()),
                if selected {
                    theme.accent
                } else {
                    Style::default()
                },
            ))
        })
        .collect();
    let mut state = ListState::default();
    state.select(
        settings::Section::ALL
            .iter()
            .position(|section| *section == settings.section),
    );
    let list_area = inner(theme, frame, columns[0], "settings");
    frame.render_stateful_widget(
        List::new(sections).highlight_style(selected_style(theme, false)),
        list_area,
        &mut state,
    );

    let fields = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(inner(
            theme,
            frame,
            columns[1],
            &format!("{} · <tab> next section", settings.section.title()),
        ));
    let items: Vec<ListItem> = settings
        .fields
        .iter()
        .map(|field| {
            let style = match &field.kind {
                // A read-only field is drawn as one, so a reader knows before
                // pressing `<enter>` that nothing here writes it.
                settings::FieldKind::ReadOnly(_) => theme.muted,
                _ => Style::default(),
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!(" {} ", fitted(field.label, SETTINGS_LABEL)),
                    theme.muted,
                ),
                Span::styled(overlays::safe_line(&field.value()), style),
            ]))
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(
        settings.cursor.min(settings.fields.len().saturating_sub(1)),
    ));
    frame.render_stateful_widget(
        List::new(items).highlight_style(selected_style(theme, true)),
        fields[0],
        &mut state,
    );
    // The highlighted field's hint, on its own line: every hint at once is a
    // wall of text, and the one that matters is the one under the cursor.
    frame.render_widget(
        Paragraph::new(Line::styled(
            settings.field().map_or("", |field| field.hint),
            theme.muted,
        )),
        fields[1],
    );
}

/// How wide the section column is.
const SETTINGS_SECTIONS: u16 = 20;

/// How wide a settings field's label column is.
const SETTINGS_LABEL: usize = 26;

/// One row's cells, each padded or truncated to its column's declared width.
///
/// A row with fewer cells than there are columns draws blanks for the rest; a
/// row with more has the extras dropped, because a cell with no column has no
/// width to be drawn at and appending it would shear the grid it is part of.
fn report_cells(cells: &[String], columns: &[ReportColumn], style: Style) -> Vec<Span<'static>> {
    columns
        .iter()
        .enumerate()
        .map(|(idx, column)| {
            let text = fitted(cells.get(idx).map_or("", String::as_str), column.width);
            Span::styled(format!(" {text}"), style)
        })
        .collect()
}

/// `text` at exactly `width` characters: padded when it is short, elided when
/// it is long.
///
/// Characters throughout, never bytes — `format!("{:<width$}")` pads on bytes,
/// so one non-ASCII cell would shift every column after it, and a byte
/// truncation of "café" can land inside the `é`.
///
/// The ellipsis is *inside* the width rather than added to it, which is the
/// difference between a grid and a nearly-a-grid:
/// [`overlays::truncate_chars`] answers `max + 1` characters when it cuts, so
/// using it directly here would push every column after an elided cell one
/// place right — on that row only.
fn fitted(text: &str, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let safe = overlays::safe_line(text);
    let count = safe.chars().count();
    if count <= width {
        return format!("{safe}{}", " ".repeat(width - count));
    }
    let kept: String = safe.chars().take(width - 1).collect();
    format!("{kept}…")
}

/// What a report's border says: the verb, and how far along it is.
fn report_title(pane: &ReportPane) -> String {
    let verb = pane.invocation.verb.join(" ");
    let state = if pane.stale {
        "stale since a row ran · r re-reads"
    } else if pane.error.is_some() {
        "failed"
    } else if pane.complete {
        "r re-runs · Esc closes"
    } else {
        "asking…"
    };
    format!(
        "{} — {} row(s) · {state}",
        overlays::safe_line(&verb),
        pane.rows.len()
    )
}

/// The style a row's tone draws in.
fn tone_style(theme: &Theme, tone: ReportTone) -> Style {
    match tone {
        ReportTone::Plain => Style::default(),
        ReportTone::Muted => theme.muted,
        ReportTone::Ok => theme.ok,
        ReportTone::Warn => theme.warn,
        ReportTone::Bad => theme.err,
    }
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
    let title = if model.is_summary_pinned() {
        "AI · pinned · \\ hides · . acts"
    } else {
        "AI · \\ hides · . acts"
    };
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
            .block(pane_block(theme, title, false))
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

/// The bottom-row notification: whichever toast [`Model::shown_toast`] picks,
/// plus a `+N` badge when the queue is carrying more than that one. The
/// badge is what keeps this a one-line reflow no matter how many are
/// queued — nothing here ever draws a second row.
fn render_toast(model: &Model, frame: &mut Frame, area: Rect) {
    let theme = &model.theme;
    let Some(shown) = model.shown_toast() else {
        return;
    };
    let mut spans = match shown {
        Toast::Undo(toast) => vec![
            Span::styled(
                format!(
                    " sending to {} in {}s ",
                    overlays::safe_line(&toast.to),
                    toast.remaining
                ),
                theme.toast,
            ),
            Span::styled("  u undoes", theme.warn),
        ],
        Toast::Completion { text } => {
            vec![Span::styled(
                format!(" {} ", overlays::safe_line(text)),
                theme.toast,
            )]
        }
        Toast::Priority { text } => {
            vec![Span::styled(
                format!(" {} ", overlays::safe_line(text)),
                theme.warn,
            )]
        }
    };
    let queued = model.toasts.len() - 1;
    if queued > 0 {
        spans.push(Span::styled(format!("  +{queued}"), theme.muted));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
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

/// Task 91's WhichKey band: what the next key can do.
///
/// One row of `key label` pairs, plus the warning row when a binding under the
/// pending prefix can never be typed.
///
/// The pinned ways out get their own reserved column at the right edge rather
/// than sitting at the end of the same unwrapped line as the other entries.
/// They used to: it reads better when everything fits, but this `Paragraph`
/// has no `.wrap()`, and the entry list is exactly the part with no fixed
/// length — the command line alone can offer 26 top-level candidates, well
/// past 120 columns, which pushed `<esc>`/`<c-c>` off the right edge of the
/// very first `:` a person opens (measured, not hypothetical). A column
/// sized to the pinned entries' *own* rendered width, not the entry list's,
/// is what makes "the way out is there whatever else is" true regardless of
/// how many other entries there are. On a terminal too narrow to hold both,
/// the pinned column wins that argument outright — `Constraint::Min(0)` on
/// the entry side gives up its space first, and that is the intended order:
/// a vanished hint is a smaller loss than a vanished exit.
///
/// The entry list is measured the same way and capped to what is left, with
/// its own `+N` when that cap — not [`Band::dropped`], which only counts
/// what [`MAX_ENTRIES`] already dropped before this ever sees them — is what
/// cuts something off. Two different reasons a `+N` can appear, so they are
/// added together rather than either one silently standing in for both, and
/// the room reserved for that suffix accounts for [`Band::dropped`] on its
/// own: `dropped` is settled before a terminal width even exists, so a row
/// wide enough for every live entry but not for the `+N` it already owes
/// would otherwise draw all of them and then run the suffix straight off
/// the edge of the column — the same disappearance this function exists to
/// stop, just for a number instead of a key.
fn render_band(theme: &Theme, band: &Band, frame: &mut Frame, area: Rect) {
    let (pinned, live): (Vec<&Entry>, Vec<&Entry>) = band
        .entries
        .iter()
        .partition(|entry| entry.kind == Kind::Pinned);

    let mut pinned_spans = entry_spans(theme, pinned.iter().copied());
    // The trailing " ·" separates one entry from the next; the last pinned
    // entry has nothing after it, and flush against the column's own right
    // edge it reads as a stray mark rather than a separator.
    if pinned_spans
        .last()
        .is_some_and(|span| span.content.as_ref() == " ·")
    {
        pinned_spans.pop();
    }
    let pinned_width = spans_width(&pinned_spans);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(0)])
        .split(area);
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Min(0), Constraint::Length(pinned_width)])
        .split(rows[0]);

    let title = Span::styled(
        format!("{} ", overlays::safe_line(&band.title)),
        theme.accent.add_modifier(Modifier::BOLD),
    );
    let title_width = u16::try_from(title.width()).unwrap_or(u16::MAX);
    let mut spans = vec![title];

    let live_spans: Vec<Vec<Span<'static>>> = live
        .iter()
        .map(|entry| entry_spans(theme, std::iter::once(*entry)))
        .collect();
    let live_widths: Vec<u16> = live_spans.iter().map(|s| spans_width(s)).collect();
    let total_live_width: u16 = live_widths.iter().fold(0, |acc, w| acc.saturating_add(*w));
    let available = columns[0].width.saturating_sub(title_width);

    // Two passes, not one budget guessed up front: whether a "+N" suffix
    // needs room depends on whether anything is actually going to be cut,
    // which is not known until every entry's width has been added up —
    // reserving unconditionally would cut the last entry off a row that was
    // always going to fit, over a suffix that was never going to be drawn.
    // But `band.dropped` alone can already force that suffix to appear
    // before a single live entry is measured, so the fast "nothing to
    // reserve for" path only applies when there is nothing owed yet.
    let cut_off = if band.dropped == 0 && total_live_width <= available {
        for entry in live_spans {
            spans.extend(entry);
        }
        0
    } else {
        // Reserved for the *worst* count this row could end up reporting —
        // every live entry cut on top of whatever was already dropped —
        // measured from the actual formatted string rather than a guessed
        // width, so a triple-digit `+N` gets a triple-digit column instead
        // of losing a digit to the same off-screen clipping this function
        // exists to prevent. Reserving for a count larger than what
        // actually gets cut only costs a sliver of otherwise-unused column;
        // reserving for one that is too small costs a wrong number.
        let worst_case = band.dropped.saturating_add(live.len());
        let reserve = spans_width(&[Span::raw(format!(" +{worst_case}"))]);
        let budget = available.saturating_sub(reserve);
        let mut used = 0u16;
        let mut taken = 0usize;
        for (entry, &width) in live_spans.into_iter().zip(&live_widths) {
            if used.saturating_add(width) > budget {
                break;
            }
            used += width;
            taken += 1;
            spans.extend(entry);
        }
        live.len().saturating_sub(taken)
    };
    let total_dropped = band.dropped.saturating_add(cut_off);
    if total_dropped > 0 {
        spans.push(Span::styled(format!(" +{total_dropped}"), theme.muted));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), columns[0]);
    frame.render_widget(Paragraph::new(Line::from(pinned_spans)), columns[1]);

    if let Some(warning) = band.warning.as_ref() {
        frame.render_widget(
            Paragraph::new(Line::styled(overlays::safe_line(warning), theme.warn)),
            rows[1],
        );
    }
}

/// The `key label ·` spans for a run of entries — factored out so the pinned
/// column and the ordinary entry list build identical-looking spans from the
/// same two styling functions.
fn entry_spans<'a>(
    theme: &Theme,
    entries: impl IntoIterator<Item = &'a Entry>,
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for entry in entries {
        spans.push(Span::styled(
            format!(" {}", overlays::safe_line(&entry.keys)),
            band_key_style(theme, entry.kind),
        ));
        if !entry.label.is_empty() {
            spans.push(Span::styled(
                format!(" {}", overlays::safe_line(&entry.label)),
                band_label_style(theme, entry.kind),
            ));
        }
        spans.push(Span::styled(" ·", theme.muted));
    }
    spans
}

/// A run of spans' combined display width.
///
/// `Span::width` is the same `unicode_width` measure ratatui's own text
/// layout walks the string with (per-grapheme, so combining marks count as
/// zero) to decide how many columns it consumes, so measuring the same way
/// here is what makes this function's arithmetic agree with what actually
/// lands on screen. A plain `chars().count()` looked equivalent for the
/// chrome this module writes itself — a chord's vim notation, an action id,
/// the `·` separator, all one column per `char` — but [`Band::title`] is not
/// this module's own text: it is `pending`'s keys or the raw `:` line, and a
/// bound key can be any `char` (a `keys.toml` binding to `Key::Char('日')` is
/// legal) while the command line echoes whatever was typed, unicode-width-2
/// glyphs included.
fn spans_width(spans: &[Span<'_>]) -> u16 {
    spans
        .iter()
        .map(|span| u16::try_from(span.width()).unwrap_or(u16::MAX))
        .fold(0, u16::saturating_add)
}

/// How a band entry's key is drawn.
fn band_key_style(theme: &Theme, kind: Kind) -> Style {
    match kind {
        // Struck through, not merely dimmed: this binding exists and the
        // keyboard cannot deliver it, which is a different thing from "not
        // interesting" and has to look different.
        Kind::Dead => theme.err.add_modifier(Modifier::CROSSED_OUT),
        Kind::Pinned => theme.warn.add_modifier(Modifier::BOLD),
        Kind::Group => theme.accent.add_modifier(Modifier::BOLD),
        Kind::Run => theme.emphasis,
    }
}

/// How a band entry's label is drawn.
fn band_label_style(theme: &Theme, kind: Kind) -> Style {
    match kind {
        Kind::Dead => theme.err.add_modifier(Modifier::CROSSED_OUT),
        Kind::Group => theme.accent,
        Kind::Pinned | Kind::Run => theme.muted,
    }
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
