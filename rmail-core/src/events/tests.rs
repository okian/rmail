//! The guarantees a subscriber depends on: durability, gaplessness, and being
//! told the truth when a cursor has fallen off the end of the log.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use super::*;
use crate::ErrorReason;
use tonic_types::StatusExt;

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Fixture {
    log: EventLog,
    db: Database,
    path: PathBuf,
}

impl Fixture {
    async fn open() -> Self {
        Self::with_retention(Retention::unlimited()).await
    }

    async fn with_retention(retention: Retention) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-events-{pid}-{n}.db"));
        // A crash plus pid reuse would otherwise hand the next run a populated
        // database and make every position assertion flake.
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", path.display())));
        }
        let db = Database::open(&path).unwrap();
        let log = EventLog::new(db.clone(), retention);
        Self { log, db, path }
    }

    /// Append `count` `NewMail` events and return their positions.
    async fn fill(&self, count: usize) -> Vec<i64> {
        let events = (0..count)
            .map(|i| {
                NewEvent::new(EventKind::NewMail)
                    .account(1)
                    .payload(serde_json::json!({ "n": i }))
            })
            .collect();
        self.log
            .append_all(events)
            .await
            .unwrap()
            .into_iter()
            .map(|e| e.seq)
            .collect()
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
async fn an_appended_event_is_durable_and_carries_its_position() {
    let fx = Fixture::open().await;

    let event = fx
        .log
        .append(
            NewEvent::new(EventKind::NewMail)
                .account(7)
                .mailbox(3)
                .message(11)
                .payload(serde_json::json!({ "uid": 42 })),
        )
        .await
        .unwrap();

    assert_eq!(
        event.seq, 1,
        "positions start at 1, so 0 is a usable cursor"
    );
    assert_eq!(event.kind, EventKind::NewMail);
    assert_eq!(event.account_id, Some(7));
    assert_eq!(event.mailbox_id, Some(3));
    assert_eq!(event.message_id, Some(11));
    assert_eq!(event.payload, serde_json::json!({ "uid": 42 }));
    assert!(event.at > 0);

    // Durable, not merely broadcast: a fresh log over the same database sees it.
    let reopened = EventLog::new(fx.db.clone(), Retention::unlimited());
    assert_eq!(reopened.since(0, 10).await.unwrap().events, vec![event]);
}

#[tokio::test]
async fn positions_are_monotonic_and_dense_within_a_batch() {
    let fx = Fixture::open().await;
    let first = fx.fill(3).await;
    let second = fx.fill(3).await;

    assert_eq!(first, vec![1, 2, 3]);
    assert_eq!(
        second,
        vec![4, 5, 6],
        "a second batch continues the sequence"
    );
}

#[tokio::test]
async fn a_subscriber_sees_events_live() {
    let fx = Fixture::open().await;
    let mut rx = fx.log.subscribe();
    assert_eq!(fx.log.subscriber_count(), 1);

    fx.log
        .append(NewEvent::new(EventKind::FlagChanged).message(5))
        .await
        .unwrap();

    let received = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv())
        .await
        .expect("a live subscriber should not have to wait")
        .unwrap();
    assert_eq!(received.kind, EventKind::FlagChanged);
    assert_eq!(received.message_id, Some(5));
    assert_eq!(received.seq, 1);
}

#[tokio::test]
async fn an_append_with_no_subscribers_still_commits() {
    // The channel is a latency shortcut, not the source of truth. If a failed
    // send could fail an append, the log would go down whenever nothing
    // happened to be listening.
    let fx = Fixture::open().await;
    assert_eq!(fx.log.subscriber_count(), 0);

    let event = fx.log.append(NewEvent::new(EventKind::SyncState)).await;

    assert!(event.is_ok(), "{:?}", event.err());
    assert_eq!(fx.log.len().await.unwrap(), 1);
}

