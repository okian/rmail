use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::config::TagSyncMode;
use crate::storage::Database;

use super::super::model::{NewMessageTag, TagSource, TagState, Target};
use super::*;

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A unique temp database, cleaned up on drop -- see
/// `rmail_core::storage::tests::TempDbPath` for the identical, independently
/// duplicated pattern this crate uses everywhere a real (WAL-requiring, so
/// not `:memory:`) SQLite file is needed for a test.
struct TempDb(PathBuf, Database);

impl TempDb {
    fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-tagsrepo-{pid}-{n}.db"));
        let db = Database::open(&path).expect("open temp db");
        Self(path, db)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn db(&self) -> &Database {
        &self.1
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ =
                std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.path().display())));
        }
    }
}

/// Seed one account, returning its id.
fn seed_account(db: &Database) -> i64 {
    db.with_write(|conn| {
        crate::repo::insert_account(
            conn,
            &crate::repo::NewAccount {
                name: format!("acct-{}", COUNTER.fetch_add(1, Ordering::Relaxed)),
                ..Default::default()
            },
        )
    })
    .unwrap()
}

/// Seed one message in a fresh account+mailbox, returning its id.
fn seed_message(db: &Database) -> i64 {
    let account_id = seed_account(db);
    db.with_write(move |conn| {
        let mailbox_id = crate::repo::insert_mailbox(
            conn,
            &crate::repo::NewMailbox {
                account_id,
                name: "INBOX".to_owned(),
                ..Default::default()
            },
        )?;
        crate::repo::insert_message(
            conn,
            &crate::repo::NewMessage {
                account_id,
                mailbox_id,
                uid: 1,
                uidvalidity: 1,
                ..Default::default()
            },
        )
    })
    .unwrap()
}

#[test]
fn insert_and_fetch_a_tag_round_trips() {
    let tmp = TempDb::open();
    let account_id = seed_account(tmp.db());
    let id = tmp
        .db()
        .with_write(|conn| {
            insert_tag(
                conn,
                account_id,
                "work",
                None,
                Some("#7aa2f7"),
                TagSyncMode::Auto,
                None,
            )
        })
        .unwrap();

    let tag = tmp
        .db()
        .with_read(|conn| get_tag(conn, id))
        .unwrap()
        .expect("tag should exist");
    assert_eq!(tag.name, "work");
    assert_eq!(tag.account_id, account_id);
    assert_eq!(tag.color.as_deref(), Some("#7aa2f7"));
    assert_eq!(tag.sync_mode, TagSyncMode::Auto);
    assert_eq!(tag.parent_id, None);

    let by_name = tmp
        .db()
        .with_read(move |conn| get_tag_by_name(conn, account_id, "work"))
        .unwrap()
        .expect("get_tag_by_name should find it");
    assert_eq!(by_name.id, id);
}

#[test]
fn a_duplicate_account_and_name_is_rejected_by_the_schema() {
    let tmp = TempDb::open();
    let account_id = seed_account(tmp.db());
    tmp.db()
        .with_write(move |conn| {
            insert_tag(
                conn,
                account_id,
                "work",
                None,
                None,
                TagSyncMode::Auto,
                None,
            )
        })
        .unwrap();
    let err = tmp
        .db()
        .with_write(move |conn| {
            insert_tag(
                conn,
                account_id,
                "work",
                None,
                None,
                TagSyncMode::Auto,
                None,
            )
        })
        .expect_err("UNIQUE(account_id, name) must reject a duplicate");
    assert!(matches!(
        err,
        crate::storage::StorageError::Sqlite(rusqlite::Error::SqliteFailure(_, _))
    ));
}

