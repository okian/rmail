//! The sync orchestrator: what the daemon actually drives.
//!
//! The engines below it each answer one question — [`crate::sync::full`] "what
//! do I not have?", [`crate::sync::delta`] "what changed?", [`crate::sync::idle`]
//! "when should I ask?". This module is what turns those into an operation an
//! RPC can name: connect to an account, run a pass over one folder or all of
//! them, record what changed in the durable log, and hand back a summary.
//!
//! # Why events are written here rather than in the engines
//!
//! The engines report changes through a [`ChangeSink`] and know nothing about
//! the log. That keeps them testable without a database of events and, more
//! importantly, keeps the *transaction boundary* honest: a change is a fact
//! about the mailbox the moment its own transaction commits, and the event
//! describing it is a separate durable write. Conflating them would mean an
//! event log that could roll back a message.
//!
//! # Pause is a promise about the next boundary, not the current instant
//!
//! [`SyncEngine::pause`] cancels the account's in-flight work and refuses to
//! start more. It does not abandon an IMAP command mid-flight — see
//! [`crate::sync::full`] for why that would leave a session that cannot be
//! reused. So "paused" means "stopping at the next safe point and starting
//! nothing new", which is what a user pressing pause actually wants.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use crate::error::Error;
use crate::events::{EventKind, EventLog, NewEvent};
use crate::imap::{conn, ImapCapabilities};
use crate::repo;
use crate::storage::Database;

use super::{full, Change, ChangeSink, SyncOptions};

/// What a pass should do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SyncMode {
    /// Let the engine choose: the initial walk for a folder with no baseline,
    /// the cheapest delta strategy for one that has it.
    #[default]
    Auto,
    /// Force the initial UID-window walk even where a delta would do.
    Full,
}

/// What one folder's pass did.
#[derive(Debug, Clone)]
pub struct FolderOutcome {
    /// The mailbox synced.
    pub mailbox_id: i64,
    /// Its name, so a caller need not look it up again.
    pub name: String,
    /// How the server was asked.
    pub strategy: String,
    /// Messages newly downloaded.
    pub new_messages: u64,
    /// Messages whose flags the server contradicted.
    pub flag_updates: u64,
    /// Messages removed because the server no longer has them.
    pub expunged: u64,
    /// Why the folder failed, if it did. One bad folder does not stop the rest.
    pub error: Option<String>,
}

/// What a whole pass did.
#[derive(Debug, Clone, Default)]
pub struct PassReport {
    /// Per-folder outcomes, in the order they were visited.
    pub folders: Vec<FolderOutcome>,
    /// The log position after the pass, so a client can watch from exactly
    /// here rather than from wherever a stream happens to start.
    pub latest_seq: i64,
}

/// Records what a pass changed into the durable log.
///
/// Hands events to a drain task over a channel rather than writing them
/// inline. [`ChangeSink::changed`] is synchronous — it is called from the
/// middle of a fetch loop — so it cannot await a commit, and accumulating a
/// whole initial sync's worth of events before writing any would both delay
/// every watcher and hold them all in memory. The channel is unbounded because
/// the alternative under pressure is dropping an event, and an event log that
/// drops is not a log.
struct LogSink {
    tx: tokio::sync::mpsc::UnboundedSender<NewEvent>,
    account_id: i64,
    mailbox_id: i64,
}

/// How many events one write batches.
///
/// Small enough that a watcher sees a busy folder's mail promptly, large enough
/// that an initial sync is not one commit per message.
const FLUSH_EVERY: usize = 256;

impl LogSink {
    fn new(
        account_id: i64,
        mailbox_id: i64,
        tx: tokio::sync::mpsc::UnboundedSender<NewEvent>,
    ) -> Self {
        Self {
            tx,
            account_id,
            mailbox_id,
        }
    }
}

