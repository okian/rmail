//! Notes: freeform markdown attached to a message or a thread (prd.md,
//! "III-4. Notes & Tags").
//!
//! # The target is a XOR, enforced twice
//!
//! A note belongs to exactly one message or one thread, never both and never
//! neither. [`Target`] makes the illegal states unrepresentable in Rust, but
//! that alone is not the whole guarantee — a raw `INSERT` (a future
//! migration, a `sqlite3` session, a bug in code this module never runs
//! through) could still write a row this API would never construct. `V23`'s
//! `CHECK ((message_id IS NULL) <> (thread_id IS NULL))` is what closes that
//! gap: the invariant holds at the schema level, for every write path, not
//! only this one. [`tests`] proves the database itself rejects both shapes
//! via raw SQL, independent of anything in this file.
//!
//! Existence of the referenced message/thread is enforced by the same
//! `REFERENCES ... ON DELETE CASCADE` foreign keys the schema already
//! carries — [`add`](NoteStore::add) does not run a separate lookup before
//! inserting; it lets the write fail on the foreign key and translates that
//! specific failure into [`Error::NotFound`] (see [`is_missing_target`]).
//! `ON DELETE CASCADE` is also what makes a note disappear automatically
//! when its message or thread is deleted, with no cleanup code of this
//! module's own to keep in sync.
//!
//! # Two FTS surfaces, not one
//!
//! `V23` also creates `notes_fts`, an external-content FTS5 index over note
//! *rows* (`content = 'notes'`, trigger-synced — see that migration's own
//! docs for why triggers are required and what they do). This module never
//! queries it directly; the search-time story it is built to serve is task
//! 56's other half:
//!
//! - [`refresh_note_index`] folds a message's *effective* note text — its
//!   own notes plus its thread's, since a thread-targeted note is meant to
//!   read as attached to every message in the conversation — into
//!   `index_content` under [`Part::Note`], the same part
//!   [`crate::index::extract`] already reserves and [`crate::index::fts`]
//!   already weights (`Bm25Weights::notes`, default `3.0`). That is what
//!   makes a plain free-text search surface a note the same way it surfaces
//!   a subject or a body, through the *existing* `fts_messages.notes`
//!   column rather than a second, redundant index of the same text.
//! - `note:`/`has:note` — the hard-filter operators task 25's parser already
//!   emits ([`crate::query::Operator::Note`],
//!   [`crate::query::HasTarget::Note`]) — are compiled in
//!   [`crate::retrieve::filtermask`] and [`crate::retrieve::lexical`]
//!   directly against the `notes` table (an `EXISTS` correlated on
//!   `messages.id`/`messages.thread_id`, the same effective-target rule
//!   `refresh_note_index` uses), the same way `subject:`/`body:` match
//!   against `messages` columns rather than `fts_messages` — a hard filter
//!   gates, it does not rank, so it has no reason to go through BM25 either.
//!
//! `notes_fts` itself is not read by anything in this crate yet; it exists
//! because the schema task 56 owns specifies it, trigger-synced so it is
//! always ready for a future consumer (e.g. a note-scoped search) without a
//! backfill.
//!
//! # Last-write-wins
//!
//! [`edit`](NoteStore::edit) is a plain `UPDATE ... SET body_md = ?,
//! updated_at = unixepoch()`, with no version/ETag check. Two concurrent
//! edits both succeed; whichever commits last is what every subsequent read
//! sees, and `updated_at` always reflects the most recent write. This is a
//! deliberate simplicity choice matching prd.md ("Concurrent note edit →
//! last-write-wins on `updated_at`, `WatchNotes` refreshes open UIs") — a
//! lost update is visible to an open UI immediately via [`NoteStore::watch`],
//! not silently swallowed.
//!
//! # Indexing is optional
//!
//! [`NoteStore::new`]'s `index_enabled` mirrors `config.notes.index` — when
//! `false`, notes are stored and served normally but never touch
//! `index_content`/the index queue, matching the config field's documented
//! meaning ("whether notes are indexed for search").

use rusqlite::{OptionalExtension, Row, Transaction};
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;

use crate::error::Error;
use crate::index::extract::{self, Part};
use crate::index::{IndexKind, IndexQueue, NewJob, PRIORITY_NORMAL};
use crate::repo;
use crate::storage::Database;

