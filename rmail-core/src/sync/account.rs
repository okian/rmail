//! Account-wide delta sync: [`super::delta_sync_folders`] over one session, and
//! the failure modes that only appear when more than one folder shares it.
//!
//! A session is session-scoped state — enabled extensions, the selected
//! mailbox, the unsolicited-response channel — and every one of those is a way
//! for folder N to corrupt folder N+1. These tests exist because none of that
//! is visible from a single-folder run.

use tokio_util::sync::CancellationToken;

use crate::imap::mock::{MockConfig, MockImap};
use crate::repo;
use crate::storage::Database;
use crate::sync::{delta_sync_folders, DeltaStrategy, SyncOptions};

use super::harness::{commands_starting, connect_with_capabilities, raw, UIDVALIDITY};

/// A temp database with one account and several mailboxes.
struct AccountFixture {
    db: Database,
    account_id: i64,
    path: std::path::PathBuf,
}

impl AccountFixture {
    async fn open(folders: &[&str]) -> Self {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);

        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-syncacct-{pid}-{n}.db"));
        let db = Database::open(&path).unwrap();
        let names: Vec<String> = folders.iter().map(|f| (*f).to_owned()).collect();
        let account_id = db
            .write(move |c| {
                let account_id = repo::insert_account(
                    c,
                    &repo::NewAccount {
                        name: "Personal".to_owned(),
                        ..Default::default()
                    },
                )?;
                for name in &names {
                    repo::insert_mailbox(
                        c,
                        &repo::NewMailbox {
                            account_id,
                            name: name.clone(),
                            ..Default::default()
                        },
                    )?;
                }
                Ok(account_id)
            })
            .await
            .unwrap();
        Self {
            db,
            account_id,
            path,
        }
    }

    /// Give every mailbox a baseline checkpoint, so the run under test takes a
    /// delta path rather than handing back to the initial walk.
    async fn seed_checkpoints(&self, modseq: Option<i64>) {
        self.seed_matching(modseq, None).await;
    }

    /// Give one named mailbox a baseline, leaving the rest without one.
    async fn seed_checkpoint(&self, folder: &str, modseq: Option<i64>) {
        self.seed_matching(modseq, Some(folder.to_owned())).await;
    }

    async fn seed_matching(&self, modseq: Option<i64>, only: Option<String>) {
        let account_id = self.account_id;
        self.db
            .write(move |c| {
                for mailbox in repo::list_mailboxes(c, account_id)? {
                    if only.as_ref().is_some_and(|name| *name != mailbox.name) {
                        continue;
                    }
                    repo::upsert_sync_state(
                        c,
                        &repo::SyncState {
                            mailbox_id: mailbox.id,
                            uidvalidity: Some(UIDVALIDITY),
                            highestmodseq: modseq,
                            last_synced_uid: Some(0),
                            walked_down_to: Some(1),
                            last_sync_at: Some(0),
                            full_sync_done: true,
                        },
                    )?;
                }
                Ok(())
            })
            .await
            .unwrap();
    }

    async fn run(&self, mock: &MockImap, cancel: &CancellationToken) -> super::AccountDeltaReport {
        let (mut session, capabilities) = connect_with_capabilities(mock).await;
        let out = delta_sync_folders(
            &mut session,
            &self.db,
            self.account_id,
            capabilities,
            SyncOptions::default(),
            cancel,
        )
        .await
        .unwrap();
        let _ = session.logout().await;
        out
    }

    fn mailbox_id(&self, folder: &str) -> i64 {
        let account_id = self.account_id;
        let folder = folder.to_owned();
        self.db
            .with_read(move |c| {
                Ok(repo::list_mailboxes(c, account_id)?
                    .into_iter()
                    .find(|m| m.name == folder))
            })
            .unwrap()
            .expect("mailbox")
            .id
    }

    fn name_of(&self, mailbox_id: i64) -> String {
        self.db
            .with_read(move |c| repo::get_mailbox(c, mailbox_id))
            .unwrap()
            .unwrap()
            .name
    }
}

impl Drop for AccountFixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(std::path::PathBuf::from(format!(
                "{}{suffix}",
                self.path.display()
            )));
        }
    }
}

/// A server with three folders, each holding one message.
fn three_folders() -> MockConfig {
    MockConfig::default()
        .password("pw")
        .uidvalidity(u32::try_from(UIDVALIDITY).unwrap())
        .folders(vec![("INBOX", ""), ("Archive", ""), ("Zebra", "")])
        .fetch_in("INBOX", 1, &["\\Seen"], &raw(1))
        .fetch_in("Archive", 1, &["\\Seen"], &raw(2))
        .fetch_in("Zebra", 1, &["\\Seen"], &raw(3))
}

#[tokio::test]
async fn qresync_is_enabled_once_for_the_session_not_once_per_folder() {
    // RFC 5161 §3.1 allows ENABLE only in the authenticated state, before any
    // mailbox is selected. Issuing it per folder works against a lax server and
    // is rejected by a strict one — and because a refusal is non-fatal by
    // design, the failure is silent: every folder after the first quietly loses
    // QRESYNC and pays for an enumeration forever.
    let fx = AccountFixture::open(&["INBOX", "Archive", "Zebra"]).await;
    fx.seed_checkpoints(Some(1)).await;
    let mock = MockImap::start(three_folders()).await;

    let out = fx.run(&mock, &CancellationToken::new()).await;

    assert!(out.failures.is_empty(), "{:?}", out.failures);
    assert_eq!(out.reports.len(), 3);
    assert_eq!(
        commands_starting(&mock, "ENABLE").len(),
        1,
        "one ENABLE for the whole session: {:?}",
        mock.commands()
    );
    for report in &out.reports {
        assert_eq!(
            report.strategy,
            DeltaStrategy::Qresync,
            "folder {} lost QRESYNC",
            fx.name_of(report.mailbox_id)
        );
    }
    assert!(
        commands_starting(&mock, "UID SEARCH").is_empty(),
        "no folder fell back to enumerating: {:?}",
        mock.commands()
    );
}

