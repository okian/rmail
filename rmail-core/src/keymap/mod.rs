//! The modal key engine: what a key press *means*, given the mode the TUI is
//! in and the bindings the user has configured (prd.md "Modal Vim Keybindings
//! Engine"; task 84).
//!
//! # Why a table and not a `match`
//!
//! Task 83's shell decided keys with nested `match` arms — one per screen,
//! plus a `bool` for the half-typed `gg`. That is the right amount of
//! machinery for six bindings and the wrong amount for a rebindable one: a
//! `match` arm cannot be read out to a help screen, cannot be renamed by a
//! config file, and cannot be addressed by name from a command palette. So
//! bindings live in a table keyed by (mode, chord) whose values are
//! [`Action`]s — stable string ids like `message.archive` — and the model's
//! job shrinks to "given an action, do it". `keys.toml` edits that table;
//! `?` renders it; task 85's palette will resolve intents to the same ids.
//!
//! # Layers
//!
//! A mode is a *layer*, not an island. [`Mode::chain`] gives the layers a
//! lookup walks, nearest first — `Viewer` before `Normal` before `Global` —
//! so the viewer inherits every navigation binding without restating one,
//! and a mode that must not inherit (an overlay: `Help`, `Pick`, `Confirm`,
//! `Insert`) simply lists `Global` as its only parent. That is what keeps
//! `j` from scrolling the list behind a modal, structurally rather than by
//! remembering to return early.
//!
//! # Three rules that are not vim's, on purpose
//!
//! 1. **An exact match fires immediately**, even when a longer binding has it
//!    as a prefix. vim waits `timeoutlen` for the rest; waiting needs a
//!    clock, and `tui::model::update` deliberately has none (it is pure and
//!    synchronous — see its module docs). Deterministic-now beats
//!    ambiguous-later, and [`Keymap::bind`] refuses the shadowing binding at
//!    load time so the situation cannot arise from a config file.
//! 2. **A dead sequence retries its own tail.** vim discards `g` + `k` whole;
//!    task 83 established that `g` then `k` moves up, and eating a keystroke
//!    because a chord half-matched is a bug, not a feature. So a sequence
//!    that can no longer become a binding drops its oldest key and re-resolves
//!    the rest, until something matches or one key is left over as
//!    [`Resolution::Unbound`].
//! 3. **`Esc` and `Ctrl-C` cannot be rebound**, and no binding may begin with
//!    either (see [`Chord::is_reserved`]). They are the way out of every mode,
//!    including modes a future task adds; a `keys.toml` that could make `Esc`
//!    the first key of a chord could make the TUI stop responding to `Esc`.
//!
//! # Bounds
//!
//! Everything the engine accumulates between keystrokes is bounded, because
//! all of it is driven by a key the user can hold down: a count saturates at
//! [`MAX_COUNT`] rather than growing digits, and a pending chord can never
//! exceed [`MAX_CHORD_KEYS`] keys — which is also the longest bindable chord,
//! so a sequence past that length is dead by construction. A count multiplies
//! *cursor arithmetic*, which is `O(1)` and clamped; it never multiplies
//! commands (see `tui::model`'s `run_action`).

pub mod continuations;
pub mod file;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::fmt;
use std::ops::Bound;

pub use continuations::{common_id_prefix, Continuation, Leads};

/// The largest count a user can type. Beyond this, further digits are
/// absorbed rather than accumulated: a held-down `9` is a stuck key, not a
/// request to allocate.
pub const MAX_COUNT: u32 = 9_999;

/// The most keys one chord may contain, and therefore the most a pending
/// sequence can hold.
pub const MAX_CHORD_KEYS: usize = 4;

// ---------------------------------------------------------------------------
// keys
// ---------------------------------------------------------------------------

/// A key press, in the TUI's own vocabulary rather than crossterm's.
///
/// Decoupled on purpose: the model tests construct key presses directly, and
/// they should not have to build a `crossterm::event::KeyEvent` (with its
/// modifiers, kind and state) to say "the user pressed j".
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Key {
    /// A printable character.
    Char(char),
    /// Control plus a character, normalized to lowercase — crossterm reports
    /// `Ctrl-C` and `Ctrl-Shift-C` as different modifier sets over the same
    /// intent, and a keymap that distinguished them would bind one and
    /// silently drop the other.
    Ctrl(char),
    /// Return.
    Enter,
    /// Escape.
    Esc,
    /// Tab.
    Tab,
    /// Backspace.
    Backspace,
    /// Cursor up.
    Up,
    /// Cursor down.
    Down,
    /// Cursor left.
    Left,
    /// Cursor right.
    Right,
    /// Home.
    Home,
    /// End.
    End,
    /// Page up.
    PageUp,
    /// Page down.
    PageDown,
}

impl Key {
    /// The literal `Ctrl-C`, spelled once so the reserved-key checks and the
    /// default bindings cannot drift apart.
    pub const CTRL_C: Self = Self::Ctrl('c');

    /// Control plus `c`, normalized the way [`Key::Ctrl`] documents.
    #[must_use]
    pub fn ctrl(c: char) -> Self {
        Self::Ctrl(c.to_ascii_lowercase())
    }

    /// The digit this key types, if it types one.
    #[must_use]
    pub fn digit(self) -> Option<u32> {
        match self {
            Self::Char(c) => c.to_digit(10),
            _ => None,
        }
    }
}

impl fmt::Display for Key {
    /// vim's notation, which is what `keys.toml` and `?` both speak.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Char(' ') => f.write_str("<space>"),
            Self::Char('<') => f.write_str("<lt>"),
            Self::Char(c) => write!(f, "{c}"),
            Self::Ctrl(c) => write!(f, "<c-{c}>"),
            Self::Enter => f.write_str("<enter>"),
            Self::Esc => f.write_str("<esc>"),
            Self::Tab => f.write_str("<tab>"),
            Self::Backspace => f.write_str("<bs>"),
            Self::Up => f.write_str("<up>"),
            Self::Down => f.write_str("<down>"),
            Self::Left => f.write_str("<left>"),
            Self::Right => f.write_str("<right>"),
            Self::Home => f.write_str("<home>"),
            Self::End => f.write_str("<end>"),
            Self::PageUp => f.write_str("<pageup>"),
            Self::PageDown => f.write_str("<pagedown>"),
        }
    }
}

/// One or more keys pressed in order, bound to an [`Action`].
///
/// Written the way vim writes them: bare characters concatenated (`gg`), with
/// `<…>` for anything that is not one printable character (`<esc>`, `<c-p>`,
/// `<enter>`). `<lt>` is a literal `<`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Chord(Vec<Key>);