#[test]
fn list_tags_with_counts_reflects_effective_applications() {
    let tmp = TempDb::open();
    let account_id = seed_account(tmp.db());
    let message_id = tmp
        .db()
        .with_write(move |conn| {
            let mailbox_id = crate::repo::insert_mailbox(
                conn,
                &crate::repo::NewMailbox {
                    account_id,
                    name: "INBOX".to_owned(),
                    ..Default::default()
                },
            )?;
            crate::repo::insert_message(
                conn,
                &crate::repo::NewMessage {
                    account_id,
                    mailbox_id,
                    uid: 1,
                    uidvalidity: 1,
                    ..Default::default()
                },
            )
        })
        .unwrap();

    let tag_id = tmp
        .db()
        .with_write(move |conn| {
            insert_tag(
                conn,
                account_id,
                "work",
                None,
                None,
                TagSyncMode::Local,
                None,
            )
        })
        .unwrap();
    // A second tag with zero applications must still be listed.
    tmp.db()
        .with_write(move |conn| {
            insert_tag(
                conn,
                account_id,
                "empty",
                None,
                None,
                TagSyncMode::Local,
                None,
            )
        })
        .unwrap();

    tmp.db()
        .with_write(move |conn| {
            insert_message_tag(
                conn,
                &NewMessageTag {
                    tag_id,
                    target: Target::Message(message_id),
                    source: TagSource::User,
                    state: TagState::Applied,
                    confidence: None,
                    rationale: None,
                },
            )
        })
        .unwrap();

    let listed = tmp
        .db()
        .with_read(move |conn| list_tags_with_counts(conn, account_id))
        .unwrap();
    assert_eq!(listed.len(), 2);
    let work = listed.iter().find(|t| t.tag.name == "work").unwrap();
    assert_eq!(work.message_count, 1);
    let empty = listed.iter().find(|t| t.tag.name == "empty").unwrap();
    assert_eq!(empty.message_count, 0);
}

#[test]
fn insert_message_tag_is_idempotent_via_the_schemas_unique_index() {
    let tmp = TempDb::open();
    let message_id = seed_message(tmp.db());
    let account_id = tmp
        .db()
        .with_read(move |conn| crate::repo::get_message(conn, message_id))
        .unwrap()
        .unwrap()
        .account_id;
    let tag_id = tmp
        .db()
        .with_write(move |conn| {
            insert_tag(
                conn,
                account_id,
                "work",
                None,
                None,
                TagSyncMode::Local,
                None,
            )
        })
        .unwrap();

    let new = NewMessageTag {
        tag_id,
        target: Target::Message(message_id),
        source: TagSource::User,
        state: TagState::Applied,
        confidence: None,
        rationale: None,
    };
    let first = tmp
        .db()
        .with_write({
            let new = new.clone();
            move |conn| insert_message_tag(conn, &new)
        })
        .unwrap();
    assert!(first.is_some(), "first apply must create a row");

    let second = tmp
        .db()
        .with_write(move |conn| insert_message_tag(conn, &new))
        .unwrap();
    assert!(
        second.is_none(),
        "a duplicate apply must be a no-op, not a second row or an error"
    );

    let count: i64 = tmp
        .db()
        .with_read(|conn| conn.query_row("SELECT count(*) FROM message_tags", [], |r| r.get(0)))
        .unwrap();
    assert_eq!(count, 1, "exactly one row must exist after the duplicate");
}

#[test]
fn message_and_thread_level_applications_of_the_same_tag_both_succeed() {
    // The two partial unique indexes are scoped independently -- a
    // message-level and a thread-level application of the *same* tag must
    // not collide with each other, only with a second copy of themselves.
    let tmp = TempDb::open();
    let message_id = seed_message(tmp.db());
    let (account_id, thread_id) = tmp
        .db()
        .with_write(move |conn| {
            let message = crate::repo::get_message(conn, message_id)?.unwrap();
            let thread_id = crate::repo::insert_thread(
                conn,
                &crate::repo::NewThread {
                    account_id: message.account_id,
                    ..Default::default()
                },
            )?;
            Ok::<_, rusqlite::Error>((message.account_id, thread_id))
        })
        .unwrap();
    let tag_id = tmp
        .db()
        .with_write(move |conn| {
            insert_tag(
                conn,
                account_id,
                "work",
                None,
                None,
                TagSyncMode::Local,
                None,
            )
        })
        .unwrap();

    let message_applied = tmp
        .db()
        .with_write(move |conn| {
            insert_message_tag(
                conn,
                &NewMessageTag {
                    tag_id,
                    target: Target::Message(message_id),
                    source: TagSource::User,
                    state: TagState::Applied,
                    confidence: None,
                    rationale: None,
                },
            )
        })
        .unwrap();
    let thread_applied = tmp
        .db()
        .with_write(move |conn| {
            insert_message_tag(
                conn,
                &NewMessageTag {
                    tag_id,
                    target: Target::Thread(thread_id),
                    source: TagSource::User,
                    state: TagState::Applied,
                    confidence: None,
                    rationale: None,
                },
            )
        })
        .unwrap();
    assert!(message_applied.is_some());
    assert!(thread_applied.is_some());
}

