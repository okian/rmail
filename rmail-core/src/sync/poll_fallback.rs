//! The degradation path: watching a folder on a server that has no `IDLE`.
//!
//! Plenty of IMAP servers — and plenty of corporate proxies in front of ones
//! that do — will not hold a connection open. The watch still has to work
//! there, at worse latency and without anything above it needing to know which
//! mode it got. These tests pin exactly that: same loop, same delta pass, same
//! observations, no `IDLE` on the wire.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::imap::mock::{MockConfig, MockImap};
use crate::imap::ImapCapabilities;
use crate::sync::{watch_folder, IdleOptions, WatchOutcome, WatchTrigger};

use super::harness::{fast_watch, mock_config, raw, until, with_baseline, Cycles, Fixture};

/// A server that cannot hold a connection open.
fn no_idle(config: MockConfig) -> MockConfig {
    config.capabilities(&["IMAP4rev1", "CONDSTORE", "QRESYNC"])
}

/// Capabilities as probed from a server without `IDLE`.
fn polling_capabilities() -> ImapCapabilities {
    ImapCapabilities {
        idle: false,
        condstore: true,
        qresync: true,
        move_: false,
    }
}

/// Start a watch against `mock`, returning its join handle.
fn spawn_watch(
    fx: &Fixture,
    mock: &MockImap,
    capabilities: ImapCapabilities,
    opts: IdleOptions,
    cancel: &CancellationToken,
    cycles: &Cycles,
) -> tokio::task::JoinHandle<Result<crate::sync::WatchReport, crate::Error>> {
    let db = fx.db.clone();
    let mailbox_id = fx.mailbox_id;
    let cancel = cancel.clone();
    let sink = cycles.sink();
    let addr = mock.addr;
    tokio::spawn(async move {
        watch_folder(
            &db,
            mailbox_id,
            capabilities,
            opts,
            &cancel,
            || async move {
                let stream = tokio::net::TcpStream::connect(addr)
                    .await
                    .map_err(|e| crate::Error::unavailable(format!("test connect failed: {e}")))?;
                crate::imap::conn::login(stream, "user", "pw").await
            },
            sink,
            &mut (),
        )
        .await
    })
}

#[tokio::test]
async fn a_server_without_idle_polls_instead_and_never_sends_idle() {
    // Sending IDLE to a server that never advertised it earns a tagged BAD and
    // leaves the connection in a state nobody planned for.
    let fx = Fixture::open().await;
    let mock = MockImap::start(no_idle(mock_config(2))).await;
    with_baseline(&fx, &mock, Some(1)).await;

    let cycles = Cycles::default();
    let cancel = CancellationToken::new();
    let watch = spawn_watch(
        &fx,
        &mock,
        polling_capabilities(),
        fast_watch(),
        &cancel,
        &cycles,
    );

    until("two poll ticks", || {
        cycles
            .triggers()
            .iter()
            .filter(|t| **t == WatchTrigger::Polled)
            .count()
            >= 2
    })
    .await;
    cancel.cancel();
    let report = watch.await.unwrap().unwrap();

    assert_eq!(report.outcome, WatchOutcome::Cancelled);
    assert!(!report.used_idle, "this server cannot park");
    assert_eq!(report.sync_failures, 0);
    assert!(
        !mock
            .commands()
            .iter()
            .any(|c| c.eq_ignore_ascii_case("IDLE")),
        "IDLE must not be sent to a server that does not advertise it: {:?}",
        mock.commands()
    );
    assert!(
        cycles.all().iter().all(|c| !c.pushing),
        "and every cycle says so"
    );
}

#[tokio::test]
async fn polling_still_delivers_new_mail() {
    // Worse latency is the only thing that should differ. The mail still lands.
    let fx = Fixture::open().await;
    let seed = MockImap::start(no_idle(mock_config(2))).await;
    with_baseline(&fx, &seed, Some(1)).await;
    assert_eq!(fx.stored_uids(), vec![1, 2]);
    drop(seed);

    let grown = MockImap::start(no_idle(mock_config(2).fetch_at(
        "INBOX",
        3,
        &["\\Recent"],
        &raw(3),
        9,
    )))
    .await;
    let cycles = Cycles::default();
    let cancel = CancellationToken::new();
    let watch = spawn_watch(
        &fx,
        &grown,
        polling_capabilities(),
        fast_watch(),
        &cancel,
        &cycles,
    );

    until("the new message to land", || {
        fx.stored_uids() == vec![1, 2, 3]
    })
    .await;
    cancel.cancel();
    let report = watch.await.unwrap().unwrap();

    assert_eq!(report.sync_failures, 0);
    let downloaded: u64 = cycles
        .all()
        .iter()
        .filter_map(|c| c.report.as_ref())
        .map(|r| r.new_messages)
        .sum();
    assert_eq!(downloaded, 1);
}

