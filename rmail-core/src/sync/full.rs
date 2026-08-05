//! Initial (full) folder sync.
//!
//! A folder is walked **downward by UID window** from the server's `UIDNEXT`,
//! so the newest mail lands first and a mailbox is useful long before the sync
//! finishes. Each window is one `UID FETCH`, persisted by
//! [`crate::message::fetch::fetch_and_persist`], which overlaps downloading and
//! writing across a bounded channel.
//!
//! **The walk is driven by two water marks**, not by which UIDs happen to be
//! stored. A real folder's UID space is mostly holes — every expunge leaves one
//! permanently — so "no row for UID 7" cannot distinguish "not fetched yet"
//! from "does not exist". Deriving the walk from the data alone would make a
//! fully-synced mailbox re-request its entire UID space forever. Instead
//! `sync_state` carries:
//!
//! - `last_synced_uid` — the **high** mark; everything above it is new mail,
//!   and on a completed folder that is the only range a run looks at (so an
//!   up-to-date folder issues zero `FETCH`es);
//! - `walked_down_to` — the **low** mark; the backlog resumes just below it.
//!
//! Together they say the walk has covered `[walked_down_to, last_synced_uid]`
//! contiguously. Stored UIDs are still consulted, but only to drop UIDs from
//! the single window being requested, which is what makes a crash mid-window
//! cost at most one window of re-fetching.
//!
//! **Bounded work.** The window bounds each round trip and how much a crash
//! costs; [`crate::message::fetch`]'s pipeline bounds how many messages are in
//! flight between socket and database. Cancellation is honored at window
//! boundaries — the only point where the IMAP session is in a clean state — and
//! each window carries its own timeout.

use std::collections::BTreeSet;
use std::time::Duration;

use async_imap::Session;
use tokio_util::sync::CancellationToken;

use super::{Change, ChangeSink};
use crate::error::Error;
use crate::imap::conn::ImapStream;
use crate::message::fetch::fetch_and_persist;
use crate::repo;
use crate::storage::Database;

/// Default UIDs per window.
pub const DEFAULT_WINDOW: u32 = 200;

/// Bounds on the window size. Too small wastes round trips; too large builds a
/// UID-set command line that real servers reject with `BAD`.
const MIN_WINDOW: u32 = 1;
const MAX_WINDOW: u32 = 2_000;

/// Default per-window deadline.
pub const DEFAULT_WINDOW_TIMEOUT: Duration = Duration::from_secs(120);

/// Tuning for a full sync.
#[derive(Debug, Clone, Copy)]
pub struct SyncOptions {
    /// UIDs per `FETCH` window, clamped to a sane range. Larger windows mean
    /// fewer round trips; smaller windows mean finer progress and less repeated
    /// work after a crash.
    pub window: u32,
    /// How long one window may take before the run gives up. A window that
    /// elapses aborts the run — the session is left mid-command and must be
    /// dropped by the caller.
    pub window_timeout: Duration,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            window: DEFAULT_WINDOW,
            window_timeout: DEFAULT_WINDOW_TIMEOUT,
        }
    }
}

impl SyncOptions {
    /// The window size actually used, clamped.
    #[must_use]
    pub fn effective_window(&self) -> i64 {
        i64::from(self.window.clamp(MIN_WINDOW, MAX_WINDOW))
    }
}

/// A progress observation, emitted once per window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncProgress {
    /// The mailbox being synced.
    pub mailbox_id: i64,
    /// Messages the server reports in the folder (`EXISTS`).
    pub total: i64,
    /// Messages newly persisted by this run so far.
    pub fetched: u64,
    /// Messages this run skipped because they were already stored.
    pub already_present: u64,
    /// The lowest UID this run has walked down to.
    pub cursor_uid: i64,
    /// The highest UID in the folder's UID space (`UIDNEXT - 1`).
    pub ceiling_uid: i64,
    /// Whether the walk has reached the bottom of the UID space.
    pub done: bool,
}

/// The outcome of syncing one folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncReport {
    /// The mailbox synced.
    pub mailbox_id: i64,
    /// The UIDVALIDITY the run was keyed to.
    pub uidvalidity: i64,
    /// Messages newly persisted.
    pub fetched: u64,
    /// Messages skipped because they were already stored.
    pub already_present: u64,
    /// Windows that required a `FETCH`. Zero means there was nothing to do —
    /// the incremental no-op.
    pub windows_fetched: u64,
    /// Whether the walk reached the bottom of the UID space.
    pub complete: bool,
    /// Whether the run stopped early because it was cancelled.
    pub cancelled: bool,
    /// Rows dropped because the server re-keyed the UID space (see
    /// [`SyncReport::uidvalidity_changed`]).
    pub purged_stale: u64,
    /// Whether the server's UIDVALIDITY differs from the stored checkpoint.
    pub uidvalidity_changed: bool,
}