impl ChangeSink for LogSink {
    fn changed(&mut self, change: Change) {
        let event = match change {
            Change::Added { message_id, uid } => NewEvent::new(EventKind::NewMail)
                .account(self.account_id)
                .mailbox(self.mailbox_id)
                .message(message_id)
                .payload(serde_json::json!({ "uid": uid })),
            Change::FlagsChanged {
                message_id,
                uid,
                flags,
            } => NewEvent::new(EventKind::FlagChanged)
                .account(self.account_id)
                .mailbox(self.mailbox_id)
                .message(message_id)
                .payload(serde_json::json!({ "uid": uid, "flags": flags })),
            // The message id is recorded even though the row is gone: a
            // consumer that indexed it needs to know which one to drop.
            Change::Removed { message_id, uid } => NewEvent::new(EventKind::Deleted)
                .account(self.account_id)
                .mailbox(self.mailbox_id)
                .message(message_id)
                .payload(serde_json::json!({ "uid": uid })),
        };
        // A send error means the drain task is gone, which only happens once
        // the pass is over. There is nothing useful to do with the event then,
        // and failing the sync over a lost notification would be worse than
        // the missing notification.
        let _ = self.tx.send(event);
    }
}

/// Drives synchronization for the daemon.
///
/// Cheap to clone: every clone shares the database, the log, and the pause set.
#[derive(Clone)]
pub struct SyncEngine {
    db: Database,
    events: EventLog,
    opts: SyncOptions,
    /// Cancellation tokens for accounts currently paused, plus the token any
    /// in-flight pass for that account is running under.
    state: Arc<Mutex<HashMap<i64, AccountState>>>,
}

#[derive(Debug, Default)]
struct AccountState {
    paused: bool,
    /// Cancels whatever pass is running for this account.
    running: Option<CancellationToken>,
}

impl std::fmt::Debug for SyncEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyncEngine").finish_non_exhaustive()
    }
}

impl SyncEngine {
    /// Build an engine over a database and event log.
    #[must_use]
    pub fn new(db: Database, events: EventLog, opts: SyncOptions) -> Self {
        Self {
            db,
            events,
            opts,
            state: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// The durable log this engine writes to.
    #[must_use]
    pub fn events(&self) -> &EventLog {
        &self.events
    }

    /// Stop syncing `account_id` until [`Self::resume`].
    ///
    /// Cancels any pass already running, which stops it at its next safe
    /// boundary rather than abandoning an IMAP command mid-flight.
    pub fn pause(&self, account_id: i64) {
        let Ok(mut state) = self.state.lock() else {
            // A poisoned lock means a panic while holding it. Refusing to pause
            // is worse than the alternative here: leave the flag alone and let
            // the caller see the unchanged state from `is_paused`.
            tracing::error!(account_id, "sync state lock poisoned; pause ignored");
            return;
        };
        let entry = state.entry(account_id).or_default();
        entry.paused = true;
        if let Some(token) = entry.running.take() {
            token.cancel();
        }
        tracing::info!(account_id, "sync paused");
    }

    /// Allow `account_id` to sync again.
    pub fn resume(&self, account_id: i64) {
        let Ok(mut state) = self.state.lock() else {
            tracing::error!(account_id, "sync state lock poisoned; resume ignored");
            return;
        };
        state.entry(account_id).or_default().paused = false;
        tracing::info!(account_id, "sync resumed");
    }

    /// Whether `account_id` is paused.
    #[must_use]
    pub fn is_paused(&self, account_id: i64) -> bool {
        self.state
            .lock()
            .map(|state| state.get(&account_id).is_some_and(|s| s.paused))
            .unwrap_or(false)
    }

    /// Register a cancellation token as this account's in-flight pass, refusing
    /// if the account is paused.
    fn begin(&self, account_id: i64, token: &CancellationToken) -> Result<(), Error> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| Error::internal("sync state lock poisoned"))?;
        let entry = state.entry(account_id).or_default();
        if entry.paused {
            return Err(Error::failed_precondition(format!(
                "account {account_id} is paused"
            )));
        }
        entry.running = Some(token.clone());
        Ok(())
    }

    fn finish(&self, account_id: i64) {
        if let Ok(mut state) = self.state.lock() {
            if let Some(entry) = state.get_mut(&account_id) {
                entry.running = None;
            }
        }
    }