impl Chord {
    /// A chord from its keys.
    ///
    /// # Errors
    ///
    /// [`KeymapError::EmptyChord`] when `keys` is empty, or
    /// [`KeymapError::ChordTooLong`] past [`MAX_CHORD_KEYS`].
    pub fn new(keys: Vec<Key>) -> Result<Self, KeymapError> {
        let chord = Self(keys);
        if chord.0.is_empty() {
            return Err(KeymapError::EmptyChord);
        }
        if chord.0.len() > MAX_CHORD_KEYS {
            return Err(KeymapError::ChordTooLong {
                chord: chord.to_string(),
                max: MAX_CHORD_KEYS,
            });
        }
        Ok(chord)
    }

    /// Parse vim notation: `gg`, `<esc>`, `<c-p>`, `g<enter>`.
    ///
    /// # Errors
    ///
    /// [`KeymapError`] describing what about `text` is not a chord — always
    /// naming `text`, because this parses strings a human typed into
    /// `keys.toml` or a shell.
    pub fn parse(text: &str) -> Result<Self, KeymapError> {
        let mut keys = Vec::new();
        let mut rest = text;
        while !rest.is_empty() {
            // Bounded here as well as in `new`: a 4 MB `keys.toml` line of
            // `jjjj…` should be refused after five keys, not parsed in full
            // and then rejected.
            if keys.len() > MAX_CHORD_KEYS {
                return Err(KeymapError::ChordTooLong {
                    chord: text.to_owned(),
                    max: MAX_CHORD_KEYS,
                });
            }
            if let Some(after) = rest.strip_prefix('<') {
                let Some(end) = after.find('>') else {
                    return Err(KeymapError::Unterminated {
                        chord: text.to_owned(),
                    });
                };
                let (name, remainder) = after.split_at(end);
                keys.push(named_key(name).ok_or_else(|| KeymapError::UnknownKey {
                    chord: text.to_owned(),
                    name: name.to_owned(),
                })?);
                rest = remainder.get(1..).unwrap_or_default();
            } else {
                let mut chars = rest.chars();
                match chars.next() {
                    Some(c) => keys.push(Key::Char(c)),
                    // Unreachable while `rest` is non-empty, but the loop's
                    // termination should not depend on that being true.
                    None => break,
                }
                rest = chars.as_str();
            }
        }
        if keys.is_empty() {
            return Err(KeymapError::EmptyChord);
        }
        Self::new(keys)
    }

    /// The keys, in press order.
    #[must_use]
    pub fn keys(&self) -> &[Key] {
        &self.0
    }

    /// Whether this chord starts with a key the engine reserves as the way
    /// out — `Esc` or `Ctrl-C`. Such a chord is refused by [`Keymap::bind`]:
    /// binding `<esc>j` would make a bare `Esc` merely *pending*, and a mode
    /// nobody can leave is the worst failure a modal UI has.
    #[must_use]
    pub fn is_reserved(&self) -> bool {
        matches!(self.0.first().copied(), Some(Key::Esc | Key::CTRL_C))
    }

    fn starts_with(&self, prefix: &Self) -> bool {
        self.0.starts_with(&prefix.0)
    }
}

impl fmt::Display for Chord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for key in &self.0 {
            write!(f, "{key}")?;
        }
        Ok(())
    }
}

