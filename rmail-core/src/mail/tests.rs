//! `MailStore` tests: the read/IMAP-mutation ordering contract the module
//! docs promise, driven against a fake [`ImapMutator`] rather than a real
//! connection — the real one needs a live/mock server, which is exactly what
//! [`crate::imap::mutate`]'s own tests exercise. What matters here is the
//! *sequencing*: does a mutation ever touch the database before IMAP has
//! confirmed it, and does a local-write failure after a successful IMAP call
//! still surface as an error rather than a silent success.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use crate::events::{EventLog, Retention};
use crate::repo::{self, NewAccount, NewMailbox, NewMessage, NewThread};
use crate::storage::Database;
use crate::ErrorReason;

use super::*;

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct TempDb {
    db: Database,
    path: PathBuf,
}

impl TempDb {
    fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-mail-{pid}-{n}.db"));
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

/// One recorded call to the fake IMAP mutator.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Call {
    SetFlags {
        account_id: i64,
        mailbox: String,
        uidvalidity: i64,
        uid: i64,
        flags: Vec<String>,
    },
    Move {
        account_id: i64,
        mailbox: String,
        uidvalidity: i64,
        uid: i64,
        dest: String,
    },
    Copy {
        account_id: i64,
        mailbox: String,
        uidvalidity: i64,
        uid: i64,
        dest: String,
    },
    Delete {
        account_id: i64,
        mailbox: String,
        uidvalidity: i64,
        uid: i64,
    },
}

/// A test double proving the *ordering* contract without a real IMAP server:
/// records every call, and can be told to fail each kind on demand.
#[derive(Debug, Default)]
struct FakeImap {
    calls: Mutex<Vec<Call>>,
    fail_set_flags: bool,
    fail_move: bool,
    fail_copy: bool,
    fail_delete: bool,
}

impl FakeImap {
    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl ImapMutator for FakeImap {
    async fn set_flags(
        &self,
        account_id: i64,
        mailbox: &str,
        uidvalidity: i64,
        uid: i64,
        flags: &[String],
    ) -> Result<(), Error> {
        self.calls.lock().unwrap().push(Call::SetFlags {
            account_id,
            mailbox: mailbox.to_owned(),
            uidvalidity,
            uid,
            flags: flags.to_vec(),
        });
        if self.fail_set_flags {
            return Err(Error::unavailable("fake imap: set_flags refused"));
        }
        Ok(())
    }

    async fn move_message(
        &self,
        account_id: i64,
        mailbox: &str,
        uidvalidity: i64,
        uid: i64,
        dest: &str,
    ) -> Result<(), Error> {
        self.calls.lock().unwrap().push(Call::Move {
            account_id,
            mailbox: mailbox.to_owned(),
            uidvalidity,
            uid,
            dest: dest.to_owned(),
        });
        if self.fail_move {
            return Err(Error::unavailable("fake imap: move refused"));
        }
        Ok(())
    }

    async fn copy_message(
        &self,
        account_id: i64,
        mailbox: &str,
        uidvalidity: i64,
        uid: i64,
        dest: &str,
    ) -> Result<(), Error> {
        self.calls.lock().unwrap().push(Call::Copy {
            account_id,
            mailbox: mailbox.to_owned(),
            uidvalidity,
            uid,
            dest: dest.to_owned(),
        });
        if self.fail_copy {
            return Err(Error::unavailable("fake imap: copy refused"));
        }
        Ok(())
    }

    async fn delete_message(
        &self,
        account_id: i64,
        mailbox: &str,
        uidvalidity: i64,
        uid: i64,
    ) -> Result<(), Error> {
        self.calls.lock().unwrap().push(Call::Delete {
            account_id,
            mailbox: mailbox.to_owned(),
            uidvalidity,
            uid,
        });
        if self.fail_delete {
            return Err(Error::unavailable("fake imap: delete refused"));
        }
        Ok(())
    }
}

/// A store over a fresh database, an unlimited event log, and a fresh fake
/// IMAP mutator the test can configure before use.
struct Fixture {
    tmp: TempDb,
    events: EventLog,
    imap: Arc<FakeImap>,
    store: MailStore,
}

impl Fixture {
    fn new() -> Self {
        Self::with_imap(FakeImap::default())
    }