#[tokio::test]
async fn a_lagging_subscriber_loses_its_place_but_not_its_data() {
    // The whole reason `seq` lives on the event rather than being implied by
    // arrival order: a subscriber that falls behind the bounded channel
    // recovers by reading the durable log, not by being handed the events
    // again.
    let fx = Fixture::with_retention(Retention::unlimited()).await;
    let log = EventLog::with_capacity(fx.db.clone(), Retention::unlimited(), 2);
    let mut rx = log.subscribe();

    for _ in 0..10 {
        log.append(NewEvent::new(EventKind::NewMail).account(1))
            .await
            .unwrap();
    }

    let lagged = rx.recv().await;
    assert!(
        matches!(lagged, Err(broadcast::error::RecvError::Lagged(_))),
        "the channel is lossy under lag by design: {lagged:?}"
    );

    // Nothing was lost — it is all still in the log, from the last position the
    // subscriber actually processed.
    let recovered = log.since(0, 100).await.unwrap().events;
    assert_eq!(recovered.len(), 10);
    assert_eq!(
        recovered.iter().map(|e| e.seq).collect::<Vec<_>>(),
        (1..=10).collect::<Vec<i64>>(),
        "and with no gaps"
    );
}

#[tokio::test]
async fn resuming_from_a_cursor_returns_exactly_what_came_after_it() {
    let fx = Fixture::open().await;
    fx.fill(10).await;

    let resumed = fx.log.since(4, 100).await.unwrap().events;
    assert_eq!(
        resumed.iter().map(|e| e.seq).collect::<Vec<_>>(),
        (5..=10).collect::<Vec<i64>>(),
        "strictly after the cursor, so the client never re-processes one"
    );

    let from_scratch = fx.log.since(0, 100).await.unwrap().events;
    assert_eq!(from_scratch.len(), 10, "cursor 0 means everything retained");

    let caught_up = fx.log.since(10, 100).await.unwrap().events;
    assert!(caught_up.is_empty(), "a current cursor yields nothing");
}

#[tokio::test]
async fn a_resume_pages_without_gaps() {
    // A client away for a week must not be answered with the whole log in one
    // allocation, and paging must not drop anything at the seams.
    let fx = Fixture::open().await;
    fx.fill(25).await;

    let mut seen: Vec<i64> = Vec::new();
    let mut cursor = 0i64;
    loop {
        let page = fx.log.since(cursor, 7).await.unwrap().events;
        if page.is_empty() {
            break;
        }
        cursor = page.last().map_or(cursor, |e| e.seq);
        seen.extend(page.iter().map(|e| e.seq));
    }

    assert_eq!(seen, (1..=25).collect::<Vec<i64>>());
}

#[tokio::test]
async fn a_cursor_past_retention_is_out_of_range_with_somewhere_to_resume() {
    // Answering "no events" here would be a lie indistinguishable from a quiet
    // mailbox, and the client would believe itself current forever.
    let fx = Fixture::with_retention(Retention {
        max_rows: Some(5),
        max_age: None,
    })
    .await;
    fx.fill(20).await;
    fx.log.prune().await.unwrap();

    let oldest = fx.log.oldest_seq().await.unwrap().unwrap();
    assert_eq!(oldest, 16, "the last five survive");

    let err = fx
        .log
        .since(3, 100)
        .await
        .expect_err("cursor 3 is long gone");
    assert_eq!(err.reason(), ErrorReason::OutOfRange);

    // The client is told where to resync from, in structured metadata rather
    // than in message text.
    let status = tonic::Status::from(err);
    assert_eq!(status.code(), tonic::Code::OutOfRange);
    let details = status.get_error_details();
    let info = details.error_info().expect("ErrorInfo attached");
    assert_eq!(
        info.metadata.get(crate::error::OLDEST_SEQ_KEY),
        Some(&oldest.to_string()),
        "metadata: {:?}",
        info.metadata
    );
}

