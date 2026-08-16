//! Integration test: `MailService.ListUnified` end-to-end against an
//! in-process tonic server over a Unix domain socket (task 80).
//!
//! The unified inbox is a *read across accounts*, and the interesting failures
//! are all in the paging. A page is a window over an order that is changing
//! underneath it — mail arrives, an account is added, an account is deleted —
//! and the contract this suite holds it to is the same one the per-mailbox
//! listing has: nothing is repeated, nothing is skipped, and the order does not
//! depend on how the caller chose to page. So the cases are deliberately the
//! nasty ones: a page boundary that lands between two accounts, an account
//! appearing and disappearing mid-walk, and a timestamp tie spanning accounts.
//!
//! Deduplication gets the same treatment. The same mail delivered to two of
//! your addresses must appear once *whichever page each copy would fall on*,
//! so the dedup cases are paged one row at a time, where a
//! window-local implementation would let the twin through.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rmail_core::events::{EventLog, Retention};
use rmail_core::imap::mutate::ImapMutator;
use rmail_core::mail::MailStore;
use rmail_core::page::NEXT_PAGE_TOKEN_METADATA_KEY;
use rmail_core::repo::{self, NewAccount, NewMailbox, NewMessage};
use rmail_core::sync::{SyncEngine, SyncOptions};
use rmail_core::Error;
use rmail_proto::v1::mail_service_client::MailServiceClient;
use rmail_proto::v1::{ListMessagesRequest, ListUnifiedRequest, Message, SetFlagsRequest};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tonic::transport::Channel;
use tonic::Code;

static COUNTER: AtomicU32 = AtomicU32::new(0);

// ---------------------------------------------------------------------------
// A recording IMAP mutator, so "the action went to the right account" is an
// assertion about what was sent to which mailbox, not about a return value.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct SetFlagsCall {
    account_id: i64,
    mailbox: String,
    uid: i64,
}

#[derive(Debug, Default)]
struct RecordingImap {
    set_flags: Mutex<Vec<SetFlagsCall>>,
}

#[async_trait::async_trait]
impl ImapMutator for RecordingImap {
    async fn set_flags(
        &self,
        account_id: i64,
        mailbox: &str,
        _uidvalidity: i64,
        uid: i64,
        _flags: &[String],
    ) -> Result<(), Error> {
        self.set_flags.lock().unwrap().push(SetFlagsCall {
            account_id,
            mailbox: mailbox.to_owned(),
            uid,
        });
        Ok(())
    }

    async fn move_message(
        &self,
        _account_id: i64,
        _mailbox: &str,
        _uidvalidity: i64,
        _uid: i64,
        _dest: &str,
    ) -> Result<(), Error> {
        Ok(())
    }

    async fn copy_message(
        &self,
        _account_id: i64,
        _mailbox: &str,
        _uidvalidity: i64,
        _uid: i64,
        _dest: &str,
    ) -> Result<(), Error> {
        Ok(())
    }

    async fn delete_message(
        &self,
        _account_id: i64,
        _mailbox: &str,
        _uidvalidity: i64,
        _uid: i64,
    ) -> Result<(), Error> {
        Ok(())
    }

