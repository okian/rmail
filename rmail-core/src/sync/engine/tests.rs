//! What the orchestrator owns that the engines below it do not: pause, the
//! translation of changes into durable events, and per-folder status.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use tokio_util::sync::CancellationToken;

use super::*;
use crate::events::{EventKind, Retention};
use crate::ErrorReason;

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Fixture {
    engine: SyncEngine,
    account_id: i64,
    mailbox_id: i64,
    path: PathBuf,
}

impl Fixture {
    async fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-engine-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", path.display())));
        }
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
        let events = EventLog::new(db.clone(), Retention::unlimited());
        let engine = SyncEngine::new(db, events, SyncOptions::default());
        Self {
            engine,
            account_id,
            mailbox_id,
            path,
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
async fn a_paused_account_refuses_to_start_a_pass() {
    // Pause has to refuse *before* connecting. Discovering the pause after the
    // TLS handshake and LOGIN would make every paused account still hammer its
    // server on schedule.
    let fx = Fixture::open().await;
    assert!(!fx.engine.is_paused(fx.account_id));

    fx.engine.pause(fx.account_id);
    assert!(fx.engine.is_paused(fx.account_id));

    let err = fx
        .engine
        .sync(
            fx.account_id,
            None,
            SyncMode::Auto,
            &CancellationToken::new(),
        )
        .await
        .expect_err("a paused account does not sync");
    assert_eq!(err.reason(), ErrorReason::FailedPrecondition);

    fx.engine.resume(fx.account_id);
    assert!(!fx.engine.is_paused(fx.account_id));
    // And now it gets as far as trying to connect, which fails for a different
    // reason — the fixture account has no server configured.
    let err = fx
        .engine
        .sync(
            fx.account_id,
            None,
            SyncMode::Auto,
            &CancellationToken::new(),
        )
        .await
        .expect_err("no IMAP server is configured");
    assert_eq!(err.reason(), ErrorReason::FailedPrecondition);
    assert!(
        err.to_string().contains("IMAP server"),
        "it failed on configuration, not on the pause: {err}"
    );
}

#[tokio::test]
async fn pause_cancels_a_pass_already_running() {
    // A pause that only refused *new* work would leave a long initial sync
    // running for hours after the user asked it to stop.
    let fx = Fixture::open().await;
    let token = CancellationToken::new();
    fx.engine.begin(fx.account_id, &token).unwrap();
    assert!(!token.is_cancelled());

    fx.engine.pause(fx.account_id);

    assert!(
        token.is_cancelled(),
        "the in-flight pass was told to stop at its next safe boundary"
    );
}

#[tokio::test]
async fn changes_become_durable_events_with_their_scope() {
    // The engines report changes; this is where they become something a
    // downstream indexer or AI queue can consume.
    let fx = Fixture::open().await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut sink = LogSink::new(fx.account_id, fx.mailbox_id, tx);

    sink.changed(Change::Added {
        message_id: 11,
        uid: 4,
    });
    sink.changed(Change::FlagsChanged {
        message_id: 12,
        uid: 5,
        flags: vec!["\\Seen".to_owned()],
    });
    sink.changed(Change::Removed {
        message_id: 13,
        uid: 6,
    });
    drop(sink);
    let mut pending = Vec::new();
    while let Some(event) = rx.recv().await {
        pending.push(event);
    }
    fx.engine.events().append_all(pending).await.unwrap();

    let events = fx.engine.events().since(0, 100).await.unwrap().events;
    assert_eq!(events.len(), 3);
    assert_eq!(
        events.iter().map(|e| e.kind).collect::<Vec<_>>(),
        vec![
            EventKind::NewMail,
            EventKind::FlagChanged,
            EventKind::Deleted
        ]
    );
    for event in &events {
        assert_eq!(event.account_id, Some(fx.account_id));
        assert_eq!(event.mailbox_id, Some(fx.mailbox_id));
    }
    assert_eq!(events[0].message_id, Some(11));
    assert_eq!(events[0].payload, serde_json::json!({ "uid": 4 }));
    assert_eq!(
        events[1].payload,
        serde_json::json!({ "uid": 5, "flags": ["\\Seen"] })
    );
    assert_eq!(
        events[2].message_id,
        Some(13),
        "a removal names the message it removed — a consumer that indexed it \
         needs to know which one to drop"
    );
}

#[tokio::test]
async fn a_pass_that_changed_nothing_writes_no_change_events() {
    // An event log that gained a row per change-less check would bury real
    // activity under the heartbeat of every quiet folder.
    let fx = Fixture::open().await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let sink = LogSink::new(fx.account_id, fx.mailbox_id, tx);
    drop(sink);
    assert!(rx.recv().await.is_none());
    assert!(fx.engine.events().is_empty().await.unwrap());
}

#[tokio::test]
async fn status_reports_every_folder_including_never_synced_ones() {
    // A folder with no checkpoint is not an error; it is the normal state of a
    // freshly discovered one, and a UI has to be able to show it.
    let fx = Fixture::open().await;

    let status = fx.engine.status(fx.account_id).await.unwrap();

    assert_eq!(status.len(), 1);
    let inbox = &status[0];
    assert_eq!(inbox.mailbox_id, fx.mailbox_id);
    assert_eq!(inbox.name, "INBOX");
    assert_eq!(inbox.highestmodseq, None);
    assert!(!inbox.full_sync_done);
    assert_eq!(inbox.message_count, 0);
}

#[tokio::test]
async fn status_reflects_the_stored_checkpoint() {
    let fx = Fixture::open().await;
    let mailbox_id = fx.mailbox_id;
    fx.engine
        .db
        .write(move |c| {
            repo::update_mailbox_uid_state(c, mailbox_id, 42, 101)?;
            repo::upsert_sync_state(
                c,
                &repo::SyncState {
                    mailbox_id,
                    uidvalidity: Some(42),
                    highestmodseq: Some(9),
                    last_synced_uid: Some(100),
                    walked_down_to: Some(1),
                    last_sync_at: Some(1_700_000_000),
                    full_sync_done: true,
                },
            )
        })
        .await
        .unwrap();

    let status = fx.engine.status(fx.account_id).await.unwrap();
    let inbox = &status[0];

    assert_eq!(inbox.uidvalidity, Some(42));
    assert_eq!(inbox.uidnext, Some(101));
    assert_eq!(inbox.highestmodseq, Some(9));
    assert_eq!(inbox.last_synced_uid, Some(100));
    assert_eq!(inbox.walked_down_to, Some(1));
    assert!(inbox.full_sync_done);
    assert_eq!(inbox.last_sync_at, Some(1_700_000_000));
}

#[tokio::test]
async fn syncing_an_account_that_does_not_exist_is_not_found() {
    let fx = Fixture::open().await;
    let err = fx
        .engine
        .sync(9_999, None, SyncMode::Auto, &CancellationToken::new())
        .await
        .expect_err("no such account");
    assert_eq!(err.reason(), ErrorReason::NotFound);
}

#[tokio::test]
async fn pause_and_resume_are_per_account() {
    let fx = Fixture::open().await;
    let other = fx
        .engine
        .db
        .write(|c| {
            repo::insert_account(
                c,
                &repo::NewAccount {
                    name: "Work".to_owned(),
                    ..Default::default()
                },
            )
        })
        .await
        .unwrap();

    fx.engine.pause(fx.account_id);

    assert!(fx.engine.is_paused(fx.account_id));
    assert!(
        !fx.engine.is_paused(other),
        "pausing one account must not stop the others"
    );
}