#[tokio::test]
async fn a_cursor_exactly_at_the_retention_floor_is_not_a_gap() {
    // The off-by-one that matters: a client whose last processed event is the
    // one immediately below the floor has missed nothing.
    let fx = Fixture::with_retention(Retention {
        max_rows: Some(5),
        max_age: None,
    })
    .await;
    fx.fill(10).await;
    fx.log.prune().await.unwrap();
    let oldest = fx.log.oldest_seq().await.unwrap().unwrap();
    assert_eq!(oldest, 6);

    let resumed = fx.log.since(oldest - 1, 100).await.unwrap().events;
    assert_eq!(
        resumed.iter().map(|e| e.seq).collect::<Vec<_>>(),
        (6..=10).collect::<Vec<i64>>(),
        "cursor 5 is current with the floor, not behind it"
    );

    assert_eq!(
        fx.log.since(oldest - 2, 100).await.unwrap_err().reason(),
        ErrorReason::OutOfRange,
        "one lower is a genuine gap"
    );
}

#[tokio::test]
async fn a_fresh_subscriber_of_an_empty_log_is_not_told_it_missed_something() {
    let fx = Fixture::open().await;
    assert!(fx.log.is_empty().await.unwrap());
    assert!(fx.log.since(0, 100).await.unwrap().events.is_empty());
}

#[tokio::test]
async fn a_cursor_ahead_of_the_log_is_out_of_range() {
    // Usually a database replaced underneath a running client. Silently
    // returning nothing would leave it waiting for events that will never come.
    let fx = Fixture::open().await;
    fx.fill(3).await;

    let err = fx
        .log
        .since(99, 10)
        .await
        .expect_err("cursor 99 never existed");
    assert_eq!(err.reason(), ErrorReason::OutOfRange);
}

#[tokio::test]
async fn retention_by_rows_keeps_the_newest_and_drops_from_the_bottom() {
    // Pruning from the bottom is what keeps the live range contiguous, which is
    // what makes "older than oldest_seq" a complete description of every gap.
    let fx = Fixture::with_retention(Retention {
        max_rows: Some(4),
        max_age: None,
    })
    .await;
    fx.fill(10).await;

    let dropped = fx.log.prune().await.unwrap();

    assert_eq!(dropped, 6);
    assert_eq!(fx.log.len().await.unwrap(), 4);
    let kept = fx.log.since(0, 100).await.unwrap().events;
    assert_eq!(
        kept.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![7, 8, 9, 10],
        "the newest four, contiguously"
    );
}

#[tokio::test]
async fn retention_by_age_drops_only_what_is_old() {
    let fx = Fixture::with_retention(Retention {
        max_rows: None,
        max_age: Some(std::time::Duration::from_secs(3600)),
    })
    .await;
    fx.fill(4).await;

    // Backdate the first two past the horizon.
    fx.db
        .write(|c| {
            c.execute(
                "UPDATE events SET at = unixepoch() - 7200 WHERE seq <= 2",
                [],
            )
        })
        .await
        .unwrap();

    let dropped = fx.log.prune().await.unwrap();

    assert_eq!(dropped, 2);
    assert_eq!(
        fx.log
            .since(0, 100)
            .await
            .unwrap()
            .events
            .iter()
            .map(|e| e.seq)
            .collect::<Vec<_>>(),
        vec![3, 4]
    );
}

#[tokio::test]
async fn pruning_an_unlimited_log_drops_nothing() {
    let fx = Fixture::open().await;
    fx.fill(50).await;
    assert_eq!(fx.log.prune().await.unwrap(), 0);
    assert_eq!(fx.log.len().await.unwrap(), 50);
}

#[tokio::test]
async fn positions_are_not_reused_after_pruning() {
    // If they were, a client's cursor could silently point at a *different*
    // event than the one it processed — the one failure mode a resumable log
    // must not have.
    let fx = Fixture::with_retention(Retention {
        max_rows: Some(2),
        max_age: None,
    })
    .await;
    fx.fill(5).await;
    fx.log.prune().await.unwrap();
    assert_eq!(fx.log.oldest_seq().await.unwrap(), Some(4));

    let next = fx
        .log
        .append(NewEvent::new(EventKind::NewMail))
        .await
        .unwrap();
    assert_eq!(next.seq, 6, "the sequence continues past what was dropped");
}

