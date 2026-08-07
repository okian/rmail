//! What task 56 owes: the message-or-thread target is a XOR enforced by the
//! schema itself (not only this module's Rust API), `notes`/`notes_fts` stay
//! in sync for every raw-SQL write path including a `DELETE FROM messages`
//! cascade, `NoteStore`'s CRUD is last-write-wins with no lost-update guard,
//! and adding/editing/deleting a note feeds `index_content` under
//! [`Part::Note`] for exactly the messages an "effective note" (a message's
//! own, plus its thread's) should reach.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use super::*;
use crate::index::QueueOptions as IndexQueueOptions;
use crate::repo;
use crate::ErrorReason;

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Fixture {
    db: Database,
    queue: IndexQueue,
    account_id: i64,
    mailbox_id: i64,
    next_uid: std::sync::atomic::AtomicI64,
    path: PathBuf,
}

impl Fixture {
    fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-notes-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", path.display())));
        }
        let db = Database::open(&path).unwrap();
        let (account_id, mailbox_id) = db
            .with_write(|c| {
                let account_id = repo::insert_account(
                    c,
                    &repo::NewAccount {
                        name: "Personal".to_owned(),
                        ..Default::default()
                    },
                )?;
                let mailbox_id = repo::insert_mailbox(
                    c,
                    &repo::NewMailbox {
                        account_id,
                        name: "INBOX".to_owned(),
                        ..Default::default()
                    },
                )?;
                Ok((account_id, mailbox_id))
            })
            .unwrap();
        Self {
            queue: IndexQueue::new(db.clone(), IndexQueueOptions::default()),
            db,
            account_id,
            mailbox_id,
            next_uid: std::sync::atomic::AtomicI64::new(1),
            path,
        }
    }

    fn store(&self) -> NoteStore {
        NoteStore::new(self.db.clone(), self.queue.clone(), true)
    }

    fn store_without_indexing(&self) -> NoteStore {
        NoteStore::new(self.db.clone(), self.queue.clone(), false)
    }

    /// Insert a bare message (no thread), returning its id.
    fn message(&self) -> i64 {
        self.message_in_thread(None)
    }

    fn message_in_thread(&self, thread_id: Option<i64>) -> i64 {
        let uid = self.next_uid.fetch_add(1, Ordering::Relaxed);
        let account_id = self.account_id;
        let mailbox_id = self.mailbox_id;
        self.db
            .with_write(move |c| {
                repo::insert_message(
                    c,
                    &repo::NewMessage {
                        account_id,
                        mailbox_id,
                        uid,
                        uidvalidity: 1,
                        thread_id,
                        subject: Some("hello".to_owned()),
                        ..Default::default()
                    },
                )
            })
            .unwrap()
    }

    fn thread(&self) -> i64 {
        let account_id = self.account_id;
        self.db
            .with_write(move |c| {
                repo::insert_thread(
                    c,
                    &repo::NewThread {
                        account_id,
                        ..Default::default()
                    },
                )
            })
            .unwrap()
    }

    /// Every message currently in `thread_id`.
    fn thread_message_ids(&self, thread_id: i64) -> Vec<i64> {
        self.db
            .with_read(move |c| repo::list_thread_message_ids(c, thread_id))
            .unwrap()
    }

    // -- raw SQL, deliberately bypassing `NoteStore` -- proving the schema's
    // own invariants hold for *any* write path, per this module's own docs.

    fn insert_note_raw(
        &self,
        message_id: Option<i64>,
        thread_id: Option<i64>,
        body_md: &str,
    ) -> Result<i64, crate::storage::StorageError> {
        let body_md = body_md.to_owned();
        self.db.with_write(move |c| {
            c.execute(
                "INSERT INTO notes (message_id, thread_id, body_md) VALUES (?1, ?2, ?3)",
                rusqlite::params![message_id, thread_id, body_md],
            )?;
            Ok(c.last_insert_rowid())
        })
    }

    fn update_note_raw(&self, id: i64, body_md: &str) {
        let body_md = body_md.to_owned();
        self.db
            .with_write(move |c| {
                c.execute(
                    "UPDATE notes SET body_md = ?1, updated_at = unixepoch() WHERE id = ?2",
                    rusqlite::params![body_md, id],
                )
            })
            .unwrap();
    }

    fn delete_note_raw(&self, id: i64) {
        self.db
            .with_write(move |c| c.execute("DELETE FROM notes WHERE id = ?1", [id]))
            .unwrap();
    }

    fn delete_message_raw(&self, id: i64) {
        self.db
            .with_write(move |c| c.execute("DELETE FROM messages WHERE id = ?1", [id]))
            .unwrap();
    }

    fn notes_count_for_message(&self, message_id: i64) -> i64 {
        self.db
            .with_read(move |c| {
                c.query_row(
                    "SELECT count(*) FROM notes WHERE message_id = ?1",
                    [message_id],
                    |r| r.get(0),
                )
            })
            .unwrap()
    }

    fn notes_fts_row_count(&self, note_id: i64) -> i64 {
        self.db
            .with_read(move |c| {
                c.query_row(
                    "SELECT count(*) FROM notes_fts WHERE rowid = ?1",
                    [note_id],
                    |r| r.get(0),
                )
            })
            .unwrap()
    }

    fn notes_fts_matches(&self, note_id: i64, term: &str) -> bool {
        let term = term.to_owned();
        self.db
            .with_read(move |c| {
                c.query_row(
                    "SELECT count(*) FROM notes_fts WHERE rowid = ?1 AND notes_fts MATCH ?2",
                    rusqlite::params![note_id, term],
                    |r| r.get::<_, i64>(0),
                )
            })
            .map(|count| count > 0)
            .unwrap()
    }

    /// The stored `index_content(part = 'note')` text for `message_id`, if
    /// any.
    fn note_index_text(&self, message_id: i64) -> Option<String> {
        self.db
            .with_read(move |c| {
                c.query_row(
                    "SELECT text FROM index_content WHERE message_id = ?1 AND part = 'note'",
                    [message_id],
                    |r| r.get(0),
                )
                .optional()
            })
            .unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.path.display())));
        }
    }
}