#[tokio::test]
async fn folders_are_visited_inbox_first() {
    let fx = AccountFixture::open(&["Zebra", "INBOX", "Archive"]).await;
    fx.seed_checkpoints(Some(1)).await;
    let mock = MockImap::start(three_folders()).await;

    let out = fx.run(&mock, &CancellationToken::new()).await;

    let order: Vec<String> = out
        .reports
        .iter()
        .map(|r| fx.name_of(r.mailbox_id))
        .collect();
    assert_eq!(order, vec!["INBOX", "Archive", "Zebra"]);
}

#[tokio::test]
async fn one_broken_folder_does_not_stop_the_others() {
    let fx = AccountFixture::open(&["INBOX", "Broken", "Archive"]).await;
    fx.seed_checkpoints(Some(1)).await;
    let mock = MockImap::start(
        three_folders()
            .folders(vec![("INBOX", ""), ("Broken", ""), ("Archive", "")])
            .unselectable("Broken"),
    )
    .await;

    let out = fx.run(&mock, &CancellationToken::new()).await;

    assert_eq!(out.reports.len(), 2, "INBOX and Archive still synced");
    assert_eq!(out.failures.len(), 1);
    assert_eq!(out.failures[0].name, "Broken");
    assert_eq!(
        out.failures[0].error.reason(),
        crate::ErrorReason::NotFound,
        "an unselectable folder is NOT_FOUND, not an auth problem"
    );
}

#[tokio::test]
async fn a_vanished_report_does_not_cross_folders() {
    // The unsolicited-response channel is session-scoped, not folder-scoped,
    // and RFC 7162 §3.2.10 lets a QRESYNC server announce an expunge at any
    // moment — including on the back of a command that never asked. Left in the
    // channel while INBOX was selected, that notice would be read back during
    // Archive's probe and matched against Archive's UID space, where the same
    // numbers name entirely different, entirely live messages.
    let fx = AccountFixture::open(&["INBOX", "Archive"]).await;
    let mock = MockImap::start(
        MockConfig::default()
            .password("pw")
            .uidvalidity(u32::try_from(UIDVALIDITY).unwrap())
            .folders(vec![("INBOX", ""), ("Archive", "")])
            .fetch_in("INBOX", 1, &["\\Seen"], &raw(1))
            .fetch_in("INBOX", 3, &["\\Seen"], &raw(3))
            // UID 2 is gone from INBOX only.
            .expunged_in("INBOX", 2, 7)
            .fetch_in("Archive", 2, &["\\Seen"], &raw(20))
            .fetch_at("Archive", 3, &["\\Seen"], &raw(30), 5),
    )
    .await;

    // Archive is already downloaded and has a baseline, so it takes the QRESYNC
    // probe. INBOX has none, so it takes the initial walk — whose body FETCHes
    // are what leave INBOX's VANISHED sitting in the channel.
    {
        let (mut session, _) = connect_with_capabilities(&mock).await;
        let archive = fx.mailbox_id("Archive");
        crate::sync::sync_folder(
            &mut session,
            &fx.db,
            archive,
            SyncOptions::default(),
            &CancellationToken::new(),
            |_| {},
        )
        .await
        .unwrap();
        let _ = session.logout().await;
    }
    fx.seed_checkpoint("Archive", Some(1)).await;

    let out = fx.run(&mock, &CancellationToken::new()).await;
    assert!(out.failures.is_empty(), "{:?}", out.failures);
    let strategies: Vec<(String, DeltaStrategy)> = out
        .reports
        .iter()
        .map(|r| (fx.name_of(r.mailbox_id), r.strategy))
        .collect();
    assert_eq!(
        strategies,
        vec![
            ("INBOX".to_owned(), DeltaStrategy::Full),
            ("Archive".to_owned(), DeltaStrategy::Qresync),
        ],
        "the walk runs first and the probe second — the order that leaks"
    );

    let surviving: Vec<(String, i64)> = fx
        .db
        .with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT m.name, x.uid FROM messages x
                 JOIN mailboxes m ON m.id = x.mailbox_id
                 ORDER BY m.name, x.uid",
            )?;
            let rows = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .unwrap();
    assert_eq!(
        surviving,
        vec![
            ("Archive".to_owned(), 2),
            ("Archive".to_owned(), 3),
            ("INBOX".to_owned(), 1),
            ("INBOX".to_owned(), 3),
        ],
        "Archive's UID 2 is alive; INBOX's expunge is not its business"
    );
}

#[tokio::test]
async fn cancellation_stops_the_remaining_folders() {
    let fx = AccountFixture::open(&["INBOX", "Archive", "Zebra"]).await;
    fx.seed_checkpoints(Some(1)).await;
    let mock = MockImap::start(three_folders()).await;

    let cancel = CancellationToken::new();
    cancel.cancel();
    let out = fx.run(&mock, &cancel).await;

    assert!(out.reports.is_empty(), "no folder was started");
    assert!(out.failures.is_empty(), "and none failed either");
}
