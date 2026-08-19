//! The TUI's state machine: [`Model`], [`Msg`], [`Cmd`], and the pure
//! [`update`] that maps one message onto the next state.
//!
//! # Why this module knows nothing about a terminal
//!
//! Everything that decides *what the TUI does* lives here, and nothing here
//! touches a terminal, a socket, or the clock. [`update`] takes a `&mut
//! Model` and a [`Msg`] and returns the [`Cmd`]s the outside world should
//! carry out; it cannot read a key, cannot await an RPC, and cannot draw.
//! Rendering is a separate function of `&Model` (`tui::view`), and the
//! gRPC work is a separate executor (`tui::grpc`).
//!
//! That split is not tidiness for its own sake — it is what makes the
//! behaviour testable. A TUI that interleaves "read a key, decide, issue the
//! call, redraw" inside one loop body can only be tested by driving a real
//! terminal against a real daemon, which in practice means it is not tested
//! at all. Here every navigation rule, every action, and every error path is
//! exercised by calling [`update`] directly (see this module's `tests`), with
//! no terminal attached and no daemon running.
//!
//! # Why [`update`] returns commands instead of performing them
//!
//! prd.md's hard requirement for this screen is that the UI *never blocks on
//! sync or AI*: it reads local state through gRPC and must stay responsive
//! while any of that is outstanding. Making [`update`] able to `.await` an
//! RPC would make violating that rule the path of least resistance — one
//! inline `client.get(..).await?` in a key handler and the event loop stalls
//! for as long as the daemon takes.
//!
//! [`update`] is therefore synchronous by type. The only way it can cause
//! network work is to *return* a [`Cmd`], which `tui::model::drive`'s loop
//! hands to an executor that spawns it as a background task; the result comes
//! back later as another [`Msg`]. Blocking the UI on a round trip is not
//! something this design discourages, it is something it cannot express.
//! [`Model::inflight`] counts the outstanding work purely so the status bar
//! can say so.
//!
//! # Keys
//!
//! No key is decided here. [`on_key`] hands the press to
//! [`crate::keymap`]'s engine along with the mode the model is currently in
//! ([`Model::mode`]), gets back an [`Action`] — a named, rebindable id — and
//! runs it. What is left in this module is the *meaning* of each action,
//! which is genuinely context-sensitive: `cursor.down` moves the folder
//! cursor, the message cursor, the folder picker's cursor or the viewer's
//! scroll depending on what is on screen, and none of those distinctions
//! belong in a key table.

use std::collections::{BTreeSet, VecDeque};

pub mod drive;
pub mod wire;

#[cfg(test)]
mod tests;

use rmail_core::command;
use rmail_core::parity::Command as Capability;

use super::history::History;
use super::manual;
use super::overlays;
use super::overlays::{
    command_matches, complete_operator, AiSummary, AskPane, AskPhase, Browse, Citation,
    CommandPane, Explanation, FinderItem, FinderKind, FinderPane, Hit, OutboxPane, OutboxRow,
    QuickAction, QuickPane, SearchFocus, SearchPane, Toast, UndoToast,
};
use super::report::{self, ReportColumn, ReportFill, ReportPane, ReportRow};
use super::theme::Theme;
pub use crate::keymap::Key;
use crate::keymap::{Action, Keymap, Mode, Pending, Resolution};

/// The IMAP flag marking a message read.
pub const SEEN: &str = "\\Seen";
/// The IMAP flag marking a message flagged/starred.
pub const FLAGGED: &str = "\\Flagged";

/// Folder names, in preference order, that a message is archived *into*.
///
/// There is no `MailService.Archive` RPC and deliberately so: archiving is a
/// move to a folder whose name is a per-server convention, not a distinct
/// server operation. Resolving it here (rather than in the daemon) keeps the
/// convention visible and lets the TUI say "no archive folder on this
/// account" instead of failing an RPC the user cannot act on.
const ARCHIVE_NAMES: &[&str] = &["Archive", "Archives", "All Mail"];

/// The most messages one action may act on at once.
///
/// A visual selection is bounded by the loaded page (`grpc::PAGE_SIZE`, 500
/// rows), and every message in it becomes its own RPC — 500 concurrent IMAP
/// mutations from one keystroke is not a bulk action, it is an outage. The
/// cap is refused loudly rather than silently truncated: acting on the first
/// hundred of what the user selected would be worse than acting on none.
pub const MAX_BULK: usize = 100;

/// The longest undo window that gets a countdown toast, in seconds.
///
/// Past this the entry is a scheduled send rather than an "oops" window, and
/// a toast is the wrong shape for it: it would hold a row of the screen and
/// force a repaint every second until it expired. The outbox pane (`O`) shows
/// those, with no countdown.
pub const MAX_UNDO_TOAST: i64 = 120;

/// How many toasts [`Model::toasts`] holds before the oldest is dropped to
/// make room. Only the front one is ever drawn — the rest are a `+N` badge —
/// so this bounds memory and the badge count, not the screen: past five
/// unread notices the exact number stops mattering next to the fact that
/// there are several.
const MAX_TOASTS: usize = 5;

/// `:set folder-width`/`:set preview-width`'s allowed range, each. Below 10
/// a column cannot hold a folder name or a subject line; above 60 the pane
/// declaring it stops being one of several and starts being the screen.
const MIN_PANE_PCT: u16 = 10;
const MAX_PANE_PCT: u16 = 60;
/// The most [`Model::folder_width_pct`] and [`Model::preview_width_pct`] may
/// sum to, leaving the message list at least this much: `100 -
/// MAX_PANES_PCT`. [`render_panes`] trusts this invariant rather than
/// re-clamping every frame — [`set_option`] is the only place either field
/// is written, and it is where the check belongs once, not on every draw.
const MAX_PANES_PCT: u16 = 90;
/// `:set ai-panel-width`'s allowed range. Below 15 the panel cannot hold a
/// `tl;dr` line; above 60 the message list underneath stops being usable
/// while the panel is open.
const MIN_AI_PANEL_PCT: u16 = 15;
const MAX_AI_PANEL_PCT: u16 = 60;

const DEFAULT_FOLDER_WIDTH_PCT: u16 = 20;
const DEFAULT_PREVIEW_WIDTH_PCT: u16 = 40;
const DEFAULT_AI_PANEL_WIDTH_PCT: u16 = 30;

/// The most characters a text prompt accepts.
///
/// Long enough for any address or subject a person types, short enough that a
/// key held down against a prompt cannot grow a `String` without limit.
pub const MAX_INPUT: usize = 512;

/// One account, as the folder pane needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    /// Row id.
    pub id: i64,
    /// Display name.
    pub name: String,
    /// The login, which for every mail provider this client targets is also
    /// the account's own address — what a draft is sent `From`.
    pub username: Option<String>,
}

/// One folder (an IMAP mailbox) in the folder pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    /// `mailboxes.id`.
    pub id: i64,
    /// Folder name as the server reports it.
    pub name: String,
    /// Messages stored locally for this folder.
    pub message_count: i64,
}

/// One row of the message list. Bodies are never carried here — a list view
/// must not pull a body across the wire per row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRow {
    /// `messages.id`.
    pub id: i64,
    /// Decoded subject, or a placeholder when the message has none.
    pub subject: String,
    /// Display name if the sender had one, else the bare address.
    pub from: String,
    /// The sender's addr-spec — what a reply is addressed to.
    pub from_addr: Option<String>,
    /// `Date` header, unix seconds.
    pub date: Option<i64>,
    /// The message's complete IMAP flag set.
    pub flags: Vec<String>,
    /// Whether the message carries attachments.
    pub has_attachments: bool,
}

impl MessageRow {
    /// Whether the message carries `flag`.
    #[must_use]
    pub fn has_flag(&self, flag: &str) -> bool {
        self.flags.iter().any(|f| f == flag)
    }

    /// The flag set with `flag` present or absent, deduplicated and ordered
    /// so the result is a function of the desired set and not of arrival
    /// order.
    ///
    /// `SetFlags` is a wholesale replace (IMAP `STORE FLAGS` semantics), so a
    /// toggle has to send the complete intended set, not a delta — and it
    /// takes the *intended* state rather than toggling per message, because
    /// over a selection "toggle" has to mean one thing for the whole
    /// selection. Toggling each row independently would leave exactly the
    /// already-read half of a mixed selection unread.
    #[must_use]
    pub fn flags_with(&self, flag: &str, present: bool) -> Vec<String> {
        let mut set: BTreeSet<&str> = self.flags.iter().map(String::as_str).collect();
        if present {
            set.insert(flag);
        } else {
            set.remove(flag);
        }
        set.into_iter().map(str::to_owned).collect()
    }
}

/// A message opened in the viewer: decoded headers and body, ready to draw.
///
/// The decoding — multipart selection, quoted-printable and base64 transfer
/// decoding, RFC 2047 encoded-words — already happened, once, in
/// `rmail_core::message::parse::parse_message` when sync stored the message;
/// `MailService.Get` hands back its output. This type is the *rendered*
/// projection of that, never a second decoder. See [`wire`] for the seam and
/// `wire::tests` for the proof that what reaches this struct is what
/// `parse_message` produced.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct OpenMessage {
    /// `messages.id`.
    pub id: i64,
    /// Header lines to show above the body, in display order.
    pub headers: Vec<(String, String)>,
    /// The plain-text body, split into lines.
    pub body: Vec<String>,
    /// Whether an HTML alternative exists — what enables "open in browser".
    pub has_html: bool,
    /// Attachment descriptions, one per line.
    pub attachments: Vec<String>,
}

/// Which pane has the cursor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// The folder list.
    Folders,
    /// The message list.
    Messages,
}

/// Which screen is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    /// Folders / message list / preview.
    List,
    /// One message, full width, scrollable.
    Viewer,
    /// The built-in manual ([`manual`]). Its state is [`Model::manual`]; see
    /// [`set_screen`] on why that is a second field rather than a payload
    /// here.
    Manual,
}

/// The screen the manual was opened from, so leaving it goes back there.
///
/// A closed set of two rather than a `Screen`, because a `Screen::Manual`
/// stored here would be a state that has to be prevented — [`enter_manual`]
/// navigates within an open manual instead of re-entering it, so the case
/// cannot arise, and a type that cannot express it needs no invariant to say
/// so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// The folder/message list.
    List,
    /// The message viewer.
    Viewer,
}

impl Origin {
    /// Which origin `screen` is. Total: [`Screen::Manual`] maps to
    /// [`Origin::List`], which is where a manual opened from the manual would
    /// have to return to anyway.
    const fn of(screen: Screen) -> Self {
        match screen {
            Screen::Viewer => Self::Viewer,
            Screen::List | Screen::Manual => Self::List,
        }
    }
}

/// Where the manual's search line is aimed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// `/` — this page.
    Page,
    /// `g/` (and `:helpgrep`) — every page.
    Manual,
}

/// The manual's search line, while it is being typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManualPrompt {
    /// What has been typed so far.
    pub pattern: String,
    /// What it will search.
    pub scope: Scope,
}

/// Somewhere the manual has been, for the jump list.
///
/// Carries the highlight as well as the cursor: a jump restores the state of
/// that page, and "which line I was on but not what was lit up on it" is a
/// half-restore that shows through immediately — grep a phrase, open a hit,
/// follow a link out of it, `<c-o>` back, and the hit you came for would be
/// unmarked.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Waypoint {
    at: manual::Location,
    cursor: usize,
    highlight: Option<String>,
}

/// The most pages the jump list remembers in each direction.
///
/// Bounded for the reason every buffer in this crate is: following links for
/// an hour is ordinary use, and it must not grow a `Vec` for as long as it
/// lasts. Sixty-four is far past any reader's memory of where they have been.
pub const MAX_JUMPS: usize = 64;

/// The manual, as the model holds it: where it is, where the cursor is, what
/// is being searched, and how to get back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManualState {
    /// The page (or hit list) on screen.
    pub at: manual::Location,
    /// Cursor within the rendered lines. The rendered document is *not* kept
    /// here — [`manual::doc`] is a pure function of the location and the
    /// keymap, cheap enough to call per frame, and a stored copy is a copy
    /// that goes stale the moment `keys.toml` is saved.
    pub cursor: usize,
    /// The search line, while one is up.
    pub prompt: Option<ManualPrompt>,
    /// The submitted in-page pattern still being highlighted.
    pub highlight: Option<String>,
    /// Where leaving the manual returns to.
    from: Origin,
    /// Pages left behind by following a link. `<c-o>` pops.
    back: Vec<Waypoint>,
    /// Pages left behind by `<c-o>`. `<c-i>` pops.
    forward: Vec<Waypoint>,
}

impl ManualState {
    fn new(at: manual::Location, from: Origin) -> Self {
        Self {
            at,
            cursor: 0,
            prompt: None,
            highlight: None,
            from,
            back: Vec::new(),
            forward: Vec::new(),
        }
    }

    /// Whether the search line is up — what makes the manual's mode
    /// [`Mode::Prompt`] rather than [`Mode::Help`].
    #[must_use]
    pub fn typing(&self) -> bool {
        self.prompt.is_some()
    }

    /// The pattern the page is highlighted for: whatever is being typed into
    /// an in-page search (so it previews as it is typed), otherwise the last
    /// one submitted.
    #[must_use]
    pub fn pattern(&self) -> Option<&str> {
        match self.prompt.as_ref() {
            Some(prompt) if prompt.scope == Scope::Page => Some(prompt.pattern.as_str()),
            _ => self.highlight.as_deref(),
        }
    }

    /// The cursor, clamped to a page of `lines` rows.
    ///
    /// The stored cursor can legitimately point past the end: nothing
    /// re-clamps it when the page it is on gets *shorter* underneath it, and
    /// two things do that without a key being pressed — a `keys.toml` reload
    /// shrinking the generated key reference, and `<c-i>`/`<c-o>` restoring a
    /// waypoint's cursor onto a page that has since changed length. Clamping
    /// on read rather than trying to find every writer is what keeps `k` from
    /// needing twenty presses to move a highlighted row, and `<enter>` from
    /// reporting "no link on this line" about a row that visibly has one.
    #[must_use]
    pub fn cursor_in(&self, lines: usize) -> usize {
        self.cursor.min(lines.saturating_sub(1))
    }

    /// Whether `<c-o>` has anywhere to go — what the pane title's marker says.
    #[must_use]
    pub fn can_jump_back(&self) -> bool {
        !self.back.is_empty()
    }

    /// Whether `<c-i>` has anywhere to go.
    #[must_use]
    pub fn can_jump_forward(&self) -> bool {
        !self.forward.is_empty()
    }

    fn here(&self) -> Waypoint {
        Waypoint {
            at: self.at.clone(),
            cursor: self.cursor,
            highlight: self.highlight.clone(),
        }
    }

    /// Go to `to`, remembering where we were.
    fn go(&mut self, to: manual::Location) {
        if to == self.at {
            return;
        }
        let here = self.here();
        push_bounded(&mut self.back, here);
        // Following a link is a new branch: what was ahead is no longer
        // reachable from here, exactly as in a browser or vim's jump list.
        self.forward.clear();
        self.at = to;
        self.cursor = 0;
        self.prompt = None;
        self.highlight = None;
    }

    fn jump(&mut self, jump: Jump) -> bool {
        let (from, to) = match jump {
            Jump::Back => (&mut self.back, &mut self.forward),
            Jump::Forward => (&mut self.forward, &mut self.back),
        };
        let Some(waypoint) = from.pop() else {
            return false;
        };
        let here = Waypoint {
            at: std::mem::replace(&mut self.at, waypoint.at),
            cursor: self.cursor,
            highlight: self.highlight.take(),
        };
        push_bounded(to, here);
        self.cursor = waypoint.cursor;
        self.highlight = waypoint.highlight;
        self.prompt = None;
        true
    }
}

/// Push onto a jump stack, dropping the oldest entry at [`MAX_JUMPS`].
fn push_bounded(stack: &mut Vec<Waypoint>, waypoint: Waypoint) {
    if stack.len() >= MAX_JUMPS {
        stack.remove(0);
    }
    stack.push(waypoint);
}

/// Which way through the jump list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Jump {
    /// `<c-o>`.
    Back,
    /// `<c-i>`.
    Forward,
}

/// What a folder-picker overlay is picking a destination for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickFor {
    /// `MailService.Copy`.
    Copy,
    /// `MailService.Move`.
    Move,
}

/// What a text-input overlay is collecting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputFor {
    /// The recipient of a forward. A forwarded message has no natural
    /// recipient to default to, and `ComposeService.CreateDraft` rejects a
    /// draft with no recipient at all, so it has to be asked for.
    ForwardTo,
}

/// A modal layer drawn over the main screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Overlay {
    /// The `?` key binding reference.
    Help,
    /// Pick a destination folder.
    Pick {
        /// What the pick is for.
        what: PickFor,
        /// The messages the pick applies to.
        ///
        /// Captured when the overlay opens, never re-derived when it closes:
        /// the message list is live (a `Msg::Changed` reload can arrive and
        /// re-clamp the cursor while the picker is up) and the viewer's
        /// message is not the one under the list cursor at all. Resolving the
        /// target late moved a message the user had not selected.
        message_ids: Vec<i64>,
        /// Cursor within the folder list.
        idx: usize,
    },
    /// Confirm a destructive action.
    Confirm {
        /// What is being asked.
        prompt: String,
        /// What `y` does.
        then: Confirmed,
    },
    /// Collect a line of text.
    Input {
        /// What is being asked.
        prompt: String,
        /// What has been typed so far.
        buffer: String,
        /// What the text is for.
        what: InputFor,
        /// The message the input applies to.
        message_id: i64,
    },
    /// `/` — streaming ranked search.
    Search(Box<SearchPane>),
    /// `Ctrl-P` — the fuzzy finder.
    Finder(Box<FinderPane>),
    /// `:` — the command line (task 89). `Ctrl-K` opens the same overlay:
    /// the palette's ranked "run a command by name" is this pane's match
    /// list.
    Command(Box<CommandPane>),
    /// `A` — the ask pane.
    Ask(Box<AskPane>),
    /// `O` — the outbox pseudo-folder.
    Outbox(Box<OutboxPane>),
    /// `.` — the AI quick-action menu.
    Quick(QuickPane),
    /// The answer to a `:` verb that reports rows (task 90).
    Report(Box<ReportPane>),
}

/// What answering `y` to an [`Overlay::Confirm`] does.
///
/// A closed vocabulary rather than a `Vec<Cmd>` captured when the question was
/// asked: the commands a confirmation implies depend on the model *at the
/// moment it is answered* (how many messages are selected, which folder is
/// open), and a pre-built list would be a decision taken before the user had
/// agreed to it. It is also what keeps `Overlay` comparable with `assert_eq!`,
/// which most of this module's tests rely on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirmed {
    /// Expunge these messages — the `d` key's question since task 83.
    ///
    /// The ids are captured when the question is asked, never re-derived when
    /// it is answered: the message list is live (a `Msg::Changed` reload can
    /// arrive and re-clamp the cursor while the question is up) and the
    /// viewer's message is not the one under the list cursor at all.
    Delete(Vec<i64>),
    /// Run this `:` line — what a report row whose verb mutates asks about
    /// first (task 90).
    Invoke {
        /// What to run. Always carries `bang: true`, so dispatching it does
        /// not re-open a confirmation the user has just answered — the same
        /// `!` the command line means by "skip the question", so there is one
        /// implementation of skipping it rather than a second, report-only one.
        invocation: Box<command::Invocation>,
        /// The report the question was asked over, put back whichever way it
        /// is answered.
        ///
        /// Carried rather than re-derived because this model has one overlay
        /// and not a stack — task 89 recorded that absence deliberately — so a
        /// question asked over a report either travels with it or loses it. A
        /// row that acts on what the report is showing and takes the report
        /// down with it is not a mechanism tasks 94 onward can use: a
        /// suggestion list whose rows accept inline would close itself on the
        /// first acceptance.
        over: Box<ReportPane>,
    },
}