#[tokio::test]
async fn a_poll_watch_stops_promptly_when_cancelled() {
    // The tick is a sleep, and a sleep that is not raced against the
    // cancellation token turns every shutdown into a wait for the interval.
    let fx = Fixture::open().await;
    let mock = MockImap::start(no_idle(mock_config(1))).await;
    with_baseline(&fx, &mock, Some(1)).await;

    let cycles = Cycles::default();
    let cancel = CancellationToken::new();
    let opts = IdleOptions {
        poll_interval: Duration::from_secs(600),
        ..fast_watch()
    };
    let mut watch = spawn_watch(&fx, &mock, polling_capabilities(), opts, &cancel, &cycles);

    until("the first pass", || cycles.len() >= 1).await;
    cancel.cancel();

    let joined = tokio::time::timeout(Duration::from_secs(5), &mut watch).await;
    if joined.is_err() {
        watch.abort();
    }
    let report = joined
        .expect("a cancelled poll watch must not wait out its interval")
        .unwrap()
        .unwrap();
    assert_eq!(report.outcome, WatchOutcome::Cancelled);
}

#[tokio::test]
async fn a_transient_sync_failure_is_retried_rather_than_ending_the_watch() {
    // A server mid-restart that answers SELECT without the response codes a
    // sync needs is broken *now*, not forever. Ending the watch on it would
    // mean a folder stops syncing because its server hiccuped once.
    let fx = Fixture::open().await;
    let mock = MockImap::start(no_idle(mock_config(1)).without_uidvalidity()).await;

    let cycles = Cycles::default();
    let cancel = CancellationToken::new();
    let watch = spawn_watch(
        &fx,
        &mock,
        polling_capabilities(),
        fast_watch(),
        &cancel,
        &cycles,
    );

    until("several failed passes", || cycles.len() >= 4).await;
    cancel.cancel();
    let report = watch.await.unwrap().unwrap();

    assert_eq!(
        report.outcome,
        WatchOutcome::Cancelled,
        "the watch kept going until it was told to stop"
    );
    assert!(report.sync_failures >= 4);
    assert_eq!(
        report.permanent_failures, 0,
        "an Unavailable is transient however many times it repeats"
    );
    assert!(
        cycles.all().iter().all(|c| c.report.is_none()),
        "and each cycle reported the failure rather than a fabricated result"
    );
}

#[tokio::test]
async fn a_connection_that_fails_intermittently_backs_off_and_recovers() {
    // The reconnect path is the same for polling as for IDLE, and its only real
    // requirement is that a transient failure does not become a permanent one.
    let fx = Fixture::open().await;
    let mock = MockImap::start(no_idle(mock_config(2))).await;
    with_baseline(&fx, &mock, Some(1)).await;

    let attempts = Arc::new(AtomicU32::new(0));
    let cycles = Cycles::default();
    let cancel = CancellationToken::new();
    let watch = tokio::spawn({
        let (db, cancel, sink) = (fx.db.clone(), cancel.clone(), cycles.sink());
        let attempts = Arc::clone(&attempts);
        let mailbox_id = fx.mailbox_id;
        let addr = mock.addr;
        async move {
            watch_folder(
                &db,
                mailbox_id,
                polling_capabilities(),
                fast_watch(),
                &cancel,
                move || {
                    let attempts = Arc::clone(&attempts);
                    async move {
                        // The first three attempts fail, as a server coming back
                        // up would.
                        if attempts.fetch_add(1, Ordering::Relaxed) < 3 {
                            return Err(crate::Error::unavailable("still starting up"));
                        }
                        let stream = tokio::net::TcpStream::connect(addr).await.map_err(|e| {
                            crate::Error::unavailable(format!("test connect failed: {e}"))
                        })?;
                        crate::imap::conn::login(stream, "user", "pw").await
                    }
                },
                sink,
                &mut (),
            )
            .await
        }
    });

    until("the watch to get through", || cycles.len() >= 1).await;
    cancel.cancel();
    let report = watch.await.unwrap().unwrap();

    assert_eq!(report.outcome, WatchOutcome::Cancelled);
    assert_eq!(
        report.permanent_failures, 0,
        "a server that is merely down is transient, so it never counts toward \
         the give-up threshold however long it stays down"
    );
    assert!(attempts.load(Ordering::Relaxed) >= 4);
    assert_eq!(
        cycles.triggers()[0],
        WatchTrigger::Initial,
        "the first successful connection is still the initial pass"
    );
}