#[tokio::test]
async fn a_subscription_can_be_scoped_to_one_account() {
    let fx = Fixture::open().await;
    fx.log
        .append_all(vec![
            NewEvent::new(EventKind::NewMail).account(1),
            NewEvent::new(EventKind::NewMail).account(2),
            NewEvent::new(EventKind::NewMail).account(1),
        ])
        .await
        .unwrap();

    let one = fx.log.since_for_account(1, 0, 100).await.unwrap().events;
    assert_eq!(one.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![1, 3]);
    assert!(one.iter().all(|e| e.account_id == Some(1)));
}

#[tokio::test]
async fn an_account_with_no_events_yet_is_not_treated_as_behind() {
    // The gap check reads the floor of the whole log, not of the filtered view.
    // Otherwise every new account's first subscription would be told it had
    // fallen behind.
    let fx = Fixture::open().await;
    fx.log
        .append_all(vec![NewEvent::new(EventKind::NewMail).account(1); 3])
        .await
        .unwrap();

    let none = fx.log.since_for_account(99, 0, 100).await.unwrap().events;
    assert!(none.is_empty());
}

#[tokio::test]
async fn an_event_without_a_kind_is_rejected_before_it_reaches_the_log() {
    let fx = Fixture::open().await;
    let err = fx
        .log
        .append(NewEvent::default())
        .await
        .expect_err("a kindless event is not an event");
    assert_eq!(err.reason(), ErrorReason::InvalidArgument);
    assert!(fx.log.is_empty().await.unwrap(), "and nothing was written");
}

#[tokio::test]
async fn a_failed_batch_publishes_nothing() {
    // Half a batch reaching subscribers while none of it reached disk would
    // hand them events that a restart erases.
    let fx = Fixture::open().await;
    let mut rx = fx.log.subscribe();

    let err = fx
        .log
        .append_all(vec![NewEvent::new(EventKind::NewMail), NewEvent::default()])
        .await
        .expect_err("the batch is invalid");
    assert_eq!(err.reason(), ErrorReason::InvalidArgument);

    assert!(fx.log.is_empty().await.unwrap());
    assert!(
        rx.try_recv().is_err(),
        "nothing was published for an uncommitted batch"
    );
}

#[tokio::test]
async fn an_empty_batch_is_a_no_op() {
    let fx = Fixture::open().await;
    assert!(fx.log.append_all(Vec::new()).await.unwrap().is_empty());
    assert!(fx.log.is_empty().await.unwrap());
}

#[tokio::test]
async fn a_negative_cursor_is_invalid_argument_not_a_gap() {
    let fx = Fixture::open().await;
    assert_eq!(
        fx.log.since(-1, 10).await.unwrap_err().reason(),
        ErrorReason::InvalidArgument
    );
}

#[tokio::test]
async fn a_page_size_is_clamped_rather_than_trusted() {
    let fx = Fixture::open().await;
    fx.fill(20).await;

    assert_eq!(
        fx.log.since(0, 0).await.unwrap().events.len(),
        20,
        "zero means the server default, as an unset proto field would — \
         clamping it to one event would make a paging client crawl"
    );
    assert_eq!(fx.log.since(0, i64::MAX).await.unwrap().events.len(), 20);
    assert_eq!(fx.log.since(0, 5).await.unwrap().events.len(), 5);
}

#[test]
fn event_kinds_round_trip_through_their_wire_strings() {
    for kind in EventKind::ALL {
        assert_eq!(EventKind::parse(kind.as_str()).unwrap(), kind);
    }
    assert_eq!(
        EventKind::parse("NOT_A_KIND").unwrap_err().reason(),
        ErrorReason::Internal,
        "a kind this build never wrote means the log came from a newer one"
    );
}