    /// Sync one folder, or every folder of an account when `mailbox_id` is
    /// `None`.
    ///
    /// Connects, runs the pass, records what changed in the log, and returns a
    /// summary. A folder that fails is reported in its own
    /// [`FolderOutcome::error`] and does not stop the others.
    ///
    /// # Errors
    ///
    /// - [`Error::FailedPrecondition`] if the account is paused or has no
    ///   credential configured.
    /// - [`Error::NotFound`] if the account or mailbox does not exist.
    /// - [`Error::Unauthenticated`]/[`Error::Unavailable`] if the server
    ///   rejects the login or is unreachable.
    /// - A mapped storage error.
    #[tracing::instrument(skip(self, cancel), fields(mode = ?mode))]
    pub async fn sync(
        &self,
        account_id: i64,
        mailbox_id: Option<i64>,
        mode: SyncMode,
        cancel: &CancellationToken,
    ) -> Result<PassReport, Error> {
        // The pass runs under a token that is both the caller's cancellation
        // and the account's pause switch, so a Pause RPC stops work already in
        // flight rather than only the next thing to start.
        let token = cancel.child_token();
        self.begin(account_id, &token)?;
        let outcome = self.run_pass(account_id, mailbox_id, mode, &token).await;
        self.finish(account_id);
        outcome
    }

    async fn run_pass(
        &self,
        account_id: i64,
        mailbox_id: Option<i64>,
        mode: SyncMode,
        cancel: &CancellationToken,
    ) -> Result<PassReport, Error> {
        let (mut session, capabilities) = conn::connect_account(&self.db, account_id).await?;

        // QRESYNC is a session-level switch and this is the only moment the
        // session has no mailbox selected.
        let capabilities =
            super::delta::enable_qresync(&mut session, capabilities, self.opts.window_timeout)
                .await;

        let mailboxes = match mailbox_id {
            Some(id) => {
                let mailbox = self
                    .db
                    .read(move |c| repo::get_mailbox(c, id))
                    .await?
                    .filter(|m| m.account_id == account_id)
                    .ok_or_else(|| {
                        Error::not_found(format!("mailbox {id} not found for account {account_id}"))
                    })?;
                vec![mailbox]
            }
            None => {
                let all = self
                    .db
                    .read(move |c| repo::list_mailboxes(c, account_id))
                    .await?;
                full::prioritize(all)
            }
        };

        let mut report = PassReport::default();
        for mailbox in mailboxes {
            if cancel.is_cancelled() {
                break;
            }
            report.folders.push(
                self.sync_one(
                    &mut session,
                    account_id,
                    &mailbox,
                    mode,
                    cancel,
                    &capabilities,
                )
                .await,
            );
        }

        // Best-effort logout; the session is dropped regardless.
        let _ = session.logout().await;

        report.latest_seq = self.events.latest_seq().await?.unwrap_or(0);
        Ok(report)
    }