impl Overlay {
    /// Where the list cursor is and how long the list is, for the overlays
    /// that have one. `None` for an overlay with nothing to move through.
    ///
    /// The folder picker is deliberately absent: it predates this and carries
    /// its own `idx`, which `Cursor::Pick` still drives.
    fn list_cursor(&self) -> Option<(usize, usize)> {
        Some(match self {
            Self::Search(pane) => (pane.cursor, pane.hits.len()),
            Self::Finder(pane) => (pane.cursor, pane.items.len()),
            Self::Ask(pane) => (pane.cursor, pane.citations.len()),
            Self::Outbox(pane) => (pane.cursor, pane.rows.len()),
            Self::Quick(pane) => (pane.cursor, QuickAction::ALL.len()),
            Self::Report(pane) => (pane.cursor, pane.rows.len()),
            // The command line is absent on purpose: its `<up>`/`<down>`
            // walk the history rather than a list, which is what `:` means
            // everywhere else it exists. Its ranked matches are a preview
            // with no cursor — `<tab>` is what puts one into the line.
            Self::Help
            | Self::Pick { .. }
            | Self::Confirm { .. }
            | Self::Input { .. }
            | Self::Command(_) => return None,
        })
    }

    /// Put that cursor at `at`. Out-of-range values are the caller's problem;
    /// every caller here clamps first (see `move_cursor`).
    fn set_list_cursor(&mut self, at: usize) {
        match self {
            Self::Search(pane) => pane.cursor = at,
            Self::Finder(pane) => pane.cursor = at,
            Self::Ask(pane) => pane.cursor = at,
            Self::Outbox(pane) => pane.cursor = at,
            Self::Quick(pane) => pane.cursor = at,
            Self::Report(pane) => pane.cursor = at,
            Self::Help
            | Self::Pick { .. }
            | Self::Confirm { .. }
            | Self::Input { .. }
            | Self::Command(_) => {}
        }
    }
}

/// Severity of the status line's current message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Ordinary progress.
    Info,
    /// Something failed. Rendered in red.
    Error,
}

/// What a completed action did to the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    /// The message is gone from this folder (moved, archived or deleted).
    Removed(i64),
    /// The message's flag set is now this.
    Flags {
        /// Which message.
        message_id: i64,
        /// Its complete new flag set.
        flags: Vec<String>,
    },
    /// A draft was created.
    Drafted(i64),
    /// The action changed nothing locally (a copy leaves the source alone;
    /// the copy itself is discovered by the destination folder's next sync).
    None,
}

/// Something that happened, from anywhere: the keyboard, a finished RPC, or
/// the daemon's event stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    /// Sent once, before the first key, to kick off the initial loads.
    Boot,
    /// A key press.
    Key(Key),
    /// `AccountService.List` finished.
    Accounts(Result<Vec<Account>, String>),
    /// `SyncService.Status` finished — the folder list.
    Folders(Result<Vec<Folder>, String>),
    /// `MailService.List` finished.
    Messages {
        /// Which folder was listed.
        mailbox_id: i64,
        /// The rows, newest first.
        result: Result<Vec<MessageRow>, String>,
    },
    /// `MailService.Get` finished.
    Opened {
        /// The message that was asked for.
        message_id: i64,
        /// It, decoded, or why it could not be fetched.
        result: Result<OpenMessage, String>,
    },
    /// A mutation or draft RPC finished.
    Done {
        /// What to say in the status line on success.
        label: String,
        /// What it did, or why it failed.
        result: Result<Effect, String>,
    },
    /// The daemon's event log reported a change to the open folder. Carries
    /// no payload beyond "something changed" on purpose: the model re-reads
    /// local state rather than trying to patch rows from an event.
    Changed,
    /// The `WatchEvents` subscription ended, with the reason.
    ///
    /// Deliberately not a [`Msg::Done`]: nobody asked for the stream and
    /// nothing counted it into [`Model::inflight`], so reporting it as a
    /// finished request would decrement a counter it never incremented.
    LiveUpdatesStopped(String),
    /// One frame of a `SearchService.Search` stream.
    Search {
        /// Which query it belongs to. A frame from a superseded one is
        /// dropped; see `overlays`' module docs.
        generation: u64,
        /// What arrived.
        event: SearchEvent,
    },
    /// One frame of a `FinderService.Find` stream.
    Finder {
        /// Which query it belongs to.
        generation: u64,
        /// What arrived.
        event: FinderEvent,
    },
    /// One frame of an `AiService.AskMailbox` stream.
    Ask {
        /// Which question it belongs to.
        generation: u64,
        /// What arrived.
        event: AskEvent,
    },
    /// `SearchService.Explain` finished.
    Explained {
        /// Which hit was explained.
        message_id: i64,
        /// The breakdown, or why it could not be produced.
        result: Result<Explanation, String>,
    },
    /// `AiService.GetSummary` or `SuggestReply` finished.
    Summarized {
        /// Which message.
        message_id: i64,
        /// Its analysis, or why it could not be fetched.
        result: Result<AiSummary, String>,
    },
    /// `SendSchedulerService.ListOutbox` finished.
    Outbox {
        /// The wall clock as the executor read it, unix seconds.
        ///
        /// Carried rather than read here because [`update`] has no clock —
        /// deciding whether an undo window is still open needs one, and the
        /// only way a pure function gets it is as data.
        now: i64,
        /// The entries, or why they could not be listed.
        result: Result<Vec<OutboxRow>, String>,
    },
    /// A second passed while an undo countdown was running, unix seconds.
    Tick(i64),
    /// One frame of a Report's answer (task 90).
    Report {
        /// Which request it belongs to. A frame from a superseded one is
        /// dropped; see `tui::report`'s module docs.
        generation: u64,
        /// What arrived.
        event: ReportEvent,
    },
    /// `keys.toml` was read (see [`crate::keymap::file::Source`]).
    Keymap {
        /// The bindings to switch to, or why the file was refused — in which
        /// case the ones already loaded keep working.
        result: Result<Keymap, String>,
        /// Whether to say so in the status line. False for the silent load at
        /// startup; see [`crate::keymap::file::Reload`].
        announce: bool,
    },
}

/// What a `SearchService.Search` stream delivered.
///
/// Hits are appended as they arrive: unlike the finder's, this stream sends
/// each hit once, in rank order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SearchEvent {
    /// One ranked hit.
    Hit(Box<Hit>),
    /// The stream ended, cleanly or not.
    Done(Result<(), String>),
}

/// What a `FinderService.Find` stream delivered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FinderEvent {
    /// A complete snapshot of the current top-K. Replaces what is shown.
    Batch {
        /// The rows, descending.
        items: Vec<FinderItem>,
        /// Whether the scan has finished.
        complete: bool,
        /// Whether it finished because a newer `Find` superseded it. A
        /// superseded stream ends *cleanly*; it is not an error to report.
        superseded: bool,
        /// Entries walked so far.
        scanned: u64,
    },
    /// The stream failed.
    Failed(String),
}

/// What a reporting verb delivered.
///
/// One frame type for the unary and the streaming case alike: `:auth status`
/// sends a single [`ReportEvent::Frame`] with `complete: true`, and a streaming
/// verb sends several with `complete: false` before its last. The pane cannot
/// tell them apart, which is what makes "line, table and stream are the same
/// thing here" true rather than aspirational.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportEvent {
    /// Rows, and how they join the ones already shown.
    Frame {
        /// Extend or replace.
        fill: ReportFill,
        /// The rows themselves.
        rows: Vec<ReportRow>,
        /// Whether this is the last frame.
        complete: bool,
    },
    /// The report failed. Rows already shown are kept — see
    /// [`ReportPane::fail`].
    Failed(String),
}

/// What an `AiService.AskMailbox` stream delivered.
///
/// The order is fixed by the proto: one trace, then tokens, then citations,
/// then done. Citations arrive *after* the prose because an inline `[n]` is
/// only resolvable once the whole answer has been seen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AskEvent {
    /// What retrieval found, pre-formatted.
    Trace(String),
    /// A chunk of prose.
    Token(String),
    /// A source the answer pointed at.
    Cite(Box<Citation>),
    /// The terminal frame.
    Done {
        /// The **daemon's** verdict on whether the answer cited anything
        /// real. Never the model's claim about itself.
        grounded: bool,
        /// Why not, when it is not.
        refusal: String,
    },
    /// The stream failed.
    Failed(String),
}

/// Work for the outside world. Returned by [`update`], never performed by it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    /// `AccountService.List`.
    LoadAccounts,
    /// `SyncService.Status` for one account.
    LoadFolders {
        /// Which account.
        account_id: i64,
    },
    /// `MailService.List` for one folder.
    LoadMessages {
        /// Which folder.
        mailbox_id: i64,
    },
    /// `MailService.Get`.
    Open {
        /// Which message.
        message_id: i64,
    },
    /// `MailService.WatchEvents` — a long-lived stream, started once.
    Watch {
        /// Which account's events.
        account_id: i64,
    },
    /// `MailService.SetFlags` (a wholesale replace).
    SetFlags {
        /// Which message.
        message_id: i64,
        /// Its complete new flag set.
        flags: Vec<String>,
        /// Status-line text on success.
        label: String,
    },
    /// `MailService.Move`.
    Move {
        /// Which message.
        message_id: i64,
        /// Where to.
        dest_mailbox_id: i64,
        /// Status-line text on success.
        label: String,
    },
    /// `MailService.Copy`.
    Copy {
        /// Which message.
        message_id: i64,
        /// Where to.
        dest_mailbox_id: i64,
    },
    /// `MailService.Delete` — expunges on the server.
    Delete {
        /// Which message.
        message_id: i64,
    },
    /// `ComposeService.CreateDraft`.
    Draft {
        /// Reply or forward.
        kind: DraftKind,
        /// The account the draft belongs to.
        account_id: i64,
        /// The address the draft is sent from.
        from: String,
        /// The address the draft is sent to.
        to: String,
        /// The message being replied to or forwarded.
        message_id: i64,
    },
    /// Write the message's HTML alternative to a private temp file and hand
    /// it to the platform opener.
    OpenHtml {
        /// Which message.
        message_id: i64,
    },
    /// `SearchService.Search`. Debounced and superseded by the executor; the
    /// generation is how a late frame from an abandoned query is recognised.
    Search {
        /// The raw query, sigils and operators included, unparsed.
        query: String,
        /// Which query this is.
        generation: u64,
        /// The account to search, or 0 for all of them.
        account_id: i64,
    },
    /// `FinderService.Find`.
    Find {
        /// The raw prompt, sigil included, unparsed.
        query: String,
        /// Which query this is.
        generation: u64,
        /// The account to search, or 0 for all of them.
        account_id: i64,
    },
    /// `SearchService.Explain` for one hit of the current query.
    Explain {
        /// The query the hit came from.
        query: String,
        /// Which hit.
        message_id: i64,
        /// The account it came from, or 0 for all of them.
        account_id: i64,
    },
    /// `AiService.AskMailbox`.
    Ask {
        /// The question.
        question: String,
        /// Which question this is.
        generation: u64,
        /// The account to search, or 0 for all of them.
        account_id: i64,
    },
    /// `AiService.GetSummary`, or `SuggestReply` when `suggest_reply`.
    LoadSummary {
        /// Which message.
        message_id: i64,
        /// Whether to spend a model call on a reply suggestion rather than
        /// read what the triage/deep passes already cached.
        suggest_reply: bool,
    },
    /// `SendSchedulerService.ListOutbox`.
    LoadOutbox {
        /// Which account.
        account_id: i64,
    },
    /// `SendSchedulerService.CancelScheduled` — the undo.
    CancelSend {
        /// Which entry.
        outbox_id: i64,
    },
    /// Deliver [`Msg::Tick`] once a second until `until` passes, so the undo
    /// countdown can move without [`update`] owning a clock.
    Countdown {
        /// Unix seconds to stop at.
        until: i64,
    },
    /// Write the `:` command line's history to its file.
    ///
    /// A [`Cmd`] rather than a write inside [`update`] for the reason every
    /// other side effect is one: `update` is pure, synchronous and clockless,
    /// and a filesystem call in it would be all three of those things
    /// undone. The whole list travels rather than the new line, so the writer
    /// needs no state of its own and a dropped write is corrected by the next
    /// one instead of leaving a gap.
    SaveHistory {
        /// Every recorded line, oldest first.
        entries: Vec<String>,
    },
    /// `ClientAuthService.AuthStatus` — the `:auth status` report.
    AuthStatus {
        /// Which report this is, so a frame from a superseded run is
        /// recognisable.
        generation: u64,
    },
    /// `ClientAuthService.ClearPassword` — remove the password gate.
    AuthClear,
    /// Stop a stream nobody is reading any more.
    ///
    /// Leaving an overlay is the one case the generation stamp does not
    /// cover. A stale *frame* is free to ignore; a stale *stream* is not —
    /// `AskMailbox` is a retrieval pass plus a model completion, and letting
    /// it run to completion after `Esc` bills the user for an answer that
    /// will never be drawn. CLAUDE.md's "honor cancellation" is about exactly
    /// this.
    CancelStream {
        /// Which one.
        which: Stream,
    },
}

/// A long-running client stream, named so it can be cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stream {
    /// `SearchService.Search`.
    Search,
    /// `FinderService.Find`.
    Find,
    /// `AiService.AskMailbox`.
    Ask,
    /// `SearchService.Explain` — superseded per cursor row, so it is a stream
    /// slot even though the RPC itself is unary.
    Explain,
    /// Whatever is feeding the Report overlay (task 90).
    ///
    /// One slot for every reporting verb rather than one per verb: only one
    /// report is on screen at a time, so a second one starting is always a
    /// supersession of the first — and a unary report (`:auth status`) shares
    /// the slot for the same reason `Explain` has one, so `Esc` has exactly one
    /// thing to cancel whichever kind is running.
    Report,
}

/// Which kind of draft `Cmd::Draft` creates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DraftKind {
    /// A reply to the message's sender.
    Reply,
    /// A forward to an address the user typed.
    Forward,
}

/// Everything the TUI draws, and everything a key press acts on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    /// The account being shown, once `AccountService.List` has answered.
    ///
    /// One at a time: switching accounts inside the session belongs with the
    /// modal keymap and the overlays (tasks 84/85), and `mail tui --account`
    /// covers the multi-account case in the meantime.
    pub account: Option<Account>,
    /// The account id `--account` asked for, if any.
    preferred_account: Option<i64>,
    /// Folders of the shown account.
    pub folders: Vec<Folder>,
    /// Cursor within [`Model::folders`].
    pub folder_idx: usize,
    /// The folder whose messages [`Model::messages`] holds, if any.
    pub open_folder: Option<i64>,
    /// Rows of the open folder, newest first.
    pub messages: Vec<MessageRow>,
    /// Cursor within [`Model::messages`].
    pub message_idx: usize,
    /// The message the viewer is showing.
    pub open: Option<OpenMessage>,
    /// The message a `MailService.Get` is outstanding for, if any. What makes
    /// a late `Msg::Opened` recognisable as stale.
    opening: Option<i64>,
    /// Viewer scroll offset, in lines.
    pub scroll: usize,
    /// Which pane has the cursor.
    pub focus: Focus,
    /// Which screen is showing.
    pub screen: Screen,
    /// The manual's state, when it is the screen. `Some` exactly when
    /// [`Model::screen`] is [`Screen::Manual`]; [`set_screen`] is the only
    /// place either is assigned, and `the_manual_state_and_the_screen_agree`
    /// is what holds them to it.
    pub manual: Option<ManualState>,
    /// The modal layer, if any.
    pub overlay: Option<Overlay>,
    /// Whether the collapsible AI panel is showing.
    pub ai_panel: bool,
    /// The analysis the AI panel is showing, if it has arrived.
    pub summary: Option<AiSummary>,
    /// The message a `GetSummary`/`SuggestReply` is outstanding for. What
    /// keeps the panel from re-requesting the same message on every keystroke
    /// while the first request is still in flight.
    summary_for: Option<i64>,
    /// The message whose analysis could not be fetched. Without it, a failure
    /// would be re-requested by the next message to arrive, forever.
    summary_failed: Option<i64>,
    /// The message the `.` menu aimed the panel at, if any. Holds the panel
    /// still until the user deliberately moves off it.
    summary_pinned: Option<i64>,
    /// The `:` command line's history, oldest first.
    pub history: History,
    /// Whether [`Model::history`] has a line the file does not.
    ///
    /// A flag rather than a write, because `update` is pure and synchronous:
    /// the write is a [`Cmd`], issued by whichever call next has a `Vec<Cmd>`
    /// to put it in, so the model never touches a filesystem.
    pub pending_history: bool,
    /// The bottom-row notification queue: at most [`MAX_TOASTS`], oldest
    /// dropped first. [`Model::shown_toast`] is what a render reads; this
    /// field is public because `view` also needs the raw count for the `+N`
    /// badge, and a second accessor for that alone would be pure ceremony.
    pub toasts: VecDeque<Toast>,
    /// Stamped onto every streaming command so a frame from a superseded
    /// query can be told from a frame of the current one. Monotonic for the
    /// life of the session.
    generation: u64,
    /// Where a visual selection started, when one is running. The other end
    /// is [`Model::message_idx`], so extending the selection is the ordinary
    /// cursor movement and needs no second set of bindings.
    pub visual: Option<usize>,
    /// The bindings in force. Replaced wholesale when `keys.toml` changes;
    /// never patched, so a half-applied reload cannot exist.
    pub keymap: Keymap,
    /// What has been typed towards a binding but has not resolved yet.
    pub pending: Pending,
    /// How many background requests are outstanding. Displayed, never waited
    /// on.
    pub inflight: usize,
    /// The status line's current text.
    pub status: String,
    /// The status line's severity.
    pub level: Level,
    /// Set once the user has asked to leave; the drive loop stops on it.
    pub quit: bool,
    /// The active color/style theme. Lives here rather than as a parameter
    /// `view::render` takes alongside `Model`, so `render` stays a pure
    /// function of one argument — the module doc's own description of it —
    /// and so a future `:set theme <name>` command is an ordinary state
    /// mutation, not a second channel into the renderer.
    pub theme: Theme,
    /// The folder column's share of the 3-pane (and 2-pane) layout, as a
    /// percentage. `:set folder-width` tunes it; [`set_option`] is the only
    /// writer and enforces the bound `render_panes` relies on without
    /// re-checking: `folder_width_pct + preview_width_pct <= MAX_PANES_PCT`.
    pub folder_width_pct: u16,
    /// The preview column's share of the 3-pane layout, as a percentage.
    /// `:set preview-width` tunes it; see [`Model::folder_width_pct`] for the
    /// invariant the two are held to together.
    pub preview_width_pct: u16,
    /// The collapsible AI panel's share of the width it is given, as a
    /// percentage. `:set ai-panel-width` tunes it.
    pub ai_panel_width_pct: u16,
}

impl Default for Model {
    fn default() -> Self {
        Self::new()
    }
}

impl Model {
    /// An empty model showing whichever account the daemon lists first.
    #[must_use]
    pub fn new() -> Self {
        Self::for_account(None)
    }

    /// An empty model — what the very first frame paints, before any data has
    /// arrived — showing account `preferred_account` if it exists.
    ///
    /// Deliberately cheap and infallible: prd.md budgets 200 ms for TUI
    /// startup, and the way to meet that is to paint an empty frame
    /// immediately and fill it as responses land, never to hold the first
    /// frame back until a folder listing completes.
    #[must_use]
    pub fn for_account(preferred_account: Option<i64>) -> Self {
        Self {
            account: None,
            preferred_account,
            folders: Vec::new(),
            folder_idx: 0,
            open_folder: None,
            messages: Vec::new(),
            message_idx: 0,
            open: None,
            opening: None,
            scroll: 0,
            focus: Focus::Messages,
            screen: Screen::List,
            manual: None,
            overlay: None,
            ai_panel: false,
            summary: None,
            summary_for: None,
            summary_failed: None,
            summary_pinned: None,
            history: History::default(),
            pending_history: false,
            toasts: VecDeque::new(),
            generation: 0,
            visual: None,
            keymap: Keymap::defaults(),
            pending: Pending::default(),
            inflight: 0,
            status: "connecting…".to_owned(),
            level: Level::Info,
            quit: false,
            theme: Theme::default(),
            folder_width_pct: DEFAULT_FOLDER_WIDTH_PCT,
            preview_width_pct: DEFAULT_PREVIEW_WIDTH_PCT,
            ai_panel_width_pct: DEFAULT_AI_PANEL_WIDTH_PCT,
        }
    }