    fn with_imap(imap: FakeImap) -> Self {
        let tmp = TempDb::open();
        let events = EventLog::new(tmp.db.clone(), Retention::unlimited());
        let imap = Arc::new(imap);
        let store = MailStore::new(tmp.db.clone(), events.clone(), imap.clone());
        Self {
            tmp,
            events,
            imap,
            store,
        }
    }

    /// Two mailboxes ("INBOX", "Archive") on one account, and one message in
    /// INBOX with the given flags. Returns (account_id, inbox_id, archive_id,
    /// message_id).
    fn seed(&self, flags: &[&str]) -> (i64, i64, i64, i64) {
        let account_id = self
            .tmp
            .db
            .with_write(|c| {
                repo::insert_account(
                    c,
                    &NewAccount {
                        name: "Personal".to_owned(),
                        ..Default::default()
                    },
                )
            })
            .unwrap();
        let inbox_id = self
            .tmp
            .db
            .with_write(|c| {
                repo::insert_mailbox(
                    c,
                    &NewMailbox {
                        account_id,
                        name: "INBOX".to_owned(),
                        ..Default::default()
                    },
                )
            })
            .unwrap();
        let archive_id = self
            .tmp
            .db
            .with_write(|c| {
                repo::insert_mailbox(
                    c,
                    &NewMailbox {
                        account_id,
                        name: "Archive".to_owned(),
                        ..Default::default()
                    },
                )
            })
            .unwrap();
        let message_id = self
            .tmp
            .db
            .with_write(|c| {
                repo::insert_message(
                    c,
                    &NewMessage {
                        account_id,
                        mailbox_id: inbox_id,
                        uid: 42,
                        uidvalidity: 1,
                        subject: Some("Hi".to_owned()),
                        ..Default::default()
                    },
                )
            })
            .unwrap();
        for flag in flags {
            self.tmp
                .db
                .with_write(|c| repo::add_flag(c, message_id, flag))
                .unwrap();
        }
        (account_id, inbox_id, archive_id, message_id)
    }
}

// ---------------------------------------------------------------------------
// Reads never touch IMAP
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_and_get_read_local_state_without_touching_imap() {
    let fx = Fixture::new();
    let (_account_id, inbox_id, _archive_id, message_id) = fx.seed(&["\\Seen"]);

    let listed = fx.store.list(inbox_id, 0).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].message.id, message_id);
    assert_eq!(listed[0].flags, vec!["\\Seen".to_owned()]);

    let got = fx.store.get(message_id).await.unwrap();
    assert_eq!(got.message.message.subject.as_deref(), Some("Hi"));
    assert!(got.attachments.is_empty());

    assert!(
        fx.imap.calls().is_empty(),
        "reads must never call the IMAP mutator"
    );
}

#[tokio::test]
async fn get_reports_not_found_for_an_unknown_message() {
    let fx = Fixture::new();
    let err = fx.store.get(999).await.unwrap_err();
    assert_eq!(err.reason(), ErrorReason::NotFound);
}