/// The `<…>` key names `keys.toml` accepts. Aliases exist where vim's spelling
/// (`<cr>`, `<bs>`) and the label on the key (`<enter>`, `<backspace>`) differ
/// — a config file should not be a spelling test.
fn named_key(name: &str) -> Option<Key> {
    let lower = name.to_ascii_lowercase();
    if let Some(c) = lower.strip_prefix("c-") {
        let mut chars = c.chars();
        return match (chars.next(), chars.next()) {
            (Some(c), None) => Some(Key::ctrl(c)),
            _ => None,
        };
    }
    Some(match lower.as_str() {
        "esc" | "escape" => Key::Esc,
        "cr" | "enter" | "return" => Key::Enter,
        "tab" => Key::Tab,
        "bs" | "backspace" => Key::Backspace,
        "up" => Key::Up,
        "down" => Key::Down,
        "left" => Key::Left,
        "right" => Key::Right,
        "home" => Key::Home,
        "end" => Key::End,
        // vim's spellings as well as the plain ones: somebody who writes
        // `<pageup>` and somebody who writes `<pgup>` mean the same key, and a
        // grammar that took one and refused the other would be a grammar with a
        // trap in it.
        "pageup" | "pgup" => Key::PageUp,
        "pagedown" | "pgdown" | "pgdn" => Key::PageDown,
        "space" => Key::Char(' '),
        "lt" => Key::Char('<'),
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// actions
// ---------------------------------------------------------------------------

/// Declares the action registry once, and derives the enum, the id table, the
/// help text and the id parser from it.
///
/// One list rather than four parallel ones: an action added to the enum but
/// forgotten in `ALL` would be unbindable, and one forgotten in `from_id`
/// would be unnameable in `keys.toml` — neither failure is visible at the call
/// site. Generating all four makes both impossible rather than merely tested.
macro_rules! actions {
    ($( $variant:ident => $id:literal, $help:literal; )*) => {
        /// Something the TUI can be asked to do, addressed by a stable id.
        ///
        /// The ids are the shared vocabulary prd.md asks for: `keys.toml`
        /// binds to them, `mail keys` prints them, and task 85's command
        /// palette resolves intents to them. Renaming one is a breaking
        /// change to a user's config file.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub enum Action {
            $( #[doc = $help] $variant, )*
        }

        impl Action {
            /// Every action, in the order the help screen lists them.
            pub const ALL: &'static [Action] = &[ $( Action::$variant, )* ];

            /// The stable id this action is bound and addressed by.
            #[must_use]
            pub const fn id(self) -> &'static str {
                match self { $( Action::$variant => $id, )* }
            }

            /// One line of help, as `?` and `mail keys list` show it.
            #[must_use]
            pub const fn describe(self) -> &'static str {
                match self { $( Action::$variant => $help, )* }
            }

            /// The action with this id, if there is one.
            #[must_use]
            pub fn from_id(id: &str) -> Option<Self> {
                match id { $( $id => Some(Action::$variant), )* _ => None }
            }
        }
    };
}

actions! {
    CursorDown    => "cursor.down",         "down";
    CursorUp      => "cursor.up",           "up";
    CursorTop     => "cursor.top",          "first row (or row N with a count)";
    CursorBottom  => "cursor.bottom",       "last row (or row N with a count)";
    CursorPageDown => "cursor.page-down",   "a page down (or N pages with a count)";
    CursorPageUp  => "cursor.page-up",      "a page up (or N pages with a count)";
    FocusToggle   => "focus.toggle",        "switch between the folder and message panes";
    FocusFolders  => "focus.folders",       "focus the folder pane";
    FocusMessages => "focus.messages",      "focus the message pane";
    Open          => "open",                "open the folder or the message";
    Back          => "back",                "back, or quit from the message list";
    Cancel        => "cancel",              "close the overlay, selection or viewer";
    Quit          => "quit",                "quit from anywhere";
    Help          => "help",                "this help";
    HelpRebind    => "help.rebind",         "rebind the highlighted key";
    SettingsOpen  => "settings",            "the settings screen: every switch this build has";
    KeysCheck     => "keys.check",           "list bindings the keyboard can never deliver";
    // Task 105's leader map. Each of these runs a `:` verb that takes no
    // arguments and acts on what is on screen — which is what makes it bindable
    // at all, and what the leader groups are made of. The verb is the surface;
    // the action is the key that reaches it, and `parity` records both.
    TagList       => "tag.list",             "every tag, and how many messages carry it";
    TagSuggest    => "tag.suggest",          "what the model would tag this message";
    RuleList      => "rule.list",            "the standing rules, and whether each is on";
    RuleRun       => "rule.run",             "dry-run the rules over the selection";
    SyncStatus    => "sync.status",           "what the fetcher is doing, per folder";
    IndexStatus   => "index.status",          "what the indexer is doing";
    AiStatus      => "ai.status",             "what the AI queue is doing";
    AttachList    => "attach.list",           "what is attached to the open message";
    LinksList     => "links",                 "the links in this message, classified";
    NoteList      => "note.list",             "the notes on this message";
    NoteWatch     => "note.watch",            "the notes on this message, live";
    WebhookList   => "webhook.list",          "where mail leaves this machine";
    HookList      => "hook.list",             "what runs here when mail arrives";
    SavedList     => "saved.list",            "the searches you have stored";
    CommandOpen   => "command",             "the : command line: run any verb by name";
    ManualOpen    => "manual",              "the manual: guides, concepts and the generated reference";
    ManualBack    => "manual.back",         "back to the manual page you came from";
    ManualForward => "manual.forward",      "forward again, after going back";
    ManualNext    => "manual.next-match",   "next match on this manual page";
    ManualPrev    => "manual.prev-match",   "previous match on this manual page";
    ManualGrep    => "manual.grep",         "search every manual page";
    VisualToggle  => "visual.toggle",       "start or leave a visual selection";
    VisualSwapEnds => "visual.swap-ends",   "jump to the other end of the selection";
    Archive       => "message.archive",     "archive";
    Delete        => "message.delete",      "delete (asks first — this expunges)";
    ToggleRead    => "message.toggle-read", "toggle read";
    ToggleFlag    => "message.toggle-flag", "toggle flagged";
    CopyTo        => "message.copy",        "copy to a folder";
    MoveTo        => "message.move",        "move to a folder";
    Reply         => "message.reply",       "reply (creates a draft)";
    Forward       => "message.forward",     "forward (creates a draft)";
    OpenHtml      => "message.open-html",   "open the HTML part in a browser";
    SearchOpen    => "search",              "search this mailbox (~ semantic, = lexical)";
    SearchExplain => "search.explain",      "why did this result match";
    FinderOpen    => "finder",              "jump to anything (>#@/: scope it)";
    PaletteOpen   => "palette",             "the : command line (alias of command)";
    AskOpen       => "ask",                 "ask a question about this mailbox";
    AiPanel       => "ai.panel",            "show or hide the AI panel";
    AiQuick       => "ai.quick",            "AI actions for this message";
    OutboxOpen    => "outbox",              "the outbox: scheduled, failed and undoable sends";
    OutboxCancel  => "outbox.cancel",       "cancel the highlighted send (undo)";
    ReportRerun   => "report.rerun",        "run this report's own : line again";
    ReportReject  => "report.reject",       "the no half of a report row that offers both";
    PromptAccept  => "prompt.accept",       "accept what has been typed";
    PromptComplete => "prompt.complete",    "complete the operator being typed";
    MenuAccept    => "menu.accept",         "use the highlighted row";
    PickAccept    => "pick.accept",         "use the highlighted folder";
    ConfirmAccept => "confirm.accept",      "confirm";
    InputSubmit   => "input.submit",        "submit what has been typed";
    InputBackspace => "input.backspace",    "delete the character before the cursor";
}

// ---------------------------------------------------------------------------
// modes
// ---------------------------------------------------------------------------

/// A layer of bindings. The active one is derived from the model's state
/// (`tui::model::Model::mode`), never stored, so it cannot disagree with what
/// is on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Mode {
    /// Bindings every mode inherits. Never active on its own.
    Global,
    /// The folder/message list.
    Normal,
    /// One message, full width. Inherits `Normal`.
    Viewer,
    /// A range of messages is selected. Inherits `Normal`.
    Visual,
    /// A text prompt is up: keys are text, not commands.
    Insert,
    /// One of task 85's typing overlays — the search box, the fuzzy finder,
    /// the command palette, the ask pane's question line.
    ///
    /// One mode for four overlays rather than four modes, for the same reason
    /// `cursor.down` is one action driving four cursors: what `<enter>` or
    /// `<tab>` *means* is context-sensitive (accept a hit, take an item, run
    /// a command, send a question) and that context is the overlay, not the
    /// key table. Four modes would be four copies of the same five bindings,
    /// free to drift apart in a user's `keys.toml`.
    Prompt,
    /// One of task 85's list overlays — search results once the query is
    /// submitted, the ask pane's citations, the outbox, the AI quick-action
    /// menu. Keys are commands again.
    Menu,
    /// The folder picker.
    Pick,
    /// A yes/no question.
    Confirm,
    /// The help overlay.
    Help,
    /// The settings screen (task 101).
    ///
    /// A layer of its own rather than a reuse of [`Mode::Menu`], and it restates
    /// `j`/`k`/`gg`/`G`/`<tab>`/`<enter>` rather than inheriting `Normal` — the
    /// same reason `Menu` and `Pick` already restate them. A settings screen that
    /// fell through to `Normal` would answer `a` with "archive" over a list of
    /// fields, and `d` with "delete" over one that is not a message.
    Settings,
}