#[test]
fn wire_strings_are_stable() {
    // These are stored in the log and branched on by clients; changing one is a
    // breaking change, so it should require changing this test too.
    assert_eq!(EventKind::NewMail.as_str(), "NEW_MAIL");
    assert_eq!(EventKind::FlagChanged.as_str(), "FLAG_CHANGED");
    assert_eq!(EventKind::Moved.as_str(), "MOVED");
    assert_eq!(EventKind::Deleted.as_str(), "DELETED");
    assert_eq!(EventKind::SyncState.as_str(), "SYNC_STATE");
    assert_eq!(EventKind::SendResult.as_str(), "SEND_RESULT");
    assert_eq!(EventKind::RuleFired.as_str(), "RULE_FIRED");
    assert_eq!(EventKind::AiSummary.as_str(), "AI_SUMMARY");
}

// ---------------------------------------------------------------------------
// The guarantee under stress: the ways a cursor can silently skip an event
// ---------------------------------------------------------------------------

#[tokio::test]
async fn positions_do_not_restart_when_retention_empties_the_log() {
    // The failure this exists to prevent: SQLite assigns a plain rowid as
    // `max(rowid) + 1` over the rows that *currently exist*, so an emptied
    // table restarts at 1. Retention empties this table routinely — a mailbox
    // quieter than the age window has every row swept — and a subscriber at
    // cursor 500 would then be handed fresh events numbered 1..100, be told
    // nothing was wrong, and skip them forever.
    let fx = Fixture::with_retention(Retention {
        max_rows: None,
        max_age: Some(std::time::Duration::from_secs(3600)),
    })
    .await;
    fx.fill(20).await;

    // Age every event past the horizon, then sweep the log clean.
    fx.db
        .write(|c| c.execute("UPDATE events SET at = unixepoch() - 7200", []))
        .await
        .unwrap();
    assert_eq!(fx.log.prune().await.unwrap(), 20);
    assert!(
        fx.log.is_empty().await.unwrap(),
        "the log is genuinely empty"
    );

    let next = fx
        .log
        .append(NewEvent::new(EventKind::NewMail))
        .await
        .unwrap();
    assert_eq!(
        next.seq, 21,
        "the sequence continues past everything ever assigned"
    );

    // And a subscriber holding a pre-sweep cursor is told the truth rather than
    // being handed the new event as if it were the next one it had not seen.
    let err = fx
        .log
        .since(10, 100)
        .await
        .expect_err("cursor 10 was swept");
    assert_eq!(err.reason(), ErrorReason::OutOfRange);
}

#[tokio::test]
async fn following_the_reported_resume_cursor_recovers_everything_retained() {
    // The off-by-one that costs exactly one event: `oldest_seq` is an id and
    // reads are strictly-after, so a client that passes it back as a cursor
    // skips the very event it names.
    let fx = Fixture::with_retention(Retention {
        max_rows: Some(5),
        max_age: None,
    })
    .await;
    fx.fill(20).await;
    fx.log.prune().await.unwrap();

    let err = fx
        .log
        .since(3, 100)
        .await
        .expect_err("cursor 3 is long gone");
    let status = tonic::Status::from(err);
    let details = status.get_error_details();
    let info = details.error_info().expect("ErrorInfo attached");
    let resume_from: i64 = info
        .metadata
        .get(crate::error::RESUME_FROM_KEY)
        .expect("a cursor to resume from")
        .parse()
        .unwrap();

    let recovered = fx.log.since(resume_from, 100).await.unwrap().events;
    assert_eq!(
        recovered.iter().map(|e| e.seq).collect::<Vec<_>>(),
        (16..=20).collect::<Vec<i64>>(),
        "following the reported cursor yields every retained event, including \
         the oldest one"
    );
}

