//! What the worker owes the queue: every stage actually run, a stage that is
//! switched off retired rather than faked, a poison job that does not take its
//! batch with it, and a cancelled drain that hands its leases back instead of
//! stranding them.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use super::*;
use crate::config::{Bm25Weights, IndexSemanticConfig};
use crate::embed::hash::HashEmbedder;
use crate::embed::{Embedder, Embedding};
use crate::events::{EventLog, NewEvent, Retention};
use crate::index::queue::QueueOptions;
use crate::index::semantic::VECTOR_DIM;
use crate::repo;

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// An embedder that sleeps, so a drain has a window to be cancelled in.
#[derive(Debug)]
struct SlowEmbedder {
    inner: HashEmbedder,
    delay: Duration,
}

#[async_trait::async_trait]
impl Embedder for SlowEmbedder {
    fn model(&self) -> &str {
        "slow-test-model"
    }
    fn dim(&self) -> usize {
        VECTOR_DIM
    }
    async fn embed(&self, texts: &[String]) -> Result<Vec<Embedding>, Error> {
        tokio::time::sleep(self.delay).await;
        self.inner.embed(texts).await
    }
}

/// An embedder of a width `vec_chunks` cannot hold — the cheapest way to make
/// one stage fail for real, through its own guard, rather than by faking a
/// queue row.
#[derive(Debug)]
struct NarrowEmbedder;

#[async_trait::async_trait]
impl Embedder for NarrowEmbedder {
    fn model(&self) -> &str {
        "narrow-test-model"
    }
    fn dim(&self) -> usize {
        8
    }
    async fn embed(&self, texts: &[String]) -> Result<Vec<Embedding>, Error> {
        Ok(texts.iter().map(|_| Embedding::new(vec![1.0; 8])).collect())
    }
}

struct Fixture {
    db: Database,
    queue: IndexQueue,
    account_id: i64,
    mailbox_id: i64,
    next_uid: std::cell::Cell<i64>,
    path: PathBuf,
}