/// How many live [`NoteChange`]s [`NoteStore::watch`] buffers per subscriber
/// before the slowest one starts missing events. [`WatchNotes`](NoteStore::watch)
/// has no durable backlog to recover from (unlike
/// [`crate::events::EventLog`]) — see [`NoteStore::watch`]'s own docs — so
/// this only has to absorb a burst, not survive a subscriber falling behind
/// for real.
const CHANNEL_CAPACITY: usize = 256;

/// What a note is attached to: exactly one message or one thread.
///
/// See the module docs' "The target is a XOR, enforced twice" section — this
/// type is one half of that guarantee, the schema's `CHECK` constraint is
/// the other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Target {
    /// Attached to one message.
    Message(i64),
    /// Attached to one thread (every current and future message in it, by
    /// convention — see [`refresh_note_index`]).
    Thread(i64),
}

impl Target {
    /// The `(message_id, thread_id)` column pair this target writes —
    /// exactly one `Some`, matching the schema's `CHECK`.
    fn columns(self) -> (Option<i64>, Option<i64>) {
        match self {
            Self::Message(id) => (Some(id), None),
            Self::Thread(id) => (None, Some(id)),
        }
    }

    /// A client-safe "not found" message naming which kind of id was bad.
    fn not_found_message(self) -> String {
        match self {
            Self::Message(id) => format!("message {id} not found"),
            Self::Thread(id) => format!("thread {id} not found"),
        }
    }
}

/// Who wrote a note.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NoteAuthor {
    /// A person, through the CLI/TUI/gRPC directly.
    User,
    /// Claude — e.g. `summarize_thread` persisting its output as a note (see
    /// prd.md's Claude Integration section for notes/tags).
    Ai,
}

impl NoteAuthor {
    /// The stable string stored in `notes.author`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Ai => "ai",
        }
    }

    /// Parse a stored value, or `None` for anything this build did not
    /// write — callers decide how to report that (a corrupt-row error when
    /// reading the database, an `InvalidArgument` when reading a request).
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "user" => Some(Self::User),
            "ai" => Some(Self::Ai),
            _ => None,
        }
    }
}

impl Default for NoteAuthor {
    /// Matches `notes.author`'s own `DEFAULT 'user'`.
    fn default() -> Self {
        Self::User
    }
}

/// A persisted note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    /// Stable id.
    pub id: i64,
    /// What it is attached to.
    pub target: Target,
    /// Markdown body.
    pub body_md: String,
    /// Who wrote it.
    pub author: NoteAuthor,
    /// Creation time (unix seconds).
    pub created_at: i64,
    /// Last-edit time (unix seconds) — see the module docs' "Last-write-wins"
    /// section.
    pub updated_at: i64,
}

impl Note {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        let id: i64 = row.get("id")?;
        let message_id: Option<i64> = row.get("message_id")?;
        let thread_id: Option<i64> = row.get("thread_id")?;
        let target = match (message_id, thread_id) {
            (Some(m), None) => Target::Message(m),
            (None, Some(t)) => Target::Thread(t),
            // Unreachable through `V23`'s own `CHECK` constraint for any row
            // this build wrote; a row that got here anyway (a future
            // migration, manual surgery) is corrupt data, not a client
            // mistake — reported the same way `crate::events::Event::from_row`
            // reports a stored value this code cannot interpret.
            (both_or_neither, _) => {
                return Err(corrupt(
                    id,
                    "message_id/thread_id",
                    &format!(
                        "expected exactly one to be set, got {both_or_neither:?}/{thread_id:?}"
                    ),
                ))
            }
        };
        let author_raw: String = row.get("author")?;
        let author =
            NoteAuthor::parse(&author_raw).ok_or_else(|| corrupt(id, "author", &author_raw))?;
        Ok(Self {
            id,
            target,
            body_md: row.get("body_md")?,
            author,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
        })
    }
}

/// A row this build cannot interpret is a corrupt log, not a bad request —
/// see `crate::events::corrupt`, which this mirrors exactly.
fn corrupt(id: i64, column: &str, value: &str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::other(format!(
            "corrupt note {id}, column {column}: {value}"
        ))),
    )
}

/// A new note to add.
#[derive(Debug, Clone)]
pub struct NewNote {
    /// What it attaches to.
    pub target: Target,
    /// Markdown body.
    pub body_md: String,
    /// Who wrote it.
    pub author: NoteAuthor,
}