    async fn store_keyword(
        &self,
        _account_id: i64,
        _mailbox: &str,
        _uidvalidity: i64,
        _uids: &[i64],
        _keyword: &str,
        _prefer_gmail_label: bool,
        _add: bool,
    ) -> Result<(), Error> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

struct TestServer {
    socket: PathBuf,
    db_path: PathBuf,
    db: rmail_core::Database,
    imap: Arc<RecordingImap>,
    shutdown: oneshot::Sender<()>,
    handle: JoinHandle<Result<(), rmaild::ServeError>>,
}

/// A message to seed, in the shape these tests care about.
#[derive(Debug, Clone)]
struct Seed {
    mailbox_id: i64,
    account_id: i64,
    uid: i64,
    date: Option<i64>,
    internaldate: Option<i64>,
    message_id: Option<String>,
    subject: String,
}

impl TestServer {
    async fn start() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let socket = PathBuf::from("/tmp").join(format!("rmail-unified-{pid}-{n}.sock"));
        let db_path = std::env::temp_dir().join(format!("rmail-unified-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", db_path.display())));
        }
        let db = rmail_core::Database::open(&db_path).unwrap();
        let log = EventLog::new(db.clone(), Retention::unlimited());
        let engine = SyncEngine::new(db.clone(), log.clone(), SyncOptions::default());
        let imap = Arc::new(RecordingImap::default());
        let mail_store = MailStore::new(
            db.clone(),
            log.clone(),
            imap.clone() as Arc<dyn ImapMutator>,
        );

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let server_socket = socket.clone();
        let server_db = db.clone();
        let handle = tokio::spawn(async move {
            // Semantic indexing off: this suite exercises one read RPC, and
            // the default would make every test load an ONNX model.
            let mut config = rmail_core::Config::default();
            config.index.semantic.enabled = false;
            rmaild::serve_uds_with_engine_and_mail_store(
                &server_socket,
                server_db,
                engine,
                mail_store,
                &config,
                async move {
                    let _ = shutdown_rx.await;
                },
            )
            .await
        });

        let mut ready = false;
        for _ in 0..200 {
            if rmail_core::connect_uds(&socket).await.is_ok() {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(ready, "server never became ready");

        Self {
            socket,
            db_path,
            db,
            imap,
            shutdown: shutdown_tx,
            handle,
        }
    }

    async fn client(&self) -> MailServiceClient<Channel> {
        MailServiceClient::new(rmail_core::connect_uds(&self.socket).await.unwrap())
    }

    /// An account with an `INBOX` and an `Archive`. Returns
    /// `(account_id, inbox_id, archive_id)`.
    fn account(&self, name: &str) -> (i64, i64, i64) {
        self.account_with_inbox_named(name, "INBOX")
    }

    /// The same, with the inbox folder under a chosen name — so a test can
    /// prove that `inbox` counts and `INBOX/Receipts` does not.
    fn account_with_inbox_named(&self, name: &str, inbox: &str) -> (i64, i64, i64) {
        let account_id = self
            .db
            .with_write(|c| {
                repo::insert_account(
                    c,
                    &NewAccount {
                        name: name.to_owned(),
                        ..Default::default()
                    },
                )
            })
            .unwrap();
        let inbox_id = self.mailbox(account_id, inbox);
        let archive_id = self.mailbox(account_id, "Archive");
        (account_id, inbox_id, archive_id)
    }

    fn mailbox(&self, account_id: i64, name: &str) -> i64 {
        let name = name.to_owned();
        self.db
            .with_write(move |c| {
                repo::insert_mailbox(
                    c,
                    &NewMailbox {
                        account_id,
                        name,
                        ..Default::default()
                    },
                )
            })
            .unwrap()
    }

    fn seed(&self, seed: Seed) -> i64 {
        self.db
            .with_write(move |c| {
                repo::insert_message(
                    c,
                    &NewMessage {
                        account_id: seed.account_id,
                        mailbox_id: seed.mailbox_id,
                        uid: seed.uid,
                        uidvalidity: 1,
                        message_id: seed.message_id.clone(),
                        subject: Some(seed.subject.clone()),
                        date: seed.date,
                        internaldate: seed.internaldate,
                        ..Default::default()
                    },
                )
            })
            .unwrap()
    }

    /// Seed one dated message into a mailbox, returning its id.
    fn message(&self, account_id: i64, mailbox_id: i64, uid: i64, date: i64, subject: &str) -> i64 {
        self.seed(Seed {
            mailbox_id,
            account_id,
            uid,
            date: Some(date),
            internaldate: Some(date),
            message_id: Some(format!("<{subject}@example.com>")),
            subject: subject.to_owned(),
        })
    }

    fn set_flags_calls(&self) -> Vec<SetFlagsCall> {
        self.imap.set_flags.lock().unwrap().clone()
    }

    async fn stop(self) {
        let _ = self.shutdown.send(());
        let _ = tokio::time::timeout(Duration::from_secs(10), self.handle).await;
        for suffix in ["", "-wal", "-shm"] {
            let _ =
                std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.db_path.display())));
        }
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// One page: the rows, plus the next-page token from the initial metadata.
async fn page(
    client: &mut MailServiceClient<Channel>,
    page_size: i32,
    token: &str,
) -> (Vec<Message>, Option<String>) {
    let response = client
        .list_unified(ListUnifiedRequest {
            page_size,
            page_token: token.to_owned(),
        })
        .await
        .expect("ListUnified");
    let next = response
        .metadata()
        .get(NEXT_PAGE_TOKEN_METADATA_KEY)
        .map(|value| value.to_str().unwrap().to_owned());
    let mut stream = response.into_inner();
    let mut rows = Vec::new();
    while let Some(message) = stream.message().await.expect("stream") {
        rows.push(message);
    }
    (rows, next)
}

