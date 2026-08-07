use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use async_trait::async_trait;

use crate::config::{TagSyncMode, TagsImap};
use crate::error::Error;
use crate::imap::conn::login;
use crate::imap::mock::{MockConfig, MockImap};
use crate::imap::mutate::{store_keyword_via, ImapMutator};
use crate::storage::Database;

use super::super::model::Tag;
use super::*;

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct TempDb(PathBuf, Database);

impl TempDb {
    fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-tagssync-{pid}-{n}.db"));
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

fn test_tag(id: i64, name: &str, sync_mode: TagSyncMode) -> Tag {
    Tag {
        id,
        account_id: 1,
        name: name.to_owned(),
        parent_id: None,
        color: None,
        sync_mode,
        imap_keyword: None,
        created_at: 0,
    }
}

fn default_imap_config() -> TagsImap {
    TagsImap::default()
}

/// An [`ImapMutator`] whose [`store_keyword`](ImapMutator::store_keyword)
/// dials a real (plaintext, loopback) [`MockImap`] server fresh per call and
/// drives the real wire path
/// ([`crate::imap::mutate::store_keyword_via`]) against it — the same
/// pattern [`crate::imap::mutate`]'s own tests use, minus the TLS handshake
/// [`MockImap`] cannot speak (see [`crate::imap::mutate`]'s module docs).
/// This is what makes the downgrade test below driven by a genuine IMAP
/// `NO`, not a boolean this test module sets itself.
#[derive(Debug)]
struct MockBackedMutator {
    addr: SocketAddr,
}

#[async_trait]
impl ImapMutator for MockBackedMutator {
    async fn set_flags(
        &self,
        _account_id: i64,
        _mailbox: &str,
        _uidvalidity: i64,
        _uid: i64,
        _flags: &[String],
    ) -> Result<(), Error> {
        Err(Error::unavailable("not exercised by tags::sync's tests"))
    }

    async fn move_message(
        &self,
        _account_id: i64,
        _mailbox: &str,
        _uidvalidity: i64,
        _uid: i64,
        _dest: &str,
    ) -> Result<(), Error> {
        Err(Error::unavailable("not exercised by tags::sync's tests"))
    }

    async fn copy_message(
        &self,
        _account_id: i64,
        _mailbox: &str,
        _uidvalidity: i64,
        _uid: i64,
        _dest: &str,
    ) -> Result<(), Error> {
        Err(Error::unavailable("not exercised by tags::sync's tests"))
    }

    async fn delete_message(
        &self,
        _account_id: i64,
        _mailbox: &str,
        _uidvalidity: i64,
        _uid: i64,
    ) -> Result<(), Error> {
        Err(Error::unavailable("not exercised by tags::sync's tests"))
    }

