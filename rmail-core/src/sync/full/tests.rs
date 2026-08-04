//! Full-sync tests: a fresh walk, resuming mid-window, and an incremental
//! re-run that touches the network for nothing.

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use async_imap::Session;
use tokio_util::sync::CancellationToken;

use crate::imap::conn::login;
use crate::imap::mock::{MockConfig, MockImap};
use crate::repo;
use crate::storage::Database;

use super::*;

static COUNTER: AtomicU32 = AtomicU32::new(0);

const UIDVALIDITY: i64 = 42;

/// Build a distinct message for `uid` so every row is independently checkable.
fn raw(uid: u32) -> Vec<u8> {
    format!(
        "From: sender{uid}@example.com\r\n\
         To: me@example.com\r\n\
         Subject: Message {uid}\r\n\
         Message-ID: <m{uid}@example.com>\r\n\
         Date: Mon, 1 Jan 2024 12:00:00 +0000\r\n\
         \r\n\
         body {uid}\r\n"
    )
    .into_bytes()
}

struct Fixture {
    db: Database,
    path: PathBuf,
    account_id: i64,
    mailbox_id: i64,
}

impl Fixture {
    async fn open() -> Self {
        Self::open_with_folders(&["INBOX"]).await
    }

    async fn open_with_folders(names: &[&str]) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-syncfull-{pid}-{n}.db"));
        let db = Database::open(&path).unwrap();
        let names: Vec<String> = names.iter().map(|n| (*n).to_owned()).collect();
        let (account_id, mailbox_id) = db
            .write(move |c| {
                let account_id = repo::insert_account(
                    c,
                    &repo::NewAccount {
                        name: "Personal".to_owned(),
                        ..Default::default()
                    },
                )?;
                let mut first = 0;
                for name in &names {
                    let id = repo::insert_mailbox(
                        c,
                        &repo::NewMailbox {
                            account_id,
                            name: name.clone(),
                            ..Default::default()
                        },
                    )?;
                    if first == 0 {
                        first = id;
                    }
                }
                Ok((account_id, first))
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

    fn message_count(&self) -> i64 {
        self.db
            .with_read(|c| c.query_row("SELECT count(*) FROM messages", [], |r| r.get(0)))
            .unwrap()
    }

    fn stored_uids(&self) -> Vec<i64> {
        let mailbox_id = self.mailbox_id;
        self.db
            .with_read(move |c| repo::list_message_uids(c, mailbox_id, UIDVALIDITY, 1, i64::MAX))
            .unwrap()
    }

    fn sync_state(&self) -> repo::SyncState {
        let mailbox_id = self.mailbox_id;
        self.db
            .with_read(move |c| repo::get_sync_state(c, mailbox_id))
            .unwrap()
            .expect("checkpoint written")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.path.display())));
        }
    }
}

/// A mock holding messages with UIDs `1..=count`.
fn mock_config(count: u32) -> MockConfig {
    let mut config = MockConfig::default()
        .password("pw")
        .uidvalidity(u32::try_from(UIDVALIDITY).unwrap());
    for uid in 1..=count {
        config = config.fetch(uid, &["\\Seen"], &raw(uid));
    }
    config
}

async fn connect(mock: &MockImap) -> Session<tokio::net::TcpStream> {
    let stream = tokio::net::TcpStream::connect(mock.addr).await.unwrap();
    login(stream, "user", "pw").await.unwrap()
}

/// Collect progress observations from a sync run.
#[derive(Default)]
struct Progress(RefCell<Vec<SyncProgress>>);

impl Progress {
    fn sink(&self) -> impl FnMut(SyncProgress) + '_ {
        |p| self.0.borrow_mut().push(p)
    }

    fn observations(&self) -> Vec<SyncProgress> {
        self.0.borrow().clone()
    }
}