/// Walk every page at `page_size` and return the concatenated rows.
///
/// The bound is a guard against a token that never terminates: a paging bug
/// that loops forever would otherwise hang the suite rather than fail it.
async fn walk(client: &mut MailServiceClient<Channel>, page_size: i32) -> Vec<Message> {
    let mut all = Vec::new();
    let mut token = String::new();
    for _ in 0..100 {
        let (rows, next) = page(client, page_size, &token).await;
        all.extend(rows);
        match next {
            Some(next) => token = next,
            None => return all,
        }
    }
    panic!("paging never terminated");
}

fn subjects(rows: &[Message]) -> Vec<String> {
    rows.iter()
        .map(|m| m.subject.clone().unwrap_or_default())
        .collect()
}

// ---------------------------------------------------------------------------
// Merge and order
// ---------------------------------------------------------------------------

#[tokio::test]
async fn every_accounts_inbox_is_merged_newest_first_and_nothing_else_is() {
    let server = TestServer::start().await;
    let (a1, inbox1, archive1) = server.account("one");
    let (a2, inbox2, _) = server.account("two");
    server.message(a1, inbox1, 1, 100, "oldest");
    server.message(a2, inbox2, 1, 200, "middle");
    server.message(a1, inbox1, 2, 300, "newest");
    // Not an inbox: an archived message must not surface in the unified view.
    server.message(a1, archive1, 3, 400, "archived");
    // A child of the inbox is a different folder, not the inbox.
    let sub = server.mailbox(a1, "INBOX/Receipts");
    server.message(a1, sub, 4, 500, "receipt");

    let mut client = server.client().await;
    let (rows, next) = page(&mut client, 50, "").await;

    assert_eq!(subjects(&rows), ["newest", "middle", "oldest"]);
    assert!(next.is_none(), "a complete page must not offer a token");
    server.stop().await;
}

#[tokio::test]
async fn the_inbox_name_is_matched_case_insensitively() {
    // RFC 3501 §5.1: INBOX is case-insensitive, and servers do vary.
    let server = TestServer::start().await;
    let (a1, inbox1, _) = server.account_with_inbox_named("one", "inbox");
    server.message(a1, inbox1, 1, 100, "lowercase");

    let mut client = server.client().await;
    let (rows, _) = page(&mut client, 50, "").await;
    assert_eq!(subjects(&rows), ["lowercase"]);
    server.stop().await;
}

#[tokio::test]
async fn an_empty_unified_inbox_is_an_empty_page_with_no_token() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let (rows, next) = page(&mut client, 50, "").await;
    assert!(rows.is_empty());
    assert!(next.is_none());
    server.stop().await;
}