    /// The toast a render draws, ranked rather than positional: the undo
    /// countdown first — it is the one with a clock on it, and `u` only
    /// ever cancels whichever send that clock belongs to — then the newest
    /// [`Toast::Priority`] (its own doc comment: "ranked to interrupt", so a
    /// stale [`Toast::Completion`] must not sit in front of one), then
    /// whatever is newest overall. Newest, not oldest: past
    /// [`MAX_TOASTS`], [`push_toast`] evicts the oldest survivor, and a
    /// front-first pick would then show the one entry a person has had the
    /// longest chance to already read.
    #[must_use]
    pub fn shown_toast(&self) -> Option<&Toast> {
        self.toasts
            .iter()
            .find(|toast| matches!(toast, Toast::Undo(_)))
            .or_else(|| {
                self.toasts
                    .iter()
                    .rev()
                    .find(|toast| matches!(toast, Toast::Priority { .. }))
            })
            .or_else(|| self.toasts.back())
    }

    /// Whether the `.` menu is holding the AI panel on a message rather than
    /// letting it follow the cursor. A method rather than a public field
    /// (unlike [`Model::ai_panel`]) because [`Model::summary_pinned`] itself
    /// stays private — nothing outside `update` needs the pinned message's
    /// id, only whether one is pinned.
    #[must_use]
    pub fn is_summary_pinned(&self) -> bool {
        self.summary_pinned.is_some()
    }

    /// The row under the message cursor.
    #[must_use]
    pub fn current_message(&self) -> Option<&MessageRow> {
        self.messages.get(self.message_idx)
    }

    /// The folder under the folder cursor.
    #[must_use]
    pub fn current_folder(&self) -> Option<&Folder> {
        self.folders.get(self.folder_idx)
    }

    /// The account whose mail is shown.
    #[must_use]
    pub fn current_account(&self) -> Option<&Account> {
        self.account.as_ref()
    }

    /// Which layer of bindings a key press is read against.
    ///
    /// Derived from what is on screen rather than stored, so there is no way
    /// for the mode to disagree with what the user is looking at — the class
    /// of bug where a modal closes and the keyboard stays trapped in it.
    #[must_use]
    pub fn mode(&self) -> Mode {
        match &self.overlay {
            Some(Overlay::Help) => Mode::Help,
            Some(Overlay::Pick { .. }) => Mode::Pick,
            Some(Overlay::Confirm { .. }) => Mode::Confirm,
            Some(Overlay::Input { .. }) => Mode::Insert,
            // The two overlays that change mode part-way through: search
            // starts on the query line and moves to its results, ask starts
            // on the question and moves to the answer. Deriving the mode from
            // that state rather than storing it is what stops a pane from
            // being in one mode while it draws the other.
            Some(Overlay::Search(pane)) if pane.typing() => Mode::Prompt,
            Some(Overlay::Ask(pane)) if pane.typing() => Mode::Prompt,
            Some(Overlay::Finder(_) | Overlay::Command(_)) => Mode::Prompt,
            Some(
                Overlay::Search(_)
                | Overlay::Ask(_)
                | Overlay::Outbox(_)
                | Overlay::Quick(_)
                | Overlay::Report(_),
            ) => Mode::Menu,
            None => match self.screen {
                // Checked before the visual selection, not after: the manual
                // can be opened mid-selection and read, and while it is on
                // screen the keyboard belongs to it. The selection is still
                // there when it closes.
                Screen::Manual => match self.manual.as_ref() {
                    Some(manual) if manual.typing() => Mode::Prompt,
                    _ => Mode::Help,
                },
                // Gated on the list rather than checked before the screen.
                // `Model::visual` deliberately *outlives* leaving the list —
                // opening a search hit found mid-selection puts the viewer up
                // with the anchor still set — and the mode has to follow what
                // is on screen, so `-- VISUAL --` over a full-width message
                // (and `o` meaning swap-ends there rather than open-html) is
                // wrong. [`Model::selection`] draws the same line for the same
                // reason.
                Screen::List if self.is_selecting() => Mode::Visual,
                Screen::List => Mode::Normal,
                Screen::Viewer => Mode::Viewer,
            },
        }
    }

    /// The rows a visual selection covers, low index first, or `None` when
    /// there is no selection.
    ///
    /// `None` on any screen but the list, whatever [`Model::visual`] holds. A
    /// selection is a range of *these rows*, and the rows are only on screen
    /// on the list — but the anchor legitimately outlives leaving it (open a
    /// hit from a search made mid-selection, or read the manual and come
    /// back), so the anchor is kept and the *range* is what stops existing.
    ///
    /// This is what keeps a bulk action from mutating mail behind a screen
    /// that is not the list at all: with the manual open over a selection and
    /// `message.archive` rebound into the `help` layer, `targets` would
    /// otherwise resolve to the rows underneath and archive them — from a page
    /// of prose, with the list not even drawn.
    #[must_use]
    pub fn selection(&self) -> Option<(usize, usize)> {
        if self.screen != Screen::List {
            return None;
        }
        let anchor = self.visual?;
        if self.messages.is_empty() {
            return None;
        }
        let last = self.messages.len() - 1;
        let (from, to) = if anchor <= self.message_idx {
            (anchor, self.message_idx)
        } else {
            (self.message_idx, anchor)
        };
        Some((from.min(last), to.min(last)))
    }

    /// Whether a selection is on screen at all.
    ///
    /// The question every caller that reads [`Model::visual`] is actually
    /// asking, spelled once. Reading the field raw is not the same question —
    /// the anchor deliberately outlives leaving the list — and having some
    /// callers ask one and some the other is how `a` came to archive the
    /// viewer's message while `r` refused, in the same state, citing a
    /// selection that was not drawn anywhere.
    #[must_use]
    pub fn is_selecting(&self) -> bool {
        self.selection().is_some()
    }

    /// Whether row `idx` is inside the visual selection.
    #[must_use]
    pub fn is_selected(&self, idx: usize) -> bool {
        self.selection()
            .is_some_and(|(from, to)| (from..=to).contains(&idx))
    }

    fn info(&mut self, text: impl Into<String>) {
        self.status = text.into();
        self.level = Level::Info;
    }

    fn fail(&mut self, text: impl Into<String>) {
        self.status = text.into();
        self.level = Level::Error;
    }

    /// Keep both cursors — and a visual selection's anchor — inside their
    /// lists after rows arrive or vanish.
    fn clamp(&mut self) {
        self.message_idx = self.message_idx.min(self.messages.len().saturating_sub(1));
        self.folder_idx = self.folder_idx.min(self.folders.len().saturating_sub(1));
        // A selection whose rows are gone is not a selection. Leaving the
        // anchor dangling would make `selection()` report a range over rows
        // the user never picked once the list reloaded shorter.
        self.visual = match (self.visual, self.messages.len()) {
            (_, 0) => None,
            (Some(anchor), len) => Some(anchor.min(len - 1)),
            (None, _) => None,
        };
    }
}

/// Change screens, keeping [`Model::screen`] and [`Model::manual`] in step.
///
/// The two are one piece of state spread over two fields, and this is the only
/// place either is assigned (besides [`enter_manual`], which sets both at
/// once). Putting [`ManualState`] *inside* the [`Screen::Manual`] variant
/// would make the invariant structural and was the first design — but
/// [`Screen`] is `Copy` and compared with `==` throughout this module and
/// `view`, and a boxed payload costs that at every one of those sites for a
/// pairing two functions can hold on their own. `Model::open`/[`Screen::Viewer`]
/// already relate the same way.
fn set_screen(model: &mut Model, screen: Screen) {
    model.screen = screen;
    if screen != Screen::Manual {
        model.manual = None;
    }
}

/// Apply one message to the model and report the work it implies.
///
/// Pure: no I/O, no clock, no terminal. This is the whole of the TUI's
/// behaviour, and the whole of what its tests drive.
pub fn update(model: &mut Model, msg: Msg) -> Vec<Cmd> {
    let mode_before = model.mode();
    let mut cmds = dispatch(model, msg);
    // Two panes follow whatever is under the cursor rather than being asked
    // to refresh: the AI panel shows the current message's analysis and the
    // why-panel the current hit's breakdown. Doing it here rather than in
    // every action that can move a cursor is what keeps the list of "things
    // that move a cursor" from having to be maintained twice — a message
    // arriving and re-clamping the list moves it too, and no key was pressed
    // for that.
    cmds.extend(follow_cursor(model));
    // A chord half-typed in one mode means nothing in another, and a
    // non-key message can change the mode underneath it — a slow `Get`
    // landing opens the viewer, a `Removed` closes it. Dropping the
    // fragment is what stops the *next* key from being read against a
    // prefix the user typed for a screen that is no longer there.
    if model.mode() != mode_before {
        model.pending.clear();
    }
    // Folded in here rather than at each recording site: `record_command`
    // sets a flag and this is the one place that turns it into work, so
    // there is exactly one write per `update` however many lines it recorded.
    if std::mem::take(&mut model.pending_history) {
        cmds.push(Cmd::SaveHistory {
            entries: model.history.entries().to_vec(),
        });
    }
    cmds
}

fn dispatch(model: &mut Model, msg: Msg) -> Vec<Cmd> {
    match msg {
        Msg::Boot => {
            // An error already on the status line at boot — an unrecognized
            // `--theme`/`$RMAIL_THEME`, say — is something the caller chose
            // to set before the first message was even sent; overwriting it
            // with "loading accounts…" a moment later would make the notice
            // invisible for the one frame it might otherwise have shown.
            if model.level != Level::Error {
                model.info("loading accounts…");
            }
            model.inflight += 1;
            vec![Cmd::LoadAccounts]
        }
        Msg::Key(key) => on_key(model, key),
        Msg::Accounts(result) => {
            model.inflight = model.inflight.saturating_sub(1);
            match result {
                Ok(accounts) => {
                    let chosen = match model.preferred_account {
                        // An explicit `--account` that does not exist is a
                        // typo worth reporting, not something to silently
                        // substitute the first account for.
                        Some(wanted) => accounts.into_iter().find(|a| a.id == wanted).ok_or(
                            format!("no account {wanted} — list them with `mail accounts`"),
                        ),
                        None => accounts
                            .into_iter()
                            .next()
                            .ok_or_else(|| "no accounts configured".to_owned()),
                    };
                    let account = match chosen {
                        Ok(account) => account,
                        Err(why) => {
                            model.fail(why);
                            return Vec::new();
                        }
                    };
                    let account_id = account.id;
                    model.account = Some(account);
                    model.info("loading folders…");
                    // Two counted requests: the folder listing and the outbox
                    // listing. `Watch` is not one — nobody asked for it and
                    // it never "finishes", so counting it would leave the
                    // status bar claiming work forever.
                    model.inflight += 2;
                    // The event stream starts here and runs for the whole
                    // session: it is how the list stays current without the
                    // TUI polling, and it is a read of local state, so it
                    // costs the daemon nothing to keep open.
                    //
                    // The outbox is listed once at startup for the undo
                    // toast's sake: an undo window is seconds long, and a
                    // countdown nobody sees until they think to open a pane
                    // is not an undo offer at all.
                    vec![
                        Cmd::LoadFolders { account_id },
                        Cmd::Watch { account_id },
                        Cmd::LoadOutbox { account_id },
                    ]
                }
                Err(error) => {
                    model.fail(format!("accounts: {error}"));
                    Vec::new()
                }
            }
        }
        Msg::Folders(result) => {
            model.inflight = model.inflight.saturating_sub(1);
            match result {
                Ok(folders) => {
                    model.folders = folders;
                    model.folder_idx = inbox_index(&model.folders);
                    model.clamp();
                    match model.current_folder().map(|f| f.id) {
                        Some(mailbox_id) => {
                            model.open_folder = Some(mailbox_id);
                            model.info("loading messages…");
                            model.inflight += 1;
                            vec![Cmd::LoadMessages { mailbox_id }]
                        }
                        None => {
                            model.info("no folders yet — run `mail sync`");
                            Vec::new()
                        }
                    }
                }
                Err(error) => {
                    model.fail(format!("folders: {error}"));
                    Vec::new()
                }
            }
        }
        Msg::Messages { mailbox_id, result } => {
            model.inflight = model.inflight.saturating_sub(1);
            match result {
                Ok(messages) => {
                    // A listing that landed after the user moved on belongs to
                    // a folder nobody is looking at any more. `open_folder` is
                    // set when the request goes out, so a response that does
                    // not match it is stale — dropping it is what stops a slow
                    // reply from yanking the pane back to a folder the user
                    // already left.
                    if model.open_folder != Some(mailbox_id) {
                        return Vec::new();
                    }
                    model.messages = messages;
                    model.clamp();
                    model.info(format!("{} message(s)", model.messages.len()));
                    Vec::new()
                }
                Err(error) => {
                    model.fail(format!("messages: {error}"));
                    Vec::new()
                }
            }
        }
        Msg::Opened { message_id, result } => {
            model.inflight = model.inflight.saturating_sub(1);
            // The same staleness rule the listing has, and for the same
            // reason: press Enter, change your mind, switch folder — and a
            // slow `Get` must not then yank a viewer open on a message from
            // the folder you left. Two Enters in flight resolve to whichever
            // the user asked for last, not whichever the daemon answered
            // last.
            if model.opening != Some(message_id) {
                return Vec::new();
            }
            model.opening = None;
            match result {
                Ok(open) => {
                    model.open = Some(open);
                    model.scroll = 0;
                    set_screen(model, Screen::Viewer);
                    model.info("q back · o open HTML · r reply · ? help · K manual");
                    Vec::new()
                }
                Err(error) => {
                    model.fail(format!("open: {error}"));
                    Vec::new()
                }
            }
        }
        Msg::Done { label, result } => {
            model.inflight = model.inflight.saturating_sub(1);
            match result {
                Ok(effect) => {
                    apply_effect(model, &effect);
                    model.info(label);
                    Vec::new()
                }
                Err(error) => {
                    model.fail(format!("{label}: {error}"));
                    Vec::new()
                }
            }
        }
        Msg::LiveUpdatesStopped(why) => {
            model.fail(format!(
                "live updates stopped ({why}) — the list is no longer refreshing itself"
            ));
            Vec::new()
        }
        Msg::Keymap { result, announce } => {
            match result {
                Ok(keymap) => {
                    model.keymap = keymap;
                    // Whatever was half-typed was typed against the old
                    // bindings; carrying it over would resolve a chord the
                    // user never started.
                    model.pending.clear();
                    if announce {
                        model.info("key bindings reloaded");
                    }
                }
                // The bindings already loaded keep working: a typo saved
                // mid-edit must not leave someone holding a TUI whose keys
                // have all changed at once.
                Err(error) => model.fail(format!("key bindings: {error}")),
            }
            Vec::new()
        }
        Msg::Search { generation, event } => {
            let mut note = None;
            if let Some(Overlay::Search(pane)) = model.overlay.as_mut() {
                match event {
                    SearchEvent::Hit(hit) => pane.push_hit(generation, *hit),
                    SearchEvent::Done(result) if generation == pane.generation => {
                        pane.complete = true;
                        note = Some(match result {
                            Ok(()) => Ok(format!(
                                "{} result(s) — Enter walks them, Esc closes",
                                pane.hits.len()
                            )),
                            Err(error) => {
                                pane.error = Some(error.clone());
                                Err(format!("search: {error}"))
                            }
                        });
                    }
                    SearchEvent::Done(_) => {}
                }
            }
            apply_note(model, note);
            Vec::new()
        }
        Msg::Finder { generation, event } => {
            let mut note = None;
            if let Some(Overlay::Finder(pane)) = model.overlay.as_mut() {
                match event {
                    FinderEvent::Batch {
                        items,
                        complete,
                        superseded,
                        scanned,
                    } => {
                        if generation == pane.generation {
                            pane.scanned = scanned;
                            // A superseded scan ended *cleanly*; it is a fact
                            // about the stream, never an error to report.
                            pane.superseded = superseded;
                        }
                        pane.apply_batch(generation, items, complete);
                    }
                    FinderEvent::Failed(error) if generation == pane.generation => {
                        pane.error = Some(error.clone());
                        note = Some(Err(format!("finder: {error}")));
                    }
                    FinderEvent::Failed(_) => {}
                }
            }
            apply_note(model, note);
            Vec::new()
        }
        Msg::Ask { generation, event } => {
            let mut note = None;
            if let Some(Overlay::Ask(pane)) = model.overlay.as_mut() {
                if generation == pane.generation {
                    match event {
                        AskEvent::Trace(trace) => pane.trace = Some(trace),
                        AskEvent::Token(token) => pane.push_token(generation, &token),
                        AskEvent::Cite(citation) => {
                            if pane.citations.len() < overlays::MAX_ROWS {
                                pane.citations.push(*citation);
                            }
                        }
                        AskEvent::Done { grounded, refusal } => {
                            pane.phase = AskPhase::Done;
                            pane.grounded = grounded;
                            pane.refusal = refusal;
                            // `grounded` is the daemon's verdict on whether
                            // the answer cited anything it actually
                            // retrieved, not the model's claim about itself,
                            // and it is said that way.
                            note = Some(if grounded {
                                Ok(format!(
                                    "{} source(s) — j/k and Enter open one",
                                    pane.citations.len()
                                ))
                            } else {
                                Err("the daemon could not ground this answer in your mail"
                                    .to_owned())
                            });
                        }
                        AskEvent::Failed(error) => {
                            pane.phase = AskPhase::Done;
                            pane.error = Some(error.clone());
                            note = Some(Err(format!("ask: {error}")));
                        }
                    }
                }
            }
            apply_note(model, note);
            Vec::new()
        }
        Msg::Report { generation, event } => {
            let mut note = None;
            if let Some(Overlay::Report(pane)) = model.overlay.as_mut() {
                match event {
                    ReportEvent::Frame {
                        fill,
                        rows,
                        complete,
                    } => {
                        let last = complete && generation == pane.generation;
                        pane.apply(generation, fill, rows, complete);
                        if last {
                            note = Some(Ok(report_summary(pane)));
                        }
                    }
                    ReportEvent::Failed(error) if generation == pane.generation => {
                        pane.fail(generation, error.clone());
                        note = Some(Err(format!("{}: {error}", pane.invocation.verb.join(" "))));
                    }
                    ReportEvent::Failed(_) => {}
                }
            }
            apply_note(model, note);
            Vec::new()
        }
        Msg::Explained { message_id, result } => {
            let mut note = None;
            if let Some(Overlay::Search(pane)) = model.overlay.as_mut() {
                if pane.explaining == Some(message_id) {
                    pane.explaining = None;
                    match result {
                        Ok(explanation) => {
                            pane.explain_failed = None;
                            pane.explanation = Some(explanation);
                        }
                        Err(error) => {
                            // Remembered, not merely reported. `follow_cursor`
                            // runs after *every* message and would otherwise
                            // see "no explanation, none in flight" and ask
                            // again — for a hit whose explanation just failed,
                            // at round-trip rate, forever. `Explain` re-runs
                            // the whole retrieval pipeline server-side, so
                            // that loop is expensive at both ends.
                            pane.explain_failed = Some(message_id);
                            pane.explanation = None;
                            note = Some(Err(format!("explain: {error}")));
                        }
                    }
                }
            }
            apply_note(model, note);
            Vec::new()
        }
        Msg::Summarized { message_id, result } => {
            model.inflight = model.inflight.saturating_sub(1);
            // The same staleness rule every other response has: the panel may
            // have moved on to another message while this was in flight.
            if model.summary_for != Some(message_id) {
                return Vec::new();
            }
            model.summary_for = None;
            match result {
                Ok(summary) => {
                    model.summary_failed = None;
                    model.summary = Some(summary);
                }
                Err(error) => {
                    // Same latch, same reason, as the why-panel's: a daemon
                    // that is down fails this without a round trip, and a
                    // panel that re-asks on every message would spin.
                    model.summary_failed = Some(message_id);
                    model.summary = None;
                    model.fail(format!("ai: {error}"));
                }
            }
            Vec::new()
        }
        Msg::Outbox { now, result } => {
            model.inflight = model.inflight.saturating_sub(1);
            let rows = match result {
                Ok(rows) => rows,
                Err(error) => {
                    if let Some(Overlay::Outbox(pane)) = model.overlay.as_mut() {
                        pane.loading = false;
                        // The rows already listed stay. A cancel reports
                        // through this message too, and a refused cancel
                        // replacing the whole outbox with its error text
                        // would lose the listing the user is looking at.
                        pane.error = Some(error.clone());
                    }
                    model.fail(format!("outbox: {error}"));
                    return Vec::new();
                }
            };
            if let Some(Overlay::Outbox(pane)) = model.overlay.as_mut() {
                pane.loading = false;
                pane.error = None;
                pane.rows.clone_from(&rows);
                pane.cursor = pane.cursor.min(pane.rows.len().saturating_sub(1));
            }
            arm_toast(model, now, &rows)
        }
        Msg::Tick(now) => {
            let Some(toast) = undo_toast_mut(model) else {
                return Vec::new();
            };
            toast.remaining = toast.deadline.saturating_sub(now).max(0);
            if toast.remaining == 0 {
                // The window closed. The message is the scheduler's now, and
                // an "undo" offer that no longer works is worse than none.
                remove_undo_toast(model);
            }
            Vec::new()
        }
        Msg::Changed => match model.open_folder {
            // Re-read rather than patch: the event says a folder changed, and
            // the authoritative answer to "what is in it now" is the local
            // database, one cheap RPC away.
            Some(mailbox_id) => {
                model.inflight += 1;
                vec![Cmd::LoadMessages { mailbox_id }]
            }
            None => Vec::new(),
        },
    }
}

