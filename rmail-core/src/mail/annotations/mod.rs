//! Escrow that carries message-level tags and notes across a `Move`.
//!
//! # The gap this closes
//!
//! [`super::MailStore::move_message`] cannot keep `messages.id` stable. IMAP
//! tells a client the UID a moved message landed on only through UIDPLUS's
//! `COPYUID` response code, which the client in use does not surface, so the
//! move deletes the local row and lets the destination folder's next sync
//! insert it fresh under a new id (see this module's parent docs, "Move does
//! not guess a new UID"). Everything keyed to `messages(id)` by
//! `ON DELETE CASCADE` dies with that row — including `message_tags` and
//! `notes`, which hold data the *user* authored and the server has never
//! heard of. Flags, bodies and attachments all come back from the next sync;
//! a tag and a note do not come back from anywhere.
//!
//! So the two halves of a move are bridged here: [`capture`] copies the
//! message-level annotations into `moved_annotations` just before the row
//! goes, and [`replay`] re-attaches them to the row the destination folder
//! syncs in, matched on the one identity a move does not change — the RFC 5322
//! `Message-ID` header.
//!
//! # What it deliberately does not do
//!
//! *Thread-level* annotations are not escrowed and need no help: they hang off
//! `threads(id)`, and a moved message rejoins its thread on resync because
//! threading reads `Message-ID`/`References`, which the move did not touch.
//!
//! A message with no `Message-ID` header is not escrowed at all. There would
//! be nothing to match it back up by, and writing a row that can only ever
//! expire is worse than not writing one — it hides the loss instead of
//! bounding it.
//!
//! # Failure is one-directional on purpose
//!
//! Replay never fails an insert. A message that syncs without its old tags is
//! a bad day; a message that refuses to sync because re-attaching a tag went
//! wrong is a broken mailbox. [`replay`] therefore reports how many
//! annotations it restored and logs anything it could not, and its caller
//! commits regardless.

use std::time::Duration;

use rusqlite::{OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};

#[cfg(test)]
mod tests;

/// How long an unclaimed escrow row survives before the reaper takes it.
///
/// This is a safety net, not a tuning knob: the normal lifetime of a row here
/// is the seconds between the `MOVE` returning and the destination folder's
/// next sync. A row still present after thirty days means the message never
/// arrived where the server said it would, and no later sync is going to
/// change that. The window is long enough to survive a laptop that was shut
/// for a fortnight mid-move, and short enough that a pathological account
/// cannot accumulate escrow indefinitely.
pub const EXPIRY: Duration = Duration::from_secs(30 * 24 * 60 * 60);

/// A `message_tags` row, less the identity columns replay reassigns.
#[derive(Debug, Serialize, Deserialize)]
struct TagPayload {
    tag_id: i64,
    source: String,
    state: String,
    confidence: Option<f64>,
    rationale: Option<String>,
    created_at: i64,
}

/// A `notes` row, less the identity columns replay reassigns.
#[derive(Debug, Serialize, Deserialize)]
struct NotePayload {
    body_md: String,
    author: String,
    created_at: i64,
    updated_at: i64,
}

const KIND_TAG: &str = "tag";
const KIND_NOTE: &str = "note";
const KIND_THREAD_TAG: &str = "thread_tag";
const KIND_THREAD_NOTE: &str = "thread_note";

/// What a restored annotation attaches to. `message_tags` and `notes` both
/// enforce this as a XOR at the schema level, so it is one or the other.
#[derive(Debug, Clone, Copy)]
enum Target {
    Message(i64),
    Thread(i64),
}

impl Target {
    /// `(message_id, thread_id)`, exactly one of which is set.
    fn columns(self) -> (Option<i64>, Option<i64>) {
        match self {
            Self::Message(id) => (Some(id), None),
            Self::Thread(id) => (None, Some(id)),
        }
    }
}