/// One folder's failure inside an account-wide sync.
#[derive(Debug)]
pub struct FolderFailure {
    /// The mailbox that failed.
    pub mailbox_id: i64,
    /// Its name, for logs and messages.
    pub name: String,
    /// Why it failed.
    pub error: Error,
}

/// The outcome of syncing every folder of an account.
#[derive(Debug, Default)]
pub struct AccountSyncReport {
    /// Folders that synced, in the order they were visited.
    pub reports: Vec<SyncReport>,
    /// Folders that failed. One bad folder does not stop the others.
    pub failures: Vec<FolderFailure>,
}

/// Sync one folder in full, newest UID first.
///
/// Emits a [`SyncProgress`] per window through `on_progress` and checkpoints
/// `sync_state` as it goes. A folder with nothing new fetches nothing.
/// `cancel` is observed at window boundaries; a cancelled run returns its
/// partial report with [`SyncReport::cancelled`] set rather than an error.
///
/// # Errors
///
/// - [`Error::NotFound`] if `mailbox_id` does not exist or the server cannot
///   select the folder.
/// - [`Error::Unavailable`] if the server does not report
///   `UIDVALIDITY`/`UIDNEXT`, or the connection breaks mid-walk.
/// - [`Error::DeadlineExceeded`] if a window exceeds
///   [`SyncOptions::window_timeout`]. The session is then mid-command and must
///   be dropped.
/// - A mapped storage error if persistence fails.
#[tracing::instrument(
    skip(session, db, opts, cancel, on_progress, sink),
    fields(folder, uidvalidity)
)]
pub async fn sync_folder<T, F>(
    session: &mut Session<T>,
    db: &Database,
    mailbox_id: i64,
    opts: SyncOptions,
    cancel: &CancellationToken,
    mut on_progress: F,
    sink: &mut impl ChangeSink,
) -> Result<SyncReport, Error>
where
    T: ImapStream,
    F: FnMut(SyncProgress),
{
    let mailbox = db
        .read(move |c| repo::get_mailbox(c, mailbox_id))
        .await?
        .ok_or_else(|| Error::not_found(format!("mailbox {mailbox_id} not found")))?;
    let account_id = mailbox.account_id;
    tracing::Span::current().record("folder", tracing::field::display(&mailbox.name));

    let selected = session
        .select(&mailbox.name)
        .await
        .map_err(|e| super::select_error(&mailbox.name, e))?;

    // Both are load-bearing: UIDVALIDITY keys the UID space, UIDNEXT bounds the
    // walk. A server reporting neither cannot be synced by UID window.
    let uidvalidity = i64::from(
        selected
            .uid_validity
            .ok_or_else(|| Error::unavailable("server did not report UIDVALIDITY on SELECT"))?,
    );
    let uidnext = i64::from(
        selected
            .uid_next
            .ok_or_else(|| Error::unavailable("server did not report UIDNEXT on SELECT"))?,
    );
    let total = i64::from(selected.exists);
    let ceiling = uidnext - 1;
    tracing::Span::current().record("uidvalidity", uidvalidity);

    db.write(move |c| repo::update_mailbox_uid_state(c, mailbox_id, uidvalidity, uidnext))
        .await?;

    let previous = db
        .read(move |c| repo::get_sync_state(c, mailbox_id))
        .await?;
    let stored_uidvalidity = previous.as_ref().and_then(|state| state.uidvalidity);
    let uidvalidity_changed = stored_uidvalidity.is_some_and(|stored| stored != uidvalidity);

    // A UIDVALIDITY bump invalidates the whole UID space. The old rows are no
    // longer addressable and their UIDs may now belong to different messages,
    // so leaving them would show the user a mailbox with every message twice.
    // Drop them and rebuild — the same safe resync [`crate::sync::delta`]
    // performs when it sees the bump first.
    let purged_stale = if uidvalidity_changed {
        tracing::warn!(
            from = stored_uidvalidity,
            to = uidvalidity,
            "UIDVALIDITY changed; dropping the stale local copy of this folder"
        );
        purge_other_uidvalidity(db, mailbox_id, uidvalidity, sink).await?
    } else {
        0
    };

    // Where the walk stands in *this* UID space.
    let same_space = !uidvalidity_changed;
    let high_water = if same_space {
        previous
            .as_ref()
            .and_then(|s| s.last_synced_uid)
            .unwrap_or(0)
    } else {
        0
    };
    let low_water = if same_space {
        previous.as_ref().and_then(|s| s.walked_down_to)
    } else {
        None
    };

    // Newest first: the new mail above the high mark, then the remaining
    // backlog below the low mark.
    let mut ranges: Vec<(i64, i64)> = Vec::new();
    if ceiling > high_water {
        ranges.push((high_water + 1, ceiling));
    }
    match low_water {
        Some(low) if low > 1 => ranges.push((1, low - 1)),
        Some(_) => {}
        None if high_water >= 1 => ranges.push((1, high_water.min(ceiling))),
        None => {}
    }

    let window = opts.effective_window();
    let mut fetched = 0u64;
    let mut already_present = 0u64;
    let mut windows_fetched = 0u64;
    let mut windows_visited = 0u64;
    let mut covered_low = low_water;
    let covered_high = high_water.max(ceiling);
    let mut cancelled = false;

    'walk: for (range_low, range_high) in ranges {
        let mut high = range_high;
        while high >= range_low {
            if cancel.is_cancelled() {
                tracing::info!("sync cancelled at a window boundary");
                cancelled = true;
                break 'walk;
            }
            let low = (high - window + 1).max(range_low);

            let stored: BTreeSet<i64> = db
                .read(move |c| repo::list_message_uids(c, mailbox_id, uidvalidity, low, high))
                .await?
                .into_iter()
                .collect();
            already_present += stored.len() as u64;
            let missing: Vec<i64> = (low..=high).filter(|uid| !stored.contains(uid)).collect();

            if !missing.is_empty() {
                let set = format_uid_set(&missing);
                let outcomes = tokio::time::timeout(
                    opts.window_timeout,
                    fetch_and_persist(session, db, account_id, mailbox_id, uidvalidity, &set),
                )
                .await
                .map_err(|_| {
                    Error::deadline_exceeded(format!(
                        "IMAP fetch of UIDs {set} exceeded {:?}",
                        opts.window_timeout
                    ))
                })??;
                for outcome in &outcomes {
                    if outcome.inserted {
                        fetched += 1;
                        sink.changed(Change::Added {
                            message_id: outcome.message_id,
                            uid: outcome.uid,
                        });
                    }
                }
                windows_fetched += 1;
                tracing::debug!(low, high, persisted = outcomes.len(), "window synced");
            }

            windows_visited += 1;
            covered_low = Some(covered_low.map_or(low, |current| current.min(low)));
            checkpoint(db, mailbox_id, uidvalidity, covered_high, covered_low).await?;
            on_progress(SyncProgress {
                mailbox_id,
                total,
                fetched,
                already_present,
                cursor_uid: low,
                ceiling_uid: ceiling,
                done: covered_low == Some(1),
            });

            if low == range_low {
                break;
            }
            high = low - 1;
        }
    }

    if windows_visited == 0 && !cancelled {
        // Nothing to walk: an empty folder, or one already synced with no new
        // mail. Still record the checkpoint so "last checked" advances and a
        // caller sees one done observation.
        covered_low = Some(covered_low.unwrap_or(1));
        checkpoint(db, mailbox_id, uidvalidity, covered_high, covered_low).await?;
        on_progress(SyncProgress {
            mailbox_id,
            total,
            fetched: 0,
            already_present: 0,
            cursor_uid: covered_low.unwrap_or(1),
            ceiling_uid: ceiling,
            done: true,
        });
    }

    let complete = covered_low == Some(1);
    tracing::info!(
        fetched,
        already_present,
        windows_fetched,
        total,
        complete,
        cancelled,
        "full folder sync finished"
    );
    Ok(SyncReport {
        mailbox_id,
        uidvalidity,
        fetched,
        already_present,
        windows_fetched,
        complete,
        cancelled,
        purged_stale,
        uidvalidity_changed,
    })
}

