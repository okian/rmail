//! Integration test: drive `SyncService` end-to-end against an in-process tonic
//! server over a Unix domain socket.
//!
//! The interesting surface is `WatchEvents`. Its contract is that a client
//! resuming from a cursor sees everything it missed and everything that happens
//! after, with nothing falling between the two and nothing delivered twice —
//! and that a client going away stops the work behind it. None of that is
//! observable from unit tests of the log alone, because the seam being tested
//! is the handler's ordering of subscribe-then-replay.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use rmail_core::events::{EventKind as CoreKind, EventLog, NewEvent, Retention};
use rmail_proto::v1::sync_service_client::SyncServiceClient;
use rmail_proto::v1::{
    EventKind, PauseRequest, ResumeRequest, SyncFolderRequest, SyncMode, SyncStatusRequest,
    WatchEventsRequest,
};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;
use tonic::transport::Channel;
use tonic::Code;

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// How long a stream assertion waits before failing. Generous because these are
/// liveness checks on spawned tasks, not latency measurements.
const STREAM_TIMEOUT: Duration = Duration::from_secs(30);

struct TestServer {
    socket: PathBuf,
    db_path: PathBuf,
    db: rmail_core::Database,
    /// The *server's* log. A second `EventLog` over the same database shares
    /// the durable rows but not the in-process channel, so a test appending
    /// through one of those would drive the backlog and never the live tail.
    log: EventLog,
    shutdown: oneshot::Sender<()>,
    handle: JoinHandle<Result<(), rmaild::ServeError>>,
}

impl TestServer {
    async fn start() -> Self {
        Self::with_retention(Retention::unlimited()).await
    }

    async fn with_retention(retention: Retention) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let socket = PathBuf::from("/tmp").join(format!("rmail-sync-{pid}-{n}.sock"));
        let db_path = std::env::temp_dir().join(format!("rmail-sync-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", db_path.display())));
        }
        let db = rmail_core::Database::open(&db_path).unwrap();
        let log = EventLog::new(db.clone(), retention);
        let engine = rmail_core::sync::SyncEngine::new(
            db.clone(),
            log.clone(),
            rmail_core::sync::SyncOptions::default(),
        );

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let server_socket = socket.clone();
        let server_db = db.clone();
        let handle = tokio::spawn(async move {
            rmaild::serve_uds_with_engine(&server_socket, server_db, engine, async move {
                let _ = shutdown_rx.await;
            })
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
            log,
            shutdown: shutdown_tx,
            handle,
        }
    }

    async fn client(&self) -> SyncServiceClient<Channel> {
        SyncServiceClient::new(rmail_core::connect_uds(&self.socket).await.unwrap())
    }

    /// An account with one mailbox, as folder discovery would have left it.
    async fn account(&self, name: &str) -> (i64, i64) {
        let name = name.to_owned();
        self.db
            .write(move |c| {
                let account_id = rmail_core::repo::insert_account(
                    c,
                    &rmail_core::repo::NewAccount {
                        name,
                        ..Default::default()
                    },
                )?;
                let mailbox_id = rmail_core::repo::insert_mailbox(
                    c,
                    &rmail_core::repo::NewMailbox {
                        account_id,
                        name: "INBOX".to_owned(),
                        ..Default::default()
                    },
                )?;
                Ok((account_id, mailbox_id))
            })
            .await
            .unwrap()
    }

    /// The server's own log, so an appended event reaches an open stream.
    fn log(&self) -> EventLog {
        self.log.clone()
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

/// Take the next event from a stream, failing rather than hanging.
async fn next_event<S>(stream: &mut S) -> rmail_proto::v1::Event
where
    S: tokio_stream::Stream<Item = Result<rmail_proto::v1::Event, tonic::Status>> + Unpin,
{
    tokio::time::timeout(STREAM_TIMEOUT, stream.next())
        .await
        .expect("the stream should have produced an event")
        .expect("the stream ended early")
        .expect("the stream returned an error")
}

#[tokio::test]
async fn status_lists_every_folder_and_reports_the_pause_flag() {
    let server = TestServer::start().await;
    let (account_id, mailbox_id) = server.account("Personal").await;
    let mut client = server.client().await;

    let status = client
        .status(SyncStatusRequest { account_id })
        .await
        .unwrap()
        .into_inner();

    assert_eq!(status.folders.len(), 1);
    assert_eq!(status.folders[0].mailbox_id, mailbox_id);
    assert_eq!(status.folders[0].name, "INBOX");
    assert!(
        !status.folders[0].full_sync_done,
        "a freshly discovered folder has no checkpoint, which is not an error"
    );
    assert!(!status.paused);

    server.stop().await;
}

#[tokio::test]
async fn pause_and_resume_round_trip_through_status() {
    let server = TestServer::start().await;
    let (account_id, _) = server.account("Personal").await;
    let mut client = server.client().await;

    assert!(
        client
            .pause(PauseRequest { account_id })
            .await
            .unwrap()
            .into_inner()
            .paused
    );
    assert!(
        client
            .status(SyncStatusRequest { account_id })
            .await
            .unwrap()
            .into_inner()
            .paused,
        "the pause is visible to any client, not just the one that set it"
    );

    assert!(
        !client
            .resume(ResumeRequest { account_id })
            .await
            .unwrap()
            .into_inner()
            .paused
    );
    assert!(
        !client
            .status(SyncStatusRequest { account_id })
            .await
            .unwrap()
            .into_inner()
            .paused
    );

    server.stop().await;
}

#[tokio::test]
async fn syncing_a_paused_account_is_failed_precondition() {
    // And it must fail *before* connecting: a paused account that still opened
    // a TLS session and logged in on every scheduled pass is not paused in any
    // sense the server would recognise.
    let server = TestServer::start().await;
    let (account_id, _) = server.account("Personal").await;
    let mut client = server.client().await;
    client.pause(PauseRequest { account_id }).await.unwrap();

    let status = client
        .sync_folder(SyncFolderRequest {
            account_id,
            mailbox_id: None,
            mode: SyncMode::Auto as i32,
        })
        .await
        .expect_err("a paused account does not sync");

    assert_eq!(status.code(), Code::FailedPrecondition);
    assert!(
        status.message().contains("paused"),
        "message: {}",
        status.message()
    );

    server.stop().await;
}

#[tokio::test]
async fn syncing_an_unknown_account_is_not_found() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let status = client
        .sync_folder(SyncFolderRequest {
            account_id: 9_999,
            mailbox_id: None,
            mode: SyncMode::Auto as i32,
        })
        .await
        .expect_err("no such account");

    assert_eq!(status.code(), Code::NotFound);
    server.stop().await;
}