    async fn store_keyword(
        &self,
        _account_id: i64,
        mailbox: &str,
        uidvalidity: i64,
        uids: &[i64],
        keyword: &str,
        prefer_gmail_label: bool,
        add: bool,
    ) -> Result<(), Error> {
        let stream = tokio::net::TcpStream::connect(self.addr)
            .await
            .map_err(|e| Error::unavailable(e.to_string()))?;
        let mut session = login(stream, "user", "pw").await?;
        // Mirrors `LiveImapMutator::store_keyword`'s own capability probe
        // (see `imap::mutate`'s module docs) rather than trusting
        // `prefer_gmail_label` blindly: a test double that always honored
        // the caller's preference regardless of what the server actually
        // advertises would silently diverge from production the moment a
        // test's `TagsImap` config left `gmail_labels` at its (`true`)
        // default against a mock that isn't configured as Gmail.
        let gmail = if prefer_gmail_label {
            session
                .capabilities()
                .await
                .map(|caps| caps.has_str("X-GM-EXT-1"))
                .unwrap_or(false)
        } else {
            false
        };
        let result = store_keyword_via(
            &mut session,
            gmail,
            mailbox,
            uidvalidity,
            uids,
            keyword,
            add,
        )
        .await;
        let _ = session.logout().await;
        result
    }
}

// ---------------------------------------------------------------------------
// group_by_mailbox
// ---------------------------------------------------------------------------

#[tokio::test]
async fn group_by_mailbox_coalesces_messages_sharing_a_mailbox() {
    let tmp = TempDb::open();
    let (account_id, mailbox_id) = tmp
        .db()
        .with_write(|conn| {
            let account_id = crate::repo::insert_account(
                conn,
                &crate::repo::NewAccount {
                    name: "acct".to_owned(),
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
            Ok::<_, rusqlite::Error>((account_id, mailbox_id))
        })
        .unwrap();
    let mut ids = Vec::new();
    for uid in [3, 1, 7] {
        let id = tmp
            .db()
            .with_write(move |conn| {
                crate::repo::insert_message(
                    conn,
                    &crate::repo::NewMessage {
                        account_id,
                        mailbox_id,
                        uid,
                        uidvalidity: 1,
                        ..Default::default()
                    },
                )
            })
            .unwrap();
        ids.push(id);
    }

    let groups = group_by_mailbox(tmp.db(), &ids).await.unwrap();
    assert_eq!(groups.len(), 1, "all three messages share one mailbox");
    let mut uids = groups[0].uids.clone();
    uids.sort_unstable();
    assert_eq!(uids, vec![1, 3, 7]);
    assert_eq!(groups[0].mailbox_name, "INBOX");
    assert_eq!(groups[0].account_id, account_id);
}

#[tokio::test]
async fn group_by_mailbox_silently_skips_a_message_that_no_longer_exists() {
    let tmp = TempDb::open();
    let groups = group_by_mailbox(tmp.db(), &[999_999]).await.unwrap();
    assert!(groups.is_empty());
}

// ---------------------------------------------------------------------------
// apply_wire
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_local_tag_never_touches_imap() {
    let mock = MockImap::start(MockConfig::default().password("pw")).await;
    let mutator = MockBackedMutator { addr: mock.addr };
    let tag = test_tag(1, "personal", TagSyncMode::Local);
    let groups = vec![MailboxGroup {
        account_id: 1,
        mailbox_name: "INBOX".to_owned(),
        uidvalidity: 1,
        uids: vec![5],
    }];

    let outcome = apply_wire(&mutator, &tag, &default_imap_config(), &groups, true)
        .await
        .unwrap();
    assert_eq!(outcome, WireOutcome::Skipped);
    assert!(
        mock.commands().is_empty(),
        "a local tag must never reach the wire"
    );
}

#[tokio::test]
async fn an_imap_tag_applies_over_a_successful_store() {
    let mock = MockImap::start(MockConfig::default().password("pw").fetch(5, &[], b"body")).await;
    let mutator = MockBackedMutator { addr: mock.addr };
    let tag = test_tag(1, "work", TagSyncMode::Imap);
    let groups = vec![MailboxGroup {
        account_id: 1,
        mailbox_name: "INBOX".to_owned(),
        uidvalidity: 1,
        uids: vec![5],
    }];

    let outcome = apply_wire(&mutator, &tag, &default_imap_config(), &groups, true)
        .await
        .unwrap();
    assert_eq!(outcome, WireOutcome::Applied);
    let commands = mock.commands();
    assert!(
        commands
            .iter()
            .any(|c| c.eq_ignore_ascii_case("UID STORE 5 +FLAGS.SILENT (rmail/work)")),
        "expected the configured keyword_prefix, got: {commands:?}"
    );
}

/// The acceptance criterion by name: `auto` downgrades on a real IMAP `NO`
/// from the mock server -- not a flag this test sets on a fake mutator.
#[tokio::test]
async fn an_auto_tag_downgrades_on_a_real_imap_no() {
    let mock = MockImap::start(
        MockConfig::default()
            .password("pw")
            .fetch(5, &[], b"body")
            .refusing_uid_commands(),
    )
    .await;
    let mutator = MockBackedMutator { addr: mock.addr };
    let tag = test_tag(1, "work", TagSyncMode::Auto);
    let groups = vec![MailboxGroup {
        account_id: 1,
        mailbox_name: "INBOX".to_owned(),
        uidvalidity: 1,
        uids: vec![5],
    }];

    let outcome = apply_wire(&mutator, &tag, &default_imap_config(), &groups, true)
        .await
        .unwrap();
    assert_eq!(
        outcome,
        WireOutcome::Downgrade,
        "an auto tag must downgrade rather than error out"
    );
    // The wire really was attempted -- the downgrade is a *reaction* to a
    // real NO, not a decision made without ever touching IMAP.
    let commands = mock.commands();
    assert!(
        commands
            .iter()
            .any(|c| c.to_ascii_uppercase().starts_with("UID STORE")),
        "expected a real STORE attempt that the mock then refused, got: {commands:?}"
    );
}

#[tokio::test]
async fn a_strict_imap_tag_propagates_the_refusal_instead_of_downgrading() {
    let mock = MockImap::start(
        MockConfig::default()
            .password("pw")
            .fetch(5, &[], b"body")
            .refusing_uid_commands(),
    )
    .await;
    let mutator = MockBackedMutator { addr: mock.addr };
    let tag = test_tag(1, "work", TagSyncMode::Imap);
    let groups = vec![MailboxGroup {
        account_id: 1,
        mailbox_name: "INBOX".to_owned(),
        uidvalidity: 1,
        uids: vec![5],
    }];

    let err = apply_wire(&mutator, &tag, &default_imap_config(), &groups, true)
        .await
        .expect_err("sync_mode=imap must propagate a refusal, not silently downgrade");
    assert_eq!(err.reason(), crate::ErrorReason::Unavailable);
}

#[tokio::test]
async fn apply_wire_issues_exactly_one_store_per_mailbox_group() {
    let mock = MockImap::start(
        MockConfig::default()
            .password("pw")
            .fetch(1, &[], b"a")
            .fetch(2, &[], b"b")
            .fetch(3, &[], b"c"),
    )
    .await;
    let mutator = MockBackedMutator { addr: mock.addr };
    let tag = test_tag(1, "urgent", TagSyncMode::Imap);
    // Two groups over the *same* mock connection target (mailbox name
    // differs only in this test's bookkeeping; what matters is that
    // `apply_wire` calls `store_keyword` once per group, not once per uid).
    let groups = vec![MailboxGroup {
        account_id: 1,
        mailbox_name: "INBOX".to_owned(),
        uidvalidity: 1,
        uids: vec![1, 2, 3],
    }];

    apply_wire(&mutator, &tag, &default_imap_config(), &groups, true)
        .await
        .unwrap();

    let store_commands: Vec<String> = mock
        .commands()
        .into_iter()
        .filter(|c| c.to_ascii_uppercase().starts_with("UID STORE"))
        .collect();
    assert_eq!(
        store_commands.len(),
        1,
        "three uids in one group must coalesce into one STORE, got: {store_commands:?}"
    );
    assert!(store_commands[0].contains("1:3"), "{store_commands:?}");
}
