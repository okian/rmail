//! Repository round-trip tests over a real (temp-file) WAL database.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::storage::Database;

use super::*;

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A temp database path that cleans up its files on drop.
struct TempDb {
    db: Database,
    path: PathBuf,
}

impl TempDb {
    fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-repo-{pid}-{n}.db"));
        let db = Database::open(&path).unwrap();
        Self { db, path }
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.path.display())));
        }
    }
}

/// Insert an account and return its id (a common test fixture).
fn seed_account(db: &Database, name: &str) -> i64 {
    db.with_write(|c| {
        insert_account(
            c,
            &NewAccount {
                name: name.to_owned(),
                imap_server: Some("imap.example.com".to_owned()),
                imap_port: Some(993),
                username: Some("user@example.com".to_owned()),
                ..Default::default()
            },
        )
    })
    .unwrap()
}

#[test]
fn account_insert_get_list() {
    let tmp = TempDb::open();
    let id = seed_account(&tmp.db, "Personal");

    let got = tmp.db.with_read(|c| get_account(c, id)).unwrap().unwrap();
    assert_eq!(got.name, "Personal");
    assert_eq!(got.imap_port, Some(993));
    assert!(got.created_at > 0);

    let by_name = tmp
        .db
        .with_read(|c| get_account_by_name(c, "Personal"))
        .unwrap()
        .unwrap();
    assert_eq!(by_name.id, id);

    seed_account(&tmp.db, "Work");
    let all = tmp.db.with_read(list_accounts).unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].name, "Personal"); // ordered by name
    assert_eq!(all[1].name, "Work");
}

#[test]
fn duplicate_account_name_is_rejected() {
    let tmp = TempDb::open();
    seed_account(&tmp.db, "Dup");
    let err = tmp.db.with_write(|c| {
        insert_account(
            c,
            &NewAccount {
                name: "Dup".to_owned(),
                ..Default::default()
            },
        )
    });
    assert!(err.is_err(), "duplicate account name must violate UNIQUE");
}

#[test]
fn mailbox_insert_get_list() {
    let tmp = TempDb::open();
    let account_id = seed_account(&tmp.db, "Personal");

    let inbox = tmp
        .db
        .with_write(|c| {
            insert_mailbox(
                c,
                &NewMailbox {
                    account_id,
                    name: "INBOX".to_owned(),
                    uidvalidity: Some(42),
                    ..Default::default()
                },
            )
        })
        .unwrap();
    tmp.db
        .with_write(|c| {
            insert_mailbox(
                c,
                &NewMailbox {
                    account_id,
                    name: "Archive".to_owned(),
                    ..Default::default()
                },
            )
        })
        .unwrap();

    let got = tmp
        .db
        .with_read(|c| get_mailbox(c, inbox))
        .unwrap()
        .unwrap();
    assert_eq!(got.name, "INBOX");
    assert_eq!(got.uidvalidity, Some(42));

    let all = tmp.db.with_read(|c| list_mailboxes(c, account_id)).unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].name, "Archive"); // ordered by name
}

#[test]
fn message_roundtrip_preserves_raw_and_orders_by_date() {
    let tmp = TempDb::open();
    let account_id = seed_account(&tmp.db, "Personal");
    let mailbox_id = tmp
        .db
        .with_write(|c| {
            insert_mailbox(
                c,
                &NewMailbox {
                    account_id,
                    name: "INBOX".to_owned(),
                    ..Default::default()
                },
            )
        })
        .unwrap();

    let raw = b"From: a@example.com\r\nSubject: Hi\r\n\r\nBody".to_vec();
    let older = tmp
        .db
        .with_write(|c| {
            insert_message(
                c,
                &NewMessage {
                    account_id,
                    mailbox_id,
                    uid: 1,
                    uidvalidity: 10,
                    message_id: Some("<older@example.com>".to_owned()),
                    subject: Some("Older".to_owned()),
                    from_addr: Some("a@example.com".to_owned()),
                    date: Some(1_000),
                    raw: Some(raw.clone()),
                    ..Default::default()
                },
            )
        })
        .unwrap();
    let newer = tmp
        .db
        .with_write(|c| {
            insert_message(
                c,
                &NewMessage {
                    account_id,
                    mailbox_id,
                    uid: 2,
                    uidvalidity: 10,
                    subject: Some("Newer".to_owned()),
                    date: Some(2_000),
                    has_attachments: true,
                    ..Default::default()
                },
            )
        })
        .unwrap();

    let got = tmp
        .db
        .with_read(|c| get_message(c, older))
        .unwrap()
        .unwrap();
    assert_eq!(got.subject.as_deref(), Some("Older"));
    assert_eq!(
        got.raw.as_deref(),
        Some(raw.as_slice()),
        "raw RFC822 preserved"
    );
    assert!(!got.has_attachments);

    let listed = tmp
        .db
        .with_read(|c| list_messages(c, mailbox_id, 10))
        .unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].id, newer, "newest by date is first");
    assert_eq!(listed[1].id, older);
    assert!(listed[0].has_attachments);
}