impl Fixture {
    async fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-pipeline-{pid}-{n}.db"));
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
        Self {
            queue: IndexQueue::new(db.clone(), QueueOptions::default()),
            db,
            account_id,
            mailbox_id,
            next_uid: std::cell::Cell::new(1),
            path,
        }
    }

    async fn message(&self, subject: &str, body: &str) -> i64 {
        let uid = self.next_uid.get();
        self.next_uid.set(uid + 1);
        let (account_id, mailbox_id) = (self.account_id, self.mailbox_id);
        let (subject, body) = (subject.to_owned(), body.to_owned());
        self.db
            .write(move |c| {
                repo::insert_message(
                    c,
                    &repo::NewMessage {
                        account_id,
                        mailbox_id,
                        uid,
                        uidvalidity: 1,
                        subject: Some(subject),
                        from_addr: Some("ada@example.com".to_owned()),
                        body_text: Some(body),
                        date: Some(1_700_000_000 + uid),
                        ..Default::default()
                    },
                )
            })
            .await
            .unwrap()
    }

    fn pipeline_with(&self, embedder: Arc<dyn Embedder>, config: &IndexConfig) -> IndexPipeline {
        IndexPipeline::new(
            self.db.clone(),
            self.queue.clone(),
            FtsIndex::new(self.db.clone(), Bm25Weights::default()),
            SemanticIndex::new(
                self.db.clone(),
                embedder,
                &IndexSemanticConfig {
                    chunk_tokens: 32,
                    chunk_overlap: 4,
                    ..IndexSemanticConfig::default()
                },
            ),
            config,
        )
        .with_worker(format!("test-{}", self.next_uid.get()))
    }

    fn pipeline(&self) -> IndexPipeline {
        self.pipeline_with(
            Arc::new(HashEmbedder::new(VECTOR_DIM)),
            &IndexConfig::default(),
        )
    }

    fn count(&self, table: &str) -> i64 {
        let sql = format!("SELECT count(*) FROM {table}");
        self.db
            .with_read(move |c| c.query_row(&sql, [], |r| r.get(0)))
            .unwrap()
    }

    fn state_kinds(&self, message_id: i64) -> Vec<String> {
        self.db
            .with_read(move |c| {
                let mut stmt =
                    c.prepare("SELECT kind FROM index_state WHERE message_id = ?1 ORDER BY kind")?;
                let rows = stmt.query_map([message_id], |r| r.get(0))?.collect();
                rows
            })
            .unwrap()
    }

    /// Drain everything, with a generous bound so a bug cannot hang the suite.
    async fn drain(&self, pipeline: &IndexPipeline) -> DrainReport {
        let cancel = CancellationToken::new();
        let mut total = DrainReport::default();
        let mut drained = false;
        for _ in 0..200 {
            let batch = pipeline.run_once(32, &cancel).await.unwrap();
            let leased = batch.leased;
            total.merge(batch);
            if leased == 0 {
                drained = true;
                break;
            }
        }
        assert!(drained, "the queue never drained");
        total
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
async fn a_drain_runs_every_stage_and_records_what_it_indexed() {
    let fx = Fixture::open().await;
    let message_id = fx
        .message(
            "Invoice INV-2024-0231",
            "Please pay invoice INV-2024-0231 for £1,299.00 by Friday.",
        )
        .await;
    fx.queue
        .enqueue(vec![NewJob::new(message_id, IndexKind::Extract)], None)
        .await
        .unwrap();

    let pipeline = fx.pipeline();
    let report = fx.drain(&pipeline).await;

    assert_eq!(report.failed, 0, "no stage should have failed: {report:?}");
    assert_eq!(
        fx.state_kinds(message_id),
        vec!["entities", "extract", "lexical", "semantic"],
        "every stage records what it indexed, so coverage has something to count"
    );
    assert_eq!(fx.count("fts_messages"), 1);
    assert!(fx.count("chunks") > 0);
    assert!(fx.count("entities") > 0);
    assert_eq!(
        pipeline.jobs_run(),
        report.retired() + report.failed,
        "the counter and the report agree about how much work happened"
    );
}

#[tokio::test]
async fn a_stage_that_is_switched_off_is_retired_rather_than_recorded_as_indexed() {
    // The coverage-lies bug in its natural habitat: `extract_message` enqueues
    // the semantic stage whatever the config says, so a daemon with embeddings
    // off *will* be handed semantic jobs. Completing them would make
    // `mail index status` report 100% semantic coverage on a mailbox with no
    // vectors in it at all.
    let fx = Fixture::open().await;
    let mut config = IndexConfig::default();
    config.semantic.enabled = false;
    let message_id = fx
        .message("Hello", "A body worth indexing lexically.")
        .await;
    fx.queue
        .enqueue(vec![NewJob::new(message_id, IndexKind::Extract)], None)
        .await
        .unwrap();

    let pipeline = fx.pipeline_with(Arc::new(HashEmbedder::new(VECTOR_DIM)), &config);
    let report = fx.drain(&pipeline).await;

    assert!(report.discarded >= 1, "the semantic job was retired");
    assert_eq!(
        fx.state_kinds(message_id),
        vec!["entities", "extract", "lexical"],
        "no semantic state row, so coverage reports the stage as unindexed"
    );
    assert_eq!(fx.count("chunks"), 0, "and nothing was embedded");
    assert_eq!(
        fx.queue.stats().await.unwrap().outstanding(),
        0,
        "the job left the queue rather than accumulating forever"
    );
}

#[tokio::test]
async fn a_failing_stage_backs_off_without_taking_its_batch_with_it() {
    let fx = Fixture::open().await;
    let message_id = fx
        .message("Subject", "Body text for the lexical index.")
        .await;
    fx.queue
        .enqueue(vec![NewJob::new(message_id, IndexKind::Extract)], None)
        .await
        .unwrap();

    // The semantic stage refuses a model of the wrong width before it writes
    // anything; every other stage is unaffected.
    let pipeline = fx.pipeline_with(Arc::new(NarrowEmbedder), &IndexConfig::default());
    let cancel = CancellationToken::new();
    pipeline.run_once(32, &cancel).await.unwrap();
    let second = pipeline.run_once(32, &cancel).await.unwrap();

    assert_eq!(second.failed, 1, "the semantic job failed: {second:?}");
    assert!(
        fx.state_kinds(message_id).contains(&"lexical".to_owned()),
        "the lexical stage in the same batch still ran"
    );
    let stats = fx.queue.stats().await.unwrap();
    assert_eq!(
        stats.backing_off, 1,
        "the failure backed the job off rather than quarantining it on the first try"
    );
    assert_eq!(stats.dead, 0);
}

#[tokio::test]
async fn an_already_cancelled_drain_leases_nothing() {
    let fx = Fixture::open().await;
    let message_id = fx.message("Subject", "Body").await;
    fx.queue
        .enqueue(vec![NewJob::new(message_id, IndexKind::Extract)], None)
        .await
        .unwrap();

    let cancel = CancellationToken::new();
    cancel.cancel();
    let report = fx.pipeline().run_once(32, &cancel).await.unwrap();

    assert_eq!(report, DrainReport::default(), "nothing was even leased");
    assert_eq!(
        fx.queue.stats().await.unwrap().ready,
        1,
        "the job is intact"
    );
}

#[tokio::test]
async fn cancelling_mid_batch_hands_the_unrun_leases_straight_back() {
    let fx = Fixture::open().await;
    // Extract first, so every message has text and the semantic jobs the
    // cancellation lands in the middle of are real work.
    let mut ids = Vec::new();
    for n in 0..6 {
        let id = fx
            .message(&format!("Subject {n}"), "Body text to chunk and embed.")
            .await;
        ids.push(id);
        fx.queue
            .enqueue(vec![NewJob::new(id, IndexKind::Extract)], None)
            .await
            .unwrap();
    }
    let fast = fx.pipeline();
    fx.drain(&fast).await;

    // Now make the semantic stage slow and re-enqueue it for every message.
    // The state row has to go first, or the enqueue below dedups against it —
    // which is exactly what it is for, and exactly what would leave this test
    // measuring an empty batch.
    fx.db
        .write(|c| c.execute("DELETE FROM index_state WHERE kind = 'semantic'", []))
        .await
        .unwrap();
    let jobs: Vec<NewJob> = ids
        .iter()
        .map(|id| NewJob::new(*id, IndexKind::Semantic))
        .collect();
    assert_eq!(fx.queue.enqueue(jobs, None).await.unwrap(), 6);

    let slow = fx.pipeline_with(
        Arc::new(SlowEmbedder {
            inner: HashEmbedder::new(VECTOR_DIM),
            delay: Duration::from_millis(120),
        }),
        &IndexConfig::default(),
    );
    let cancel = CancellationToken::new();
    let canceller = {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            cancel.cancel();
        })
    };
    let report = slow.run_once(6, &cancel).await.unwrap();
    canceller.await.unwrap();

    assert_eq!(report.leased, 6);
    assert!(
        report.released > 0,
        "the leases this pass never got to should have been handed back: {report:?}"
    );
    let stats = fx.queue.stats().await.unwrap();
    assert_eq!(
        stats.leased, 0,
        "no lease is left stranded for the reaper to collect five minutes from now"
    );
    let attempts: Vec<i64> = fx
        .db
        .with_read(|c| {
            let mut stmt = c.prepare(
                "SELECT attempts FROM index_queue WHERE kind = 'semantic' AND state = 'pending'",
            )?;
            let rows = stmt.query_map([], |r| r.get(0))?.collect();
            rows
        })
        .unwrap();
    assert!(
        attempts.iter().all(|a| *a == 0),
        "a job that was never run has not failed and must not be charged for it: {attempts:?}"
    );
}