impl Mode {
    /// The modes `keys.toml` may bind, in the order `mail keys list` prints
    /// them. [`Mode::Global`] is deliberately absent: it holds `Esc` and
    /// `Ctrl-C`, which are not the user's to reassign.
    pub const CONFIGURABLE: &'static [Mode] = &[
        Mode::Normal,
        Mode::Viewer,
        Mode::Visual,
        Mode::Insert,
        Mode::Prompt,
        Mode::Menu,
        Mode::Pick,
        Mode::Confirm,
        Mode::Help,
        Mode::Settings,
    ];

    /// The name this mode has in `keys.toml` and on the command line.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Global => "global",
            Self::Normal => "normal",
            Self::Viewer => "viewer",
            Self::Visual => "visual",
            Self::Insert => "insert",
            Self::Prompt => "prompt",
            Self::Menu => "menu",
            Self::Pick => "pick",
            Self::Confirm => "confirm",
            Self::Help => "help",
            Self::Settings => "settings",
        }
    }

    /// The mode with this name, if a user may bind it.
    #[must_use]
    pub fn from_id(id: &str) -> Option<Self> {
        Self::CONFIGURABLE.iter().copied().find(|m| m.id() == id)
    }

    /// The layers a lookup walks, nearest first.
    ///
    /// The overlay modes stop at [`Mode::Global`] rather than falling through
    /// to [`Mode::Normal`]: a key that reaches the list behind a modal is the
    /// bug `keys_do_not_reach_the_list_while_help_is_up` guards.
    #[must_use]
    pub const fn chain(self) -> &'static [Mode] {
        match self {
            Self::Global => &[Mode::Global],
            Self::Normal => &[Mode::Normal, Mode::Global],
            Self::Viewer => &[Mode::Viewer, Mode::Normal, Mode::Global],
            Self::Visual => &[Mode::Visual, Mode::Normal, Mode::Global],
            Self::Insert => &[Mode::Insert, Mode::Global],
            Self::Prompt => &[Mode::Prompt, Mode::Global],
            Self::Menu => &[Mode::Menu, Mode::Global],
            Self::Pick => &[Mode::Pick, Mode::Global],
            Self::Confirm => &[Mode::Confirm, Mode::Global],
            Self::Help => &[Mode::Help, Mode::Global],
            Self::Settings => &[Mode::Settings, Mode::Global],
        }
    }

    /// Whether a leading digit starts a count here.
    ///
    /// False in [`Mode::Insert`] and [`Mode::Prompt`], where digits are text:
    /// an address — or a `from:alice2` search — with a `3` in it must not
    /// become a repeat count.
    #[must_use]
    pub const fn takes_counts(self) -> bool {
        !matches!(self, Self::Insert | Self::Prompt)
    }

    /// Whether a multi-key chord may be bound in this mode.
    ///
    /// False in [`Mode::Insert`] and [`Mode::Prompt`] for the same reason
    /// counts are: the first key of a chord is *held*, and holding a keystroke
    /// back inside a text field is indistinguishable from dropping it.
    #[must_use]
    pub const fn allows_chords(self) -> bool {
        !matches!(self, Self::Insert | Self::Prompt)
    }
}

// ---------------------------------------------------------------------------
// pending state
// ---------------------------------------------------------------------------

/// What has been typed so far towards a binding: an optional count and the
/// keys of a half-finished chord. Both are bounded; see this module's docs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pending {
    count: Option<u32>,
    keys: Vec<Key>,
}

impl Pending {
    /// Whether nothing is half-typed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.count.is_none() && self.keys.is_empty()
    }

    /// The count typed so far, if any.
    #[must_use]
    pub fn count(&self) -> Option<u32> {
        self.count
    }

    /// The keys of the half-finished chord.
    #[must_use]
    pub fn keys(&self) -> &[Key] {
        &self.keys
    }

    /// Forget everything half-typed. The engine does this itself whenever a
    /// sequence resolves; the model calls it when the mode changes underneath
    /// the user (a `Msg` can close the viewer while a `g` is pending).
    pub fn clear(&mut self) {
        self.count = None;
        self.keys.clear();
    }

    /// What the status line shows so a half-typed command is visible rather
    /// than mysterious — `3g` after `3` then `g`, empty when nothing is
    /// pending.
    #[must_use]
    pub fn label(&self) -> String {
        let mut out = String::new();
        if let Some(count) = self.count() {
            out.push_str(&count.to_string());
        }
        for key in self.keys() {
            out.push_str(&key.to_string());
        }
        out
    }

    /// Absorb one digit, saturating at [`MAX_COUNT`].
    fn push_digit(&mut self, digit: u32) {
        let next = self
            .count
            .unwrap_or(0)
            .saturating_mul(10)
            .saturating_add(digit);
        self.count = Some(next.min(MAX_COUNT));
    }

    fn take_count(&mut self) -> Option<u32> {
        self.count.take()
    }
}

/// What one key press turned out to mean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// The keys so far are a prefix of at least one binding. Nothing to do
    /// yet; the next key decides.
    Pending,
    /// Run this action.
    Run {
        /// What to do.
        action: Action,
        /// The count the user typed, or `None` when they typed none — an
        /// action that means something different for "go to the last row" and
        /// "go to row 1" needs to tell those apart.
        count: Option<u32>,
    },
    /// Nothing is bound to this key here, and nothing can be. Carries the key
    /// itself so a text prompt can type it (see `tui::model`'s insert mode).
    Unbound(Key),
}

// ---------------------------------------------------------------------------
// the map
// ---------------------------------------------------------------------------

/// Every binding, by mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keymap {
    layers: BTreeMap<Mode, BTreeMap<Chord, Action>>,
}