#[test]
fn duplicate_message_uid_is_rejected() {
    let tmp = TempDb::open();
    let account_id = seed_account(&tmp.db, "Personal");
    let mailbox_id = tmp
        .db
        .with_write(|c| {
            insert_mailbox(
                c,
                &NewMailbox {
                    account_id,
                    name: "INBOX".to_owned(),
                    ..Default::default()
                },
            )
        })
        .unwrap();

    let msg = NewMessage {
        account_id,
        mailbox_id,
        uid: 5,
        uidvalidity: 1,
        ..Default::default()
    };
    tmp.db.with_write(|c| insert_message(c, &msg)).unwrap();
    let dup = tmp.db.with_write(|c| insert_message(c, &msg));
    assert!(
        dup.is_err(),
        "duplicate (account, mailbox, uidvalidity, uid) must violate UNIQUE"
    );
}

#[test]
fn deleting_account_cascades_to_messages() {
    let tmp = TempDb::open();
    let account_id = seed_account(&tmp.db, "Personal");
    let mailbox_id = tmp
        .db
        .with_write(|c| {
            insert_mailbox(
                c,
                &NewMailbox {
                    account_id,
                    name: "INBOX".to_owned(),
                    ..Default::default()
                },
            )
        })
        .unwrap();
    let message_id = tmp
        .db
        .with_write(|c| {
            insert_message(
                c,
                &NewMessage {
                    account_id,
                    mailbox_id,
                    uid: 1,
                    uidvalidity: 1,
                    ..Default::default()
                },
            )
        })
        .unwrap();

    // Deleting the account must cascade to its mailboxes and messages.
    tmp.db
        .with_write(|c| c.execute("DELETE FROM accounts WHERE id = ?1", [account_id]))
        .unwrap();

    assert!(tmp
        .db
        .with_read(|c| get_message(c, message_id))
        .unwrap()
        .is_none());
    assert!(tmp
        .db
        .with_read(|c| get_mailbox(c, mailbox_id))
        .unwrap()
        .is_none());
}

#[test]
fn flags_add_list_remove() {
    let tmp = TempDb::open();
    let account_id = seed_account(&tmp.db, "Personal");
    let mailbox_id = tmp
        .db
        .with_write(|c| {
            insert_mailbox(
                c,
                &NewMailbox {
                    account_id,
                    name: "INBOX".to_owned(),
                    ..Default::default()
                },
            )
        })
        .unwrap();
    let message_id = tmp
        .db
        .with_write(|c| {
            insert_message(
                c,
                &NewMessage {
                    account_id,
                    mailbox_id,
                    uid: 1,
                    uidvalidity: 1,
                    ..Default::default()
                },
            )
        })
        .unwrap();

    tmp.db
        .with_write(|c| add_flag(c, message_id, "\\Seen"))
        .unwrap();
    tmp.db
        .with_write(|c| add_flag(c, message_id, "\\Flagged"))
        .unwrap();
    // Adding an existing flag is idempotent.
    tmp.db
        .with_write(|c| add_flag(c, message_id, "\\Seen"))
        .unwrap();

    let flags = tmp.db.with_read(|c| list_flags(c, message_id)).unwrap();
    assert_eq!(flags, vec!["\\Flagged".to_owned(), "\\Seen".to_owned()]);

    tmp.db
        .with_write(|c| remove_flag(c, message_id, "\\Seen"))
        .unwrap();
    let flags = tmp.db.with_read(|c| list_flags(c, message_id)).unwrap();
    assert_eq!(flags, vec!["\\Flagged".to_owned()]);
}

#[test]
fn attachments_insert_list() {
    let tmp = TempDb::open();
    let account_id = seed_account(&tmp.db, "Personal");
    let mailbox_id = tmp
        .db
        .with_write(|c| {
            insert_mailbox(
                c,
                &NewMailbox {
                    account_id,
                    name: "INBOX".to_owned(),
                    ..Default::default()
                },
            )
        })
        .unwrap();
    let message_id = tmp
        .db
        .with_write(|c| {
            insert_message(
                c,
                &NewMessage {
                    account_id,
                    mailbox_id,
                    uid: 1,
                    uidvalidity: 1,
                    has_attachments: true,
                    ..Default::default()
                },
            )
        })
        .unwrap();

    tmp.db
        .with_write(|c| {
            insert_attachment(
                c,
                &NewAttachment {
                    message_id,
                    filename: Some("invoice.pdf".to_owned()),
                    content_type: Some("application/pdf".to_owned()),
                    size: Some(1024),
                    ..Default::default()
                },
            )
        })
        .unwrap();

    let attachments = tmp
        .db
        .with_read(|c| list_attachments(c, message_id))
        .unwrap();
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments[0].filename.as_deref(), Some("invoice.pdf"));
    assert!(!attachments[0].is_inline);
}