#[tokio::test]
async fn get_thread_orders_messages_oldest_first_and_includes_flags() {
    let fx = Fixture::new();
    let (account_id, inbox_id, _archive_id, _message_id) = fx.seed(&[]);
    let thread_id = fx
        .tmp
        .db
        .with_write(|c| {
            repo::insert_thread(
                c,
                &NewThread {
                    account_id,
                    ..Default::default()
                },
            )
        })
        .unwrap();
    let older = fx
        .tmp
        .db
        .with_write(|c| {
            repo::insert_message(
                c,
                &NewMessage {
                    account_id,
                    mailbox_id: inbox_id,
                    uid: 100,
                    uidvalidity: 1,
                    thread_id: Some(thread_id),
                    date: Some(1_000),
                    ..Default::default()
                },
            )
        })
        .unwrap();
    let newer = fx
        .tmp
        .db
        .with_write(|c| {
            repo::insert_message(
                c,
                &NewMessage {
                    account_id,
                    mailbox_id: inbox_id,
                    uid: 101,
                    uidvalidity: 1,
                    thread_id: Some(thread_id),
                    date: Some(2_000),
                    ..Default::default()
                },
            )
        })
        .unwrap();
    fx.tmp
        .db
        .with_write(|c| repo::add_flag(c, newer, "\\Flagged"))
        .unwrap();

    let view = fx.store.get_thread(thread_id).await.unwrap();
    assert_eq!(view.messages.len(), 2);
    assert_eq!(view.messages[0].message.id, older, "oldest first");
    assert_eq!(view.messages[1].message.id, newer);
    assert_eq!(view.messages[1].flags, vec!["\\Flagged".to_owned()]);
    assert!(fx.imap.calls().is_empty());
}

// ---------------------------------------------------------------------------
// SetFlags
// ---------------------------------------------------------------------------

#[tokio::test]
async fn set_flags_rejects_an_unsafe_flag_before_touching_imap_or_the_database() {
    let fx = Fixture::new();
    let (_account_id, _inbox_id, _archive_id, message_id) = fx.seed(&["\\Seen"]);

    let err = fx
        .store
        .set_flags(message_id, vec!["not a flag (injected)".to_owned()])
        .await
        .unwrap_err();
    assert_eq!(err.reason(), ErrorReason::InvalidArgument);
    assert!(fx.imap.calls().is_empty(), "must fail before calling IMAP");

    let still = fx
        .tmp
        .db
        .with_read(|c| repo::list_flags(c, message_id))
        .unwrap();
    assert_eq!(still, vec!["\\Seen".to_owned()], "local flags untouched");
}

#[tokio::test]
async fn set_flags_replaces_locally_and_emits_an_event_when_the_flag_set_changes() {
    let fx = Fixture::new();
    let (account_id, inbox_id, _archive_id, message_id) = fx.seed(&["\\Seen"]);

    let changed = fx
        .store
        .set_flags(
            message_id,
            vec!["\\Seen".to_owned(), "\\Flagged".to_owned()],
        )
        .await
        .unwrap();
    assert!(changed);

    let stored = fx
        .tmp
        .db
        .with_read(|c| repo::list_flags(c, message_id))
        .unwrap();
    assert_eq!(stored, vec!["\\Flagged".to_owned(), "\\Seen".to_owned()]);

    assert_eq!(
        fx.imap.calls(),
        vec![Call::SetFlags {
            account_id,
            mailbox: "INBOX".to_owned(),
            uidvalidity: 1,
            uid: 42,
            flags: vec!["\\Seen".to_owned(), "\\Flagged".to_owned()],
        }]
    );

    let page = fx.events.since(0, 10).await.unwrap();
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].kind, crate::events::EventKind::FlagChanged);
    assert_eq!(page.events[0].account_id, Some(account_id));
    assert_eq!(page.events[0].mailbox_id, Some(inbox_id));
    assert_eq!(page.events[0].message_id, Some(message_id));
}

#[tokio::test]
async fn set_flags_returns_false_and_emits_no_event_when_the_flag_set_is_unchanged() {
    let fx = Fixture::new();
    let (_account_id, _inbox_id, _archive_id, message_id) = fx.seed(&["\\Seen"]);

    let changed = fx
        .store
        .set_flags(message_id, vec!["\\Seen".to_owned()])
        .await
        .unwrap();
    assert!(!changed);

    // The IMAP call still happens — the server, not the local cache, is the
    // source of truth, so an apparently-unchanged local flag set is not a
    // reason to skip telling the server.
    assert_eq!(fx.imap.calls().len(), 1);

    let page = fx.events.since(0, 10).await.unwrap();
    assert!(
        page.events.is_empty(),
        "no-op flag set must not emit an event"
    );
}

