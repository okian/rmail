//! The three properties the queue exists for: dedup against what is already
//! done, a lease that survives the worker holding it, and a poison job that
//! stays out of everyone else's way.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use super::*;
use crate::repo;
use crate::ErrorReason;

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Fixture {
    queue: IndexQueue,
    db: Database,
    messages: Vec<i64>,
    path: PathBuf,
}

impl Fixture {
    async fn open() -> Self {
        Self::with_options(QueueOptions::default()).await
    }

    async fn with_options(opts: QueueOptions) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-idxq-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", path.display())));
        }
        let db = Database::open(&path).unwrap();
        // Ten stored messages: jobs reference real rows, because the queue
        // deliberately refuses to schedule work for mail that is not there.
        let messages = db
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
                let mut ids = Vec::new();
                for uid in 1..=10 {
                    ids.push(repo::insert_message(
                        c,
                        &repo::NewMessage {
                            account_id,
                            mailbox_id,
                            uid,
                            uidvalidity: 1,
                            subject: Some(format!("Message {uid}")),
                            ..Default::default()
                        },
                    )?);
                }
                Ok(ids)
            })
            .await
            .unwrap();
        let queue = IndexQueue::new(db.clone(), opts);
        Self {
            queue,
            db,
            messages,
            path,
        }
    }

    fn message(&self, n: usize) -> i64 {
        self.messages[n]
    }

    /// Force a job's `next_attempt_at` into the past, standing in for the
    /// backoff having elapsed.
    async fn expire_backoff(&self, job_id: i64) {
        self.db
            .write(move |c| {
                c.execute(
                    "UPDATE index_queue SET next_attempt_at = 0 WHERE job_id = ?1",
                    [job_id],
                )
            })
            .await
            .unwrap();
    }

    /// Force a lease into the past, standing in for the worker having died.
    async fn expire_lease(&self, job_id: i64) {
        self.db
            .write(move |c| {
                c.execute(
                    "UPDATE index_queue SET lease_expires_at = 1 WHERE job_id = ?1",
                    [job_id],
                )
            })
            .await
            .unwrap();
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
async fn a_queued_job_is_leased_completed_and_recorded() {
    let fx = Fixture::open().await;
    let message_id = fx.message(0);

    let queued = fx
        .queue
        .enqueue(
            vec![NewJob::new(message_id, IndexKind::Extract).content_hash(*b"hash-a")],
            None,
        )
        .await
        .unwrap();
    assert_eq!(queued, 1);

    let leased = fx.queue.lease("worker-1", 10).await.unwrap();
    assert_eq!(leased.len(), 1);
    assert_eq!(leased[0].message_id, message_id);
    assert_eq!(leased[0].kind, IndexKind::Extract);
    assert_eq!(leased[0].content_hash.as_deref(), Some(&b"hash-a"[..]));
    assert_eq!(leased[0].attempts, 1, "the lease counts as an attempt");

    fx.queue.complete(&leased[0], None).await.unwrap();

    let stats = fx.queue.stats().await.unwrap();
    assert_eq!(stats.done, 1);
    assert_eq!(stats.outstanding(), 0);
}

#[tokio::test]
async fn re_enqueuing_unchanged_work_is_a_no_op() {
    // The common case on every restart: sync re-enqueues the world, and nearly
    // all of it is already indexed. If that cost a re-index rather than a
    // query, starting the daemon would re-index the mailbox.
    let fx = Fixture::open().await;
    let job = |id| NewJob::new(id, IndexKind::Lexical).content_hash(*b"same");

    fx.queue
        .enqueue(vec![job(fx.message(0))], None)
        .await
        .unwrap();
    let leased = fx.queue.lease("w", 10).await.unwrap();
    fx.queue.complete(&leased[0], None).await.unwrap();

    let queued = fx
        .queue
        .enqueue(vec![job(fx.message(0))], None)
        .await
        .unwrap();

    assert_eq!(queued, 0, "nothing changed, so nothing needs doing");
    assert!(fx.queue.lease("w", 10).await.unwrap().is_empty());
}

#[tokio::test]
async fn changed_content_re_enqueues() {
    let fx = Fixture::open().await;
    let message_id = fx.message(0);

    fx.queue
        .enqueue(
            vec![NewJob::new(message_id, IndexKind::Lexical).content_hash(*b"before")],
            None,
        )
        .await
        .unwrap();
    let leased = fx.queue.lease("w", 10).await.unwrap();
    fx.queue.complete(&leased[0], None).await.unwrap();

    let queued = fx
        .queue
        .enqueue(
            vec![NewJob::new(message_id, IndexKind::Lexical).content_hash(*b"after")],
            None,
        )
        .await
        .unwrap();

    assert_eq!(queued, 1);
    let leased = fx.queue.lease("w", 10).await.unwrap();
    assert_eq!(leased.len(), 1);
    assert_eq!(leased[0].content_hash.as_deref(), Some(&b"after"[..]));
    assert_eq!(
        leased[0].attempts, 1,
        "a re-queued job starts its attempts over rather than inheriting the \
         old job's history"
    );
}

#[tokio::test]
async fn a_changed_embedding_model_re_enqueues_unchanged_content() {
    // Content is identical; the vectors are not. A model switch has to re-embed
    // exactly the affected stages, and nothing about the message says so.
    let fx = Fixture::open().await;
    let message_id = fx.message(0);
    let job = || NewJob::new(message_id, IndexKind::Semantic).content_hash(*b"same");

    fx.queue
        .enqueue(vec![job()], Some("model-v1"))
        .await
        .unwrap();
    let leased = fx.queue.lease("w", 10).await.unwrap();
    fx.queue
        .complete(&leased[0], Some("model-v1"))
        .await
        .unwrap();

    assert_eq!(
        fx.queue
            .enqueue(vec![job()], Some("model-v1"))
            .await
            .unwrap(),
        0,
        "same model, same content"
    );
    assert_eq!(
        fx.queue
            .enqueue(vec![job()], Some("model-v2"))
            .await
            .unwrap(),
        1,
        "a different model makes the recorded vectors stale"
    );
}

#[tokio::test]
async fn stages_are_independent() {
    // The reason each stage is its own row: an embeddings provider being down
    // must not stop lexical search from being built.
    let fx = Fixture::open().await;
    let message_id = fx.message(0);
    fx.queue
        .enqueue(
            IndexKind::ALL
                .into_iter()
                .map(|kind| NewJob::new(message_id, kind))
                .collect(),
            None,
        )
        .await
        .unwrap();

    let leased = fx.queue.lease("w", 10).await.unwrap();
    assert_eq!(leased.len(), IndexKind::ALL.len());

    // Semantic fails; everything else completes.
    for lease in &leased {
        if lease.kind == IndexKind::Semantic {
            fx.queue
                .fail(lease, "embeddings unreachable")
                .await
                .unwrap();
        } else {
            fx.queue.complete(lease, None).await.unwrap();
        }
    }

    let stats = fx.queue.stats().await.unwrap();
    assert_eq!(stats.done, 4, "the other stages finished regardless");
    assert_eq!(stats.backing_off, 1);
}

#[tokio::test]
async fn one_outstanding_job_per_message_and_stage() {
    let fx = Fixture::open().await;
    let message_id = fx.message(0);
    let job = || NewJob::new(message_id, IndexKind::Extract);

    assert_eq!(
        fx.queue
            .enqueue(vec![job(), job(), job()], None)
            .await
            .unwrap(),
        1,
        "the count is queue rows, not inputs"
    );

    let leased = fx.queue.lease("w", 10).await.unwrap();
    assert_eq!(
        leased.len(),
        1,
        "three enqueues of the same work are one job, not three"
    );
}

#[tokio::test]
async fn a_more_urgent_enqueue_promotes_a_queued_job() {
    // A message the user just opened outranks the backfill entry that happened
    // to get there first.
    let fx = Fixture::open().await;
    fx.queue
        .enqueue(
            vec![
                NewJob::new(fx.message(0), IndexKind::Extract).priority(PRIORITY_BACKFILL),
                NewJob::new(fx.message(1), IndexKind::Extract).priority(PRIORITY_NORMAL),
            ],
            None,
        )
        .await
        .unwrap();
    fx.queue
        .enqueue(
            vec![NewJob::new(fx.message(0), IndexKind::Extract).priority(PRIORITY_RECENT)],
            None,
        )
        .await
        .unwrap();

    let leased = fx.queue.lease("w", 10).await.unwrap();
    assert_eq!(
        leased[0].message_id,
        fx.message(0),
        "the promoted job runs first"
    );
}

#[tokio::test]
async fn a_less_urgent_enqueue_does_not_demote() {
    let fx = Fixture::open().await;
    fx.queue
        .enqueue(
            vec![NewJob::new(fx.message(0), IndexKind::Extract).priority(PRIORITY_RECENT)],
            None,
        )
        .await
        .unwrap();
    fx.queue
        .enqueue(
            vec![
                NewJob::new(fx.message(0), IndexKind::Extract).priority(PRIORITY_BACKFILL),
                NewJob::new(fx.message(1), IndexKind::Extract).priority(PRIORITY_NORMAL),
            ],
            None,
        )
        .await
        .unwrap();

    let leased = fx.queue.lease("w", 10).await.unwrap();
    assert_eq!(
        leased[0].message_id,
        fx.message(0),
        "a backfill sweep must not push a message the user is looking at to \
         the back of the queue"
    );
}

#[tokio::test]
async fn recent_mail_is_leased_before_the_backlog() {
    let fx = Fixture::open().await;
    fx.queue
        .enqueue(
            vec![
                NewJob::new(fx.message(0), IndexKind::Extract).priority(PRIORITY_BACKFILL),
                NewJob::new(fx.message(1), IndexKind::Extract).priority(PRIORITY_BACKFILL),
                NewJob::new(fx.message(2), IndexKind::Extract).priority(PRIORITY_RECENT),
            ],
            None,
        )
        .await
        .unwrap();

    let leased = fx.queue.lease("w", 1).await.unwrap();
    assert_eq!(leased[0].message_id, fx.message(2));
}

#[tokio::test]
async fn a_leased_job_is_not_leased_again() {
    // Two workers polling at the same moment must not both take the same job.
    let fx = Fixture::open().await;
    fx.queue
        .enqueue(vec![NewJob::new(fx.message(0), IndexKind::Extract)], None)
        .await
        .unwrap();

    let first = fx.queue.lease("worker-1", 10).await.unwrap();
    let second = fx.queue.lease("worker-2", 10).await.unwrap();

    assert_eq!(first.len(), 1);
    assert!(second.is_empty(), "the job is already owned");
}

#[tokio::test]
async fn an_expired_lease_returns_the_job_to_the_queue() {
    // The whole recovery story: a worker that died mid-job left its lease in
    // the past, and nothing else knows the job exists.
    let fx = Fixture::open().await;
    fx.queue
        .enqueue(vec![NewJob::new(fx.message(0), IndexKind::Extract)], None)
        .await
        .unwrap();
    let leased = fx.queue.lease("doomed-worker", 10).await.unwrap();
    assert!(fx.queue.lease("other", 10).await.unwrap().is_empty());

    fx.expire_lease(leased[0].job_id).await;
    assert_eq!(fx.queue.reap_expired().await.unwrap(), 1);

    let reclaimed = fx.queue.lease("other", 10).await.unwrap();
    assert_eq!(reclaimed.len(), 1);
    assert_eq!(reclaimed[0].job_id, leased[0].job_id);
    assert_eq!(
        reclaimed[0].attempts, 2,
        "the attempt is not rolled back — a job that keeps killing its worker \
         is exactly the kind that should eventually be quarantined"
    );
}

#[tokio::test]
async fn reaping_does_not_disturb_a_live_lease() {
    let fx = Fixture::open().await;
    fx.queue
        .enqueue(vec![NewJob::new(fx.message(0), IndexKind::Extract)], None)
        .await
        .unwrap();
    fx.queue.lease("worker", 10).await.unwrap();

    assert_eq!(fx.queue.reap_expired().await.unwrap(), 0);
    assert_eq!(fx.queue.stats().await.unwrap().leased, 1);
}

#[tokio::test]
async fn a_failing_job_backs_off_before_it_is_retried() {
    let fx = Fixture::open().await;
    fx.queue
        .enqueue(vec![NewJob::new(fx.message(0), IndexKind::Extract)], None)
        .await
        .unwrap();
    let leased = fx.queue.lease("w", 10).await.unwrap();

    let outcome = fx.queue.fail(&leased[0], "transient").await.unwrap();

    match outcome {
        Some(Failure::Retrying { attempts, .. }) => assert_eq!(attempts, 1),
        other => unreachable!("one failure is not poison: {other:?}"),
    }
    assert!(
        fx.queue.lease("w", 10).await.unwrap().is_empty(),
        "it is invisible until the backoff elapses"
    );
    assert_eq!(fx.queue.stats().await.unwrap().backing_off, 1);

    fx.expire_backoff(leased[0].job_id).await;
    assert_eq!(fx.queue.lease("w", 10).await.unwrap().len(), 1);
}

#[tokio::test]
async fn a_poison_job_is_quarantined_and_stops_being_leased() {
    let fx = Fixture::with_options(QueueOptions {
        max_attempts: 3,
        ..QueueOptions::default()
    })
    .await;
    fx.queue
        .enqueue(vec![NewJob::new(fx.message(0), IndexKind::Extract)], None)
        .await
        .unwrap();

    let mut last = None;
    for attempt in 1..=3 {
        let leased = fx.queue.lease("w", 10).await.unwrap();
        assert_eq!(leased.len(), 1, "attempt {attempt} should be leasable");
        let job_id = leased[0].job_id;
        last = fx.queue.fail(&leased[0], "always broken").await.unwrap();
        fx.expire_backoff(job_id).await;
    }

    assert!(
        matches!(last, Some(Failure::Quarantined { attempts: 3 })),
        "got {last:?}"
    );
    assert!(
        fx.queue.lease("w", 10).await.unwrap().is_empty(),
        "a quarantined job is never leased again"
    );
    assert_eq!(fx.queue.stats().await.unwrap().dead, 1);

    let dead = fx.queue.dead_letters(10).await.unwrap();
    assert_eq!(dead.len(), 1);
    assert_eq!(dead[0].kind, IndexKind::Extract);
    assert_eq!(
        dead[0].last_error.as_deref(),
        Some("always broken"),
        "the failure is kept, not dropped"
    );
}

#[tokio::test]
async fn a_poison_job_does_not_block_the_queue_behind_it() {
    // The property the ready-set index exists for. One message with an
    // unparsable attachment must not stop a mailbox from being indexed.
    let fx = Fixture::with_options(QueueOptions {
        max_attempts: 1,
        ..QueueOptions::default()
    })
    .await;
    // The poison job is enqueued first and at the most urgent priority, so
    // anything that head-of-line blocks would block on it.
    fx.queue
        .enqueue(
            vec![NewJob::new(fx.message(0), IndexKind::Extract).priority(PRIORITY_RECENT)],
            None,
        )
        .await
        .unwrap();
    let poison = fx.queue.lease("w", 10).await.unwrap();
    fx.queue.fail(&poison[0], "unparsable").await.unwrap();
    assert_eq!(fx.queue.stats().await.unwrap().dead, 1);

    fx.queue
        .enqueue(
            (1..5)
                .map(|i| NewJob::new(fx.message(i), IndexKind::Extract))
                .collect(),
            None,
        )
        .await
        .unwrap();

    let leased = fx.queue.lease("w", 10).await.unwrap();
    assert_eq!(leased.len(), 4, "the rest of the queue drains normally");
    assert!(leased.iter().all(|l| l.message_id != fx.message(0)));
}

#[tokio::test]
async fn a_quarantined_job_can_be_revived() {
    // A job quarantined by a bug that has since been fixed should not need the
    // message re-synced to be retried.
    let fx = Fixture::with_options(QueueOptions {
        max_attempts: 1,
        ..QueueOptions::default()
    })
    .await;
    fx.queue
        .enqueue(vec![NewJob::new(fx.message(0), IndexKind::Extract)], None)
        .await
        .unwrap();
    let leased = fx.queue.lease("w", 10).await.unwrap();
    fx.queue.fail(&leased[0], "was a bug").await.unwrap();

    assert!(fx.queue.revive(leased[0].job_id).await.unwrap());

    let revived = fx.queue.lease("w", 10).await.unwrap();
    assert_eq!(revived.len(), 1);
    assert_eq!(revived[0].attempts, 1, "its history is cleared");
    assert!(
        !fx.queue.revive(leased[0].job_id).await.unwrap(),
        "reviving a job that is not quarantined does nothing"
    );
}

#[tokio::test]
async fn a_lease_that_lapses_on_the_final_attempt_is_quarantined_not_relooped() {
    // Otherwise a job that reliably kills its worker cycles forever: the reaper
    // returns it, the worker dies, the reaper returns it again.
    let fx = Fixture::with_options(QueueOptions {
        max_attempts: 1,
        ..QueueOptions::default()
    })
    .await;
    fx.queue
        .enqueue(vec![NewJob::new(fx.message(0), IndexKind::Extract)], None)
        .await
        .unwrap();
    let leased = fx.queue.lease("doomed", 10).await.unwrap();
    fx.expire_lease(leased[0].job_id).await;

    fx.queue.reap_expired().await.unwrap();

    assert_eq!(fx.queue.stats().await.unwrap().dead, 1);
    assert!(fx.queue.lease("w", 10).await.unwrap().is_empty());
}

#[tokio::test]
async fn work_for_a_message_that_no_longer_exists_is_skipped() {
    // Sync and indexing race; a message deleted between the two is not an
    // error, and the foreign key would reject the insert anyway.
    let fx = Fixture::open().await;
    let queued = fx
        .queue
        .enqueue(vec![NewJob::new(9_999, IndexKind::Extract)], None)
        .await
        .unwrap();
    assert_eq!(queued, 0);
    assert_eq!(fx.queue.stats().await.unwrap().outstanding(), 0);
}

#[tokio::test]
async fn deleting_a_message_takes_its_queued_work_with_it() {
    let fx = Fixture::open().await;
    let message_id = fx.message(0);
    fx.queue
        .enqueue(vec![NewJob::new(message_id, IndexKind::Extract)], None)
        .await
        .unwrap();

    fx.db
        .write(move |c| c.execute("DELETE FROM messages WHERE id = ?1", [message_id]))
        .await
        .unwrap();

    assert_eq!(
        fx.queue.stats().await.unwrap().outstanding(),
        0,
        "an orphaned job would be leased forever and never succeed"
    );
}

#[tokio::test]
async fn failing_a_job_this_worker_does_not_hold_is_refused() {
    let fx = Fixture::open().await;
    let ghost = Lease {
        job_id: 9_999,
        message_id: fx.message(0),
        kind: IndexKind::Extract,
        content_hash: None,
        attempts: 1,
        lease_expires_at: i64::MAX,
        worker: "w".to_owned(),
    };
    assert_eq!(
        fx.queue.fail(&ghost, "whatever").await.unwrap(),
        None,
        "a job that is not there cannot be failed"
    );
}

#[tokio::test]
async fn completing_a_job_this_worker_does_not_hold_is_refused() {
    // A worker holding a lease for a job whose message was deleted underneath
    // it should not fail — but it must not report success either, because
    // success is what writes `index_state`.
    let fx = Fixture::open().await;
    let ghost = Lease {
        job_id: 9_999,
        message_id: fx.message(0),
        kind: IndexKind::Extract,
        content_hash: None,
        attempts: 1,
        lease_expires_at: i64::MAX,
        worker: "w".to_owned(),
    };
    assert!(!fx.queue.complete(&ghost, None).await.unwrap());
}

#[tokio::test]
async fn an_empty_enqueue_and_a_zero_lease_are_no_ops() {
    let fx = Fixture::open().await;
    assert_eq!(fx.queue.enqueue(Vec::new(), None).await.unwrap(), 0);
    assert!(fx.queue.lease("w", 0).await.unwrap().is_empty());
    assert!(fx.queue.lease("w", -1).await.unwrap().is_empty());
}

#[tokio::test]
async fn the_queue_survives_being_reopened() {
    // Durability is the whole reason this is in SQLite: the first index of a
    // large mailbox is hours of work on a laptop that will be closed.
    let fx = Fixture::open().await;
    fx.queue
        .enqueue(
            (0..5)
                .map(|i| NewJob::new(fx.message(i), IndexKind::Extract))
                .collect(),
            None,
        )
        .await
        .unwrap();
    let leased = fx.queue.lease("worker-that-dies", 2).await.unwrap();
    assert_eq!(leased.len(), 2);

    // A fresh queue over the same database, as a restarted daemon would build.
    let restarted = IndexQueue::new(fx.db.clone(), QueueOptions::default());
    assert_eq!(restarted.stats().await.unwrap().outstanding(), 5);

    for lease in &leased {
        fx.expire_lease(lease.job_id).await;
    }
    assert_eq!(restarted.reap_expired().await.unwrap(), 2);
    assert_eq!(restarted.lease("new-worker", 10).await.unwrap().len(), 5);
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

#[test]
fn backoff_doubles_and_is_capped() {
    let opts = QueueOptions {
        backoff: Duration::from_secs(10),
        max_backoff: Duration::from_secs(60),
        ..QueueOptions::default()
    };
    assert_eq!(opts.backoff_for(1), Duration::from_secs(10));
    assert_eq!(opts.backoff_for(2), Duration::from_secs(20));
    assert_eq!(opts.backoff_for(3), Duration::from_secs(40));
    assert_eq!(opts.backoff_for(4), Duration::from_secs(60), "capped");
    assert_eq!(
        opts.backoff_for(1_000),
        Duration::from_secs(60),
        "a large attempt count must not overflow the shift"
    );
    assert_eq!(opts.backoff_for(0), Duration::from_secs(10), "clamped");
}

#[test]
fn index_kinds_round_trip_through_their_wire_strings() {
    for kind in IndexKind::ALL {
        assert_eq!(IndexKind::parse(kind.as_str()).unwrap(), kind);
    }
    assert_eq!(
        IndexKind::parse("nope").unwrap_err().reason(),
        ErrorReason::Internal,
        "a kind this build never wrote means the queue came from a newer one"
    );
}

#[test]
fn wire_strings_are_stable() {
    // Stored in the queue and in index_state; changing one silently invalidates
    // every recorded row.
    assert_eq!(IndexKind::Extract.as_str(), "extract");
    assert_eq!(IndexKind::Lexical.as_str(), "lexical");
    assert_eq!(IndexKind::Entities.as_str(), "entities");
    assert_eq!(IndexKind::Semantic.as_str(), "semantic");
    assert_eq!(IndexKind::Thread.as_str(), "thread");
    assert_eq!(JobState::Pending.as_str(), "pending");
    assert_eq!(JobState::Leased.as_str(), "leased");
    assert_eq!(JobState::Done.as_str(), "done");
    assert_eq!(JobState::Dead.as_str(), "dead");
    for state in [
        JobState::Pending,
        JobState::Leased,
        JobState::Done,
        JobState::Dead,
    ] {
        assert_eq!(JobState::parse(state.as_str()).unwrap(), state);
    }
}

// ---------------------------------------------------------------------------
// The cells a happy-path suite leaves empty: a job whose ownership changed
// underneath the worker holding it.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_sweep_during_a_lease_does_not_mark_the_new_content_indexed() {
    // The worst failure this queue can have, and it is silent. A sync sweep
    // rewrites a leased row with new content; the worker — still indexing the
    // *old* content — finishes. If completion read the hash back from the row
    // it would record the new one as indexed, and every later enqueue of that
    // content would dedup to nothing. The message becomes permanently
    // unindexable with no error anywhere.
    let fx = Fixture::open().await;
    let message_id = fx.message(0);

    fx.queue
        .enqueue(
            vec![NewJob::new(message_id, IndexKind::Lexical).content_hash(*b"v1")],
            None,
        )
        .await
        .unwrap();
    let leased = fx.queue.lease("worker", 10).await.unwrap();
    assert_eq!(leased[0].content_hash.as_deref(), Some(&b"v1"[..]));

    // The message changes while the worker is running.
    fx.queue
        .enqueue(
            vec![NewJob::new(message_id, IndexKind::Lexical).content_hash(*b"v2")],
            None,
        )
        .await
        .unwrap();

    // The worker finishes the work it actually did.
    let held = fx.queue.complete(&leased[0], None).await.unwrap();
    assert!(
        !held,
        "its row was taken over by the sweep, so the completion does not apply"
    );

    // v2 is still outstanding, and re-enqueuing it does not dedup away.
    let pending = fx.queue.lease("worker-2", 10).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(
        pending[0].content_hash.as_deref(),
        Some(&b"v2"[..]),
        "the new content is still there to be indexed"
    );
}

#[tokio::test]
async fn a_completion_records_what_the_worker_indexed_not_what_the_row_says() {
    // Same hazard from the other side: even when the lease still holds, the
    // recorded hash must be the one the worker was given.
    let fx = Fixture::open().await;
    let message_id = fx.message(0);
    fx.queue
        .enqueue(
            vec![NewJob::new(message_id, IndexKind::Lexical).content_hash(*b"v1")],
            None,
        )
        .await
        .unwrap();
    let leased = fx.queue.lease("worker", 10).await.unwrap();
    assert!(fx.queue.complete(&leased[0], None).await.unwrap());

    // v1 is recorded, so re-enqueuing v1 dedups...
    assert_eq!(
        fx.queue
            .enqueue(
                vec![NewJob::new(message_id, IndexKind::Lexical).content_hash(*b"v1")],
                None
            )
            .await
            .unwrap(),
        0
    );
    // ...and v2 does not.
    assert_eq!(
        fx.queue
            .enqueue(
                vec![NewJob::new(message_id, IndexKind::Lexical).content_hash(*b"v2")],
                None
            )
            .await
            .unwrap(),
        1
    );
}

#[tokio::test]
async fn a_sweep_does_not_un_quarantine_a_poison_job() {
    // A dead job never wrote `index_state`, so the dedup can never
    // short-circuit it. Without an explicit guard, every sync sweep revives
    // every poison job, which then burns its attempts again — forever.
    let fx = Fixture::with_options(QueueOptions {
        max_attempts: 1,
        ..QueueOptions::default()
    })
    .await;
    let message_id = fx.message(0);
    let job = || NewJob::new(message_id, IndexKind::Extract).content_hash(*b"broken");

    fx.queue.enqueue(vec![job()], None).await.unwrap();
    let leased = fx.queue.lease("w", 10).await.unwrap();
    fx.queue.fail(&leased[0], "unparsable").await.unwrap();
    assert_eq!(fx.queue.stats().await.unwrap().dead, 1);

    // The sweep runs again over the same, unchanged message.
    let queued = fx.queue.enqueue(vec![job()], None).await.unwrap();

    assert_eq!(queued, 0, "the sweep left the quarantine alone");
    assert_eq!(fx.queue.stats().await.unwrap().dead, 1);
    assert!(fx.queue.lease("w", 10).await.unwrap().is_empty());
}

#[tokio::test]
async fn changed_content_does_earn_a_quarantined_job_a_fresh_attempt() {
    // The quarantine was for the content that poisoned it. New content is a
    // different question, and a message the user just edited should not be
    // permanently unindexable because an older version broke the extractor.
    let fx = Fixture::with_options(QueueOptions {
        max_attempts: 1,
        ..QueueOptions::default()
    })
    .await;
    let message_id = fx.message(0);
    fx.queue
        .enqueue(
            vec![NewJob::new(message_id, IndexKind::Extract).content_hash(*b"broken")],
            None,
        )
        .await
        .unwrap();
    let leased = fx.queue.lease("w", 10).await.unwrap();
    fx.queue.fail(&leased[0], "unparsable").await.unwrap();

    let queued = fx
        .queue
        .enqueue(
            vec![NewJob::new(message_id, IndexKind::Extract).content_hash(*b"fixed")],
            None,
        )
        .await
        .unwrap();

    assert_eq!(queued, 1);
    let retried = fx.queue.lease("w", 10).await.unwrap();
    assert_eq!(retried.len(), 1);
    assert_eq!(retried[0].content_hash.as_deref(), Some(&b"fixed"[..]));
}

#[tokio::test]
async fn a_reaped_worker_cannot_complete_the_job_out_from_under_its_new_owner() {
    let fx = Fixture::open().await;
    fx.queue
        .enqueue(vec![NewJob::new(fx.message(0), IndexKind::Extract)], None)
        .await
        .unwrap();
    let stalled = fx.queue.lease("stalled-worker", 10).await.unwrap();

    fx.expire_lease(stalled[0].job_id).await;
    fx.queue.reap_expired().await.unwrap();
    let new_owner = fx.queue.lease("fresh-worker", 10).await.unwrap();
    assert_eq!(new_owner.len(), 1);

    // The stalled worker finally finishes — but the job is not its any more.
    assert!(
        !fx.queue.complete(&stalled[0], None).await.unwrap(),
        "writing index_state here would record work under whoever holds the \
         job now"
    );
    assert_eq!(fx.queue.stats().await.unwrap().leased, 1);

    // The real owner still can.
    assert!(fx.queue.complete(&new_owner[0], None).await.unwrap());
}

#[tokio::test]
async fn a_stale_failure_does_not_back_off_a_freshly_queued_job() {
    // An unfenced failure applies a backoff and an attempt to whatever occupies
    // that row now — which for a message the user just opened means it sits out
    // a delay it never earned.
    let fx = Fixture::open().await;
    let message_id = fx.message(0);
    fx.queue
        .enqueue(vec![NewJob::new(message_id, IndexKind::Extract)], None)
        .await
        .unwrap();
    let stalled = fx.queue.lease("stalled-worker", 10).await.unwrap();

    fx.expire_lease(stalled[0].job_id).await;
    fx.queue.reap_expired().await.unwrap();
    // The message is re-enqueued at the most urgent priority.
    fx.queue
        .enqueue(
            vec![NewJob::new(message_id, IndexKind::Extract).priority(PRIORITY_RECENT)],
            None,
        )
        .await
        .unwrap();

    assert_eq!(
        fx.queue.fail(&stalled[0], "too late").await.unwrap(),
        None,
        "the stalled worker no longer holds this row"
    );

    let stats = fx.queue.stats().await.unwrap();
    assert_eq!(stats.ready, 1, "still leasable right now");
    assert_eq!(stats.backing_off, 0);
}

#[tokio::test]
async fn a_stage_without_a_model_does_not_churn_when_one_is_configured() {
    // A lexical worker has no embedding model and completes with none. If the
    // re-index decision compared a configured model against it anyway, every
    // non-embedding stage would re-enqueue on every restart, for every message.
    let fx = Fixture::open().await;
    let message_id = fx.message(0);
    let job = |kind| NewJob::new(message_id, kind).content_hash(*b"same");

    for kind in IndexKind::ALL {
        fx.queue
            .enqueue(vec![job(kind)], Some("embed-v1"))
            .await
            .unwrap();
    }
    for lease in fx.queue.lease("w", 10).await.unwrap() {
        // Each worker completes with the model it actually used: none, except
        // the embedding one.
        let model = lease.kind.uses_model().then_some("embed-v1");
        assert!(fx.queue.complete(&lease, model).await.unwrap());
    }

    let requeued = fx
        .queue
        .enqueue(
            IndexKind::ALL.into_iter().map(job).collect(),
            Some("embed-v1"),
        )
        .await
        .unwrap();
    assert_eq!(requeued, 0, "nothing changed, so nothing re-runs");

    // A model switch re-runs the embedding stage and only that one.
    let requeued = fx
        .queue
        .enqueue(
            IndexKind::ALL.into_iter().map(job).collect(),
            Some("embed-v2"),
        )
        .await
        .unwrap();
    assert_eq!(requeued, 1);
    let leased = fx.queue.lease("w", 10).await.unwrap();
    assert_eq!(leased.len(), 1);
    assert_eq!(leased[0].kind, IndexKind::Semantic);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_workers_never_take_the_same_job_and_never_lose_one() {
    // The claim the whole worker pool rests on, tested the only way it can be:
    // several real tasks draining a real queue at once.
    let fx = Fixture::with_options(QueueOptions::default()).await;
    // More jobs than messages: every stage of every message.
    let jobs: Vec<NewJob> = fx
        .messages
        .iter()
        .flat_map(|id| {
            IndexKind::ALL
                .into_iter()
                .map(move |kind| NewJob::new(*id, kind))
        })
        .collect();
    let total = jobs.len();
    fx.queue.enqueue(jobs, None).await.unwrap();

    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut workers = Vec::new();
    for n in 0..8 {
        let queue = fx.queue.clone();
        let seen = std::sync::Arc::clone(&seen);
        workers.push(tokio::spawn(async move {
            let name = format!("worker-{n}");
            loop {
                let leased = queue.lease(&name, 3).await.unwrap();
                if leased.is_empty() {
                    return;
                }
                for lease in leased {
                    seen.lock().unwrap().push(lease.job_id);
                    assert!(queue.complete(&lease, None).await.unwrap());
                }
            }
        }));
    }
    for worker in workers {
        worker.await.unwrap();
    }

    let mut seen = seen.lock().unwrap().clone();
    let before = seen.len();
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(before, seen.len(), "a job was leased twice");
    assert_eq!(seen.len(), total, "a job was lost");
    assert_eq!(fx.queue.stats().await.unwrap().outstanding(), 0);
}

#[tokio::test]
async fn a_corrupt_kind_surfaces_rather_than_being_leased() {
    // A kind this build cannot parse means the queue was written by a newer
    // one. Leasing it and failing later would look like a poison message.
    let fx = Fixture::open().await;
    let message_id = fx.message(0);
    fx.db
        .write(move |c| {
            c.execute(
                "INSERT INTO index_queue (message_id, kind, priority, state)
                 VALUES (?1, 'from-the-future', 1, 'pending')",
                [message_id],
            )
        })
        .await
        .unwrap();

    assert_eq!(
        fx.queue.lease("w", 10).await.unwrap_err().reason(),
        ErrorReason::Internal
    );
}

#[tokio::test]
async fn a_corrupt_state_is_not_silently_dropped_from_the_count() {
    // A queue that looks drained while work sits in it is the worst answer
    // available — worse than an error, because nobody goes looking.
    let fx = Fixture::open().await;
    let message_id = fx.message(0);
    fx.db
        .write(move |c| {
            c.execute(
                "INSERT INTO index_queue (message_id, kind, priority, state)
                 VALUES (?1, 'extract', 1, 'also-from-the-future')",
                [message_id],
            )
        })
        .await
        .unwrap();

    assert_eq!(
        fx.queue.stats().await.unwrap_err().reason(),
        ErrorReason::Internal
    );
}
