use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::config::TagSyncMode;
use crate::storage::Database;

use super::*;

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct TempDb(PathBuf, Database);

impl TempDb {
    fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-tagsquery-{pid}-{n}.db"));
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

/// Run `compile(account_id, raw)` against a real database and return the
/// matching message ids.
fn run(db: &Database, account_id: i64, raw: &str) -> Vec<i64> {
    let (where_sql, params) = compile(account_id, raw);
    let sql = format!("SELECT id FROM messages WHERE {where_sql} ORDER BY id");
    db.with_read(|conn| {
        let mut stmt = conn.prepare(&sql)?;
        let bind: Vec<&dyn rusqlite::ToSql> =
            params.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
        let rows = stmt.query_map(bind.as_slice(), |row| row.get::<_, i64>(0))?;
        rows.collect::<rusqlite::Result<Vec<i64>>>()
    })
    .unwrap()
}

/// Seed an account with two mailboxes and messages `(from, subject, flags)`,
/// returning `(account_id, [message_ids in seed order])`.
fn seed(db: &Database, messages: &[(&str, &str, &[&str])]) -> (i64, Vec<i64>) {
    let account_id = db
        .with_write(|conn| {
            crate::repo::insert_account(
                conn,
                &crate::repo::NewAccount {
                    name: format!("acct-{}", COUNTER.fetch_add(1, Ordering::Relaxed)),
                    ..Default::default()
                },
            )
        })
        .unwrap();
    let mailbox_id = db
        .with_write(move |conn| {
            crate::repo::insert_mailbox(
                conn,
                &crate::repo::NewMailbox {
                    account_id,
                    name: "INBOX".to_owned(),
                    ..Default::default()
                },
            )
        })
        .unwrap();

    let mut ids = Vec::new();
    for (i, (from, subject, flags)) in messages.iter().enumerate() {
        let uid = i as i64 + 1;
        let from = (*from).to_owned();
        let subject = (*subject).to_owned();
        let id = db
            .with_write(move |conn| {
                crate::repo::insert_message(
                    conn,
                    &crate::repo::NewMessage {
                        account_id,
                        mailbox_id,
                        uid,
                        uidvalidity: 1,
                        from_addr: Some(from),
                        subject: Some(subject),
                        ..Default::default()
                    },
                )
            })
            .unwrap();
        for flag in *flags {
            db.with_write({
                let flag = (*flag).to_owned();
                move |conn| crate::repo::add_flag(conn, id, &flag)
            })
            .unwrap();
        }
        ids.push(id);
    }
    (account_id, ids)
}

#[test]
fn from_filters_to_matching_senders_only() {
    let tmp = TempDb::open();
    let (account_id, ids) = seed(
        tmp.db(),
        &[
            ("billing@stripe.com", "Invoice", &[]),
            ("alice@example.com", "Hello", &[]),
        ],
    );

    let matched = run(tmp.db(), account_id, "from:stripe");
    assert_eq!(matched, vec![ids[0]]);
}

#[test]
fn is_unread_matches_messages_without_seen() {
    let tmp = TempDb::open();
    let (account_id, ids) = seed(
        tmp.db(),
        &[
            ("a@example.com", "Read", &["\\Seen"]),
            ("b@example.com", "Unread", &[]),
        ],
    );

    let matched = run(tmp.db(), account_id, "is:unread");
    assert_eq!(matched, vec![ids[1]]);
}

#[test]
fn negation_inverts_the_predicate() {
    let tmp = TempDb::open();
    let (account_id, ids) = seed(
        tmp.db(),
        &[
            ("billing@stripe.com", "Invoice", &[]),
            ("alice@example.com", "Hello", &[]),
        ],
    );

    let matched = run(tmp.db(), account_id, "-from:stripe");
    assert_eq!(matched, vec![ids[1]]);
}

#[test]
fn an_unrecognized_operator_is_silently_dropped_not_an_error() {
    let tmp = TempDb::open();
    let (account_id, ids) = seed(tmp.db(), &[("a@example.com", "Hi", &[])]);

    // `body:` is not in this compiler's subset -- the query degrades to
    // "every message in the account" rather than matching nothing or
    // panicking.
    let matched = run(tmp.db(), account_id, "body:whatever");
    assert_eq!(matched, ids);
}

#[test]
fn results_are_scoped_to_the_given_account() {
    let tmp = TempDb::open();
    let (account_a, ids_a) = seed(tmp.db(), &[("x@example.com", "A", &[])]);
    let (_account_b, _ids_b) = seed(tmp.db(), &[("x@example.com", "B", &[])]);

    let matched = run(tmp.db(), account_a, "from:x");
    assert_eq!(matched, ids_a, "must not cross into another account");
}

#[test]
fn tag_filter_reuses_the_same_predicate_search_uses() {
    let tmp = TempDb::open();
    let (account_id, ids) = seed(
        tmp.db(),
        &[
            ("a@example.com", "Tagged", &[]),
            ("b@example.com", "Plain", &[]),
        ],
    );
    let tag_id = tmp
        .db()
        .with_write(move |conn| {
            super::super::repo::insert_tag(
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
    let first_id = ids[0];
    tmp.db()
        .with_write(move |conn| {
            super::super::repo::insert_message_tag(
                conn,
                &super::super::model::NewMessageTag {
                    tag_id,
                    target: super::super::model::Target::Message(first_id),
                    source: super::super::model::TagSource::User,
                    state: super::super::model::TagState::Applied,
                    confidence: None,
                    rationale: None,
                },
            )
        })
        .unwrap();

    let matched = run(tmp.db(), account_id, "tag:work");
    assert_eq!(matched, vec![first_id]);
}