#[tokio::test]
async fn set_flags_does_not_touch_local_state_when_the_imap_call_fails() {
    let fx = Fixture::with_imap(FakeImap {
        fail_set_flags: true,
        ..Default::default()
    });
    let (_account_id, _inbox_id, _archive_id, message_id) = fx.seed(&["\\Seen"]);

    let err = fx
        .store
        .set_flags(message_id, vec!["\\Flagged".to_owned()])
        .await
        .unwrap_err();
    assert_eq!(err.reason(), ErrorReason::Unavailable);

    let still = fx
        .tmp
        .db
        .with_read(|c| repo::list_flags(c, message_id))
        .unwrap();
    assert_eq!(still, vec!["\\Seen".to_owned()], "local flags untouched");

    let page = fx.events.since(0, 10).await.unwrap();
    assert!(page.events.is_empty());
}

// ---------------------------------------------------------------------------
// Move
// ---------------------------------------------------------------------------

#[tokio::test]
async fn move_message_deletes_the_local_row_and_emits_a_moved_event() {
    let fx = Fixture::new();
    let (account_id, inbox_id, archive_id, message_id) = fx.seed(&[]);

    fx.store.move_message(message_id, archive_id).await.unwrap();

    assert!(
        fx.tmp
            .db
            .with_read(|c| repo::get_message(c, message_id))
            .unwrap()
            .is_none(),
        "the local row is dropped, not re-pointed at a guessed identity"
    );
    assert_eq!(
        fx.imap.calls(),
        vec![Call::Move {
            account_id,
            mailbox: "INBOX".to_owned(),
            uidvalidity: 1,
            uid: 42,
            dest: "Archive".to_owned(),
        }]
    );

    let page = fx.events.since(0, 10).await.unwrap();
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].kind, crate::events::EventKind::Moved);
    assert_eq!(page.events[0].mailbox_id, Some(inbox_id));
    assert_eq!(page.events[0].message_id, Some(message_id));
    assert_eq!(
        page.events[0].payload["to_mailbox_id"].as_i64(),
        Some(archive_id)
    );
}