/// A status line an overlay arm chose while it still held a borrow of the
/// overlay. `Ok` is [`Model::info`], `Err` is [`Model::fail`].
///
/// The indirection exists because those two take the whole model and the arms
/// above are inside `model.overlay.as_mut()`; deciding the text there and
/// applying it here is the difference between one borrow and a fight with the
/// borrow checker in every arm.
type Note = Option<Result<String, String>>;

fn apply_note(model: &mut Model, note: Note) {
    match note {
        Some(Ok(text)) => model.info(text),
        Some(Err(text)) => model.fail(text),
        None => {}
    }
}

/// Raise the undo toast for whichever listed send is still inside its window,
/// and ask for the clock ticks that will count it down.
///
/// The *earliest* deadline wins when several are open: it is the one about to
/// stop being undoable, and it is the one a countdown is for.
fn arm_toast(model: &mut Model, now: i64, rows: &[OutboxRow]) -> Vec<Cmd> {
    let soonest = rows
        .iter()
        // `state` as well as the deadline: a canceled or already-sent entry
        // can still carry an `undo_deadline` in the future, and offering to
        // undo one of those would be an offer that cannot be honoured.
        .filter(|row| row.state == "scheduled")
        .filter_map(|row| row.undo_deadline.map(|deadline| (deadline, row)))
        // An open window, and one short enough to be an *undo* offer. A
        // generous `send.undo_window` would otherwise pin a toast row to the
        // screen and repaint the whole TUI once a second for as long as it
        // lasted; past this it is a scheduled send, which the outbox pane is
        // for.
        .filter(|(deadline, _)| *deadline > now && *deadline - now <= MAX_UNDO_TOAST)
        .min_by_key(|(deadline, _)| *deadline);
    let Some((deadline, row)) = soonest else {
        remove_undo_toast(model);
        return Vec::new();
    };
    set_undo_toast(
        model,
        UndoToast {
            outbox_id: row.id,
            to: row.to.clone(),
            deadline,
            remaining: deadline.saturating_sub(now).max(0),
        },
    );
    vec![Cmd::Countdown { until: deadline }]
}

/// The queued undo toast, if any. At most one exists at a time —
/// [`set_undo_toast`] replaces rather than appends — so a linear find is
/// always enough, over a queue capped at [`MAX_TOASTS`].
fn undo_toast(model: &Model) -> Option<&UndoToast> {
    model.toasts.iter().find_map(|toast| match toast {
        Toast::Undo(toast) => Some(toast),
        Toast::Completion { .. } | Toast::Priority { .. } => None,
    })
}

fn undo_toast_mut(model: &mut Model) -> Option<&mut UndoToast> {
    model.toasts.iter_mut().find_map(|toast| match toast {
        Toast::Undo(toast) => Some(toast),
        Toast::Completion { .. } | Toast::Priority { .. } => None,
    })
}

/// Replace whatever undo toast is queued with `toast`. There is only ever
/// one: [`arm_toast`] re-derives it from the outbox's own state on every
/// listing rather than accumulating one per send.
fn set_undo_toast(model: &mut Model, toast: UndoToast) {
    remove_undo_toast(model);
    push_toast(model, Toast::Undo(toast));
}

/// Drop the queued undo toast, if any. A no-op otherwise — callers do not
/// need to check first.
fn remove_undo_toast(model: &mut Model) {
    model
        .toasts
        .retain(|toast| !matches!(toast, Toast::Undo(_)));
}

/// Append a toast to the queue, dropping the oldest *non-undo* entry first
/// if that would exceed [`MAX_TOASTS`].
///
/// Not simply the oldest: an active undo countdown must survive a flood of
/// other toasts, or `u` stops cancelling a send that is still inside its
/// window while [`Cmd::Countdown`] keeps ticking against nothing. At most
/// one [`Toast::Undo`] ever exists ([`set_undo_toast`] replaces rather than
/// accumulates), so among [`MAX_TOASTS`] queued there is always a non-undo
/// victim; `unwrap_or(0)` is a total fallback for that impossible case, not
/// a claim it can happen.
fn push_toast(model: &mut Model, toast: Toast) {
    if model.toasts.len() >= MAX_TOASTS {
        let victim = model
            .toasts
            .iter()
            .position(|toast| !matches!(toast, Toast::Undo(_)))
            .unwrap_or(0);
        model.toasts.remove(victim);
    }
    model.toasts.push_back(toast);
}

/// Keep the two cursor-following panes pointed at what the cursor is on.
///
/// Both are cheap local reads and both are guarded by "is one already in
/// flight for this id", so holding `j` down issues one request per row at
/// most — not one per keystroke, and never a second for a row that is
/// already loading.
fn follow_cursor(model: &mut Model) -> Vec<Cmd> {
    let mut cmds = explain_current(model);
    cmds.extend(summarize_current(model));
    cmds
}

fn explain_current(model: &mut Model) -> Vec<Cmd> {
    let Some(Overlay::Search(pane)) = model.overlay.as_mut() else {
        return Vec::new();
    };
    if !pane.explain {
        return Vec::new();
    }
    let Some(message_id) = pane.hit().map(|hit| hit.message_id) else {
        pane.explanation = None;
        return Vec::new();
    };
    let already = pane
        .explanation
        .as_ref()
        .is_some_and(|explanation| explanation.message_id == message_id);
    if already || pane.explaining == Some(message_id) || pane.explain_failed == Some(message_id) {
        return Vec::new();
    }
    pane.explaining = Some(message_id);
    pane.explanation = None;
    let query = pane.query.clone();
    // Uncounted, like the search stream itself: holding `j` supersedes this
    // once per row and the executor aborts the previous one, so an aborted
    // request would never deliver the message that decremented the counter.
    let account_id = model.current_account().map_or(0, |account| account.id);
    vec![Cmd::Explain {
        query,
        message_id,
        account_id,
    }]
}

fn summarize_current(model: &mut Model) -> Vec<Cmd> {
    if !model.ai_panel {
        return Vec::new();
    }
    // Only what the overlays leave alone: a panel that chased the highlighted
    // *search hit* as well would be a second cursor to reason about, and the
    // why-panel already covers "tell me about this hit".
    if model.overlay.is_some() {
        return Vec::new();
    }
    let Some(message_id) = target_message(model) else {
        model.summary = None;
        return Vec::new();
    };
    let already = model
        .summary
        .as_ref()
        .is_some_and(|summary| summary.message_id == message_id);
    // Never two at once, whichever message the outstanding one is for; never
    // again for a message whose load just failed; and never over a pinned
    // one. The pin is what makes `.` mean something: it aims the panel at a
    // message, and a `Msg::Changed` reload that re-clamps the cursor a second
    // later must not throw away the answer — for a reply suggestion, a paid
    // answer — that the user explicitly asked for. Deliberate movement
    // clears it (see `move_cursor`); a list reloading underneath does not.
    if already
        || model.summary_for.is_some()
        || model.summary_failed == Some(message_id)
        || model.summary_pinned.is_some()
    {
        return Vec::new();
    }
    model.summary_for = Some(message_id);
    model.summary = None;
    model.inflight += 1;
    vec![Cmd::LoadSummary {
        message_id,
        suggest_reply: false,
    }]
}

/// Fold a completed action's effect into the local view.
fn apply_effect(model: &mut Model, effect: &Effect) {
    match effect {
        Effect::Removed(id) => {
            model.messages.retain(|m| m.id != *id);
            if model.open.as_ref().is_some_and(|o| o.id == *id) {
                model.open = None;
                // Only when the viewer is actually what is on screen. The
                // manual may have been opened over it, and a message being
                // archived elsewhere is not a reason to close the page
                // somebody is reading; `leave_manual` notices the viewer is
                // empty and returns to the list instead.
                if model.screen == Screen::Viewer {
                    set_screen(model, Screen::List);
                }
            }
            if model.opening == Some(*id) {
                model.opening = None;
            }
            model.clamp();
        }
        Effect::Flags { message_id, flags } => {
            if let Some(row) = model.messages.iter_mut().find(|m| m.id == *message_id) {
                row.flags.clone_from(flags);
            }
        }
        Effect::Drafted(_) | Effect::None => {}
    }
}

/// The index of the folder the TUI should open first.
///
/// INBOX by name, case-insensitively, because IMAP mandates that folder's
/// name but not its case; otherwise the first folder, so an account whose
/// INBOX has not synced yet still opens on something.
fn inbox_index(folders: &[Folder]) -> usize {
    folders
        .iter()
        .position(|f| f.name.eq_ignore_ascii_case("INBOX"))
        .unwrap_or(0)
}

/// Route a key press through the keymap engine.
///
/// The whole of "which key does what" is this one call: the mode comes from
/// the model's own state, the bindings from [`Model::keymap`], and what comes
/// back is an action to run, a request to wait for the rest of a chord, or a
/// key nothing claims.
fn on_key(model: &mut Model, key: Key) -> Vec<Cmd> {
    let mode = model.mode();
    match model.keymap.resolve(mode, &mut model.pending, key) {
        Resolution::Pending => Vec::new(),
        Resolution::Run { action, count } => run_action(model, action, count),
        Resolution::Unbound(key) => on_unbound(model, mode, key),
    }
}

/// A key no binding claims.
///
/// In a text prompt that is not a mistake, it is the text — which is why
/// insert mode binds almost nothing and lets the rest fall through here.
/// Everywhere else it is silence: an error line per stray keystroke would
/// bury the messages that matter.
fn on_unbound(model: &mut Model, mode: Mode, key: Key) -> Vec<Cmd> {
    if !matches!(mode, Mode::Insert | Mode::Prompt) {
        return Vec::new();
    }
    let Key::Char(c) = key else {
        return Vec::new();
    };
    edit_prompt(model, TextEdit::Push(c))
}

/// One change to whatever text field is up.
#[derive(Debug, Clone, Copy)]
enum TextEdit {
    /// Type a character.
    Push(char),
    /// Delete the one before the cursor.
    Backspace,
}

/// Which typing surface an edit landed on.
///
/// The follow-up (a fresh search, a fresh find, recomputed palette matches)
/// needs the whole model, and the edit needed a borrow of one field inside
/// it — so the edit reports what it touched and the follow-up runs after.
#[derive(Debug, Clone, Copy)]
enum Typed {
    /// Nothing that needs anything to happen next.
    Nothing,
    /// The search query.
    Search,
    /// The finder prompt.
    Finder,
    /// The `:` command line.
    Command,
}

/// Apply `edit` to whichever text field is up, and issue whatever the change
/// implies.
fn edit_prompt(model: &mut Model, edit: TextEdit) -> Vec<Cmd> {
    // The manual's search line is not an overlay — the manual is a screen —
    // so it is checked first rather than added to the match below. Nothing
    // follows an edit to it: an in-page pattern previews from
    // `ManualState::pattern` as it is typed, which is a render-time read of
    // state that is already here, not work to issue.
    if let Some(prompt) = model
        .manual
        .as_mut()
        .and_then(|manual| manual.prompt.as_mut())
    {
        apply_edit(&mut prompt.pattern, edit);
        return Vec::new();
    }
    let typed = match model.overlay.as_mut() {
        Some(Overlay::Input { buffer, .. }) => {
            apply_edit(buffer, edit);
            Typed::Nothing
        }
        Some(Overlay::Search(pane)) if pane.typing() => {
            once(apply_edit(&mut pane.query, edit), Typed::Search)
        }
        Some(Overlay::Finder(pane)) => once(apply_edit(&mut pane.query, edit), Typed::Finder),
        Some(Overlay::Command(pane)) => {
            // An edit ends a history walk and clears the last complaint: the
            // line is the typist's again, and an error about text they have
            // started fixing is an error about text that is no longer there.
            let changed = apply_edit(&mut pane.input, edit);
            if changed {
                pane.browse = None;
                pane.error = None;
            }
            once(changed, Typed::Command)
        }
        Some(Overlay::Ask(pane)) if pane.typing() => {
            apply_edit(&mut pane.question, edit);
            Typed::Nothing
        }
        _ => Typed::Nothing,
    };
    match typed {
        // An edit that changed nothing — backspace on an empty prompt, a key
        // held past the length cap — must not re-issue the query. Otherwise a
        // held-down key is one RPC per repeat for a string that never moves.
        Typed::Nothing => Vec::new(),
        Typed::Search => search_now(model),
        Typed::Finder => find_now(model),
        Typed::Command => {
            refresh_command(model);
            Vec::new()
        }
    }
}

fn once(changed: bool, typed: Typed) -> Typed {
    if changed {
        typed
    } else {
        Typed::Nothing
    }
}

/// Apply one edit, reporting whether the buffer actually moved.
fn apply_edit(buffer: &mut String, edit: TextEdit) -> bool {
    match edit {
        // Bounded: a prompt collects an address or a query, and a key held
        // down against it must not grow a `String` for as long as it is
        // leaned on.
        TextEdit::Push(_) if buffer.chars().count() >= MAX_INPUT => false,
        TextEdit::Push(c) => {
            buffer.push(c);
            true
        }
        TextEdit::Backspace => buffer.pop().is_some(),
    }
}

/// Do one named thing.
///
/// This match is the model's entire key-driven surface, and the seam a later
/// task extends: task 85's search, palette and ask panes each add their
/// actions here and their bindings to `keymap`'s defaults, and no arm below
/// has to learn they exist.
fn run_action(model: &mut Model, action: Action, count: Option<u32>) -> Vec<Cmd> {
    match action {
        Action::CursorDown => move_cursor(model, Direction::Down, count),
        Action::CursorUp => move_cursor(model, Direction::Up, count),
        Action::CursorTop => jump(model, Edge::Top, count),
        Action::CursorBottom => jump(model, Edge::Bottom, count),
        Action::FocusToggle => set_focus(
            model,
            match model.focus {
                Focus::Folders => Focus::Messages,
                Focus::Messages => Focus::Folders,
            },
        ),
        Action::FocusFolders => set_focus(model, Focus::Folders),
        Action::FocusMessages => set_focus(model, Focus::Messages),
        Action::Open => open(model),
        Action::Back => leave(model, Leave::ThenQuit),
        Action::Cancel => leave(model, Leave::ThenNothing),
        Action::Quit => {
            model.quit = true;
            Vec::new()
        }
        Action::Help => {
            model.overlay = Some(Overlay::Help);
            Vec::new()
        }
        Action::ManualOpen => open_manual(model),
        Action::ManualBack => manual_jump(model, Jump::Back),
        Action::ManualForward => manual_jump(model, Jump::Forward),
        Action::ManualNext => step_manual_match(model, Direction::Down),
        Action::ManualPrev => step_manual_match(model, Direction::Up),
        Action::ManualGrep => open_manual_grep(model),
        Action::VisualToggle => toggle_visual(model),
        Action::VisualSwapEnds => swap_ends(model),
        Action::Archive => archive(model),
        Action::Delete => confirm_delete(model),
        Action::ToggleRead => toggle_flag(model, SEEN, "read"),
        Action::ToggleFlag => toggle_flag(model, FLAGGED, "flagged"),
        Action::CopyTo => pick(model, PickFor::Copy),
        Action::MoveTo => pick(model, PickFor::Move),
        Action::Reply => reply(model),
        Action::Forward => forward(model),
        Action::OpenHtml => open_html(model),
        Action::SearchOpen => open_search(model),
        Action::SearchExplain => toggle_explain(model),
        Action::FinderOpen => open_finder(model),
        Action::CommandOpen | Action::PaletteOpen => open_command(model),
        Action::AskOpen => open_ask(model, String::new()),
        Action::AiPanel => toggle_ai_panel(model),
        Action::AiQuick => open_quick(model),
        Action::OutboxOpen => open_outbox(model),
        Action::OutboxCancel => undo_send(model),
        Action::ReportRerun => rerun_report(model),
        Action::PromptAccept => prompt_accept(model),
        Action::PromptComplete => prompt_complete(model),
        Action::MenuAccept => menu_accept(model),
        Action::PickAccept => accept_pick(model),
        Action::ConfirmAccept => accept_confirm(model),
        Action::InputSubmit => submit(model),
        Action::InputBackspace => backspace(model),
    }
}

// ---------------------------------------------------------------------------
// cursors
// ---------------------------------------------------------------------------

/// Which way a movement goes.
#[derive(Debug, Clone, Copy)]
enum Direction {
    /// Towards the end of the list.
    Down,
    /// Towards the start.
    Up,
}

/// Which end of a list a jump lands on.
#[derive(Debug, Clone, Copy)]
enum Edge {
    /// The first row.
    Top,
    /// The last row.
    Bottom,
}

/// Which cursor `cursor.down` and friends move.
///
/// One binding, four cursors: the point of naming the *action* rather than
/// the field is that `j` keeps meaning "down" in the folder pane, the message
/// pane, the folder picker and the message body without four bindings that
/// could drift apart.
#[derive(Debug, Clone, Copy)]
enum Cursor {
    /// The folder picker's highlight.
    Pick,
    /// Whichever of task 85's overlays is up — its hits, items, matches,
    /// citations, entries or menu rows. One `Cursor` for six lists for the
    /// same reason there is one for four: the *action* is "down", and which
    /// list that moves is state, not a binding.
    Overlay,
    /// The viewer's scroll offset, in body lines.
    Scroll,
    /// The folder list.
    Folders,
    /// The message list.
    Messages,
    /// The manual's selected line. A row cursor rather than a scroll offset
    /// (which is what the viewer has) because a manual row is followable:
    /// `<enter>` takes the link on it.
    Manual,
}

fn active_cursor(model: &Model) -> Option<Cursor> {
    match &model.overlay {
        Some(Overlay::Pick { .. }) => Some(Cursor::Pick),
        Some(overlay) if overlay.list_cursor().is_some() => Some(Cursor::Overlay),
        // A confirm, a prompt or the help screen has nothing to scroll, and
        // must not scroll what is behind it.
        Some(_) => None,
        None => match model.screen {
            Screen::Viewer => Some(Cursor::Scroll),
            Screen::Manual => Some(Cursor::Manual),
            Screen::List => Some(match model.focus {
                Focus::Folders => Cursor::Folders,
                Focus::Messages => Cursor::Messages,
            }),
        },
    }
}