#[tokio::test]
async fn a_server_that_stays_down_retries_forever_rather_than_giving_up() {
    // The mirror of `a_permanently_broken_watch_gives_up_instead_of_spinning`.
    // A provider outage or a sleeping laptop must not end a watch — a mailbox
    // that silently stops receiving mail and looks healthy is the exact failure
    // this engine exists to prevent.
    let fx = Fixture::open().await;
    let attempts = Arc::new(AtomicU32::new(0));
    let cancel = CancellationToken::new();

    let watch = tokio::spawn({
        let (db, cancel) = (fx.db.clone(), cancel.clone());
        let attempts = Arc::clone(&attempts);
        let mailbox_id = fx.mailbox_id;
        async move {
            watch_folder(
                &db,
                mailbox_id,
                polling_capabilities(),
                fast_watch(),
                &cancel,
                move || {
                    let attempts = Arc::clone(&attempts);
                    async move {
                        attempts.fetch_add(1, Ordering::Relaxed);
                        Err::<async_imap::Session<tokio::net::TcpStream>, _>(
                            crate::Error::unavailable("the server is down"),
                        )
                    }
                },
                |_| {},
                &mut (),
            )
            .await
        }
    });

    // Well past MAX_PERMANENT_FAILURES: a transient error must never reach it.
    until("many retries", || {
        attempts.load(Ordering::Relaxed) > crate::sync::idle::MAX_PERMANENT_FAILURES * 3
    })
    .await;
    cancel.cancel();
    let report = watch.await.unwrap().unwrap();

    assert_eq!(
        report.outcome,
        WatchOutcome::Cancelled,
        "it was still trying when we stopped it"
    );
    assert_eq!(report.permanent_failures, 0);
}

#[tokio::test]
async fn a_persistent_post_connect_failure_backs_off_instead_of_hammering() {
    // Connecting successfully and then failing is the shape that hides a hot
    // loop: a per-stage backoff that resets on connect undoes itself on every
    // pass, and a folder the server refuses to select becomes thousands of
    // logins an hour, forever.
    let fx = Fixture::open().await;
    let mock = MockImap::start(no_idle(mock_config(1)).unselectable("INBOX")).await;

    let attempts = Arc::new(AtomicU32::new(0));
    let cycles = Cycles::default();
    let cancel = CancellationToken::new();
    let opts = IdleOptions {
        backoff_min: Duration::from_millis(10),
        backoff_max: Duration::from_millis(40),
        ..fast_watch()
    };
    let watch = tokio::spawn({
        let (db, cancel, sink) = (fx.db.clone(), cancel.clone(), cycles.sink());
        let attempts = Arc::clone(&attempts);
        let mailbox_id = fx.mailbox_id;
        let addr = mock.addr;
        async move {
            watch_folder(
                &db,
                mailbox_id,
                polling_capabilities(),
                opts,
                &cancel,
                move || {
                    let attempts = Arc::clone(&attempts);
                    async move {
                        attempts.fetch_add(1, Ordering::Relaxed);
                        let stream = tokio::net::TcpStream::connect(addr).await.map_err(|e| {
                            crate::Error::unavailable(format!("test connect failed: {e}"))
                        })?;
                        crate::imap::conn::login(stream, "user", "pw").await
                    }
                },
                sink,
                &mut (),
            )
            .await
        }
    });

    // A SELECT that returns NO is NOT_FOUND — permanent — so this watch should
    // give up rather than reconnect indefinitely.
    let report = tokio::time::timeout(Duration::from_secs(10), watch)
        .await
        .expect("a permanently unselectable folder must not be retried forever")
        .unwrap()
        .unwrap();

    cancel.cancel();
    assert_eq!(report.outcome, WatchOutcome::GaveUp);
    assert_eq!(
        report.permanent_failures,
        crate::sync::idle::MAX_PERMANENT_FAILURES
    );
    assert!(
        attempts.load(Ordering::Relaxed) <= 4,
        "it reconnected once per failure, not in a loop: {} attempts",
        attempts.load(Ordering::Relaxed)
    );
}
