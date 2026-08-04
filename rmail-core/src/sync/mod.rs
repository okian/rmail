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
//!
//! Both key on `(mailbox, UIDVALIDITY, UID)` and checkpoint into the same
//! `sync_state` row, so a delta run over a folder with no usable baseline hands
//! straight back to the full walk. The IDLE push engine lands alongside them in
//! task 13.

pub mod delta;
pub mod full;

use rusqlite::{Connection, OptionalExtension};
use std::collections::BTreeSet;

use crate::error::Error;

pub(crate) use crate::imap::command_error;

pub use delta::{delta_sync, delta_sync_folders, AccountDeltaReport, DeltaReport, DeltaStrategy};
pub use full::{
    prioritize, sync_folder, sync_folders, SyncOptions, SyncProgress, SyncReport, DEFAULT_WINDOW,
};

// The delta-sync test suites live beside the engines rather than inside
// `delta.rs`, so each path is addressable on its own: `sync::qresync` covers
// the modseq engines and `sync::uiddiff_fallback` covers the server that has
// neither extension. `tasks.md` names both filters as this task's proof.
#[cfg(test)]
mod account;
#[cfg(test)]
mod harness;
#[cfg(test)]
mod qresync;
#[cfg(test)]
mod uiddiff_fallback;

/// Map a `SELECT` failure for `folder`.
///
/// A tagged `NO` here means the folder is gone or unselectable — not that the
/// credentials are bad, which is what the login-shaped
/// [`crate::imap::map_imap_err`] would say, and which would send a client
/// chasing an authentication problem it does not have.
pub(crate) fn select_error(folder: &str, err: async_imap::error::Error) -> Error {
    match err {
        async_imap::error::Error::No(msg) => {
            Error::not_found(format!("cannot select folder {folder}: {msg}"))
        }
        other => crate::imap::map_imap_err(other),
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
) -> rusqlite::Result<usize> {
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
