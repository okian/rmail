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

use std::collections::BTreeSet;

pub mod drive;
pub mod wire;

#[cfg(test)]
mod tests;

use super::overlays;
use super::overlays::{
    complete_operator, palette_matches, AiSummary, AskPane, AskPhase, Citation, Explanation,
    FinderItem, FinderKind, FinderPane, Hit, OutboxPane, OutboxRow, PalettePane, QuickAction,
    QuickPane, SearchFocus, SearchPane, UndoToast,
};
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
        /// The messages the answer applies to, captured when the question was
        /// asked for the same reason the picker captures its own.
        message_ids: Vec<i64>,
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
    /// `Ctrl-K` — the command palette.
    Palette(Box<PalettePane>),
    /// `A` — the ask pane.
    Ask(Box<AskPane>),
    /// `O` — the outbox pseudo-folder.
    Outbox(Box<OutboxPane>),
    /// `.` — the AI quick-action menu.
    Quick(QuickPane),
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
            Self::Palette(pane) => (pane.cursor, pane.matches.len()),
            Self::Ask(pane) => (pane.cursor, pane.citations.len()),
            Self::Outbox(pane) => (pane.cursor, pane.rows.len()),
            Self::Quick(pane) => (pane.cursor, QuickAction::ALL.len()),
            Self::Help | Self::Pick { .. } | Self::Confirm { .. } | Self::Input { .. } => {
                return None
            }
        })
    }

    /// Put that cursor at `at`. Out-of-range values are the caller's problem;
    /// every caller here clamps first (see `move_cursor`).
    fn set_list_cursor(&mut self, at: usize) {
        match self {
            Self::Search(pane) => pane.cursor = at,
            Self::Finder(pane) => pane.cursor = at,
            Self::Palette(pane) => pane.cursor = at,
            Self::Ask(pane) => pane.cursor = at,
            Self::Outbox(pane) => pane.cursor = at,
            Self::Quick(pane) => pane.cursor = at,
            Self::Help | Self::Pick { .. } | Self::Confirm { .. } | Self::Input { .. } => {}
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
    /// The undo-send countdown, when a scheduled send is still inside its
    /// window.
    pub toast: Option<UndoToast>,
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
            overlay: None,
            ai_panel: false,
            summary: None,
            summary_for: None,
            summary_failed: None,
            summary_pinned: None,
            toast: None,
            generation: 0,
            visual: None,
            keymap: Keymap::defaults(),
            pending: Pending::default(),
            inflight: 0,
            status: "connecting…".to_owned(),
            level: Level::Info,
            quit: false,
            theme: Theme::default(),
        }
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
            Some(Overlay::Finder(_) | Overlay::Palette(_)) => Mode::Prompt,
            Some(Overlay::Search(_) | Overlay::Ask(_) | Overlay::Outbox(_) | Overlay::Quick(_)) => {
                Mode::Menu
            }
            None if self.visual.is_some() => Mode::Visual,
            None => match self.screen {
                Screen::List => Mode::Normal,
                Screen::Viewer => Mode::Viewer,
            },
        }
    }

    /// The rows a visual selection covers, low index first, or `None` when
    /// there is no selection.
    #[must_use]
    pub fn selection(&self) -> Option<(usize, usize)> {
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
                    model.screen = Screen::Viewer;
                    model.info("q back · o open HTML · r reply · ? help");
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
            let Some(toast) = model.toast.as_mut() else {
                return Vec::new();
            };
            toast.remaining = toast.deadline.saturating_sub(now).max(0);
            if toast.remaining == 0 {
                // The window closed. The message is the scheduler's now, and
                // an "undo" offer that no longer works is worse than none.
                model.toast = None;
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
        model.toast = None;
        return Vec::new();
    };
    model.toast = Some(UndoToast {
        outbox_id: row.id,
        to: row.to.clone(),
        deadline,
        remaining: deadline.saturating_sub(now).max(0),
    });
    vec![Cmd::Countdown { until: deadline }]
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
                model.screen = Screen::List;
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
    /// The palette input.
    Palette,
}

/// Apply `edit` to whichever text field is up, and issue whatever the change
/// implies.
fn edit_prompt(model: &mut Model, edit: TextEdit) -> Vec<Cmd> {
    let typed = match model.overlay.as_mut() {
        Some(Overlay::Input { buffer, .. }) => {
            apply_edit(buffer, edit);
            Typed::Nothing
        }
        Some(Overlay::Search(pane)) if pane.typing() => {
            once(apply_edit(&mut pane.query, edit), Typed::Search)
        }
        Some(Overlay::Finder(pane)) => once(apply_edit(&mut pane.query, edit), Typed::Finder),
        Some(Overlay::Palette(pane)) => once(apply_edit(&mut pane.input, edit), Typed::Palette),
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
        Typed::Palette => {
            refresh_palette(model);
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
        Action::PaletteOpen => open_palette(model),
        Action::AskOpen => open_ask(model, String::new()),
        Action::AiPanel => toggle_ai_panel(model),
        Action::AiQuick => open_quick(model),
        Action::OutboxOpen => open_outbox(model),
        Action::OutboxCancel => undo_send(model),
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
    };
    (len > 0).then(|| (idx, len - 1))
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
    }
}

fn move_cursor(model: &mut Model, direction: Direction, count: Option<u32>) -> Vec<Cmd> {
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
        return streams_of(&overlay)
            .iter()
            .map(|which| Cmd::CancelStream { which: *which })
            .collect();
    }
    if model.visual.take().is_some() {
        model.info("selection cleared");
        return Vec::new();
    }
    if model.screen == Screen::Viewer {
        model.screen = Screen::List;
        model.open = None;
        model.opening = None;
        return Vec::new();
    }
    if then == Leave::ThenQuit {
        model.quit = true;
    }
    Vec::new()
}