/// A live add/edit/delete, as [`NoteStore::watch`] streams it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoteChange {
    /// A note was added.
    Added(Note),
    /// A note's body was replaced.
    Edited(Note),
    /// A note was deleted.
    Deleted {
        /// The deleted note's id.
        id: i64,
        /// What it had been attached to (the row is already gone, so this is
        /// the only place a subscriber can still learn it).
        target: Target,
    },
}

impl NoteChange {
    /// The target this change concerns — for [`NoteStore::watch`]'s
    /// consumers to filter a subscription down to one target.
    #[must_use]
    pub fn target(&self) -> Target {
        match self {
            Self::Added(note) | Self::Edited(note) => note.target,
            Self::Deleted { target, .. } => *target,
        }
    }
}

/// Notes storage: CRUD plus the lexical-index feed and the live change
/// stream.
///
/// Cheap to clone: every clone shares one database handle, one index queue
/// handle, and one broadcast channel.
#[derive(Debug, Clone)]
pub struct NoteStore {
    db: Database,
    index_queue: IndexQueue,
    /// Mirrors `config.notes.index` — see the module docs' "Indexing is
    /// optional" section.
    index_enabled: bool,
    tx: broadcast::Sender<NoteChange>,
}

impl NoteStore {
    /// Open a store over `db`, feeding `index_queue` when `index_enabled`.
    #[must_use]
    pub fn new(db: Database, index_queue: IndexQueue, index_enabled: bool) -> Self {
        let (tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        Self {
            db,
            index_queue,
            index_enabled,
            tx,
        }
    }

    /// Subscribe to the live tail of every add/edit/delete.
    ///
    /// Unlike [`crate::events::EventLog`], there is no durable backlog to
    /// recover from — a subscriber that lags past [`CHANNEL_CAPACITY`] loses
    /// the events in between, not merely its place, matching prd.md's own
    /// framing ("`WatchNotes` refreshes open UIs", not "replays history"). A
    /// UI that needs the current state after a gap re-reads it with
    /// [`NoteStore::list`], the same way it would after opening the view for
    /// the first time.
    #[must_use]
    pub fn watch(&self) -> broadcast::Receiver<NoteChange> {
        self.tx.subscribe()
    }

    /// Add a note.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] if `body_md` is empty (after trimming).
    /// [`Error::NotFound`] if the target message/thread does not exist —
    /// detected via the schema's own foreign keys (see the module docs),
    /// not a separate lookup. Otherwise a mapped storage error.
    #[tracing::instrument(skip(self, new), fields(note_id))]
    pub async fn add(&self, new: NewNote) -> Result<Note, Error> {
        let body_md = new.body_md.trim().to_owned();
        if body_md.is_empty() {
            return Err(Error::invalid_argument("note body must not be empty"));
        }
        let target = new.target;
        let author = new.author;
        let index_enabled = self.index_enabled;
        let (message_id, thread_id) = target.columns();

        let outcome = self
            .db
            .write(move |conn| {
                let tx = conn.transaction()?;
                let inserted = tx.query_row(
                    "INSERT INTO notes (message_id, thread_id, body_md, author, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, unixepoch(), unixepoch())
                     RETURNING id, message_id, thread_id, body_md, author, created_at, updated_at",
                    rusqlite::params![message_id, thread_id, body_md, author.as_str()],
                    Note::from_row,
                );
                let note = match inserted {
                    Ok(note) => note,
                    Err(err) if is_missing_target(&err) => {
                        return Ok(Err(Error::not_found(target.not_found_message())))
                    }
                    Err(err) => return Err(err),
                };
                let hashes = if index_enabled {
                    reindex(&tx, target)?
                } else {
                    Vec::new()
                };
                tx.commit()?;
                Ok(Ok((note, hashes)))
            })
            .await?;
        let (note, hashes) = outcome?;

        self.enqueue_reindex(hashes).await?;
        tracing::Span::current().record("note_id", note.id);
        let _ = self.tx.send(NoteChange::Added(note.clone()));
        Ok(note)
    }

    /// Replace an existing note's body. Last-write-wins — see the module
    /// docs.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] if `body_md` is empty (after trimming).
    /// [`Error::NotFound`] if no note has `note_id`. Otherwise a mapped
    /// storage error.
    #[tracing::instrument(skip(self, body_md))]
    pub async fn edit(&self, note_id: i64, body_md: String) -> Result<Note, Error> {
        let body_md = body_md.trim().to_owned();
        if body_md.is_empty() {
            return Err(Error::invalid_argument("note body must not be empty"));
        }
        let index_enabled = self.index_enabled;

        let outcome = self
            .db
            .write(move |conn| {
                let tx = conn.transaction()?;
                let updated: Option<Note> = tx
                    .query_row(
                        "UPDATE notes SET body_md = ?1, updated_at = unixepoch() WHERE id = ?2
                         RETURNING id, message_id, thread_id, body_md, author, created_at, updated_at",
                        rusqlite::params![body_md, note_id],
                        Note::from_row,
                    )
                    .optional()?;
                let Some(note) = updated else {
                    return Ok(Err(Error::not_found(format!("note {note_id} not found"))));
                };
                let hashes = if index_enabled {
                    reindex(&tx, note.target)?
                } else {
                    Vec::new()
                };
                tx.commit()?;
                Ok(Ok((note, hashes)))
            })
            .await?;
        let (note, hashes) = outcome?;

        self.enqueue_reindex(hashes).await?;
        let _ = self.tx.send(NoteChange::Edited(note.clone()));
        Ok(note)
    }

    /// Delete a note.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] if no note has `note_id`. Otherwise a mapped
    /// storage error.
    #[tracing::instrument(skip(self))]
    pub async fn delete(&self, note_id: i64) -> Result<(), Error> {
        let index_enabled = self.index_enabled;

        let outcome = self
            .db
            .write(move |conn| {
                let tx = conn.transaction()?;
                let deleted: Option<Note> = tx
                    .query_row(
                        "DELETE FROM notes WHERE id = ?1
                         RETURNING id, message_id, thread_id, body_md, author, created_at, updated_at",
                        [note_id],
                        Note::from_row,
                    )
                    .optional()?;
                let Some(prior) = deleted else {
                    return Ok(Err(Error::not_found(format!("note {note_id} not found"))));
                };
                let hashes = if index_enabled {
                    reindex(&tx, prior.target)?
                } else {
                    Vec::new()
                };
                tx.commit()?;
                Ok(Ok((prior, hashes)))
            })
            .await?;
        let (prior, hashes) = outcome?;

        self.enqueue_reindex(hashes).await?;
        let _ = self.tx.send(NoteChange::Deleted {
            id: prior.id,
            target: prior.target,
        });
        Ok(())
    }

    /// List a target's notes, newest first.
    ///
    /// A target naming a message/thread that does not exist is not an
    /// error — it simply has no notes, the same answer an existing target
    /// with none would give. `AddNote`'s foreign-key check is what actually
    /// guards target validity; a list read has no write to piggyback that
    /// check on and no reason to pay for a second lookup just to say what an
    /// empty result already says.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    pub async fn list(&self, target: Target) -> Result<Vec<Note>, Error> {
        let (message_id, thread_id) = target.columns();
        Ok(self
            .db
            .read(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, message_id, thread_id, body_md, author, created_at, updated_at
                     FROM notes WHERE message_id IS ?1 AND thread_id IS ?2
                     ORDER BY created_at DESC, id DESC",
                )?;
                let rows = stmt
                    .query_map(rusqlite::params![message_id, thread_id], Note::from_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?)
    }

    /// Queue the lexical/entity/semantic follow-on jobs for every message a
    /// write touched — mirrors `index::extract::extract_message`'s own
    /// follow-on set (task 18) so a note is picked up by exactly the stages a
    /// changed body/subject would be.
    async fn enqueue_reindex(&self, hashes: Vec<(i64, Vec<u8>)>) -> Result<(), Error> {
        if hashes.is_empty() {
            return Ok(());
        }
        let jobs: Vec<NewJob> = hashes
            .into_iter()
            .flat_map(|(message_id, hash)| {
                [IndexKind::Lexical, IndexKind::Entities, IndexKind::Semantic]
                    .into_iter()
                    .map(move |kind| {
                        NewJob::new(message_id, kind)
                            .content_hash(hash.clone())
                            .priority(PRIORITY_NORMAL)
                    })
            })
            .collect();
        self.index_queue.enqueue(jobs, None).await?;
        Ok(())
    }
}

/// Whether `err` is the foreign-key violation `V23`'s `notes.message_id`/
/// `notes.thread_id` raise for a target that does not exist.
///
/// Matched by SQLite's structured extended result code, not message text —
/// the same discipline `index::fts::malformed_query` documents: message
/// wording is not a contract, a `SQLITE_CONSTRAINT_FOREIGNKEY` is. This is
/// also what tells a bad target apart from the XOR `CHECK` violation a
/// caller of this module can never actually trigger (every write path here
/// goes through [`Target::columns`], which always sets exactly one column) —
/// the `CHECK` exists for write paths *outside* this module, per the module
/// docs, and this function has no reason to special-case it.
fn is_missing_target(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(inner, _)
            if inner.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY
    )
}

