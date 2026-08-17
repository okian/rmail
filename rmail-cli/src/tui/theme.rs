//! Semantic color/style tokens for the TUI, and the built-in themes.
//!
//! Before this module, `view.rs` spoke `ratatui::Color` directly at each
//! call site — "does this read on a light terminal" was a question with as
//! many answers as there were call sites (`overlays.rs` holds pane *state*,
//! not drawing code, and never had one). [`Theme`] gives
//! every one of those call sites a name instead of a color: `theme.err`
//! rather than `Color::Red`, `theme.muted` rather than `Color::DarkGray`. A
//! call site says *what it means*; a [`Theme`] says what that looks like.
//!
//! # Why named `Color` variants only
//!
//! Every built-in here stays on ratatui's named 16-color `Color` enum — no
//! `Color::Rgb`/`Color::Indexed`. That is real terminal portability (a
//! 16-color-only terminal, an unusual `$TERM`, a remapped palette some users
//! run deliberately) rather than an oversight, and it is why [`Theme::mono`]
//! and [`Theme::high_contrast`] exist as *named* alternatives instead of
//! asking a user to hand-tune RGB values. A `truecolor` theme is a plausible
//! later addition; it does not have to replace this.
//!
//! # The rule every built-in follows
//!
//! No token's *meaning* is carried by color alone — every state this crate
//! draws also has a glyph (the message list's `●`/`★`/`@`) or a [`Modifier`]
//! distinguishing it. [`Theme::mono`] is what makes that rule checkable: it
//! never calls `.fg()`/`.bg()` at all and relies on modifiers and glyphs
//! alone, so a token that quietly depended on hue to be legible fails to
//! *read* under `mono` long before it fails a test.

use ratatui::style::{Color, Modifier, Style};

/// One coherent set of styles for every named concept the TUI draws.
///
/// Grouped by what the token is *about*, not by which screen uses it — the
/// same `muted` token is a date in the message list, a hint under a prompt,
/// and a citation's source line, because all three are "secondary text" and
/// should move together if that concept's look ever changes.
///
/// Deliberately **not** exhaustive of every future indicator this crate will
/// ever draw. Daemon-activity and AI-spend tokens belong to the tasks that
/// first render a daemon indicator or a spend meter (tasks 92 and 96) — a
/// field nothing reads yet is exactly the half-finished state this
/// project's non-negotiables refuse. Extending this struct when a new
/// concept is actually drawn is the expected shape of its growth, the same
/// way `keymap::Action` has grown one variant at a time rather than
/// pre-declaring every action a future task might want.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    // -- surfaces --------------------------------------------------------
    /// A pane's border and title when it holds focus.
    pub border_focus: Style,
    /// A pane's border and title when it does not.
    pub border_blur: Style,
    /// The highlighted row of a focused list.
    pub sel_focus: Style,
    /// The highlighted row of a list that is not focused.
    pub sel_blur: Style,
    /// A message-list row inside an active visual selection, on every row
    /// it covers — not only the one under the cursor.
    pub sel_row: Style,
    /// The undo-send toast's background band.
    pub toast: Style,

    // -- text --------------------------------------------------------
    /// Secondary text: dates, hints, descriptions, "searching…"-style
    /// microcopy, a busy marker.
    pub muted: Style,
    /// A subject line, a header name, an id — text that should stand out
    /// from the row around it without implying a color.
    pub emphasis: Style,
    /// A prompt's sigil and cursor, and small pieces of chrome that should
    /// draw the eye without claiming to mean anything (ok/warn/err do not
    /// apply to them).
    pub accent: Style,
    /// A fuzzy-match or search-snippet highlight.
    pub match_hl: Style,

    // -- semantics --------------------------------------------------------
    /// Success / info-level status.
    pub ok: Style,
    /// Failure / error-level status.
    pub err: Style,
    /// Notable but not urgent: a half-typed chord, a citation marker, an
    /// outbox entry still waiting to send.
    pub warn: Style,

    // -- keys --------------------------------------------------------
    /// The `-- VISUAL --` / `-- INSERT --` / `-- SELECT --` status-line
    /// mode indicator.
    pub mode_indicator: Style,

    // -- mail --------------------------------------------------------
    /// The unread marker (`●`). Color is a secondary cue here — the glyph
    /// itself is what [`Theme::mono`] relies on.
    pub unread: Style,
    /// The flagged marker (`★`).
    pub flagged: Style,
    /// The attachment marker (`@`).
    pub attachment: Style,

    // -- finder --------------------------------------------------------
    /// The finder's per-row kind label (`folder`, `tag`, `contact`, …).
    pub finder_kind: Style,
}

