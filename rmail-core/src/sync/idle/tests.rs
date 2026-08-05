//! The `IDLE` push path: a watch that parks, wakes on what the server
//! volunteers, survives a dropped connection, and stops when told to.
//!
//! These tests drive real sockets against the in-process mock, so the `IDLE`
//! command, the `+ idling` continuation, the untagged push, and the `DONE`
//! handshake all actually happen. Timings are milliseconds rather than minutes;
//! nothing here sleeps for a cadence it could assert on instead.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::imap::mock::{MockConfig, MockImap};
use crate::imap::ImapCapabilities;

use super::super::harness::{
    connect, fast_watch, mock_config, raw, until, with_baseline, Cycles, Fixture, UIDVALIDITY,
};
use super::*;

#[tokio::test]
async fn a_push_wakes_the_watch_and_syncs_within_millis() {
    // The whole point of IDLE: the server speaks, and the client reacts without
    // anybody having asked it to.
    let fx = Fixture::open().await;
    let mock = MockImap::start(mock_config(2)).await;
    with_baseline(&fx, &mock, Some(1)).await;

    let cycles = Cycles::default();
    let cancel = CancellationToken::new();
    let watch = tokio::spawn({
        let (db, cancel, sink) = (fx.db.clone(), cancel.clone(), cycles.sink());
        let addr = mock.addr;
        let mailbox_id = fx.mailbox_id;
        async move {
            watch_folder(
                &db,
                mailbox_id,
                ImapCapabilities {
                    idle: true,
                    condstore: true,
                    qresync: true,
                    move_: false,
                },
                fast_watch(),
                &cancel,
                || async move {
                    let stream = tokio::net::TcpStream::connect(addr).await.map_err(|e| {
                        crate::Error::unavailable(format!("test connect failed: {e}"))
                    })?;
                    crate::imap::conn::login(stream, "user", "pw").await
                },
                sink,
                &mut (),
            )
            .await
        }
    });

    // The initial pass runs before parking, so a client never waits on IDLE for
    // mail that had already arrived.
    until("the initial sync", || cycles.len() >= 1).await;
    assert_eq!(cycles.triggers()[0], WatchTrigger::Initial);
    until("the watch to park on IDLE", || mock.idling()).await;

    mock.push("* 3 EXISTS");
    until("the pushed cycle", || {
        cycles.triggers().contains(&WatchTrigger::Pushed)
    })
    .await;

    cancel.cancel();
    let report = watch.await.unwrap().unwrap();
    assert_eq!(report.outcome, WatchOutcome::Cancelled);
    assert!(report.used_idle, "the watch really parked on IDLE");
    assert_eq!(report.sync_failures, 0);
    assert!(
        mock.commands().iter().any(|c| c == "IDLE"),
        "IDLE was issued: {:?}",
        mock.commands()
    );
    assert!(
        cycles.all().iter().all(|c| c.pushing),
        "every cycle reported itself as push-driven"
    );
}

#[tokio::test]
async fn a_pushed_message_lands_in_the_database() {
    // Waking is only half of it — the cycle has to actually resolve what
    // changed, which is the delta engine's job, not IDLE's.
    let fx = Fixture::open().await;
    let mock = MockImap::start(mock_config(2)).await;
    with_baseline(&fx, &mock, Some(1)).await;
    assert_eq!(fx.stored_uids(), vec![1, 2]);

    // The mock grows a message while the watch is parked. Its modseq is above
    // the checkpoint, so the delta probe will report it.
    let grown =
        MockImap::start(mock_config(2).fetch_at("INBOX", 3, &["\\Recent"], &raw(3), 9)).await;

    let cycles = Cycles::default();
    let cancel = CancellationToken::new();
    let watch = tokio::spawn({
        let (db, cancel, sink) = (fx.db.clone(), cancel.clone(), cycles.sink());
        let addr = grown.addr;
        let mailbox_id = fx.mailbox_id;
        async move {
            watch_folder(
                &db,
                mailbox_id,
                ImapCapabilities {
                    idle: true,
                    condstore: true,
                    qresync: true,
                    move_: false,
                },
                fast_watch(),
                &cancel,
                || async move {
                    let stream = tokio::net::TcpStream::connect(addr).await.map_err(|e| {
                        crate::Error::unavailable(format!("test connect failed: {e}"))
                    })?;
                    crate::imap::conn::login(stream, "user", "pw").await
                },
                sink,
                &mut (),
            )
            .await
        }
    });

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
    assert_eq!(downloaded, 1, "exactly the one new message");
}

