//! Mailbox synchronization.
//!
//! Two engines, one folder model:
//!
//! - [`full`] is the initial sync — a UID-window walk that downloads a folder
//!   newest-first and is resumable by construction. It answers "what do I not
//!   have yet?".
//! - [`delta`] is the steady state — CONDSTORE/QRESYNC (or a UID enumeration
//!   diff where the server has neither). It answers "what changed?", which the
//!   UID walk structurally cannot see: a flag flipped on a message already
//!   stored, or a message expunged out from under it.
//! - [`idle`] decides *when* to ask. It parks a long-lived connection on IMAP
//!   `IDLE` so the server can speak the moment something happens, and falls
//!   back to interval polling where it cannot.
//!
//! Both key on `(mailbox, UIDVALIDITY, UID)` and checkpoint into the same
//! `sync_state` row, so a delta run over a folder with no usable baseline hands
//! straight back to the full walk.

pub mod delta;
pub mod engine;
pub mod full;
pub mod idle;

use rusqlite::{Connection, OptionalExtension};
use std::collections::BTreeSet;

pub(crate) use crate::imap::{command_error, select_error};

pub use delta::{delta_sync, delta_sync_folders, AccountDeltaReport, DeltaReport, DeltaStrategy};
pub use engine::{FolderOutcome, PassReport, SyncEngine, SyncMode};
pub use full::{
    prioritize, sync_folder, sync_folders, SyncOptions, SyncProgress, SyncReport, DEFAULT_WINDOW,
};
pub use idle::{
    watch_folder, watch_folders, AccountWatchReport, IdleOptions, WatchCycle, WatchOutcome,
    WatchReport, WatchTrigger,
};

// The sync test suites live beside the engines rather than inside them, so each
// path is addressable on its own — `sync::qresync` for the modseq engines,
// `sync::uiddiff_fallback` for a server with neither extension, `sync::idle`
// and `sync::poll_fallback` for the push engine. `tasks.md` names those filters
// as the proof for each task.
#[cfg(test)]
mod account;
#[cfg(test)]
mod harness;
#[cfg(test)]
mod poll_fallback;
#[cfg(test)]
mod qresync;
#[cfg(test)]
mod uiddiff_fallback;

/// Something a sync pass changed in the local store.
///
/// Reported as it happens rather than accumulated into the report, because a
/// first delta over a busy folder can touch tens of thousands of messages and
/// the consumer — the durable event log — wants to see them as they land, not
/// after.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change {
    /// A message was downloaded and stored.
    Added {
        /// Stable local id.
        message_id: i64,
        /// IMAP UID.
        uid: i64,
    },
    /// A stored message's flag set was replaced with the server's.
    FlagsChanged {
        /// Stable local id.
        message_id: i64,
        /// IMAP UID.
        uid: i64,
        /// The flags the message now has.
        flags: Vec<String>,
    },
    /// A stored message was removed because the server no longer has it.
    Removed {
        /// Stable local id, now gone.
        message_id: i64,
        /// IMAP UID, now gone.
        uid: i64,
    },
}

/// Where a sync pass reports what it changed.
///
/// A trait rather than a closure so `&mut ()` is a valid "nobody is watching" —
/// the engines are useful without an event log attached, and every test that
/// does not care about events should not have to name a callback.
pub trait ChangeSink {
    /// Record one change.
    fn changed(&mut self, change: Change);
}

/// The null sink: syncing with nothing listening.
impl ChangeSink for () {
    fn changed(&mut self, _change: Change) {}
}

impl<F: FnMut(Change)> ChangeSink for F {
    fn changed(&mut self, change: Change) {
        self(change);
    }
}