// ---------------------------------------------------------------------------
// The XOR target -- a schema-level CHECK constraint, not only Rust.
// ---------------------------------------------------------------------------

#[test]
fn database_rejects_a_note_targeting_both_a_message_and_a_thread() {
    let fx = Fixture::open();
    let message_id = fx.message();
    let thread_id = fx.thread();

    let result = fx.insert_note_raw(Some(message_id), Some(thread_id), "both");
    assert!(
        result.is_err(),
        "V23's CHECK ((message_id IS NULL) <> (thread_id IS NULL)) must reject a row \
         naming both a message and a thread, even via raw SQL that never touches NoteStore"
    );
}

#[test]
fn database_rejects_a_note_targeting_neither_a_message_nor_a_thread() {
    let fx = Fixture::open();
    let result = fx.insert_note_raw(None, None, "neither");
    assert!(
        result.is_err(),
        "the same CHECK constraint must reject a row naming neither target"
    );
}

#[test]
fn database_accepts_exactly_one_of_message_or_thread() {
    let fx = Fixture::open();
    let message_id = fx.message();
    let thread_id = fx.thread();

    assert!(fx
        .insert_note_raw(Some(message_id), None, "on a message")
        .is_ok());
    assert!(fx
        .insert_note_raw(None, Some(thread_id), "on a thread")
        .is_ok());
}

// ---------------------------------------------------------------------------
// notes_fts trigger sync -- raw SQL only, the model being
// `ai::triage::tests::ai_fts_stays_in_sync_across_multiple_passes_upserts_deletes_and_message_cascade`.
// ---------------------------------------------------------------------------