/// Recompute and re-store [`Part::Note`]'s `index_content` row for every
/// message `target` affects, inside the caller's already-open transaction —
/// so it is atomic with the `notes` write that triggered it, and returns
/// each touched message's up-to-date content hash for the caller to enqueue
/// follow-on index jobs with once the transaction commits (mirroring
/// `ai::deep::feed_index`'s own upsert-then-enqueue shape, task 49).
///
/// A [`Target::Thread`] touches every message currently in the thread —
/// see [`affected_messages`] — which is what makes a thread-targeted note
/// searchable from any message in the conversation, matching how the TUI
/// renders it (prd.md: notes shown in the preview pane regardless of which
/// message in the thread is open).
fn reindex(tx: &Transaction<'_>, target: Target) -> rusqlite::Result<Vec<(i64, Vec<u8>)>> {
    affected_messages(tx, target)?
        .into_iter()
        .map(|message_id| refresh_note_index(tx, message_id).map(|hash| (message_id, hash)))
        .collect()
}

/// The messages a note write against `target` must re-feed into the lexical
/// index: just the one message for [`Target::Message`], every message
/// currently in the thread for [`Target::Thread`].
fn affected_messages(tx: &Transaction<'_>, target: Target) -> rusqlite::Result<Vec<i64>> {
    match target {
        Target::Message(id) => Ok(vec![id]),
        Target::Thread(id) => repo::list_thread_message_ids(tx, id),
    }
}

