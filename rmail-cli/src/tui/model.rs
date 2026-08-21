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

use super::commands::{self, Answer, Target};
use super::config_block::ConfigBlock;
use super::form::FormPane;
use super::help::{self, HelpPane};
use super::history::History;
use super::layout::{self, Card};
use super::manual;
use super::overlays;
use super::overlays::{
    command_matches, complete_operator, AiSummary, AskPane, AskPhase, Browse, Citation,
    CommandPane, Explanation, FinderItem, FinderKind, FinderPane, Hit, OutboxPane, OutboxRow,
    QuickAction, QuickPane, ReplyPane, SearchFocus, SearchPane, Toast, UndoToast,
};
use super::report::{self, ReportColumn, ReportFill, ReportPane, ReportRow};
use super::settings::{self, SettingsState};
use super::status::{Daemon, Health, Subsystem};
use super::theme::Theme;
use crate::keymap::file::{self as keys_file, keys_path_from_env};
pub use crate::keymap::Key;
use crate::keymap::{Action, Chord, Keymap, Mode, Pending, Resolution};

/// The IMAP flag marking a message read.
pub const SEEN: &str = "\\Seen";
/// The IMAP flag marking a message flagged/starred.
pub const FLAGGED: &str = "\\Flagged";
/// The IMAP flag marking a message replied-to.
pub const ANSWERED: &str = "\\Answered";

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

/// The most overlays [`Model::overlay_stack`] holds at once — tui.md
/// §2.2.2's example is confirm-over-picker-over-collection, three deep.
/// [`Model::push_overlay`] refuses a fourth outright rather than evicting
/// the oldest: silently closing something the user still has open to make
/// room for something new is a worse surprise than the new thing refusing
/// to open.
///
/// Read only inside [`Model::push_overlay`], which no *existing* call site
/// in this build reaches yet (every current "open an overlay" site goes
/// through [`Model::set_overlay`], which preserves the pre-108 single-slot
/// behavior exactly — see its own docs) — so both this constant and
/// `push_overlay` are `#[allow(dead_code)]` in the non-test binary target,
/// proven live instead by `push_overlay`'s own tests in
/// `tui::overlays::tests`. The declared-shape-a-future-task-consumes
/// pattern task 92 already established for `Toast::Completion`/
/// `Toast::Priority`, not a stub.
#[allow(dead_code)]
pub const MAX_OVERLAY_DEPTH: usize = 3;

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

/// The terminal height a model assumes until a [`Msg::Resize`] tells it
/// otherwise — the classic 80x24, which is also the smallest terminal this
/// screen was ever designed for.
///
/// It matters for exactly one thing, [`page_rows`], and the consequence of
/// being wrong is a page that is a few rows short of the window rather than
/// anything incorrect: the movement is clamped to the list either way. A
/// default rather than an `Option` because "how tall is the terminal" has a
/// sensible answer before the first frame and a `None` would have to be
/// answered with a number here anyway.
const DEFAULT_VIEWPORT_ROWS: u16 = 24;
/// [`DEFAULT_VIEWPORT_ROWS`]'s width half — the classic 80x24 — added in
/// task 109 alongside [`Model::viewport_cols`]; see that field's own docs
/// for why the model did not need to know its width before now.
const DEFAULT_VIEWPORT_COLS: u16 = 80;

/// Rows of a frame that are never the scrolling pane: the status line, and the
/// two border rows of the pane itself.
///
/// Deliberately a floor rather than the real chrome, which varies with what is
/// on screen — a toast, the WhichKey band and the command line each take a row
/// when they are up, and `view` is the only thing that knows. Understating it
/// makes a page slightly *taller* than the visible rows in those layouts,
/// which costs at most the row of overlap [`PAGE_OVERLAP`] adds; overstating
/// it would make every page short in the common case. Keeping the arithmetic
/// here also keeps `view`'s layout out of the model, which is what lets
/// `update` stay a pure function of messages.
const CHROME_ROWS: u16 = 3;

/// Rows a page deliberately does not advance, so the line that was at the
/// bottom of the screen is still on it afterwards.
///
/// One is what `less` and vim's own page keys keep, and the reason is that a
/// page boundary in the middle of a paragraph is unreadable without it.
const PAGE_OVERLAP: usize = 1;

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
    /// Whether the message (or its thread) has a note. Declared shape a
    /// future task populates — same deferral as [`tags`](Self::tags): notes
    /// are their own table (`NoteService`), not part of `ListMessages`.
    pub has_note: bool,
    /// The `To` recipients, as the server reports them (comma-joined
    /// addr-specs, not parsed into a list) — already on the wire response
    /// (`Message.to_addrs`), so carrying it here costs nothing extra.
    pub to: Option<String>,
    /// Applied tag names. Declared shape a future task populates: no RPC in
    /// today's `ListMessages` response carries tag data (`TagService` is
    /// per-message/per-query, not part of a mailbox listing), so
    /// [`wire::message_row`](crate::tui::model::wire::message_row) always
    /// leaves this empty. [`filter`](crate::tui::filter)'s `tag:`/`has:tag`
    /// evaluation is written and tested against this field regardless, so it
    /// starts working the moment something populates it — but
    /// [`filter::Predicate::unloaded_data`](crate::tui::filter::Predicate::unloaded_data)
    /// is what a caller must check to avoid presenting an always-empty
    /// `tag:` result as an authoritative one before that happens.
    ///
    /// Task 123 (List card row anatomy) is the earliest task that plausibly
    /// needs real per-row tag data at all, for chip rendering — but its own
    /// requirement (tui.md:536, "chip text auto-contrasts on wire
    /// `Tag.color`") needs more than a bare name, so whatever populates this
    /// field for real will likely need to widen it (a `Tag`-shaped element
    /// carrying color, not `String`) rather than fill this exact shape as
    /// written. `filter`'s own matching only ever needs the name half of
    /// that, whatever the eventual element type turns out to be — effective
    /// tags too (own or inherited from the thread, applied state only),
    /// matching `has:tag`'s server definition
    /// (`retrieve::filtermask::has_tag_predicate_sql`), not just a message's
    /// own row in `message_tags`.
    pub tags: Vec<String>,
    /// The subset of AI enrichment [`filter`](crate::tui::filter) can match
    /// against. Same deferral as [`tags`](Self::tags), including which task
    /// owns populating it being unpinned — no task in tasks.md's 107–179
    /// range yet names threading triage data onto list rows specifically;
    /// whichever one does should also fold multiple `ai_summaries` passes
    /// (triage, deep) into one [`AiFacts`], since `retrieve::filtermask`
    /// treats each as its own `EXISTS` and a single record here can only
    /// ever answer for whichever pass the populator chose.
    pub ai: Option<AiFacts>,
}