#[tokio::test]
async fn syncing_an_account_with_no_server_is_failed_precondition() {
    let server = TestServer::start().await;
    let (account_id, _) = server.account("Personal").await;
    let mut client = server.client().await;

    let status = client
        .sync_folder(SyncFolderRequest {
            account_id,
            mailbox_id: None,
            mode: SyncMode::Auto as i32,
        })
        .await
        .expect_err("the account has no IMAP server configured");

    assert_eq!(status.code(), Code::FailedPrecondition);
    server.stop().await;
}

#[tokio::test]
async fn watch_replays_the_backlog_then_follows_the_tail() {
    // The seam the whole design turns on: a client sees what it missed and what
    // happens next, in order, with no gap between them.
    let server = TestServer::start().await;
    let (account_id, mailbox_id) = server.account("Personal").await;
    let log = server.log();

    log.append_all(vec![
        NewEvent::new(CoreKind::NewMail)
            .account(account_id)
            .mailbox(mailbox_id)
            .message(1),
        NewEvent::new(CoreKind::NewMail)
            .account(account_id)
            .mailbox(mailbox_id)
            .message(2),
    ])
    .await
    .unwrap();

    let mut client = server.client().await;
    let mut stream = client
        .watch_events(WatchEventsRequest {
            account_id,
            since_seq: 0,
            kinds: Vec::new(),
        })
        .await
        .unwrap()
        .into_inner();

    // The backlog, oldest first.
    assert_eq!(next_event(&mut stream).await.seq, 1);
    assert_eq!(next_event(&mut stream).await.seq, 2);

    // Then the tail.
    log.append(
        NewEvent::new(CoreKind::FlagChanged)
            .account(account_id)
            .mailbox(mailbox_id)
            .message(2),
    )
    .await
    .unwrap();
    let live = next_event(&mut stream).await;
    assert_eq!(live.seq, 3);
    assert_eq!(live.kind(), EventKind::FlagChanged);
    assert_eq!(live.message_id, Some(2));

    server.stop().await;
}

#[tokio::test]
async fn watch_resumes_after_a_cursor_without_replaying_what_was_seen() {
    let server = TestServer::start().await;
    let (account_id, mailbox_id) = server.account("Personal").await;
    let log = server.log();
    for i in 1..=5 {
        log.append(
            NewEvent::new(CoreKind::NewMail)
                .account(account_id)
                .mailbox(mailbox_id)
                .message(i),
        )
        .await
        .unwrap();
    }

    let mut client = server.client().await;
    let mut stream = client
        .watch_events(WatchEventsRequest {
            account_id,
            since_seq: 3,
            kinds: Vec::new(),
        })
        .await
        .unwrap()
        .into_inner();

    assert_eq!(
        next_event(&mut stream).await.seq,
        4,
        "strictly after the cursor, so the client never re-processes one"
    );
    assert_eq!(next_event(&mut stream).await.seq, 5);

    server.stop().await;
}