#[tokio::test]
async fn a_drain_stops_when_its_progress_consumer_stops_listening() {
    let fx = Fixture::open().await;
    for n in 0..40 {
        let id = fx.message(&format!("Subject {n}"), "Body").await;
        fx.queue
            .enqueue(vec![NewJob::new(id, IndexKind::Extract)], None)
            .await
            .unwrap();
    }
    let pipeline = fx.pipeline();
    let cancel = CancellationToken::new();
    let batches = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let report = pipeline
        .drain(4, 0, &cancel, |_, remaining| {
            let batches = std::sync::Arc::clone(&batches);
            async move {
                let seen = batches.fetch_add(1, Ordering::SeqCst) + 1;
                assert!(remaining > 0, "the batch report knows what is still queued");
                if seen == 1 {
                    std::ops::ControlFlow::Break(())
                } else {
                    std::ops::ControlFlow::Continue(())
                }
            }
        })
        .await
        .unwrap();

    assert_eq!(
        batches.load(Ordering::SeqCst),
        1,
        "the drain stopped at the consumer's word"
    );
    assert_eq!(report.leased, 4);
    assert!(
        fx.queue.stats().await.unwrap().outstanding() > 0,
        "and it stopped because it was told to, not because it ran out of work"
    );
}

#[tokio::test]
async fn max_jobs_bounds_a_drain() {
    let fx = Fixture::open().await;
    for n in 0..20 {
        let id = fx.message(&format!("Subject {n}"), "Body").await;
        fx.queue
            .enqueue(vec![NewJob::new(id, IndexKind::Extract)], None)
            .await
            .unwrap();
    }
    let pipeline = fx.pipeline();
    let cancel = CancellationToken::new();
    let report = pipeline
        .drain(4, 6, &cancel, |_, _| async {
            std::ops::ControlFlow::Continue(())
        })
        .await
        .unwrap();

    assert_eq!(report.retired(), 6, "exactly the bound, not the next batch");
    assert!(fx.queue.stats().await.unwrap().outstanding() > 0);
}