#[tokio::test]
async fn a_unified_row_names_the_account_and_folder_it_really_lives_in() {
    let server = TestServer::start().await;
    let (a1, inbox1, _) = server.account("one");
    let (a2, inbox2, _) = server.account_with_inbox_named("two", "INBOX");
    server.message(a1, inbox1, 11, 100, "from-one");
    server.message(a2, inbox2, 22, 200, "from-two");

    let mut client = server.client().await;
    let (rows, _) = page(&mut client, 50, "").await;

    let two = &rows[0];
    assert_eq!(two.account_id, a2);
    assert_eq!(two.mailbox_id, inbox2);
    let one = &rows[1];
    assert_eq!(one.account_id, a1);
    assert_eq!(one.mailbox_id, inbox1);

    // The point of carrying the real ids: an action on a unified row is
    // routed back to its own account and folder by the ordinary mutation
    // path, with nothing unified-specific in between.
    client
        .set_flags(SetFlagsRequest {
            message_id: two.id,
            flags: vec!["\\Seen".to_owned()],
            idempotency_key: String::new(),
        })
        .await
        .expect("SetFlags on a unified row");

    assert_eq!(
        server.set_flags_calls(),
        vec![SetFlagsCall {
            account_id: a2,
            mailbox: "INBOX".to_owned(),
            uid: 22,
        }]
    );
    server.stop().await;
}

// ---------------------------------------------------------------------------
// Deduplication
// ---------------------------------------------------------------------------

#[tokio::test]
async fn one_message_delivered_to_two_accounts_appears_once() {
    let server = TestServer::start().await;
    let (a1, inbox1, _) = server.account("one");
    let (a2, inbox2, _) = server.account("two");
    let shared = Some("<shared@example.com>".to_owned());
    server.seed(Seed {
        mailbox_id: inbox1,
        account_id: a1,
        uid: 1,
        date: Some(200),
        internaldate: Some(200),
        message_id: shared.clone(),
        subject: "copy-in-one".to_owned(),
    });
    let second = server.seed(Seed {
        mailbox_id: inbox2,
        account_id: a2,
        uid: 1,
        date: Some(200),
        internaldate: Some(200),
        message_id: shared,
        subject: "copy-in-two".to_owned(),
    });
    server.message(a1, inbox1, 2, 100, "unrelated");

    let mut client = server.client().await;
    let (rows, _) = page(&mut client, 50, "").await;

    assert_eq!(rows.len(), 2, "the duplicate should have collapsed");
    // The surviving copy is the one that sorts first: same timestamp, so the
    // higher id — which is the copy a newest-first reader would have seen.
    assert_eq!(rows[0].id, second);
    assert_eq!(subjects(&rows), ["copy-in-two", "unrelated"]);
    server.stop().await;
}

#[tokio::test]
async fn a_duplicate_is_suppressed_even_when_its_twin_is_on_another_page() {
    // The case a page-local deduplication passes and a row-local one catches:
    // with one row per page, the two copies are never in the same window.
    let server = TestServer::start().await;
    let (a1, inbox1, _) = server.account("one");
    let (a2, inbox2, _) = server.account("two");
    let shared = Some("<shared@example.com>".to_owned());
    server.message(a1, inbox1, 1, 500, "newest");
    server.seed(Seed {
        mailbox_id: inbox1,
        account_id: a1,
        uid: 2,
        date: Some(400),
        internaldate: Some(400),
        message_id: shared.clone(),
        subject: "copy-a".to_owned(),
    });
    server.message(a2, inbox2, 3, 300, "middle");
    server.seed(Seed {
        mailbox_id: inbox2,
        account_id: a2,
        uid: 4,
        // An older copy of the same mail — two pages away from its twin.
        date: Some(200),
        internaldate: Some(200),
        message_id: shared,
        subject: "copy-b".to_owned(),
    });
    server.message(a2, inbox2, 5, 100, "oldest");

    let mut client = server.client().await;
    let one_at_a_time = subjects(&walk(&mut client, 1).await);
    assert_eq!(
        one_at_a_time,
        ["newest", "copy-a", "middle", "oldest"],
        "the older copy must stay suppressed across page boundaries"
    );
    // And the answer must not depend on how the caller paged.
    let in_one_page = subjects(&walk(&mut client, 50).await);
    assert_eq!(one_at_a_time, in_one_page);
    server.stop().await;
}