    async fn sync_one<T: conn::ImapStream>(
        &self,
        session: &mut async_imap::Session<T>,
        account_id: i64,
        mailbox: &repo::Mailbox,
        mode: SyncMode,
        cancel: &CancellationToken,
        capabilities: &ImapCapabilities,
    ) -> FolderOutcome {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let drain = tokio::spawn({
            let events = self.events.clone();
            async move {
                let mut batch = Vec::with_capacity(FLUSH_EVERY);
                loop {
                    let received = rx.recv_many(&mut batch, FLUSH_EVERY).await;
                    if received == 0 {
                        break;
                    }
                    if let Err(error) = events.append_all(std::mem::take(&mut batch)).await {
                        // A log write that fails must not undo mail that has
                        // already landed: the mailbox is correct either way,
                        // and the next pass re-derives what it could not
                        // record from the same durable state.
                        tracing::warn!(%error, received, "could not record sync changes");
                    }
                }
            }
        });
        let mut sink = LogSink::new(account_id, mailbox.id, tx);
        let mut outcome = FolderOutcome {
            mailbox_id: mailbox.id,
            name: mailbox.name.clone(),
            strategy: "full".to_owned(),
            new_messages: 0,
            flag_updates: 0,
            expunged: 0,
            error: None,
        };

        let result = match mode {
            SyncMode::Full => full::sync_folder(
                session,
                &self.db,
                mailbox.id,
                self.opts,
                cancel,
                |_| {},
                &mut sink,
            )
            .await
            .map(|r| {
                outcome.new_messages = r.fetched;
                outcome.expunged = r.purged_stale;
            }),
            SyncMode::Auto => super::delta::delta_sync(
                session,
                &self.db,
                mailbox.id,
                *capabilities,
                self.opts,
                cancel,
                &mut sink,
            )
            .await
            .map(|r| {
                outcome.strategy = r.strategy.as_str().to_owned();
                outcome.new_messages = r.new_messages;
                outcome.flag_updates = r.flag_updates;
                outcome.expunged = r.expunged;
            }),
        };

        // Close the channel and wait for the drain, so every change this pass
        // observed is durable before the pass reports itself finished. A pass
        // that failed halfway still applied the changes it got to, and events
        // describing applied changes belong in the log.
        drop(sink);
        if let Err(error) = drain.await {
            tracing::warn!(%error, "the event drain task failed");
        }

        if let Err(error) = result {
            tracing::warn!(folder = %mailbox.name, %error, "folder sync failed");
            outcome.error = Some(error.to_string());
        }

        let progress = NewEvent::new(EventKind::SyncState)
            .account(account_id)
            .mailbox(mailbox.id)
            .payload(serde_json::json!({
                "folder": mailbox.name,
                "strategy": outcome.strategy,
                "new_messages": outcome.new_messages,
                "flag_updates": outcome.flag_updates,
                "expunged": outcome.expunged,
                "error": outcome.error,
            }));
        if let Err(error) = self.events.append(progress).await {
            tracing::warn!(%error, "could not record sync progress");
        }

        outcome
    }

    /// Per-folder sync state for an account.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    pub async fn status(&self, account_id: i64) -> Result<Vec<FolderStatus>, Error> {
        let mailboxes = self
            .db
            .read(move |c| repo::list_mailboxes(c, account_id))
            .await?;
        let mut out = Vec::with_capacity(mailboxes.len());
        for mailbox in mailboxes {
            let id = mailbox.id;
            let (state, message_count) = self
                .db
                .read(move |c| {
                    let state = repo::get_sync_state(c, id)?;
                    let count: i64 = c.query_row(
                        "SELECT count(*) FROM messages WHERE mailbox_id = ?1",
                        [id],
                        |row| row.get(0),
                    )?;
                    Ok((state, count))
                })
                .await?;
            out.push(FolderStatus {
                mailbox_id: mailbox.id,
                name: mailbox.name,
                uidvalidity: mailbox.uidvalidity,
                uidnext: mailbox.uidnext,
                highestmodseq: state.as_ref().and_then(|s| s.highestmodseq),
                last_synced_uid: state.as_ref().and_then(|s| s.last_synced_uid),
                walked_down_to: state.as_ref().and_then(|s| s.walked_down_to),
                full_sync_done: state.as_ref().is_some_and(|s| s.full_sync_done),
                last_sync_at: state.as_ref().and_then(|s| s.last_sync_at),
                message_count,
            });
        }
        Ok(out)
    }
}

/// One folder's durable sync state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderStatus {
    /// The mailbox.
    pub mailbox_id: i64,
    /// Its name.
    pub name: String,
    /// Last-seen UIDVALIDITY.
    pub uidvalidity: Option<i64>,
    /// Last-seen UIDNEXT.
    pub uidnext: Option<i64>,
    /// The delta checkpoint.
    pub highestmodseq: Option<i64>,
    /// High-water mark of the initial walk.
    pub last_synced_uid: Option<i64>,
    /// Low-water mark of the initial walk.
    pub walked_down_to: Option<i64>,
    /// Whether the initial walk reached the bottom.
    pub full_sync_done: bool,
    /// When the folder was last checked (unix seconds).
    pub last_sync_at: Option<i64>,
    /// Messages stored locally.
    pub message_count: i64,
}

#[cfg(test)]
mod tests;