#[test]
fn notes_fts_stays_in_sync_across_insert_update_delete_and_message_cascade() {
    let fx = Fixture::open();
    let message_id = fx.message();

    // 1. Insert directly via raw SQL -- notes_fts gains exactly one row,
    //    matching its text.
    let note_id = fx
        .insert_note_raw(Some(message_id), None, "roadmap tldr")
        .unwrap();
    assert_eq!(fx.notes_fts_row_count(note_id), 1);
    assert!(fx.notes_fts_matches(note_id, "roadmap"));

    // 2. A second, independent note on the same message -- its own rowid,
    //    and does not disturb the first.
    let note_id_2 = fx
        .insert_note_raw(Some(message_id), None, "quarterly numbers")
        .unwrap();
    assert_eq!(fx.notes_fts_row_count(note_id_2), 1);
    assert!(
        fx.notes_fts_matches(note_id, "roadmap"),
        "sibling insert leaves the first row alone"
    );
    assert!(fx.notes_fts_matches(note_id_2, "quarterly"));

    // 3. Update the first note via raw SQL -- the superseded text's tokens
    //    must be gone, not merely supplemented (delete-then-reinsert, not
    //    an in-place FTS5 update).
    fx.update_note_raw(note_id, "budget review");
    assert_eq!(fx.notes_fts_row_count(note_id), 1);
    assert!(
        !fx.notes_fts_matches(note_id, "roadmap"),
        "the superseded text must not still match"
    );
    assert!(fx.notes_fts_matches(note_id, "budget"));
    assert!(
        fx.notes_fts_matches(note_id_2, "quarterly"),
        "the sibling note is untouched by the first note's update"
    );

    // 4. Delete the first note via raw SQL -- its row is gone, the sibling
    //    survives.
    fx.delete_note_raw(note_id);
    assert_eq!(fx.notes_fts_row_count(note_id), 0);
    assert!(fx.notes_fts_matches(note_id_2, "quarterly"));

    // 5. `DELETE FROM messages` cascades to `notes` (ON DELETE CASCADE),
    //    which must in turn clean up `notes_fts` via the same delete
    //    trigger -- the acceptance case this task explicitly names.
    fx.delete_message_raw(message_id);
    assert_eq!(
        fx.notes_count_for_message(message_id),
        0,
        "ON DELETE CASCADE must remove notes rows when the message is deleted"
    );
    assert_eq!(
        fx.notes_fts_row_count(note_id_2),
        0,
        "the cascaded notes delete must reach notes_fts too"
    );
}

// ---------------------------------------------------------------------------
// NoteStore CRUD
// ---------------------------------------------------------------------------