/// The bindings a fresh install has. Task 83's shell, restated as data.
const DEFAULTS: &[(Mode, &str, Action)] = &[
    // The way out, from every mode there is or will be. Not rebindable.
    (Mode::Global, "<c-c>", Action::Quit),
    (Mode::Global, "<esc>", Action::Cancel),
    (Mode::Normal, "j", Action::CursorDown),
    (Mode::Normal, "<down>", Action::CursorDown),
    (Mode::Normal, "k", Action::CursorUp),
    (Mode::Normal, "<up>", Action::CursorUp),
    (Mode::Normal, "gg", Action::CursorTop),
    (Mode::Normal, "G", Action::CursorBottom),
    // Paging, in every layer that has something to page (task 106). `<c-d>`
    // and `<c-u>` are restated in `Prompt`, `Menu`, `Pick` and `Help` below
    // rather than bound once here, because those layers' chains stop at
    // `Global` — the same reason each of them restates `j`/`k`.
    //
    // The two layers that do *not* get them are `Insert` and `Confirm`, and
    // that is the absence of something to move rather than an omission: a
    // one-line text field and a yes/no question have no rows and no scroll
    // offset, so `active_cursor` finds nothing there and the key would be a
    // documented binding that does nothing wherever it was pressed.
    //
    // `<pagedown>`/`<pageup>` are bound alongside the vim spelling wherever it
    // is, and only because task 105 made them deliverable — the terminal's own
    // events for those keys were dropped before they reached this table, so
    // binding them before now would have been writing a line that could never
    // fire. Two spellings of one action rather than two actions: somebody who
    // reaches for the key with the name on it means what `<c-d>` means.
    (Mode::Normal, "<c-d>", Action::CursorPageDown),
    (Mode::Normal, "<c-u>", Action::CursorPageUp),
    (Mode::Normal, "<pagedown>", Action::CursorPageDown),
    (Mode::Normal, "<pageup>", Action::CursorPageUp),
    (Mode::Normal, "<tab>", Action::FocusToggle),
    (Mode::Normal, "h", Action::FocusFolders),
    (Mode::Normal, "l", Action::FocusMessages),
    (Mode::Normal, "<enter>", Action::Open),
    (Mode::Normal, "q", Action::Back),
    (Mode::Normal, "?", Action::Help),
    // Task 89's command line. Bound in `Normal` and `Menu` only: `Viewer` and
    // `Visual` inherit `Normal`, and neither shadows `:` — a second literal
    // entry for each would be two more rows that must never disagree with
    // this one. `Menu` restates it for the reason it restates `j`/`k`: its
    // chain stops at `Global`, so nothing reaches it from `Normal`.
    (Mode::Normal, ":", Action::CommandOpen),
    // vim's `K` — "look this up" — for the manual (task 103). `?` stays the
    // key reference; `K` is the prose behind it.
    (Mode::Normal, "K", Action::ManualOpen),
    (Mode::Normal, "v", Action::VisualToggle),
    (Mode::Normal, "a", Action::Archive),
    (Mode::Normal, "d", Action::Delete),
    (Mode::Normal, "s", Action::ToggleRead),
    (Mode::Normal, "f", Action::ToggleFlag),
    (Mode::Normal, "c", Action::CopyTo),
    (Mode::Normal, "M", Action::MoveTo),
    (Mode::Normal, "r", Action::Reply),
    (Mode::Normal, "F", Action::Forward),
    (Mode::Normal, "o", Action::OpenHtml),
    // Task 85's overlays. Every one of them opens from the message list and
    // is left with Esc, which the global layer owns and no config file can
    // take away — see `Chord::is_reserved`.
    (Mode::Normal, "/", Action::SearchOpen),
    (Mode::Normal, "<c-p>", Action::FinderOpen),
    (Mode::Normal, "<c-k>", Action::PaletteOpen),
    (Mode::Normal, "A", Action::AskOpen),
    (Mode::Normal, ".", Action::AiQuick),
    (Mode::Normal, "\\", Action::AiPanel),
    (Mode::Normal, "O", Action::OutboxOpen),
    // `u` is "undo the send that is still inside its window" — the toast's
    // key. Bound in Normal as well as Menu because the toast is visible from
    // the message list, where the outbox pane is not up.
    (Mode::Normal, "u", Action::OutboxCancel),
    // Viewer inherits all of the above; `o` there is still "open the HTML
    // part", which is the only place it can do anything.
    //
    // Visual shadows exactly one binding: `o` is vim's swap-ends, and the
    // single-message actions it hides are refused on a selection anyway.
    (Mode::Visual, "o", Action::VisualSwapEnds),
    (Mode::Insert, "<enter>", Action::InputSubmit),
    (Mode::Insert, "<bs>", Action::InputBackspace),
    // Prompt binds only what cannot be typed. Everything else falls through
    // as text, which is why a search for `q` or a contact called `Jan` is
    // possible at all.
    (Mode::Prompt, "<enter>", Action::PromptAccept),
    (Mode::Prompt, "<bs>", Action::InputBackspace),
    (Mode::Prompt, "<tab>", Action::PromptComplete),
    (Mode::Prompt, "<up>", Action::CursorUp),
    (Mode::Prompt, "<down>", Action::CursorDown),
    // A control chord is not text, so paging the hits underneath a query line
    // takes nothing away from the line itself — which is the whole reason
    // `Prompt` binds `<up>`/`<down>` and not `k`/`j`.
    (Mode::Prompt, "<c-d>", Action::CursorPageDown),
    (Mode::Prompt, "<c-u>", Action::CursorPageUp),
    (Mode::Prompt, "<pagedown>", Action::CursorPageDown),
    (Mode::Prompt, "<pageup>", Action::CursorPageUp),
    // Menu is a list again, so the list bindings come back — restated rather
    // than inherited from Normal, because an overlay whose chain reached
    // Normal would let `d` delete the mail behind it.
    (Mode::Menu, "j", Action::CursorDown),
    (Mode::Menu, "<down>", Action::CursorDown),
    (Mode::Menu, "k", Action::CursorUp),
    (Mode::Menu, "<up>", Action::CursorUp),
    (Mode::Menu, "gg", Action::CursorTop),
    (Mode::Menu, "G", Action::CursorBottom),
    (Mode::Menu, "<c-d>", Action::CursorPageDown),
    (Mode::Menu, "<c-u>", Action::CursorPageUp),
    (Mode::Menu, "<pagedown>", Action::CursorPageDown),
    (Mode::Menu, "<pageup>", Action::CursorPageUp),
    (Mode::Menu, "<enter>", Action::MenuAccept),
    (Mode::Menu, "x", Action::SearchExplain),
    (Mode::Menu, "u", Action::OutboxCancel),
    // `r` re-runs a report's own `:` line (task 90). Bound in `Menu` only —
    // `Normal`'s `r` is `message.reply`, and a report is the one thing in this
    // layer that has a line to run again.
    (Mode::Menu, "r", Action::ReportRerun),
    // `n` is the *no* to `<enter>`'s yes on a report row that offers both (task
    // 95's tag suggestions). Bound here rather than reusing `Mode::Confirm`'s
    // `n`, because this is not a modal question: the list stays up and the next
    // row is answered next.
    (Mode::Menu, "n", Action::ReportReject),
    // Back to the query line of whichever overlay is up.
    (Mode::Menu, ":", Action::CommandOpen),
    (Mode::Menu, "/", Action::SearchOpen),
    (Mode::Menu, "q", Action::Cancel),
    (Mode::Pick, "j", Action::CursorDown),
    (Mode::Pick, "<down>", Action::CursorDown),
    (Mode::Pick, "k", Action::CursorUp),
    (Mode::Pick, "<up>", Action::CursorUp),
    (Mode::Pick, "gg", Action::CursorTop),
    (Mode::Pick, "G", Action::CursorBottom),
    (Mode::Pick, "<c-d>", Action::CursorPageDown),
    (Mode::Pick, "<c-u>", Action::CursorPageUp),
    (Mode::Pick, "<pagedown>", Action::CursorPageDown),
    (Mode::Pick, "<pageup>", Action::CursorPageUp),
    (Mode::Pick, "<enter>", Action::PickAccept),
    (Mode::Pick, "q", Action::Cancel),
    (Mode::Confirm, "y", Action::ConfirmAccept),
    (Mode::Confirm, "Y", Action::ConfirmAccept),
    (Mode::Confirm, "n", Action::Cancel),
    (Mode::Confirm, "N", Action::Cancel),
    (Mode::Confirm, "q", Action::Cancel),
    (Mode::Help, "q", Action::Cancel),
    (Mode::Help, "?", Action::Cancel),
    // `Mode::Help` is also `Screen::Manual`'s layer (task 103), so `<enter>`
    // has to mean "use the row under the cursor" rather than "close" — on the
    // manual that row is a `[[link]]`. It was `Action::Cancel` here through
    // tasks 83–102 and the *behaviour* is unchanged: the `?` overlay has no
    // row cursor, so `menu.accept` there still falls through to closing it
    // (`menu_accept`'s `Overlay::Help` arm, pinned by
    // `enter_still_closes_the_help_overlay`). Task 102 makes that arm run the
    // highlighted binding instead, which is the same key meaning the same
    // thing about a richer overlay.
    (Mode::Help, "<enter>", Action::MenuAccept),
    // The manual is a document: it scrolls, it is searched, and it has a jump
    // list. None of these were bound in this layer before, so none of them
    // takes anything away — and the `?` overlay, which has no cursor of its
    // own until task 102 gives it one, is unaffected by all of them.
    (Mode::Help, "j", Action::CursorDown),
    (Mode::Help, "<down>", Action::CursorDown),
    (Mode::Help, "k", Action::CursorUp),
    (Mode::Help, "<up>", Action::CursorUp),
    (Mode::Help, "gg", Action::CursorTop),
    (Mode::Help, "G", Action::CursorBottom),
    // A manual page is the longest thing in the client, so this is the layer
    // paging matters most in — and the one where a reader arrives already
    // expecting a pager's keys.
    (Mode::Help, "<c-d>", Action::CursorPageDown),
    (Mode::Help, "<c-u>", Action::CursorPageUp),
    (Mode::Help, "<pagedown>", Action::CursorPageDown),
    (Mode::Help, "<pageup>", Action::CursorPageUp),
    // `K` in this layer *is* a change: it used to be unbound in the `?`
    // overlay and now closes it and opens the manual. Deliberate — the two are
    // halves of the same thing, the reference and the prose behind it — and
    // task 102 refines it to land on the page documenting the highlighted row
    // rather than the front page.
    (Mode::Help, "K", Action::ManualOpen),
    // Task 102's third row action, alongside `<enter>` and `K`: open a
    // rebind for the highlighted binding. Still bound in the manual, which
    // shares this layer, but inert there: `open_help_rebind` requires an
    // open `?` overlay to read a highlighted row from, and the manual has
    // no such overlay — only a screen — so it always finds none.
    (Mode::Help, "c", Action::HelpRebind),
    // `/` is "search what is in front of me" in every mode that binds it; on
    // the manual that is this page rather than the mailbox (`open_search`
    // dispatches on the screen, the same way `cursor.down` dispatches on
    // which list is up). `g/` widens it to every page.
    //
    // Task 102 wants `/` for filtering the `?` overlay's rows, which is the
    // same collision `<tab>` has below and takes the same answer: one action,
    // dispatched on which of the two surfaces sharing this layer is up.
    (Mode::Help, "/", Action::SearchOpen),
    (Mode::Help, "g/", Action::ManualGrep),
    (Mode::Help, "n", Action::ManualNext),
    (Mode::Help, "N", Action::ManualPrev),
    // vim's jump list. `<tab>` is bound alongside `<c-i>` because the two are
    // the same byte (0x09) on a terminal without the kitty keyboard protocol:
    // crossterm reports `KeyCode::Tab` for Ctrl-I, so `<c-i>` alone would be
    // a binding most terminals can never deliver. Task 102 wants `<tab>` for
    // cycling the `?` overlay's mode — that is a different surface under the
    // same layer, so make `manual.forward` context-sensitive there rather
    // than taking this binding away.
    (Mode::Help, "<c-o>", Action::ManualBack),
    (Mode::Help, "<c-i>", Action::ManualForward),
    (Mode::Help, "<tab>", Action::ManualForward),
    // The settings screen (task 101). Every navigation binding is restated
    // rather than inherited, for the reason `Mode::Settings`' own docs give: a
    // screen of fields that fell through to `Normal` would answer `a` with
    // "archive" and `d` with "delete" over rows that are not messages.
    (Mode::Settings, "j", Action::CursorDown),
    (Mode::Settings, "<down>", Action::CursorDown),
    (Mode::Settings, "k", Action::CursorUp),
    (Mode::Settings, "<up>", Action::CursorUp),
    (Mode::Settings, "gg", Action::CursorTop),
    (Mode::Settings, "G", Action::CursorBottom),
    // `<enter>` on a field does whatever that field's kind means — toggle,
    // cycle a choice, run a command, open a config block. One action
    // dispatched on the field, for the reason `cursor.down` is one action
    // driving four cursors.
    (Mode::Settings, "<enter>", Action::MenuAccept),
    // `<tab>` moves between sections. `focus.toggle` rather than an action of
    // its own: the key means "the next thing over" and what that is depends on
    // the screen, which is the same shape `cursor.down` has. An id under
    // `settings.` would also auto-derive a `:settings section` verb that
    // shadowed `:settings <section>`.
    (Mode::Settings, "<tab>", Action::FocusToggle),
    (Mode::Settings, "q", Action::Cancel),
    // The `:` line reaches every one of these fields by name, which is the
    // property the whole screen is built on — see `tui::settings`.
    (Mode::Settings, ":", Action::CommandOpen),
    (Mode::Settings, "?", Action::Help),
    (Mode::Settings, "K", Action::ManualOpen),
    // From a Report, which is what the acceptance asks for: a table of daemon
    // state and the switches behind it are the same subject.
    (Mode::Menu, "s", Action::SettingsOpen),
    (Mode::Normal, "gs", Action::SettingsOpen),
    // -- the leader map (task 105) -------------------------------------------
    //
    // `<space>` opens a page of grouped commands, one letter per domain. Bound
    // in `Normal` *only*, and that is not a shortfall: `Viewer` and `Visual`
    // both chain through `Normal` ([`Mode::chain`]), so every chord here is
    // live in all three — and binding it three times would be three copies free
    // to drift apart in somebody's `keys.toml`.
    //
    // Every member is an existing action, and every action runs a `:` verb that
    // takes no arguments and acts on what is on screen. That is what makes a
    // domain bindable at all: a verb needing an address or a query has nothing a
    // keystroke could supply, and belongs on the `:` line (or on the settings
    // screen, which puts it there for you).
    //
    // The *labels* the band draws are derived, never written here: task 91's
    // `common_id_prefix` names a group only when its members share a leading id
    // segment, and says "N commands" when they do not. A group like `<space>d`
    // spans three services on purpose and reads as a count for exactly that
    // reason — inventing "daemon" for it is the hand-written group table the
    // derivation exists to avoid.
    //
    // `<space>a` — AI.
    (Mode::Normal, " ap", Action::AiPanel),
    (Mode::Normal, " aq", Action::AiQuick),
    (Mode::Normal, " as", Action::AiStatus),
    // `<space>t` — tags.
    (Mode::Normal, " tl", Action::TagList),
    (Mode::Normal, " ts", Action::TagSuggest),
    // `<space>r` — rules.
    (Mode::Normal, " rl", Action::RuleList),
    (Mode::Normal, " rr", Action::RuleRun),
    // `<space>d` — the daemon's three subsystems.
    (Mode::Normal, " ds", Action::SyncStatus),
    (Mode::Normal, " di", Action::IndexStatus),
    (Mode::Normal, " da", Action::AiStatus),
    // `<space>c` — configuration.
    (Mode::Normal, " cc", Action::SettingsOpen),
    (Mode::Normal, " ck", Action::KeysCheck),
    // `<space>s` — search and what is saved.
    (Mode::Normal, " ss", Action::SearchOpen),
    // Deliberately not `search.explain`: it toggles the why-panel over a
    // *result*, so from the message list it is a key that does nothing where it
    // is bound. It stays where it belongs, on `x` in `Mode::Menu`. The test that
    // walks this map and refuses an inert member is what caught it.
    (Mode::Normal, " sf", Action::FinderOpen),
    (Mode::Normal, " sl", Action::SavedList),
    // `<space>o` — the outbox.
    (Mode::Normal, " oo", Action::OutboxOpen),
    (Mode::Normal, " ou", Action::OutboxCancel),
    // `<space>x` — what is inside a message.
    (Mode::Normal, " xa", Action::AttachList),
    (Mode::Normal, " xl", Action::LinksList),
    // `<space>n` — notes.
    (Mode::Normal, " nl", Action::NoteList),
    (Mode::Normal, " nw", Action::NoteWatch),
    // `<space>g` — going somewhere.
    (Mode::Normal, " gf", Action::FocusFolders),
    (Mode::Normal, " gm", Action::FocusMessages),
    // `<space>w` — what leaves the machine, and what runs on it.
    (Mode::Normal, " ww", Action::WebhookList),
    (Mode::Normal, " wh", Action::HookList),
    // `<space>h` — help.
    (Mode::Normal, " hh", Action::Help),
    (Mode::Normal, " hm", Action::ManualOpen),
    (Mode::Normal, " hg", Action::ManualGrep),
];