impl Theme {
    /// Today's colors, unchanged — the default, and the theme every
    /// existing render test is pinned against.
    #[must_use]
    pub const fn dark() -> Self {
        Self {
            border_focus: Style::new().fg(Color::Cyan),
            border_blur: Style::new().fg(Color::DarkGray),
            sel_focus: Style::new()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
            sel_blur: Style::new().add_modifier(Modifier::REVERSED),
            sel_row: Style::new().bg(Color::DarkGray).fg(Color::White),
            toast: Style::new()
                .bg(Color::Yellow)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
            muted: Style::new().fg(Color::DarkGray),
            emphasis: Style::new().add_modifier(Modifier::BOLD),
            accent: Style::new().fg(Color::Cyan),
            match_hl: Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ok: Style::new().fg(Color::Green),
            err: Style::new().fg(Color::Red),
            warn: Style::new().fg(Color::Yellow),
            mode_indicator: Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
            unread: Style::new().fg(Color::Yellow),
            flagged: Style::new().fg(Color::Yellow),
            attachment: Style::new().fg(Color::Yellow),
            finder_kind: Style::new().fg(Color::Magenta),
        }
    }

    /// For a light terminal background. Not `dark` with colors swapped — the
    /// problem is specifically Yellow or Cyan **as a foreground painted on
    /// the ambient background**, which both wash out against white in most
    /// terminal palettes. Every such foreground use is replaced: Cyan
    /// becomes Blue throughout (`border_focus`, `accent`, `match_hl`);
    /// Yellow's replacement depends on what the token groups with — `warn`,
    /// `mode_indicator` and `finder_kind` become Magenta (matching
    /// `mode_indicator`'s existing hue rather than introducing a third),
    /// while `unread`/`flagged`/`attachment` become Blue (matching
    /// `match_hl`/`accent`, since all four are "something to notice," and a
    /// message-list marker's glyph — `●`/`★`/`@` — already carries the
    /// distinction from a highlight or an accent, so sharing a hue with
    /// them costs nothing).
    ///
    /// `toast`'s background is the deliberate exception: `bg(Yellow)` is
    /// opaque paint with `fg(Black)` on top of it, not yellow text sitting
    /// on the terminal's own background, so the wash-out problem this theme
    /// exists to fix does not apply and it is kept.
    ///
    /// `sel_row`'s background moves off `DarkGray` to `Gray`: `DarkGray` is
    /// what `muted` uses for foreground text, and a `muted` span (e.g. a
    /// date) inside a visually-selected row would otherwise render its own
    /// color on top of an identical background.
    ///
    /// Everything else (`Red`/`Green`) has enough contrast on both
    /// backgrounds and is kept as-is.
    #[must_use]
    pub const fn light() -> Self {
        Self {
            border_focus: Style::new().fg(Color::Blue),
            border_blur: Style::new().fg(Color::DarkGray),
            sel_focus: Style::new()
                .bg(Color::Blue)
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
            sel_blur: Style::new().add_modifier(Modifier::REVERSED),
            sel_row: Style::new().bg(Color::Gray).fg(Color::Black),
            toast: Style::new()
                .bg(Color::Yellow)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
            muted: Style::new().fg(Color::DarkGray),
            emphasis: Style::new().add_modifier(Modifier::BOLD),
            accent: Style::new().fg(Color::Blue),
            match_hl: Style::new().fg(Color::Blue).add_modifier(Modifier::BOLD),
            ok: Style::new().fg(Color::Green),
            err: Style::new().fg(Color::Red),
            warn: Style::new().fg(Color::Magenta),
            mode_indicator: Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD),
            unread: Style::new().fg(Color::Blue),
            flagged: Style::new().fg(Color::Blue),
            attachment: Style::new().fg(Color::Blue),
            finder_kind: Style::new().fg(Color::Magenta),
        }
    }

    /// Modifiers and glyphs only — no field calls `.fg()`/`.bg()` at all, so
    /// every color stays unset (terminal default) rather than painted over.
    /// The message-list markers (`unread`/`flagged`/`attachment`) need
    /// nothing at all: `●`/`★`/`@` already carry their meaning as glyphs,
    /// which is the point this theme exists to prove.
    #[must_use]
    pub const fn mono() -> Self {
        Self {
            border_focus: Style::new().add_modifier(Modifier::BOLD),
            border_blur: Style::new(),
            sel_focus: Style::new().add_modifier(Modifier::REVERSED.union(Modifier::BOLD)),
            sel_blur: Style::new().add_modifier(Modifier::REVERSED),
            sel_row: Style::new().add_modifier(Modifier::UNDERLINED),
            toast: Style::new().add_modifier(Modifier::REVERSED.union(Modifier::BOLD)),
            muted: Style::new().add_modifier(Modifier::DIM),
            emphasis: Style::new().add_modifier(Modifier::BOLD),
            accent: Style::new().add_modifier(Modifier::UNDERLINED),
            match_hl: Style::new().add_modifier(Modifier::BOLD.union(Modifier::UNDERLINED)),
            ok: Style::new(),
            err: Style::new().add_modifier(Modifier::REVERSED.union(Modifier::BOLD)),
            warn: Style::new().add_modifier(Modifier::ITALIC),
            mode_indicator: Style::new().add_modifier(Modifier::REVERSED.union(Modifier::BOLD)),
            unread: Style::new(),
            flagged: Style::new(),
            attachment: Style::new(),
            finder_kind: Style::new().add_modifier(Modifier::DIM),
        }
    }

    /// Maximum legibility: bright/`Light*` foregrounds, bold throughout, and
    /// `match_hl` additionally underlined so it is distinguishable from
    /// plain `emphasis` even to a viewer who cannot resolve the color at
    /// all.
    ///
    /// `sel_row`'s background is `LightGreen`, not `White`: `muted` sets its
    /// foreground to `White` for legibility (this theme has no "dim" text,
    /// on principle), and a `muted` span — a date, say — inside a
    /// visually-selected row would otherwise paint white text on a white
    /// background. `LightGreen` is otherwise unused in this theme, so it
    /// also reads as a distinct state from `sel_focus`'s `LightBlue` rather
    /// than a dimmer version of it.
    #[must_use]
    pub const fn high_contrast() -> Self {
        Self {
            border_focus: Style::new()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
            border_blur: Style::new().fg(Color::White),
            sel_focus: Style::new()
                .bg(Color::LightBlue)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
            sel_blur: Style::new().add_modifier(Modifier::REVERSED.union(Modifier::BOLD)),
            sel_row: Style::new()
                .bg(Color::LightGreen)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
            toast: Style::new()
                .bg(Color::LightYellow)
                .fg(Color::Black)
                .add_modifier(Modifier::BOLD),
            muted: Style::new().fg(Color::White),
            emphasis: Style::new().fg(Color::White).add_modifier(Modifier::BOLD),
            accent: Style::new()
                .fg(Color::LightCyan)
                .add_modifier(Modifier::BOLD),
            match_hl: Style::new()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD.union(Modifier::UNDERLINED)),
            ok: Style::new()
                .fg(Color::LightGreen)
                .add_modifier(Modifier::BOLD),
            err: Style::new()
                .fg(Color::LightRed)
                .add_modifier(Modifier::BOLD),
            warn: Style::new()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD),
            mode_indicator: Style::new()
                .fg(Color::LightMagenta)
                .add_modifier(Modifier::BOLD),
            unread: Style::new()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD),
            flagged: Style::new()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD),
            attachment: Style::new()
                .fg(Color::LightYellow)
                .add_modifier(Modifier::BOLD),
            finder_kind: Style::new()
                .fg(Color::LightMagenta)
                .add_modifier(Modifier::BOLD),
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::dark()
    }
}

/// The stable name a built-in theme is known by — what a future `:set theme
/// <name>` (task 89) parses and what `?`/the manual print. Kept separate
/// from [`Theme`] itself for the same reason [`crate::keymap::Action`] keeps
/// its id apart from its behavior: a name is addressed from outside this
/// crate's Rust (a config value, a typed command), a [`Theme`] value is not.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ThemeName {
    #[default]
    Dark,
    Light,
    Mono,
    HighContrast,
}

impl ThemeName {
    /// Every built-in, in the order it should be offered/listed.
    pub const ALL: &'static [Self] = &[Self::Dark, Self::Light, Self::Mono, Self::HighContrast];

    /// The name this theme is written and matched by.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
            Self::Mono => "mono",
            Self::HighContrast => "high-contrast",
        }
    }

    /// The theme named `id`, if `id` names one.
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|theme| theme.id() == id)
    }

    /// The actual styles this name resolves to.
    #[must_use]
    pub const fn resolve(self) -> Theme {
        match self {
            Self::Dark => Theme::dark(),
            Self::Light => Theme::light(),
            Self::Mono => Theme::mono(),
            Self::HighContrast => Theme::high_contrast(),
        }
    }
}

#[cfg(test)]
mod tests;