#[tokio::test]
async fn a_quiet_account_never_falls_permanently_out_of_range() {
    // A filtered cursor that only advanced on *matching* events would freeze at
    // whatever position the account's last event had, while retention is
    // global and keeps moving. The account then goes stale, resyncs, receives
    // nothing (it has had nothing), and goes stale again — forever, having
    // missed nothing at all.
    let fx = Fixture::with_retention(Retention {
        max_rows: Some(15),
        max_age: None,
    })
    .await;

    // Our account speaks once, so its cursor is non-zero from here on.
    fx.log
        .append(NewEvent::new(EventKind::NewMail).account(1))
        .await
        .unwrap();
    let mut cursor = fx.log.since_for_account(1, 0, 100).await.unwrap().next_seq;
    assert_eq!(cursor, 1);

    // Then it goes quiet while another account is busy. Retention keeps well
    // ahead of one round's traffic, so nothing this client could have wanted is
    // ever pruned unseen.
    for round in 0..5 {
        fx.log
            .append_all(
                (0..10)
                    .map(|_| NewEvent::new(EventKind::NewMail).account(2))
                    .collect(),
            )
            .await
            .unwrap();
        fx.log.prune().await.unwrap();

        let page = fx
            .log
            .since_for_account(1, cursor, 100)
            .await
            .unwrap_or_else(|e| unreachable!("round {round} went out of range: {e}"));
        assert!(page.events.is_empty(), "account 1 has had no new events");
        assert!(
            page.next_seq > cursor,
            "round {round}: the cursor advances on what was scanned, not on \
             what matched"
        );
        cursor = page.next_seq;
    }

    // And when its next event finally arrives, the account still receives it.
    fx.log
        .append(NewEvent::new(EventKind::NewMail).account(1))
        .await
        .unwrap();
    let page = fx.log.since_for_account(1, cursor, 100).await.unwrap();
    assert_eq!(page.events.len(), 1);
}

#[tokio::test]
async fn concurrent_appends_publish_in_commit_order() {
    // A subscriber tracking the highest seq it has seen sets its cursor from
    // the live stream. If publish order were not commit order, an out-of-order
    // arrival would push the cursor past events that then became unreachable
    // from the durable read too, which is strictly-after.
    let fx = Fixture::open().await;
    let mut rx = fx.log.subscribe();

    let batches: Vec<_> = (0..8)
        .map(|i| {
            let log = fx.log.clone();
            tokio::spawn(async move {
                log.append_all(vec![
                    NewEvent::new(EventKind::NewMail).account(i),
                    NewEvent::new(EventKind::NewMail).account(i),
                ])
                .await
            })
        })
        .collect();
    for batch in batches {
        batch.await.unwrap().unwrap();
    }

    let mut published = Vec::new();
    while let Ok(event) = rx.try_recv() {
        published.push(event.seq);
    }
    assert_eq!(published.len(), 16, "every event reached the channel");
    let mut sorted = published.clone();
    sorted.sort_unstable();
    assert_eq!(
        published, sorted,
        "published in ascending seq, which is commit order: {published:?}"
    );
}

#[tokio::test]
async fn a_prune_racing_a_read_cannot_produce_a_silent_gap() {
    // Bounds and page must come from one snapshot. Split across two reads, a
    // prune landing in between lets the gap check pass against a floor that no
    // longer exists while the page skips everything just deleted.
    let fx = Fixture::with_retention(Retention {
        max_rows: Some(20),
        max_age: None,
    })
    .await;
    fx.fill(200).await;

    let reader = {
        let log = fx.log.clone();
        tokio::spawn(async move {
            let mut outcomes = Vec::new();
            for _ in 0..200 {
                outcomes.push(log.since(5, 100).await);
            }
            outcomes
        })
    };
    let pruner = {
        let log = fx.log.clone();
        tokio::spawn(async move {
            for _ in 0..50 {
                log.prune().await.unwrap();
                tokio::task::yield_now().await;
            }
        })
    };
    pruner.await.unwrap();

    for outcome in reader.await.unwrap() {
        match outcome {
            // Either the cursor was still inside the live range, in which case
            // the page must begin exactly where the cursor left off...
            Ok(page) => {
                if let Some(first) = page.events.first() {
                    assert_eq!(
                        first.seq, 6,
                        "a page that starts above the cursor is a silent gap"
                    );
                }
            }
            // ...or it had been pruned away, in which case the client is told.
            Err(error) => assert_eq!(error.reason(), ErrorReason::OutOfRange),
        }
    }
}