#[tokio::test]
async fn an_archived_copy_does_not_suppress_the_inbox_copy() {
    // Deduplication is scoped to the inboxes. Drop that scope from the
    // subquery and *any* folder's copy suppresses the inbox one — and every
    // Gmail account keeps a copy of every message in `[Gmail]/All Mail`, so
    // the unified inbox would simply be empty for them. The archived copy
    // here is deliberately the *newer* of the two, which is what makes it win
    // the "sorts first" comparison if it is allowed into it at all.
    let server = TestServer::start().await;
    let (a1, inbox1, archive1) = server.account("one");
    let shared = Some("<shared@example.com>".to_owned());
    server.seed(Seed {
        mailbox_id: inbox1,
        account_id: a1,
        uid: 1,
        date: Some(100),
        internaldate: Some(100),
        message_id: shared.clone(),
        subject: "in-the-inbox".to_owned(),
    });
    server.seed(Seed {
        mailbox_id: archive1,
        account_id: a1,
        uid: 2,
        date: Some(900),
        internaldate: Some(900),
        message_id: shared,
        subject: "in-the-archive".to_owned(),
    });

    let mut client = server.client().await;
    let (rows, _) = page(&mut client, 50, "").await;
    assert_eq!(
        subjects(&rows),
        ["in-the-inbox"],
        "a copy outside the inbox must neither appear nor suppress the inbox copy"
    );
    server.stop().await;
}

#[tokio::test]
async fn messages_with_no_message_id_are_never_deduplicated() {
    // Two messages with no identity are two messages, not one. Collapsing
    // them would hide unrelated mail — the opposite of the feature.
    let server = TestServer::start().await;
    let (a1, inbox1, _) = server.account("one");
    let (a2, inbox2, _) = server.account("two");
    for (account, inbox, uid, date, subject, message_id) in [
        (a1, inbox1, 1, 400, "no-id-one", None),
        (a2, inbox2, 2, 300, "no-id-two", None),
        // A malformed `Message-ID: <>` parses to an empty string, which must
        // not make every such message the same message.
        (a1, inbox1, 3, 200, "empty-id-one", Some(String::new())),
        (a2, inbox2, 4, 100, "empty-id-two", Some(String::new())),
    ] {
        server.seed(Seed {
            mailbox_id: inbox,
            account_id: account,
            uid,
            date: Some(date),
            internaldate: Some(date),
            message_id,
            subject: subject.to_owned(),
        });
    }

    let mut client = server.client().await;
    let rows = walk(&mut client, 1).await;
    assert_eq!(
        subjects(&rows),
        ["no-id-one", "no-id-two", "empty-id-one", "empty-id-two"]
    );
    server.stop().await;
}

// ---------------------------------------------------------------------------
// Paging
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_page_boundary_between_two_accounts_neither_repeats_nor_skips() {
    let server = TestServer::start().await;
    let (a1, inbox1, _) = server.account("one");
    let (a2, inbox2, _) = server.account("two");
    let (a3, inbox3, _) = server.account("three");
    // Interleaved on purpose, so a boundary at any page size falls between
    // accounts somewhere.
    for (i, (account, inbox)) in [
        (a1, inbox1),
        (a2, inbox2),
        (a3, inbox3),
        (a2, inbox2),
        (a1, inbox1),
        (a3, inbox3),
        (a1, inbox1),
    ]
    .into_iter()
    .enumerate()
    {
        let n = 7 - i as i64;
        server.message(account, inbox, i as i64 + 1, n * 100, &format!("m{n}"));
    }

    let mut client = server.client().await;
    let whole = subjects(&walk(&mut client, 50).await);
    assert_eq!(whole, ["m7", "m6", "m5", "m4", "m3", "m2", "m1"]);
    // Every page size must produce the same sequence: the boundary lands in a
    // different place each time, and none of them may drop or repeat a row.
    for size in 1..=8 {
        let paged = subjects(&walk(&mut client, size).await);
        assert_eq!(paged, whole, "page size {size} changed the result");
    }
    server.stop().await;
}

