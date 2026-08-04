//! Persistence idempotency + IMAP fetch-adapter tests.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::imap::conn::login;
use crate::imap::mock::{MockConfig, MockImap};
use crate::repo;
use crate::storage::Database;

use super::*;

static COUNTER: AtomicU32 = AtomicU32::new(0);

const RAW: &[u8] = b"From: Alice <alice@example.com>\r\n\
To: bob@example.com\r\n\
Subject: Fetched\r\n\
Message-ID: <fetched@example.com>\r\n\
Content-Type: multipart/mixed; boundary=\"b\"\r\n\
\r\n\
--b\r\n\
Content-Type: text/plain\r\n\
\r\n\
body text\r\n\
--b\r\n\
Content-Type: application/pdf; name=\"a.pdf\"\r\n\
Content-Disposition: attachment; filename=\"a.pdf\"\r\n\
Content-Transfer-Encoding: base64\r\n\
\r\n\
aGVsbG8=\r\n\
--b--\r\n";

/// A reply to `RAW`, for the threading assertion below.
const REPLY: &[u8] = b"From: Bob <bob@example.com>\r\n\
To: alice@example.com\r\n\
Subject: Re: Fetched\r\n\
Message-ID: <reply@example.com>\r\n\
In-Reply-To: <fetched@example.com>\r\n\
References: <fetched@example.com>\r\n\
\r\n\
replying\r\n";

struct Fixture {
    db: Database,
    path: PathBuf,
    account_id: i64,
    mailbox_id: i64,
}

impl Fixture {
    async fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-msgfetch-{pid}-{n}.db"));
        let db = Database::open(&path).unwrap();
        let (account_id, mailbox_id) = db
            .write(|c| {
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
            .await
            .unwrap();
        Self {
            db,
            path,
            account_id,
            mailbox_id,
        }
    }

    fn fetched(&self, uid: i64) -> FetchedMessage {
        self.fetched_raw(uid, RAW)
    }

    fn fetched_raw(&self, uid: i64, raw: &[u8]) -> FetchedMessage {
        FetchedMessage {
            uid,
            uidvalidity: 10,
            internaldate: Some(1_700_000_000),
            size: Some(raw.len() as i64),
            flags: vec!["\\Seen".to_owned(), "\\Flagged".to_owned()],
            raw: raw.to_vec(),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.path.display())));
        }
    }
}

