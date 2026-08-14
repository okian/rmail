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
            visual: None,
            keymap: Keymap::defaults(),
            pending: Pending::default(),
            inflight: 0,
            status: "connecting…".to_owned(),
            level: Level::Info,
            quit: false,
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
    let cmds = dispatch(model, msg);
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
            model.info("loading accounts…");
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
                    model.inflight += 1;
                    // The event stream starts here and runs for the whole
                    // session: it is how the list stays current without the
                    // TUI polling, and it is a read of local state, so it
                    // costs the daemon nothing to keep open.
                    vec![Cmd::LoadFolders { account_id }, Cmd::Watch { account_id }]
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
    if mode != Mode::Insert {
        return Vec::new();
    }
    let Key::Char(c) = key else {
        return Vec::new();
    };
    if let Some(Overlay::Input { buffer, .. }) = model.overlay.as_mut() {
        // Bounded: the prompt collects an address, and a key held down
        // against it must not grow a `String` for as long as it is leaned on.
        if buffer.chars().count() < MAX_INPUT {
            buffer.push(c);
        }
    }
    Vec::new()
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
        // be a lie; the other three were.
        if !matches!(overlay, Overlay::Help) {
            model.info("cancelled");
        }
        return Vec::new();
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
    if let Some(Overlay::Input { buffer, .. }) = model.overlay.as_mut() {
        buffer.pop();
    }
    Vec::new()
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

/// The message an action applies to: the one in the viewer when it is open,
/// otherwise the one under the list cursor.
fn target_message(model: &Model) -> Option<i64> {
    match model.screen {
        Screen::Viewer => model.open.as_ref().map(|o| o.id),
        Screen::List => model.current_message().map(|m| m.id),
    }
}