/// Where `cursor` is and how far it can go, or `None` when it has no rows to
/// sit on at all.
fn cursor_span(model: &Model, cursor: Cursor) -> Option<(usize, usize)> {
    let (idx, len) = match cursor {
        Cursor::Pick => match &model.overlay {
            Some(Overlay::Pick { idx, .. }) => (*idx, model.folders.len()),
            _ => return None,
        },
        Cursor::Overlay => model.overlay.as_ref()?.list_cursor()?,
        Cursor::Scroll => (
            model.scroll,
            model.open.as_ref().map_or(0, |open| open.body.len()),
        ),
        Cursor::Folders => (model.folder_idx, model.folders.len()),
        Cursor::Messages => (model.message_idx, model.messages.len()),
        Cursor::Manual => {
            let manual = model.manual.as_ref()?;
            let lines = manual_doc(model)?.lines.len();
            (manual.cursor_in(lines), lines)
        }
    };
    (len > 0).then(|| (idx, len - 1))
}

/// The manual page as it stands, or `None` when the manual is not up.
///
/// Rendered on demand rather than cached: it is a pure function of the
/// location and the bindings in force, so a cache would be a copy that lies
/// about the keymap the moment `keys.toml` is saved — and the whole document
/// is a few kilobytes of `&'static str` plus a `Vec` walk, which is cheaper
/// than the invalidation would be.
fn manual_doc(model: &Model) -> Option<manual::Doc> {
    let manual = model.manual.as_ref()?;
    Some(manual::doc(&manual.at, &model.keymap))
}

fn set_cursor(model: &mut Model, cursor: Cursor, at: usize) {
    match cursor {
        Cursor::Pick => {
            if let Some(Overlay::Pick { idx, .. }) = model.overlay.as_mut() {
                *idx = at;
            }
        }
        Cursor::Overlay => {
            if let Some(overlay) = model.overlay.as_mut() {
                overlay.set_list_cursor(at);
            }
        }
        Cursor::Scroll => model.scroll = at,
        Cursor::Folders => model.folder_idx = at,
        Cursor::Messages => model.message_idx = at,
        Cursor::Manual => {
            if let Some(manual) = model.manual.as_mut() {
                manual.cursor = at;
            }
        }
    }
}

fn move_cursor(model: &mut Model, direction: Direction, count: Option<u32>) -> Vec<Cmd> {
    // The command line's `<up>`/`<down>` are its history, not a cursor —
    // vim's meaning of those keys on a `:` line, and the reason the pane
    // reports no list cursor at all. Handled here rather than by a binding of
    // its own so `cursor.up` keeps meaning "the previous thing" wherever it
    // is pressed, which is what a shared vocabulary is for.
    if browse_history(model, direction) {
        return Vec::new();
    }
    let Some(cursor) = active_cursor(model) else {
        return Vec::new();
    };
    let Some((idx, last)) = cursor_span(model, cursor) else {
        return Vec::new();
    };
    unpin_summary(model, cursor);
    // Saturating and clamped, so a count costs the same arithmetic whether it
    // is 3 or `keymap::MAX_COUNT`. A count never multiplies *commands* — no
    // arm of `run_action` issues more than the one the action names.
    let by = rows(count);
    let at = match direction {
        Direction::Down => idx.saturating_add(by).min(last),
        Direction::Up => idx.saturating_sub(by),
    };
    set_cursor(model, cursor, at);
    Vec::new()
}

fn jump(model: &mut Model, edge: Edge, count: Option<u32>) -> Vec<Cmd> {
    let Some(cursor) = active_cursor(model) else {
        return Vec::new();
    };
    let Some((_, last)) = cursor_span(model, cursor) else {
        return Vec::new();
    };
    unpin_summary(model, cursor);
    // vim's rule: with a count, both `gg` and `G` mean "row N" (1-based);
    // without one they mean the ends.
    let at = match (count, edge) {
        (Some(n), _) => rows(Some(n)).saturating_sub(1).min(last),
        (None, Edge::Top) => 0,
        (None, Edge::Bottom) => last,
    };
    set_cursor(model, cursor, at);
    Vec::new()
}

/// A count as a number of rows. Saturating: `MAX_COUNT` is well inside
/// `usize` on every platform this builds for, and a hypothetical one that is
/// not should clamp rather than wrap into a jump backwards.
fn rows(count: Option<u32>) -> usize {
    usize::try_from(count.unwrap_or(1)).unwrap_or(usize::MAX)
}

/// A deliberate move off the message list releases the AI panel's pin.
///
/// Only a *deliberate* one: the pin exists so that a list reloading
/// underneath the user cannot discard an answer they paid for, and a reload
/// re-clamps `message_idx` without going through here.
fn unpin_summary(model: &mut Model, cursor: Cursor) {
    if matches!(cursor, Cursor::Messages) {
        model.summary_pinned = None;
    }
}

fn set_focus(model: &mut Model, focus: Focus) -> Vec<Cmd> {
    model.focus = focus;
    Vec::new()
}

// ---------------------------------------------------------------------------
// leaving things
// ---------------------------------------------------------------------------

/// What to do once there is nothing left to back out of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Leave {
    /// `q`: quit the TUI.
    ThenQuit,
    /// `Esc`: stay where you are.
    ThenNothing,
}

/// Close the innermost thing that is open: an overlay, then a selection, then
/// the viewer.
///
/// One function for both `q` and `Esc` because "get me out of here" has one
/// meaning; the difference between them is only what happens when there is
/// nothing left to leave. This is also the guarantee that no mode is a trap:
/// `Esc` is bound in the global layer, cannot be rebound
/// (`keymap::Chord::is_reserved`), and always makes progress towards the
/// message list.
fn leave(model: &mut Model, then: Leave) -> Vec<Cmd> {
    if let Some(overlay) = model.overlay.take() {
        // The help screen was not collecting anything, so "cancelled" would
        // be a lie; the others were.
        if !matches!(overlay, Overlay::Help) {
            model.info("cancelled");
        }
        let stop = cancels(&overlay);
        // A question asked over a report puts the report back rather than
        // dropping two layers for one `n`: declining the question is not
        // asking to leave the screen it was asked on, and the report's own
        // stream is still the one running.
        if let Overlay::Confirm {
            then: Confirmed::Invoke { over, .. },
            ..
        } = overlay
        {
            model.overlay = Some(Overlay::Report(over));
        }
        return stop;
    }
    // The manual's own layers, innermost first: the search line, then the
    // highlight it left behind, then the manual itself. Ahead of the visual
    // selection rather than after it, because the manual can be opened
    // mid-selection — and backing out of the page somebody is reading by
    // silently discarding their selection instead would be the opposite of
    // "leave the innermost thing".
    if let Some(manual) = model.manual.as_mut() {
        if manual.prompt.take().is_some() {
            model.info("cancelled");
            return Vec::new();
        }
        if manual.highlight.take().is_some() {
            model.info("search cleared");
            return Vec::new();
        }
        leave_manual(model);
        return Vec::new();
    }
    // `is_selecting` rather than the raw anchor: in the viewer the anchor is
    // still set but nothing is drawn selected, and "selection cleared" about
    // an invisible range reads as a keypress that did nothing. Leaving the
    // viewer with the anchor intact puts the selection back where it was made.
    if model.is_selecting() {
        model.visual = None;
        model.info("selection cleared");
        return Vec::new();
    }
    if model.screen == Screen::Viewer {
        set_screen(model, Screen::List);
        model.open = None;
        model.opening = None;
        return Vec::new();
    }
    if then == Leave::ThenQuit {
        model.quit = true;
    }
    Vec::new()
}

/// The [`Cmd::CancelStream`]s that closing `overlay` implies.
fn cancels(overlay: &Overlay) -> Vec<Cmd> {
    streams_of(overlay)
        .iter()
        .map(|which| Cmd::CancelStream { which: *which })
        .collect()
}

/// The streams an overlay was feeding on, which closing it should stop.
///
/// The search pane owns two: its own hits, and the why-panel's `Explain`.
fn streams_of(overlay: &Overlay) -> &'static [Stream] {
    match overlay {
        Overlay::Search(_) => &[Stream::Search, Stream::Explain],
        Overlay::Finder(_) => &[Stream::Find],
        Overlay::Ask(_) => &[Stream::Ask],
        Overlay::Report(_) => &[Stream::Report],
        Overlay::Help
        | Overlay::Pick { .. }
        | Overlay::Confirm { .. }
        | Overlay::Input { .. }
        | Overlay::Command(_)
        | Overlay::Outbox(_)
        | Overlay::Quick(_) => &[],
    }
}

// ---------------------------------------------------------------------------
// visual selection
// ---------------------------------------------------------------------------

fn toggle_visual(model: &mut Model) -> Vec<Cmd> {
    if model.is_selecting() {
        model.visual = None;
        model.info("selection cleared");
        return Vec::new();
    }
    if model.screen != Screen::List || model.focus != Focus::Messages {
        model.fail("visual selects messages — Tab to the message list first");
        return Vec::new();
    }
    if model.messages.is_empty() {
        model.fail("no messages to select");
        return Vec::new();
    }
    model.visual = Some(model.message_idx);
    model.info("visual — j/k extend · a archive · d delete · s/f flags · c/M copy/move · Esc ends");
    Vec::new()
}

/// vim's `o`: put the cursor on the other end of the selection, so the end
/// that is wrong can be adjusted without starting again.
fn swap_ends(model: &mut Model) -> Vec<Cmd> {
    if let Some(anchor) = model.visual {
        model.visual = Some(model.message_idx);
        model.message_idx = anchor.min(model.messages.len().saturating_sub(1));
    }
    Vec::new()
}

// ---------------------------------------------------------------------------
// targets
// ---------------------------------------------------------------------------

/// The messages an action applies to: the visual selection when there is one,
/// otherwise the viewer's message, otherwise the row under the cursor.
fn targets(model: &Model) -> Vec<i64> {
    if let Some((from, to)) = model.selection() {
        return model
            .messages
            .get(from..=to)
            .map(|rows| rows.iter().map(|row| row.id).collect())
            .unwrap_or_default();
    }
    match target_message(model) {
        Some(id) => vec![id],
        None => Vec::new(),
    }
}

/// The messages a bulk-capable action applies to — or `None`, having said
/// why, when there are none or too many.
fn bulk_targets(model: &mut Model, what: &str) -> Option<Vec<i64>> {
    let ids = targets(model);
    if ids.is_empty() {
        model.fail("no message selected");
        return None;
    }
    if ids.len() > MAX_BULK {
        // Refused rather than truncated: acting on the first hundred of what
        // somebody selected is worse than acting on none of it.
        model.fail(format!(
            "{} messages selected — {what} acts on at most {MAX_BULK} at a time",
            ids.len()
        ));
        return None;
    }
    Some(ids)
}