#[tokio::test]
async fn persist_stores_raw_metadata_attachments_and_flags() {
    let fx = Fixture::open().await;
    let outcome = persist_fetched(&fx.db, fx.account_id, fx.mailbox_id, fx.fetched(1))
        .await
        .unwrap();
    assert!(outcome.inserted);

    let id = outcome.message_id;
    let message = fx
        .db
        .read(move |c| repo::get_message(c, id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(message.subject.as_deref(), Some("Fetched"));
    assert_eq!(message.message_id.as_deref(), Some("fetched@example.com"));
    assert_eq!(
        message.raw.as_deref(),
        Some(RAW),
        "raw RFC822 preserved verbatim"
    );
    assert_eq!(message.body_text.as_deref(), Some("body text"));
    assert!(message.has_attachments);
    assert_eq!(message.internaldate, Some(1_700_000_000));

    let attachments = fx
        .db
        .read(move |c| repo::list_attachments(c, id))
        .await
        .unwrap();
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments[0].filename.as_deref(), Some("a.pdf"));

    let mut flags = fx.db.read(move |c| repo::list_flags(c, id)).await.unwrap();
    flags.sort();
    assert_eq!(flags, vec!["\\Flagged".to_owned(), "\\Seen".to_owned()]);
}

#[tokio::test]
async fn persist_threads_the_message_in_the_same_transaction() {
    let fx = Fixture::open().await;
    let root = persist_fetched(&fx.db, fx.account_id, fx.mailbox_id, fx.fetched(1))
        .await
        .unwrap();
    let reply = persist_fetched(
        &fx.db,
        fx.account_id,
        fx.mailbox_id,
        fx.fetched_raw(2, REPLY),
    )
    .await
    .unwrap();

    let thread_id = root.thread_id.expect("root was threaded on persist");
    assert_eq!(
        reply.thread_id,
        Some(thread_id),
        "the reply joins its parent's thread"
    );

    let thread = fx
        .db
        .read(move |c| repo::get_thread(c, thread_id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(thread.message_count, 2);
    assert_eq!(thread.root_message_id, Some(root.message_id));
    assert_eq!(thread.subject_norm.as_deref(), Some("fetched"));
    assert_eq!(
        thread.participant_list(),
        vec!["alice@example.com", "bob@example.com"]
    );

    // A re-fetch reports the same thread without re-threading.
    let again = persist_fetched(&fx.db, fx.account_id, fx.mailbox_id, fx.fetched(1))
        .await
        .unwrap();
    assert!(!again.inserted);
    assert_eq!(again.thread_id, Some(thread_id));
}

#[tokio::test]
async fn refetch_repairs_a_message_left_without_a_thread() {
    let fx = Fixture::open().await;
    let stored = persist_fetched(&fx.db, fx.account_id, fx.mailbox_id, fx.fetched(1))
        .await
        .unwrap();

    // Simulate a row stored before threading existed: a re-fetch is otherwise
    // a no-op, so without repair this message would never join a conversation.
    fx.db
        .write(|c| {
            c.execute("UPDATE messages SET thread_id = NULL", [])?;
            c.execute("DELETE FROM thread_refs", [])?;
            c.execute("DELETE FROM threads", [])
        })
        .await
        .unwrap();

    let repaired = persist_fetched(&fx.db, fx.account_id, fx.mailbox_id, fx.fetched(1))
        .await
        .unwrap();
    assert!(!repaired.inserted, "still a no-op for the message row");
    let thread_id = repaired.thread_id.expect("re-fetch threaded it");

    let thread = fx
        .db
        .read(move |c| repo::get_thread(c, thread_id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(thread.message_count, 1);
    assert_eq!(thread.root_message_id, Some(stored.message_id));
}

#[tokio::test]
async fn refetch_is_a_noop() {
    let fx = Fixture::open().await;
    let first = persist_fetched(&fx.db, fx.account_id, fx.mailbox_id, fx.fetched(7))
        .await
        .unwrap();
    assert!(first.inserted);

    // Same identity again -> no-op, same id, not re-inserted.
    let second = persist_fetched(&fx.db, fx.account_id, fx.mailbox_id, fx.fetched(7))
        .await
        .unwrap();
    assert!(!second.inserted);
    assert_eq!(second.message_id, first.message_id);

    // Exactly one message and one attachment row (no duplication).
    let mailbox_id = fx.mailbox_id;
    let count = fx
        .db
        .read(move |c| {
            c.query_row(
                "SELECT count(*) FROM messages WHERE mailbox_id = ?1",
                [mailbox_id],
                |r| r.get::<_, i64>(0),
            )
        })
        .await
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn fetch_and_persist_over_imap_mock() {
    let fx = Fixture::open().await;
    let mock = MockImap::start(
        MockConfig::default()
            .password("pw")
            .fetch(101, &["\\Seen"], RAW),
    )
    .await;
    let stream = tokio::net::TcpStream::connect(mock.addr).await.unwrap();
    let mut session = login(stream, "user", "pw").await.unwrap();

    let outcomes = fetch_and_persist(
        &mut session,
        &fx.db,
        fx.account_id,
        fx.mailbox_id,
        10,
        "1:*",
    )
    .await
    .unwrap();
    assert_eq!(outcomes.len(), 1);
    assert!(outcomes[0].inserted);

    let id = outcomes[0].message_id;
    let message = fx
        .db
        .read(move |c| repo::get_message(c, id))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(message.uid, 101);
    assert_eq!(message.uidvalidity, 10);
    assert_eq!(message.subject.as_deref(), Some("Fetched"));
    assert_eq!(message.raw.as_deref(), Some(RAW));
    // INTERNALDATE "01-Jan-2024 00:00:00 +0000" and server RFC822.SIZE.
    assert_eq!(message.internaldate, Some(1_704_067_200));
    assert_eq!(message.size, Some(RAW.len() as i64));

    let flags = fx.db.read(move |c| repo::list_flags(c, id)).await.unwrap();
    assert_eq!(flags, vec!["\\Seen".to_owned()]);

    let _ = session.logout().await;
}