#[test]
fn effective_tags_does_not_double_count_a_message_tagged_both_ways() {
    // A message that is *itself* a member of the thread, tagged both
    // directly and via its thread with the same tag, must still appear
    // exactly once in `messages_tags_effective` -- see migration V24's
    // `DISTINCT` comment for why a plain join would surface it twice.
    let tmp = TempDb::open();
    let account_id = seed_account(tmp.db());
    let (thread_id, message_id) = tmp
        .db()
        .with_write(move |conn| {
            let thread_id = crate::repo::insert_thread(
                conn,
                &crate::repo::NewThread {
                    account_id,
                    ..Default::default()
                },
            )?;
            let mailbox_id = crate::repo::insert_mailbox(
                conn,
                &crate::repo::NewMailbox {
                    account_id,
                    name: "INBOX".to_owned(),
                    ..Default::default()
                },
            )?;
            let message_id = crate::repo::insert_message(
                conn,
                &crate::repo::NewMessage {
                    account_id,
                    mailbox_id,
                    uid: 1,
                    uidvalidity: 1,
                    thread_id: Some(thread_id),
                    ..Default::default()
                },
            )?;
            Ok::<_, rusqlite::Error>((thread_id, message_id))
        })
        .unwrap();
    let tag_id = tmp
        .db()
        .with_write(move |conn| {
            insert_tag(
                conn,
                account_id,
                "work",
                None,
                None,
                TagSyncMode::Local,
                None,
            )
        })
        .unwrap();
    tmp.db()
        .with_write(move |conn| {
            insert_message_tag(
                conn,
                &NewMessageTag {
                    tag_id,
                    target: Target::Message(message_id),
                    source: TagSource::User,
                    state: TagState::Applied,
                    confidence: None,
                    rationale: None,
                },
            )
        })
        .unwrap();
    tmp.db()
        .with_write(move |conn| {
            insert_message_tag(
                conn,
                &NewMessageTag {
                    tag_id,
                    target: Target::Thread(thread_id),
                    source: TagSource::User,
                    state: TagState::Applied,
                    confidence: None,
                    rationale: None,
                },
            )
        })
        .unwrap();

    let rows: Vec<i64> = tmp
        .db()
        .with_read(move |conn| {
            let mut stmt =
                conn.prepare("SELECT tag_id FROM messages_tags_effective WHERE message_id = ?1")?;
            let rows = stmt.query_map([message_id], |row| row.get::<_, i64>(0))?;
            rows.collect::<rusqlite::Result<Vec<i64>>>()
        })
        .unwrap();
    assert_eq!(
        rows,
        vec![tag_id],
        "the same (message, tag) pair must appear exactly once, not twice"
    );

    let listed = tmp
        .db()
        .with_read(move |conn| list_tags_with_counts(conn, account_id))
        .unwrap();
    let work = listed.iter().find(|t| t.tag.name == "work").unwrap();
    assert_eq!(
        work.message_count, 1,
        "one message tagged two ways must count as one, not two"
    );
}

#[test]
fn delete_message_tag_removes_exactly_the_matching_row() {
    let tmp = TempDb::open();
    let message_id = seed_message(tmp.db());
    let account_id = tmp
        .db()
        .with_read(move |conn| crate::repo::get_message(conn, message_id))
        .unwrap()
        .unwrap()
        .account_id;
    let tag_id = tmp
        .db()
        .with_write(move |conn| {
            insert_tag(
                conn,
                account_id,
                "work",
                None,
                None,
                TagSyncMode::Local,
                None,
            )
        })
        .unwrap();
    tmp.db()
        .with_write(move |conn| {
            insert_message_tag(
                conn,
                &NewMessageTag {
                    tag_id,
                    target: Target::Message(message_id),
                    source: TagSource::User,
                    state: TagState::Applied,
                    confidence: None,
                    rationale: None,
                },
            )
        })
        .unwrap();

    let removed = tmp
        .db()
        .with_write(move |conn| delete_message_tag(conn, tag_id, Target::Message(message_id)))
        .unwrap();
    assert!(removed);

    let removed_again = tmp
        .db()
        .with_write(move |conn| delete_message_tag(conn, tag_id, Target::Message(message_id)))
        .unwrap();
    assert!(!removed_again, "removing an absent application is a no-op");
}