#[tokio::test]
async fn the_idle_is_reissued_on_cadence() {
    // RFC 2177 §3: a server may log off a client whose IDLE has run too long,
    // so parking forever is not an option however quiet the folder is.
    let fx = Fixture::open().await;
    let mock = MockImap::start(mock_config(1)).await;
    with_baseline(&fx, &mock, Some(1)).await;

    let cycles = Cycles::default();
    let cancel = CancellationToken::new();
    let watch = tokio::spawn({
        let (db, cancel, sink) = (fx.db.clone(), cancel.clone(), cycles.sink());
        let addr = mock.addr;
        let mailbox_id = fx.mailbox_id;
        let opts = IdleOptions {
            re_idle: Duration::from_millis(40),
            ..fast_watch()
        };
        async move {
            watch_folder(
                &db,
                mailbox_id,
                ImapCapabilities {
                    idle: true,
                    condstore: true,
                    qresync: true,
                    move_: false,
                },
                opts,
                &cancel,
                || async move {
                    let stream = tokio::net::TcpStream::connect(addr).await.map_err(|e| {
                        crate::Error::unavailable(format!("test connect failed: {e}"))
                    })?;
                    crate::imap::conn::login(stream, "user", "pw").await
                },
                sink,
                &mut (),
            )
            .await
        }
    });

    until("two re-IDLE cycles", || {
        cycles
            .triggers()
            .iter()
            .filter(|t| **t == WatchTrigger::ReIdle)
            .count()
            >= 2
    })
    .await;
    cancel.cancel();
    let report = watch.await.unwrap().unwrap();

    assert_eq!(report.outcome, WatchOutcome::Cancelled);
    let idles = mock.commands().iter().filter(|c| *c == "IDLE").count();
    assert!(
        idles >= 3,
        "IDLE was torn down and reissued, not held open: {idles} issued"
    );
}