/// The streams an overlay was feeding on, which closing it should stop.
///
/// The search pane owns two: its own hits, and the why-panel's `Explain`.
fn streams_of(overlay: &Overlay) -> &'static [Stream] {
    match overlay {
        Overlay::Search(_) => &[Stream::Search, Stream::Explain],
        Overlay::Finder(_) => &[Stream::Find],
        Overlay::Ask(_) => &[Stream::Ask],
        Overlay::Help
        | Overlay::Pick { .. }
        | Overlay::Confirm { .. }
        | Overlay::Input { .. }
        | Overlay::Palette(_)
        | Overlay::Outbox(_)
        | Overlay::Quick(_) => &[],
    }
}

// ---------------------------------------------------------------------------
// visual selection
// ---------------------------------------------------------------------------

fn toggle_visual(model: &mut Model) -> Vec<Cmd> {
    if model.visual.take().is_some() {
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
    if model.visual.is_some() {
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
    if model.screen == Screen::Viewer {
        return Vec::new();
    }
    if model.visual.is_some() {
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
    let message_ids = match model.overlay.take() {
        Some(Overlay::Confirm { message_ids, .. }) => message_ids,
        other => {
            model.overlay = other;
            return Vec::new();
        }
    };
    model.inflight += message_ids.len();
    message_ids
        .into_iter()
        .map(|message_id| Cmd::Delete { message_id })
        .collect()
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
        message_ids: ids,
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

/// `Ctrl-K` — the command palette.
fn open_palette(model: &mut Model) -> Vec<Cmd> {
    if !screen_is_clear(model) {
        return Vec::new();
    }
    model.overlay = Some(Overlay::Palette(Box::default()));
    refresh_palette(model);
    model.info("palette — type a command, Enter runs it");
    Vec::new()
}

fn refresh_palette(model: &mut Model) {
    let Some(Overlay::Palette(pane)) = model.overlay.as_ref() else {
        return;
    };
    let matches = palette_matches(&pane.input.clone(), &model.keymap);
    if let Some(Overlay::Palette(pane)) = model.overlay.as_mut() {
        pane.matches = matches;
        pane.cursor = pane.cursor.min(pane.matches.len().saturating_sub(1));
    }
}

/// `Enter` in the palette: run the highlighted command.
fn run_palette(model: &mut Model) -> Vec<Cmd> {
    let chosen = match model.overlay.as_ref() {
        Some(Overlay::Palette(pane)) => pane.entry().map(|entry| entry.action),
        _ => return Vec::new(),
    };
    let Some(action) = chosen else {
        model.fail("no command matches");
        return Vec::new();
    };
    run_command(model, action)
}

/// Run `id` as a command, if this build has one by that name.
fn run_command_id(model: &mut Model, id: &str) -> Vec<Cmd> {
    let Some(action) = Action::from_id(id) else {
        model.fail(format!("this build has no command {id:?}"));
        return Vec::new();
    };
    run_command(model, action)
}

/// Close the overlay, then do the named thing.
///
/// Closing first is not tidiness: every action reads `Model::mode()`, and one
/// run against a palette that is still up would ask the *palette* what
/// `cursor.down` means rather than the screen it is about to reveal.
fn run_command(model: &mut Model, action: Action) -> Vec<Cmd> {
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
        _ => model
            .toast
            .as_ref()
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
    if model
        .toast
        .as_ref()
        .is_some_and(|toast| toast.outbox_id == outbox_id)
    {
        model.toast = None;
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
        _ => Chosen::Nothing,
    };
    match chosen {
        Chosen::Message(message_id) => open_message_by_id(model, message_id),
        Chosen::Outbox => describe_outbox_row(model),
        Chosen::Quick(action) => run_quick(model, action),
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
    model.screen = Screen::List;
    open_folder(model)
}

/// `Enter` on a typing overlay's prompt.
fn prompt_accept(model: &mut Model) -> Vec<Cmd> {
    enum Which {
        SearchQuery,
        Finder,
        Palette,
        AskQuestion,
        Nothing,
    }
    let which = match model.overlay.as_ref() {
        Some(Overlay::Search(pane)) if pane.typing() => Which::SearchQuery,
        Some(Overlay::Finder(_)) => Which::Finder,
        Some(Overlay::Palette(_)) => Which::Palette,
        Some(Overlay::Ask(pane)) if pane.typing() => Which::AskQuestion,
        _ => Which::Nothing,
    };
    match which {
        Which::SearchQuery => focus_results(model),
        Which::Finder => activate_finder(model),
        Which::Palette => run_palette(model),
        Which::AskQuestion => ask_now(model),
        Which::Nothing => Vec::new(),
    }
}

/// `Tab` — complete the operator being typed in the search box.
///
/// Only the search box: the finder's grammar is sigils, not `key:value`, and
/// completing an operator into it would type text the finder matches
/// literally.
fn prompt_complete(model: &mut Model) -> Vec<Cmd> {
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
    }
}

/// The message an action applies to: the one in the viewer when it is open,
/// otherwise the one under the list cursor.
fn target_message(model: &Model) -> Option<i64> {
    match model.screen {
        Screen::Viewer => model.open.as_ref().map(|o| o.id),
        Screen::List => model.current_message().map(|m| m.id),
    }
}