#[tokio::test]
async fn the_age_sweep_never_punches_a_hole_in_the_middle() {
    // `at` is not monotonic across a backwards clock step (an NTP correction
    // after boot, a restored VM snapshot). Deleting on `at` directly would
    // remove a row from the middle of the live range, and the contiguity the
    // entire gap contract rests on would silently be false.
    let fx = Fixture::with_retention(Retention {
        max_rows: None,
        max_age: Some(std::time::Duration::from_secs(3600)),
    })
    .await;
    fx.fill(6).await;

    // Event 4 got an old timestamp: the clock stepped backwards after it and
    // before 5 and 6.
    fx.db
        .write(|c| {
            c.execute(
                "UPDATE events SET at = unixepoch() - 7200 WHERE seq = 4",
                [],
            )
        })
        .await
        .unwrap();

    fx.log.prune().await.unwrap();

    let kept: Vec<i64> = fx
        .log
        .since(0, 100)
        .await
        .unwrap()
        .events
        .iter()
        .map(|e| e.seq)
        .collect();
    assert_eq!(
        kept,
        vec![5, 6],
        "the sweep resolved the horizon to a position and cut below it, \
         leaving the survivors contiguous rather than {{1,2,3,5,6}}"
    );
}

#[tokio::test]
async fn catch_up_leaves_no_window_between_the_backlog_and_the_live_tail() {
    // Drain-then-subscribe drops whatever committed in between. Every consumer
    // would otherwise have to rediscover this ordering, and the ones that got
    // it wrong would look fine until a busy mailbox proved otherwise.
    let fx = Fixture::open().await;
    fx.fill(5).await;

    let catchup = fx.log.catch_up(0, 100).await.unwrap();
    assert_eq!(catchup.backlog.len(), 5);
    assert_eq!(catchup.next_seq, 5);

    fx.log
        .append(NewEvent::new(EventKind::NewMail).account(1))
        .await
        .unwrap();

    let mut live = catchup.live;
    let event = tokio::time::timeout(std::time::Duration::from_secs(5), live.recv())
        .await
        .expect("the subscription was live before the backlog was read")
        .unwrap();
    assert_eq!(event.seq, 6);
    assert!(
        event.seq > catchup.next_seq,
        "and it is past the backlog, so nothing is processed twice"
    );
}

#[tokio::test]
async fn a_zero_row_retention_keeps_nothing_rather_than_everything() {
    // `Some(0)` reading as "unlimited" would turn a config typo into unbounded
    // disk growth, which is the opposite of what it says.
    let fx = Fixture::with_retention(Retention {
        max_rows: Some(0),
        max_age: None,
    })
    .await;
    fx.fill(10).await;

    assert_eq!(fx.log.prune().await.unwrap(), 10);
    assert!(fx.log.is_empty().await.unwrap());
}

#[tokio::test]
async fn an_oversized_payload_is_rejected_at_the_boundary() {
    // 1024 buffered channel slots of unbounded JSON is unbounded memory, and
    // payloads derive from IMAP data.
    let fx = Fixture::open().await;
    let huge = serde_json::json!({ "body": "x".repeat(MAX_PAYLOAD_BYTES + 1) });

    let err = fx
        .log
        .append(NewEvent::new(EventKind::NewMail).payload(huge))
        .await
        .expect_err("payloads are summaries, not message bodies");
    assert_eq!(err.reason(), ErrorReason::InvalidArgument);
    assert!(fx.log.is_empty().await.unwrap());
}

#[tokio::test]
async fn a_corrupt_row_fails_the_read_rather_than_returning_a_hollow_event() {
    let fx = Fixture::open().await;
    fx.fill(1).await;
    fx.db
        .write(|c| c.execute("UPDATE events SET payload = 'not json' WHERE seq = 1", []))
        .await
        .unwrap();

    assert!(
        fx.log.since(0, 10).await.is_err(),
        "substituting null would hand a consumer an event whose detail vanished"
    );
}