#[tokio::test]
async fn watch_rejects_a_cursor_past_retention_and_says_where_to_resume() {
    // Answering "no events" here would be indistinguishable from a quiet
    // mailbox, and the client would believe itself current forever.
    let server = TestServer::with_retention(Retention {
        max_rows: Some(3),
        max_age: None,
    })
    .await;
    let (account_id, mailbox_id) = server.account("Personal").await;
    let log = server.log();
    for i in 1..=10 {
        log.append(
            NewEvent::new(CoreKind::NewMail)
                .account(account_id)
                .mailbox(mailbox_id)
                .message(i),
        )
        .await
        .unwrap();
    }
    log.prune().await.unwrap();

    let mut client = server.client().await;
    let status = client
        .watch_events(WatchEventsRequest {
            account_id,
            since_seq: 1,
            kinds: Vec::new(),
        })
        .await
        .expect_err("cursor 1 was pruned away");

    assert_eq!(status.code(), Code::OutOfRange);
    let details = tonic_types::StatusExt::get_error_details(&status);
    let info = details.error_info().expect("ErrorInfo attached");
    assert!(
        info.metadata
            .contains_key(rmail_core::error::OLDEST_SEQ_KEY),
        "the client is told how far back the log goes: {:?}",
        info.metadata
    );
    assert!(
        info.metadata
            .contains_key(rmail_core::error::RESUME_FROM_KEY),
        "and the exact cursor to resume with, which differs by one: {:?}",
        info.metadata
    );

    server.stop().await;
}

#[tokio::test]
async fn watch_filters_by_account_and_kind() {
    let server = TestServer::start().await;
    let (mine, my_inbox) = server.account("Personal").await;
    let (theirs, _) = server.account("Work").await;
    let log = server.log();

    log.append_all(vec![
        NewEvent::new(CoreKind::NewMail).account(theirs).message(1),
        NewEvent::new(CoreKind::SyncState)
            .account(mine)
            .mailbox(my_inbox),
        NewEvent::new(CoreKind::NewMail).account(mine).message(2),
    ])
    .await
    .unwrap();

    let mut client = server.client().await;
    let mut stream = client
        .watch_events(WatchEventsRequest {
            account_id: mine,
            since_seq: 0,
            kinds: vec![EventKind::NewMail as i32],
        })
        .await
        .unwrap()
        .into_inner();

    let event = next_event(&mut stream).await;
    assert_eq!(
        event.seq, 3,
        "the other account's mail and my sync-state event were both filtered out"
    );
    assert_eq!(event.account_id, Some(mine));
    assert_eq!(event.kind(), EventKind::NewMail);

    server.stop().await;
}

#[tokio::test]
async fn an_empty_kind_filter_means_every_kind() {
    // An unset repeated field is how a client says "no preference", not "none".
    let server = TestServer::start().await;
    let (account_id, _) = server.account("Personal").await;
    let log = server.log();
    log.append(NewEvent::new(CoreKind::SyncState).account(account_id))
        .await
        .unwrap();

    let mut client = server.client().await;
    let mut stream = client
        .watch_events(WatchEventsRequest {
            account_id,
            since_seq: 0,
            kinds: Vec::new(),
        })
        .await
        .unwrap()
        .into_inner();

    assert_eq!(next_event(&mut stream).await.kind(), EventKind::SyncState);
    server.stop().await;
}

#[tokio::test]
async fn dropping_the_stream_stops_the_work_behind_it() {
    // A disconnected client must stop the producer, not merely stop being read.
    // Otherwise every abandoned watch leaves a task holding a broadcast
    // subscription for the life of the process.
    let server = TestServer::start().await;
    let (account_id, _) = server.account("Personal").await;
    let log = server.log();
    log.append(NewEvent::new(CoreKind::NewMail).account(account_id))
        .await
        .unwrap();

    let mut client = server.client().await;
    let mut stream = client
        .watch_events(WatchEventsRequest {
            account_id,
            since_seq: 0,
            kinds: Vec::new(),
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(next_event(&mut stream).await.seq, 1);
    assert_eq!(log.subscriber_count(), 1, "the stream holds a subscription");

    drop(stream);
    drop(client);

    // The producer notices on its next send, so it takes an event to shake it
    // loose — exactly what would happen in production.
    let mut released = false;
    for _ in 0..300 {
        log.append(NewEvent::new(CoreKind::NewMail).account(account_id))
            .await
            .unwrap();
        if log.subscriber_count() == 0 {
            released = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        released,
        "the producer task outlived the client that asked for it"
    );

    server.stop().await;
}

#[tokio::test]
async fn a_shutdown_closes_open_streams_rather_than_holding_them() {
    // A daemon that waits for every watcher to disconnect never shuts down.
    let server = TestServer::start().await;
    let (account_id, _) = server.account("Personal").await;
    let mut client = server.client().await;
    let mut stream = client
        .watch_events(WatchEventsRequest {
            account_id,
            since_seq: 0,
            kinds: Vec::new(),
        })
        .await
        .unwrap()
        .into_inner();

    // Nothing to read yet; the stream is parked on the live tail.
    let shutdown = tokio::time::timeout(Duration::from_secs(30), server.stop());
    assert!(
        shutdown.await.is_ok(),
        "shutdown must not wait on an open event stream"
    );

    // And the stream ends rather than hanging.
    let ended = tokio::time::timeout(STREAM_TIMEOUT, stream.next()).await;
    assert!(ended.is_ok(), "the stream should end when the server does");
}