impl Default for Keymap {
    fn default() -> Self {
        Self::defaults()
    }
}

impl Keymap {
    /// The built-in bindings.
    ///
    /// Infallible by contract — the TUI must start whatever else is broken —
    /// so a [`DEFAULTS`] entry that failed to parse is logged and skipped
    /// rather than propagated. `defaults_are_all_installable` asserts none
    /// ever is, which is what makes the log line unreachable rather than
    /// merely unlikely.
    #[must_use]
    pub fn defaults() -> Self {
        let mut map = Self {
            layers: BTreeMap::new(),
        };
        for (mode, chord, action) in DEFAULTS {
            match Chord::parse(chord) {
                Ok(chord) => {
                    map.insert(*mode, chord, *action);
                }
                Err(error) => tracing::error!(
                    chord = %chord,
                    mode = %mode.id(),
                    %error,
                    "a built-in key binding does not parse and was skipped",
                ),
            }
        }
        map
    }

    /// A map with no bindings at all — what a test builds on when it wants to
    /// exercise the engine rather than the default bindings. Nothing in the
    /// TUI itself wants a keyboard that does nothing; `pub`, not
    /// `#[cfg(test)]`, because `cfg(test)` is per-crate and `rmail_cli`'s own
    /// `tui::help` test suite (task 102) needs this same isolated fixture
    /// across the crate boundary, where a test-only item in this crate does
    /// not exist at all.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            layers: BTreeMap::new(),
        }
    }

    /// Bind `chord` in `mode`, replacing whatever that chord meant there.
    ///
    /// # Errors
    ///
    /// - [`KeymapError::Reserved`] if the chord starts with `Esc` or `Ctrl-C`.
    /// - [`KeymapError::ChordInInsert`] for a multi-key chord in a mode that
    ///   takes text.
    /// - [`KeymapError::Shadowed`] if the chord is a strict prefix of, or is
    ///   strictly prefixed by, another binding in the same layer. Either way
    ///   one of the two could never be typed (see this module's rule 1), and
    ///   silently making a binding unreachable is worse than refusing the edit.
    pub fn bind(&mut self, mode: Mode, chord: Chord, action: Action) -> Result<(), KeymapError> {
        if chord.is_reserved() {
            return Err(KeymapError::Reserved {
                chord: chord.to_string(),
            });
        }
        if !mode.allows_chords() && chord.keys().len() > 1 {
            return Err(KeymapError::ChordInInsert {
                chord: chord.to_string(),
                mode: mode.id(),
            });
        }
        if let Some(other) = self.shadow_conflict(mode, &chord) {
            return Err(KeymapError::Shadowed {
                chord: chord.to_string(),
                other: other.to_string(),
            });
        }
        self.insert(mode, chord, action);
        Ok(())
    }

    /// Remove `chord` from `mode`. Returns what it was bound to, if anything.
    pub fn unbind(&mut self, mode: Mode, chord: &Chord) -> Option<Action> {
        self.layers.get_mut(&mode)?.remove(chord)
    }

    /// The bindings in one layer, chord order.
    pub fn layer(&self, mode: Mode) -> impl Iterator<Item = (&Chord, Action)> + '_ {
        self.layers
            .get(&mode)
            .into_iter()
            .flat_map(|layer| layer.iter().map(|(chord, action)| (chord, *action)))
    }

    /// Every chord that runs `action` in `mode`, counting inherited layers —
    /// what the help screen prints next to the description.
    #[must_use]
    pub fn chords_for(&self, mode: Mode, action: Action) -> Vec<Chord> {
        let mut found: Vec<Chord> = Vec::new();
        for layer in mode.chain() {
            for (chord, bound) in self.layer(*layer) {
                // A nearer layer's binding for this chord wins, so a chord
                // rebound closer in is not also advertised for what it used
                // to do further out — and a chord bound to the same action in
                // two layers of one chain is still one way to press it.
                if bound == action
                    && self.lookup(mode, chord) == Some(action)
                    && !found.contains(chord)
                {
                    found.push(chord.clone());
                }
            }
        }
        found
    }

    /// The action bound to exactly `chord`, walking `mode`'s layers nearest
    /// first.
    #[must_use]
    pub fn lookup(&self, mode: Mode, chord: &Chord) -> Option<Action> {
        mode.chain()
            .iter()
            .find_map(|layer| self.layers.get(layer)?.get(chord).copied())
    }

    /// Whether some binding in `mode`'s layers is strictly longer than
    /// `chord` and starts with it — i.e. whether waiting for another key
    /// could still produce a match.
    fn has_extension(&self, mode: Mode, chord: &Chord) -> bool {
        mode.chain().iter().any(|layer| {
            self.layers.get(layer).is_some_and(|bindings| {
                // Chords sort lexicographically by key, so every extension of
                // `chord` sorts immediately after it and before any sibling:
                // one range probe answers this without scanning the layer.
                bindings
                    .range((Bound::Excluded(chord), Bound::Unbounded))
                    .next()
                    .is_some_and(|(candidate, _)| candidate.starts_with(chord))
            })
        })
    }

    /// A binding in the same layer that `chord` would make unreachable, or
    /// that would make `chord` unreachable.
    fn shadow_conflict(&self, mode: Mode, chord: &Chord) -> Option<Chord> {
        let layer = self.layers.get(&mode)?;
        layer
            .keys()
            .find(|other| *other != chord && (other.starts_with(chord) || chord.starts_with(other)))
            .cloned()
    }

    fn insert(&mut self, mode: Mode, chord: Chord, action: Action) {
        self.layers.entry(mode).or_default().insert(chord, action);
    }

    /// Fold one key press into `pending` and say what it meant.
    ///
    /// The whole engine is here; see this module's docs for the three rules
    /// it follows and why each departs from vim.
    pub fn resolve(&self, mode: Mode, pending: &mut Pending, key: Key) -> Resolution {
        // A digit only starts a count when no chord is in progress — `2gg` is
        // a count and a chord, `g2` is not. A leading `0` is a key, not a
        // count, exactly as in vim (nothing binds it today; a user may).
        if mode.takes_counts() && pending.keys.is_empty() {
            if let Some(digit) = key.digit() {
                if digit != 0 || pending.count.is_some() {
                    pending.push_digit(digit);
                    return Resolution::Pending;
                }
            }
        }

        let mut keys = std::mem::take(&mut pending.keys);
        keys.push(key);

        loop {
            // `Chord::new` only fails past MAX_CHORD_KEYS, which the pushes
            // below cannot reach: `has_extension` is false once no binding is
            // that long, and the queue shrinks on every other path.
            let Ok(chord) = Chord::new(keys.clone()) else {
                pending.clear();
                return Resolution::Unbound(key);
            };

            if let Some(action) = self.lookup(mode, &chord) {
                let count = pending.take_count();
                pending.clear();
                return Resolution::Run { action, count };
            }

            if keys.len() < MAX_CHORD_KEYS && self.has_extension(mode, &chord) {
                pending.keys = keys;
                return Resolution::Pending;
            }

            // Dead sequence. Drop the key that has been waiting longest and
            // try again with the rest, so a mistyped prefix costs the prefix
            // and not the keystroke that followed it.
            if keys.len() <= 1 {
                let key = keys.first().copied().unwrap_or(key);
                pending.clear();
                tracing::trace!(mode = %mode.id(), key = %key, "unbound key");
                return Resolution::Unbound(key);
            }
            keys.remove(0);
        }
    }
}