#[test]
fn resolve_message_tag_only_transitions_a_pending_row() {
    let tmp = TempDb::open();
    let message_id = seed_message(tmp.db());
    let account_id = tmp
        .db()
        .with_read(move |conn| crate::repo::get_message(conn, message_id))
        .unwrap()
        .unwrap()
        .account_id;
    let tag_id = tmp
        .db()
        .with_write(move |conn| {
            insert_tag(
                conn,
                account_id,
                "urgent",
                None,
                None,
                TagSyncMode::Local,
                None,
            )
        })
        .unwrap();
    let row_id = tmp
        .db()
        .with_write(move |conn| {
            insert_message_tag(
                conn,
                &NewMessageTag {
                    tag_id,
                    target: Target::Message(message_id),
                    source: TagSource::Ai,
                    state: TagState::Pending,
                    confidence: Some(0.9),
                    rationale: Some("mentions a due date".to_owned()),
                },
            )
        })
        .unwrap()
        .unwrap();

    let resolved = tmp
        .db()
        .with_write(move |conn| resolve_message_tag(conn, row_id, TagState::Applied))
        .unwrap();
    assert!(resolved);

    let row = tmp
        .db()
        .with_read(move |conn| get_message_tag(conn, row_id))
        .unwrap()
        .unwrap();
    assert_eq!(row.state, TagState::Applied);

    // A second resolution of the same (now non-pending) row is a no-op.
    let resolved_again = tmp
        .db()
        .with_write(move |conn| resolve_message_tag(conn, row_id, TagState::Rejected))
        .unwrap();
    assert!(!resolved_again);
    let row = tmp
        .db()
        .with_read(move |conn| get_message_tag(conn, row_id))
        .unwrap()
        .unwrap();
    assert_eq!(
        row.state,
        TagState::Applied,
        "an already-resolved row must not flip a second time"
    );
}

#[test]
fn list_pending_suggestions_returns_only_pending_rows_for_that_message() {
    let tmp = TempDb::open();
    let message_id = seed_message(tmp.db());
    let account_id = tmp
        .db()
        .with_read(move |conn| crate::repo::get_message(conn, message_id))
        .unwrap()
        .unwrap()
        .account_id;
    let tag_id = tmp
        .db()
        .with_write(move |conn| {
            insert_tag(
                conn,
                account_id,
                "finance/invoice",
                None,
                None,
                TagSyncMode::Local,
                None,
            )
        })
        .unwrap();
    tmp.db()
        .with_write(move |conn| {
            insert_message_tag(
                conn,
                &NewMessageTag {
                    tag_id,
                    target: Target::Message(message_id),
                    source: TagSource::Ai,
                    state: TagState::Pending,
                    confidence: Some(0.77),
                    rationale: Some("looks like an invoice".to_owned()),
                },
            )
        })
        .unwrap();
    // An already-applied tag on the same message must not show up as a
    // "pending suggestion".
    let applied_tag_id = tmp
        .db()
        .with_write(move |conn| {
            insert_tag(
                conn,
                account_id,
                "work",
                None,
                None,
                TagSyncMode::Local,
                None,
            )
        })
        .unwrap();
    tmp.db()
        .with_write(move |conn| {
            insert_message_tag(
                conn,
                &NewMessageTag {
                    tag_id: applied_tag_id,
                    target: Target::Message(message_id),
                    source: TagSource::User,
                    state: TagState::Applied,
                    confidence: None,
                    rationale: None,
                },
            )
        })
        .unwrap();

    let pending = tmp
        .db()
        .with_read(move |conn| list_pending_suggestions(conn, message_id))
        .unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].tag.name, "finance/invoice");
    assert_eq!(pending[0].message_tag.state, TagState::Pending);
    assert_eq!(pending[0].message_tag.confidence, Some(0.77));
    assert_eq!(
        pending[0].message_tag.rationale.as_deref(),
        Some("looks like an invoice")
    );
}