/// Delete messages by surrogate id and repair the threads they were in,
/// returning how many rows went away.
///
/// Removing a message can empty or re-root a conversation, so every touched
/// thread is recomputed and any left with no members is collected — otherwise
/// an expunge (or a UIDVALIDITY purge) leaves threads pointing at messages that
/// no longer exist. `flags` and `attachments` cascade with the row.
///
/// The caller must already hold a write transaction: the delete and the thread
/// repair have to commit together or a crash between them leaves dangling
/// conversations.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub(crate) fn remove_messages(conn: &Connection, ids: &[i64]) -> rusqlite::Result<usize> {
    if ids.is_empty() {
        return Ok(0);
    }

    let mut threads: BTreeSet<i64> = BTreeSet::new();
    {
        let mut select = conn.prepare("SELECT thread_id FROM messages WHERE id = ?1")?;
        for id in ids {
            let thread: Option<i64> = select
                .query_row([id], |row| row.get::<_, Option<i64>>(0))
                .optional()?
                .flatten();
            threads.extend(thread);
        }
    }

    // Before the delete: the mentions cascade away on their own, but the
    // co-occurrence weights they supported do not, and mail a user expunged
    // must stop influencing what search ranks first.
    crate::index::entities::withdraw_messages(conn, ids)?;
    // `chunks` cascades from `messages`, but `vec_chunks` is a virtual table
    // and cascades from nothing. An orphaned vector is not merely wasted space:
    // kNN returns it, the join to `chunks` drops it, and it has silently
    // consumed one of the k slots a user asked for.
    crate::index::semantic::drop_vectors(conn, ids)?;

    let mut deleted = 0usize;
    {
        let mut delete = conn.prepare("DELETE FROM messages WHERE id = ?1")?;
        for id in ids {
            deleted += delete.execute([id])?;
        }
    }

    repair_threads(conn, threads)?;
    Ok(deleted)
}

/// Delete every message of a mailbox that is *not* in `keep`'s UID space, and
/// repair the threads they were in.
///
/// The set-based twin of [`remove_messages`], for the one caller whose input is
/// a predicate rather than a list. A `UIDVALIDITY` bump can invalidate a
/// six-figure folder in one go; enumerating it into a `Vec` to issue a statement
/// per row would hold the single writer connection for the duration.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub(crate) fn purge_other_uidvalidity(
    conn: &Connection,
    mailbox_id: i64,
    keep: i64,
    removed: &mut Vec<(i64, i64)>,
) -> rusqlite::Result<usize> {
    // Collect identities before the delete: a purge removes messages just as
    // surely as an expunge does, and a consumer that indexed them needs to know
    // which ones to drop. Reporting only a count would leave every downstream
    // index holding documents for mail that is gone.
    {
        let mut stmt = conn
            .prepare("SELECT id, uid FROM messages WHERE mailbox_id = ?1 AND uidvalidity <> ?2")?;
        let rows = stmt.query_map(rusqlite::params![mailbox_id, keep], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
        for row in rows {
            removed.push(row?);
        }
    }
    let threads: BTreeSet<i64> = {
        let mut stmt = conn.prepare(
            "SELECT DISTINCT thread_id FROM messages
             WHERE mailbox_id = ?1 AND uidvalidity <> ?2 AND thread_id IS NOT NULL",
        )?;
        let rows = stmt.query_map(rusqlite::params![mailbox_id, keep], |row| row.get(0))?;
        rows.collect::<rusqlite::Result<BTreeSet<i64>>>()?
    };
    let deleted = conn.execute(
        "DELETE FROM messages WHERE mailbox_id = ?1 AND uidvalidity <> ?2",
        rusqlite::params![mailbox_id, keep],
    )?;
    // A `UIDVALIDITY` bump can invalidate a six-figure folder, so the affected
    // entity set is not worth naming. The mentions are already gone by cascade;
    // one set-based pass restores every weight from what is left.
    crate::index::entities::reconcile_edges(conn)?;
    crate::index::semantic::sweep_orphan_vectors(conn)?;
    repair_threads(conn, threads)?;
    Ok(deleted)
}

/// Recompute each thread and collect the ones left with no members.
fn repair_threads(conn: &Connection, threads: BTreeSet<i64>) -> rusqlite::Result<()> {
    for thread_id in threads {
        crate::thread::recompute_thread(conn, thread_id)?;
        conn.execute(
            "DELETE FROM threads WHERE id = ?1 AND message_count = 0",
            [thread_id],
        )?;
    }
    Ok(())
}