#[test]
fn contacts_upsert_bumps_count() {
    let tmp = TempDb::open();

    let id1 = tmp
        .db
        .with_write(|c| upsert_contact(c, "alice@example.com", Some("Alice"), 100))
        .unwrap();
    let id2 = tmp
        .db
        .with_write(|c| upsert_contact(c, "alice@example.com", None, 200))
        .unwrap();
    assert_eq!(id1, id2, "same address upserts the same row");

    let contact = tmp
        .db
        .with_read(|c| get_contact_by_address(c, "alice@example.com"))
        .unwrap()
        .unwrap();
    assert_eq!(contact.message_count, 2, "second upsert bumps the count");
    assert_eq!(contact.name.as_deref(), Some("Alice"), "name retained");
    assert_eq!(contact.last_seen, Some(200), "last_seen advanced");

    let all = tmp.db.with_read(list_contacts).unwrap();
    assert_eq!(all.len(), 1);
}

#[test]
fn sync_state_upsert_and_get() {
    let tmp = TempDb::open();
    let account_id = seed_account(&tmp.db, "Personal");
    let mailbox_id = tmp
        .db
        .with_write(|c| {
            insert_mailbox(
                c,
                &NewMailbox {
                    account_id,
                    name: "INBOX".to_owned(),
                    ..Default::default()
                },
            )
        })
        .unwrap();

    tmp.db
        .with_write(|c| {
            upsert_sync_state(
                c,
                &SyncState {
                    mailbox_id,
                    uidvalidity: Some(10),
                    last_synced_uid: Some(50),
                    full_sync_done: false,
                    ..Default::default()
                },
            )
        })
        .unwrap();
    // Second upsert updates in place.
    tmp.db
        .with_write(|c| {
            upsert_sync_state(
                c,
                &SyncState {
                    mailbox_id,
                    uidvalidity: Some(10),
                    last_synced_uid: Some(120),
                    full_sync_done: true,
                    ..Default::default()
                },
            )
        })
        .unwrap();

    let state = tmp
        .db
        .with_read(|c| get_sync_state(c, mailbox_id))
        .unwrap()
        .unwrap();
    assert_eq!(state.last_synced_uid, Some(120));
    assert!(state.full_sync_done);
}

#[test]
fn thread_insert_get() {
    let tmp = TempDb::open();
    let account_id = seed_account(&tmp.db, "Personal");
    let id = tmp
        .db
        .with_write(|c| {
            insert_thread(
                c,
                &NewThread {
                    account_id,
                    subject_norm: Some("office move".to_owned()),
                    last_message_at: Some(1_234),
                    ..Default::default()
                },
            )
        })
        .unwrap();

    let thread = tmp.db.with_read(|c| get_thread(c, id)).unwrap().unwrap();
    assert_eq!(thread.account_id, account_id);
    assert_eq!(thread.subject_norm.as_deref(), Some("office move"));
    assert_eq!(thread.message_count, 0);
}

#[test]
fn message_lookup_by_imap_identity() {
    let tmp = TempDb::open();
    let account_id = seed_account(&tmp.db, "Personal");
    let mailbox_id = tmp
        .db
        .with_write(|c| {
            insert_mailbox(
                c,
                &NewMailbox {
                    account_id,
                    name: "INBOX".to_owned(),
                    ..Default::default()
                },
            )
        })
        .unwrap();
    let id = tmp
        .db
        .with_write(|c| {
            insert_message(
                c,
                &NewMessage {
                    account_id,
                    mailbox_id,
                    uid: 7,
                    uidvalidity: 99,
                    ..Default::default()
                },
            )
        })
        .unwrap();

    let found = tmp
        .db
        .with_read(|c| get_message_by_identity(c, mailbox_id, 99, 7))
        .unwrap()
        .unwrap();
    assert_eq!(found.id, id);

    let missing = tmp
        .db
        .with_read(|c| get_message_by_identity(c, mailbox_id, 99, 8))
        .unwrap();
    assert!(missing.is_none());
}

#[test]
fn listing_falls_back_to_internaldate_when_date_missing() {
    let tmp = TempDb::open();
    let account_id = seed_account(&tmp.db, "Personal");
    let mailbox_id = tmp
        .db
        .with_write(|c| {
            insert_mailbox(
                c,
                &NewMailbox {
                    account_id,
                    name: "INBOX".to_owned(),
                    ..Default::default()
                },
            )
        })
        .unwrap();

    // Message A has a Date header; message B has none but a later INTERNALDATE.
    let with_date = tmp
        .db
        .with_write(|c| {
            insert_message(
                c,
                &NewMessage {
                    account_id,
                    mailbox_id,
                    uid: 1,
                    uidvalidity: 1,
                    date: Some(1_000),
                    internaldate: Some(1_000),
                    ..Default::default()
                },
            )
        })
        .unwrap();
    let no_date = tmp
        .db
        .with_write(|c| {
            insert_message(
                c,
                &NewMessage {
                    account_id,
                    mailbox_id,
                    uid: 2,
                    uidvalidity: 1,
                    date: None,
                    internaldate: Some(5_000),
                    ..Default::default()
                },
            )
        })
        .unwrap();

    let listed = tmp
        .db
        .with_read(|c| list_messages(c, mailbox_id, 10))
        .unwrap();
    assert_eq!(listed.len(), 2);
    assert_eq!(
        listed[0].id, no_date,
        "a missing Date must fall back to the later INTERNALDATE, not sink to the bottom"
    );
    assert_eq!(listed[1].id, with_date);
}
