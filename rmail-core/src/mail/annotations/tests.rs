//! Escrow semantics for [`super::capture`]/[`super::replay`]/[`super::expire`].
//!
//! The end-to-end proof that a real `Move` followed by a real resync keeps a
//! user's tags and notes lives in `mail::tests` — it needs `MailStore` and the
//! fetch path, which these do not. What is proved here is the behaviour those
//! two halves rely on at the edges: what is *not* escrowed, what happens when
//! the message resurfaces somewhere other than where it was sent, what happens
//! when the tag it pointed at is gone by then, and that nothing accumulates
//! forever.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use rusqlite::Connection;

use crate::repo::{self, NewAccount, NewMailbox, NewMessage};
use crate::storage::Database;

use super::*;

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Fixture {
    db: Database,
    path: PathBuf,
    account_id: i64,
    inbox_id: i64,
    archive_id: i64,
}

const HEADER: &str = "moved@example.com";

impl Fixture {
    fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-annot-{pid}-{n}.db"));
        let db = Database::open(&path).unwrap();
        let (account_id, inbox_id, archive_id) = db
            .with_write(|c| {
                let account_id = repo::insert_account(
                    c,
                    &NewAccount {
                        name: "Personal".to_owned(),
                        ..Default::default()
                    },
                )?;
                let inbox_id = repo::insert_mailbox(
                    c,
                    &NewMailbox {
                        account_id,
                        name: "INBOX".to_owned(),
                        ..Default::default()
                    },
                )?;
                let archive_id = repo::insert_mailbox(
                    c,
                    &NewMailbox {
                        account_id,
                        name: "Archive".to_owned(),
                        ..Default::default()
                    },
                )?;
                Ok((account_id, inbox_id, archive_id))
            })
            .unwrap();
        Self {
            db,
            path,
            account_id,
            inbox_id,
            archive_id,
        }
    }

    /// A message in `mailbox_id` carrying `header` as its `Message-ID`.
    fn message(&self, mailbox_id: i64, uid: i64, header: Option<&str>) -> i64 {
        self.db
            .with_write(|c| {
                repo::insert_message(
                    c,
                    &NewMessage {
                        account_id: self.account_id,
                        mailbox_id,
                        uid,
                        uidvalidity: 1,
                        message_id: header.map(ToOwned::to_owned),
                        subject: Some("Hi".to_owned()),
                        ..Default::default()
                    },
                )
            })
            .unwrap()
    }

    /// Raw SQL rather than `TagStore`/`NoteStore`: what is under test is the
    /// escrow, and driving the annotation tables directly keeps this
    /// independent of either API's own conventions.
    fn tag(&self, name: &str) -> i64 {
        self.db
            .with_write(|c| {
                c.execute(
                    "INSERT INTO tags (account_id, name) VALUES (?1, ?2)",
                    rusqlite::params![self.account_id, name],
                )?;
                Ok(c.last_insert_rowid())
            })
            .unwrap()
    }

    fn apply_tag(&self, tag_id: i64, message_id: i64) {
        self.db
            .with_write(|c| {
                c.execute(
                    "INSERT INTO message_tags (tag_id, message_id, source, state, created_at)
                     VALUES (?1, ?2, 'user', 'applied', 1000)",
                    rusqlite::params![tag_id, message_id],
                )?;
                Ok(())
            })
            .unwrap();
    }

    fn note(&self, message_id: i64, body: &str) {
        self.db
            .with_write(|c| {
                c.execute(
                    "INSERT INTO notes (message_id, body_md, author, created_at, updated_at)
                     VALUES (?1, ?2, 'user', 1000, 2000)",
                    rusqlite::params![message_id, body],
                )?;
                Ok(())
            })
            .unwrap();
    }

    /// Escrow `message_id` for `dest`, then drop the message the way
    /// `MailStore::move_message` does.
    fn capture_and_remove(&self, message_id: i64, dest: i64) -> usize {
        self.db
            .with_write(|c| {
                let tx = c.transaction()?;
                let escrowed = capture(&tx, message_id, dest)?;
                crate::sync::remove_messages(&tx, &[message_id])?;
                tx.commit()?;
                Ok(escrowed)
            })
            .unwrap()
    }

    fn replay_onto(&self, message_id: i64, mailbox_id: i64, header: &str) -> usize {
        let header = header.to_owned();
        self.db
            .with_write(|c| {
                let tx = c.transaction()?;
                let restored = replay(&tx, message_id, mailbox_id, &header)?;
                tx.commit()?;
                Ok(restored)
            })
            .unwrap()
    }

    fn escrow_rows(&self) -> i64 {
        self.db
            .with_write(|c| c.query_row("SELECT COUNT(*) FROM moved_annotations", [], |r| r.get(0)))
            .unwrap()
    }

    fn tags_on(&self, message_id: i64) -> Vec<(i64, String, String, i64)> {
        self.db
            .with_write(|c| {
                let mut stmt = c.prepare(
                    "SELECT tag_id, source, state, created_at FROM message_tags
                     WHERE message_id = ?1 ORDER BY tag_id",
                )?;
                let rows = stmt.query_map([message_id], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap()
    }

    fn notes_on(&self, message_id: i64) -> Vec<(String, String, i64, i64)> {
        self.db
            .with_write(|c| {
                let mut stmt = c.prepare(
                    "SELECT body_md, author, created_at, updated_at FROM notes
                     WHERE message_id = ?1 ORDER BY id",
                )?;
                let rows = stmt.query_map([message_id], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
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

#[test]
fn capture_then_replay_restores_tags_and_notes_with_their_original_timestamps() {
    let fx = Fixture::open();
    let old = fx.message(fx.inbox_id, 42, Some(HEADER));
    // `messages.id` is a plain `INTEGER PRIMARY KEY`, so SQLite hands the
    // deleted row's rowid straight back to the next insert. A later message
    // keeps the counter past `old`, which is what makes the "the id really did
    // change" assertion below mean anything instead of passing by accident.
    let _later = fx.message(fx.inbox_id, 43, Some("unrelated@example.com"));
    let tag_id = fx.tag("invoices");
    fx.apply_tag(tag_id, old);
    fx.note(old, "chase this on Friday");

    assert_eq!(fx.capture_and_remove(old, fx.archive_id), 2);
    assert_eq!(
        fx.escrow_rows(),
        2,
        "one row per annotation, held in escrow"
    );

    // The destination folder syncs the message in under a brand new row id.
    let new = fx.message(fx.archive_id, 900, Some(HEADER));
    assert_ne!(new, old, "the move genuinely does not preserve messages.id");
    assert_eq!(fx.replay_onto(new, fx.archive_id, HEADER), 2);

    assert_eq!(
        fx.tags_on(new),
        vec![(tag_id, "user".to_owned(), "applied".to_owned(), 1000)],
        "the tag is back on the new row, still user-sourced and still applied"
    );
    assert_eq!(
        fx.notes_on(new),
        vec![(
            "chase this on Friday".to_owned(),
            "user".to_owned(),
            1000,
            2000
        )],
        "the note keeps the timestamps it was written with, not the move's"
    );
    assert_eq!(
        fx.escrow_rows(),
        0,
        "escrow is consumed, not left to expire"
    );
}

#[test]
fn a_message_without_a_message_id_header_is_not_escrowed_at_all() {
    let fx = Fixture::open();
    let old = fx.message(fx.inbox_id, 42, None);
    let tag_id = fx.tag("invoices");
    fx.apply_tag(tag_id, old);
    fx.note(old, "no header to find this by");

    assert_eq!(
        fx.capture_and_remove(old, fx.archive_id),
        0,
        "nothing can re-identify it after the move, so nothing is written"
    );
    assert_eq!(fx.escrow_rows(), 0);
}

#[test]
fn escrow_is_scoped_to_the_destination_so_the_source_cannot_reclaim_it() {
    let fx = Fixture::open();
    let old = fx.message(fx.inbox_id, 42, Some(HEADER));
    let tag_id = fx.tag("invoices");
    fx.apply_tag(tag_id, old);
    assert_eq!(fx.capture_and_remove(old, fx.archive_id), 1);

    // The source folder resyncs first — an IMAP move the source has not yet
    // caught up with. It must not consume the escrow: the next expunge would
    // delete this copy and take the tag with it for good.
    let resurfaced_in_source = fx.message(fx.inbox_id, 43, Some(HEADER));
    assert_eq!(
        fx.replay_onto(resurfaced_in_source, fx.inbox_id, HEADER),
        0,
        "a copy in the source mailbox is not the move's destination"
    );
    assert_eq!(fx.escrow_rows(), 1, "still held for the real destination");

    let arrived = fx.message(fx.archive_id, 900, Some(HEADER));
    assert_eq!(fx.replay_onto(arrived, fx.archive_id, HEADER), 1);
    assert_eq!(fx.tags_on(arrived).len(), 1);
    assert!(fx.tags_on(resurfaced_in_source).is_empty());
}

#[test]
fn a_tag_deleted_mid_move_is_dropped_without_failing_the_sync_or_leaking_escrow() {
    let fx = Fixture::open();
    let old = fx.message(fx.inbox_id, 42, Some(HEADER));
    let tag_id = fx.tag("invoices");
    fx.apply_tag(tag_id, old);
    fx.note(old, "the note still comes back");
    assert_eq!(fx.capture_and_remove(old, fx.archive_id), 2);

    // The user deletes the tag itself before the destination syncs. The
    // escrowed row now points at a `tags` id that no longer exists; a plain
    // insert would fail the foreign key and take the whole message insert
    // down with it.
    fx.db
        .with_write(|c| {
            c.execute("DELETE FROM tags WHERE id = ?1", [tag_id])?;
            Ok(())
        })
        .unwrap();

    let new = fx.message(fx.archive_id, 900, Some(HEADER));
    assert_eq!(
        fx.replay_onto(new, fx.archive_id, HEADER),
        1,
        "the note is restored; the tag has nowhere to point and is dropped"
    );
    assert!(fx.tags_on(new).is_empty());
    assert_eq!(fx.notes_on(new).len(), 1);
    assert_eq!(
        fx.escrow_rows(),
        0,
        "the unrestorable row is consumed too, not left to sit until it expires"
    );
}

#[test]
fn replaying_twice_does_not_duplicate_an_annotation() {
    let fx = Fixture::open();
    let old = fx.message(fx.inbox_id, 42, Some(HEADER));
    let tag_id = fx.tag("invoices");
    fx.apply_tag(tag_id, old);
    fx.capture_and_remove(old, fx.archive_id);

    let new = fx.message(fx.archive_id, 900, Some(HEADER));
    assert_eq!(fx.replay_onto(new, fx.archive_id, HEADER), 1);
    assert_eq!(
        fx.replay_onto(new, fx.archive_id, HEADER),
        0,
        "the escrow is gone, so a second sync of the same folder finds nothing"
    );
    assert_eq!(fx.tags_on(new).len(), 1);
}

#[test]
fn thread_level_annotations_are_left_alone() {
    let fx = Fixture::open();
    let old = fx.message(fx.inbox_id, 42, Some(HEADER));
    let tag_id = fx.tag("invoices");
    let account_id = fx.account_id;
    let thread_id = fx
        .db
        .with_write(|c| {
            c.execute("INSERT INTO threads (account_id) VALUES (?1)", [account_id])?;
            let thread_id = c.last_insert_rowid();
            c.execute(
                "UPDATE messages SET thread_id = ?1 WHERE id = ?2",
                rusqlite::params![thread_id, old],
            )?;
            c.execute(
                "INSERT INTO message_tags (tag_id, thread_id, source, state)
                 VALUES (?1, ?2, 'user', 'applied')",
                rusqlite::params![tag_id, thread_id],
            )?;
            Ok(thread_id)
        })
        .unwrap();

    // Moving the thread's only message empties it, and `repair_threads`
    // deletes an empty thread — so the thread-level tag cascades away exactly
    // like a message-level one, and has to be escrowed too.
    assert_eq!(fx.capture_and_remove(old, fx.archive_id), 1);
    let thread_gone: i64 = fx
        .db
        .with_write(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM threads WHERE id = ?1",
                [thread_id],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(thread_gone, 0, "the emptied thread was reaped");

    // On resync the message joins a *new* thread, and the tag has to land on
    // that one.
    let new = fx.message(fx.archive_id, 900, Some(HEADER));
    let new_thread = fx
        .db
        .with_write(|c| {
            c.execute("INSERT INTO threads (account_id) VALUES (?1)", [account_id])?;
            let new_thread = c.last_insert_rowid();
            c.execute(
                "UPDATE messages SET thread_id = ?1 WHERE id = ?2",
                rusqlite::params![new_thread, new],
            )?;
            Ok(new_thread)
        })
        .unwrap();
    assert_eq!(fx.replay_onto(new, fx.archive_id, HEADER), 1);

    let restored: i64 = fx
        .db
        .with_write(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM message_tags WHERE thread_id = ?1 AND tag_id = ?2",
                rusqlite::params![new_thread, tag_id],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(restored, 1, "the thread tag landed on the new conversation");
}

#[test]
fn a_thread_that_keeps_other_messages_is_not_escrowed_and_is_not_double_tagged() {
    let fx = Fixture::open();
    let old = fx.message(fx.inbox_id, 42, Some(HEADER));
    let sibling = fx.message(fx.inbox_id, 43, Some("sibling@example.com"));
    let tag_id = fx.tag("invoices");
    let account_id = fx.account_id;
    let thread_id = fx
        .db
        .with_write(|c| {
            c.execute("INSERT INTO threads (account_id) VALUES (?1)", [account_id])?;
            let thread_id = c.last_insert_rowid();
            c.execute(
                "UPDATE messages SET thread_id = ?1 WHERE id IN (?2, ?3)",
                rusqlite::params![thread_id, old, sibling],
            )?;
            c.execute(
                "INSERT INTO message_tags (tag_id, thread_id, source, state)
                 VALUES (?1, ?2, 'user', 'applied')",
                rusqlite::params![tag_id, thread_id],
            )?;
            Ok(thread_id)
        })
        .unwrap();

    assert_eq!(
        fx.capture_and_remove(old, fx.archive_id),
        0,
        "the thread still has a message in it, so nothing was lost to escrow"
    );
    let surviving: i64 = fx
        .db
        .with_write(|c| {
            c.query_row(
                "SELECT COUNT(*) FROM message_tags WHERE thread_id = ?1",
                [thread_id],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(surviving, 1, "the thread tag was never at risk");
}

#[test]
fn expire_reaps_only_rows_older_than_the_window() {
    let fx = Fixture::open();
    let old = fx.message(fx.inbox_id, 42, Some(HEADER));
    let tag_id = fx.tag("invoices");
    fx.apply_tag(tag_id, old);
    fx.capture_and_remove(old, fx.archive_id);
    assert_eq!(fx.escrow_rows(), 1);

    let reaped = fx.db.with_write(|c| expire(c)).unwrap();
    assert_eq!(reaped, 0, "a fresh row is nowhere near the window");

    // Age it past the window.
    let window = i64::try_from(EXPIRY.as_secs()).unwrap();
    fx.db
        .with_write(|c: &mut Connection| {
            c.execute(
                "UPDATE moved_annotations SET created_at = unixepoch() - ?1",
                [window + 1],
            )?;
            Ok(())
        })
        .unwrap();

    let reaped = fx.db.with_write(|c| expire(c)).unwrap();
    assert_eq!(reaped, 1);
    assert_eq!(fx.escrow_rows(), 0);
}