#[tokio::test]
async fn a_job_whose_message_vanished_is_retired_rather_than_quarantined() {
    // Only reachable when the foreign key did not cascade — a database written
    // with `foreign_keys` off, or restored from a partial copy. Simulated here
    // rather than raced, because the branch's whole point is that an ordinary
    // deletion never becomes a dead letter an operator has to triage.
    let fx = Fixture::open().await;
    let message_id = fx.message("Subject", "Body").await;
    fx.queue
        .enqueue(vec![NewJob::new(message_id, IndexKind::Extract)], None)
        .await
        .unwrap();
    fx.db
        .write(move |c| {
            c.pragma_update(None, "foreign_keys", false)?;
            let deleted = c.execute("DELETE FROM messages WHERE id = ?1", [message_id])?;
            c.pragma_update(None, "foreign_keys", true)?;
            Ok(deleted)
        })
        .await
        .unwrap();

    let report = fx.drain(&fx.pipeline()).await;

    assert_eq!(report.failed, 0, "a missing message is not a failure");
    assert_eq!(report.discarded, 1);
    assert_eq!(fx.queue.stats().await.unwrap().dead, 0);
}

#[tokio::test]
async fn the_loop_turns_new_mail_events_into_indexed_mail() {
    let fx = Fixture::open().await;
    let events = EventLog::new(fx.db.clone(), Retention::unlimited());
    let message_id = fx.message("Quarterly report", "Revenue was up.").await;
    events
        .append(
            NewEvent::new(EventKind::NewMail)
                .account(fx.account_id)
                .message(message_id),
        )
        .await
        .unwrap();

    let index_loop = IndexLoop::new(events, fx.pipeline()).with_lease_limit(32);
    let cancel = CancellationToken::new();
    let first = index_loop.tick(&cancel).await.unwrap();
    assert_eq!(first.enqueued, 1, "the event became an extract job");

    for _ in 0..20 {
        if fx.state_kinds(message_id).len() == 4 {
            break;
        }
        index_loop.tick(&cancel).await.unwrap();
    }
    assert_eq!(
        fx.state_kinds(message_id),
        vec!["entities", "extract", "lexical", "semantic"],
        "the loop is the only thing in the process that closes sync -> searchable"
    );

    // The cursor advanced, so a second pass over the same log costs nothing.
    let again = index_loop.tick(&cancel).await.unwrap();
    assert_eq!(again.enqueued, 0);
}

#[tokio::test]
async fn a_paused_loop_neither_enqueues_nor_drains() {
    let fx = Fixture::open().await;
    let events = EventLog::new(fx.db.clone(), Retention::unlimited());
    let message_id = fx.message("Subject", "Body").await;
    events
        .append(
            NewEvent::new(EventKind::NewMail)
                .account(fx.account_id)
                .message(message_id),
        )
        .await
        .unwrap();
    fx.queue
        .enqueue(vec![NewJob::new(message_id, IndexKind::Extract)], None)
        .await
        .unwrap();

    let pipeline = fx.pipeline();
    let paused = pipeline.pause_flag();
    paused.set(true);
    let index_loop = IndexLoop::new(events, pipeline);
    let cancel = CancellationToken::new();

    let report = index_loop.tick(&cancel).await.unwrap();
    assert_eq!(report.enqueued, 0);
    assert!(report.drain.is_none(), "the tick was skipped entirely");
    assert_eq!(
        fx.queue.stats().await.unwrap().ready,
        1,
        "the queue is durable, so a stopped worker loses nothing"
    );

    paused.set(false);
    index_loop.tick(&cancel).await.unwrap();
    assert!(
        fx.state_kinds(message_id).contains(&"extract".to_owned()),
        "and starting again picks the work straight back up"
    );
}