/// Sync every selectable folder of an account over one session, in the order
/// [`prioritize`] gives — INBOX first, so the view a user opens is populated
/// before the archive is.
///
/// A folder that fails is recorded and the run continues: one stale mailbox row
/// must not stop every other folder from ever syncing. A cancellation stops the
/// remaining folders.
///
/// # Errors
///
/// Only storage errors reading the folder list; per-folder failures are
/// collected into [`AccountSyncReport::failures`].
#[tracing::instrument(skip(session, db, opts, cancel, on_progress, sink))]
pub async fn sync_folders<T, F>(
    session: &mut Session<T>,
    db: &Database,
    account_id: i64,
    opts: SyncOptions,
    cancel: &CancellationToken,
    mut on_progress: F,
    sink: &mut impl ChangeSink,
) -> Result<AccountSyncReport, Error>
where
    T: ImapStream,
    F: FnMut(SyncProgress),
{
    let mailboxes = db
        .read(move |c| repo::list_mailboxes(c, account_id))
        .await?;
    let mut out = AccountSyncReport::default();
    for mailbox in prioritize(mailboxes) {
        if cancel.is_cancelled() {
            break;
        }
        match sync_folder(
            session,
            db,
            mailbox.id,
            opts,
            cancel,
            &mut on_progress,
            sink,
        )
        .await
        {
            Ok(report) => out.reports.push(report),
            Err(error) => {
                tracing::warn!(folder = %mailbox.name, %error, "folder sync failed; continuing");
                out.failures.push(FolderFailure {
                    mailbox_id: mailbox.id,
                    name: mailbox.name,
                    error,
                });
            }
        }
    }
    Ok(out)
}