/// Recompute `message_id`'s *effective* note text — its own notes plus its
/// thread's, if it has one — normalize it, and replace its
/// `index_content(message_id, part = 'note')` row (or remove it, if the
/// effective text is now empty). Returns the message's up-to-date content
/// hash, read back inside this same transaction so it describes exactly what
/// a follow-on index stage will find — the identical discipline
/// `index::extract::store`/`ai::deep::feed_index` document for their own
/// version of this read.
///
/// `OR thread_id = (SELECT thread_id FROM messages WHERE id = ?1)` is the
/// "effective" half: a thread-targeted note has no `message_id` of its own,
/// so the only way it reaches a particular message's index row is by this
/// join. A message with no thread (`thread_id IS NULL`) simply never matches
/// that arm, which is correct — there is no thread for a thread-note to have
/// been attached to.
fn refresh_note_index(tx: &Transaction<'_>, message_id: i64) -> rusqlite::Result<Vec<u8>> {
    let raw: String = tx.query_row(
        "SELECT COALESCE(group_concat(body_md, ' '), '') FROM notes
         WHERE message_id = ?1 OR thread_id = (SELECT thread_id FROM messages WHERE id = ?1)",
        [message_id],
        |row| row.get(0),
    )?;
    let text = extract::normalize(&raw);
    let note_key = Part::Note.as_key();

    if text.is_empty() {
        tx.execute(
            "DELETE FROM index_content WHERE message_id = ?1 AND part = ?2",
            rusqlite::params![message_id, note_key],
        )?;
    } else {
        let chars = i64::try_from(text.chars().count()).unwrap_or(i64::MAX);
        let part_hash = Sha256::digest(text.as_bytes()).to_vec();
        tx.execute(
            "INSERT INTO index_content
                 (message_id, part, text, chars, content_hash, extracted_at, extractor)
             VALUES (?1, ?2, ?3, ?4, ?5, unixepoch(), ?6)
             ON CONFLICT(message_id, part) DO UPDATE SET
                 text = excluded.text,
                 chars = excluded.chars,
                 content_hash = excluded.content_hash,
                 extracted_at = excluded.extracted_at,
                 extractor = excluded.extractor",
            rusqlite::params![
                message_id,
                note_key,
                text,
                chars,
                part_hash,
                extract::EXTRACTOR
            ],
        )?;
    }

    let stored: Vec<(String, Vec<u8>)> = {
        let mut stmt =
            tx.prepare("SELECT part, content_hash FROM index_content WHERE message_id = ?1")?;
        let rows = stmt
            .query_map([message_id], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    Ok(extract::message_hash(&stored))
}

#[cfg(test)]
mod tests;