/// The thread the resynced message was assigned to, read at most once per
/// replay and memoized in `cache` — most replays have no thread-level rows and
/// must not pay for the lookup at all.
fn resolve_thread(
    tx: &Transaction<'_>,
    message_id: i64,
    cache: &mut Option<Option<i64>>,
) -> rusqlite::Result<Option<i64>> {
    if let Some(cached) = cache {
        return Ok(*cached);
    }
    let thread_id: Option<i64> = tx
        .query_row(
            "SELECT thread_id FROM messages WHERE id = ?1",
            [message_id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    *cache = Some(thread_id);
    Ok(thread_id)
}

/// Read the tag applications hanging off one target column.
fn read_tags(tx: &Transaction<'_>, column: &str, id: i64) -> rusqlite::Result<Vec<TagPayload>> {
    // `column` is one of two literals chosen by this module, never caller
    // input — there is nothing here for a query parameter to bind to, since a
    // column name is not a value.
    let sql = format!(
        "SELECT tag_id, source, state, confidence, rationale, created_at
         FROM message_tags WHERE {column} = ?1"
    );
    let mut stmt = tx.prepare(&sql)?;
    let rows = stmt.query_map([id], |row| {
        Ok(TagPayload {
            tag_id: row.get(0)?,
            source: row.get(1)?,
            state: row.get(2)?,
            confidence: row.get(3)?,
            rationale: row.get(4)?,
            created_at: row.get(5)?,
        })
    })?;
    rows.collect()
}

/// Read the notes hanging off one target column.
fn read_notes(tx: &Transaction<'_>, column: &str, id: i64) -> rusqlite::Result<Vec<NotePayload>> {
    let sql =
        format!("SELECT body_md, author, created_at, updated_at FROM notes WHERE {column} = ?1");
    let mut stmt = tx.prepare(&sql)?;
    let rows = stmt.query_map([id], |row| {
        Ok(NotePayload {
            body_md: row.get(0)?,
            author: row.get(1)?,
            created_at: row.get(2)?,
            updated_at: row.get(3)?,
        })
    })?;
    rows.collect()
}

/// The thread `message_id` belongs to, but only if removing this message will
/// leave it empty — which is when `sync::repair_threads` deletes it and takes
/// its thread-level annotations with it.
///
/// A thread with other messages still in it survives the move untouched, so
/// escrowing its annotations would re-apply tags it never lost.
fn thread_about_to_be_orphaned(
    tx: &Transaction<'_>,
    message_id: i64,
) -> rusqlite::Result<Option<i64>> {
    let thread_id: Option<i64> = tx
        .query_row(
            "SELECT thread_id FROM messages WHERE id = ?1",
            [message_id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    let Some(thread_id) = thread_id else {
        return Ok(None);
    };
    let siblings: i64 = tx.query_row(
        "SELECT COUNT(*) FROM messages WHERE thread_id = ?1 AND id <> ?2",
        rusqlite::params![thread_id, message_id],
        |row| row.get(0),
    )?;
    Ok((siblings == 0).then_some(thread_id))
}

/// Copy `message_id`'s message-level tags and notes into escrow, to be
/// re-attached when `dest_mailbox_id` next syncs the message in.
///
/// Call this *before* the local row is deleted, in the same transaction as the
/// delete: an escrow row written outside it would survive a rolled-back move
/// and re-apply tags to a message that never went anywhere.
///
/// Returns the number of annotations escrowed. Zero is routine — most messages
/// carry no tags or notes, and a message with no `Message-ID` header is
/// skipped entirely.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub(crate) fn capture(
    tx: &Transaction<'_>,
    message_id: i64,
    dest_mailbox_id: i64,
) -> rusqlite::Result<usize> {
    let identity: Option<(i64, Option<String>)> = tx
        .query_row(
            "SELECT account_id, message_id FROM messages WHERE id = ?1",
            [message_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    // No row, or no `Message-ID` header to find it by again.
    let Some((account_id, Some(header))) = identity else {
        return Ok(0);
    };

    let mut escrowed = 0usize;

    for tag in &read_tags(tx, "message_id", message_id)? {
        escrowed += insert_escrow(tx, account_id, dest_mailbox_id, &header, KIND_TAG, tag)?;
    }
    for note in &read_notes(tx, "message_id", message_id)? {
        escrowed += insert_escrow(tx, account_id, dest_mailbox_id, &header, KIND_NOTE, note)?;
    }

    // The thread's own annotations, but only when this move is what empties
    // the thread — see `thread_about_to_be_orphaned`.
    if let Some(thread_id) = thread_about_to_be_orphaned(tx, message_id)? {
        for tag in &read_tags(tx, "thread_id", thread_id)? {
            escrowed += insert_escrow(
                tx,
                account_id,
                dest_mailbox_id,
                &header,
                KIND_THREAD_TAG,
                tag,
            )?;
        }
        for note in &read_notes(tx, "thread_id", thread_id)? {
            escrowed += insert_escrow(
                tx,
                account_id,
                dest_mailbox_id,
                &header,
                KIND_THREAD_NOTE,
                note,
            )?;
        }
    }

    if escrowed > 0 {
        tracing::debug!(
            message_id,
            dest_mailbox_id,
            escrowed,
            "held message-level annotations in escrow across a move"
        );
    }
    Ok(escrowed)
}

/// Serialize one annotation into `moved_annotations`.
///
/// A payload that will not serialize is dropped with a warning rather than
/// failing the move: these are plain structs of owned scalars, so this is
/// unreachable in practice, but "the move fails" is the wrong answer to it.
fn insert_escrow<T: Serialize>(
    tx: &Transaction<'_>,
    account_id: i64,
    dest_mailbox_id: i64,
    header_message_id: &str,
    kind: &str,
    payload: &T,
) -> rusqlite::Result<usize> {
    let encoded = match serde_json::to_string(payload) {
        Ok(encoded) => encoded,
        Err(error) => {
            tracing::warn!(
                %error,
                kind,
                "could not encode an annotation for escrow; it will not survive the move"
            );
            return Ok(0);
        }
    };
    tx.execute(
        "INSERT INTO moved_annotations
             (account_id, dest_mailbox_id, header_message_id, kind, payload)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![
            account_id,
            dest_mailbox_id,
            header_message_id,
            kind,
            encoded
        ],
    )
}

/// Re-attach any escrowed annotations for `header_message_id` to
/// `new_message_id`, which has just been inserted into `mailbox_id`.
///
/// Call this in the same transaction as the insert, after threading has run —
/// a replayed note's search-index refresh reads the message's thread.
///
/// Escrow rows are consumed whether or not their annotation could be restored,
/// so a tag whose `tags` row the user deleted in the meantime does not sit in
/// the table until it expires.
///
/// Returns the number of annotations restored.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub(crate) fn replay(
    tx: &Transaction<'_>,
    new_message_id: i64,
    mailbox_id: i64,
    header_message_id: &str,
) -> rusqlite::Result<usize> {
    let pending = {
        let mut stmt = tx.prepare(
            "SELECT id, kind, payload FROM moved_annotations
             WHERE dest_mailbox_id = ?1 AND header_message_id = ?2
             ORDER BY id",
        )?;
        let rows = stmt.query_map(rusqlite::params![mailbox_id, header_message_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    if pending.is_empty() {
        return Ok(0);
    }

    // Resolved once, and only if something actually needs it: threading has
    // already run for this message, so its `thread_id` is the conversation the
    // escrowed thread-level annotations belong on now. `None` means the
    // message ended up unthreaded, in which case a thread-level annotation has
    // nowhere to go and is dropped rather than silently re-scoped to the
    // message.
    let mut thread_id: Option<Option<i64>> = None;

    let mut restored = 0usize;
    let mut restored_a_note = false;
    for (escrow_id, kind, payload) in &pending {
        match kind.as_str() {
            KIND_TAG | KIND_THREAD_TAG => {
                let target = if kind == KIND_TAG {
                    Some(Target::Message(new_message_id))
                } else {
                    resolve_thread(tx, new_message_id, &mut thread_id)?.map(Target::Thread)
                };
                match (target, serde_json::from_str::<TagPayload>(payload)) {
                    (Some(target), Ok(tag)) => restored += restore_tag(tx, target, &tag)?,
                    (None, _) => tracing::warn!(
                        escrow_id,
                        "dropping an escrowed thread tag: the resynced message has no thread"
                    ),
                    (_, Err(error)) => {
                        tracing::warn!(%error, escrow_id, "dropping an undecodable escrowed tag");
                    }
                }
            }
            KIND_NOTE | KIND_THREAD_NOTE => {
                let target = if kind == KIND_NOTE {
                    Some(Target::Message(new_message_id))
                } else {
                    resolve_thread(tx, new_message_id, &mut thread_id)?.map(Target::Thread)
                };
                match (target, serde_json::from_str::<NotePayload>(payload)) {
                    (Some(target), Ok(note)) => {
                        let added = restore_note(tx, target, &note)?;
                        restored += added;
                        restored_a_note |= added > 0;
                    }
                    (None, _) => tracing::warn!(
                        escrow_id,
                        "dropping an escrowed thread note: the resynced message has no thread"
                    ),
                    (_, Err(error)) => {
                        tracing::warn!(%error, escrow_id, "dropping an undecodable escrowed note");
                    }
                }
            }
            other => tracing::warn!(
                kind = other,
                escrow_id,
                "dropping an escrow row of unknown kind"
            ),
        }
        tx.execute("DELETE FROM moved_annotations WHERE id = ?1", [escrow_id])?;
    }

    // Only once, and only if a note actually landed: the refresh rewrites the
    // message's whole effective note text, so doing it per note would repeat
    // identical work, and doing it when no note was restored would touch
    // `index_content` for nothing.
    if restored_a_note {
        crate::notes::refresh_note_index(tx, new_message_id)?;
    }

    if restored > 0 {
        tracing::info!(
            message_id = new_message_id,
            mailbox_id,
            restored,
            "restored message-level annotations onto a moved message"
        );
    }
    Ok(restored)
}

/// Re-apply one escrowed tag.
///
/// `WHERE EXISTS` guards the `tags(id)` foreign key: the user may have deleted
/// the tag itself between the two halves of the move, and a plain insert would
/// fail the whole transaction — i.e. block the message from syncing — over an
/// annotation that no longer has anywhere to point. `OR IGNORE` covers the
/// unique `(tag_id, message_id)`/`(tag_id, thread_id)` indexes, so a replay
/// racing an already-applied tag is a no-op rather than an error.
fn restore_tag(tx: &Transaction<'_>, target: Target, tag: &TagPayload) -> rusqlite::Result<usize> {
    let (message_id, thread_id) = target.columns();
    tx.execute(
        "INSERT OR IGNORE INTO message_tags
             (tag_id, message_id, thread_id, source, state, confidence, rationale, created_at)
         SELECT ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8
         WHERE EXISTS (SELECT 1 FROM tags WHERE id = ?1)",
        rusqlite::params![
            tag.tag_id,
            message_id,
            thread_id,
            tag.source,
            tag.state,
            tag.confidence,
            tag.rationale,
            tag.created_at,
        ],
    )
}

/// Re-attach one escrowed note, preserving the timestamps the user's note was
/// written with rather than stamping it with the move.
fn restore_note(
    tx: &Transaction<'_>,
    target: Target,
    note: &NotePayload,
) -> rusqlite::Result<usize> {
    let (message_id, thread_id) = target.columns();
    tx.execute(
        "INSERT INTO notes (message_id, thread_id, body_md, author, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            message_id,
            thread_id,
            note.body_md,
            note.author,
            note.created_at,
            note.updated_at,
        ],
    )
}

/// Delete escrow rows older than [`EXPIRY`], returning how many went.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub fn expire(conn: &rusqlite::Connection) -> rusqlite::Result<usize> {
    let cutoff = i64::try_from(EXPIRY.as_secs()).unwrap_or(i64::MAX);
    conn.execute(
        "DELETE FROM moved_annotations WHERE created_at < unixepoch() - ?1",
        [cutoff],
    )
}