// ---------------------------------------------------------------------------
// errors
// ---------------------------------------------------------------------------

/// Why a chord, an action id, or a `keys.toml` could not be used.
///
/// Every variant names the offending text: these are read by someone who just
/// typed `mail keys set` or edited a file by hand, and "invalid binding" would
/// leave them guessing which of forty lines is wrong.
#[derive(Debug, thiserror::Error)]
pub enum KeymapError {
    /// An empty string where a chord was expected.
    #[error("a key binding needs at least one key")]
    EmptyChord,
    /// More keys than [`MAX_CHORD_KEYS`].
    #[error("chord {chord:?} is longer than the {max}-key limit")]
    ChordTooLong {
        /// The chord as written.
        chord: String,
        /// [`MAX_CHORD_KEYS`].
        max: usize,
    },
    /// A `<` with no `>`.
    #[error("chord {chord:?} has an unterminated `<`")]
    Unterminated {
        /// The chord as written.
        chord: String,
    },
    /// A `<name>` that is not a key.
    #[error("chord {chord:?} names an unknown key {name:?} — try <esc>, <enter>, <tab>, <bs>, <up>, <down>, <space>, <lt> or <c-x>")]
    UnknownKey {
        /// The chord as written.
        chord: String,
        /// The unrecognised `<…>` body.
        name: String,
    },
    /// An action id with no action.
    #[error("unknown action {id:?} — `mail keys actions` lists every id")]
    UnknownAction {
        /// The id as written.
        id: String,
    },
    /// A mode name that is not bindable.
    #[error(
        "unknown mode {id:?} — one of normal, viewer, visual, insert, pick, confirm, help \
         (the global layer holds Esc and Ctrl-C and is not configurable)"
    )]
    UnknownMode {
        /// The mode name as written.
        id: String,
    },
    /// `mail keys unset` for a chord the file does not bind.
    #[error("{chord} is not bound in {mode} mode by keys.toml; nothing to unset")]
    NotBound {
        /// The chord as written.
        chord: String,
        /// The mode it was looked for in.
        mode: &'static str,
    },
    /// The in-place edit would have changed something else too.
    #[error(
        "could not change {chord} in {mode} mode without disturbing the rest of keys.toml; \
         nothing was written — edit the file by hand"
    )]
    EditFailed {
        /// The chord being bound.
        chord: String,
        /// The mode it was being bound in.
        mode: &'static str,
    },
    /// A chord starting with `Esc` or `Ctrl-C`.
    #[error("{chord} cannot be bound: Esc and Ctrl-C are how every mode is escaped")]
    Reserved {
        /// The chord as written.
        chord: String,
    },
    /// A multi-key chord in a text-entry mode.
    #[error("{chord} cannot be bound in {mode} mode: a chord there would hold back the keystrokes typed after it")]
    ChordInInsert {
        /// The chord as written.
        chord: String,
        /// The mode it was bound in.
        mode: &'static str,
    },
    /// Two bindings in one layer where one is a prefix of the other.
    #[error("{chord} and {other} cannot both be bound: one of them could never be typed — unbind {other} first")]
    Shadowed {
        /// The chord being bound.
        chord: String,
        /// The binding it collides with.
        other: String,
    },
    /// `keys.toml` is not TOML.
    #[error("{path} is not valid TOML: {source}")]
    Toml {
        /// The file.
        path: String,
        /// What the parser said.
        #[source]
        source: toml::de::Error,
    },
    /// `keys.toml` could not be read or written.
    #[error("{path}: {source}")]
    Io {
        /// The file.
        path: String,
        /// What the filesystem said.
        #[source]
        source: std::io::Error,
    },
}