#[tokio::test]
async fn an_account_added_mid_walk_joins_below_the_cursor_and_repeats_nothing() {
    let server = TestServer::start().await;
    let (a1, inbox1, _) = server.account("one");
    server.message(a1, inbox1, 1, 500, "a-newest");
    server.message(a1, inbox1, 2, 300, "a-middle");
    server.message(a1, inbox1, 3, 100, "a-oldest");

    let mut client = server.client().await;
    let (first, token) = page(&mut client, 2, "").await;
    assert_eq!(subjects(&first), ["a-newest", "a-middle"]);
    let token = token.expect("a token for the third row");

    // A whole account appears between the two pages.
    let (a2, inbox2, _) = server.account("two");
    // Below the cursor (older than "a-middle"): it belongs in this walk.
    server.message(a2, inbox2, 1, 200, "b-below");
    // Above the cursor (newer than the newest row already served): it does
    // not, and must not be smuggled in by a later page — a client that wants
    // it starts a new walk.
    server.message(a2, inbox2, 2, 900, "b-above");

    let (second, next) = page(&mut client, 2, &token).await;
    assert_eq!(subjects(&second), ["b-below", "a-oldest"]);
    assert!(next.is_none());

    // And a fresh walk sees everything, including the newer arrival.
    let restarted = subjects(&walk(&mut client, 2).await);
    assert_eq!(
        restarted,
        ["b-above", "a-newest", "a-middle", "b-below", "a-oldest"]
    );
    server.stop().await;
}

#[tokio::test]
async fn an_account_deleted_mid_walk_takes_only_its_own_rows() {
    let server = TestServer::start().await;
    let (a1, inbox1, _) = server.account("one");
    let (a2, inbox2, _) = server.account("two");
    server.message(a1, inbox1, 1, 500, "a-newest");
    // The cursor will point at this row — and then it will not exist. A
    // keyset cursor is a position in a value ordering, not a reference to a
    // row, so the walk must continue from where it was regardless.
    server.message(a2, inbox2, 1, 400, "b-cursor");
    server.message(a1, inbox1, 2, 300, "a-middle");
    server.message(a2, inbox2, 2, 200, "b-gone");
    server.message(a1, inbox1, 3, 100, "a-oldest");

    let mut client = server.client().await;
    let (first, token) = page(&mut client, 2, "").await;
    assert_eq!(subjects(&first), ["a-newest", "b-cursor"]);
    let token = token.expect("a token for the rest");

    // Deleting the account cascades to its mailboxes and messages — including
    // the row the cursor was taken from.
    rmail_core::account::delete(&server.db, a2).await.unwrap();

    let (second, next) = page(&mut client, 2, &token).await;
    assert_eq!(subjects(&second), ["a-middle", "a-oldest"]);
    assert!(next.is_none(), "nothing is left after the last row");
    server.stop().await;
}

#[tokio::test]
async fn a_timestamp_tie_across_accounts_is_ordered_deterministically() {
    let server = TestServer::start().await;
    let (a1, inbox1, _) = server.account("one");
    let (a2, inbox2, _) = server.account("two");
    let (a3, inbox3, _) = server.account("three");
    // Every message shares one timestamp: a bulk import, or a mailing list
    // fanning out. Only the id can break the tie, and it must break it the
    // same way every time and at every page size.
    let mut ids = Vec::new();
    for (i, (account, inbox)) in [
        (a1, inbox1),
        (a2, inbox2),
        (a3, inbox3),
        (a1, inbox1),
        (a2, inbox2),
    ]
    .into_iter()
    .enumerate()
    {
        ids.push(server.message(
            account,
            inbox,
            i as i64 + 1,
            1_700_000_000,
            &format!("tie{i}"),
        ));
    }

    let mut client = server.client().await;
    let first = walk(&mut client, 50).await;
    let expected: Vec<i64> = {
        let mut sorted = ids.clone();
        sorted.sort_unstable_by(|a, b| b.cmp(a));
        sorted
    };
    assert_eq!(
        first.iter().map(|m| m.id).collect::<Vec<_>>(),
        expected,
        "a tie must be broken by id, descending"
    );
    // Same answer however it is paged, and however often it is asked.
    for size in [1, 2, 3, 50] {
        let again = walk(&mut client, size).await;
        assert_eq!(
            again.iter().map(|m| m.id).collect::<Vec<_>>(),
            expected,
            "page size {size} reordered a tie"
        );
    }
    server.stop().await;
}