#[tokio::test]
async fn add_edit_delete_and_list_round_trip_on_a_message_target() {
    let fx = Fixture::open();
    let store = fx.store();
    let message_id = fx.message();

    let note = store
        .add(NewNote {
            target: Target::Message(message_id),
            body_md: "  # heading\n\nsome *markdown*  ".to_owned(),
            author: NoteAuthor::User,
        })
        .await
        .unwrap();
    assert_eq!(note.target, Target::Message(message_id));
    assert_eq!(note.author, NoteAuthor::User);
    // Markdown is stored verbatim (trimmed of surrounding whitespace only) --
    // this module renders nothing and validates nothing about its shape, it
    // is a client-rendering concern.
    assert_eq!(note.body_md, "# heading\n\nsome *markdown*");
    assert_eq!(note.created_at, note.updated_at);

    let listed = store.list(Target::Message(message_id)).await.unwrap();
    assert_eq!(listed, vec![note.clone()]);

    let edited = store
        .edit(note.id, "revised body".to_owned())
        .await
        .unwrap();
    assert_eq!(edited.id, note.id);
    assert_eq!(edited.body_md, "revised body");
    assert!(edited.updated_at >= note.updated_at);

    let listed = store.list(Target::Message(message_id)).await.unwrap();
    assert_eq!(listed, vec![edited]);

    store.delete(note.id).await.unwrap();
    assert!(store
        .list(Target::Message(message_id))
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn add_and_list_round_trip_on_a_thread_target() {
    let fx = Fixture::open();
    let store = fx.store();
    let thread_id = fx.thread();

    let note = store
        .add(NewNote {
            target: Target::Thread(thread_id),
            body_md: "thread-wide note".to_owned(),
            author: NoteAuthor::Ai,
        })
        .await
        .unwrap();
    assert_eq!(note.target, Target::Thread(thread_id));
    assert_eq!(note.author, NoteAuthor::Ai);

    let listed = store.list(Target::Thread(thread_id)).await.unwrap();
    assert_eq!(listed, vec![note]);

    // A message-scoped list for a message that happens to live in this
    // thread is a *different* target -- a thread note is not itself listed
    // there (it is folded into the message's *index*, not its own note
    // list; the TUI renders thread notes from a `Target::Thread` list).
    let message_in_thread = fx.message_in_thread(Some(thread_id));
    assert!(store
        .list(Target::Message(message_in_thread))
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn listing_notes_for_a_target_with_none_is_an_empty_list_not_an_error() {
    let fx = Fixture::open();
    let store = fx.store();
    let message_id = fx.message();
    assert!(store
        .list(Target::Message(message_id))
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn adding_a_note_with_an_empty_body_is_rejected() {
    let fx = Fixture::open();
    let store = fx.store();
    let message_id = fx.message();

    let err = store
        .add(NewNote {
            target: Target::Message(message_id),
            body_md: "   \n  ".to_owned(),
            author: NoteAuthor::User,
        })
        .await
        .unwrap_err();
    assert_eq!(err.reason(), ErrorReason::InvalidArgument);
}

#[tokio::test]
async fn editing_a_note_with_an_empty_body_is_rejected() {
    let fx = Fixture::open();
    let store = fx.store();
    let message_id = fx.message();
    let note = store
        .add(NewNote {
            target: Target::Message(message_id),
            body_md: "keep me".to_owned(),
            author: NoteAuthor::User,
        })
        .await
        .unwrap();

    let err = store.edit(note.id, "".to_owned()).await.unwrap_err();
    assert_eq!(err.reason(), ErrorReason::InvalidArgument);

    // Rejected, not silently applied -- the original body must survive.
    let listed = store.list(Target::Message(message_id)).await.unwrap();
    assert_eq!(listed[0].body_md, "keep me");
}

#[tokio::test]
async fn adding_a_note_against_a_message_that_does_not_exist_is_not_found() {
    let fx = Fixture::open();
    let store = fx.store();
    let err = store
        .add(NewNote {
            target: Target::Message(999_999),
            body_md: "orphan".to_owned(),
            author: NoteAuthor::User,
        })
        .await
        .unwrap_err();
    assert_eq!(err.reason(), ErrorReason::NotFound);
}

#[tokio::test]
async fn adding_a_note_against_a_thread_that_does_not_exist_is_not_found() {
    let fx = Fixture::open();
    let store = fx.store();
    let err = store
        .add(NewNote {
            target: Target::Thread(999_999),
            body_md: "orphan".to_owned(),
            author: NoteAuthor::User,
        })
        .await
        .unwrap_err();
    assert_eq!(err.reason(), ErrorReason::NotFound);
}

#[tokio::test]
async fn editing_a_note_that_does_not_exist_is_not_found() {
    let fx = Fixture::open();
    let store = fx.store();
    let err = store.edit(999_999, "x".to_owned()).await.unwrap_err();
    assert_eq!(err.reason(), ErrorReason::NotFound);
}

#[tokio::test]
async fn deleting_a_note_that_does_not_exist_is_not_found() {
    let fx = Fixture::open();
    let store = fx.store();
    let err = store.delete(999_999).await.unwrap_err();
    assert_eq!(err.reason(), ErrorReason::NotFound);
}

// ---------------------------------------------------------------------------
// Last-write-wins
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_edits_are_last_write_wins_with_no_lost_update_guard() {
    let fx = Fixture::open();
    let store = fx.store();
    let message_id = fx.message();
    let note = store
        .add(NewNote {
            target: Target::Message(message_id),
            body_md: "v1".to_owned(),
            author: NoteAuthor::User,
        })
        .await
        .unwrap();

    // Two independent editors, neither aware of the other -- there is no
    // version/ETag on `EditNote`'s request shape for either to have carried,
    // so both succeed unconditionally rather than one failing a conflict
    // check the API does not offer.
    let first = store
        .edit(note.id, "v2 from editor A".to_owned())
        .await
        .unwrap();
    let second = store
        .edit(note.id, "v3 from editor B".to_owned())
        .await
        .unwrap();

    assert!(second.updated_at >= first.updated_at);
    let listed = store.list(Target::Message(message_id)).await.unwrap();
    assert_eq!(
        listed[0].body_md, "v3 from editor B",
        "the most recent commit wins outright -- no error, no merge, no rejection of the second writer"
    );
}

// ---------------------------------------------------------------------------
// Live change stream
// ---------------------------------------------------------------------------

#[tokio::test]
async fn watch_streams_add_edit_and_delete_live() {
    let fx = Fixture::open();
    let store = fx.store();
    let message_id = fx.message();
    let mut changes = store.watch();

    let note = store
        .add(NewNote {
            target: Target::Message(message_id),
            body_md: "first".to_owned(),
            author: NoteAuthor::User,
        })
        .await
        .unwrap();
    match changes.recv().await.unwrap() {
        NoteChange::Added(added) => assert_eq!(added.id, note.id),
        other => unreachable!("expected Added, got {other:?}"),
    }

    let edited = store.edit(note.id, "second".to_owned()).await.unwrap();
    match changes.recv().await.unwrap() {
        NoteChange::Edited(got) => assert_eq!(got.body_md, edited.body_md),
        other => unreachable!("expected Edited, got {other:?}"),
    }

    store.delete(note.id).await.unwrap();
    match changes.recv().await.unwrap() {
        NoteChange::Deleted { id, target } => {
            assert_eq!(id, note.id);
            assert_eq!(target, Target::Message(message_id));
        }
        other => unreachable!("expected Deleted, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Feeding the lexical index (`index_content`, `Part::Note`) -- what makes
// `note:`/free-text search actually surface a note, per the module docs'
// "Two FTS surfaces, not one" section.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn adding_a_message_targeted_note_feeds_index_content_and_deleting_it_clears_that_row() {
    let fx = Fixture::open();
    let store = fx.store();
    let message_id = fx.message();

    assert_eq!(fx.note_index_text(message_id), None);

    let note = store
        .add(NewNote {
            target: Target::Message(message_id),
            body_md: "please follow up by friday".to_owned(),
            author: NoteAuthor::User,
        })
        .await
        .unwrap();
    let indexed = fx
        .note_index_text(message_id)
        .expect("a note part after add");
    assert!(indexed.contains("follow up by friday"));

    store.delete(note.id).await.unwrap();
    assert_eq!(
        fx.note_index_text(message_id),
        None,
        "the last note going away must remove the index_content row, not leave stale text"
    );
}

#[tokio::test]
async fn a_thread_targeted_note_feeds_every_message_currently_in_the_thread() {
    let fx = Fixture::open();
    let store = fx.store();
    let thread_id = fx.thread();
    let a = fx.message_in_thread(Some(thread_id));
    let b = fx.message_in_thread(Some(thread_id));
    let outside = fx.message();

    store
        .add(NewNote {
            target: Target::Thread(thread_id),
            body_md: "shared context for the whole thread".to_owned(),
            author: NoteAuthor::User,
        })
        .await
        .unwrap();

    assert_eq!(fx.thread_message_ids(thread_id).len(), 2);
    for message_id in [a, b] {
        let text = fx
            .note_index_text(message_id)
            .expect("every message in the thread gets the effective note text");
        assert!(text.contains("shared context"));
    }
    assert_eq!(
        fx.note_index_text(outside),
        None,
        "a message outside the thread is untouched"
    );
}

#[tokio::test]
async fn a_message_note_folds_together_with_its_threads_note_in_that_messages_index_row() {
    let fx = Fixture::open();
    let store = fx.store();
    let thread_id = fx.thread();
    let message_id = fx.message_in_thread(Some(thread_id));

    store
        .add(NewNote {
            target: Target::Thread(thread_id),
            body_md: "thread-level heads-up".to_owned(),
            author: NoteAuthor::User,
        })
        .await
        .unwrap();
    store
        .add(NewNote {
            target: Target::Message(message_id),
            body_md: "message-specific detail".to_owned(),
            author: NoteAuthor::User,
        })
        .await
        .unwrap();

    let text = fx.note_index_text(message_id).unwrap();
    assert!(text.contains("thread-level heads-up"));
    assert!(text.contains("message-specific detail"));
}

#[tokio::test]
async fn indexing_disabled_never_touches_index_content() {
    let fx = Fixture::open();
    let store = fx.store_without_indexing();
    let message_id = fx.message();

    store
        .add(NewNote {
            target: Target::Message(message_id),
            body_md: "should not be indexed".to_owned(),
            author: NoteAuthor::User,
        })
        .await
        .unwrap();

    assert_eq!(
        fx.note_index_text(message_id),
        None,
        "config.notes.index = false must keep NoteStore from writing index_content at all"
    );
}