/// The single message an action that has no bulk form applies to.
fn single_target(model: &mut Model) -> Option<i64> {
    if model.is_selecting() {
        model.fail("that acts on one message — Esc ends the selection");
        return None;
    }
    match target_message(model) {
        Some(id) => Some(id),
        None => {
            model.fail("no message selected");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// actions
// ---------------------------------------------------------------------------

fn open(model: &mut Model) -> Vec<Cmd> {
    // Only the list has something to open. The viewer already holds the
    // message; the manual's own `<enter>` is `menu.accept` (follow a link),
    // and this action reaching it through a rebind must not load mail behind
    // the page being read.
    if model.screen != Screen::List {
        return Vec::new();
    }
    if model.is_selecting() {
        model.fail("that acts on one message — Esc ends the selection");
        return Vec::new();
    }
    match model.focus {
        Focus::Folders => open_folder(model),
        Focus::Messages => open_message(model),
    }
}

fn accept_pick(model: &mut Model) -> Vec<Cmd> {
    // Put back anything that is not a picker rather than dropping it: these
    // actions are bindable in any mode, and `pick.accept` bound somewhere it
    // does not belong should do nothing, not silently close the overlay that
    // happens to be up.
    let (what, message_ids, idx) = match model.overlay.take() {
        Some(Overlay::Pick {
            what,
            message_ids,
            idx,
        }) => (what, message_ids, idx),
        other => {
            model.overlay = other;
            return Vec::new();
        }
    };
    let Some(dest) = model.folders.get(idx).cloned() else {
        model.fail("no such folder");
        return Vec::new();
    };
    model.inflight += message_ids.len();
    message_ids
        .into_iter()
        .map(|message_id| match what {
            PickFor::Copy => Cmd::Copy {
                message_id,
                dest_mailbox_id: dest.id,
            },
            PickFor::Move => Cmd::Move {
                message_id,
                dest_mailbox_id: dest.id,
                label: format!("moved to {}", dest.name),
            },
        })
        .collect()
}

fn accept_confirm(model: &mut Model) -> Vec<Cmd> {
    let then = match model.overlay.take() {
        Some(Overlay::Confirm { then, .. }) => then,
        other => {
            model.overlay = other;
            return Vec::new();
        }
    };
    match then {
        Confirmed::Delete(message_ids) => {
            model.inflight += message_ids.len();
            message_ids
                .into_iter()
                .map(|message_id| Cmd::Delete { message_id })
                .collect()
        }
        // Straight back through the one dispatcher, carrying the bang the gate
        // stamped on it — so the answered question is not asked again, and a
        // confirmed row does exactly what typing that line with a `!` does.
        Confirmed::Invoke { invocation, over } => run_row(model, *invocation, over),
    }
}

fn submit(model: &mut Model) -> Vec<Cmd> {
    let (buffer, what, message_id) = match model.overlay.take() {
        Some(Overlay::Input {
            buffer,
            what,
            message_id,
            ..
        }) => (buffer, what, message_id),
        other => {
            model.overlay = other;
            return Vec::new();
        }
    };
    submit_input(model, what, buffer.trim(), message_id)
}

fn backspace(model: &mut Model) -> Vec<Cmd> {
    edit_prompt(model, TextEdit::Backspace)
}

fn submit_input(model: &mut Model, what: InputFor, value: &str, message_id: i64) -> Vec<Cmd> {
    match what {
        InputFor::ForwardTo => {
            if value.is_empty() {
                model.info("cancelled");
                return Vec::new();
            }
            draft(model, DraftKind::Forward, message_id, value.to_owned())
        }
    }
}

fn open_folder(model: &mut Model) -> Vec<Cmd> {
    let Some(folder) = model.current_folder().cloned() else {
        return Vec::new();
    };
    model.focus = Focus::Messages;
    model.message_idx = 0;
    model.messages.clear();
    // A selection is a range of *these* rows; the folder it was made in is
    // the only place it means anything.
    model.visual = None;
    model.opening = None;
    model.open_folder = Some(folder.id);
    model.info(format!("loading {}…", folder.name));
    model.inflight += 1;
    vec![Cmd::LoadMessages {
        mailbox_id: folder.id,
    }]
}

fn open_message(model: &mut Model) -> Vec<Cmd> {
    let Some(message_id) = model.current_message().map(|m| m.id) else {
        return Vec::new();
    };
    model.opening = Some(message_id);
    model.info("opening…");
    model.inflight += 1;
    vec![Cmd::Open { message_id }]
}

fn archive(model: &mut Model) -> Vec<Cmd> {
    let Some(ids) = bulk_targets(model, "archive") else {
        return Vec::new();
    };
    let Some(dest) = archive_folder(&model.folders, model.open_folder) else {
        model.fail("no archive folder on this account");
        return Vec::new();
    };
    model.visual = None;
    model.inflight += ids.len();
    ids.into_iter()
        .map(|message_id| Cmd::Move {
            message_id,
            dest_mailbox_id: dest,
            label: "archived".to_owned(),
        })
        .collect()
}

/// The mailbox id to archive into, or `None` when the account has no folder
/// that looks like an archive. Never the folder already open — archiving a
/// message into the folder it is already in is a no-op the server may well
/// reject, and reporting "no archive folder" is clearer than a failed `Move`.
///
/// Matched on the folder's leaf name, not its full path: servers routinely
/// nest the special folders (Gmail's `[Gmail]/All Mail`, Dovecot's
/// `INBOX/Archive`), and a full-name match would silently find nothing on
/// exactly the accounts most likely to have an archive folder.
fn archive_folder(folders: &[Folder], open: Option<i64>) -> Option<i64> {
    ARCHIVE_NAMES.iter().find_map(|name| {
        folders
            .iter()
            .find(|f| leaf(&f.name).eq_ignore_ascii_case(name) && Some(f.id) != open)
            .map(|f| f.id)
    })
}

/// The last segment of a hierarchical folder name.
///
/// `/` only: it is the delimiter IMAP servers overwhelmingly report, and `.`
/// — the other common one — appears inside ordinary folder names often
/// enough (`v1.2 releases`) that splitting on it would rename folders in the
/// user's own head for no gain.
fn leaf(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

fn confirm_delete(model: &mut Model) -> Vec<Cmd> {
    let Some(ids) = bulk_targets(model, "delete") else {
        return Vec::new();
    };
    // `MailService.Delete` marks \Deleted and expunges on the server: the
    // message is gone from the account, not moved to a trash folder. That is
    // not something a stray keystroke should be able to do.
    let prompt = if ids.len() == 1 {
        "delete permanently (expunges on the server)? [y/N]".to_owned()
    } else {
        format!(
            "delete {} messages permanently (expunges on the server)? [y/N]",
            ids.len()
        )
    };
    model.visual = None;
    model.overlay = Some(Overlay::Confirm {
        prompt,
        then: Confirmed::Delete(ids),
    });
    Vec::new()
}

fn toggle_flag(model: &mut Model, flag: &str, noun: &str) -> Vec<Cmd> {
    let Some(ids) = bulk_targets(model, &format!("marking {noun}")) else {
        return Vec::new();
    };
    let rows: Vec<MessageRow> = ids
        .iter()
        .filter_map(|id| model.messages.iter().find(|row| row.id == *id))
        .cloned()
        .collect();
    if rows.is_empty() {
        model.fail("no message selected");
        return Vec::new();
    }
    // One intent for the whole selection: clear the flag only when every
    // message already has it, so marking a mixed selection read does not
    // leave the already-read half unread.
    let present = !rows.iter().all(|row| row.has_flag(flag));
    let label = if present {
        format!("marked {noun}")
    } else {
        format!("marked not {noun}")
    };
    model.visual = None;
    model.inflight += rows.len();
    rows.into_iter()
        .map(|row| Cmd::SetFlags {
            message_id: row.id,
            flags: row.flags_with(flag, present),
            label: label.clone(),
        })
        .collect()
}

fn pick(model: &mut Model, what: PickFor) -> Vec<Cmd> {
    if model.folders.is_empty() {
        model.fail("no folders to pick from");
        return Vec::new();
    }
    let Some(ids) = bulk_targets(
        model,
        match what {
            PickFor::Copy => "copy",
            PickFor::Move => "move",
        },
    ) else {
        return Vec::new();
    };
    model.visual = None;
    model.overlay = Some(Overlay::Pick {
        what,
        message_ids: ids,
        idx: 0,
    });
    Vec::new()
}

fn reply(model: &mut Model) -> Vec<Cmd> {
    let Some(id) = single_target(model) else {
        return Vec::new();
    };
    let Some(row) = model.messages.iter().find(|row| row.id == id).cloned() else {
        model.fail("no message selected");
        return Vec::new();
    };
    let Some(to) = row.from_addr.clone().filter(|a| !a.trim().is_empty()) else {
        model.fail("that message has no sender address to reply to");
        return Vec::new();
    };
    draft(model, DraftKind::Reply, row.id, to)
}

fn forward(model: &mut Model) -> Vec<Cmd> {
    let Some(message_id) = single_target(model) else {
        return Vec::new();
    };
    model.overlay = Some(Overlay::Input {
        prompt: "forward to".to_owned(),
        buffer: String::new(),
        what: InputFor::ForwardTo,
        message_id,
    });
    Vec::new()
}

fn draft(model: &mut Model, kind: DraftKind, message_id: i64, to: String) -> Vec<Cmd> {
    let Some(account) = model.current_account().cloned() else {
        model.fail("no account");
        return Vec::new();
    };
    // `CreateDraft` requires a `From` addr-spec and rejects anything that is
    // not one. The account's login is that address for every provider this
    // client targets; when it is unset there is nothing to guess.
    let Some(from) = account.username.clone().filter(|u| u.contains('@')) else {
        model.fail(format!(
            "account {} has no address to send from",
            account.name
        ));
        return Vec::new();
    };
    model.inflight += 1;
    vec![Cmd::Draft {
        kind,
        account_id: account.id,
        from,
        to,
        message_id,
    }]
}

fn open_html(model: &mut Model) -> Vec<Cmd> {
    let Some(open) = model.open.as_ref() else {
        model.fail("open the message first (Enter)");
        return Vec::new();
    };
    if !open.has_html {
        model.fail("this message has no HTML part");
        return Vec::new();
    }
    let message_id = open.id;
    model.inflight += 1;
    vec![Cmd::OpenHtml { message_id }]
}

// ---------------------------------------------------------------------------
// the manual (task 103)
// ---------------------------------------------------------------------------

/// `K` — the manual, at its front page.
fn open_manual(model: &mut Model) -> Vec<Cmd> {
    open_manual_at(model, manual::START)
}

/// Put the manual on screen at the page `name` addresses.
///
/// The seam the rest of the TUI reaches the manual through, and the one an
/// [`Action`] cannot express on its own — [`run_action`] takes a count, not a
/// string. Task 89's `:` dispatch needs it to carry a page argument, and task
/// 102's `K`-on-a-key-reference-row needs it to land on the page documenting
/// that action.
///
/// `name` is a page anchor, or — for that second caller — an [`Action::id`]
/// or a verb path, which [`manual::home_of`] resolves to the page declaring
/// itself that action's home. A row of the key reference carries an action id
/// and no anchor, and deriving one from the id would be a second mapping to
/// keep in step with the first.
///
/// An anchor wins over an action id where a string could be both. Nothing
/// spells one of each today, and the page set is the thing a reader typed a
/// name *at*.
///
/// # Errors, of a sort
///
/// A name that is neither is refused on the status line rather than opened:
/// [`manual::doc`] is total and would render a "no such page" page, which is
/// the right answer for a link inside the manual and the wrong one for a
/// caller that got a name wrong.
pub fn open_manual_at(model: &mut Model, name: &str) -> Vec<Cmd> {
    let Some(anchor) = manual::page(name)
        .or_else(|| manual::home_of(name))
        .map(|page| page.anchor)
    else {
        model.fail(format!("this build has no manual page called {name:?}"));
        return Vec::new();
    };
    // The manual is a *screen*, so an overlay left up would cover the thing
    // the caller just asked to show. Taking it also stops whatever it was
    // streaming, which is `leave`'s rule for closing one.
    let stop = model.overlay.take().map(|overlay| cancels(&overlay));
    let mut cmds = enter_manual(model, manual::Location::Page(anchor.to_owned()));
    cmds.extend(stop.unwrap_or_default());
    cmds
}

/// Put the manual on screen at `at`, or navigate an already-open one.
///
/// Navigating rather than re-entering is what keeps [`Origin`] honest: the
/// screen the manual returns to is the one it was *first* opened from, not
/// whichever page happened to be showing when a link was followed.
fn enter_manual(model: &mut Model, at: manual::Location) -> Vec<Cmd> {
    match model.manual.as_mut() {
        Some(manual) => manual.go(at),
        None => {
            model.manual = Some(ManualState::new(at, Origin::of(model.screen)));
            model.screen = Screen::Manual;
            // A `MailService.Get` still in flight would otherwise land later
            // and replace the manual with a viewer nobody asked for — the
            // same reason leaving the viewer clears it.
            model.opening = None;
        }
    }
    announce_manual(model);
    Vec::new()
}

/// Take the manual off screen, back where it came from.
fn leave_manual(model: &mut Model) {
    let from = model
        .manual
        .as_ref()
        .map_or(Origin::List, |manual| manual.from);
    // Back to the viewer only if it still holds something: the message can
    // have been archived, or the folder reloaded, while a page was being read.
    let screen = match from {
        Origin::Viewer if model.open.is_some() => Screen::Viewer,
        Origin::Viewer | Origin::List => Screen::List,
    };
    set_screen(model, screen);
    model.info("closed the manual");
}

fn announce_manual(model: &mut Model) {
    let Some(manual) = model.manual.as_ref() else {
        return;
    };
    let label = manual.at.label();
    model.info(format!(
        "{label} — Enter follows a link · <c-o> back · / searches · g/ searches every page · q leaves"
    ));
}

/// `<c-o>` / `<c-i>`.
fn manual_jump(model: &mut Model, jump: Jump) -> Vec<Cmd> {
    let Some(manual) = model.manual.as_mut() else {
        return Vec::new();
    };
    if manual.jump(jump) {
        announce_manual(model);
    } else {
        model.fail(match jump {
            Jump::Back => "no page to go back to",
            Jump::Forward => "no page to go forward to — <c-o> goes back",
        });
    }
    Vec::new()
}

/// `g/` — the cross-page search prompt, opening the manual first when it is
/// not already up.
fn open_manual_grep(model: &mut Model) -> Vec<Cmd> {
    // A prompt raised behind a modal is a prompt nobody can see themselves
    // typing into, so the *key* path refuses rather than opening one: whatever
    // that modal is, it is what the keyboard belongs to. The argument-carrying
    // path takes the modal down first instead — it was dispatched *by* one
    // (task 89's command line), which is a modal
    // asking to be replaced rather than one being talked over.
    if model.overlay.is_some() {
        return Vec::new();
    }
    open_manual_grep_for(model, "")
}

/// Show the cross-page hits for `pattern` — `:helpgrep <pattern>` with its
/// argument supplied.
///
/// The consumer of the `pattern` positional `command::explicit` declares, and
/// the counterpart of [`open_manual_at`]: [`run_action`] takes a count, not a
/// string, so an argument-carrying verb cannot reach this through the ordinary
/// action path and task 89 calls it directly. A blank pattern raises the
/// prompt rather than listing nothing, which is what a bare `:helpgrep` means.
pub fn open_manual_grep_for(model: &mut Model, pattern: &str) -> Vec<Cmd> {
    let stop = model.overlay.take().map(|overlay| cancels(&overlay));
    // The front page first, so `<c-o>` from the hit list has somewhere to go
    // rather than the list being a dead end.
    if model.screen != Screen::Manual {
        enter_manual(model, manual::Location::start());
    }
    let pattern = pattern.trim();
    let mut cmds = if pattern.is_empty() {
        prompt_manual(model, Scope::Manual)
    } else {
        enter_manual(model, manual::Location::Grep(pattern.to_owned()))
    };
    cmds.extend(stop.unwrap_or_default());
    cmds
}

/// Raise the manual's search line.
fn prompt_manual(model: &mut Model, scope: Scope) -> Vec<Cmd> {
    let Some(manual) = model.manual.as_mut() else {
        return Vec::new();
    };
    manual.prompt = Some(ManualPrompt {
        pattern: String::new(),
        scope,
    });
    model.info(match scope {
        Scope::Page => "search this page — Enter jumps to the first match, then n and N step",
        Scope::Manual => "search every page — Enter lists what mentions it",
    });
    Vec::new()
}

/// `Enter` on the manual's search line.
fn submit_manual_search(model: &mut Model) -> Vec<Cmd> {
    let Some(prompt) = model
        .manual
        .as_mut()
        .and_then(|manual| manual.prompt.take())
    else {
        return Vec::new();
    };
    let pattern = prompt.pattern.trim().to_owned();
    if pattern.is_empty() {
        // Nothing typed means nothing searched for, not everything matched —
        // the rule `search_now` follows for the mailbox box, for the same
        // reason.
        if let Some(manual) = model.manual.as_mut() {
            manual.highlight = None;
        }
        model.info("cancelled");
        return Vec::new();
    }
    match prompt.scope {
        Scope::Manual => enter_manual(model, manual::Location::Grep(pattern)),
        Scope::Page => search_manual_page(model, pattern),
    }
}

fn search_manual_page(model: &mut Model, pattern: String) -> Vec<Cmd> {
    let hits = manual_matches(model, &pattern);
    let from = model
        .manual
        .as_ref()
        .map_or(0, |manual| manual.cursor_in(hits_of(model)));
    // vim's `/`: forward from where the cursor already is, wrapping to the
    // top rather than reporting nothing when every hit is above it.
    let landing = hits
        .iter()
        .copied()
        .find(|line| *line >= from)
        .or_else(|| hits.first().copied());
    if let Some(manual) = model.manual.as_mut() {
        manual.highlight = Some(pattern.clone());
        if let Some(landing) = landing {
            manual.cursor = landing;
        }
    }
    if hits.is_empty() {
        model.fail(format!(
            "{pattern:?} is not on this page — g/ searches all of them"
        ));
    } else {
        model.info(format!(
            "{} line(s) match — n and N step through them",
            hits.len()
        ));
    }
    Vec::new()
}

/// `n` / `N`.
fn step_manual_match(model: &mut Model, direction: Direction) -> Vec<Cmd> {
    // Silent when the manual is not up at all. These are bound in the `help`
    // layer, which the `?` overlay shares — pressing `n` there would otherwise
    // paint a red "nothing searched for yet — / searches this page" over the
    // status line, about a page the reader is not on. `manual_jump` is silent
    // in the same situation for the same reason.
    if model.manual.is_none() {
        return Vec::new();
    }
    let Some(pattern) = model
        .manual
        .as_ref()
        .and_then(|manual| manual.highlight.clone())
    else {
        model.fail("nothing searched for yet — / searches this page");
        return Vec::new();
    };
    let hits = manual_matches(model, &pattern);
    if hits.is_empty() {
        model.fail(format!("{pattern:?} is no longer on this page"));
        return Vec::new();
    }
    let at = model
        .manual
        .as_ref()
        .map_or(0, |manual| manual.cursor_in(hits_of(model)));
    // Wrapping both ways: a step that stopped dead at the last hit would
    // leave the reader guessing whether there were more above it.
    let next = match direction {
        Direction::Down => hits
            .iter()
            .copied()
            .find(|line| *line > at)
            .or_else(|| hits.first().copied()),
        Direction::Up => hits
            .iter()
            .copied()
            .rev()
            .find(|line| *line < at)
            .or_else(|| hits.last().copied()),
    };
    let Some(next) = next else {
        return Vec::new();
    };
    if let Some(manual) = model.manual.as_mut() {
        manual.cursor = next;
    }
    let which = hits.iter().position(|line| *line == next).unwrap_or(0) + 1;
    model.info(format!("match {which} of {}", hits.len()));
    Vec::new()
}

/// How many rendered lines the open page has, for clamping a cursor against.
fn hits_of(model: &Model) -> usize {
    manual_doc(model).map_or(0, |doc| doc.lines.len())
}

/// Which rendered lines of the open page contain `pattern`.
fn manual_matches(model: &Model, pattern: &str) -> Vec<usize> {
    manual_doc(model)
        .map(|doc| manual::matching_lines(&doc, pattern))
        .unwrap_or_default()
}

/// `Enter` on a manual row: follow the link on it.
fn follow_manual_link(model: &mut Model) -> Vec<Cmd> {
    let (target, carry) = {
        let Some(manual) = model.manual.as_ref() else {
            return Vec::new();
        };
        let Some(doc) = manual_doc(model) else {
            return Vec::new();
        };
        let target = doc
            .lines
            .get(manual.cursor_in(doc.lines.len()))
            .and_then(manual::DocLine::link);
        // A hit list's rows carry their pattern with them: arriving on the
        // page with nothing highlighted would lose the one thing that made
        // the row a hit.
        let carry = match &manual.at {
            manual::Location::Grep(pattern) => Some(pattern.clone()),
            manual::Location::Page(_) => None,
        };
        (target, carry)
    };
    let Some(anchor) = target else {
        model.fail("no link on this line — j and k move, Enter follows one");
        return Vec::new();
    };
    let cmds = enter_manual(model, manual::Location::Page(anchor.to_owned()));
    if let Some(pattern) = carry {
        return [cmds, search_manual_page(model, pattern)].concat();
    }
    cmds
}

// ---------------------------------------------------------------------------
// task 85's overlays
// ---------------------------------------------------------------------------

/// Whether a new overlay may take the screen right now.
///
/// Only from a bare screen. Every overlay-opening action is bound in more
/// than one mode (`/` is in Normal *and* Menu, so it can take the search pane
/// back to its query line), and a stray press must never replace a pane that
/// is holding a streamed answer or a half-typed question.
fn screen_is_clear(model: &Model) -> bool {
    model.overlay.is_none()
}

/// `/` — open the search overlay, or take an open one back to its query line.
fn open_search(model: &mut Model) -> Vec<Cmd> {
    // `/` means "search what is in front of me" in every layer that binds it.
    // On the manual that is this page: opening the mailbox search overlay
    // there would cover the text it was pressed to search.
    if model.screen == Screen::Manual && model.overlay.is_none() {
        return prompt_manual(model, Scope::Page);
    }
    if let Some(Overlay::Search(pane)) = model.overlay.as_mut() {
        pane.focus = SearchFocus::Query;
        return Vec::new();
    }
    if !screen_is_clear(model) {
        return Vec::new();
    }
    model.overlay = Some(Overlay::Search(Box::default()));
    model.info("search — ~ semantic · = lexical · Tab completes an operator · Enter walks results");
    Vec::new()
}

/// Re-issue the search for whatever is in the box.
///
/// Not counted into [`Model::inflight`]: this stream is superseded on every
/// keystroke, so counting it would ratchet the busy marker up by one per
/// character typed and never bring it back down. The pane's own "searching…"
/// state is what says a search is running.
fn search_now(model: &mut Model) -> Vec<Cmd> {
    model.generation += 1;
    let generation = model.generation;
    let account_id = model.current_account().map_or(0, |account| account.id);
    let Some(Overlay::Search(pane)) = model.overlay.as_mut() else {
        return Vec::new();
    };
    pane.restart(generation);
    let query = pane.query.clone();
    // An empty box searches for nothing rather than for everything — and
    // stops whatever the last non-empty one started, which the generation
    // stamp alone would not do: a superseding request is what cancels these,
    // and issuing none supersedes nothing.
    if query.trim().is_empty() {
        pane.complete = true;
        return vec![
            Cmd::CancelStream {
                which: Stream::Search,
            },
            Cmd::CancelStream {
                which: Stream::Explain,
            },
        ];
    }
    vec![Cmd::Search {
        query,
        generation,
        account_id,
    }]
}

/// `Enter` on the query line: hand the keyboard to the results.
fn focus_results(model: &mut Model) -> Vec<Cmd> {
    let mut note = None;
    if let Some(Overlay::Search(pane)) = model.overlay.as_mut() {
        if pane.hits.is_empty() {
            note = Some(Err("no results to walk yet".to_owned()));
        } else {
            pane.focus = SearchFocus::Results;
            note = Some(Ok(
                "j/k moves · x explains · Enter opens · Esc closes".to_owned()
            ));
        }
    }
    apply_note(model, note);
    Vec::new()
}

/// `x` — the why-panel.
fn toggle_explain(model: &mut Model) -> Vec<Cmd> {
    let Some(Overlay::Search(pane)) = model.overlay.as_mut() else {
        return Vec::new();
    };
    pane.explain = !pane.explain;
    if !pane.explain {
        pane.explanation = None;
        pane.explaining = None;
        pane.explain_failed = None;
        return vec![Cmd::CancelStream {
            which: Stream::Explain,
        }];
    }
    // The request itself is `follow_cursor`'s, which runs after every message
    // — so the panel is filled the same way whether it was just opened or the
    // cursor moved under it.
    Vec::new()
}

/// Replace whatever is on screen with a search for `query` and run it. The
/// finder's jump targets that are not a message or a folder land here.
fn search_for(model: &mut Model, query: String) -> Vec<Cmd> {
    model.overlay = Some(Overlay::Search(Box::new(SearchPane {
        query,
        ..SearchPane::default()
    })));
    search_now(model)
}

/// `Ctrl-P` — the fuzzy finder.
fn open_finder(model: &mut Model) -> Vec<Cmd> {
    if !screen_is_clear(model) {
        return Vec::new();
    }
    model.overlay = Some(Overlay::Finder(Box::default()));
    model.info("find — > commands · # tags · @ people · / saved searches · : folders");
    // Opened with an empty prompt on purpose: an empty finder query means
    // "rank by signals alone", which is the recents-and-frequents list a
    // picker should already be showing when it appears.
    find_now(model)
}

/// Re-issue the find. Uncounted for the same reason [`search_now`] is.
fn find_now(model: &mut Model) -> Vec<Cmd> {
    model.generation += 1;
    let generation = model.generation;
    let account_id = model.current_account().map_or(0, |account| account.id);
    let Some(Overlay::Finder(pane)) = model.overlay.as_mut() else {
        return Vec::new();
    };
    pane.restart(generation);
    let query = pane.query.clone();
    vec![Cmd::Find {
        query,
        generation,
        account_id,
    }]
}

/// `Enter` in the finder: go to whatever is highlighted.
///
/// What "go to" means is the item's kind, and the mapping is deliberately
/// total — a kind this build does not know is refused rather than guessed at,
/// because `ref_id` is a row id in whichever table the kind names and those
/// id spaces overlap (tag 7, mailbox 7 and message 7 all exist).
fn activate_finder(model: &mut Model) -> Vec<Cmd> {
    let item = match model.overlay.as_ref() {
        Some(Overlay::Finder(pane)) => pane.item().cloned(),
        _ => return Vec::new(),
    };
    let Some(item) = item else {
        model.fail("nothing to jump to");
        return Vec::new();
    };
    match item.kind {
        FinderKind::Message => open_message_by_id(model, item.ref_id),
        FinderKind::Mailbox => {
            model.overlay = None;
            open_folder_by_id(model, item.ref_id)
        }
        // A saved search's second line *is* its query text; a tag and a
        // contact become the operator that selects them. All three go through
        // the same grammar the search box does, so none of them is a second
        // way to express a filter.
        FinderKind::SavedSearch => search_for(model, item.secondary.clone()),
        FinderKind::Tag => search_for(model, format!("tag:{}", item.primary)),
        FinderKind::Contact => {
            let who = if item.secondary.trim().is_empty() {
                &item.primary
            } else {
                &item.secondary
            };
            search_for(model, format!("from:{who}"))
        }
        FinderKind::Command => run_command_id(model, &item.secondary.clone()),
        FinderKind::Unknown => {
            model.fail("this build does not know that kind of result");
            Vec::new()
        }
    }
}

/// `:` — the command line. `Ctrl-K` opens the same overlay.
fn open_command(model: &mut Model) -> Vec<Cmd> {
    // `:` is bound in `Menu` as well as `Normal`, so a list overlay is the
    // one thing it may open *over* — and it replaces that overlay rather
    // than stacking on it, taking whatever it was streaming down with it.
    // The alternative reading, "refuse while anything is up", makes the
    // binding dead in the layer it was deliberately added to; the reading
    // after that, "restore the menu on Esc", is an overlay stack this model
    // does not have and would leave a restored search pane holding results
    // whose stream was cancelled. A modal that answers `:` is a modal asking
    // to be replaced — the same call `open_manual_grep_for` makes when it is
    // dispatched from one.
    let stop = if model.mode() == Mode::Menu {
        model.overlay.take().map(|overlay| cancels(&overlay))
    } else if screen_is_clear(model) {
        None
    } else {
        return Vec::new();
    };
    // vim's own behaviour, and the reason `'<,'>` is spelled the way it is:
    // opening `:` over a selection means "act on this", so the range is
    // already there rather than something to remember to type.
    // `is_selecting`, not `visual.is_some()`: the anchor outlives leaving the
    // list (task 103), so the raw field is set in the viewer too — and there
    // `Model::selection` returns `None`, so a prefilled `'<,'>` would be a
    // range nothing could honour. `Model::is_selecting`'s own docs name this
    // as the mistake that once let `a` archive the viewer's message while `r`
    // refused, citing a selection drawn nowhere.
    let input = if model.is_selecting() {
        SELECTION_RANGE.to_owned()
    } else {
        String::new()
    };
    model.overlay = Some(Overlay::Command(Box::new(CommandPane {
        input,
        ..CommandPane::default()
    })));
    refresh_command(model);
    model.info("command — type a verb, Enter runs it, Tab completes");
    stop.unwrap_or_default()
}

/// The range prefix a `:` opened over a visual selection starts with.
const SELECTION_RANGE: &str = "'<,'>";

fn refresh_command(model: &mut Model) {
    let Some(Overlay::Command(pane)) = model.overlay.as_ref() else {
        return;
    };
    let matches = command_matches(&pane.input.clone(), &model.keymap);
    if let Some(Overlay::Command(pane)) = model.overlay.as_mut() {
        pane.matches = matches;
    }
}

/// `Enter` on the command line.
///
/// Three outcomes, and the overlay only closes on the first: the line names a
/// verb and it runs; the line names *no* verb, in which case the best-ranked
/// match runs, which is what keeps task 85's palette — type a fuzzy name,
/// press Enter — working through the same pane; or the line does not parse at
/// all, and the complaint is rendered inside the line with the offending text
/// still there to fix.
///
/// The fallback is deliberately narrow. Only [`CommandError::UnknownVerb`]
/// takes it, because that is the one failure that means "you have not
/// finished naming it yet". A malformed range or an unterminated quote is a
/// line whose *shape* is wrong, and quietly running something else because
/// the verb inside it happened to rank first would be a keystroke doing what
/// nobody asked.
fn submit_command(model: &mut Model) -> Vec<Cmd> {
    let Some(Overlay::Command(pane)) = model.overlay.as_ref() else {
        return Vec::new();
    };
    let line = pane.input.trim().to_owned();
    match command::parse(&line) {
        Ok(command::Resolution::Invocation(invocation)) => {
            record_command(model, &line);
            run_invocation(model, *invocation)
        }
        Ok(command::Resolution::Children { path, children }) => {
            let mut names: Vec<String> = children
                .iter()
                .filter_map(|verb| verb.path.get(path.len()).map(|s| (*s).to_owned()))
                .collect();
            names.sort_unstable();
            names.dedup();
            complain(
                model,
                format!("{} needs one of: {}", path.join(" "), names.join(", ")),
            )
        }
        Err(command::CommandError::UnknownVerb { .. }) if carries_a_flag(&line) => complain(
            model,
            format!("{line:?} names no command, and its flags cannot be guessed"),
        ),
        Err(command::CommandError::UnknownVerb { .. }) => match best_match(model) {
            Some(verb) => {
                record_command(model, &line);
                run_best(model, &verb)
            }
            None => complain(model, format!("no command matches {line:?}")),
        },
        Err(error) => complain(model, error.to_string()),
    }
}

/// Whether `line` carries a flag.
///
/// The fallback rebuilds `range + verb + bang` and nothing else, because a
/// fuzzy verb cannot tell an abbreviation from an argument — so a line with a
/// flag on it must be refused rather than run without it. `:message archive
/// --force` is already refused by the parser; `:arch --force` has to be
/// refused here, or the abbreviation is *less* strict than the spelling.
fn carries_a_flag(line: &str) -> bool {
    line.split_whitespace().any(|word| word.starts_with('-'))
}

/// Show `why` inside the command line, leaving the overlay up — or on the
/// status line when there is no command line to show it in.
///
/// The fallback is not decoration: a report row's command is dispatched with
/// the report down and no command line ever opened (task 90's [`run_row`]), and
/// a refusal written only into an overlay that is not there is a keystroke that
/// silently did nothing.
fn complain(model: &mut Model, why: String) -> Vec<Cmd> {
    if let Some(Overlay::Command(pane)) = model.overlay.as_mut() {
        pane.error = Some(why);
        return Vec::new();
    }
    model.fail(why);
    Vec::new()
}

/// The best-ranked match's verb path, if the pane has one.
fn best_match(model: &Model) -> Option<String> {
    match model.overlay.as_ref() {
        Some(Overlay::Command(pane)) => pane.best().map(|entry| entry.verb.clone()),
        _ => None,
    }
}

/// Run the ranked fallback: the verb itself, with whatever range and bang the
/// typed line carried, and nothing else — so `:'<,'>arch` and `:del!` mean
/// what they look like they mean.
///
/// Re-parsed rather than dispatched from the [`CommandEntry`] directly, so
/// this path and the exact-match path above are the same code from here on —
/// a fallback with its own dispatch would be a second place for `'<,'>` and
/// `!` to be honoured, free to drift from the first.
fn run_best(model: &mut Model, verb: &str) -> Vec<Cmd> {
    let (prefix, bang) = match model.overlay.as_ref() {
        Some(Overlay::Command(pane)) => (
            range_prefix(&pane.input),
            pane.input.trim_end().ends_with('!'),
        ),
        _ => (String::new(), false),
    };
    let bang = if bang { "!" } else { "" };
    match command::parse(&format!("{prefix}{verb}{bang}")) {
        Ok(command::Resolution::Invocation(invocation)) => run_invocation(model, *invocation),
        // Unreachable: `verb` came out of the registry, so it resolves. A
        // status line saying so beats an `unwrap` that cannot be reasoned
        // about from the call site.
        _ => complain(model, format!("{verb:?} did not resolve")),
    }
}

/// The range `input` opens with, as typed — what a fallback dispatch has to
/// carry over. Empty when there is none.
fn range_prefix(input: &str) -> String {
    let rest = input.trim_start();
    if let Some(kept) = rest.strip_prefix(SELECTION_RANGE) {
        return input[..input.len() - kept.len()].to_owned();
    }
    if let Some(kept) = rest.strip_prefix('%') {
        return input[..input.len() - kept.len()].to_owned();
    }
    let digits = rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
    if digits == 0 {
        return String::new();
    }
    input[..input.len() - rest.len() + digits].to_owned()
}

/// Record `line` in the history, and rewrite the file when it took.
///
/// Called once the line *parses*, not once it succeeds. A verb refused for
/// its range or its arguments is exactly the line somebody wants `<up>` to
/// bring back and fix; a line that did not parse at all never left the
/// overlay, so there is nothing to recall.
fn record_command(model: &mut Model, line: &str) {
    if model.history.record(line) {
        model.pending_history = true;
    }
}

/// Dispatch a parsed `:` line.
///
/// The delegation this task exists for: a verb that carries an [`Action`] and
/// was typed with no arguments is [`run_action`], unchanged. The 39
/// behaviours the keyboard already reaches keep exactly one implementation,
/// and a `:` line cannot drift from the key that runs the same thing.
///
/// What is *not* delegated is everything the action signature cannot express:
/// [`run_action`] takes a count and nothing else, so an argument-carrying
/// verb is dispatched here by name. Two exist today, both task 103's.
fn run_invocation(model: &mut Model, invocation: command::Invocation) -> Vec<Cmd> {
    let verb = invocation.verb.join(" ");
    if let Some(why) = unsupported_range(model, &verb, invocation.range) {
        return complain(model, why);
    }
    // Argument-carrying verbs first: these are the ones an `Action` cannot
    // carry, so they are named rather than delegated.
    if matches!(verb.as_str(), "manual grep" | "helpgrep") {
        // Joined rather than `positionals.first()`: an unquoted multi-word
        // pattern is what somebody types, and searching only its first word
        // while dropping the rest is the silent-truncation answer. It is
        // also what `:helpgrep` means in vim, where the pattern is the rest
        // of the line.
        let pattern = invocation.positionals.join(" ");
        close_command(model);
        return open_manual_grep_for(model, &pattern);
    }
    if verb == "set" {
        // Left open on a bad option/value (`complain` writes into the
        // command pane's own error line) and only closed once `set_option`
        // has actually applied something — the same rule the generic path
        // below follows for a bad flag or a missing action.
        let [option, value] = invocation.positionals.as_slice() else {
            return complain(
                model,
                "set needs two arguments: an option and a value".to_owned(),
            );
        };
        return set_option(model, option, value);
    }
    if !invocation.positionals.is_empty() {
        return complain(
            model,
            format!(
                "{verb} takes no arguments, and was given {}",
                invocation.positionals.join(" ")
            ),
        );
    }
    if let Some(flag) = invocation.flags.first() {
        // Defence, not a path anything reaches today: `command::parse`
        // rejects a flag no verb declares, and no verb declares one. Worded
        // for the case that *would* arrive first — a declared flag this
        // dispatch has not learned — rather than claiming the verb has no
        // such flag, which by then would be false.
        return complain(model, format!("{verb} --{}: not wired up yet", flag.name));
    }
    // Task 90's verbs, after the argument guards rather than before them: these
    // reach a capability with no `Action` behind them, so there is nothing to
    // delegate to and they are named here — and a flag neither of them declares
    // has to be refused rather than silently dropped on the way past.
    // `auth status` answers with rows and opens a report; `auth clear` answers
    // with a fact and does not.
    if verb == "auth clear" {
        return run_auth_clear(model);
    }
    if invocation.action.is_none() && invocation.capability.is_some() {
        return open_report(model, invocation);
    }
    let Some(action) = invocation.action else {
        return complain(model, format!("{verb} is not something this TUI runs"));
    };
    close_command(model);
    let mut cmds = run_action(model, action, None);
    // `!` means "skip the confirmation", and only that. Applied here rather
    // than inside each action for the same reason the range is: an action
    // that opened a `Confirm` is the *only* thing a bang changes, and one
    // implementation of that is one place it can be wrong.
    if invocation.bang && matches!(model.overlay, Some(Overlay::Confirm { .. })) {
        cmds.extend(accept_confirm(model));
    }
    cmds
}

/// The tunables `:set` reaches.
enum PaneOption {
    Folder,
    Preview,
    AiPanel,
}

/// `:set <option> <value>` — the pane widths and the AI panel width are the
/// only tunables this grammar reaches yet. A fuller settings surface is task
/// 101's `Screen::Settings`; when it lands, this is where a new `Invocation`
/// it wants to issue for one of its own fields should keep landing too,
/// rather than a second `:set`-shaped path growing next to it.
fn set_option(model: &mut Model, option: &str, value: &str) -> Vec<Cmd> {
    // `option` is matched into `PaneOption` before `value` is parsed: `set
    // bogus abc` should say `bogus` is not an option this build has, not
    // that `abc` is not a number — the second reads as though `bogus` were
    // real and only its value were wrong. Matching once into an enum, not
    // twice against `&str`, is also what keeps the acting `match` below
    // exhaustive with no wildcard arm — a fourth option added to this list
    // and forgotten in the other would otherwise silently report "unknown
    // option" for something real, or need a dead arm to stay total.
    let kind = match option {
        "folder-width" => PaneOption::Folder,
        "preview-width" => PaneOption::Preview,
        "ai-panel-width" => PaneOption::AiPanel,
        _ => return complain(model, format!("unknown option: {option}")),
    };
    let Ok(pct) = value.parse::<u16>() else {
        return complain(
            model,
            format!("{option}: \"{value}\" is not a whole number"),
        );
    };
    match kind {
        PaneOption::Folder => {
            if let Err(why) = check_pane_pct(pct, model.preview_width_pct, "folder-width") {
                return complain(model, why);
            }
            model.folder_width_pct = pct;
        }
        PaneOption::Preview => {
            if let Err(why) = check_pane_pct(pct, model.folder_width_pct, "preview-width") {
                return complain(model, why);
            }
            model.preview_width_pct = pct;
        }
        PaneOption::AiPanel => {
            if !(MIN_AI_PANEL_PCT..=MAX_AI_PANEL_PCT).contains(&pct) {
                return complain(
                    model,
                    format!("ai-panel-width must be {MIN_AI_PANEL_PCT}-{MAX_AI_PANEL_PCT}"),
                );
            }
            model.ai_panel_width_pct = pct;
        }
    }
    close_command(model);
    model.info(format!("{option} set to {pct}"));
    Vec::new()
}

/// `pct`, and its counterpart `other` (the other of `folder-width`/
/// `preview-width`), against the bounds [`set_option`] enforces so
/// `render_panes` never has to: `pct` alone in range, and the pair leaving
/// the message list at least `100 - MAX_PANES_PCT` wide.
fn check_pane_pct(pct: u16, other: u16, name: &str) -> Result<(), String> {
    if !(MIN_PANE_PCT..=MAX_PANE_PCT).contains(&pct) {
        return Err(format!("{name} must be {MIN_PANE_PCT}-{MAX_PANE_PCT}"));
    }
    if pct + other > MAX_PANES_PCT {
        return Err(format!(
            "folder-width + preview-width must not exceed {MAX_PANES_PCT} \
             (the message list needs the rest)"
        ));
    }
    Ok(())
}

/// Why this range cannot be honoured, or `None` when it can.
///
/// `'<,'>` is honoured by delegation and needs no code of its own: every
/// bulk-capable action already reads [`Model::selection`], so a `:` line
/// carrying the selection range does exactly what the key does with the same
/// selection up. The other two have no model support at all — nothing here
/// can address "every row listed" or "N rows down" — and saying so is the
/// only honest answer. Silently acting on one message instead would be a
/// range that looked honoured and was not.
fn unsupported_range(model: &Model, verb: &str, range: Option<command::Range>) -> Option<String> {
    match range {
        None => None,
        Some(command::Range::Selection) if !model.is_selecting() => {
            Some("'<,'> needs a visual selection on the message list".to_owned())
        }
        // A range names a set of *messages*, so a verb that reaches no
        // capability at all reaches no message either, and a range on it is
        // not something to quietly ignore. Derived from the parity table
        // rather than from a list here, so a verb gaining a capability gains
        // the range with it.
        Some(command::Range::Selection) if !acts_on_mail(verb) => Some(format!(
            "'<,'> is a set of messages, and {verb} does not act on one"
        )),
        Some(command::Range::Selection) => None,
        Some(command::Range::All) => {
            Some("% is not supported yet: select the rows and use '<,'>".to_owned())
        }
        Some(command::Range::Count(_)) => {
            Some("a count range is not supported yet: select the rows and use '<,'>".to_owned())
        }
    }
}

/// Whether a verb acts on mail, and so has something for a range to mean.
///
/// Read off `parity::Command`: a verb whose action reaches a capability acts
/// on the mailbox, and one that reaches none — `help`, `cursor.down`,
/// `manual` — is local to this screen. That is exactly the distinction
/// `LOCAL_ACTIONS` already draws, so there is no second list to keep in step.
fn acts_on_mail(verb: &str) -> bool {
    let path: Vec<&str> = verb.split(' ').collect();
    command::verb_at(&path).is_some_and(|verb| {
        verb.capability.is_some()
            || verb
                .action
                .is_some_and(|action| Capability::for_action(action).next().is_some())
    })
}

/// Take the command line down before running what it named.
///
/// Not tidiness: every action reads [`Model::mode`], and one run against an
/// overlay that is still up would ask the *command line* what `cursor.down`
/// means rather than the screen it is about to reveal.
fn close_command(model: &mut Model) {
    model.overlay = None;
}

/// Walk the history, filtered by whatever was typed before the walk began.
///
/// `<up>` from an empty line walks everything; from `mess` it walks only the
/// lines that start with it, which is what makes a long invocation
/// recoverable by its first word rather than by counting presses.
fn browse_history(model: &mut Model, direction: Direction) -> bool {
    // Destructured rather than `model.history.clone()`: this runs on every
    // `<up>`, and cloning five hundred strings to read a prefix off them is
    // work proportional to the history for a keystroke that is not.
    let Model {
        history, overlay, ..
    } = model;
    let Some(Overlay::Command(pane)) = overlay.as_mut() else {
        return false;
    };
    let seed = match &pane.browse {
        Some(browse) => browse.seed.clone(),
        None => pane.input.clone(),
    };
    let matches = history.matching(&seed);
    if matches.is_empty() {
        return true;
    }
    let at = pane.browse.as_ref().map(|browse| browse.at);
    let next = match (direction, at) {
        (Direction::Up, None) => Some(0),
        (Direction::Up, Some(at)) => Some((at + 1).min(matches.len() - 1)),
        (Direction::Down, None) => None,
        (Direction::Down, Some(0)) => None,
        (Direction::Down, Some(at)) => Some(at - 1),
    };
    match next {
        Some(at) => {
            // `at` came from `min(len - 1)` or from `at - 1`, so it indexes
            // a row that exists; the seed is the answer for the branch that
            // cannot happen rather than an `unwrap` nobody can check.
            pane.input = truncated(
                matches
                    .get(at)
                    .map_or_else(|| seed.clone(), |line| (*line).to_owned()),
            );
            pane.browse = Some(Browse { seed, at });
        }
        None => {
            pane.input = truncated(seed);
            pane.browse = None;
        }
    }
    pane.error = None;
    refresh_command(model);
    true
}

// ---------------------------------------------------------------------------
// reports
// ---------------------------------------------------------------------------

/// The title, columns and request a reporting verb answers with, or `None`
/// when this build has no report for it.
///
/// The seam tasks 94 onward extend. Keyed on the verb path rather than on the
/// capability because the *presentation* is a client decision — how wide a
/// column is, what the border says — and two verbs over one capability may
/// well want different ones. One function rather than one per caller so
/// opening a report and re-running it cannot disagree about either.
fn report_spec(verb: &str, generation: u64) -> Option<(String, Vec<ReportColumn>, Cmd)> {
    match verb {
        "auth status" => Some((
            "auth — access to rmail's own API".to_owned(),
            vec![
                ReportColumn::new("setting", 21),
                ReportColumn::new("state", 48),
            ],
            Cmd::AuthStatus { generation },
        )),
        _ => None,
    }
}

/// Open a report for `invocation`, or say why this build has none for it.
fn open_report(model: &mut Model, invocation: command::Invocation) -> Vec<Cmd> {
    let verb = invocation.verb.join(" ");
    let generation = model.generation + 1;
    let Some((title, columns, cmd)) = report_spec(&verb, generation) else {
        return complain(
            model,
            format!("{verb} reaches the daemon, but this build has no report for it"),
        );
    };
    model.generation = generation;
    close_command(model);
    model.overlay = Some(Overlay::Report(Box::new(ReportPane::new(
        invocation, title, columns, generation,
    ))));
    model.info(format!("{verb} — r re-runs · Esc closes"));
    vec![cmd]
}

/// `r` — run this report's own `:` line again.
///
/// Restarted in place rather than rebuilt, so the pane keeps its title, its
/// columns and — deliberately — its cursor (see [`ReportPane::restart`]): a
/// re-run is the same report asked again, not a differently shaped one.
///
/// The new generation is what makes it immune to the previous run's tail: a
/// frame still in flight is dropped by [`ReportPane::apply`] rather than mixed
/// into the new answer. No [`Cmd::CancelStream`] is issued, for the reason
/// `restart_search` spells out — the *superseding request* is what cancels the
/// old stream (`tui::grpc`'s `reporting` slot aborts the task it replaces), and
/// an explicit cancel is needed only where no new request follows.
fn rerun_report(model: &mut Model) -> Vec<Cmd> {
    let Some(Overlay::Report(pane)) = model.overlay.as_ref() else {
        return Vec::new();
    };
    let verb = pane.invocation.verb.join(" ");
    let generation = model.generation + 1;
    let Some((_, _, cmd)) = report_spec(&verb, generation) else {
        // Unreachable: the pane exists because `report_spec` answered for this
        // verb when it opened, and the registry does not change at runtime. A
        // status line saying so beats an `unwrap` nobody can check from here.
        model.fail(format!("{verb} can no longer be re-run"));
        return Vec::new();
    };
    model.generation = generation;
    if let Some(Overlay::Report(pane)) = model.overlay.as_mut() {
        pane.restart(generation);
    }
    model.info(format!("{verb} — re-running…"));
    vec![cmd]
}

/// `<enter>` on a report row: run what the row carries, asking first when it
/// mutates.
///
/// The gate reads [`Capability::effect`] through [`report::mutates`] rather
/// than a list of dangerous verbs kept here — see that function's docs. A
/// mutating row becomes an [`Overlay::Confirm`] carrying both the bang'd
/// invocation and the report itself, so `y` runs it without asking twice and
/// either answer leaves the reader on the screen they were reading.
fn run_report_row(model: &mut Model, invocation: command::Invocation) -> Vec<Cmd> {
    let Some(Overlay::Report(over)) = model.overlay.take() else {
        return Vec::new();
    };
    if !report::mutates(&invocation) || invocation.bang {
        return run_row(model, invocation, over);
    }
    let prompt = format!(":{} — run it? [y/N]", invocation.verb.join(" "));
    model.overlay = Some(Overlay::Confirm {
        prompt,
        then: Confirmed::Invoke {
            invocation: Box::new(command::Invocation {
                bang: true,
                ..invocation
            }),
            over,
        },
    });
    Vec::new()
}

/// Run a row's command with the report down, and put the report back unless
/// the command put something else in its place.
///
/// Down first for the reason [`close_command`] takes the command line down:
/// every action reads [`Model::mode`], and `cursor.down` dispatched with the
/// report still up would move the *report's* cursor rather than doing whatever
/// the row's verb is about.
///
/// Back afterwards, marked stale. Stale rather than re-read: the mutation is
/// still in flight when this returns, so a re-run issued here would race it and
/// could redraw the state from *before* the change — which is worse than saying
/// plainly that the rows are from before it. `r` is what re-reads, and the
/// title says so.
fn run_row(
    model: &mut Model,
    invocation: command::Invocation,
    mut over: Box<ReportPane>,
) -> Vec<Cmd> {
    let stale = report::mutates(&invocation);
    let cmds = run_invocation(model, invocation);
    if model.overlay.is_none() {
        over.stale = over.stale || stale;
        model.overlay = Some(Overlay::Report(over));
    }
    cmds
}

/// What the status line says once a report has finished arriving.
fn report_summary(pane: &ReportPane) -> String {
    let verb = pane.invocation.verb.join(" ");
    match pane.rows.len() {
        0 => format!("{verb} — nothing to report"),
        1 => format!("{verb} — 1 row · r re-runs"),
        n => format!("{verb} — {n} rows · r re-runs"),
    }
}

/// `:auth clear` — remove the password gate.
///
/// Not a report: it answers with a fact, not with rows, and the status line is
/// where a one-line answer belongs. It is here because it is the mutating row
/// `:auth status`'s report offers, and the confirmation that row goes through
/// is [`run_report_row`]'s — typed bare on the command line it is confirmed by
/// nothing, exactly as `mail auth clear` is, because a line somebody typed in
/// full is already the deliberate act a confirmation asks for.
fn run_auth_clear(model: &mut Model) -> Vec<Cmd> {
    close_command(model);
    model.inflight += 1;
    model.info("clearing the password…");
    vec![Cmd::AuthClear]
}

/// Run `id` as a command, if this build has one by that name.
fn run_command_id(model: &mut Model, id: &str) -> Vec<Cmd> {
    let Some(action) = Action::from_id(id) else {
        model.fail(format!("this build has no command {id:?}"));
        return Vec::new();
    };
    run_named(model, action)
}

/// Close the overlay, then do the named thing.
///
/// Closing first is not tidiness: every action reads `Model::mode()`, and one
/// run against an overlay that is still up would ask the *overlay* what
/// `cursor.down` means rather than the screen it is about to reveal.
fn run_named(model: &mut Model, action: Action) -> Vec<Cmd> {
    model.overlay = None;
    run_action(model, action, None)
}

/// `A` — the ask pane, with `question` already typed.
fn open_ask(model: &mut Model, question: String) -> Vec<Cmd> {
    // Reachable from a bare screen and from the `.` menu, which is the one
    // overlay that exists to launch others.
    if !matches!(model.overlay, None | Some(Overlay::Quick(_))) {
        return Vec::new();
    }
    model.overlay = Some(Overlay::Ask(Box::new(AskPane {
        question,
        ..AskPane::default()
    })));
    model.info("ask — type a question about your mail, Enter sends it");
    Vec::new()
}

/// `Enter` on the question line: send it.
fn ask_now(model: &mut Model) -> Vec<Cmd> {
    let question = match model.overlay.as_ref() {
        Some(Overlay::Ask(pane)) => pane.question.trim().to_owned(),
        _ => return Vec::new(),
    };
    if question.is_empty() {
        model.fail("ask what?");
        return Vec::new();
    }
    model.generation += 1;
    let generation = model.generation;
    let account_id = model.current_account().map_or(0, |account| account.id);
    let Some(Overlay::Ask(pane)) = model.overlay.as_mut() else {
        return Vec::new();
    };
    // Reset wholesale rather than field by field: a re-ask that carried over
    // the previous answer's citations would show sources for prose that is no
    // longer on screen.
    **pane = AskPane {
        question: pane.question.clone(),
        generation,
        phase: AskPhase::Streaming,
        ..AskPane::default()
    };
    model.info("asking…");
    vec![Cmd::Ask {
        question,
        generation,
        account_id,
    }]
}

/// `\` — the collapsible AI panel.
fn toggle_ai_panel(model: &mut Model) -> Vec<Cmd> {
    model.ai_panel = !model.ai_panel;
    if model.ai_panel {
        model.info("AI panel — cached analysis only; `.` offers the calls that cost");
    } else {
        model.summary = None;
        model.summary_for = None;
        model.summary_pinned = None;
        model.summary_failed = None;
        model.info("AI panel hidden");
    }
    // The load is `follow_cursor`'s, so opening the panel and moving the
    // cursor under it take exactly the same path.
    Vec::new()
}

/// `.` — the AI quick-action menu for the message under the cursor.
fn open_quick(model: &mut Model) -> Vec<Cmd> {
    if !screen_is_clear(model) {
        return Vec::new();
    }
    let Some((message_id, subject)) = target_subject(model) else {
        model.fail("no message selected");
        return Vec::new();
    };
    model.overlay = Some(Overlay::Quick(QuickPane {
        message_id,
        subject,
        cursor: 0,
    }));
    Vec::new()
}

fn run_quick(model: &mut Model, action: QuickAction) -> Vec<Cmd> {
    let (message_id, subject) = match model.overlay.as_ref() {
        Some(Overlay::Quick(pane)) => (pane.message_id, pane.subject.clone()),
        _ => return Vec::new(),
    };
    match action {
        QuickAction::Summarize => ai_panel_for(model, message_id, false),
        QuickAction::Ask => open_ask(model, format!("About \"{subject}\": ")),
        QuickAction::SuggestReply => ai_panel_for(model, message_id, true),
    }
}

/// Open the AI panel on one specific message.
fn ai_panel_for(model: &mut Model, message_id: i64, suggest_reply: bool) -> Vec<Cmd> {
    model.overlay = None;
    model.ai_panel = true;
    model.summary = None;
    model.summary_for = Some(message_id);
    // Pinned: this is the panel being *aimed*, not the panel following the
    // cursor. A list reloading a second later re-clamps `message_idx` with
    // nobody pressing a key, and without the pin the answer — for a reply
    // suggestion, a paid one — would be discarded in the same `update` that
    // delivered it. Deliberate movement releases it; see `unpin_summary`.
    model.summary_pinned = Some(message_id);
    model.summary_failed = None;
    model.inflight += 1;
    model.info(if suggest_reply {
        "asking the model for a reply…"
    } else {
        "reading the cached analysis…"
    });
    vec![Cmd::LoadSummary {
        message_id,
        suggest_reply,
    }]
}

/// `O` — the outbox pseudo-folder.
fn open_outbox(model: &mut Model) -> Vec<Cmd> {
    if !screen_is_clear(model) {
        return Vec::new();
    }
    let Some(account_id) = model.current_account().map(|account| account.id) else {
        model.fail("no account");
        return Vec::new();
    };
    model.overlay = Some(Overlay::Outbox(Box::new(OutboxPane {
        loading: true,
        ..OutboxPane::default()
    })));
    model.inflight += 1;
    model.info("outbox — u cancels the highlighted send");
    vec![Cmd::LoadOutbox { account_id }]
}

/// `u` — cancel a send.
///
/// In the outbox pane that is the highlighted row; anywhere else it is the
/// toast, which is the only send the user can see from the message list.
fn undo_send(model: &mut Model) -> Vec<Cmd> {
    let target = match model.overlay.as_ref() {
        Some(Overlay::Outbox(pane)) => pane
            .row()
            .map(|row| (row.id, row.to.clone(), row.state.clone())),
        _ => undo_toast(model)
            .map(|toast| (toast.outbox_id, toast.to.clone(), "scheduled".to_owned())),
    };
    let Some((outbox_id, to, state)) = target else {
        model.fail("nothing to undo");
        return Vec::new();
    };
    // The daemon refuses these too — `CancelScheduled` is the authority — but
    // saying so here costs a round trip nobody needs and reads better than
    // FAILED_PRECONDITION does.
    if matches!(state.as_str(), "sent" | "canceled" | "sending") {
        model.fail(format!("that one is already {state}"));
        return Vec::new();
    }
    if undo_toast(model).is_some_and(|toast| toast.outbox_id == outbox_id) {
        remove_undo_toast(model);
    }
    model.inflight += 1;
    model.info(format!("cancelling the send to {to}…"));
    vec![Cmd::CancelSend { outbox_id }]
}

/// `Enter` in a list overlay.
fn menu_accept(model: &mut Model) -> Vec<Cmd> {
    enum Chosen {
        Message(i64),
        Outbox,
        Quick(QuickAction),
        /// A report row carrying something to run.
        Row(Box<command::Invocation>),
        /// The row under the cursor is a manual link.
        ManualLink,
        /// There is no row cursor here, so "use the highlighted row" is
        /// "close this" — which is what `<enter>` has meant on the `?`
        /// overlay since task 83.
        Close,
        Nothing,
    }
    let chosen = match model.overlay.as_ref() {
        Some(Overlay::Search(pane)) => pane
            .hit()
            .map_or(Chosen::Nothing, |hit| Chosen::Message(hit.message_id)),
        Some(Overlay::Ask(pane)) => pane.citation().map_or(Chosen::Nothing, |citation| {
            Chosen::Message(citation.message_id)
        }),
        Some(Overlay::Outbox(_)) => Chosen::Outbox,
        Some(Overlay::Quick(pane)) => pane.action().map_or(Chosen::Nothing, Chosen::Quick),
        Some(Overlay::Report(pane)) => pane
            .row()
            .and_then(|row| row.on_enter.clone())
            .map_or(Chosen::Nothing, |invocation| {
                Chosen::Row(Box::new(invocation))
            }),
        // Task 102 gives the key reference a row cursor of its own; until it
        // does, `<enter>` there closes it, exactly as before this action was
        // bound to that key.
        Some(Overlay::Help) => Chosen::Close,
        None if model.screen == Screen::Manual => Chosen::ManualLink,
        _ => Chosen::Nothing,
    };
    match chosen {
        Chosen::Message(message_id) => open_message_by_id(model, message_id),
        Chosen::Outbox => describe_outbox_row(model),
        Chosen::Quick(action) => run_quick(model, action),
        Chosen::Row(invocation) => run_report_row(model, *invocation),
        Chosen::ManualLink => follow_manual_link(model),
        Chosen::Close => leave(model, Leave::ThenNothing),
        Chosen::Nothing => Vec::new(),
    }
}

/// The outbox has nothing to open — a scheduled send is not a message in a
/// folder yet — so `Enter` says what the highlighted row's state means
/// instead, which is the only thing anyone reads an outbox for.
fn describe_outbox_row(model: &mut Model) -> Vec<Cmd> {
    let note = match model.overlay.as_ref() {
        Some(Overlay::Outbox(pane)) => pane.row().map(|row| match &row.last_error {
            Some(error) => Err(format!("{}: {error}", row.state)),
            None => Ok(format!("{} — to {}", row.state, row.to)),
        }),
        _ => None,
    };
    apply_note(model, note);
    Vec::new()
}

/// Open a message by id, from wherever the overlay found it.
fn open_message_by_id(model: &mut Model, message_id: i64) -> Vec<Cmd> {
    model.overlay = None;
    model.opening = Some(message_id);
    model.info("opening…");
    model.inflight += 1;
    vec![Cmd::Open { message_id }]
}

/// Select and load folder `mailbox_id`, if this account has it.
fn open_folder_by_id(model: &mut Model, mailbox_id: i64) -> Vec<Cmd> {
    let Some(idx) = model.folders.iter().position(|f| f.id == mailbox_id) else {
        model.fail("that folder is not in this account");
        return Vec::new();
    };
    model.folder_idx = idx;
    set_screen(model, Screen::List);
    open_folder(model)
}

/// `Enter` on a typing overlay's prompt.
fn prompt_accept(model: &mut Model) -> Vec<Cmd> {
    enum Which {
        SearchQuery,
        Finder,
        Command,
        AskQuestion,
        Nothing,
    }
    // The manual's search line first, and unconditionally: it is the only
    // typing surface that is a screen rather than an overlay, so an overlay
    // cannot be up at the same time as it.
    if model
        .manual
        .as_ref()
        .is_some_and(|manual| manual.typing() && model.overlay.is_none())
    {
        return submit_manual_search(model);
    }
    let which = match model.overlay.as_ref() {
        Some(Overlay::Search(pane)) if pane.typing() => Which::SearchQuery,
        Some(Overlay::Finder(_)) => Which::Finder,
        Some(Overlay::Command(_)) => Which::Command,
        Some(Overlay::Ask(pane)) if pane.typing() => Which::AskQuestion,
        _ => Which::Nothing,
    };
    match which {
        Which::Nothing => Vec::new(),
        Which::SearchQuery => focus_results(model),
        Which::Finder => activate_finder(model),
        Which::Command => submit_command(model),
        Which::AskQuestion => ask_now(model),
    }
}

/// `Tab` — complete the operator being typed in the search box.
///
/// Only the search box: the finder's grammar is sigils, not `key:value`, and
/// completing an operator into it would type text the finder matches
/// literally.
fn prompt_complete(model: &mut Model) -> Vec<Cmd> {
    if matches!(model.overlay, Some(Overlay::Command(_))) {
        return complete_command(model);
    }
    let completed = match model.overlay.as_ref() {
        Some(Overlay::Search(pane)) if pane.typing() => complete_operator(&pane.query),
        _ => None,
    };
    let Some(completed) = completed else {
        return Vec::new();
    };
    if let Some(Overlay::Search(pane)) = model.overlay.as_mut() {
        pane.query = completed;
    }
    search_now(model)
}

/// `Tab` on the command line: extend the line by as much as the registry can
/// say for certain.
///
/// `command::complete` is the registry's own positional completer — verb
/// segments while a path is being typed, then that verb's flags once it
/// resolves — so this is the same answer `mail` would give and not a second
/// one. It appends the candidates' longest common prefix, plus a space when
/// exactly one candidate remains and it is a leaf: two verbs sharing a
/// prefix must not have one of them silently chosen, which would be a
/// keystroke that did the wrong thing rather than one that did nothing.
fn complete_command(model: &mut Model) -> Vec<Cmd> {
    let Some(Overlay::Command(pane)) = model.overlay.as_ref() else {
        return Vec::new();
    };
    let input = pane.input.clone();
    // A line whose last word is a flag has nothing here to complete: the
    // registry's completer drops flags before it looks at anything, so it
    // would answer about the *verb* and the answer would be substituted over
    // the flag — which is how `:search --x` once became `search search`.
    if input
        .split_whitespace()
        .last()
        .is_some_and(|word| word.starts_with('-'))
    {
        return Vec::new();
    }
    let candidates = command::complete(&input);
    let Some(first) = candidates.first() else {
        return Vec::new();
    };
    // One candidate settles the segment, so it gets a separator either way: a
    // space after a leaf ends the verb, and a space after a group starts its
    // next segment. Several candidates settle only their shared prefix, and
    // adding anything after that would be choosing between them.
    let settled = candidates.len() == 1;
    let common = if settled {
        first.text.clone()
    } else {
        longest_common_prefix(&candidates)
    };
    let typed = trailing_token(&input);
    // `settled` and no longer than what is there is not "nothing to do": it
    // is the segment already typed in full, which still wants its separator.
    // Without that, `<tab>` stalls on `message` rather than opening
    // `message archive`.
    if common.len() <= typed.len() && !settled {
        return Vec::new();
    }
    let head = input
        .get(..input.len() - typed.len())
        .unwrap_or_default()
        .to_owned();
    let tail = if settled { " " } else { "" };
    let completed = format!("{head}{common}{tail}");
    if completed == input {
        return Vec::new();
    }
    set_command_line(model, completed);
    Vec::new()
}

/// Put `line` on the command line, bounded, and recompute what it matches.
///
/// Bounded for the reason [`apply_edit`] bounds a typed one: a history file
/// line is only bounded by the whole file's size, and a pane holding an
/// unbounded string is re-sanitized and re-wrapped on every frame.
fn set_command_line(model: &mut Model, line: String) {
    if let Some(Overlay::Command(pane)) = model.overlay.as_mut() {
        pane.input = truncated(line);
        pane.browse = None;
        pane.error = None;
    }
    refresh_command(model);
}

/// `line`, cut at [`MAX_INPUT`] characters on a character boundary.
fn truncated(mut line: String) -> String {
    if let Some((at, _)) = line.char_indices().nth(MAX_INPUT) {
        line.truncate(at);
    }
    line
}

/// The longest prefix every candidate shares.
fn longest_common_prefix(candidates: &[command::Candidate]) -> String {
    let mut common = candidates
        .first()
        .map(|c| c.text.clone())
        .unwrap_or_default();
    for candidate in candidates.iter().skip(1) {
        let shared = common
            .char_indices()
            .zip(candidate.text.chars())
            .take_while(|((_, a), b)| a == b)
            .count();
        common.truncate(
            common
                .char_indices()
                .nth(shared)
                .map_or(common.len(), |(at, _)| at),
        );
    }
    common
}

/// The whitespace- or dot-delimited token at the end of `input` — what a
/// completion replaces. Empty when the line ends in a separator, which means
/// the completion is appended rather than substituted.
///
/// The range counts as a separator, because `command::complete` strips it
/// before it looks at anything: without that, the `'<,'>` a `:` opened over a
/// selection is read as part of the first word, `message` never looks longer
/// than `'<,'>mess`, and `<tab>` is dead in exactly the state the command
/// line is documented to open in.
fn trailing_token(input: &str) -> &str {
    let after_range = range_prefix(input).len();
    let rest = input.get(after_range..).unwrap_or_default();
    let at = rest.rfind([' ', '.']).map_or(0, |at| {
        at + rest[at..].chars().next().map_or(1, char::len_utf8)
    });
    rest.get(at..).unwrap_or_default()
}

/// The message an action applies to, and its subject.
fn target_subject(model: &Model) -> Option<(i64, String)> {
    match model.screen {
        Screen::Viewer => model.open.as_ref().map(|open| {
            let subject = open
                .headers
                .iter()
                .find(|(name, _)| name == "Subject")
                .map(|(_, value)| value.clone())
                .unwrap_or_default();
            (open.id, subject)
        }),
        Screen::List => model
            .current_message()
            .map(|row| (row.id, row.subject.clone())),
        // The manual is not about a message, and an action that needs one
        // must say "no message selected" rather than reach behind the page.
        Screen::Manual => None,
    }
}

/// The message an action applies to: the one in the viewer when it is open,
/// otherwise the one under the list cursor.
fn target_message(model: &Model) -> Option<i64> {
    match model.screen {
        Screen::Viewer => model.open.as_ref().map(|o| o.id),
        Screen::List => model.current_message().map(|m| m.id),
        Screen::Manual => None,
    }
}