#[tokio::test]
async fn fresh_sync_walks_the_whole_folder_newest_first() {
    let fx = Fixture::open().await;
    let mock = MockImap::start(mock_config(25)).await;
    let mut session = connect(&mock).await;
    let progress = Progress::default();

    let report = sync_folder(
        &mut session,
        &fx.db,
        fx.mailbox_id,
        SyncOptions {
            window: 10,
            ..Default::default()
        },
        &CancellationToken::new(),
        progress.sink(),
    )
    .await
    .unwrap();

    assert_eq!(report.fetched, 25);
    assert_eq!(report.already_present, 0);
    assert!(!report.uidvalidity_changed);
    assert_eq!(report.uidvalidity, UIDVALIDITY);
    assert_eq!(fx.stored_uids(), (1..=25).collect::<Vec<i64>>());

    // UIDs 1..=25 in windows of 10, walked downward: 16:25, 6:15, 1:5.
    assert_eq!(mock.fetch_commands(), vec!["16:25", "6:15", "1:5"]);

    let seen = progress.observations();
    assert_eq!(seen.len(), 3, "one observation per window");
    assert_eq!(seen[0].ceiling_uid, 25);
    assert_eq!(seen[0].total, 25);
    assert!(
        seen[0].cursor_uid > seen[2].cursor_uid,
        "the cursor walks downward: {:?}",
        seen.iter().map(|p| p.cursor_uid).collect::<Vec<_>>()
    );
    assert!(!seen[0].done && seen[2].done);
    assert_eq!(seen[2].fetched, 25);

    let state = fx.sync_state();
    assert_eq!(state.uidvalidity, Some(UIDVALIDITY));
    assert_eq!(state.last_synced_uid, Some(25));
    assert!(state.full_sync_done);
    assert!(state.last_sync_at.is_some_and(|t| t > 0));

    let _ = session.logout().await;
}

#[tokio::test]
async fn the_newest_window_lands_before_the_rest() {
    // "Useful early" is the point of walking downward: after the first window
    // the newest mail is already queryable.
    let fx = Fixture::open().await;
    let mock = MockImap::start(mock_config(30)).await;
    let mut session = connect(&mock).await;

    let newest_present = RefCell::new(Vec::new());
    sync_folder(
        &mut session,
        &fx.db,
        fx.mailbox_id,
        SyncOptions {
            window: 10,
            ..Default::default()
        },
        &CancellationToken::new(),
        |p| {
            newest_present
                .borrow_mut()
                .push((p.cursor_uid, fx.stored_uids()));
        },
    )
    .await
    .unwrap();

    let (first_cursor, after_first_window) = newest_present.borrow_mut().remove(0);
    assert_eq!(first_cursor, 21);
    assert_eq!(
        after_first_window,
        (21..=30).collect::<Vec<i64>>(),
        "the newest 10 messages are stored after the first window"
    );

    let _ = session.logout().await;
}

#[tokio::test]
async fn an_interrupted_sync_resumes_without_refetching() {
    const TOTAL: usize = 60;
    let fx = Fixture::open().await;
    let mock = MockImap::start(mock_config(u32::try_from(TOTAL).unwrap())).await;

    // First run: drop the sync future once a window has been committed, which
    // is what a crash or a cancelled RPC does to it mid-walk.
    {
        let mut session = connect(&mock).await;
        let cancel = CancellationToken::new();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let mut tx = Some(tx);
        tokio::select! {
            biased;
            _ = rx => {}
            _ = sync_folder(
                &mut session,
                &fx.db,
                fx.mailbox_id,
                SyncOptions { window: 5, ..Default::default() },
                &cancel,
                |_| {
                    if let Some(tx) = tx.take() {
                        let _ = tx.send(());
                    }
                },
            ) => panic!("the run should have been interrupted before finishing"),
        }
    }

    let after_crash = fx.stored_uids();
    assert!(
        !after_crash.is_empty() && after_crash.len() < TOTAL,
        "the run was interrupted mid-walk, got {} rows",
        after_crash.len()
    );
    assert!(
        !fx.sync_state().full_sync_done,
        "an interrupted walk is not marked complete"
    );
    let fetches_before_resume = mock.fetch_commands().len();

    // Second run: finishes the job and re-fetches nothing already stored.
    let mut session = connect(&mock).await;
    let report = sync_folder(
        &mut session,
        &fx.db,
        fx.mailbox_id,
        SyncOptions {
            window: 5,
            ..Default::default()
        },
        &CancellationToken::new(),
        |_| {},
    )
    .await
    .unwrap();

    assert_eq!(
        report.already_present, 0,
        "the resume walks only the backlog below the low mark, so it does not \
         even re-examine the windows the first run finished"
    );
    assert_eq!(
        report.fetched as usize,
        TOTAL - after_crash.len(),
        "only the missing UIDs were downloaded"
    );
    assert_eq!(fx.stored_uids(), (1..=60).collect::<Vec<i64>>());
    assert_eq!(fx.message_count() as usize, TOTAL, "no duplicate rows");
    assert!(fx.sync_state().full_sync_done);

    // No window already stored was asked for again.
    let resumed: Vec<String> = mock.fetch_commands().split_off(fetches_before_resume);
    let refetched: Vec<&String> = resumed
        .iter()
        .filter(|set| {
            set.split(':')
                .next()
                .and_then(|lo| lo.parse::<usize>().ok())
                .is_some_and(|lo| after_crash.contains(&(lo as i64)))
        })
        .collect();
    assert!(
        refetched.is_empty(),
        "the resume re-fetched already-stored UIDs: {refetched:?}"
    );

    let _ = session.logout().await;
}