/// The matchable slice of a message's AI triage/deep-pass results — not the
/// full [`AiSummary`](super::overlays::AiSummary) shown in the rail panel,
/// which also carries prose (`tl_dr`, `summary`, `key_points`) that no filter
/// predicate ever compares against.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AiFacts {
    /// Triage priority, e.g. `"high"`.
    pub priority: Option<String>,
    /// Triage category, e.g. `"invoice"`.
    pub category: Option<String>,
    /// Triage sentiment, e.g. `"negative"`.
    pub sentiment: Option<String>,
    /// Whether triage thought a reply was needed.
    pub needs_reply: Option<bool>,
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
    /// The settings screen (task 101). Its state is [`Model::settings`], a
    /// second field for the reason the manual's is.
    Settings,
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
            Screen::List | Screen::Manual | Screen::Settings => Self::List,
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
    /// The `?` key binding reference (task 102).
    Help(Box<HelpPane>),
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
    /// `:reply --ai` — the streamed-reply pane.
    Reply(Box<ReplyPane>),
    /// `O` — the outbox pseudo-folder.
    Outbox(Box<OutboxPane>),
    /// `.` — the AI quick-action menu.
    Quick(QuickPane),
    /// The answer to a `:` verb that reports rows (task 90).
    Report(Box<ReportPane>),
    /// A set of fields a verb is being given, applied as a `:` line (task 96).
    Form(Box<FormPane>),
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
        ///
        /// `None` when the question was asked of a *typed* line rather than of
        /// a report row — task 94's `:index rebuild`, which asks before it
        /// starts. There is no report behind that one to put back.
        over: Option<Box<ReportPane>>,
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
            Self::Form(pane) => (pane.cursor, pane.rows()),
            // Counts only `help::Row::Binding` entries — the group headers
            // interspersed among them are not something a cursor can land
            // on, so they must not be something its length counts either.
            Self::Help(pane) => (pane.cursor, help::binding_count(pane)),
            // The command line is absent on purpose: its `<up>`/`<down>`
            // walk the history rather than a list, which is what `:` means
            // everywhere else it exists. Its ranked matches are a preview
            // with no cursor — `<tab>` is what puts one into the line.
            // The reply pane is absent for the same reason the command line
            // is: nothing in it is a list a cursor walks.
            Self::Pick { .. }
            | Self::Confirm { .. }
            | Self::Input { .. }
            | Self::Command(_)
            | Self::Reply(_) => return None,
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
            Self::Form(pane) => pane.cursor = at,
            Self::Help(pane) => pane.cursor = at,
            Self::Pick { .. }
            | Self::Confirm { .. }
            | Self::Input { .. }
            | Self::Command(_)
            | Self::Reply(_) => {}
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
    /// [`Cmd::WriteKeybinding`] finished.
    ///
    /// Not a [`Msg::Done`]: that variant's [`Effect`] is a mail-side change
    /// the model applies to its own rows, and a keybinding write is
    /// neither — `model.keymap` is never touched here. A running `mail tui`
    /// picks the edit up within a second through the same file-watch reload
    /// path a `mail keys set` run from a second terminal already relies on.
    KeysWritten {
        /// What to say on success.
        label: String,
        /// The write's outcome.
        result: Result<(), String>,
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
    /// One frame of a `ComposeService.DraftReply` stream.
    Reply {
        /// Which reply it belongs to.
        generation: u64,
        /// What arrived.
        event: ReplyEvent,
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
    /// `:rule new` drafted a rule (task 95).
    ///
    /// Separate from the Report the draft is *shown* in, because the two have
    /// different lifetimes: somebody reads the dry run, closes the report,
    /// thinks, and then types `:rule add`. Not a `Msg::Done` either — nothing
    /// finished, and the counter would go negative.
    RuleDrafted(String),
    /// A verb produced a TOML block for the operator to paste (task 97).
    ///
    /// Its own message for the reason [`Msg::RuleDrafted`] is: the block
    /// outlives the report it is shown in, and nothing finished, so a
    /// [`Msg::Done`] would decrement a counter it never incremented.
    Block(Box<ConfigBlock>),
    /// One subsystem's standing, from the heartbeat (task 92).
    ///
    /// Deliberately not a [`Msg::Done`]: nobody asked for it and nothing
    /// counted it into [`Model::inflight`], so reporting it as a finished
    /// request would decrement a counter it never incremented — the same
    /// reason [`Msg::LiveUpdatesStopped`] is its own variant.
    Daemon {
        /// Which subsystem answered.
        subsystem: Subsystem,
        /// What it said, or why it could not be asked.
        result: Result<Health, String>,
    },
    /// One frame of a Report's answer (task 90).
    Report {
        /// Which request it belongs to. A frame from a superseded one is
        /// dropped; see `tui::report`'s module docs.
        generation: u64,
        /// What arrived.
        event: ReportEvent,
    },
    /// What a form's pre-fill read reported (task 96).
    ///
    /// Its own message rather than a [`Msg::Report`] frame: what arrives names
    /// *fields*, not cells, and a form fed a report's rows would have to guess
    /// which column was which flag.
    Form {
        /// Which form it belongs to. An answer to a superseded one is dropped.
        generation: u64,
        /// What arrived.
        event: FormEvent,
    },
    /// The terminal is this size — sent once at startup and again on every
    /// resize.
    ///
    /// A message rather than something read from the terminal where it is
    /// needed, because [`update`] is pure: the window's size is an input
    /// event that arrives on the same channel as a key press (crossterm
    /// delivers it on the same event stream), not a thing the model may go
    /// and ask for. Carries both dimensions since task 109 gave the width
    /// half a reason to exist — see [`Model::viewport_cols`]'s own docs.
    Resize {
        /// The new column count.
        cols: u16,
        /// The new row count.
        rows: u16,
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

/// What a form's pre-fill read delivered.
///
/// No streaming case and no `complete` flag: a form is filled by one unary read
/// and applied once. A verb whose fields arrived in pieces would be a form that
/// could be applied halfway through being told what it was replacing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormEvent {
    /// The values in force, as `(flag, value)` pairs. A flag this build has no
    /// field for is ignored — see [`FormPane::fill`].
    Fields(Vec<(String, String)>),
    /// The read failed. The form stays un-appliable; see [`FormPane::ready`].
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

/// What a `ComposeService.DraftReply` stream delivered.
///
/// The order is fixed by the proto: one context frame, then tokens, then the
/// draft the daemon created from the finished prose, then done. See
/// [`AskEvent`] for why a stream that ends with no done frame is reported as
/// failed rather than treated as a complete answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplyEvent {
    /// What the drafter read before it wrote anything, pre-formatted.
    Context(String),
    /// A chunk of prose.
    Token(String),
    /// The draft the daemon created from the finished prose.
    Drafted {
        /// Its id.
        draft_id: i64,
        /// Its recipient(s), joined.
        to: String,
    },
    /// The terminal frame.
    Done,
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
    /// Write one binding into `keys.toml` — `:keys set`'s (task 102)
    /// blocking half.
    ///
    /// A [`Cmd`] rather than a write inside [`update`] for the reason
    /// [`Cmd::SaveHistory`] is: `update` is pure, synchronous and clockless,
    /// and `crate::keymap::file::write_atomic`'s `fsync` is none of those
    /// things. Unlike history, a failed write here has to reach the person
    /// who asked for it — there is no "the next line corrects it" — so this
    /// one reports back through [`Msg::KeysWritten`] rather than only
    /// logging.
    ///
    /// Not superseding (unlike most streamed work here): two of these in
    /// flight at once run two independent read-edit-write cycles with no
    /// lock between them, so the second's read can land before the first's
    /// `rename`. `mail keys set` already carries the same exposure between
    /// two terminals; this does not add to it, only widens who can trigger
    /// it from inside one.
    WriteKeybinding {
        /// `keys.toml`'s path (env-overridable; see `keys_path_from_env`).
        path: std::path::PathBuf,
        /// Which mode's layer to bind in.
        mode: Mode,
        /// The chord to bind.
        chord: Chord,
        /// What it should run.
        action: Action,
        /// What to say on success; formatted before the write so a failure
        /// can still explain what was attempted.
        label: String,
    },
    /// Start the daemon heartbeat (task 92): poll `SyncService.Status`,
    /// `IndexService.Status`, `AiService.GetUsage` and
    /// `AiPolicyService.GetSpend` on a timer and report each as a
    /// [`Msg::Daemon`].
    ///
    /// "Start", not "do once", because [`update`] has no clock — the same
    /// reason [`Cmd::Countdown`] is shaped this way. Issued once when the
    /// account is known; the executor supersedes an earlier one rather than
    /// running two.
    Heartbeat {
        /// The account whose sync and spend to ask about.
        account_id: i64,
    },
    /// `ClientAuthService.AuthStatus` — the `:auth status` report.
    AuthStatus {
        /// Which report this is, so a frame from a superseded run is
        /// recognisable.
        generation: u64,
    },
    /// `ClientAuthService.ClearPassword` — remove the password gate.
    AuthClear,
    /// `IndexService.Status` — the `:index status` report.
    IndexStatus {
        /// Which report this is.
        generation: u64,
    },
    /// `IndexService.Reindex` — a streamed pass over the queue.
    IndexReindex {
        /// Which report this is.
        generation: u64,
        /// Drain what is queued, or re-enqueue the open folder.
        mode: commands::Reindex,
        /// The folder to re-enqueue, for [`commands::Reindex::Selection`].
        mailbox_id: Option<i64>,
    },
    /// `IndexService.Rebuild` — a streamed rebuild from scratch.
    IndexRebuild {
        /// Which report this is.
        generation: u64,
    },
    /// `IndexService.Verify` — the drift report.
    IndexVerify {
        /// Which report this is.
        generation: u64,
    },
    /// `IndexService.Gc` — reclaim orphaned rows.
    IndexGc {
        /// Which report this is.
        generation: u64,
    },
    /// `IndexService.ListEntities` — the extracted-entity listing.
    IndexEntities {
        /// Which report this is.
        generation: u64,
        /// Which kind to list, as typed. Validated by the daemon, not here.
        kind: String,
    },
    /// `IndexService.SetPaused` — stop or start the background worker.
    IndexSetPaused {
        /// Which way.
        pause: commands::Pause,
    },
    /// `SyncService.Status` as a report of its own, rather than as the folder
    /// listing [`Cmd::LoadFolders`] reads from the same RPC.
    SyncStatusReport {
        /// Which report this is.
        generation: u64,
        /// Whose folders.
        account_id: i64,
    },
    /// `SyncService.SyncFolder` — a pass over every folder now.
    SyncNow {
        /// Which report this is.
        generation: u64,
        /// Whose folders.
        account_id: i64,
    },
    /// `SyncService.Pause`/`Resume`.
    SyncSetPaused {
        /// Whose sync.
        account_id: i64,
        /// Which way.
        pause: commands::Pause,
    },
    /// `AiService.GetUsage` — the `:ai status` and `:ai cost` reports.
    AiUsage {
        /// Which report this is.
        generation: u64,
        /// Whether to render the spend view rather than the loop view.
        costs: bool,
    },
    /// `AiService.SetPaused` — stop or start the dispatch loop.
    AiSetPaused {
        /// Which way.
        pause: commands::Pause,
    },
    /// `AiService.RetryFailed` — move quarantined jobs back to pending.
    AiRetry,
    /// `AiService.AnalyzeMessage` — a streamed analysis of one message.
    AiProcess {
        /// Which report this is.
        generation: u64,
        /// Which message.
        message_id: i64,
    },
    /// `FinderService.IndexStatus`.
    FinderStatus {
        /// Which report this is.
        generation: u64,
    },
    /// `FinderService.RebuildIndex`.
    FinderRebuild,
    /// `TagService.ListTags`.
    TagList {
        /// Which report this is.
        generation: u64,
        /// Whose tags.
        account_id: i64,
    },
    /// `TagService.AddTag`/`RemoveTag` over the selection.
    TagApply {
        /// Which report this is.
        generation: u64,
        /// The messages, as `Target::selection` gave them.
        message_ids: Vec<i64>,
        /// The tag.
        name: String,
        /// Whether to remove it rather than add it.
        remove: bool,
    },
    /// `TagService.CreateTag`.
    TagCreate {
        /// Whose tag.
        account_id: i64,
        /// Its name.
        name: String,
        /// A colour, if one was given.
        color: Option<String>,
        /// Its IMAP sync mode, if one was given.
        sync: Option<commands::tag::Sync>,
    },
    /// `TagService.BulkTag` — everything a query selects, in one transaction.
    TagBulk {
        /// Which report this is.
        generation: u64,
        /// Whose mail.
        account_id: i64,
        /// The filter-only query.
        query: String,
        /// The tag to apply.
        name: String,
    },
    /// `TagService.SuggestTags` — a streamed classification of one message.
    TagSuggest {
        /// Which report this is.
        generation: u64,
        /// Which message.
        message_id: i64,
    },
    /// `TagService.ResolveSuggestion`.
    TagResolve {
        /// Which pending suggestion.
        message_tag_id: i64,
        /// Accept it or discard it.
        resolve: commands::tag::Resolve,
    },
    /// `TagService.ListTagRules`.
    TagRules {
        /// Which report this is.
        generation: u64,
        /// Whose rules.
        account_id: i64,
    },
    /// `TagService.SetTagRule`.
    TagRuleSet {
        /// Whose rule.
        account_id: i64,
        /// The rule's own name.
        name: String,
        /// The tag it applies.
        tag: String,
        /// Whether a confident suggestion applies itself.
        mode: commands::tag::RuleMode,
        /// The confidence it needs, in whole percent — see
        /// `commands::tag::percent` on why not an `f64`.
        min_conf_pct: u32,
        /// Whether the rule is stored enabled.
        enabled: bool,
    },
    /// `RuleService.ListRules`.
    RuleList {
        /// Which report this is.
        generation: u64,
        /// Whose rules.
        account_id: i64,
    },
    /// `RuleService.SynthesizeRule` — draft a rule from words, with a dry run.
    RuleSynthesize {
        /// Which report this is.
        generation: u64,
        /// Whose mail the dry run reads.
        account_id: i64,
        /// What the rule should do, in the caller's own words.
        instruction: String,
        /// How far back the dry run looks, or the daemon's default.
        days: Option<u32>,
    },
    /// `RuleService.CreateRule` — store the drafted TOML.
    RuleCreate {
        /// Whose rule.
        account_id: i64,
        /// The document, as `SynthesizeRule` produced it.
        toml: String,
    },
    /// `RuleService.EvaluateRules` — a dry run over named messages.
    RuleEvaluate {
        /// Which report this is.
        generation: u64,
        /// Whose rules.
        account_id: i64,
        /// The messages to evaluate.
        message_ids: Vec<i64>,
        /// One rule by name, or every enabled one.
        rule: Option<String>,
    },
    /// `RuleService.BacktestRule`.
    RuleBacktest {
        /// Which report this is.
        generation: u64,
        /// Whose mail.
        account_id: i64,
        /// Which rule.
        name: String,
        /// How far back, or the daemon's default.
        days: Option<u32>,
    },
    /// `ExportService.Export`, written to disk by
    /// `rmail_core::export::write::DestinationWriter` at the wire seam.
    Export {
        /// Which report this is.
        generation: u64,
        /// The query selecting what to export. Empty when `thread_id` is set —
        /// the proto's selection is a oneof.
        query: String,
        /// One thread instead of a query.
        thread_id: Option<i64>,
        /// How the archive is framed on disk.
        format: commands::content::analytics::Format,
        /// The directory to write into.
        to: String,
        /// Include what the AI passes produced.
        with_ai: bool,
        /// At most this many messages.
        limit: Option<i64>,
    },
    /// `AnalyticsService.GetResponseTimes`.
    ResponseTimes {
        /// Which report this is.
        generation: u64,
        /// Which account.
        account_id: i64,
        /// One row per contact, or per mailbox.
        group_by: commands::content::analytics::GroupBy,
        /// How far back to look, in seconds. Subtracted from `now` at the wire
        /// seam — see `commands::content`'s module docs on why the model does not
        /// read a clock.
        since_secs: Option<i64>,
        /// Where the window ends, as unix seconds, or `None` for now.
        until: Option<i64>,
        /// At most this many groups.
        limit: Option<i64>,
        /// Ignore a group with fewer samples than this.
        min_samples: Option<i64>,
    },
    /// `AnalyticsService.AskAnalytics`.
    AskAnalytics {
        /// Which report this is.
        generation: u64,
        /// Which account.
        account_id: i64,
        /// The question, in words.
        question: String,
        /// Also have the rows summarized in prose.
        narrate: bool,
    },
    /// `AnalyticsService.GenerateDigest`.
    Digest {
        /// Which report this is.
        generation: u64,
        /// Which account.
        account_id: i64,
        /// How far back, in seconds.
        since_secs: Option<i64>,
        /// Where the window ends.
        until: Option<i64>,
        /// Regenerate rather than answering from the cache.
        force: bool,
    },
    /// `AnalyticsService.GetContactInsight`.
    ContactInsight {
        /// Which report this is.
        generation: u64,
        /// Which account.
        account_id: i64,
        /// Whose.
        address: String,
        /// How far back, in seconds.
        since_secs: Option<i64>,
        /// Where the window ends.
        until: Option<i64>,
        /// Skip the model briefing.
        metrics_only: bool,
    },
    /// `AnalyticsService.ListSubscriptions`.
    Subscriptions {
        /// Which report this is.
        generation: u64,
        /// Which account.
        account_id: i64,
        /// How far back, in seconds.
        since_secs: Option<i64>,
        /// Where the window ends.
        until: Option<i64>,
        /// At most this many senders.
        limit: Option<i64>,
        /// Only the senders worth unsubscribing from.
        candidates_only: bool,
        /// Have a model classify the ones the heuristics could not.
        classify_unknown: bool,
    },
    /// `AttachmentService.ExtractTables`.
    AttachTables {
        /// Which report this is.
        generation: u64,
        /// Which message.
        message_id: i64,
        /// One part, or every one the extractor recognises.
        part: Option<String>,
        /// Let a model read what the parsers cannot.
        allow_model: bool,
    },
    /// `AttachmentService.ExtractInvoice`.
    AttachInvoice {
        /// Which report this is.
        generation: u64,
        /// Which message.
        message_id: i64,
        /// One part, or the best candidate.
        part: Option<String>,
        /// Let a model read what the parsers cannot.
        use_model: bool,
    },
    /// `AttachmentService.ExportInvoices`.
    AttachInvoices {
        /// Which report this is.
        generation: u64,
        /// Which account.
        account_id: i64,
        /// Narrow to one vendor.
        vendor: Option<String>,
        /// How far back, in seconds.
        since_secs: Option<i64>,
        /// Where the window ends.
        until: Option<i64>,
        /// At most this many invoices.
        limit: Option<i64>,
        /// Rows, or a CSV document.
        format: commands::content::analytics::InvoiceFormat,
    },
    /// `AttachmentService.AskAttachment` — streamed.
    AttachAsk {
        /// Which report this is.
        generation: u64,
        /// The question.
        question: String,
        /// One message, or 0 for the whole account.
        message_id: i64,
        /// The account, when the question is account-wide.
        account_id: i64,
        /// One part, or every one.
        part: Option<String>,
        /// How many passages to retrieve.
        top_k: Option<i64>,
    },
    /// `SearchService.SearchAttachments`.
    AttachSearch {
        /// Which report this is.
        generation: u64,
        /// The query.
        query: String,
        /// Which account.
        account_id: i64,
        /// One message, or 0 for the whole account.
        message_id: i64,
        /// At most this many hits.
        limit: Option<i64>,
    },
    /// `ExtractService.ExtractEvents` or `ExtractTasks`.
    ///
    /// One command for two RPCs because they are the same act over two item
    /// kinds: the request fields, the sink, the idempotency claim and the report
    /// shape are identical, and two commands would be two copies of one wire seam.
    Extract {
        /// Which report this is.
        generation: u64,
        /// Which message.
        message_id: i64,
        /// Tasks rather than events.
        tasks: bool,
        /// Let a model read free text, rather than only a real `.ics` part.
        use_model: bool,
        /// Where the items are delivered.
        sink: commands::content::extract::Sink,
    },
    /// `ExtractService.ExtractStructured`.
    ExtractData {
        /// Which report this is.
        generation: u64,
        /// Which message.
        message_id: i64,
        /// Which configured schema.
        schema: String,
        /// Re-extract rather than answering from the cache.
        refresh: bool,
    },
    /// `LinkService.ExtractLinks`.
    Links {
        /// Which report this is.
        generation: u64,
        /// Which message.
        message_id: i64,
        /// Let a model classify what the rules could not.
        use_model: bool,
    },
    /// `SearchService.CompileQuery`.
    CompileQuery {
        /// Which report this is.
        generation: u64,
        /// Which account.
        account_id: i64,
        /// The sentence to compile.
        query: String,
        /// Recompile rather than answering from the cache.
        refresh: bool,
    },
    /// `SearchService.SearchEntities`.
    SearchEntities {
        /// Which report this is.
        generation: u64,
        /// Which account.
        account_id: i64,
        /// The query.
        query: String,
        /// Narrow to these kinds.
        kinds: Vec<String>,
        /// How far back, in seconds.
        since_secs: Option<i64>,
        /// At most this many hits.
        limit: Option<i64>,
    },
    /// `SearchService.Evaluate`, over a golden set read from `path`.
    SearchEval {
        /// Which report this is.
        generation: u64,
        /// The golden-set file. Parsed at the wire seam, on a blocking task —
        /// the one file this client reads, and `commands::content::extract`'s
        /// module docs say why it has to.
        path: String,
        /// Which retrieval arm to score, or the daemon's default.
        mode: Option<commands::content::extract::Mode>,
        /// How many results to score per query.
        limit: Option<i64>,
    },
    /// `NoteService.AddNote`.
    NoteAdd {
        /// The message the note is about.
        message_id: i64,
        /// Whether it is about that message's thread rather than the message.
        thread: bool,
        /// The note, as markdown.
        body: String,
    },
    /// `NoteService.ListNotes`.
    NoteList {
        /// Which report this is.
        generation: u64,
        /// The message.
        message_id: i64,
        /// Whether it is the thread.
        thread: bool,
    },
    /// `NoteService.WatchNotes` — the live listing.
    NoteWatch {
        /// Which report this is.
        generation: u64,
        /// The message.
        message_id: i64,
        /// Whether it is the thread.
        thread: bool,
    },
    /// `NoteService.EditNote`.
    NoteEdit {
        /// Which note.
        note_id: i64,
        /// What it should say now.
        body: String,
    },
    /// `NoteService.DeleteNote`.
    NoteDelete {
        /// Which note.
        note_id: i64,
    },
    /// `SavedSearchService.ListSavedSearches`.
    SavedList {
        /// Which report this is.
        generation: u64,
        /// Which account.
        account_id: i64,
    },
    /// `SavedSearchService.CreateSavedSearch` or `UpdateSavedSearch`.
    SavedSet {
        /// Which account.
        account_id: i64,
        /// The name it is stored under.
        name: String,
        /// The query it stands for.
        query: String,
        /// Update an existing one rather than creating a new one.
        update: bool,
    },
    /// `SavedSearchService.RunSavedSearch` — streamed.
    SavedRun {
        /// Which report this is.
        generation: u64,
        /// Which account.
        account_id: i64,
        /// Which saved search.
        name: String,
        /// At most this many hits.
        limit: Option<i64>,
        /// Ask for each hit's ranking explanation.
        explain: bool,
    },
    /// `SavedSearchService.DeleteSavedSearch`.
    SavedDelete {
        /// Which account.
        account_id: i64,
        /// Which saved search.
        name: String,
    },
    /// `SavedSearchService.ListSmartFolders`.
    FolderList {
        /// Which report this is.
        generation: u64,
        /// Which account.
        account_id: i64,
    },
    /// `SavedSearchService.CreateSmartFolder` or `CompileSmartFolder`.
    FolderCreate {
        /// Which report this is.
        generation: u64,
        /// Which account.
        account_id: i64,
        /// The folder's name.
        name: String,
        /// Its predicate, or — when `compile` is set — the sentence to compile
        /// into one.
        text: String,
        /// Have a model compile a sentence rather than taking a predicate.
        compile: bool,
        /// A tag to apply to whatever enters it.
        auto_tag: Option<String>,
        /// Notify on what enters it.
        notify: bool,
        /// Recompile rather than answering from the cache.
        refresh: bool,
    },
    /// `SavedSearchService.ListSmartFolderMembers` — streamed.
    FolderMembers {
        /// Which report this is.
        generation: u64,
        /// Which account.
        account_id: i64,
        /// Which folder.
        name: String,
        /// At most this many members.
        limit: Option<i64>,
    },
    /// `SavedSearchService.EvaluateSmartFolder`.
    FolderEval {
        /// Which report this is.
        generation: u64,
        /// Which account.
        account_id: i64,
        /// Which folder.
        name: String,
    },
    /// `SavedSearchService.DeleteSmartFolder`.
    FolderDelete {
        /// Which account.
        account_id: i64,
        /// Which folder.
        name: String,
    },
    /// `WebhookService.List` — the `:webhook list` report.
    WebhookList {
        /// Which report this is.
        generation: u64,
        /// Show each destination's full URL rather than its authority. A webhook
        /// URL is frequently the credential itself, so this is off unless asked.
        reveal_url: bool,
    },
    /// `WebhookService.Register`.
    WebhookAdd {
        /// Which report this is.
        generation: u64,
        /// The handle `:forward --to` and `:webhook rm` address it by.
        name: String,
        /// Where deliveries are POSTed.
        url: String,
        /// How the payload is rendered.
        template: commands::automation::Template,
        /// The events it subscribes to, as canonical wire strings. Empty
        /// registers a destination that only ever receives an explicit
        /// `:forward`.
        events: Vec<String>,
        /// Whether it is entitled to message bodies.
        include_body: bool,
        /// Register it disabled.
        disabled: bool,
        /// Where its HMAC signing key is resolved from — a reference, never the
        /// key.
        secret: Option<commands::automation::Secret>,
        /// Attempt cap for its deliveries, or `None` for the daemon's default.
        max_attempts: Option<i64>,
    },
    /// `WebhookService.Remove`.
    WebhookRemove {
        /// Which destination.
        name: String,
    },
    /// `WebhookService.SetEnabled`.
    WebhookEnabled {
        /// Which report this is.
        generation: u64,
        /// Which destination.
        name: String,
        /// Whether to send to it.
        enabled: bool,
    },
    /// `WebhookService.ListDeliveries`.
    WebhookDeliveries {
        /// Which report this is.
        generation: u64,
        /// One destination by name, or every one.
        destination: Option<String>,
        /// How many rows, or the daemon's default.
        limit: Option<i64>,
        /// Include the frozen request body on each row.
        show_payload: bool,
    },
    /// `WebhookService.ReplayDelivery`.
    WebhookReplay {
        /// Which report this is.
        generation: u64,
        /// Which delivery.
        delivery_id: i64,
    },
    /// `WebhookService.Forward`.
    Forward {
        /// Which report this is.
        generation: u64,
        /// Which message.
        message_id: i64,
        /// Which destination, by name.
        destination: String,
    },
    /// `HookService.ListHooks`.
    HookList {
        /// Which report this is.
        generation: u64,
    },
    /// `HookService.TestHook`.
    HookTest {
        /// Which report this is.
        generation: u64,
        /// Which hook.
        name: String,
        /// Event JSON for its stdin, or `None` for a synthetic sample.
        event_json: Option<String>,
    },
    /// `NotificationService.StreamAlerts` — the live `:notify list` report.
    NotifyAlerts {
        /// Which report this is.
        generation: u64,
        /// Replay everything after this alert id first, or `None` for only what
        /// fires from now on.
        since_id: Option<i64>,
    },
    /// `NotificationService.ScoreMessage`.
    NotifyScore {
        /// Which report this is.
        generation: u64,
        /// Which message.
        message_id: i64,
    },
    /// `AccountService.List` — the `:account list` report.
    ///
    /// Its own command rather than a second use of [`Cmd::LoadAccounts`]: that
    /// one is the session's own startup read, and its answer *picks* an account
    /// and starts the folder load. A report asking to see the accounts must not
    /// move the one being looked at.
    AccountList {
        /// Which report this is.
        generation: u64,
        /// The account on screen, or 0 for none — so the listing can mark which
        /// one this session is looking at. Carried rather than read at the wire
        /// seam, because the executor holds no model.
        open: i64,
    },
    /// `AccountService.Get`.
    AccountShow {
        /// Which report this is.
        generation: u64,
        /// Which account.
        account_id: i64,
    },
    /// `AccountService.Autoconfigure` — discover an address's settings and
    /// report a proposal. Writes nothing.
    AccountDiscover {
        /// Which report this is.
        generation: u64,
        /// The address to configure.
        email: String,
        /// How to obtain the password, if one was given — never the password.
        /// Supplied, the discovery is verified by a real IMAP login.
        credential: Option<Credential>,
        /// Let a model propose settings when every probe misses.
        allow_model: bool,
    },
    /// `AccountService.Create`.
    AccountCreate {
        /// The account's name, which for every provider this targets is also
        /// its address.
        name: String,
        /// The settings to store, as `(flag, value)` text pairs — the wire seam
        /// is the one place a port becomes a number.
        settings: Vec<(String, String)>,
    },
    /// `AccountService.TestConnection`.
    AccountTest {
        /// Which report this is.
        generation: u64,
        /// Which account.
        account_id: i64,
    },
    /// `AccountService.Delete`.
    AccountDelete {
        /// Which account.
        account_id: i64,
    },
    /// `AccountService.BeginOAuth` followed by `CompleteOAuth` — the whole
    /// loopback+PKCE flow, reported as it goes.
    ///
    /// One command for two RPCs because they are two halves of one act: the
    /// first returns a URL and binds a port, the second blocks until the
    /// browser comes back, and a client that issued only the first would leave
    /// a port held for a flow nobody could finish. `mail account login` does
    /// exactly the same two calls.
    AccountLogin {
        /// Which report this is.
        generation: u64,
        /// Which account the grant belongs to.
        account_id: i64,
        /// `google`/`gmail` or `microsoft`/`outlook`.
        provider: String,
        /// The OAuth client id of a registered native application. Not a
        /// secret.
        client_id: String,
        /// A command whose stdout is the client secret, for providers that
        /// require one from a native client. The secret never crosses the API.
        client_secret_command: Option<String>,
        /// Scopes to request; empty means the provider's defaults.
        scopes: Vec<String>,
        /// Whether to hand the authorization URL to the platform opener.
        open_browser: bool,
    },
    /// `AccountService.RefreshToken`.
    AccountRefresh {
        /// Which report this is.
        generation: u64,
        /// Which account.
        account_id: i64,
        /// Refresh even if the stored token has not expired yet.
        force: bool,
    },
    /// `AdminService.ListTokens` — metadata only, never a secret.
    TokenList {
        /// Which report this is.
        generation: u64,
    },
    /// `AdminService.MintToken`.
    TokenCreate {
        /// Which report this is.
        generation: u64,
        /// The token's label.
        name: String,
        /// The scopes to grant.
        scopes: Vec<String>,
        /// Seconds from now until it expires, or `None` for no expiry.
        ttl_secs: Option<i64>,
    },
    /// `AdminService.RevokeToken`.
    TokenRevoke {
        /// Which token.
        token_id: i64,
    },
    /// Write text to a private temp file and hand it to the platform opener.
    ///
    /// The copy affordance a report row offers for a block somebody has to
    /// paste elsewhere. There is no clipboard dependency in this workspace and
    /// adding one is a platform matrix; a private file handed to the platform
    /// opener is the mechanism "open HTML in browser" already uses.
    OpenText {
        /// The text to write.
        text: String,
        /// The file extension, which is what decides the handler.
        extension: String,
        /// What to call it on the status line.
        label: String,
    },
    /// `AiPolicyService.GetSpend` — the `:ai budget status` report.
    BudgetStatus {
        /// Which report this is.
        generation: u64,
        /// Which scope: 0 is the global budget.
        account_id: i64,
    },
    /// `AiPolicyService.GetSpend`, read to pre-fill the budget form.
    ///
    /// The same RPC as [`Cmd::BudgetStatus`] and deliberately its own command:
    /// the answer goes somewhere different (a form, not a report), and one command
    /// whose destination depended on a flag would be two behaviours behind one
    /// name.
    BudgetForm {
        /// Which form this is.
        generation: u64,
        /// Which scope.
        account_id: i64,
        /// Which sub-budget the form will write.
        class: commands::ai_policy::Class,
    },
    /// `AiPolicyService.SetBudget` — replaces the scope's whole budget.
    BudgetSet {
        /// Which scope.
        account_id: i64,
        /// Which sub-budget.
        class: commands::ai_policy::Class,
        /// The caps to store, as `(flag, value)` text pairs. A cap absent from
        /// this list is a cap *cleared* — the RPC replaces rather than merges.
        caps: Vec<(String, String)>,
    },
    /// `AiPolicyService.GetAiProvider`.
    ProviderStatus {
        /// Which report this is.
        generation: u64,
        /// Which scope.
        account_id: i64,
    },
    /// `AiPolicyService.SetAiProvider`.
    ProviderSet {
        /// Which scope.
        account_id: i64,
        /// Which backend, or inherit.
        provider: commands::ai_policy::Provider,
    },
    /// `AiSafetyService.ScanInjection`.
    ScanInjection {
        /// Which report this is.
        generation: u64,
        /// Which message.
        message_id: i64,
    },
    /// `AiSafetyService.ConfirmInjection`.
    ConfirmInjection {
        /// Which report this is.
        generation: u64,
        /// Which message.
        message_id: i64,
        /// Release the withheld actions, or withhold them again.
        confirm: commands::ai_policy::Confirm,
    },
    /// `AuditService.QueryAiCalls`, or `ExportLedger` when the whole ledger was
    /// asked for.
    AuditQuery {
        /// Which report this is.
        generation: u64,
        /// Narrow to one account, or 0 for every one.
        account_id: i64,
        /// Narrow to one model.
        model: Option<String>,
        /// Only the calls that failed.
        failed_only: bool,
        /// Walk the whole ledger rather than the most recent page.
        whole_ledger: bool,
    },
    /// `RuleService.RecordCorrection`.
    RuleCorrect {
        /// Whose examples.
        account_id: i64,
        /// The message the correction is about.
        message_id: i64,
        /// The `claude_is` criterion being corrected.
        prompt: String,
        /// What the answer should have been.
        expected: bool,
    },

    // -- reply and drafts (task 100) -----------------------------------
    /// `ComposeService.DraftReply` — a streamed reply, written from an
    /// intent rather than typed by hand.
    DraftReply {
        /// Which reply this is.
        generation: u64,
        /// The message being replied to.
        message_id: i64,
        /// What the reply should say. May be empty.
        intent: String,
        /// Address everyone the parent addressed, not only its author.
        reply_all: bool,
    },
    /// `ComposeService.ListDrafts` — the `:draft list` report.
    DraftList {
        /// Which report this is.
        generation: u64,
        /// Whose drafts.
        account_id: i64,
    },
    /// `ComposeService.GetDraft` — the `:draft show` report.
    DraftShow {
        /// Which report this is.
        generation: u64,
        /// Which draft.
        draft_id: i64,
    },
    /// `ComposeService.UpdateDraft` — replace a draft's body.
    DraftEdit {
        /// Which report this is.
        generation: u64,
        /// Which draft.
        draft_id: i64,
        /// Its new body.
        body: String,
    },
    /// `ComposeService.DeleteDraft`.
    DraftDelete {
        /// Which draft.
        draft_id: i64,
    },
    /// `ComposeService.RenderDraft` — what a draft renders to, unsent.
    DraftRender {
        /// Which report this is.
        generation: u64,
        /// Which draft.
        draft_id: i64,
    },
    /// `ComposeService.RewriteDraft`.
    DraftRewrite {
        /// Which report this is.
        generation: u64,
        /// Which draft.
        draft_id: i64,
        /// The register to aim for, as typed. Validated by the daemon, not
        /// here.
        tone: Option<String>,
        /// Aim shorter.
        shorter: bool,
        /// Aim longer.
        longer: bool,
        /// What to change, in the caller's own words.
        instruction: String,
    },
    /// `ComposeService.ListDraftRevisions`.
    DraftRevisions {
        /// Which report this is.
        generation: u64,
        /// Which draft.
        draft_id: i64,
    },
    /// `ComposeService.SelectDraftRevision` — restore an earlier revision.
    DraftRevert {
        /// Which report this is.
        generation: u64,
        /// Which draft.
        draft_id: i64,
        /// Which revision, 0 for the original text.
        seq: i64,
    },

    // -- send and the outbox (task 100) ---------------------------------
    /// `SendSchedulerService.ScheduleSend`. The same "mutate, then re-list,
    /// then one [`Msg::Outbox`]" template [`Cmd::CancelSend`] uses, so
    /// scheduling a send (re-)arms the undo toast exactly as cancelling one
    /// does — the moment right after this succeeds is the moment "undo"
    /// matters most.
    ScheduleSend {
        /// Whose draft.
        account_id: i64,
        /// The draft to send.
        draft_id: i64,
        /// A time expression, resolved daemon-side. Empty sends now (subject
        /// to the account's undo window).
        at: String,
        /// An undo-window override, seconds. `None` uses the account default.
        undo: Option<i64>,
    },
    /// `SendSchedulerService.RetryFailed`. Same template as
    /// [`Cmd::ScheduleSend`].
    RetryFailed {
        /// Which entry.
        outbox_id: i64,
    },
    /// `SendSchedulerService.RescheduleSend`. Same template as
    /// [`Cmd::ScheduleSend`].
    RescheduleSend {
        /// Which entry.
        outbox_id: i64,
        /// A time expression, resolved daemon-side.
        at: String,
    },
    /// `SendSchedulerService.UpdateScheduledBody`. Same template as
    /// [`Cmd::ScheduleSend`].
    UpdateScheduledBody {
        /// Which entry.
        outbox_id: i64,
        /// Its new body.
        body: String,
    },
    /// `SendSchedulerService.SendNow`. Same template as [`Cmd::ScheduleSend`].
    SendNow {
        /// Which entry.
        outbox_id: i64,
    },
    /// `SendSchedulerService.SuggestSendTime` — the `:outbox suggest` report.
    SuggestSendTime {
        /// Which report this is.
        generation: u64,
        /// Whose outbox.
        account_id: i64,
    },

    // -- follow-ups and the pre-send guardian (task 100) -----------------
    /// `SendSchedulerService.ListFollowups` — the `:followup list` report.
    FollowupList {
        /// Which report this is.
        generation: u64,
        /// Whose follow-ups.
        account_id: i64,
    },
    /// `SendSchedulerService.CreateFollowup`. Resolves `message_id`'s RFC
    /// 5322 Message-ID via `MailService.Get` first — the request wants the
    /// header the server knows the message by, and a row id is all a `:`
    /// line can name.
    FollowupNew {
        /// The local message to follow up.
        message_id: i64,
        /// A time expression, or empty for `send.followup.default_delay`.
        remind_in: String,
        /// A note to carry on the reminder.
        note: String,
    },
    /// `SendSchedulerService.DismissFollowup`.
    FollowupDismiss {
        /// Which follow-up.
        id: i64,
    },
    /// `SendSchedulerService.ListWaitingOn` — the `:waiting` report.
    Waiting {
        /// Which report this is.
        generation: u64,
        /// Whose follow-ups.
        account_id: i64,
        /// Only entries already past their `remind_at`.
        overdue: bool,
    },
    /// `SendSchedulerService.DraftNudge` — a drafted chase message. Sends
    /// nothing; the daemon only writes the words.
    DraftNudge {
        /// Which report this is.
        generation: u64,
        /// The waiting-on entry to chase.
        id: i64,
    },
    /// `SendSchedulerService.PreflightCheck` — the `:preflight` report.
    PreflightCheck {
        /// Which report this is.
        generation: u64,
        /// Whose draft — `PreflightCheckRequest.account_id` is required even
        /// when `draft_id` is set.
        account_id: i64,
        /// The draft to check.
        draft_id: i64,
    },

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
    /// `ComposeService.DraftReply`.
    Reply,
    /// Whatever is feeding the Report overlay (task 90).
    ///
    /// One slot for every reporting verb rather than one per verb: only one
    /// report is on screen at a time, so a second one starting is always a
    /// supersession of the first — and a unary report (`:auth status`) shares
    /// the slot for the same reason `Explain` has one, so `Esc` has exactly one
    /// thing to cancel whichever kind is running.
    Report,
}

/// How to obtain an account's password — never the password itself.
///
/// The proto's `CredentialRef` oneof, as the command grammar spells it. A
/// separate type rather than three `Option<String>`s on the command, so
/// "exactly one source" is what the type says and not something every reader
/// has to check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credential {
    /// A shell command whose stdout is the password.
    Command(String),
    /// The name of an environment variable holding the password.
    Env(String),
    /// A macOS Keychain generic-password service name.
    Keychain(String),
    /// The Keychain service holding an OAuth2 grant. The account authenticates
    /// with XOAUTH2 rather than a password.
    OAuth(String),
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
    /// One at a time on screen, and since task 97 the one on screen can change:
    /// `:account use <id>` picks another of [`Model::accounts`] without
    /// restarting.
    pub account: Option<Account>,
    /// Every account the daemon listed.
    ///
    /// Kept rather than consumed picking [`Model::account`], because
    /// `:account use` needs to know an id is real before it clears the screen —
    /// and because refusing an id the daemon has never mentioned is a better
    /// answer than a `NOT_FOUND` two round trips and one blank folder pane
    /// later. `:account list` re-reads it; nothing else writes it.
    pub accounts: Vec<Account>,
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
    /// The settings screen's state, when it is the screen. `Some` exactly when
    /// [`Model::screen`] is [`Screen::Settings`], held to it the same way and by
    /// the same function.
    pub settings: Option<SettingsState>,
    /// Which section the settings screen was last on, so reopening it goes back
    /// there rather than to the first one.
    ///
    /// Outside [`Model::settings`] because it outlives the screen — which is the
    /// same reason `rule_draft` and `block` are session state.
    last_settings_section: Option<settings::Section>,
    /// The modal layer(s), innermost (topmost, the one receiving key input)
    /// last — a stack rather than a single slot (tui.md §2.2.2), so a
    /// confirm can be asked *over* a picker without discarding it. Capped at
    /// [`MAX_OVERLAY_DEPTH`].
    ///
    /// Private rather than `pub`, unlike most of this struct: every other
    /// field is data a view or a test reasonably wants to set up directly,
    /// but this one has an invariant (the depth cap) that a bare `Vec`
    /// cannot enforce on its own — a `model.overlay_stack.push(..)` reaching
    /// in from outside this module would walk straight past
    /// [`Model::push_overlay`]'s refusal. Read via [`Model::overlays`]/
    /// [`Model::overlay_top`]/[`Model::overlay_top_mut`]; write via
    /// [`Model::push_overlay`]/[`Model::pop_overlay`]/[`Model::set_overlay`]/
    /// [`Model::restore_overlay`] — every one of those lives in this module
    /// specifically so the cap has exactly one gate to reason about.
    overlay_stack: Vec<Overlay>,
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
    /// The TOML block this session last produced, which `:toml` opens.
    ///
    /// Session state rather than a field on the Report it was shown in, for the
    /// reason [`Model::rule_draft`] is: it outlives the report — somebody reads
    /// the proposal, closes it, thinks, then wants the block — and a generic
    /// overlay growing one field per verb is how it stops being generic.
    ///
    /// One slot for every producer (`:account add`, `:hook add`, `:notify set`)
    /// rather than one each: they all produce the same thing, and the newest is
    /// the one anybody means by "the block".
    pub block: Option<Box<ConfigBlock>>,
    /// The TOML `:rule new` last drafted, which `:rule add` stores.
    ///
    /// Session state rather than a field on the Report it was shown in: a draft
    /// outlives the report (somebody reads the dry run, closes it, thinks, then
    /// adds it), and a `ReportPane` holding domain-specific payload would be the
    /// generic overlay growing one field per verb.
    pub rule_draft: Option<String>,
    /// What the heartbeat has learned about the daemon's subsystems.
    ///
    /// Never counted into [`Model::inflight`]: that counter is what the busy
    /// marker reads, and it means "work the *user* asked for". A five-second
    /// poll incrementing it would pin the marker on forever — see
    /// `tui::status`' module docs.
    pub daemon: Daemon,
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
    /// How many rows the terminal has, as of the last [`Msg::Resize`].
    ///
    /// The one fact about the *window* this model keeps, and it is here
    /// because `cursor.page-down` cannot be answered without it: a page is a
    /// property of the terminal, not of the mailbox. Everything else about
    /// geometry stays in `view`, which is handed a `Rect` per frame — see
    /// [`page_rows`] for what this is and is not used for.
    pub viewport_rows: u16,
    /// The terminal's width, as of the last [`Msg::Resize`] — [`viewport_rows`](Model::viewport_rows)'s
    /// sibling, added in task 109 once [`layout::breakpoint`] gave the model
    /// its first real reason to know it: `\`/`C-b`'s "flip a preference at
    /// affording widths, focus-summon a drawer at narrower ones" behavior
    /// (§4.4) reads the current breakpoint, and that needs the real column
    /// count, not a guess. `viewport_rows`'s own former doc note ("width is
    /// deliberately absent") is why this field did not exist until now — the
    /// model genuinely had nothing to do with it before this task.
    pub viewport_cols: u16,
    /// The card zoomed full-bleed (`Z`), if any — sticky across resizes and
    /// focus changes (tui.md §4.5); cleared only by `Z` again or the Esc
    /// ladder (task 115).
    pub zoom: Option<Card>,
    /// Which of the four cards has keyboard focus. Defaults to
    /// [`Card::List`] and, until task 132 wires `h`/`l`/`Tab`, is changed
    /// only by [`Card::Sidebar`]/[`Card::Rail`]'s own focus-summon toggles
    /// (`C-b`/`\` at breakpoints too narrow to show them as part of the
    /// normal split — §4.4).
    pub card_focus: Card,
    /// Whether the sidebar is shown by default at breakpoints that can
    /// afford it (M/L/XL) — `C-b`'s toggle target at those widths. A plain
    /// session field for now; task 114's `tui.toml` is what will persist it
    /// across sessions and let a user's own default override this one.
    pub sidebar_visible: bool,
    /// The rail's equivalent of [`sidebar_visible`](Model::sidebar_visible)
    /// — `\`'s toggle target at the widths that consult it (M/L/XL; see
    /// [`layout::layout_mode`]'s own per-breakpoint docs for exactly which).
    /// Defaults `false` rather than [`layout::default_rail_visible`]'s
    /// width-dependent answer: computing that default properly (and keeping
    /// it in step as the terminal resizes) is task 114's job once there is
    /// somewhere to persist an override against it, not this one's.
    pub rail_visible: bool,
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
            accounts: Vec::new(),
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
            settings: None,
            last_settings_section: None,
            overlay_stack: Vec::new(),
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
            daemon: Daemon::default(),
            block: None,
            rule_draft: None,
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
            viewport_rows: DEFAULT_VIEWPORT_ROWS,
            viewport_cols: DEFAULT_VIEWPORT_COLS,
            zoom: None,
            card_focus: Card::List,
            sidebar_visible: true,
            rail_visible: false,
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
        match self.overlay_top() {
            Some(Overlay::Pick { .. }) => Mode::Pick,
            Some(Overlay::Confirm { .. }) => Mode::Confirm,
            Some(Overlay::Input { .. }) => Mode::Insert,
            // The overlays that change mode part-way through: search starts
            // on the query line and moves to its results, ask starts on the
            // question and moves to the answer, help starts browsing and
            // moves to its filter line on `/`. Deriving the mode from that
            // state rather than storing it is what stops a pane from being
            // in one mode while it draws the other.
            Some(Overlay::Search(pane)) if pane.typing() => Mode::Prompt,
            Some(Overlay::Ask(pane)) if pane.typing() => Mode::Prompt,
            Some(Overlay::Help(pane)) if pane.editing => Mode::Prompt,
            Some(Overlay::Help(_)) => Mode::Help,
            Some(Overlay::Finder(_) | Overlay::Command(_)) => Mode::Prompt,
            // A form is two modes for the same reason the search pane is: the
            // keyboard is text while a field is open and commands while it is
            // not, and deriving that from the pane rather than storing it is what
            // stops the two from disagreeing.
            Some(Overlay::Form(pane)) if pane.editing.is_some() => Mode::Insert,
            Some(
                Overlay::Search(_)
                | Overlay::Ask(_)
                | Overlay::Reply(_)
                | Overlay::Outbox(_)
                | Overlay::Quick(_)
                | Overlay::Report(_)
                | Overlay::Form(_),
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
                // Before the visual selection for the reason the manual is:
                // the settings screen can be opened mid-selection, and while it
                // is up the keyboard belongs to it.
                Screen::Settings => Mode::Settings,
                Screen::List if self.is_selecting() => Mode::Visual,
                Screen::List => Mode::Normal,
                Screen::Viewer => Mode::Viewer,
            },
        }
    }

    /// Where the four cards go right now — tui.md §2.2's single source of
    /// truth (task 107's [`layout::layout_mode`]), fed from the facts this
    /// module holds about them. Computed fresh on every call rather than
    /// cached: `layout_mode` is pure and cheap, and caching it here would be
    /// one more place model and view state could quietly disagree.
    ///
    /// `reader_open` reads [`Model::open`] — whether a message is currently
    /// open is exactly what tui.md means by it (§4.2's S-breakpoint row),
    /// and it is state this model already has, not something task 109 needed
    /// to invent. `height_tier` reads [`Model::viewport_rows`], the same
    /// field [`page_rows`] already trusts for the window's real size.
    ///
    /// `#[allow(dead_code)]`: `view.rs` still renders the v1 three-pane
    /// frame — wiring this into a real draw is task 120's outer grid. Proven
    /// live by this module's own tests instead, the same "declared shape a
    /// named future task consumes" pattern task 107's `layout_mode` itself
    /// carries until the same task wires it in.
    #[allow(dead_code)]
    #[must_use]
    pub fn deck_plan(&self) -> layout::DeckPlan {
        layout::layout_mode(
            layout::Rect::new(0, 0, self.viewport_cols, self.viewport_rows),
            layout::DeckContext {
                focus: self.card_focus,
                zoom: self.zoom,
                sidebar_visible: self.sidebar_visible,
                rail_visible: self.rail_visible,
                reader_open: self.open.is_some(),
                height_tier: layout::height_tier(self.viewport_rows),
            },
        )
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

    /// Push a new overlay on top of the stack — the only way one should be
    /// opened outside this module (§2.2.2). Refused, with a status-line
    /// explanation rather than a silent no-op or an eviction, once
    /// [`MAX_OVERLAY_DEPTH`] is reached; the caller that asked for the new
    /// overlay simply does not get it, exactly like [`bulk_targets`]'s
    /// [`MAX_BULK`] refusal refuses rather than truncates.
    ///
    /// `#[allow(dead_code)]` for the same reason [`MAX_OVERLAY_DEPTH`]
    /// carries it — see that constant's docs.
    ///
    /// # A known gap for whoever writes the first real call site
    ///
    /// `dispatch`'s `Msg::{Search,Finder,Ask,Reply,Report,Form,Explained,
    /// Outbox}` arms all route their streamed events through
    /// `overlay_top_mut()` — the *topmost* matching overlay only. Push a
    /// second overlay over one that owns a live stream and every event for
    /// the one underneath is silently dropped on the floor until it is
    /// popped back to the top. Nothing reachable today does this (every
    /// pre-108 call site still opens overlays via `set_overlay`, which never
    /// leaves two stacked), so it has not needed fixing yet — but the first
    /// call site that genuinely pushes over something with a stream open
    /// will need those arms changed to search the whole stack
    /// (`overlay_stack.iter_mut().rev().find_map(..)`) for the matching
    /// variant, not just look at the top.
    #[allow(dead_code)]
    pub fn push_overlay(&mut self, overlay: Overlay) {
        if self.overlay_stack.len() >= MAX_OVERLAY_DEPTH {
            self.fail(format!(
                "{MAX_OVERLAY_DEPTH} overlays already open — close one first"
            ));
            return;
        }
        self.overlay_stack.push(overlay);
    }

    /// Pop and return the topmost overlay, if any — the stack's half of
    /// [`Model::push_overlay`], and the Esc ladder's step 2 (task 115).
    pub fn pop_overlay(&mut self) -> Option<Overlay> {
        self.overlay_stack.pop()
    }

    /// Close every open overlay, bottom to top, and return what was cleared
    /// in that order — for the handful of call sites that are about to show
    /// something (a different screen, a different mode's action) which *no*
    /// overlay may be left covering, not "close the one thing that is
    /// currently on top." `pop_overlay` is the wrong primitive for those:
    /// it only reaches the topmost layer, and every one of these call sites
    /// predates this task's stack entirely — their own doc comments (e.g.
    /// [`open_manual_at`]'s "the manual is a screen, so an overlay left up
    /// would cover the thing the caller just asked to show") state the
    /// invariant as "no overlay," not "one fewer overlay."
    ///
    /// Returns the cleared overlays so a caller that needs to cancel their
    /// streams can do so for all of them — `over.iter().flat_map(cancels)`
    /// — rather than only the one `pop_overlay` would have reached.
    pub(crate) fn clear_overlays(&mut self) -> Vec<Overlay> {
        std::mem::take(&mut self.overlay_stack)
    }

    /// Push `overlay` back exactly where [`Model::pop_overlay`] took it
    /// from — for the "pop, inspect, and put back if it wasn't a match"
    /// idiom several call sites use (e.g. answering a `Pick`/`Confirm`/
    /// `Input` overlay when the popped value turns out to be a different
    /// variant). A no-op on `None` so callers can pass a `pop_overlay()`
    /// result straight through without an extra `if let`.
    pub(crate) fn restore_overlay(&mut self, overlay: Option<Overlay>) {
        if let Some(overlay) = overlay {
            // Deliberately *not* `push_overlay`: this is undoing a pop that
            // already happened, not opening something new, so it must
            // never trip the depth refusal — the stack was never over
            // capacity a moment ago and restoring what was just removed
            // from it cannot make it so. Asserted, not just claimed: every
            // real caller passes through a value `pop_overlay` (or a
            // decrement of one) just handed back, so this can only fire if
            // a future caller starts using `restore_overlay` to push
            // something that was never popped from a stack this size.
            debug_assert!(
                self.overlay_stack.len() < MAX_OVERLAY_DEPTH,
                "restore_overlay should only ever put back what pop_overlay just removed"
            );
            self.overlay_stack.push(overlay);
        }
    }

    /// Replace the topmost overlay with `overlay`, or open it fresh if
    /// nothing is open — every call site this migrates from the old
    /// `Model.overlay: Option<Overlay>` field's `= Some(x)` assignment,
    /// which always unconditionally replaced whatever was there. This is
    /// the compatibility shim that makes that translation exact rather than
    /// a behavior change: none of those call sites decided to *stack* a new
    /// overlay over an existing one (the type could not express that), so
    /// none of them should start doing so just because the storage
    /// underneath became a `Vec`.
    ///
    /// A call site that *does* want real stacking (opening a genuinely new
    /// layer over what is already showing — tui.md §2.2.2's confirm-over-
    /// picker) should call [`Model::push_overlay`] directly instead; this
    /// method exists for the sites that were always "the current one",
    /// never "one more".
    pub(crate) fn set_overlay(&mut self, overlay: Overlay) {
        match self.overlay_stack.last_mut() {
            Some(top) => *top = overlay,
            None => self.overlay_stack.push(overlay),
        }
    }

    /// Every open overlay, outermost (bottom) first — [`view::render`]'s own
    /// read of the stack for back-to-front drawing (§2.2.2), and the general
    /// escape hatch for anything else that needs to look at more than the
    /// top without gaining write access.
    #[must_use]
    pub fn overlays(&self) -> &[Overlay] {
        &self.overlay_stack
    }

    /// The topmost overlay — the only one that receives key input. Lower
    /// ones (if any) are visually present but inert; nothing in this module
    /// dispatches a keypress to them.
    #[must_use]
    pub fn overlay_top(&self) -> Option<&Overlay> {
        self.overlay_stack.last()
    }

    /// Mutable access to the topmost overlay — the write half of
    /// [`Model::overlay_top`].
    pub fn overlay_top_mut(&mut self) -> Option<&mut Overlay> {
        self.overlay_stack.last_mut()
    }

    /// Whether any overlay is open at all.
    #[must_use]
    pub fn overlay_is_open(&self) -> bool {
        !self.overlay_stack.is_empty()
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
    if screen != Screen::Settings {
        model.settings = None;
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
                    // Stored before one is picked, and stored even when picking
                    // fails: `:account list` and `:account use` both read this,
                    // and a session that could not choose an account is exactly
                    // the session that needs to be able to list them.
                    model.accounts = accounts;
                    let chosen = match model.preferred_account {
                        // An explicit `--account` that does not exist is a
                        // typo worth reporting, not something to silently
                        // substitute the first account for.
                        Some(wanted) => model
                            .accounts
                            .iter()
                            .find(|a| a.id == wanted)
                            .cloned()
                            .ok_or(format!(
                                "no account {wanted} — list them with `mail accounts`"
                            )),
                        None => model
                            .accounts
                            .first()
                            .cloned()
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
                    // The heartbeat starts here for the same reason the event
                    // stream does, and is counted for neither of the same
                    // reasons: nobody asked for it and it never finishes.
                    vec![
                        Cmd::LoadFolders { account_id },
                        Cmd::Watch { account_id },
                        Cmd::LoadOutbox { account_id },
                        Cmd::Heartbeat { account_id },
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
        Msg::KeysWritten { label, result } => {
            model.inflight = model.inflight.saturating_sub(1);
            match result {
                Ok(()) => model.info(format!(
                    "{label} — picked up within a second, nothing to restart"
                )),
                Err(error) => model.fail(format!("{label}: {error}")),
            }
            Vec::new()
        }
        Msg::LiveUpdatesStopped(why) => {
            model.fail(format!(
                "live updates stopped ({why}) — the list is no longer refreshing itself"
            ));
            Vec::new()
        }
        Msg::Resize { cols, rows } => {
            // Silent: a resize is something the user did to their own window
            // and can see the result of, and a status line about it would push
            // off whatever the last thing that happened was.
            model.viewport_cols = cols;
            model.viewport_rows = rows;
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
                    // The key reference's own rows are cached (`help.rs`'s
                    // module docs explain why), and a cache the reload path
                    // forgot to invalidate is exactly the staleness this
                    // whole feature exists to rule out — the point of
                    // generating the list from the live keymap is lost if
                    // "live" stops being true the moment the overlay that
                    // shows it is actually open.
                    reload_help(model);
                    // The lint task 91 built the check for, run on every load
                    // including the first — which is what makes it a *startup*
                    // lint without a startup path of its own. A binding that
                    // shadows a longer one in a farther layer is legal, cannot
                    // be refused at load time (the two are in different modes,
                    // so neither `bind` sees the other), and leaves a chord
                    // nobody can ever type. Saying so beats letting somebody
                    // conclude their `keys.toml` is broken.
                    //
                    // The status line rather than stdout: this process's stdout
                    // *is* the alternate screen, so a `println!` would be
                    // written into cells ratatui does not know it wrote and
                    // would sit there for the session. `:keys check` is the
                    // detail the line points at.
                    let shadowed = model.keymap.shadowed_across_layers();
                    if shadowed.is_empty() {
                        if announce {
                            model.info("key bindings reloaded");
                        }
                    } else {
                        model.fail(format!(
                            "{} binding(s) can never be typed — :keys check lists them",
                            shadowed.len()
                        ));
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
            if let Some(Overlay::Search(pane)) = model.overlay_top_mut() {
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
            if let Some(Overlay::Finder(pane)) = model.overlay_top_mut() {
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
            if let Some(Overlay::Ask(pane)) = model.overlay_top_mut() {
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
        Msg::Block(block) => {
            // Replaces whatever was produced before, for the reason a second
            // rule draft does: `:toml` opens "the block", and two of them would
            // make which one it meant depend on the order two reports happened
            // to answer in.
            model.block = Some(block);
            Vec::new()
        }
        Msg::RuleDrafted(toml) => {
            // Replaces whatever was drafted before: `:rule add` stores "the
            // draft", and two of them would make which one it meant depend on
            // the order two reports happened to answer in.
            model.rule_draft = Some(toml);
            Vec::new()
        }
        Msg::Reply { generation, event } => {
            let mut note = None;
            if let Some(Overlay::Reply(pane)) = model.overlay_top_mut() {
                if generation == pane.generation {
                    match event {
                        ReplyEvent::Context(context) => pane.context = Some(context),
                        ReplyEvent::Token(token) => pane.push_token(generation, &token),
                        ReplyEvent::Drafted { draft_id, to } => {
                            pane.drafted = Some((draft_id, to));
                        }
                        ReplyEvent::Done => {
                            pane.done = true;
                            note = Some(Ok(match &pane.drafted {
                                Some((id, to)) => format!(
                                    "draft {id} created for {to} — `mail draft rewrite {id}` to adjust, :send --draft={id} to schedule"
                                ),
                                None => "drafted, but the daemon named no draft — check `:draft list`".to_owned(),
                            }));
                        }
                        ReplyEvent::Failed(error) => {
                            pane.done = true;
                            pane.error = Some(error.clone());
                            note = Some(Err(format!("reply: {error}")));
                        }
                    }
                }
            }
            apply_note(model, note);
            Vec::new()
        }
        Msg::Daemon { subsystem, result } => {
            // No `inflight` arithmetic in either direction. A heartbeat that
            // decremented on arrival would drive the counter below zero on the
            // first tick, which `saturating_sub` would hide rather than fix.
            model.daemon.set(
                subsystem,
                match result {
                    Ok(health) => health,
                    // Recorded on the indicator rather than shouted on the
                    // status line: nobody asked, and a daemon that went away
                    // must not overwrite the answer to whatever the user *did*
                    // ask, once every five seconds, forever.
                    Err(error) => Health::failed(error),
                },
            );
            Vec::new()
        }
        Msg::Report { generation, event } => {
            let mut note = None;
            if let Some(Overlay::Report(pane)) = model.overlay_top_mut() {
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
        Msg::Form { generation, event } => {
            let mut note = None;
            if let Some(Overlay::Form(pane)) = model.overlay_top_mut() {
                match event {
                    FormEvent::Fields(values) => {
                        if pane.fill(generation, &values) {
                            note = Some(Ok(format!(
                                "{} — <enter> edits a field · apply to store · Esc closes",
                                pane.invocation.verb.join(" ")
                            )));
                        }
                    }
                    FormEvent::Failed(error) => {
                        if pane.fail(generation, error.clone()) {
                            note =
                                Some(Err(format!("{}: {error}", pane.invocation.verb.join(" "))));
                        }
                    }
                }
            }
            apply_note(model, note);
            Vec::new()
        }
        Msg::Explained { message_id, result } => {
            let mut note = None;
            if let Some(Overlay::Search(pane)) = model.overlay_top_mut() {
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
                    if let Some(Overlay::Outbox(pane)) = model.overlay_top_mut() {
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
            if let Some(Overlay::Outbox(pane)) = model.overlay_top_mut() {
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
/// above are inside `model.overlay_top_mut()`; deciding the text there and
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
    let Some(Overlay::Search(pane)) = model.overlay_top_mut() else {
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
    if model.overlay_is_open() {
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
    /// The key reference's filter (task 102).
    HelpFilter,
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
    let typed = match model.overlay_top_mut() {
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
        Some(Overlay::Help(pane)) if pane.editing => {
            once(apply_edit(&mut pane.filter, edit), Typed::HelpFilter)
        }
        // The pane's own methods rather than `apply_edit`: a field is bounded by
        // `form::MAX_VALUE` rather than `MAX_INPUT`, and an edit to one clears
        // the complaint the last apply left — which is state `apply_edit` cannot
        // see from behind a `&mut String`.
        Some(Overlay::Form(pane)) if pane.editing.is_some() => {
            match edit {
                TextEdit::Push(c) => pane.push(c),
                TextEdit::Backspace => pane.backspace(),
            }
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
        Typed::HelpFilter => {
            refresh_help(model);
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
        Action::CursorPageDown => page(model, Direction::Down, count),
        Action::CursorPageUp => page(model, Direction::Up, count),
        // `<tab>` means "the next thing over", and what that is depends on the
        // screen: the other pane on the list, the next section of the settings
        // screen. One action dispatched on the surface rather than two, for the
        // reason `cursor.down` is one action driving four cursors — and because
        // `settings.section` as an id would auto-derive a `:settings section`
        // verb that shadowed `:settings <section>`, which
        // `command::tests::no_real_verb_that_takes_positionals_is_shadowed_by_a_longer_one`
        // refuses and rightly.
        Action::FocusToggle if model.screen == Screen::Settings => next_settings_section(model),
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
        Action::Help => open_help(model),
        Action::HelpRebind => open_help_rebind(model),
        Action::SettingsOpen => open_settings(model, None),
        Action::KeysCheck => run_verb(model, "keys check"),
        // Task 105's leader map. Each runs the verb its own id names, through
        // `run_verb` — so a key and the typed line are one code path, and the
        // key cannot do anything the line could not.
        Action::TagList => run_verb(model, "tag list"),
        Action::TagSuggest => run_verb(model, "tag suggest"),
        Action::RuleList => run_verb(model, "rule list"),
        Action::RuleRun => run_verb(model, "rule run"),
        Action::SyncStatus => run_verb(model, "sync status"),
        Action::IndexStatus => run_verb(model, "index status"),
        Action::AiStatus => run_verb(model, "ai status"),
        Action::AttachList => run_verb(model, "attach list"),
        Action::LinksList => run_verb(model, "links"),
        Action::NoteList => run_verb(model, "note list"),
        Action::NoteWatch => run_verb(model, "note watch"),
        Action::WebhookList => run_verb(model, "webhook list"),
        Action::HookList => run_verb(model, "hook list"),
        Action::SavedList => run_verb(model, "saved list"),
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
        Action::Zoom => toggle_zoom(model),
        Action::SidebarToggle => toggle_sidebar(model),
        Action::RailToggle => toggle_rail(model),
        Action::AiQuick => open_quick(model),
        Action::OutboxOpen => open_outbox(model),
        Action::OutboxCancel => undo_send(model),
        Action::ReportRerun => rerun_report(model),
        Action::ReportReject => reject_report_row(model),
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
    /// The settings screen's highlighted field.
    Settings,
}

fn active_cursor(model: &Model) -> Option<Cursor> {
    match model.overlay_top() {
        Some(Overlay::Pick { .. }) => Some(Cursor::Pick),
        Some(overlay) if overlay.list_cursor().is_some() => Some(Cursor::Overlay),
        // A confirm or a prompt has nothing to scroll, and must not scroll
        // what is behind it. The help screen used to land here too, before
        // task 102 gave it a row cursor of its own — `list_cursor()` now
        // answers `Some` for it, so it is caught by the arm above instead.
        Some(_) => None,
        None => match model.screen {
            Screen::Viewer => Some(Cursor::Scroll),
            Screen::Manual => Some(Cursor::Manual),
            Screen::Settings => Some(Cursor::Settings),
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
        Cursor::Pick => match model.overlay_top() {
            Some(Overlay::Pick { idx, .. }) => (*idx, model.folders.len()),
            _ => return None,
        },
        Cursor::Overlay => model.overlay_top()?.list_cursor()?,
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
        Cursor::Settings => {
            let settings = model.settings.as_ref()?;
            (settings.cursor, settings.fields.len())
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
            if let Some(Overlay::Pick { idx, .. }) = model.overlay_top_mut() {
                *idx = at;
            }
        }
        Cursor::Overlay => {
            if let Some(overlay) = model.overlay_top_mut() {
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
        Cursor::Settings => {
            if let Some(settings) = model.settings.as_mut() {
                settings.cursor = at;
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

/// How many rows one `cursor.page-down` moves, given the terminal the last
/// [`Msg::Resize`] described.
///
/// The visible rows of the scrolling pane, less [`PAGE_OVERLAP`], and never
/// less than one: a terminal shorter than its own chrome still has to page by
/// *something*, and one row is `cursor.down`, which is the correct degenerate
/// answer rather than a movement of zero that reads as a dead key.
fn page_rows(model: &Model) -> usize {
    usize::from(model.viewport_rows.saturating_sub(CHROME_ROWS))
        .saturating_sub(PAGE_OVERLAP)
        .max(1)
}

/// `<c-d>`/`<c-u>` — move the active cursor by a screenful.
///
/// The same movement [`move_cursor`] makes over a different distance, and
/// deliberately built on the same three pieces ([`active_cursor`],
/// [`cursor_span`], [`set_cursor`]) rather than on a scroll offset of its own:
/// every surface with a cursor pages, and none of them needs to know that it
/// does. Which is also why there is no arm here for "the viewer" or "the
/// manual" — the viewer's cursor *is* its scroll offset and the manual's is a
/// row, and both are already `Cursor` variants.
///
/// A count means pages, the way vim's own page keys read a count, so `3<c-d>`
/// is three screens rather than three rows — `3j` is already three rows. It
/// multiplies arithmetic that is clamped either way, never a command.
///
/// The command line is the one layer that binds these and does nothing with
/// them: it reports no list cursor at all (its `<up>`/`<down>` are its
/// history), so `active_cursor` finds nothing and the key is inert there
/// rather than paging the mail behind it.
fn page(model: &mut Model, direction: Direction, count: Option<u32>) -> Vec<Cmd> {
    let Some(cursor) = active_cursor(model) else {
        return Vec::new();
    };
    let Some((idx, last)) = cursor_span(model, cursor) else {
        return Vec::new();
    };
    unpin_summary(model, cursor);
    let by = page_rows(model).saturating_mul(rows(count));
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
    // A form's open field is inside the form, the way the manual's search line
    // is inside the manual: `<esc>` puts the value back and leaves the form up.
    // Ahead of the take below, because taking the overlay first would close the
    // whole form to cancel one field.
    if let Some(Overlay::Form(pane)) = model.overlay_top_mut() {
        if pane.cancel_edit() {
            model.info("cancelled");
            return Vec::new();
        }
    }
    if let Some(overlay) = model.pop_overlay() {
        // The help screen was not collecting anything, so "cancelled" would
        // be a lie; the others were. Half true since task 102: browsing it
        // still collects nothing, but a half-typed filter is exactly the
        // thing every other typing surface here calls cancelled.
        let collecting = match &overlay {
            Overlay::Help(pane) => pane.editing,
            _ => true,
        };
        if collecting {
            model.info("cancelled");
        }
        let stop = cancels(&overlay);
        // A question asked over a report puts the report back rather than
        // dropping two layers for one `n`: declining the question is not
        // asking to leave the screen it was asked on, and the report's own
        // stream is still the one running.
        //
        // `restore_overlay`, not `set_overlay`: this is putting back what
        // was just popped, not opening something new. The distinction is
        // not academic — `set_overlay` replaces whatever is *now* on top,
        // so if the confirm had itself been pushed over a third layer
        // (§2.2.2's own "confirm over picker over collection"), `set_overlay`
        // would silently clobber that third layer with the restored report
        // instead of leaving it where popping the confirm already put it.
        if let Overlay::Confirm {
            then: Confirmed::Invoke {
                over: Some(over), ..
            },
            ..
        } = overlay
        {
            model.restore_overlay(Some(Overlay::Report(over)));
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
    // Before the viewer check, and before the visual selection above: the
    // settings screen is the innermost thing when it is up.
    if model.screen == Screen::Settings {
        set_screen(model, Screen::List);
        model.info("closed settings");
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
        Overlay::Reply(_) => &[Stream::Reply],
        Overlay::Report(_) => &[Stream::Report],
        // A form's own pre-fill read shares the report slot: only one of the two
        // is on screen at a time, and `Esc` needs one thing to cancel whichever
        // it is.
        Overlay::Form(_) => &[Stream::Report],
        Overlay::Help(_)
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
    let (what, message_ids, idx) = match model.pop_overlay() {
        Some(Overlay::Pick {
            what,
            message_ids,
            idx,
        }) => (what, message_ids, idx),
        other => {
            model.restore_overlay(other);
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
    let then = match model.pop_overlay() {
        Some(Overlay::Confirm { then, .. }) => then,
        other => {
            model.restore_overlay(other);
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
    // `<enter>` closes the field it opened and leaves the form up. Not an
    // apply: the form is several fields and committing on the first `<enter>`
    // would make filling in the second one impossible.
    if let Some(Overlay::Form(pane)) = model.overlay_top_mut() {
        if pane.editing.is_some() {
            pane.commit();
            return Vec::new();
        }
    }
    let (buffer, what, message_id) = match model.pop_overlay() {
        Some(Overlay::Input {
            buffer,
            what,
            message_id,
            ..
        }) => (buffer, what, message_id),
        other => {
            model.restore_overlay(other);
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
    model.set_overlay(Overlay::Confirm {
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
    model.set_overlay(Overlay::Pick {
        what,
        message_ids: ids,
        idx: 0,
    });
    Vec::new()
}

/// `:reply --ai [intent]`: open the streaming reply pane and start it.
///
/// Not counted into [`Model::inflight`], for the reason [`ask_now`] is not:
/// the pane's own "drafting…" state is what says this is running, the same
/// way a superseded search's own state is.
fn start_ai_reply(model: &mut Model, intent: String, reply_all: bool) -> Vec<Cmd> {
    // `single_target`, not `target_message`, and closed first regardless of
    // which way this goes: `reply(model)` is what bare `:reply` calls, and it
    // is `single_target` that refuses a visual selection ("that acts on one
    // message") rather than silently drafting from the row under the cursor.
    // Two rules for what "the target" means on the same verb, one gated on a
    // flag, would be a `--ai` that quietly acts on different mail than the
    // line without it names.
    close_command(model);
    let Some(message_id) = single_target(model) else {
        return Vec::new();
    };
    model.generation += 1;
    let generation = model.generation;
    model.set_overlay(Overlay::Reply(Box::new(ReplyPane {
        message_id,
        generation,
        ..ReplyPane::default()
    })));
    model.info("reply — drafting…");
    vec![Cmd::DraftReply {
        generation,
        message_id,
        intent,
        reply_all,
    }]
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
    model.set_overlay(Overlay::Input {
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
// the key reference (task 102)
// ---------------------------------------------------------------------------

/// `?` — the key reference, at whichever mode is current.
fn open_help(model: &mut Model) -> Vec<Cmd> {
    let mode = model.mode();
    model.set_overlay(Overlay::Help(Box::new(HelpPane::new(mode, &model.keymap))));
    Vec::new()
}

/// Recompute the key reference's rows after its mode or filter changes, and
/// reset the cursor to the top of the new set.
///
/// The two-step borrow `refresh_command` also uses: [`help::rows`] wants
/// `&Keymap` and the pane's own `mode`/`filter` at once, which a single
/// `model.overlay_top_mut()` cannot offer alongside `&model.keymap` borrowed
/// at the same time.
fn refresh_help(model: &mut Model) {
    let Some(Overlay::Help(pane)) = model.overlay_top() else {
        return;
    };
    let rows = help::rows(pane.mode, &pane.filter, &model.keymap);
    if let Some(Overlay::Help(pane)) = model.overlay_top_mut() {
        pane.rows = rows;
        // Reset rather than clamped: a mode switch or a filter edit changes
        // *what* is at every index, not just how many, so whatever survived
        // at the old cursor position is unlikely to be the row somebody
        // meant to still be looking at.
        pane.cursor = 0;
    }
}

/// Recompute the key reference's rows after `keys.toml` reloads underneath
/// it, clamping the cursor rather than resetting it.
///
/// Not [`refresh_help`]: a mode switch or a filter edit is the *person
/// looking at this screen* asking for a different list, where jumping to
/// the top is the right answer for a set of rows that is now about
/// something else entirely. A `keys.toml` reload is not that — it can land
/// at any moment mid-browse, was not asked for by whoever is looking at
/// this overlay, and typically changes at most one row. Resetting to the
/// top on every one of those would make browsing the key reference while
/// rebinding things from a second terminal (this very overlay's own `c`
/// row action, run from *this* terminal, reloads the same way) actively
/// hostile. The cursor's *index* survives, not necessarily the row at
/// it — a reload that removes a binding above it shifts a different action
/// underneath, the same trade every other list here makes — and if the
/// list shrank past the old index, it clamps to the new last row.
fn reload_help(model: &mut Model) {
    let Some(Overlay::Help(pane)) = model.overlay_top() else {
        return;
    };
    let rows = help::rows(pane.mode, &pane.filter, &model.keymap);
    if let Some(Overlay::Help(pane)) = model.overlay_top_mut() {
        pane.rows = rows;
        let count = help::binding_count(pane);
        pane.cursor = pane.cursor.min(count.saturating_sub(1));
    }
}

/// `<tab>`/`<c-i>` (forward) and `<c-o>` (back) on the key reference: cycle
/// which mode's chain it shows, wrapping through every configurable mode.
///
/// The same two direction bindings the manual's jump list uses in this
/// layer, made context-sensitive here rather than given bindings of their
/// own — the collision `keymap::mod`'s defaults document, and the same
/// answer `open_search` already gives the sibling collision on `/`.
fn cycle_help_mode(model: &mut Model, jump: Jump) -> Vec<Cmd> {
    let Some(Overlay::Help(pane)) = model.overlay_top_mut() else {
        return Vec::new();
    };
    let modes = Mode::CONFIGURABLE;
    let Some(idx) = modes.iter().position(|candidate| *candidate == pane.mode) else {
        return Vec::new();
    };
    pane.mode = match jump {
        Jump::Forward => modes[(idx + 1) % modes.len()],
        Jump::Back => modes[(idx + modes.len() - 1) % modes.len()],
    };
    refresh_help(model);
    Vec::new()
}

/// `c` on the key reference: open a rebind for the highlighted row, the
/// command line pre-filled with `keys set <chord> <action>`.
///
/// The mode flag is only spelled out when it would change anything: `keys
/// set`'s own default is `normal`, so a row from that mode's own chain does
/// not need `--mode normal` to say what is already true.
fn open_help_rebind(model: &mut Model) -> Vec<Cmd> {
    let Some(Overlay::Help(pane)) = model.overlay_top() else {
        return Vec::new();
    };
    let Some(action) = help::selected(pane) else {
        model.fail("nothing highlighted to rebind");
        return Vec::new();
    };
    let mode = pane.mode;
    let Some(chord) = model.keymap.chords_for(mode, action).into_iter().next() else {
        return Vec::new();
    };
    // `chords_for` walks `mode`'s whole chain, so the chord it found can
    // live in a farther layer than the one being browsed — most rows do,
    // since a mode's own key reference mostly shows what it *inherits*.
    // Rebinding has to target the layer that actually owns the binding, not
    // the layer being looked at: prefilling the browsed mode would not
    // replace anything, only add a shadow next to a binding still live
    // everywhere that layer is inherited.
    let Some(owner) = owning_mode(&model.keymap, mode, action, &chord) else {
        return Vec::new();
    };
    if !Mode::CONFIGURABLE.contains(&owner) {
        // Global's two bindings are the way out from every mode and are
        // deliberately not rebindable (`DEFAULTS`'s own comment). Failing
        // here says why, instead of opening a command line that
        // `keys_file::edit`'s reserved-chord check would refuse anyway.
        model.fail(format!(
            "{} is bound in every mode and cannot be rebound",
            action.id()
        ));
        return Vec::new();
    }
    let mode_flag = if owner == Mode::Normal {
        String::new()
    } else {
        // `=`-joined: `command::tokenize` only recognizes `--name=value` as
        // a value-carrying flag — `--mode help` (space separated) tokenizes
        // as an empty `--mode` flag followed by a stray `help` word, which
        // `check_flags` refuses as `MissingFlagValue`. Caught by
        // `every_colon_line_an_authored_page_shows_parses_and_uses_an_honoured_range`
        // against the manual's own `:keys set --mode viewer …` example
        // before it ever reached this line's own behavior.
        format!(" --mode={}", owner.id())
    };
    let input = format!("keys set {chord} {}{mode_flag}", action.id());
    model.set_overlay(Overlay::Command(Box::new(CommandPane {
        input,
        ..CommandPane::default()
    })));
    refresh_command(model);
    model.info("rebind — edit and press Enter, or Esc to cancel");
    Vec::new()
}

/// Which mode in `browsing`'s chain actually binds `chord` to `action` in
/// its own layer, as opposed to inheriting it from farther out.
///
/// [`Keymap::chords_for`] walks the same chain but only answers "does a
/// chord reach `browsing`", not "which layer declared it" — the second
/// question is what [`open_help_rebind`] needs, since editing the layer
/// that is merely on screen would shadow an inherited binding rather than
/// replace it.
fn owning_mode(keymap: &Keymap, browsing: Mode, action: Action, chord: &Chord) -> Option<Mode> {
    browsing
        .chain()
        .iter()
        .copied()
        .find(|&layer| keymap.layer(layer).any(|(c, a)| c == chord && a == action))
}

// ---------------------------------------------------------------------------
// the manual (task 103)
// ---------------------------------------------------------------------------

/// `K` — the manual, at its front page, or — from the key reference (task
/// 102) — at the page documenting the highlighted row.
fn open_manual(model: &mut Model) -> Vec<Cmd> {
    if let Some(Overlay::Help(pane)) = model.overlay_top() {
        if let Some(action) = help::selected(pane) {
            return open_manual_at(model, action.id());
        }
    }
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
    // The manual is a *screen*, so any overlay left up would cover the thing
    // the caller just asked to show — every one of them, not just the top,
    // so this clears the whole stack rather than popping one. Clearing also
    // stops whatever each was streaming.
    let stop: Vec<Cmd> = model.clear_overlays().iter().flat_map(cancels).collect();
    let mut cmds = enter_manual(model, manual::Location::Page(anchor.to_owned()));
    cmds.extend(stop);
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
    // The key reference's own mode-cycling, task 102: `<tab>`/`<c-i>` and
    // `<c-o>` are this layer's manual-jump bindings, made context-sensitive
    // here rather than given a binding of their own — the same collision
    // `open_search` already resolves for the sibling `/`.
    if matches!(model.overlay_top(), Some(Overlay::Help(_))) {
        return cycle_help_mode(model, jump);
    }
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
    if model.overlay_is_open() {
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
    let stop: Vec<Cmd> = model.clear_overlays().iter().flat_map(cancels).collect();
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
    cmds.extend(stop);
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
    !model.overlay_is_open()
}

/// `/` — open the search overlay, or take an open one back to its query line.
fn open_search(model: &mut Model) -> Vec<Cmd> {
    // `/` means "search what is in front of me" in every layer that binds it.
    // On the manual that is this page: opening the mailbox search overlay
    // there would cover the text it was pressed to search.
    if model.screen == Screen::Manual && !model.overlay_is_open() {
        return prompt_manual(model, Scope::Page);
    }
    // The key reference's own rows, task 102: `/` starts a live filter over
    // them rather than opening a second overlay on top. Silent on the status
    // line like every other key this overlay's own cursor and mode-cycling
    // already answer — a hint on every press of `/`, `<tab>` or `j` would be
    // noise none of Help's other in-place moves write either.
    if let Some(Overlay::Help(pane)) = model.overlay_top_mut() {
        pane.editing = true;
        return Vec::new();
    }
    if let Some(Overlay::Search(pane)) = model.overlay_top_mut() {
        pane.focus = SearchFocus::Query;
        return Vec::new();
    }
    if !screen_is_clear(model) {
        return Vec::new();
    }
    model.set_overlay(Overlay::Search(Box::default()));
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
    let Some(Overlay::Search(pane)) = model.overlay_top_mut() else {
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
    if let Some(Overlay::Search(pane)) = model.overlay_top_mut() {
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
    let Some(Overlay::Search(pane)) = model.overlay_top_mut() else {
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
    model.set_overlay(Overlay::Search(Box::new(SearchPane {
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
    model.set_overlay(Overlay::Finder(Box::default()));
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
    let Some(Overlay::Finder(pane)) = model.overlay_top_mut() else {
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
    let item = match model.overlay_top() {
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
            model.clear_overlays();
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
    // one thing it may open *over* — and it clears whatever is open rather
    // than stacking on it (`clear_overlays`, not `push_overlay`), taking
    // whatever any of it was streaming down too. The alternative, "restore
    // the menu on Esc" (real now that task 108 gave `Model` an actual
    // overlay stack), would leave a restored search pane holding results
    // whose stream was already cancelled — worse than the pane just being
    // gone. A modal that answers `:` is a modal asking to be replaced — the
    // same call `open_manual_grep_for` makes when it is dispatched from one.
    let stop: Vec<Cmd> = if model.mode() == Mode::Menu {
        model.clear_overlays().iter().flat_map(cancels).collect()
    } else if screen_is_clear(model) {
        Vec::new()
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
    model.set_overlay(Overlay::Command(Box::new(CommandPane {
        input,
        ..CommandPane::default()
    })));
    refresh_command(model);
    model.info("command — type a verb, Enter runs it, Tab completes");
    stop
}

/// The range prefix a `:` opened over a visual selection starts with.
const SELECTION_RANGE: &str = "'<,'>";

fn refresh_command(model: &mut Model) {
    let Some(Overlay::Command(pane)) = model.overlay_top() else {
        return;
    };
    let matches = command_matches(&pane.input.clone(), &model.keymap);
    if let Some(Overlay::Command(pane)) = model.overlay_top_mut() {
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
    let Some(Overlay::Command(pane)) = model.overlay_top() else {
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
    if let Some(Overlay::Command(pane)) = model.overlay_top_mut() {
        pane.error = Some(why);
        return Vec::new();
    }
    model.fail(why);
    Vec::new()
}

/// The best-ranked match's verb path, if the pane has one.
fn best_match(model: &Model) -> Option<String> {
    match model.overlay_top() {
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
    let (prefix, bang) = match model.overlay_top() {
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
    if verb == "keys set" {
        // Same rule as `set` just above: left open on a bad chord/action/mode
        // (`complain` writes into the command pane's own error line) and
        // only closed once the write has actually landed. A hand-written
        // case rather than the generic too-many-arguments check just below,
        // for the same reason `set` is: both positionals are declared
        // optional so this custom "needs two arguments" message can fire,
        // and neither `run_action` nor `open_report` know how to edit a
        // file.
        let [chord, action] = invocation.positionals.as_slice() else {
            return complain(
                model,
                "keys set needs two arguments: a chord and an action".to_owned(),
            );
        };
        let mode = invocation
            .flags
            .iter()
            .find(|flag| flag.name == "mode")
            .and_then(|flag| flag.value.as_deref())
            .unwrap_or("normal");
        return set_keybinding(model, &keys_path_from_env(), mode, chord, action);
    }
    if verb == "account use" {
        // Hand-written for the reason `set` and `keys set` are: it reaches no
        // capability, and the id it takes is not something an `Action` can
        // carry. Left open on a bad id (`complain` writes into the command
        // pane's own error line) and closed only once the switch has happened.
        let Some(id) = invocation.positionals.first() else {
            return complain(
                model,
                "account use needs an id — :account list has them".to_owned(),
            );
        };
        let Ok(id) = id.parse::<i64>() else {
            return complain(
                model,
                format!("{id:?} is not an account id — :account list has them"),
            );
        };
        return use_account(model, id);
    }
    if verb == "keys check" {
        // Hand-written next to `:keys set` and for the same reason: it reads the
        // keymap in this process and reaches no capability. Every hit is a
        // binding somebody wrote that the keyboard can never deliver, which is a
        // fact about a local file and nothing a daemon knows.
        let shadowed = model.keymap.shadowed_across_layers();
        let generation = model.generation + 1;
        model.generation = generation;
        close_command(model);
        let rows: Vec<ReportRow> = shadowed
            .iter()
            .map(|(mode, dead, killer)| {
                ReportRow::new([mode.id().to_owned(), dead.to_string(), killer.to_string()])
                    .toned(report::ReportTone::Bad)
            })
            .collect();
        let found = rows.len();
        let mut pane = ReportPane::new(
            invocation,
            "keys check — bindings the keyboard can never deliver",
            vec![
                ReportColumn::new("mode", 12),
                ReportColumn::new("never fires", 16),
                ReportColumn::new("because this does", 16),
            ],
            generation,
        );
        // Complete on arrival: the answer is in this process, so a border
        // reading "asking…" would describe a request that was never made.
        pane.apply(generation, ReportFill::Replace, rows, true);
        model.set_overlay(Overlay::Report(Box::new(pane)));
        model.info(match found {
            0 => "every binding can be typed".to_owned(),
            1 => "1 binding can never be typed — unbind the shorter one".to_owned(),
            n => format!("{n} bindings can never be typed — unbind the shorter ones"),
        });
        return Vec::new();
    }
    if verb == "settings" {
        // Hand-written for the reason `manual grep` is: the section is not
        // something an `Action` can carry. Left open on a name this build does
        // not have, so the complaint lands on the command line with the offending
        // word still there to fix.
        let section = match invocation.positionals.first() {
            None => None,
            Some(name) => match settings::Section::from_id(name) {
                Some(section) => Some(section),
                None => {
                    let names: Vec<&str> = settings::Section::ALL
                        .iter()
                        .map(|section| section.id())
                        .collect();
                    return complain(model, format!("{name:?}: one of {}", names.join(", ")));
                }
            },
        };
        close_command(model);
        return open_settings(model, section);
    }
    if verb == "message open" {
        // Hand-written next to `:attach list` and for the same reason: it reaches
        // no capability of its own. What it does is navigate, which no `Answer`
        // shape describes — a report row that opened a message would otherwise
        // have to be a special case in the dispatcher rather than a `:` line.
        let Some(message_id) = invocation
            .positionals
            .first()
            .and_then(|id| id.parse::<i64>().ok())
        else {
            return complain(
                model,
                "message open needs an id — a citing report row carries one".to_owned(),
            );
        };
        close_command(model);
        return open_message_by_id(model, message_id);
    }
    if verb == "attach list" {
        // Hand-written next to `:toml` and for the same reason: it reaches no
        // capability. The open message's parts came back with the message, so a
        // round trip to re-fetch what the preview pane is already drawing would
        // be a second source of truth for one listing.
        let Some(open) = model.open.as_ref() else {
            return complain(
                model,
                "no message open — <enter> on a row opens one".to_owned(),
            );
        };
        let rows: Vec<ReportRow> = open
            .attachments
            .iter()
            .map(|line| ReportRow::new([line.clone()]))
            .collect();
        let message_id = open.id;
        let generation = model.generation + 1;
        model.generation = generation;
        close_command(model);
        let mut pane = ReportPane::new(
            invocation,
            format!("attach list {message_id}"),
            vec![ReportColumn::new("attachment", 72)],
            generation,
        );
        // Complete on arrival, for the reason a block's report is: nothing is
        // outstanding, so a border reading "asking…" would describe a request
        // that was never made.
        pane.apply(generation, ReportFill::Replace, rows, true);
        let count = pane.rows.len();
        model.set_overlay(Overlay::Report(Box::new(pane)));
        model.info(match count {
            0 => "nothing attached to this message".to_owned(),
            1 => "1 attachment · :attach tables, :attach invoice, :attach ask".to_owned(),
            n => format!("{n} attachments · :attach tables, :attach invoice, :attach ask"),
        });
        return Vec::new();
    }
    if verb == "toml" {
        // Hand-written next to `:account use` and for the same reason: it
        // reaches no capability. Closed only once there is something to open,
        // so the complaint lands on the command line the way `:set`'s does.
        let Some(block) = model.block.clone() else {
            return complain(
                model,
                "no block yet — :account add, :hook add and :notify set each produce one"
                    .to_owned(),
            );
        };
        close_command(model);
        model.info(format!("opening {}…", block.label));
        return vec![Cmd::OpenText {
            text: block.toml.clone(),
            extension: "toml".to_owned(),
            label: block.label.clone(),
        }];
    }
    if verb == "reply" {
        // Hand-written for the reason `set`/`keys set` are: `--ai` branches
        // between two things an `Action` cannot carry — delegating to the
        // plain reply flow `r` already runs, or opening a new streaming
        // pane — so this has to decide before the generic daemon-verb
        // routing below ever sees it, the same way `keys set`'s `--mode`
        // is read before the generic flag check would refuse it as
        // "not wired up yet".
        let ai = invocation.flags.iter().any(|flag| flag.name == "ai");
        let reply_all = invocation.flags.iter().any(|flag| flag.name == "reply-all");
        // Joined rather than `positionals.first()`, for the reason
        // `helpgrep`'s `pattern` is: an unquoted multi-word intent is what
        // somebody types.
        let intent = invocation.positionals.join(" ");
        if !ai {
            if reply_all {
                return complain(model, "--reply-all needs --ai".to_owned());
            }
            if !intent.is_empty() {
                return complain(
                    model,
                    "an intent needs --ai — try `:reply --ai ...`".to_owned(),
                );
            }
            close_command(model);
            return reply(model);
        }
        return start_ai_reply(model, intent, reply_all);
    }
    // More arguments than the verb declares. Derived from the registry rather
    // than "action-backed verbs take none", which is what this was before task
    // 94 declared verbs that take one: `command::parse` collects trailing words
    // whatever a verb declares (task 89's own note says so), so a verb that
    // accepted them silently would be the "quietly accepts an argument it never
    // mentions" the grammar's docs call out.
    let declaration = command::verb_at(&path_of(&invocation));
    let declared = declaration.map_or(0, |verb| verb.positionals.len());
    // A verb whose last declared positional takes the rest has no upper bound:
    // `:rule new archive newsletters from marketing` is one instruction, and
    // reading only its first word is the silent truncation `:helpgrep`'s docs
    // call out. `Positional::rest` is where that is declared rather than being a
    // list of verb names here.
    let variadic = declaration
        .and_then(|verb| verb.positionals.last())
        .is_some_and(|positional| positional.rest);
    if !variadic && invocation.positionals.len() > declared {
        return complain(
            model,
            match declared {
                0 => format!(
                    "{verb} takes no arguments, and was given {}",
                    invocation.positionals.join(" ")
                ),
                n => format!(
                    "{verb} takes {n} argument(s), and was given {}",
                    invocation.positionals.len()
                ),
            },
        );
    }
    // Every verb with no `Action` behind it, after the argument guards rather
    // than before them: there is nothing to delegate to, so `tui::commands` is
    // the table that answers. Usually that means a capability to reach; since
    // task 98 it can also mean a block to render (`:hook add`, `:notify set`
    // reach no RPC at all), which is why this no longer tests
    // `capability.is_some()` — doing so sent those two into the flag check below
    // and had them refused for carrying flags nothing had read.
    //
    // Before that check for the same reason it always was: task 100's verbs were
    // the first to declare flags of their own (`draft edit --body`,
    // `outbox reschedule --at`, `waiting --overdue`…), and `commands::answer` is
    // what reads them — the same way `keys set`'s hand-written `--mode` is read
    // above.
    // Who implements this verb: the answer table, or an `Action`.
    //
    // Asked of the table directly rather than inferred from the invocation.
    // Neither proxy works. "It has no action" was true until task 105's leader
    // map gave thirteen table verbs an action as well — the verb is the
    // capability's surface and the action is only the key that reaches it — and
    // routing on that would send `:tag list` to `run_action`, whose arm runs
    // `:tag list`, forever. "It has a capability" is false the other way:
    // `Action::Delete`'s auto-derived `:message delete` carries `MailDelete`
    // through `Capability::for_action`, and the table has no arm for it.
    //
    // `commands::answer` is pure — no overlay, no request, no `Model` — so
    // asking it here and again inside cannot drift, and it is the only thing that
    // actually knows.
    //
    // Before the generic flag check below, for the reason it always was: these
    // verbs declare flags of their own and `commands::answer` is what reads them.
    if invocation.action.is_none()
        || commands::answer(&invocation, &target_of(model), model.generation + 1).is_some()
    {
        return run_answered_command(model, invocation);
    }
    if let Some(flag) = invocation.flags.first() {
        // Reachable now only by an action-backed verb given a flag it did not
        // declare — `command::parse` already rejects a flag no verb declares
        // at all, so this is a verb that takes none refusing the one case
        // that gets this far: a flag *declared* on a verb with no daemon
        // command and no hand-written case above to read it.
        return complain(model, format!("{verb} --{}: not wired up yet", flag.name));
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
    if invocation.bang && matches!(model.overlay_top(), Some(Overlay::Confirm { .. })) {
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

/// `:set <option> <value>` — the pane widths, the AI panel width and the theme.
///
/// Task 101's settings screen lands its own `Invocation`s here rather than
/// growing a second `:set`-shaped path next to it, which is what this function's
/// docs asked for before that screen existed. `theme` is the field it added, and
/// `Model::theme`'s own docs anticipated it by name: the theme lives on the model
/// rather than being a parameter `view::render` takes, so switching it is an
/// ordinary state mutation and not a second channel into the renderer.
fn set_option(model: &mut Model, option: &str, value: &str) -> Vec<Cmd> {
    // Ahead of the percentage options because it is not one. Not remembered
    // across sessions: that would be a config write, and `:set` writes nothing
    // anywhere else either.
    if option == "theme" {
        let Some(name) = super::theme::ThemeName::from_id(value) else {
            let names: Vec<&str> = super::theme::ThemeName::ALL
                .iter()
                .map(|theme| theme.id())
                .collect();
            return complain(
                model,
                format!("theme {value:?}: one of {}", names.join(", ")),
            );
        };
        model.theme = name.resolve();
        close_command(model);
        model.info(format!("theme {}", name.id()));
        return Vec::new();
    }
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

/// `:keys set <chord> <action> [--mode <mode>]` (task 102) — `keys.toml`'s
/// TUI-side counterpart to `mail keys set`, and the verb the key
/// reference's `c` row action pre-fills.
///
/// Edits the file directly rather than going through the daemon: a key
/// binding is a property of the terminal in front of whoever is pressing
/// keys, not of the mailbox, the same reason `mail keys set` is a local file
/// edit and not an RPC (`keys_cli`'s own module docs). `model.keymap` itself
/// is not touched here — a running `mail tui` re-reads `keys.toml` within a
/// second and swaps its bindings live, the same reload path a
/// `mail keys set` run from a second terminal already relies on.
///
/// Only validates and dispatches [`Cmd::WriteKeybinding`]; [`write_keybinding`]
/// does the actual read/edit/write, off this function and off [`update`],
/// for the reason that command's own doc gives.
///
/// Takes `path` as a parameter rather than resolving `$RMAIL_KEYS` itself,
/// the same split `keys_cli::set` keeps between "which file" and "edit this
/// file" — `$RMAIL_KEYS` is process-global and `tests` runs alongside
/// everything else in this test binary, so the boundary that reads it stays
/// at the caller.
fn set_keybinding(
    model: &mut Model,
    path: &std::path::Path,
    mode: &str,
    chord: &str,
    action: &str,
) -> Vec<Cmd> {
    let Some(mode) = Mode::from_id(mode) else {
        return complain(model, format!("unknown mode: {mode}"));
    };
    let chord = match Chord::parse(chord) {
        Ok(chord) => chord,
        Err(error) => return complain(model, error.to_string()),
    };
    let Some(action) = Action::from_id(action) else {
        return complain(model, format!("unknown action: {action}"));
    };

    let label = format!(
        "bound {chord} to {} in {} mode ({})",
        action.id(),
        mode.id(),
        path.display()
    );
    close_command(model);
    model.inflight += 1;
    vec![Cmd::WriteKeybinding {
        path: path.to_path_buf(),
        mode,
        chord,
        action,
        label,
    }]
}

/// The blocking half of [`Cmd::WriteKeybinding`]: read `keys.toml` (or
/// treat it as empty if absent — the normal first-run state, the same
/// reading `keys_cli::set` gives it), edit in the one binding, and write it
/// back. `crate::keymap::file` is the one place the edit itself is
/// implemented; this calls it rather than growing a second copy.
///
/// `keys_file::read_bounded` rather than `std::fs::read_to_string`: this
/// runs on a blocking-pool thread behind [`Model::inflight`], which nothing
/// but [`Msg::KeysWritten`] arriving ever releases — a path that never
/// reaches EOF (a device file, a fifo someone pointed `$RMAIL_KEYS` at)
/// would hang the read forever and strand the counter with it. See
/// `read_bounded`'s own doc for why `keys_cli::set`, a short-lived CLI
/// process, does not need the same guard.
///
/// # Errors
///
/// A human-readable reason the read, the edit or the write failed.
pub fn write_keybinding(
    path: &std::path::Path,
    mode: Mode,
    chord: &Chord,
    action: Action,
) -> Result<(), String> {
    let existing = match keys_file::read_bounded(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(format!("reading {}: {error}", path.display())),
    };
    let updated =
        keys_file::edit(&existing, mode, chord, Some(action)).map_err(|error| error.to_string())?;
    keys_file::write_atomic(path, &updated)
        .map_err(|error| format!("writing {}: {error}", path.display()))
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

/// An invocation's verb path as the registry indexes it.
///
/// Borrowed from the invocation's own segments rather than re-split from the
/// joined string, so there is no second place that decides where a path breaks.
fn path_of(invocation: &command::Invocation) -> Vec<&str> {
    invocation.verb.iter().map(String::as_str).collect()
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
    model.clear_overlays();
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
        history,
        overlay_stack,
        ..
    } = model;
    let Some(Overlay::Command(pane)) = overlay_stack.last_mut() else {
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

/// Dispatch a verb with no [`Action`] behind it — task 90's seam, and the one
/// place tasks 94 onward plug into.
///
/// The verb's answer is looked up in `tui::commands`, which is pure data; this
/// is the only code that turns one into an overlay, a request or a refusal. So
/// the confirmation gate, the generation stamp and the Report exist once
/// regardless of how many verbs the table grows to.
///
/// Most of these reach a capability. A few reach nothing — task 98's `:hook add`
/// and `:notify set` render a TOML block, because the settings they name have no
/// RPC that writes them — and they come through here too, so the pane, the
/// status line and the refusal wording are the same code for both.
fn run_answered_command(model: &mut Model, invocation: command::Invocation) -> Vec<Cmd> {
    let verb = invocation.verb.join(" ");
    let generation = model.generation + 1;
    let Some(answer) = commands::answer(&invocation, &target_of(model), generation) else {
        return complain(
            model,
            format!("{verb} is declared, but this build has no answer for it"),
        );
    };
    let request = match answer {
        Answer::Refused(why) => return complain(model, format!("{verb}: {why}")),
        // A form's read is issued and its pane opened here rather than below,
        // because a form has no columns and no confirmation: the pane *is* the
        // confirmation — it shows what is about to be replaced, which is more
        // than a `[y/N]` could say.
        Answer::Form(request) => {
            model.generation = generation;
            close_command(model);
            let mut pane = FormPane::new(invocation, request.title, request.fields, generation);
            pane.prefill(&pane.invocation.flags.clone());
            model.set_overlay(Overlay::Form(Box::new(pane)));
            model.info(format!("{verb} — reading what is in force…"));
            return vec![request.cmd];
        }
        // A block, and no request: the verb names a setting nothing writes over
        // the wire. Opened as a report over the block's own rows, and remembered
        // so `:toml` can open it after the report has been closed — which is
        // where the copy affordance lives, for the reason `tui::config_block`
        // gives.
        Answer::Block(block) => {
            model.generation = generation;
            close_command(model);
            let mut pane = ReportPane::new(
                invocation,
                format!("{} — paste it into your config", block.label),
                vec![
                    ReportColumn::new("what", 14),
                    ReportColumn::new("value", 62),
                ],
                generation,
            );
            // Complete on arrival: there is nothing outstanding, so a border
            // reading "asking…" would be describing a request that was never
            // made.
            pane.apply(generation, ReportFill::Replace, block.rows(), true);
            let label = block.label.clone();
            model.block = Some(block);
            model.set_overlay(Overlay::Report(Box::new(pane)));
            model.info(format!("{label} — <enter> on a row, or :toml to open it"));
            return Vec::new();
        }
        Answer::Rows(request) | Answer::Fact(request) => request,
    };
    // The question comes before anything else is touched, and it carries the
    // whole invocation with a bang on it — so answering `y` re-enters here and
    // takes the same path a typed `!` would, rather than a second one that
    // could drift from it.
    if let Some(prompt) = request.confirm.clone() {
        close_command(model);
        model.set_overlay(Overlay::Confirm {
            prompt,
            then: Confirmed::Invoke {
                invocation: Box::new(command::Invocation {
                    bang: true,
                    ..invocation
                }),
                over: None,
            },
        });
        return Vec::new();
    }
    model.generation = generation;
    close_command(model);
    if request.columns.is_empty() {
        // A fact. Counted into `inflight`, unlike the heartbeat: somebody asked
        // for this one.
        model.inflight += 1;
        model.info(request.title);
        return vec![request.cmd];
    }
    let mut pane = ReportPane::new(invocation, request.title, request.columns, generation);
    if request.once {
        pane = pane.only_once();
    }
    model.set_overlay(Overlay::Report(Box::new(pane)));
    model.info(format!("{verb} — r re-runs · Esc closes"));
    vec![request.cmd]
}

/// `:account use <id>` — look at another account without restarting.
///
/// Everything on screen belongs to the account it came from: the folder list,
/// the message rows, the open message, the analysis panel, the visual selection.
/// So this clears all of it rather than leaving one account's rows under a header
/// naming another, and then issues exactly what `Msg::Accounts` issues when the
/// first account loads — one path for "start looking at this account", not two
/// that can drift.
///
/// Refused for an id the daemon has never listed, rather than sent: a
/// `LoadFolders` for an account that does not exist answers `NOT_FOUND` two
/// round trips later, by which point the screen has already been cleared.
fn use_account(model: &mut Model, id: i64) -> Vec<Cmd> {
    let Some(account) = model
        .accounts
        .iter()
        .find(|account| account.id == id)
        .cloned()
    else {
        return complain(
            model,
            match model.accounts.is_empty() {
                true => "no accounts listed yet — :account list reads them".to_owned(),
                false => format!("no account {id} — :account list has the ids"),
            },
        );
    };
    close_command(model);
    if model.account.as_ref().is_some_and(|open| open.id == id) {
        // Not an error, and deliberately not a reload either: somebody asking
        // for the account they are already on wants nothing to happen, and
        // throwing away their cursor and their open message to fetch the same
        // rows again would be the opposite of nothing.
        model.info(format!("already looking at {}", account.name));
        return Vec::new();
    }
    let name = account.name.clone();
    model.account = Some(account);
    // Every one of these is about the account being left. `folder_idx` and
    // `message_idx` go to zero rather than being clamped, because a cursor is a
    // position in a list and this is a different list.
    model.folders = Vec::new();
    model.folder_idx = 0;
    model.open_folder = None;
    model.messages = Vec::new();
    model.message_idx = 0;
    model.open = None;
    model.opening = None;
    model.scroll = 0;
    model.visual = None;
    model.summary = None;
    model.summary_for = None;
    model.summary_failed = None;
    model.summary_pinned = None;
    model.focus = Focus::Folders;
    // The viewer and the manual are screens over an account's mail; the list is
    // where a freshly switched account can actually be looked at.
    set_screen(model, Screen::List);
    // The undo toast counts down a send from the account being left, and `u`
    // would cancel an outbox entry that is no longer on screen.
    remove_undo_toast(model);
    let account_id = id;
    model.info(format!("{name} — loading folders…"));
    // Two counted requests, and two that are not, for exactly the reasons
    // `Msg::Accounts` gives: nobody asked for the event stream or the heartbeat
    // and neither ever finishes. Both supersede rather than accumulate — see
    // `tui::grpc`'s `watching` and `beating` slots — so switching accounts twice
    // does not leave two streams open on the daemon.
    model.inflight += 2;
    vec![
        Cmd::LoadFolders { account_id },
        Cmd::Watch { account_id },
        Cmd::LoadOutbox { account_id },
        Cmd::Heartbeat { account_id },
    ]
}

/// `:settings [<section>]`, `gs`, and `s` from any Report.
///
/// Opened on `section`, or on whatever section was last open — so `<tab>`ing to
/// Notifications, closing, and reopening puts you back there rather than at
/// Accounts. The state is rebuilt either way, because a field's selection is
/// where the last `<enter>` left it and carrying that across a close would be
/// remembering a value nothing has read back.
fn open_settings(model: &mut Model, section: Option<settings::Section>) -> Vec<Cmd> {
    // Any overlay open is the same rule `:` and the manual follow: a Report
    // answering `s` is a Report asking to be replaced (the whole stack, not
    // just its top), and clearing takes every one of their streams down
    // with it.
    let stop: Vec<Cmd> = if model.mode() == Mode::Menu {
        model.clear_overlays().iter().flat_map(cancels).collect()
    } else if screen_is_clear(model) {
        Vec::new()
    } else {
        return Vec::new();
    };
    let section = section
        .or(model.last_settings_section)
        .unwrap_or(settings::Section::Accounts);
    model.last_settings_section = Some(section);
    set_screen(model, Screen::Settings);
    model.settings = Some(settings::SettingsState::new(section));
    announce_settings(model);
    stop
}

/// `<tab>` — the next section.
fn next_settings_section(model: &mut Model) -> Vec<Cmd> {
    let Some(settings) = model.settings.as_mut() else {
        return Vec::new();
    };
    let next = settings.section.next();
    settings.go(next);
    model.last_settings_section = Some(next);
    announce_settings(model);
    Vec::new()
}

/// What the status line says about the section on screen.
fn announce_settings(model: &mut Model) {
    let Some(settings) = model.settings.as_ref() else {
        return;
    };
    let title = settings.section.title();
    let fields = settings.fields.len();
    model.info(format!(
        "settings › {title} — {fields} field(s) · <tab> next section · Esc closes"
    ));
}

/// `<enter>` on a settings field.
///
/// Every write here goes through `run_invocation`, which is the whole point of
/// the screen: there is no private path to the daemon, so a field cannot reach a
/// capability no verb reaches, cannot skip a confirmation a verb asks for, and
/// cannot do anything a typed line could not.
fn accept_setting(model: &mut Model) -> Vec<Cmd> {
    let Some(settings) = model.settings.as_ref() else {
        return Vec::new();
    };
    let Some(field) = settings.field() else {
        return Vec::new();
    };
    let label = field.label;
    match field.accept() {
        settings::Accepted::Run { line, at } => {
            if let Some(settings) = model.settings.as_mut() {
                if let Some(field) = settings.fields.get_mut(settings.cursor) {
                    field.at = at;
                }
            }
            let invocation = match settings::invocation(line) {
                Ok(invocation) => invocation,
                // Unreachable: `settings::tests::every_line_parses` walks every
                // field. Reported rather than unwrapped, because a client
                // holding a terminal in raw mode must not panic.
                Err(error) => {
                    model.fail(format!("{label}: {error}"));
                    return Vec::new();
                }
            };
            record_command(model, line);
            run_invocation(model, invocation)
        }
        // The command line, with the verb on it and the cursor after it. The one
        // kind that runs nothing: an address, a token label, a chord and an
        // action are things only the person at the keyboard has, so there is no
        // write for the screen to express.
        settings::Accepted::Type { line } => {
            let mut cmds = open_command(model);
            if let Some(Overlay::Command(pane)) = model.overlay_top_mut() {
                pane.input = if line.ends_with('=') {
                    line.to_owned()
                } else {
                    format!("{line} ")
                };
            }
            refresh_command(model);
            model.info(format!("{label} — finish the line and press Enter"));
            cmds.extend(std::iter::empty());
            cmds
        }
        settings::Accepted::Say { why } => {
            model.fail(format!("{label}: {why}"));
            Vec::new()
        }
        settings::Accepted::Nothing => Vec::new(),
    }
}

/// What the screen can offer a verb that needs a target.
fn target_of(model: &Model) -> Target {
    Target {
        account_id: model.current_account().map_or(0, |account| account.id),
        mailbox_id: model.open_folder,
        // `target_message` is what every message-shaped action already reads,
        // so `:ai process` acts on the same message `.` would analyse rather
        // than on a second notion of "the current one".
        message_id: target_message(model),
        // `targets` is what every bulk-capable action reads, which is what makes
        // `:'<,'>tag add work` do exactly what the key does with the same
        // selection up.
        selection: targets(model),
        rule_draft: model.rule_draft.clone(),
    }
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
///
/// A re-run never asks again, whatever the verb's own `confirm` says: the
/// question was answered to open this report, and asking it on every `r` would
/// make `r` the wrong key to press.
fn rerun_report(model: &mut Model) -> Vec<Cmd> {
    let Some(Overlay::Report(pane)) = model.overlay_top() else {
        return Vec::new();
    };
    let verb = pane.invocation.verb.join(" ");
    // A report whose verb *produced* something rather than read it. `r` means
    // "ask this again", and asking `:token create` again mints a second token —
    // so this refuses rather than doing it, and says which key does re-ask.
    if pane.once {
        model.fail(format!(
            "{verb} ran once — Esc, then type it again to run another"
        ));
        return Vec::new();
    }
    let invocation = command::Invocation {
        bang: true,
        ..pane.invocation.clone()
    };
    let generation = model.generation + 1;
    let request = match commands::answer(&invocation, &target_of(model), generation) {
        Some(Answer::Rows(request) | Answer::Fact(request)) => request,
        // A verb that answers with a form rather than rows. Unreachable today —
        // no form opens a report — and reported rather than ignored, because
        // "`r` did nothing" is the one outcome that would send somebody looking
        // for a broken key.
        Some(Answer::Form(_)) => {
            model.fail(format!("{verb} answers with a form — Esc, then type it"));
            return Vec::new();
        }
        // A block, which is a pure function of the line that produced it: `r`
        // would redraw exactly what is already on screen. Reported rather than
        // silently doing nothing, for the reason the form arm above is.
        Some(Answer::Block(_)) => {
            model.fail(format!("{verb} is already showing everything it has"));
            return Vec::new();
        }
        // A report open for a verb whose answer has become unavailable: the
        // account went away under a `:sync status`, say. Reported rather than
        // silently doing nothing, and the rows already on screen are left
        // alone because they were true when they arrived.
        Some(Answer::Refused(why)) => {
            model.fail(format!("{verb}: {why}"));
            return Vec::new();
        }
        None => {
            model.fail(format!("{verb} can no longer be re-run"));
            return Vec::new();
        }
    };
    model.generation = generation;
    if let Some(Overlay::Report(pane)) = model.overlay_top_mut() {
        pane.restart(generation);
    }
    model.info(format!("{verb} — re-running…"));
    vec![request.cmd]
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
    let Some(Overlay::Report(over)) = model.pop_overlay() else {
        return Vec::new();
    };
    if !report::mutates(&invocation) || invocation.bang {
        return run_row(model, invocation, Some(over));
    }
    let prompt = format!(":{} — run it? [y/N]", invocation.verb.join(" "));
    model.set_overlay(Overlay::Confirm {
        prompt,
        then: Confirmed::Invoke {
            invocation: Box::new(command::Invocation {
                bang: true,
                ..invocation
            }),
            over: Some(over),
        },
    });
    Vec::new()
}

/// `n` — the *no* half of a report row that offers both (task 95).
///
/// Goes through [`run_report_row`]'s own gate rather than dispatching directly,
/// so a rejection that happens to reach a mutating capability asks first for the
/// same reason an acceptance does. A row with no rejection is a no-op rather than
/// a complaint: `n` is bound in the whole `Menu` layer, and most rows there have
/// nothing to say no to.
fn reject_report_row(model: &mut Model) -> Vec<Cmd> {
    let rejection = match model.overlay_top() {
        Some(Overlay::Report(pane)) => pane.row().and_then(|row| row.on_reject.clone()),
        _ => None,
    };
    match rejection {
        Some(invocation) => run_report_row(model, invocation),
        None => Vec::new(),
    }
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
///
/// "Unless the command put something else in its place" is checked against
/// the *top* of the stack, not the whole thing — correct today, because
/// `over` was popped by [`run_report_row`] from a stack that was at most one
/// deep before it (every pre-108 call site still opens overlays via
/// `set_overlay`). If a future overlay this report was stacked *under* is
/// still there when this returns, this restores on top of it via
/// `set_overlay`, which is safe specifically because the `None` guard has
/// already established the stack is empty at that point — but it means the
/// report is silently dropped rather than restored whenever the guard is
/// `Some` (something else *is* on top), even if that something is unrelated
/// to where the report itself belongs. Not fixed here: doing so requires
/// deciding where in a multi-layer stack a report reappears, which nothing
/// reachable today needs an answer for, and tui.md does not specify at this
/// level of detail.
fn run_row(
    model: &mut Model,
    invocation: command::Invocation,
    over: Option<Box<ReportPane>>,
) -> Vec<Cmd> {
    let stale = report::mutates(&invocation);
    let cmds = run_invocation(model, invocation);
    if let (None, Some(mut over)) = (model.overlay_top(), over) {
        over.stale = over.stale || stale;
        model.set_overlay(Overlay::Report(over));
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

/// Run a `:` line, as though it had been typed.
///
/// What a key bound to a domain action does — task 105's leader map is a page of
/// them — and the reason it goes through [`run_invocation`] rather than reaching
/// the answer table directly: the range check, the argument guard, the
/// hand-written cases and the confirmation gate are all there, and a key that
/// skipped them would be a key that could do something no typed line can.
///
/// A line that does not parse is a bug in a *default binding*, not in anything a
/// user did, so it is reported rather than unwrapped — this client must not panic
/// with a terminal in raw mode. `keymap::tests::every_action_runs_a_line_that_parses`
/// is what keeps it unreachable.
fn run_verb(model: &mut Model, line: &str) -> Vec<Cmd> {
    match command::parse(line) {
        Ok(command::Resolution::Invocation(invocation)) => {
            close_command(model);
            run_invocation(model, *invocation)
        }
        _ => {
            model.fail(format!("{line} is not a command this build has"));
            Vec::new()
        }
    }
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
    model.clear_overlays();
    run_action(model, action, None)
}

/// `A` — the ask pane, with `question` already typed.
fn open_ask(model: &mut Model, question: String) -> Vec<Cmd> {
    // Reachable from a bare screen and from the `.` menu, which is the one
    // overlay that exists to launch others.
    if !matches!(model.overlay_top(), None | Some(Overlay::Quick(_))) {
        return Vec::new();
    }
    model.set_overlay(Overlay::Ask(Box::new(AskPane {
        question,
        ..AskPane::default()
    })));
    model.info("ask — type a question about your mail, Enter sends it");
    Vec::new()
}

/// `Enter` on the question line: send it.
fn ask_now(model: &mut Model) -> Vec<Cmd> {
    let question = match model.overlay_top() {
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
    let Some(Overlay::Ask(pane)) = model.overlay_top_mut() else {
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

/// `Z` — zoom the focused card full-bleed inside the card area, or unzoom it
/// (tui.md §4.5). [`super::view`] draws a named placeholder while a zoom is
/// active — each card's own zoomed render (List's headed sortable table is
/// task 143) lands later; this task only proves the toggle is real and
/// observable.
///
/// Refuses outside [`Screen::List`]: `card_focus`/`zoom` describe the card
/// deck, and only [`super::view::render_panes`] (List's own render path)
/// ever draws the placeholder. Reachable without this guard — `Z` is bound
/// in [`Mode::Viewer`]'s chain too — and without it the viewer would zoom
/// whatever `card_focus` was last left at, announce success, and change
/// nothing on screen until the user backs out to the list and finds a
/// placeholder they do not remember causing.
///
/// Always targets [`Model::card_focus`], never whatever
/// [`Model::zoom`] currently names: pressing `Z` is "zoom *this*", not
/// "toggle whatever was last zoomed" — so if a different card were zoomed
/// already, `Z` would replace it with the focused one rather than clearing
/// an unrelated zoom. That divergence is not reachable today: every path
/// that can change `card_focus` — only [`toggle_sidebar`]/[`toggle_rail`]'s
/// focus-summon branch, until task 132 — clears `zoom` itself, so the two
/// never disagree here. `zoom == Some(card_focus)` is therefore a safe
/// simplifying assumption for callers *today*, not a permanent one; task
/// 132 is what reopens the question and it must not assume this comment
/// still holds by then.
fn toggle_zoom(model: &mut Model) -> Vec<Cmd> {
    if model.screen != Screen::List {
        model.info("zoom applies to the card deck, not this screen");
        return Vec::new();
    }
    model.zoom = if model.zoom == Some(model.card_focus) {
        model.info(format!("{} unzoomed", model.card_focus.label()));
        None
    } else {
        model.info(format!("{} zoomed", model.card_focus.label()));
        Some(model.card_focus)
    };
    Vec::new()
}

/// Whether `bp` is a breakpoint at which [`layout::layout_mode`] actually
/// consults a card's visibility preference (`sidebar_visible`/
/// `rail_visible`) rather than deciding purely from focus — M/L/XL for
/// both cards (see `layout_m`'s/`layout_l_xl`'s own `ctx.sidebar_visible`/
/// `ctx.rail_visible` reads; `layout_s`/`layout_xs` never read either
/// field). The shared half of [`toggle_sidebar`]/[`toggle_rail`]'s "flip a
/// preference where that has any effect, focus-summon where it would not"
/// split (§4.4).
fn affords_split(bp: layout::Breakpoint) -> bool {
    matches!(
        bp,
        layout::Breakpoint::M | layout::Breakpoint::L | layout::Breakpoint::Xl
    )
}

/// `C-b` — the sidebar (tui.md §4.4).
///
/// Flips [`Model::sidebar_visible`] at a breakpoint that affords showing it
/// as part of the normal split; at a narrower one, `sidebar_visible` has no
/// effect on anything `layout::layout_mode` draws, so this instead
/// focus-summons it as a drawer — "same key, same meaning: show me it."
///
/// The affording branch is a *preference* flip, not a promise about this
/// exact frame — deliberately: `layout_mode`'s zoom branch outranks it (zoom
/// wins over every other rule, §4.5), so flipping the preference while some
/// other card is zoomed changes nothing on screen until the zoom clears,
/// same as it would once this build actually draws the deck. The status
/// message says "on"/"off" rather than "shown"/"hidden" for exactly that
/// reason — it must never claim an immediate visual effect this build
/// cannot deliver.
///
/// The narrow branch's summon is a different kind of thing — a focus
/// change, not a preference — and it *does* clear [`Model::zoom`], because
/// "show me it" there is a promise about this exact frame: `layout_mode`'s
/// zoom branch answers only `ctx.zoom`, never `ctx.focus`, so a stale zoom
/// would leave the summoned card hidden behind it, a focus that points at a
/// card the frame does not draw. Clearing it here is what keeps that
/// promise regardless of what was zoomed before.
fn toggle_sidebar(model: &mut Model) -> Vec<Cmd> {
    if affords_split(layout::breakpoint(model.viewport_cols)) {
        model.sidebar_visible = !model.sidebar_visible;
        model.info(if model.sidebar_visible {
            "sidebar on"
        } else {
            "sidebar off"
        });
    } else {
        model.zoom = None;
        model.card_focus = Card::Sidebar;
        model.info("sidebar focused");
    }
    Vec::new()
}

/// `\` — the rail (tui.md §4.4). [`toggle_sidebar`]'s exact counterpart;
/// see that function's docs, including why the affording branch says
/// "on"/"off" rather than "shown"/"hidden", and why only the focus-summon
/// branch clears [`Model::zoom`].
fn toggle_rail(model: &mut Model) -> Vec<Cmd> {
    if affords_split(layout::breakpoint(model.viewport_cols)) {
        model.rail_visible = !model.rail_visible;
        model.info(if model.rail_visible {
            "rail on"
        } else {
            "rail off"
        });
    } else {
        model.zoom = None;
        model.card_focus = Card::Rail;
        model.info("rail focused");
    }
    Vec::new()
}

/// `<space>ap` — the collapsible AI panel. No longer bound to `\`, which the
/// rail's own `✦ AI` tab takes over once task 128 renders it — see
/// [`toggle_rail`].
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
    model.set_overlay(Overlay::Quick(QuickPane {
        message_id,
        subject,
        cursor: 0,
    }));
    Vec::new()
}

fn run_quick(model: &mut Model, action: QuickAction) -> Vec<Cmd> {
    let (message_id, subject) = match model.overlay_top() {
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
    model.clear_overlays();
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
    model.set_overlay(Overlay::Outbox(Box::new(OutboxPane {
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
    let target = match model.overlay_top() {
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
        /// A form's highlighted field, or its apply row.
        Field,
        /// The settings screen's highlighted field.
        Setting,
        /// The row under the cursor is a manual link.
        ManualLink,
        /// The highlighted key reference row (task 102).
        Run(Action),
        /// There is no row cursor here, so "use the highlighted row" is
        /// "close this" — what `<enter>` meant on the whole `?` overlay
        /// through task 102, and still means when nothing is highlighted
        /// (an empty filter's match set, or the manual sharing this layer).
        Close,
        Nothing,
    }
    let chosen = match model.overlay_top() {
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
        Some(Overlay::Form(_)) => Chosen::Field,
        Some(Overlay::Help(pane)) => help::selected(pane).map_or(Chosen::Close, Chosen::Run),
        None if model.screen == Screen::Manual => Chosen::ManualLink,
        None if model.screen == Screen::Settings => Chosen::Setting,
        _ => Chosen::Nothing,
    };
    match chosen {
        Chosen::Message(message_id) => open_message_by_id(model, message_id),
        Chosen::Outbox => describe_outbox_row(model),
        Chosen::Quick(action) => run_quick(model, action),
        Chosen::Row(invocation) => run_report_row(model, *invocation),
        Chosen::Field => edit_or_apply_form(model),
        Chosen::Setting => accept_setting(model),
        Chosen::ManualLink => follow_manual_link(model),
        // Not `run_named` for this one action: `open_help_rebind` reads the
        // key reference's own `pane` (the highlighted action, and which
        // mode it is bound in) to build the line it pre-fills, and
        // `run_named`'s whole point is to close the triggering overlay
        // *before* the action runs — which for every other action is
        // exactly right (it should see the screen it is about to reveal,
        // not the overlay asking it to run), but here would mean this
        // action finds the overlay it needs already gone.
        Chosen::Run(Action::HelpRebind) => open_help_rebind(model),
        Chosen::Run(action) => run_named(model, action),
        Chosen::Close => leave(model, Leave::ThenNothing),
        Chosen::Nothing => Vec::new(),
    }
}

/// `<enter>` on a form: open the highlighted field, or apply the whole thing.
///
/// One key for both because [`FormPane::rows`] makes the apply *a row*: `Menu`
/// has one gesture for "use what is highlighted", and a form whose fields and
/// whose commit answered to different keys would be a second vocabulary to
/// learn for one overlay.
fn edit_or_apply_form(model: &mut Model) -> Vec<Cmd> {
    let Some(Overlay::Form(pane)) = model.overlay_top_mut() else {
        return Vec::new();
    };
    if !pane.on_apply() {
        pane.edit();
        return Vec::new();
    }
    // The one rule that matters about this pane, and it lives on the pane: an
    // unfilled form must not replace what it could not read. See
    // `FormPane::blocked`.
    if let Some(why) = pane.blocked() {
        let verb = pane.invocation.verb.join(" ");
        model.fail(format!("{verb}: {why}"));
        return Vec::new();
    }
    let line = pane.line();
    let applied = match pane.apply() {
        Ok(invocation) => invocation,
        // The parser's own complaint about a field's value.
        Err(error) => return refuse_form(model, error.to_string()),
    };
    // The verb's own refusal, asked here as well as by the dispatcher so a value
    // it will not accept is refused *on the form*. `commands::answer` is pure —
    // no overlay, no request, no `Model` — so asking it twice costs nothing and
    // cannot drift from the answer that will actually run.
    let generation = model.generation + 1;
    if let Some(Answer::Refused(why)) = commands::answer(&applied, &target_of(model), generation) {
        return refuse_form(model, why);
    }
    // Down before the invocation runs, for the reason `run_row` takes the
    // report down: every action reads `Model::mode`, and one dispatched with the
    // form still up would be answered by the form's own `Menu` layer.
    model.clear_overlays();
    record_command(model, &line);
    run_invocation(model, applied)
}

/// Keep the form up, with `why` on it.
///
/// The field that caused the refusal is still on screen and still editable,
/// which is the whole point of refusing at the form rather than a round trip
/// later — and of not closing it first.
fn refuse_form(model: &mut Model, why: String) -> Vec<Cmd> {
    let Some(Overlay::Form(pane)) = model.overlay_top_mut() else {
        return Vec::new();
    };
    let verb = pane.invocation.verb.join(" ");
    pane.error = Some(why.clone());
    model.fail(format!("{verb}: {why}"));
    Vec::new()
}

/// The outbox has nothing to open — a scheduled send is not a message in a
/// folder yet — so `Enter` says what the highlighted row's state means
/// instead, which is the only thing anyone reads an outbox for.
fn describe_outbox_row(model: &mut Model) -> Vec<Cmd> {
    let note = match model.overlay_top() {
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
    model.clear_overlays();
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
        /// The key reference's filter (task 102) — `<enter>` stops editing
        /// it and returns to browsing the (already live-filtered) rows.
        HelpFilterDone,
        Nothing,
    }
    // The manual's search line first, and unconditionally: it is the only
    // typing surface that is a screen rather than an overlay, so an overlay
    // cannot be up at the same time as it.
    if model
        .manual
        .as_ref()
        .is_some_and(|manual| manual.typing() && !model.overlay_is_open())
    {
        return submit_manual_search(model);
    }
    let which = match model.overlay_top() {
        Some(Overlay::Search(pane)) if pane.typing() => Which::SearchQuery,
        Some(Overlay::Finder(_)) => Which::Finder,
        Some(Overlay::Command(_)) => Which::Command,
        Some(Overlay::Ask(pane)) if pane.typing() => Which::AskQuestion,
        Some(Overlay::Help(pane)) if pane.editing => Which::HelpFilterDone,
        _ => Which::Nothing,
    };
    match which {
        Which::Nothing => Vec::new(),
        Which::SearchQuery => focus_results(model),
        Which::Finder => activate_finder(model),
        Which::Command => submit_command(model),
        Which::AskQuestion => ask_now(model),
        Which::HelpFilterDone => {
            if let Some(Overlay::Help(pane)) = model.overlay_top_mut() {
                pane.editing = false;
            }
            Vec::new()
        }
    }
}

/// `Tab` — complete the operator being typed in the search box.
///
/// Only the search box: the finder's grammar is sigils, not `key:value`, and
/// completing an operator into it would type text the finder matches
/// literally.
fn prompt_complete(model: &mut Model) -> Vec<Cmd> {
    if matches!(model.overlay_top(), Some(Overlay::Command(_))) {
        return complete_command(model);
    }
    let completed = match model.overlay_top() {
        Some(Overlay::Search(pane)) if pane.typing() => complete_operator(&pane.query),
        _ => None,
    };
    let Some(completed) = completed else {
        return Vec::new();
    };
    if let Some(Overlay::Search(pane)) = model.overlay_top_mut() {
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
    let Some(Overlay::Command(pane)) = model.overlay_top() else {
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
    if let Some(Overlay::Command(pane)) = model.overlay_top_mut() {
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
        // Neither the manual nor the settings screen is about a message, and an
        // action that needs one must say "no message selected" rather than reach
        // behind the page.
        Screen::Manual | Screen::Settings => None,
    }
}

/// The message an action applies to: the one in the viewer when it is open,
/// otherwise the one under the list cursor.
fn target_message(model: &Model) -> Option<i64> {
    match model.screen {
        Screen::Viewer => model.open.as_ref().map(|o| o.id),
        Screen::List => model.current_message().map(|m| m.id),
        Screen::Manual | Screen::Settings => None,
    }
}