/// Order folders so the ones a user notices first sync first — INBOX, then the
/// other well-known folders, then everything else alphabetically — and drop
/// folders the server says cannot be selected.
#[must_use]
pub fn prioritize(mailboxes: Vec<repo::Mailbox>) -> Vec<repo::Mailbox> {
    let mut selectable: Vec<repo::Mailbox> = mailboxes
        .into_iter()
        .filter(|m| {
            let attributes = m
                .attributes
                .as_deref()
                .unwrap_or_default()
                .to_ascii_lowercase();
            // \Noselect (RFC 3501) and \NonExistent (RFC 5258) both mean
            // "SELECT will fail here".
            !attributes.contains("\\noselect") && !attributes.contains("\\nonexistent")
        })
        .collect();
    selectable.sort_by(|a, b| {
        folder_rank(&a.name)
            .cmp(&folder_rank(&b.name))
            .then_with(|| a.name.cmp(&b.name))
    });
    selectable
}

/// Sync priority for a folder name: lower syncs first.
///
/// Matches on the leaf so `[Gmail]/Sent Mail` ranks with `Sent`. The hierarchy
/// delimiter is server-defined; `/` and `.` cover every delimiter in practice,
/// and a miss only costs ordering, never correctness.
fn folder_rank(name: &str) -> u8 {
    let leaf = name.rsplit(['/', '.']).next().unwrap_or(name);
    if name.eq_ignore_ascii_case("INBOX") {
        0
    } else if [
        "sent",
        "sent mail",
        "sent items",
        "drafts",
        "archive",
        "all mail",
    ]
    .iter()
    .any(|known| leaf.eq_ignore_ascii_case(known))
    {
        1
    } else {
        2
    }
}

/// Render a sorted, ascending UID list as a compact IMAP set (`1:3,7,10:12`),
/// so a contiguous window is one range rather than hundreds of numbers.
pub(crate) fn format_uid_set(uids: &[i64]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut iter = uids.iter().copied();
    let Some(mut start) = iter.next() else {
        return String::new();
    };
    let mut end = start;
    for uid in iter {
        if uid == end + 1 {
            end = uid;
        } else {
            parts.push(render_range(start, end));
            start = uid;
            end = uid;
        }
    }
    parts.push(render_range(start, end));
    parts.join(",")
}

fn render_range(start: i64, end: i64) -> String {
    if start == end {
        start.to_string()
    } else {
        format!("{start}:{end}")
    }
}

/// Drop this mailbox's messages that belong to a superseded UID space, and
/// repair the threads they were in.
pub(crate) async fn purge_other_uidvalidity(
    db: &Database,
    mailbox_id: i64,
    keep: i64,
    sink: &mut impl ChangeSink,
) -> Result<u64, Error> {
    let (deleted, removed) = db
        .write(move |conn| {
            let tx = conn.transaction()?;
            let mut removed = Vec::new();
            let deleted = super::purge_other_uidvalidity(&tx, mailbox_id, keep, &mut removed)?;
            tx.commit()?;
            Ok((deleted, removed))
        })
        .await?;
    // Reported after the commit, for the same reason an expunge is: a rollback
    // would otherwise have announced a removal that did not happen.
    for (message_id, uid) in removed {
        sink.changed(Change::Removed { message_id, uid });
    }
    Ok(deleted as u64)
}

/// Persist the folder's sync checkpoint: both water marks plus status.
async fn checkpoint(
    db: &Database,
    mailbox_id: i64,
    uidvalidity: i64,
    last_synced_uid: i64,
    walked_down_to: Option<i64>,
) -> Result<(), Error> {
    let now = chrono::Utc::now().timestamp();
    db.write(move |conn| {
        // Preserve the modseq the delta sync owns; this task only advances the
        // UID-window checkpoint.
        let highestmodseq = repo::get_sync_state(conn, mailbox_id)?.and_then(|s| s.highestmodseq);
        repo::upsert_sync_state(
            conn,
            &repo::SyncState {
                mailbox_id,
                uidvalidity: Some(uidvalidity),
                highestmodseq,
                last_synced_uid: Some(last_synced_uid),
                walked_down_to,
                last_sync_at: Some(now),
                full_sync_done: walked_down_to == Some(1),
            },
        )
    })
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests;