#[tokio::test]
async fn move_message_is_rejected_across_accounts_without_calling_imap() {
    let fx = Fixture::new();
    let (_account_id, _inbox_id, _archive_id, message_id) = fx.seed(&[]);
    let other_account = fx
        .tmp
        .db
        .with_write(|c| {
            repo::insert_account(
                c,
                &NewAccount {
                    name: "Other".to_owned(),
                    ..Default::default()
                },
            )
        })
        .unwrap();
    let other_mailbox = fx
        .tmp
        .db
        .with_write(|c| {
            repo::insert_mailbox(
                c,
                &NewMailbox {
                    account_id: other_account,
                    name: "INBOX".to_owned(),
                    ..Default::default()
                },
            )
        })
        .unwrap();

    let err = fx
        .store
        .move_message(message_id, other_mailbox)
        .await
        .unwrap_err();
    assert_eq!(err.reason(), ErrorReason::InvalidArgument);
    assert!(fx.imap.calls().is_empty());
    assert!(fx
        .tmp
        .db
        .with_read(|c| repo::get_message(c, message_id))
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn move_message_rejects_the_messages_own_mailbox_as_a_destination() {
    let fx = Fixture::new();
    let (_account_id, inbox_id, _archive_id, message_id) = fx.seed(&[]);

    let err = fx
        .store
        .move_message(message_id, inbox_id)
        .await
        .unwrap_err();
    assert_eq!(err.reason(), ErrorReason::InvalidArgument);
    assert!(fx.imap.calls().is_empty());
}

#[tokio::test]
async fn move_message_leaves_the_local_row_when_imap_fails() {
    let fx = Fixture::with_imap(FakeImap {
        fail_move: true,
        ..Default::default()
    });
    let (_account_id, _inbox_id, archive_id, message_id) = fx.seed(&[]);

    let err = fx
        .store
        .move_message(message_id, archive_id)
        .await
        .unwrap_err();
    assert_eq!(err.reason(), ErrorReason::Unavailable);

    assert!(
        fx.tmp
            .db
            .with_read(|c| repo::get_message(c, message_id))
            .unwrap()
            .is_some(),
        "a failed IMAP move must not delete the local row"
    );
    let page = fx.events.since(0, 10).await.unwrap();
    assert!(page.events.is_empty());
}

// ---------------------------------------------------------------------------
// Copy
// ---------------------------------------------------------------------------

#[tokio::test]
async fn copy_message_touches_only_imap_and_leaves_local_state_and_events_untouched() {
    let fx = Fixture::new();
    let (account_id, _inbox_id, archive_id, message_id) = fx.seed(&["\\Seen"]);

    fx.store.copy_message(message_id, archive_id).await.unwrap();

    assert_eq!(
        fx.imap.calls(),
        vec![Call::Copy {
            account_id,
            mailbox: "INBOX".to_owned(),
            uidvalidity: 1,
            uid: 42,
            dest: "Archive".to_owned(),
        }]
    );
    // The source message is exactly as it was — Copy creates a message this
    // database has never seen, under a UID it does not know, so there is
    // nothing correct to write locally.
    let still = fx
        .tmp
        .db
        .with_read(|c| repo::get_message(c, message_id))
        .unwrap();
    assert!(still.is_some());
    let page = fx.events.since(0, 10).await.unwrap();
    assert!(page.events.is_empty(), "copy emits no local event");
}

#[tokio::test]
async fn copy_message_is_rejected_across_accounts() {
    let fx = Fixture::new();
    let (_account_id, _inbox_id, _archive_id, message_id) = fx.seed(&[]);
    let other_account = fx
        .tmp
        .db
        .with_write(|c| {
            repo::insert_account(
                c,
                &NewAccount {
                    name: "Other".to_owned(),
                    ..Default::default()
                },
            )
        })
        .unwrap();
    let other_mailbox = fx
        .tmp
        .db
        .with_write(|c| {
            repo::insert_mailbox(
                c,
                &NewMailbox {
                    account_id: other_account,
                    name: "INBOX".to_owned(),
                    ..Default::default()
                },
            )
        })
        .unwrap();

    let err = fx
        .store
        .copy_message(message_id, other_mailbox)
        .await
        .unwrap_err();
    assert_eq!(err.reason(), ErrorReason::InvalidArgument);
    assert!(fx.imap.calls().is_empty());
}

// ---------------------------------------------------------------------------
// Delete
// ---------------------------------------------------------------------------

#[tokio::test]
async fn delete_message_removes_the_local_row_and_emits_a_deleted_event() {
    let fx = Fixture::new();
    let (account_id, inbox_id, _archive_id, message_id) = fx.seed(&[]);

    fx.store.delete_message(message_id).await.unwrap();

    assert!(fx
        .tmp
        .db
        .with_read(|c| repo::get_message(c, message_id))
        .unwrap()
        .is_none());
    assert_eq!(
        fx.imap.calls(),
        vec![Call::Delete {
            account_id,
            mailbox: "INBOX".to_owned(),
            uidvalidity: 1,
            uid: 42,
        }]
    );
    let page = fx.events.since(0, 10).await.unwrap();
    assert_eq!(page.events.len(), 1);
    assert_eq!(page.events[0].kind, crate::events::EventKind::Deleted);
    assert_eq!(page.events[0].mailbox_id, Some(inbox_id));
    assert_eq!(page.events[0].message_id, Some(message_id));
}

#[tokio::test]
async fn delete_message_repairs_the_thread_it_leaves_behind() {
    // The whole reason `delete_message`/`move_message` route through
    // `crate::sync::remove_messages` rather than a bare `DELETE FROM
    // messages`: a plain delete would leave `threads.message_count` (and
    // `participants`/`root_message_id`) stale, and a thread reduced to zero
    // members would never be cleaned up.
    let fx = Fixture::new();
    let (account_id, inbox_id, _archive_id, first) = fx.seed(&[]);
    let thread_id = fx
        .tmp
        .db
        .with_write(|c| {
            repo::insert_thread(
                c,
                &NewThread {
                    account_id,
                    ..Default::default()
                },
            )
        })
        .unwrap();
    fx.tmp
        .db
        .with_write(move |c| {
            c.execute(
                "UPDATE messages SET thread_id = ?1 WHERE id = ?2",
                rusqlite::params![thread_id, first],
            )
        })
        .unwrap();
    let second = fx
        .tmp
        .db
        .with_write(move |c| {
            repo::insert_message(
                c,
                &NewMessage {
                    account_id,
                    mailbox_id: inbox_id,
                    uid: 43,
                    uidvalidity: 1,
                    thread_id: Some(thread_id),
                    ..Default::default()
                },
            )
        })
        .unwrap();
    fx.tmp
        .db
        .with_write(move |c| crate::thread::recompute_thread(c, thread_id))
        .unwrap();
    let before = fx
        .tmp
        .db
        .with_read(move |c| repo::get_thread(c, thread_id))
        .unwrap()
        .expect("thread exists before either message is deleted");
    assert_eq!(before.message_count, 2);

    fx.store.delete_message(first).await.unwrap();

    let after_one = fx
        .tmp
        .db
        .with_read(move |c| repo::get_thread(c, thread_id))
        .unwrap()
        .expect("thread still has one member");
    assert_eq!(
        after_one.message_count, 1,
        "message_count must reflect the deletion, not the pre-delete count a \
         bare `DELETE FROM messages` would have left behind"
    );

    fx.store.delete_message(second).await.unwrap();

    let after_both = fx
        .tmp
        .db
        .with_read(move |c| repo::get_thread(c, thread_id))
        .unwrap();
    assert!(
        after_both.is_none(),
        "a thread reduced to zero members must be cleaned up, not left as an \
         empty row"
    );
}

#[tokio::test]
async fn delete_message_leaves_the_local_row_when_imap_fails() {
    let fx = Fixture::with_imap(FakeImap {
        fail_delete: true,
        ..Default::default()
    });
    let (_account_id, _inbox_id, _archive_id, message_id) = fx.seed(&[]);

    let err = fx.store.delete_message(message_id).await.unwrap_err();
    assert_eq!(err.reason(), ErrorReason::Unavailable);
    assert!(
        fx.tmp
            .db
            .with_read(|c| repo::get_message(c, message_id))
            .unwrap()
            .is_some(),
        "a failed IMAP delete must not remove the local row"
    );
    let page = fx.events.since(0, 10).await.unwrap();
    assert!(page.events.is_empty());
}

#[tokio::test]
async fn mutating_an_unknown_message_is_not_found_without_calling_imap() {
    let fx = Fixture::new();
    let err = fx.store.delete_message(999).await.unwrap_err();
    assert_eq!(err.reason(), ErrorReason::NotFound);
    assert!(fx.imap.calls().is_empty());
}

// ---------------------------------------------------------------------------
// Attachment bytes
// ---------------------------------------------------------------------------

const MULTI_ATTACHMENT_RAW: &[u8] = b"From: a@example.com\r\n\
Subject: Two attachments\r\n\
Content-Type: multipart/mixed; boundary=\"b\"\r\n\
\r\n\
--b\r\n\
Content-Type: text/plain\r\n\
\r\n\
see attached\r\n\
--b\r\n\
Content-Type: text/plain; name=\"first.txt\"\r\n\
Content-Disposition: attachment; filename=\"first.txt\"\r\n\
\r\n\
first contents\r\n\
--b\r\n\
Content-Type: application/pdf; name=\"second.pdf\"\r\n\
Content-Disposition: attachment; filename=\"second.pdf\"\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
c2Vjb25k\r\n\
--b--\r\n";

#[tokio::test]
async fn attachment_bytes_returns_the_matching_part_by_position() {
    let fx = Fixture::new();
    let (account_id, inbox_id, _archive_id, _message_id) = fx.seed(&[]);
    let message_id = fx
        .tmp
        .db
        .with_write(|c| {
            repo::insert_message(
                c,
                &NewMessage {
                    account_id,
                    mailbox_id: inbox_id,
                    uid: 43,
                    uidvalidity: 1,
                    raw: Some(MULTI_ATTACHMENT_RAW.to_vec()),
                    has_attachments: true,
                    ..Default::default()
                },
            )
        })
        .unwrap();

    let first = fx.store.attachment_bytes(message_id, "0").await.unwrap();
    assert_eq!(first.filename.as_deref(), Some("first.txt"));
    assert_eq!(first.bytes, b"first contents");

    let second = fx.store.attachment_bytes(message_id, "1").await.unwrap();
    assert_eq!(second.filename.as_deref(), Some("second.pdf"));
    assert_eq!(second.content_type.as_deref(), Some("application/pdf"));
    assert_eq!(second.bytes, b"second");

    assert!(fx.imap.calls().is_empty());
}

#[tokio::test]
async fn attachment_bytes_errors_not_found_for_an_unknown_part() {
    let fx = Fixture::new();
    let (account_id, inbox_id, _archive_id, _message_id) = fx.seed(&[]);
    let message_id = fx
        .tmp
        .db
        .with_write(|c| {
            repo::insert_message(
                c,
                &NewMessage {
                    account_id,
                    mailbox_id: inbox_id,
                    uid: 43,
                    uidvalidity: 1,
                    raw: Some(MULTI_ATTACHMENT_RAW.to_vec()),
                    ..Default::default()
                },
            )
        })
        .unwrap();

    let err = fx
        .store
        .attachment_bytes(message_id, "99")
        .await
        .unwrap_err();
    assert_eq!(err.reason(), ErrorReason::NotFound);
}

#[tokio::test]
async fn attachment_bytes_errors_not_found_for_an_unknown_message() {
    let fx = Fixture::new();
    let err = fx.store.attachment_bytes(999, "0").await.unwrap_err();
    assert_eq!(err.reason(), ErrorReason::NotFound);
}

#[tokio::test]
async fn attachment_bytes_errors_failed_precondition_without_a_stored_body() {
    let fx = Fixture::new();
    let (account_id, inbox_id, _archive_id, _message_id) = fx.seed(&[]);
    let message_id = fx
        .tmp
        .db
        .with_write(|c| {
            repo::insert_message(
                c,
                &NewMessage {
                    account_id,
                    mailbox_id: inbox_id,
                    uid: 44,
                    uidvalidity: 1,
                    raw: None,
                    ..Default::default()
                },
            )
        })
        .unwrap();

    let err = fx
        .store
        .attachment_bytes(message_id, "0")
        .await
        .unwrap_err();
    assert_eq!(err.reason(), ErrorReason::FailedPrecondition);
}

// ---------------------------------------------------------------------------
// Flag validation
// ---------------------------------------------------------------------------

#[test]
fn is_safe_flag_admits_predefined_and_keyword_flags() {
    for ok in [
        "\\Seen",
        "\\Flagged",
        "\\Answered",
        "\\Draft",
        "Junk",
        "To-Do",
        "a.b_c",
    ] {
        assert!(is_safe_flag(ok), "{ok:?} should be a safe flag");
    }
}

#[test]
fn is_safe_flag_rejects_anything_that_could_smuggle_imap_syntax() {
    for bad in [
        "",
        "\\",
        " ",
        "has space",
        "(paren)",
        "quo\"te",
        "back\\slash",
        "cr\rlf",
    ] {
        assert!(!is_safe_flag(bad), "{bad:?} should not be a safe flag");
    }
}

// ---------------------------------------------------------------------------
// List page-size normalization
// ---------------------------------------------------------------------------

#[test]
fn normalize_limit_uses_the_default_for_zero_or_negative() {
    assert_eq!(normalize_limit(0), DEFAULT_LIST_LIMIT);
    assert_eq!(normalize_limit(-1), DEFAULT_LIST_LIMIT);
    assert_eq!(normalize_limit(i64::MIN), DEFAULT_LIST_LIMIT);
}

#[test]
fn normalize_limit_passes_through_a_reasonable_request() {
    assert_eq!(normalize_limit(1), 1);
    assert_eq!(normalize_limit(50), 50);
    assert_eq!(normalize_limit(MAX_LIST_LIMIT), MAX_LIST_LIMIT);
}

#[test]
fn normalize_limit_clamps_a_pathological_request_to_the_cap() {
    assert_eq!(normalize_limit(MAX_LIST_LIMIT + 1), MAX_LIST_LIMIT);
    assert_eq!(normalize_limit(i64::MAX), MAX_LIST_LIMIT);
}

#[tokio::test]
async fn list_never_returns_more_than_the_cap() {
    // The unit tests above prove the clamp function; this proves `list`
    // actually applies it rather than passing the raw request straight to
    // `repo::list_messages`.
    let fx = Fixture::new();
    let (account_id, inbox_id, _archive_id, _message_id) = fx.seed(&[]);
    for uid in 1..=5 {
        fx.tmp
            .db
            .with_write(move |c| {
                repo::insert_message(
                    c,
                    &NewMessage {
                        account_id,
                        mailbox_id: inbox_id,
                        uid: 1000 + uid,
                        uidvalidity: 1,
                        ..Default::default()
                    },
                )
            })
            .unwrap();
    }

    // 6 messages exist (the one from `seed` plus 5 more); asking for a limit
    // of 2 must return exactly 2, not the whole mailbox.
    let capped = fx.store.list(inbox_id, 2).await.unwrap();
    assert_eq!(capped.len(), 2);
}

/// A span field declared by `#[tracing::instrument]` actually carries a value.
///
/// This is a regression guard for a real bug, not a smoke test. Under
/// `tracing-attributes` 0.1.31 the bare-identifier form — `fields(message_id)`
/// with no `= value` — does two things at once: it declares the field as
/// `tracing::field::Empty`, and it suppresses the automatic recording of the
/// argument that shares its name. The result is a span whose field is
/// guaranteed to be empty, which is strictly less information than writing no
/// `fields(..)` at all. Every handler in this module carried that form, so the
/// instrumentation added for observability recorded nothing.
///
/// Asserting on the value rather than the field name is what makes this bite:
/// the name is present in the output either way, so a test looking only for
/// `"message_id"` would pass against the broken version.
#[tokio::test]
async fn an_instrumented_handler_records_its_field_values() {
    use std::io;
    use tracing_subscriber::fmt::MakeWriter;
    use tracing_subscriber::layer::SubscriberExt as _;

    #[derive(Clone)]
    struct BufWriter(Arc<Mutex<Vec<u8>>>);

    impl io::Write for BufWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            let mut guard = self
                .0
                .lock()
                .map_err(|_| io::Error::other("log buffer poisoned"))?;
            guard.extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'a> MakeWriter<'a> for BufWriter {
        type Writer = BufWriter;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    let fx = Fixture::new();
    // A message that does not exist: `get` is instrumented with `err`, so the
    // miss is enough to close the span with its field recorded.
    let missing: i64 = 987_654;

    let buf = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(
        tracing_subscriber::fmt::layer()
            .json()
            .with_span_events(tracing_subscriber::fmt::format::FmtSpan::CLOSE)
            .with_writer(BufWriter(buf.clone())),
    );

    // `get` is instrumented with `err`, so a miss closes the span with the
    // field recorded — no successful fixture read needed.
    let guard = tracing::subscriber::set_default(subscriber);
    let _ = fx.store.get(missing).await;
    drop(guard);

    let captured = String::from_utf8(buf.lock().unwrap().clone()).unwrap();
    assert!(!captured.is_empty(), "nothing reached the subscriber");
    // Assert on the span field carrying the value — `"message_id":987654` —
    // not on the bare number. The number also appears in the `err`-recorded
    // message ("not found: message 987654"), so a `contains("987654")` check
    // passes against the broken build and proves nothing. An `Empty` field is
    // omitted from the JSON entirely, so this is what tells the two apart.
    assert!(
        captured.contains(&format!("\"message_id\":{missing}")),
        "message_id was declared on the span but never given a value — the \
         bare `fields(message_id)` form declares it Empty and suppresses the \
         auto-recorded argument. Captured: {captured}"
    );
}