#[tokio::test]
async fn a_dropped_connection_reconnects_and_keeps_watching() {
    // A long-lived connection is a connection that will be dropped. A watcher
    // that exits on the first broken pipe is a mail client that silently stops
    // receiving mail while looking perfectly healthy.
    let fx = Fixture::open().await;
    let first = MockImap::start(mock_config(2)).await;
    with_baseline(&fx, &first, Some(1)).await;

    // Two servers, one address at a time: the first is dropped mid-watch, which
    // closes its listener and every connection it was serving.
    let second = MockImap::start(mock_config(2)).await;
    let addr = Arc::new(Mutex::new(first.addr));
    let cycles = Cycles::default();
    let cancel = CancellationToken::new();

    let watch = tokio::spawn({
        let (db, cancel, sink) = (fx.db.clone(), cancel.clone(), cycles.sink());
        let addr = Arc::clone(&addr);
        let mailbox_id = fx.mailbox_id;
        async move {
            watch_folder(
                &db,
                mailbox_id,
                ImapCapabilities {
                    idle: true,
                    condstore: true,
                    qresync: true,
                    move_: false,
                },
                fast_watch(),
                &cancel,
                move || {
                    let target = *addr.lock().expect("addr poisoned");
                    async move {
                        let stream = tokio::net::TcpStream::connect(target).await.map_err(|e| {
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

    until("the first connection to park", || first.idling()).await;
    *addr.lock().unwrap() = second.addr;
    drop(first);

    until("the watch to reconnect", || {
        cycles.triggers().contains(&WatchTrigger::Reconnected)
    })
    .await;

    // And it is genuinely watching again, not merely reconnected.
    until("the new connection to park", || second.idling()).await;
    second.push("* 3 EXISTS");
    until("a push on the new connection", || {
        cycles
            .all()
            .iter()
            .skip_while(|c| c.trigger != WatchTrigger::Reconnected)
            .any(|c| c.trigger == WatchTrigger::Pushed)
    })
    .await;

    cancel.cancel();
    let report = watch.await.unwrap().unwrap();
    assert_eq!(report.outcome, WatchOutcome::Cancelled);
}

#[tokio::test]
async fn a_permanently_broken_watch_gives_up_instead_of_spinning() {
    // Retrying forever on an account whose password was revoked is a busy loop
    // with a mail-client-shaped wrapper. A server that is merely *down* is a
    // different thing and must not be treated this way — see
    // `sync::poll_fallback` for that side of the rule.
    let fx = Fixture::open().await;
    let attempts = Arc::new(AtomicU32::new(0));

    let report = watch_folder(
        &fx.db,
        fx.mailbox_id,
        ImapCapabilities::default(),
        fast_watch(),
        &CancellationToken::new(),
        || {
            let attempts = Arc::clone(&attempts);
            async move {
                attempts.fetch_add(1, Ordering::Relaxed);
                Err::<async_imap::Session<tokio::net::TcpStream>, _>(crate::Error::unauthenticated(
                    "password was revoked",
                ))
            }
        },
        |_| {},
        &mut (),
    )
    .await
    .unwrap();

    assert_eq!(report.outcome, WatchOutcome::GaveUp);
    assert_eq!(report.permanent_failures, MAX_PERMANENT_FAILURES);
    assert_eq!(report.cycles, 0, "it never got far enough to sync");
}

#[tokio::test]
async fn cancelling_a_parked_watch_terminates_the_idle_cleanly() {
    // A shutdown that abandons IDLE mid-flight leaves the server holding a
    // command it never saw finish — the connection is abandoned rather than
    // closed, and the server keeps the mailbox locked until its own timeout.
    let fx = Fixture::open().await;
    let mock = MockImap::start(mock_config(1)).await;
    with_baseline(&fx, &mock, Some(1)).await;

    let cancel = CancellationToken::new();
    let cycles = Cycles::default();
    let mut watch = tokio::spawn({
        let (db, cancel, sink) = (fx.db.clone(), cancel.clone(), cycles.sink());
        let addr = mock.addr;
        let mailbox_id = fx.mailbox_id;
        // Long enough that the cadence cannot be what ends the park.
        let opts = IdleOptions {
            re_idle: Duration::from_secs(600),
            ..fast_watch()
        };
        async move {
            watch_folder(
                &db,
                mailbox_id,
                ImapCapabilities {
                    idle: true,
                    condstore: true,
                    qresync: true,
                    move_: false,
                },
                opts,
                &cancel,
                || async move {
                    let stream = tokio::net::TcpStream::connect(addr).await.map_err(|e| {
                        crate::Error::unavailable(format!("test connect failed: {e}"))
                    })?;
                    crate::imap::conn::login(stream, "user", "pw").await
                },
                sink,
                &mut (),
            )
            .await
        }
    });

    until("the watch to park", || mock.idling()).await;
    cancel.cancel();

    // It returns promptly rather than waiting out the 10-minute cadence.
    let joined = tokio::time::timeout(Duration::from_secs(5), &mut watch).await;
    if joined.is_err() {
        // Otherwise the detached task keeps the runtime alive and the failure
        // reads as a hung suite rather than as this assertion.
        watch.abort();
    }
    let report = joined
        .expect("a cancelled watch must not wait for its re-IDLE cadence")
        .unwrap()
        .unwrap();
    assert_eq!(report.outcome, WatchOutcome::Cancelled);

    until("DONE to reach the server", || {
        mock.commands()
            .iter()
            .any(|c| c.eq_ignore_ascii_case("DONE"))
    })
    .await;
    assert!(
        mock.commands().iter().any(|c| c.starts_with("LOGOUT")),
        "and the session was closed, not dropped: {:?}",
        mock.commands()
    );
}

#[tokio::test]
async fn a_watch_cancelled_before_it_starts_makes_no_connection() {
    let fx = Fixture::open().await;
    let attempts = Arc::new(AtomicU32::new(0));
    let cancel = CancellationToken::new();
    cancel.cancel();

    let report = watch_folder(
        &fx.db,
        fx.mailbox_id,
        ImapCapabilities::default(),
        fast_watch(),
        &cancel,
        || {
            let attempts = Arc::clone(&attempts);
            async move {
                attempts.fetch_add(1, Ordering::Relaxed);
                Err::<async_imap::Session<tokio::net::TcpStream>, _>(crate::Error::unavailable(
                    "should not be reached",
                ))
            }
        },
        |_| {},
        &mut (),
    )
    .await
    .unwrap();

    assert_eq!(report.outcome, WatchOutcome::Cancelled);
    assert_eq!(attempts.load(Ordering::Relaxed), 0);
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

#[test]
fn the_idle_duration_is_clamped_to_what_rfc_2177_allows() {
    // A server may log off a client whose IDLE has run too long, so a generous
    // configured value must not become a disconnection.
    let opts = IdleOptions {
        re_idle: Duration::from_secs(60 * 60),
        ..Default::default()
    };
    assert_eq!(opts.effective_re_idle(), MAX_IDLE);

    let opts = IdleOptions {
        re_idle: Duration::from_secs(30),
        ..Default::default()
    };
    assert_eq!(opts.effective_re_idle(), Duration::from_secs(30));

    // Zero would busy-loop the wait.
    let opts = IdleOptions {
        re_idle: Duration::ZERO,
        ..Default::default()
    };
    assert!(opts.effective_re_idle() > Duration::ZERO);
}

#[test]
fn triggers_have_stable_names() {
    assert_eq!(WatchTrigger::Initial.as_str(), "initial");
    assert_eq!(WatchTrigger::Pushed.as_str(), "pushed");
    assert_eq!(WatchTrigger::ReIdle.as_str(), "re-idle");
    assert_eq!(WatchTrigger::Polled.as_str(), "polled");
    assert_eq!(WatchTrigger::Reconnected.as_str(), "reconnected");
}

/// The mock is only useful here if `connect` actually reaches it.
#[tokio::test]
async fn the_test_connector_reaches_the_mock() {
    let mock = MockImap::start(MockConfig::default().password("pw")).await;
    let mut session = connect(&mock).await;
    let _ = session.logout().await;
}

#[tokio::test]
async fn keepalives_do_not_postpone_the_re_idle_cadence() {
    // Dovecot, Cyrus and Gmail all volunteer `* OK Still here` every couple of
    // minutes so intermediaries do not reap the connection. async-imap treats
    // every such response as a reason to restart its own timeout, so a client
    // that relies on that timeout for its cadence never reissues IDLE at all —
    // and RFC 2177 §3's "a server MAY log the client off" becomes a mailbox
    // that quietly stops receiving mail.
    let fx = Fixture::open().await;
    let mock = MockImap::start(mock_config(1).idle_keepalive(Duration::from_millis(10))).await;
    with_baseline(&fx, &mock, Some(1)).await;

    let cycles = Cycles::default();
    let cancel = CancellationToken::new();
    let opts = IdleOptions {
        // Deliberately longer than the keepalive: if keepalives reset the
        // cadence, this never elapses.
        re_idle: Duration::from_millis(60),
        ..fast_watch()
    };
    let watch = tokio::spawn({
        let (db, cancel, sink) = (fx.db.clone(), cancel.clone(), cycles.sink());
        let addr = mock.addr;
        let mailbox_id = fx.mailbox_id;
        async move {
            watch_folder(
                &db,
                mailbox_id,
                ImapCapabilities {
                    idle: true,
                    condstore: true,
                    qresync: true,
                    move_: false,
                },
                opts,
                &cancel,
                || async move {
                    let stream = tokio::net::TcpStream::connect(addr).await.map_err(|e| {
                        crate::Error::unavailable(format!("test connect failed: {e}"))
                    })?;
                    crate::imap::conn::login(stream, "user", "pw").await
                },
                sink,
                &mut (),
            )
            .await
        }
    });

    until("re-IDLE despite a stream of keepalives", || {
        cycles
            .triggers()
            .iter()
            .filter(|t| **t == WatchTrigger::ReIdle)
            .count()
            >= 2
    })
    .await;
    cancel.cancel();
    let report = watch.await.unwrap().unwrap();
    assert_eq!(report.outcome, WatchOutcome::Cancelled);

    let idles = mock.commands().iter().filter(|c| *c == "IDLE").count();
    assert!(idles >= 3, "IDLE was actually reissued: {idles} issued");
}

#[tokio::test]
async fn a_pushed_flag_change_is_reflected() {
    // The acceptance criterion names three changes, not one. A flag flipped on
    // another device leaves the UID set identical, so nothing about the folder's
    // shape reveals it — only the modseq probe the wake-up runs.
    let fx = Fixture::open().await;
    let seed = MockImap::start(mock_config(2)).await;
    with_baseline(&fx, &seed, Some(1)).await;
    assert_eq!(fx.flags_of(2), vec!["\\Seen".to_owned()]);
    drop(seed);

    let changed = MockImap::start(mock_config(2).change(2, &["\\Seen", "\\Flagged"], 11)).await;
    let cancel = CancellationToken::new();
    let cycles = Cycles::default();
    let watch = tokio::spawn({
        let (db, cancel, sink) = (fx.db.clone(), cancel.clone(), cycles.sink());
        let addr = changed.addr;
        let mailbox_id = fx.mailbox_id;
        async move {
            watch_folder(
                &db,
                mailbox_id,
                ImapCapabilities {
                    idle: true,
                    condstore: true,
                    qresync: true,
                    move_: false,
                },
                fast_watch(),
                &cancel,
                || async move {
                    let stream = tokio::net::TcpStream::connect(addr).await.map_err(|e| {
                        crate::Error::unavailable(format!("test connect failed: {e}"))
                    })?;
                    crate::imap::conn::login(stream, "user", "pw").await
                },
                sink,
                &mut (),
            )
            .await
        }
    });

    until("the flag change to land", || {
        fx.flags_of(2) == vec!["\\Flagged".to_owned(), "\\Seen".to_owned()]
    })
    .await;
    cancel.cancel();
    assert_eq!(watch.await.unwrap().unwrap().sync_failures, 0);
}

#[tokio::test]
async fn a_pushed_expunge_is_reflected() {
    let fx = Fixture::open().await;
    let seed = MockImap::start(mock_config(3)).await;
    with_baseline(&fx, &seed, Some(1)).await;
    assert_eq!(fx.stored_uids(), vec![1, 2, 3]);
    drop(seed);

    let after = MockImap::start(
        MockConfig::default()
            .password("pw")
            .uidvalidity(u32::try_from(UIDVALIDITY).unwrap())
            .fetch(1, &["\\Seen"], &raw(1))
            .fetch(3, &["\\Seen"], &raw(3))
            .expunged(2, 12),
    )
    .await;
    let cancel = CancellationToken::new();
    let cycles = Cycles::default();
    let watch = tokio::spawn({
        let (db, cancel, sink) = (fx.db.clone(), cancel.clone(), cycles.sink());
        let addr = after.addr;
        let mailbox_id = fx.mailbox_id;
        async move {
            watch_folder(
                &db,
                mailbox_id,
                ImapCapabilities {
                    idle: true,
                    condstore: true,
                    qresync: true,
                    move_: false,
                },
                fast_watch(),
                &cancel,
                || async move {
                    let stream = tokio::net::TcpStream::connect(addr).await.map_err(|e| {
                        crate::Error::unavailable(format!("test connect failed: {e}"))
                    })?;
                    crate::imap::conn::login(stream, "user", "pw").await
                },
                sink,
                &mut (),
            )
            .await
        }
    });

    until("the expunge to land", || fx.stored_uids() == vec![1, 3]).await;
    cancel.cancel();
    assert_eq!(watch.await.unwrap().unwrap().sync_failures, 0);
}

#[tokio::test]
async fn an_account_watches_its_highest_priority_folders_up_to_the_limit() {
    // Every watch is a socket the server holds open, and servers cap concurrent
    // connections per account. Watching everything would get the folders that
    // actually matter refused.
    let fx = Fixture::open_with_folders(&["Zebra", "INBOX", "Archive", "Work"]).await;
    let mock = MockImap::start(
        mock_config(1)
            .folders(vec![
                ("INBOX", ""),
                ("Archive", ""),
                ("Work", ""),
                ("Zebra", ""),
            ])
            .fetch_in("Archive", 1, &["\\Seen"], &raw(2))
            .fetch_in("Work", 1, &["\\Seen"], &raw(3))
            .fetch_in("Zebra", 1, &["\\Seen"], &raw(4)),
    )
    .await;

    let cancel = CancellationToken::new();
    let opts = IdleOptions {
        watch_limit: 2,
        ..fast_watch()
    };
    let watch = tokio::spawn({
        let (db, cancel) = (fx.db.clone(), cancel.clone());
        let account_id = fx.account_id;
        let addr = mock.addr;
        async move {
            crate::sync::watch_folders(
                &db,
                account_id,
                ImapCapabilities {
                    idle: true,
                    condstore: true,
                    qresync: true,
                    move_: false,
                },
                opts,
                &cancel,
                move || async move {
                    let stream = tokio::net::TcpStream::connect(addr).await.map_err(|e| {
                        crate::Error::unavailable(format!("test connect failed: {e}"))
                    })?;
                    crate::imap::conn::login(stream, "user", "pw").await
                },
                |_| {},
                (),
            )
            .await
        }
    });

    until("both watches to park", || mock.idling()).await;
    cancel.cancel();
    let out = watch.await.unwrap().unwrap();

    assert!(out.failures.is_empty(), "{:?}", out.failures);
    assert_eq!(out.reports.len(), 2, "the limit is a budget, not a hint");
    let watched: Vec<String> = out
        .reports
        .iter()
        .map(|r| {
            let id = r.mailbox_id;
            fx.db
                .with_read(move |c| crate::repo::get_mailbox(c, id))
                .unwrap()
                .unwrap()
                .name
        })
        .collect();
    assert_eq!(
        watched,
        vec!["INBOX", "Archive"],
        "the folders a user notices first got the connections"
    );
    assert!(out
        .reports
        .iter()
        .all(|r| r.outcome == WatchOutcome::Cancelled));
}