#[tokio::test]
async fn a_completed_sync_reruns_without_touching_the_network() {
    let fx = Fixture::open().await;
    let mock = MockImap::start(mock_config(12)).await;

    let mut session = connect(&mock).await;
    let first = sync_folder(
        &mut session,
        &fx.db,
        fx.mailbox_id,
        SyncOptions {
            window: 5,
            ..Default::default()
        },
        &CancellationToken::new(),
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(first.fetched, 12);
    let fetches_after_first = mock.fetch_commands().len();
    assert!(fetches_after_first > 0);

    let second = sync_folder(
        &mut session,
        &fx.db,
        fx.mailbox_id,
        SyncOptions {
            window: 5,
            ..Default::default()
        },
        &CancellationToken::new(),
        |_| {},
    )
    .await
    .unwrap();

    assert_eq!(second.fetched, 0);
    assert_eq!(
        second.windows_fetched, 0,
        "an already-synced folder issues no FETCH at all"
    );
    assert!(second.complete);
    assert_eq!(
        mock.fetch_commands().len(),
        fetches_after_first,
        "the re-run sent no new FETCH command"
    );
    assert_eq!(fx.message_count(), 12);

    let _ = session.logout().await;
}

#[tokio::test]
async fn new_mail_arriving_after_a_full_sync_is_picked_up() {
    let fx = Fixture::open().await;
    let mock = MockImap::start(mock_config(5)).await;
    let mut session = connect(&mock).await;
    sync_folder(
        &mut session,
        &fx.db,
        fx.mailbox_id,
        SyncOptions::default(),
        &CancellationToken::new(),
        |_| {},
    )
    .await
    .unwrap();
    let _ = session.logout().await;

    // The folder grows; a second server run serves UIDs 1..=8.
    let grown = MockImap::start(mock_config(8)).await;
    let mut session = connect(&grown).await;
    let report = sync_folder(
        &mut session,
        &fx.db,
        fx.mailbox_id,
        SyncOptions::default(),
        &CancellationToken::new(),
        |_| {},
    )
    .await
    .unwrap();

    assert_eq!(report.fetched, 3, "only the three new UIDs");
    assert_eq!(
        grown.fetch_commands(),
        vec!["6:8"],
        "only the range above the high mark is requested"
    );
    assert_eq!(fx.stored_uids(), (1..=8).collect::<Vec<i64>>());

    let _ = session.logout().await;
}

#[tokio::test]
async fn an_empty_folder_completes_and_checkpoints() {
    let fx = Fixture::open().await;
    let mock = MockImap::start(mock_config(0)).await;
    let mut session = connect(&mock).await;
    let progress = Progress::default();

    let report = sync_folder(
        &mut session,
        &fx.db,
        fx.mailbox_id,
        SyncOptions::default(),
        &CancellationToken::new(),
        progress.sink(),
    )
    .await
    .unwrap();

    assert_eq!(report.fetched, 0);
    assert_eq!(report.windows_fetched, 0);
    assert!(mock.fetch_commands().is_empty());
    assert!(fx.sync_state().full_sync_done, "an empty folder is synced");
    assert_eq!(progress.observations().len(), 1);
    assert!(progress.observations()[0].done);

    let _ = session.logout().await;
}

#[tokio::test]
async fn a_changed_uidvalidity_is_reported_and_the_new_space_walked() {
    let fx = Fixture::open().await;
    let mailbox_id = fx.mailbox_id;
    // A checkpoint from a previous UID space.
    fx.db
        .write(move |c| {
            repo::upsert_sync_state(
                c,
                &repo::SyncState {
                    mailbox_id,
                    uidvalidity: Some(7),
                    last_synced_uid: Some(99),
                    full_sync_done: true,
                    ..Default::default()
                },
            )
        })
        .await
        .unwrap();

    let mock = MockImap::start(mock_config(3)).await;
    let mut session = connect(&mock).await;
    let report = sync_folder(
        &mut session,
        &fx.db,
        fx.mailbox_id,
        SyncOptions::default(),
        &CancellationToken::new(),
        |_| {},
    )
    .await
    .unwrap();

    assert!(report.uidvalidity_changed);
    assert_eq!(
        report.fetched, 3,
        "the new UID space is walked from scratch"
    );
    assert_eq!(fx.sync_state().uidvalidity, Some(UIDVALIDITY));

    let _ = session.logout().await;
}

#[tokio::test]
async fn syncing_an_unknown_mailbox_is_not_found() {
    let fx = Fixture::open().await;
    let mock = MockImap::start(mock_config(1)).await;
    let mut session = connect(&mock).await;

    let err = sync_folder(
        &mut session,
        &fx.db,
        9_999,
        SyncOptions::default(),
        &CancellationToken::new(),
        |_| {},
    )
    .await
    .unwrap_err();
    assert_eq!(
        tonic::Status::from(err).code(),
        tonic::Code::NotFound,
        "an unknown mailbox is NOT_FOUND at the boundary"
    );

    let _ = session.logout().await;
}

#[tokio::test]
async fn sync_folders_visits_the_inbox_first() {
    let fx = Fixture::open_with_folders(&["Zebra", "INBOX", "Archive"]).await;
    let mock = MockImap::start(mock_config(2)).await;
    let mut session = connect(&mock).await;

    let reports = sync_folders(
        &mut session,
        &fx.db,
        fx.account_id,
        SyncOptions::default(),
        &CancellationToken::new(),
        |_| {},
    )
    .await
    .unwrap();

    assert!(reports.failures.is_empty());
    let order: Vec<String> = reports
        .reports
        .iter()
        .map(|r| {
            let id = r.mailbox_id;
            fx.db
                .with_read(move |c| repo::get_mailbox(c, id))
                .unwrap()
                .unwrap()
                .name
        })
        .collect();
    assert_eq!(order, vec!["INBOX", "Archive", "Zebra"]);

    let _ = session.logout().await;
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

#[test]
fn uid_sets_collapse_into_ranges() {
    assert_eq!(format_uid_set(&[]), "");
    assert_eq!(format_uid_set(&[7]), "7");
    assert_eq!(format_uid_set(&[1, 2, 3]), "1:3");
    assert_eq!(format_uid_set(&[1, 2, 3, 7]), "1:3,7");
    assert_eq!(format_uid_set(&[1, 3, 5]), "1,3,5");
    assert_eq!(format_uid_set(&[1, 2, 5, 6, 7, 20]), "1:2,5:7,20");
}

#[test]
fn prioritize_orders_well_known_folders_and_drops_unselectable() {
    let mailbox = |id: i64, name: &str, attributes: Option<&str>| repo::Mailbox {
        id,
        account_id: 1,
        name: name.to_owned(),
        uidvalidity: None,
        uidnext: None,
        highestmodseq: None,
        attributes: attributes.map(str::to_owned),
        created_at: 0,
        updated_at: 0,
    };

    let ordered = prioritize(vec![
        mailbox(1, "Work/Projects", None),
        mailbox(2, "[Gmail]", Some("\\Noselect \\HasChildren")),
        mailbox(3, "INBOX", None),
        mailbox(4, "[Gmail]/Sent Mail", None),
        mailbox(5, "Archive", None),
    ]);

    let names: Vec<&str> = ordered.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(
        names,
        ["INBOX", "Archive", "[Gmail]/Sent Mail", "Work/Projects"],
        "INBOX first, then well-known leaves, then the rest alphabetically; \
         \\Noselect dropped"
    );
}

// ---------------------------------------------------------------------------
// The failure modes a data-derived walk cannot see
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_sparse_uid_space_still_converges_to_a_true_no_op() {
    // Every expunge leaves a permanent hole, so "no row for UID 7" cannot mean
    // "not fetched yet". Derived from the stored UIDs alone, a folder like this
    // would re-request its whole UID space on every sync, forever.
    let fx = Fixture::open().await;
    let mut config = MockConfig::default()
        .password("pw")
        .uidvalidity(u32::try_from(UIDVALIDITY).unwrap());
    for uid in [3u32, 10, 30] {
        config = config.fetch(uid, &["\\Seen"], &raw(uid));
    }
    let mock = MockImap::start(config).await;
    let mut session = connect(&mock).await;
    let opts = SyncOptions {
        window: 5,
        ..Default::default()
    };

    let first = sync_folder(
        &mut session,
        &fx.db,
        fx.mailbox_id,
        opts,
        &CancellationToken::new(),
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(first.fetched, 3);
    assert!(first.complete);
    let after_first = mock.fetch_commands().len();
    assert!(after_first > 0);

    let second = sync_folder(
        &mut session,
        &fx.db,
        fx.mailbox_id,
        opts,
        &CancellationToken::new(),
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(second.windows_fetched, 0);
    assert_eq!(
        mock.fetch_commands().len(),
        after_first,
        "the 27 holes in this UID space must not be re-requested"
    );

    let _ = session.logout().await;
}

#[tokio::test]
async fn a_uidvalidity_bump_replaces_the_folder_instead_of_duplicating_it() {
    let fx = Fixture::open().await;
    // First sync in the old UID space.
    let old = MockImap::start(mock_config(3).uidvalidity(7).password("pw")).await;
    let mut session = connect(&old).await;
    sync_folder(
        &mut session,
        &fx.db,
        fx.mailbox_id,
        SyncOptions::default(),
        &CancellationToken::new(),
        |_| {},
    )
    .await
    .unwrap();
    assert_eq!(fx.message_count(), 3);
    let _ = session.logout().await;

    // The server re-keys the UID space.
    let new = MockImap::start(mock_config(3)).await;
    let mut session = connect(&new).await;
    let report = sync_folder(
        &mut session,
        &fx.db,
        fx.mailbox_id,
        SyncOptions::default(),
        &CancellationToken::new(),
        |_| {},
    )
    .await
    .unwrap();

    assert!(report.uidvalidity_changed);
    assert_eq!(report.purged_stale, 3, "the stale UID space was dropped");
    assert_eq!(report.fetched, 3);
    assert_eq!(
        fx.message_count(),
        3,
        "the mailbox is replaced, not shown twice"
    );
    let threads: i64 = fx
        .db
        .with_read(|c| c.query_row("SELECT count(*) FROM threads", [], |r| r.get(0)))
        .unwrap();
    assert_eq!(
        threads, 3,
        "threads left by the purged copies are collected"
    );

    let _ = session.logout().await;
}

#[tokio::test]
async fn cancellation_stops_the_walk_at_a_window_boundary() {
    let fx = Fixture::open().await;
    let mock = MockImap::start(mock_config(60)).await;
    let mut session = connect(&mock).await;
    let cancel = CancellationToken::new();

    let report = sync_folder(
        &mut session,
        &fx.db,
        fx.mailbox_id,
        SyncOptions {
            window: 5,
            ..Default::default()
        },
        &cancel,
        |_| cancel.cancel(),
    )
    .await
    .unwrap();

    assert!(report.cancelled);
    assert!(!report.complete, "a cancelled walk is not complete");
    assert_eq!(report.fetched, 5, "it stopped after the window in flight");
    assert!(
        !fx.sync_state().full_sync_done,
        "and the checkpoint says so, so the next run resumes"
    );

    let _ = session.logout().await;
}

#[tokio::test]
async fn an_unselectable_folder_is_not_found_not_unauthenticated() {
    // A tagged NO on SELECT means the folder is gone — telling the client its
    // credentials are bad would send it chasing the wrong problem.
    let fx = Fixture::open().await;
    let mock = MockImap::start(mock_config(1).unselectable("INBOX")).await;
    let mut session = connect(&mock).await;

    let err = sync_folder(
        &mut session,
        &fx.db,
        fx.mailbox_id,
        SyncOptions::default(),
        &CancellationToken::new(),
        |_| {},
    )
    .await
    .unwrap_err();
    assert_eq!(tonic::Status::from(err).code(), tonic::Code::NotFound);
}

#[tokio::test]
async fn a_server_without_uid_response_codes_is_unavailable() {
    for config in [
        mock_config(2).without_uidvalidity(),
        mock_config(2).without_uidnext(),
    ] {
        let fx = Fixture::open().await;
        let mock = MockImap::start(config).await;
        let mut session = connect(&mock).await;
        let err = sync_folder(
            &mut session,
            &fx.db,
            fx.mailbox_id,
            SyncOptions::default(),
            &CancellationToken::new(),
            |_| {},
        )
        .await
        .unwrap_err();
        assert_eq!(
            tonic::Status::from(err).code(),
            tonic::Code::Unavailable,
            "a UID-window sync needs both response codes"
        );
    }
}

#[tokio::test]
async fn one_broken_folder_does_not_stop_the_others() {
    let fx = Fixture::open_with_folders(&["INBOX", "Broken", "Archive"]).await;
    let mock = MockImap::start(
        MockConfig::default()
            .password("pw")
            .uidvalidity(u32::try_from(UIDVALIDITY).unwrap())
            .fetch_in("INBOX", 1, &["\\Seen"], &raw(1))
            .fetch_in("Archive", 1, &["\\Seen"], &raw(2))
            .unselectable("Broken"),
    )
    .await;
    let mut session = connect(&mock).await;

    let out = sync_folders(
        &mut session,
        &fx.db,
        fx.account_id,
        SyncOptions::default(),
        &CancellationToken::new(),
        |_| {},
    )
    .await
    .unwrap();

    assert_eq!(out.reports.len(), 2, "INBOX and Archive still synced");
    assert_eq!(out.failures.len(), 1);
    assert_eq!(out.failures[0].name, "Broken");
    assert_eq!(fx.message_count(), 2);

    let _ = session.logout().await;
}

#[tokio::test]
async fn each_folder_syncs_its_own_messages() {
    let fx = Fixture::open_with_folders(&["INBOX", "Archive"]).await;
    let mock = MockImap::start(
        MockConfig::default()
            .password("pw")
            .uidvalidity(u32::try_from(UIDVALIDITY).unwrap())
            .fetch_in("INBOX", 1, &["\\Seen"], &raw(1))
            .fetch_in("INBOX", 2, &["\\Seen"], &raw(2))
            .fetch_in("Archive", 1, &["\\Seen"], &raw(9)),
    )
    .await;
    let mut session = connect(&mock).await;

    sync_folders(
        &mut session,
        &fx.db,
        fx.account_id,
        SyncOptions::default(),
        &CancellationToken::new(),
        |_| {},
    )
    .await
    .unwrap();

    let subjects: Vec<(String, String)> = fx
        .db
        .with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT m.name, x.subject FROM messages x
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
        subjects,
        vec![
            ("Archive".to_owned(), "Message 9".to_owned()),
            ("INBOX".to_owned(), "Message 1".to_owned()),
            ("INBOX".to_owned(), "Message 2".to_owned()),
        ],
        "each folder got its own messages, not the same set twice"
    );

    let _ = session.logout().await;
}

#[test]
fn the_window_size_is_clamped_to_a_range_servers_accept() {
    assert_eq!(
        SyncOptions {
            window: 0,
            ..Default::default()
        }
        .effective_window(),
        1
    );
    assert_eq!(
        SyncOptions {
            window: u32::MAX,
            ..Default::default()
        }
        .effective_window(),
        i64::from(MAX_WINDOW)
    );
    assert_eq!(
        SyncOptions::default().effective_window(),
        i64::from(DEFAULT_WINDOW)
    );
}