#[tokio::test]
async fn a_message_with_no_date_sorts_by_arrival_and_still_pages() {
    let server = TestServer::start().await;
    let (a1, inbox1, _) = server.account("one");
    let (a2, inbox2, _) = server.account("two");
    server.message(a1, inbox1, 1, 300, "dated");
    // No `Date` header: the listing key falls back to INTERNALDATE.
    server.seed(Seed {
        mailbox_id: inbox2,
        account_id: a2,
        uid: 1,
        date: None,
        internaldate: Some(400),
        message_id: Some("<undated@example.com>".to_owned()),
        subject: "arrived-later".to_owned(),
    });
    // Neither: the key is 0, which sorts last rather than becoming
    // unreachable to a cursor.
    server.seed(Seed {
        mailbox_id: inbox2,
        account_id: a2,
        uid: 2,
        date: None,
        internaldate: None,
        message_id: Some("<neither@example.com>".to_owned()),
        subject: "no-timestamps".to_owned(),
    });

    let mut client = server.client().await;
    assert_eq!(
        subjects(&walk(&mut client, 1).await),
        ["arrived-later", "dated", "no-timestamps"]
    );
    server.stop().await;
}

// ---------------------------------------------------------------------------
// Token binding and argument errors
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_mailbox_list_token_cannot_be_replayed_against_the_unified_inbox() {
    let server = TestServer::start().await;
    let (a1, inbox1, _) = server.account("one");
    server.message(a1, inbox1, 1, 300, "one");
    server.message(a1, inbox1, 2, 200, "two");

    let mut client = server.client().await;
    let response = client
        .list(ListMessagesRequest {
            mailbox_id: inbox1,
            page_size: 1,
            page_token: String::new(),
        })
        .await
        .expect("List");
    let list_token = response
        .metadata()
        .get(NEXT_PAGE_TOKEN_METADATA_KEY)
        .expect("List paginated")
        .to_str()
        .unwrap()
        .to_owned();

    let status = client
        .list_unified(ListUnifiedRequest {
            page_size: 1,
            page_token: list_token,
        })
        .await
        .expect_err("a List token must not resume a unified listing");
    assert_eq!(status.code(), Code::InvalidArgument);
    server.stop().await;
}

#[tokio::test]
async fn a_unified_token_cannot_be_replayed_against_a_mailbox_listing() {
    let server = TestServer::start().await;
    let (a1, inbox1, _) = server.account("one");
    server.message(a1, inbox1, 1, 300, "one");
    server.message(a1, inbox1, 2, 200, "two");

    let mut client = server.client().await;
    let (_, token) = page(&mut client, 1, "").await;
    let token = token.expect("ListUnified paginated");

    let status = client
        .list(ListMessagesRequest {
            mailbox_id: inbox1,
            page_size: 1,
            page_token: token,
        })
        .await
        .expect_err("a unified token must not resume a mailbox listing");
    assert_eq!(status.code(), Code::InvalidArgument);
    server.stop().await;
}

#[tokio::test]
async fn a_malformed_page_token_is_invalid_argument() {
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let status = client
        .list_unified(ListUnifiedRequest {
            page_size: 10,
            page_token: "not-a-token".to_owned(),
        })
        .await
        .expect_err("a malformed token is refused");
    assert_eq!(status.code(), Code::InvalidArgument);
    server.stop().await;
}

#[tokio::test]
async fn a_negative_page_size_is_invalid_argument() {
    // The same answer `List` gives the same input.
    let server = TestServer::start().await;
    let mut client = server.client().await;
    let status = client
        .list_unified(ListUnifiedRequest {
            page_size: -1,
            page_token: String::new(),
        })
        .await
        .expect_err("a negative page size is refused");
    assert_eq!(status.code(), Code::InvalidArgument);
    server.stop().await;
}
