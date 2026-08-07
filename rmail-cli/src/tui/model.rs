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
//! # Multi-key sequences
//!
//! `gg` is the only chord in this shell (task 84 owns the general modal
//! keymap engine). It is modelled as a single [`Model::pending_g`] flag that
//! *any* other key clears before that key is handled normally — a half-typed
//! `g` must never swallow the keystroke that follows it. See
//! `partial_g_does_not_swallow_the_next_key` for the regression proof.

use std::collections::BTreeSet;

pub mod drive;
pub mod wire;

#[cfg(test)]
mod tests;

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

    /// The flag set with `flag` added or removed, deduplicated and ordered so
    /// the result is a function of the desired set and not of arrival order.
    ///
    /// `SetFlags` is a wholesale replace (IMAP `STORE FLAGS` semantics), so a
    /// toggle has to send the complete intended set, not a delta.
    #[must_use]
    pub fn flags_toggled(&self, flag: &str) -> Vec<String> {
        let mut set: BTreeSet<&str> = self.flags.iter().map(String::as_str).collect();
        if !set.remove(flag) {
            set.insert(flag);
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
        /// The message the pick applies to.
        ///
        /// Captured when the overlay opens, never re-derived when it closes:
        /// the message list is live (a `Msg::Changed` reload can arrive and
        /// re-clamp the cursor while the picker is up) and the viewer's
        /// message is not the one under the list cursor at all. Resolving the
        /// target late moved a message the user had not selected.
        message_id: i64,
        /// Cursor within the folder list.
        idx: usize,
    },
    /// Confirm a destructive action.
    Confirm {
        /// What is being asked.
        prompt: String,
        /// The message the answer applies to.
        message_id: i64,
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

/// A key press, in the TUI's own vocabulary rather than crossterm's.
///
/// Decoupled on purpose: the model tests construct key presses directly, and
/// they should not have to build a `crossterm::event::KeyEvent` (with its
/// modifiers, kind and state) to say "the user pressed j".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// A printable character.
    Char(char),
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
    /// Ctrl-C — quits from anywhere, including a modal overlay.
    CtrlC,
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
    /// Whether a bare `g` is waiting for its partner.
    pub pending_g: bool,
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
            pending_g: false,
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

    fn info(&mut self, text: impl Into<String>) {
        self.status = text.into();
        self.level = Level::Info;
    }

    fn fail(&mut self, text: impl Into<String>) {
        self.status = text.into();
        self.level = Level::Error;
    }

    /// Keep both cursors inside their lists after rows arrive or vanish.
    fn clamp(&mut self) {
        self.message_idx = self.message_idx.min(self.messages.len().saturating_sub(1));
        self.folder_idx = self.folder_idx.min(self.folders.len().saturating_sub(1));
    }
}

/// Apply one message to the model and report the work it implies.
///
/// Pure: no I/O, no clock, no terminal. This is the whole of the TUI's
/// behaviour, and the whole of what its tests drive.
pub fn update(model: &mut Model, msg: Msg) -> Vec<Cmd> {
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

/// Route a key press, clearing any half-typed `gg` first.
fn on_key(model: &mut Model, key: Key) -> Vec<Cmd> {
    if key == Key::CtrlC {
        model.quit = true;
        return Vec::new();
    }

    // A pending `g` is consumed by *this* key whatever it is. Taking it
    // before dispatch is what stops `g` followed by `j` from eating the `j`:
    // the chord fails, and `j` is then handled as an ordinary key.
    let pending_g = std::mem::take(&mut model.pending_g);

    if let Some(overlay) = model.overlay.clone() {
        return on_overlay_key(model, overlay, key);
    }

    match model.screen {
        Screen::List => on_list_key(model, key, pending_g),
        Screen::Viewer => on_viewer_key(model, key, pending_g),
    }
}

fn on_overlay_key(model: &mut Model, overlay: Overlay, key: Key) -> Vec<Cmd> {
    match overlay {
        Overlay::Help => {
            if matches!(key, Key::Esc | Key::Char('q') | Key::Char('?') | Key::Enter) {
                model.overlay = None;
            }
            Vec::new()
        }
        Overlay::Pick {
            what,
            message_id,
            idx,
        } => on_pick_key(model, what, message_id, idx, key),
        Overlay::Confirm { message_id, .. } => match key {
            Key::Char('y') | Key::Char('Y') => {
                model.overlay = None;
                model.inflight += 1;
                vec![Cmd::Delete { message_id }]
            }
            Key::Esc | Key::Char('n') | Key::Char('N') | Key::Char('q') => {
                model.overlay = None;
                model.info("cancelled");
                Vec::new()
            }
            _ => Vec::new(),
        },
        Overlay::Input {
            prompt,
            mut buffer,
            what,
            message_id,
        } => match key {
            Key::Esc => {
                model.overlay = None;
                model.info("cancelled");
                Vec::new()
            }
            Key::Enter => {
                model.overlay = None;
                submit_input(model, what, buffer.trim(), message_id)
            }
            Key::Backspace => {
                buffer.pop();
                model.overlay = Some(Overlay::Input {
                    prompt,
                    buffer,
                    what,
                    message_id,
                });
                Vec::new()
            }
            Key::Char(c) => {
                buffer.push(c);
                model.overlay = Some(Overlay::Input {
                    prompt,
                    buffer,
                    what,
                    message_id,
                });
                Vec::new()
            }
            _ => {
                model.overlay = Some(Overlay::Input {
                    prompt,
                    buffer,
                    what,
                    message_id,
                });
                Vec::new()
            }
        },
    }
}

fn on_pick_key(
    model: &mut Model,
    what: PickFor,
    message_id: i64,
    idx: usize,
    key: Key,
) -> Vec<Cmd> {
    let last = model.folders.len().saturating_sub(1);
    match key {
        Key::Esc | Key::Char('q') => {
            model.overlay = None;
            model.info("cancelled");
            Vec::new()
        }
        Key::Char('j') | Key::Down => {
            model.overlay = Some(Overlay::Pick {
                what,
                message_id,
                idx: idx.saturating_add(1).min(last),
            });
            Vec::new()
        }
        Key::Char('k') | Key::Up => {
            model.overlay = Some(Overlay::Pick {
                what,
                message_id,
                idx: idx.saturating_sub(1),
            });
            Vec::new()
        }
        Key::Enter => {
            model.overlay = None;
            let Some(dest) = model.folders.get(idx).cloned() else {
                model.fail("no such folder");
                return Vec::new();
            };
            model.inflight += 1;
            match what {
                PickFor::Copy => vec![Cmd::Copy {
                    message_id,
                    dest_mailbox_id: dest.id,
                }],
                PickFor::Move => vec![Cmd::Move {
                    message_id,
                    dest_mailbox_id: dest.id,
                    label: format!("moved to {}", dest.name),
                }],
            }
        }
        _ => Vec::new(),
    }
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

fn on_list_key(model: &mut Model, key: Key, pending_g: bool) -> Vec<Cmd> {
    match key {
        Key::Char('g') => {
            if pending_g {
                jump_top(model);
            } else {
                model.pending_g = true;
            }
            Vec::new()
        }
        Key::Char('G') => {
            jump_bottom(model);
            Vec::new()
        }
        Key::Char('j') | Key::Down => {
            step(model, 1);
            Vec::new()
        }
        Key::Char('k') | Key::Up => {
            step(model, -1);
            Vec::new()
        }
        Key::Tab => {
            model.focus = match model.focus {
                Focus::Folders => Focus::Messages,
                Focus::Messages => Focus::Folders,
            };
            Vec::new()
        }
        Key::Char('h') => {
            model.focus = Focus::Folders;
            Vec::new()
        }
        Key::Char('l') => {
            model.focus = Focus::Messages;
            Vec::new()
        }
        Key::Enter => match model.focus {
            Focus::Folders => open_folder(model),
            Focus::Messages => open_message(model),
        },
        Key::Char('q') => {
            model.quit = true;
            Vec::new()
        }
        Key::Char('?') => {
            model.overlay = Some(Overlay::Help);
            Vec::new()
        }
        _ => action_key(model, key),
    }
}

fn on_viewer_key(model: &mut Model, key: Key, pending_g: bool) -> Vec<Cmd> {
    let last = model
        .open
        .as_ref()
        .map_or(0, |o| o.body.len().saturating_sub(1));
    match key {
        Key::Char('g') => {
            if pending_g {
                model.scroll = 0;
            } else {
                model.pending_g = true;
            }
            Vec::new()
        }
        Key::Char('G') => {
            model.scroll = last;
            Vec::new()
        }
        Key::Char('j') | Key::Down => {
            model.scroll = model.scroll.saturating_add(1).min(last);
            Vec::new()
        }
        Key::Char('k') | Key::Up => {
            model.scroll = model.scroll.saturating_sub(1);
            Vec::new()
        }
        Key::Char('q') | Key::Esc => {
            model.screen = Screen::List;
            model.open = None;
            model.opening = None;
            Vec::new()
        }
        Key::Char('?') => {
            model.overlay = Some(Overlay::Help);
            Vec::new()
        }
        _ => action_key(model, key),
    }
}

/// The action keys, shared by the list and the viewer so an action means the
/// same thing wherever the message is looked at.
fn action_key(model: &mut Model, key: Key) -> Vec<Cmd> {
    match key {
        Key::Char('a') => archive(model),
        Key::Char('d') => confirm_delete(model),
        Key::Char('s') => toggle_flag(model, SEEN, "read"),
        Key::Char('f') => toggle_flag(model, FLAGGED, "flagged"),
        Key::Char('c') => pick(model, PickFor::Copy),
        Key::Char('M') => pick(model, PickFor::Move),
        Key::Char('r') => reply(model),
        Key::Char('F') => forward(model),
        Key::Char('o') => open_html(model),
        _ => Vec::new(),
    }
}

fn step(model: &mut Model, delta: isize) {
    let (len, idx) = match model.focus {
        Focus::Folders => (model.folders.len(), model.folder_idx),
        Focus::Messages => (model.messages.len(), model.message_idx),
    };
    if len == 0 {
        return;
    }
    let next = if delta < 0 {
        idx.saturating_sub(delta.unsigned_abs())
    } else {
        idx.saturating_add(delta.unsigned_abs()).min(len - 1)
    };
    match model.focus {
        Focus::Folders => model.folder_idx = next,
        Focus::Messages => model.message_idx = next,
    }
}

fn jump_top(model: &mut Model) {
    match model.focus {
        Focus::Folders => model.folder_idx = 0,
        Focus::Messages => model.message_idx = 0,
    }
}

fn jump_bottom(model: &mut Model) {
    match model.focus {
        Focus::Folders => model.folder_idx = model.folders.len().saturating_sub(1),
        Focus::Messages => model.message_idx = model.messages.len().saturating_sub(1),
    }
}

fn open_folder(model: &mut Model) -> Vec<Cmd> {
    let Some(folder) = model.current_folder().cloned() else {
        return Vec::new();
    };
    model.focus = Focus::Messages;
    model.message_idx = 0;
    model.messages.clear();
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
    let Some(message_id) = target_message(model) else {
        model.fail("no message selected");
        return Vec::new();
    };
    let Some(dest) = archive_folder(&model.folders, model.open_folder) else {
        model.fail("no archive folder on this account");
        return Vec::new();
    };
    model.inflight += 1;
    vec![Cmd::Move {
        message_id,
        dest_mailbox_id: dest,
        label: "archived".to_owned(),
    }]
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
    let Some(message_id) = target_message(model) else {
        model.fail("no message selected");
        return Vec::new();
    };
    // `MailService.Delete` marks \Deleted and expunges on the server: the
    // message is gone from the account, not moved to a trash folder. That is
    // not something a stray keystroke should be able to do.
    model.overlay = Some(Overlay::Confirm {
        prompt: "delete permanently (expunges on the server)? [y/N]".to_owned(),
        message_id,
    });
    Vec::new()
}

fn toggle_flag(model: &mut Model, flag: &str, noun: &str) -> Vec<Cmd> {
    let Some(row) = current_row(model).cloned() else {
        model.fail("no message selected");
        return Vec::new();
    };
    let flags = row.flags_toggled(flag);
    let now_set = flags.iter().any(|f| f == flag);
    let label = if now_set {
        format!("marked {noun}")
    } else {
        format!("marked not {noun}")
    };
    model.inflight += 1;
    vec![Cmd::SetFlags {
        message_id: row.id,
        flags,
        label,
    }]
}

fn pick(model: &mut Model, what: PickFor) -> Vec<Cmd> {
    if model.folders.is_empty() {
        model.fail("no folders to pick from");
        return Vec::new();
    }
    let Some(message_id) = target_message(model) else {
        model.fail("no message selected");
        return Vec::new();
    };
    model.overlay = Some(Overlay::Pick {
        what,
        message_id,
        idx: 0,
    });
    Vec::new()
}

fn reply(model: &mut Model) -> Vec<Cmd> {
    let Some(row) = current_row(model).cloned() else {
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
    let Some(message_id) = target_message(model) else {
        model.fail("no message selected");
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

/// The list row an action applies to. The viewer's message is still a row in
/// the list behind it, so flag toggles and replies work identically in both.
fn current_row(model: &Model) -> Option<&MessageRow> {
    let id = target_message(model)?;
    model.messages.iter().find(|m| m.id == id)
}
