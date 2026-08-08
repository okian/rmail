//! The numbers `mail index status` prints, and the difference between the
//! verbs that report and the verbs that destroy.
//!
//! Every coverage assertion here seeds a known state and checks an exact
//! figure. The bug this is written against is the plausible one: a coverage
//! meter whose denominator is the set of rows that already reached the stage,
//! which is 100% by construction and can never report the problem it exists to
//! report.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use super::*;
use crate::config::{Bm25Weights, IndexSemanticConfig};
use crate::embed::hash::HashEmbedder;
use crate::embed::Embedder;
use crate::error::ErrorReason;
use crate::index::fts::FtsIndex;
use crate::index::pipeline::{DrainReport, IndexPipeline};
use crate::index::queue::QueueOptions;
use crate::index::semantic::VECTOR_DIM;
use crate::repo;
use tokio_util::sync::CancellationToken;

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// The arrival time of the first seeded message; later ones step forward from
/// it so lag is a number the test chose rather than one the clock did.
const FIRST_ARRIVAL: i64 = 1_700_000_000;

/// Seconds between successive seeded messages.
const ARRIVAL_STEP: i64 = 1_000;

struct Fixture {
    db: Database,
    queue: IndexQueue,
    semantic: SemanticIndex,
    account_id: i64,
    mailbox_id: i64,
    next_uid: std::cell::Cell<i64>,
    path: PathBuf,
}

impl Fixture {
    async fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-index-admin-{pid}-{n}.db"));
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
        let embedder: Arc<dyn Embedder> = Arc::new(HashEmbedder::new(VECTOR_DIM));
        Self {
            semantic: SemanticIndex::new(
                db.clone(),
                embedder,
                &IndexSemanticConfig {
                    chunk_tokens: 32,
                    chunk_overlap: 4,
                    ..IndexSemanticConfig::default()
                },
            ),
            queue: IndexQueue::new(db.clone(), QueueOptions::default()),
            db,
            account_id,
            mailbox_id,
            next_uid: std::cell::Cell::new(1),
            path,
        }
    }

    fn admin(&self) -> IndexAdmin {
        self.admin_with(&IndexConfig::default())
    }

    fn admin_with(&self, config: &IndexConfig) -> IndexAdmin {
        IndexAdmin::new(
            self.db.clone(),
            self.queue.clone(),
            self.semantic.clone(),
            config,
            IndexPauseFlag::default(),
        )
    }

    fn pipeline(&self) -> IndexPipeline {
        IndexPipeline::new(
            self.db.clone(),
            self.queue.clone(),
            FtsIndex::new(self.db.clone(), Bm25Weights::default()),
            self.semantic.clone(),
            &IndexConfig::default(),
        )
    }

    async fn message(&self, subject: &str, body: &str) -> i64 {
        let uid = self.next_uid.get();
        self.next_uid.set(uid + 1);
        let (account_id, mailbox_id) = (self.account_id, self.mailbox_id);
        let (subject, body) = (subject.to_owned(), body.to_owned());
        let arrival = FIRST_ARRIVAL + (uid - 1) * ARRIVAL_STEP;
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
                        date: Some(arrival),
                        internaldate: Some(arrival),
                        ..Default::default()
                    },
                )
            })
            .await
            .unwrap()
    }

    /// Run the whole pipeline over `ids` — the real stages, not a fixture that
    /// writes `index_state` by hand, so the hashes coverage and drift compare
    /// are the ones production writes.
    async fn index(&self, ids: &[i64]) {
        let jobs = ids
            .iter()
            .map(|id| NewJob::new(*id, IndexKind::Extract))
            .collect();
        self.queue.enqueue(jobs, None).await.unwrap();
        self.drain().await;
    }

    async fn drain(&self) -> DrainReport {
        let pipeline = self.pipeline();
        let cancel = CancellationToken::new();
        let mut total = DrainReport::default();
        let mut drained = false;
        for _ in 0..200 {
            let batch = pipeline.run_once(64, &cancel).await.unwrap();
            let leased = batch.leased;
            total.completed += batch.completed;
            total.discarded += batch.discarded;
            total.failed += batch.failed;
            if leased == 0 {
                drained = true;
                break;
            }
        }
        assert!(drained, "the queue never drained");
        total
    }

    fn count(&self, table: &str) -> i64 {
        let sql = format!("SELECT count(*) FROM {table}");
        self.db
            .with_read(move |c| c.query_row(&sql, [], |r| r.get(0)))
            .unwrap()
    }

    fn kind(&self, status: &IndexStatus, kind: IndexKind) -> KindStatus {
        status
            .kinds
            .iter()
            .find(|k| k.kind == kind)
            .unwrap_or_else(|| unreachable!("no status row for {kind:?}"))
            .clone()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.path.display())));
        }
    }
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

#[tokio::test]
async fn coverage_divides_by_every_message_not_by_the_ones_that_got_this_far() {
    let fx = Fixture::open().await;
    let mut ids = Vec::new();
    for n in 0..4 {
        ids.push(fx.message(&format!("Subject {n}"), "Some body text.").await);
    }
    // Three of the four indexed. The denominator that produces 0.75 is the
    // message count; the one that produces 1.0 is `index_state` counting
    // itself, which is the bug.
    fx.index(&ids[..3]).await;

    let status = fx.admin().status().await.unwrap();
    assert_eq!(status.messages, 4);
    for kind in IndexKind::PER_MESSAGE {
        let row = fx.kind(&status, kind);
        assert_eq!(row.eligible, 4, "{kind:?} eligible");
        assert_eq!(row.indexed, 3, "{kind:?} indexed");
        assert!(
            (row.coverage() - 0.75).abs() < f64::EPSILON,
            "{kind:?} coverage was {}",
            row.coverage()
        );
    }
}

#[tokio::test]
async fn an_empty_mailbox_reports_zero_coverage_not_a_vacuous_hundred_percent() {
    let fx = Fixture::open().await;
    let status = fx.admin().status().await.unwrap();
    assert_eq!(status.messages, 0);
    for kind in IndexKind::PER_MESSAGE {
        let row = fx.kind(&status, kind);
        assert_eq!(row.eligible, 0);
        assert_eq!(row.indexed, 0);
        assert!(
            row.coverage().abs() < f64::EPSILON,
            "an unindexed empty mailbox is 0%, not 100%"
        );
        assert_eq!(row.lag_seconds, None);
    }
}

#[tokio::test]
async fn an_unindexed_mailbox_reports_zero_coverage_for_every_stage() {
    let fx = Fixture::open().await;
    for n in 0..3 {
        fx.message(&format!("Subject {n}"), "Body").await;
    }
    let status = fx.admin().status().await.unwrap();
    for kind in IndexKind::PER_MESSAGE {
        let row = fx.kind(&status, kind);
        assert_eq!(row.eligible, 3);
        assert_eq!(row.indexed, 0);
        assert!(row.coverage().abs() < f64::EPSILON);
    }
}

#[tokio::test]
async fn status_reports_queue_depth_and_quarantine_per_stage() {
    let fx = Fixture::open().await;
    let a = fx.message("One", "Body one").await;
    let b = fx.message("Two", "Body two").await;
    fx.queue
        .enqueue(
            vec![
                NewJob::new(a, IndexKind::Extract),
                NewJob::new(b, IndexKind::Extract),
                NewJob::new(a, IndexKind::Lexical).content_hash(*b"h"),
            ],
            None,
        )
        .await
        .unwrap();
    // One quarantined lexical job, written directly: reaching `dead` through
    // the real backoff would take five leases and a clock.
    fx.db
        .write(move |c| {
            c.execute(
                "UPDATE index_queue SET state = 'dead', last_error = 'poison'
                 WHERE message_id = ?1 AND kind = 'lexical'",
                [a],
            )
        })
        .await
        .unwrap();

    let status = fx.admin().status().await.unwrap();
    assert_eq!(status.queue.ready, 2);
    assert_eq!(status.queue.dead, 1);
    let extract = fx.kind(&status, IndexKind::Extract);
    assert_eq!(extract.pending, 2);
    assert_eq!(extract.quarantined, 0);
    let lexical = fx.kind(&status, IndexKind::Lexical);
    assert_eq!(lexical.pending, 0, "a quarantined job is not outstanding");
    assert_eq!(lexical.quarantined, 1);
    let semantic = fx.kind(&status, IndexKind::Semantic);
    assert_eq!(semantic.pending, 0, "a stage with no jobs reports zero");
}

#[tokio::test]
async fn status_reports_the_configured_model_and_the_width_the_schema_holds() {
    let fx = Fixture::open().await;
    let status = fx.admin().status().await.unwrap();
    assert_eq!(status.model, fx.semantic.model());
    assert_eq!(status.dim, i64::try_from(VECTOR_DIM).unwrap());
    assert!(status.semantic_enabled);

    let mut off = IndexConfig::default();
    off.semantic.enabled = false;
    let status = fx.admin_with(&off).status().await.unwrap();
    assert!(!status.semantic_enabled);
    assert!(
        !fx.kind(&status, IndexKind::Semantic).enabled,
        "a switched-off stage says so, so 0% coverage reads as a choice"
    );
    assert_eq!(
        status.model,
        fx.semantic.model(),
        "the model a re-enabled daemon would use is still worth reporting"
    );
}

#[tokio::test]
async fn lag_is_the_gap_between_the_newest_message_and_the_newest_indexed_one() {
    let fx = Fixture::open().await;
    let first = fx.message("Oldest", "Body").await;
    let _second = fx.message("Middle", "Body").await;
    let _third = fx.message("Newest", "Body").await;

    let status = fx.admin().status().await.unwrap();
    assert_eq!(
        fx.kind(&status, IndexKind::Lexical).lag_seconds,
        None,
        "a stage that has indexed nothing is not behind by a number"
    );

    fx.index(&[first]).await;
    let status = fx.admin().status().await.unwrap();
    assert_eq!(
        fx.kind(&status, IndexKind::Lexical).lag_seconds,
        Some(2 * ARRIVAL_STEP),
        "two arrivals behind the newest message"
    );

    fx.index(&[_second, _third]).await;
    let status = fx.admin().status().await.unwrap();
    assert_eq!(
        fx.kind(&status, IndexKind::Lexical).lag_seconds,
        Some(0),
        "caught up to the newest message"
    );
}

#[tokio::test]
async fn status_reflects_the_pause_flag_the_worker_actually_reads() {
    let fx = Fixture::open().await;
    let paused = IndexPauseFlag::default();
    let admin = IndexAdmin::new(
        fx.db.clone(),
        fx.queue.clone(),
        fx.semantic.clone(),
        &IndexConfig::default(),
        paused.clone(),
    );
    assert!(!admin.status().await.unwrap().paused);
    paused.set(true);
    assert!(
        admin.status().await.unwrap().paused,
        "status shares the flag rather than a copy of its value, or `mail index stop` \
         would look like it had failed"
    );
}

// ---------------------------------------------------------------------------
// verify
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_freshly_indexed_mailbox_verifies_clean() {
    let fx = Fixture::open().await;
    let a = fx
        .message("Invoice", "Invoice INV-2024-0231 for £1,299.00.")
        .await;
    let b = fx
        .message("Shipping", "Tracking 1Z999AA10123456784 is out.")
        .await;
    fx.index(&[a, b]).await;

    let drift = fx.admin().verify().await.unwrap();
    assert!(
        drift.is_clean(),
        "a clean index should verify clean: {drift:?}"
    );
}

#[tokio::test]
async fn verify_reports_content_hash_drift_and_repairs_none_of_it() {
    let fx = Fixture::open().await;
    let message_id = fx.message("Subject", "The original body.").await;
    fx.index(&[message_id]).await;

    let before = fx
        .db
        .with_read(move |c| {
            let mut stmt = c.prepare(
                "SELECT kind, content_hash FROM index_state WHERE message_id = ?1 ORDER BY kind",
            )?;
            let rows = stmt
                .query_map([message_id], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, Option<Vec<u8>>>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>();
            rows
        })
        .unwrap();

    // The text moves under the index: a new body with a new hash, and no stage
    // re-run. This is exactly what a partially-applied write, an interrupted
    // rebuild, or a subsystem that forgot to enqueue leaves behind.
    fx.db
        .write(move |c| {
            c.execute(
                "UPDATE index_content SET text = 'a different body', content_hash = X'DEADBEEF'
                 WHERE message_id = ?1 AND part = 'body'",
                [message_id],
            )
        })
        .await
        .unwrap();

    let drift = fx.admin().verify().await.unwrap();
    assert_eq!(
        drift.content_hash_drift, 3,
        "one per downstream stage that recorded the old hash: {drift:?}"
    );
    assert!(!drift.is_clean());

    let after = fx
        .db
        .with_read(move |c| {
            let mut stmt = c.prepare(
                "SELECT kind, content_hash FROM index_state WHERE message_id = ?1 ORDER BY kind",
            )?;
            let rows = stmt
                .query_map([message_id], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, Option<Vec<u8>>>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>();
            rows
        })
        .unwrap();
    assert_eq!(before, after, "verify is a report, not a repair");
    assert_eq!(
        fx.queue.stats().await.unwrap().outstanding(),
        0,
        "and it enqueues nothing on its own"
    );

    let again = fx.admin().verify().await.unwrap();
    assert_eq!(again.content_hash_drift, 3, "the drift is still there");
}

#[tokio::test]
async fn verify_reports_a_lexical_row_that_went_missing_and_one_that_outlived_its_message() {
    let fx = Fixture::open().await;
    let message_id = fx.message("Subject", "Body text.").await;
    fx.index(&[message_id]).await;

    fx.db
        .write(move |c| {
            // The row the lexical stage claims to have written, gone.
            c.execute("DELETE FROM fts_messages WHERE rowid = ?1", [message_id])?;
            // And one for a message that never existed.
            c.execute(
                "INSERT INTO fts_messages(rowid, subject, sender, recipients, body,
                                          attachments, notes, summary)
                 VALUES (999999, 'ghost', '', '', '', '', '', '')",
                [],
            )
        })
        .await
        .unwrap();

    let drift = fx.admin().verify().await.unwrap();
    assert_eq!(drift.lexical_missing, 1);
    assert_eq!(drift.lexical_orphaned, 1);
}

#[tokio::test]
async fn verify_reports_a_quarantined_job_because_that_is_why_coverage_stalls() {
    let fx = Fixture::open().await;
    let message_id = fx.message("Subject", "Body").await;
    fx.queue
        .enqueue(vec![NewJob::new(message_id, IndexKind::Extract)], None)
        .await
        .unwrap();
    fx.db
        .write(|c| c.execute("UPDATE index_queue SET state = 'dead'", []))
        .await
        .unwrap();

    let drift = fx.admin().verify().await.unwrap();
    assert_eq!(drift.quarantined, 1);
    assert!(!drift.is_clean());
}

// ---------------------------------------------------------------------------
// gc
// ---------------------------------------------------------------------------

#[tokio::test]
async fn gc_removes_orphans_and_leaves_every_live_row_exactly_where_it_was() {
    // The negative half of this test matters more than the positive one: a
    // collector that deletes a live vector is catastrophic and silent — search
    // simply stops returning a message, with nothing in any log to say why.
    let fx = Fixture::open().await;
    let doomed = fx
        .message("Invoice", "Invoice INV-2024-0231 from ada@example.com.")
        .await;
    let survivor = fx
        .message(
            "Shipping",
            "Tracking 1Z999AA10123456784 from bob@example.com.",
        )
        .await;
    fx.index(&[doomed, survivor]).await;

    let live_chunks = fx
        .db
        .with_read(move |c| {
            c.query_row(
                "SELECT count(*) FROM chunks WHERE message_id = ?1",
                [survivor],
                |r| r.get::<_, i64>(0),
            )
        })
        .unwrap();
    assert!(live_chunks > 0, "the survivor has something to lose");

    // Deleting the message cascades to `chunks` but not to `vec_chunks`, which
    // is a virtual table and takes no foreign key. That is precisely the orphan
    // gc exists for.
    fx.db
        .write(move |c| c.execute("DELETE FROM messages WHERE id = ?1", [doomed]))
        .await
        .unwrap();

    let before = fx.admin().verify().await.unwrap();
    assert!(
        before.semantic.orphaned > 0,
        "the deleted message left vectors behind: {before:?}"
    );

    let report = fx.admin().gc().await.unwrap();
    assert_eq!(
        report.vectors as i64,
        before.semantic.orphaned + 1,
        "every chunk vector the delete stranded, plus the message centroid it also \
         stranded: `Drift::orphaned` counts `vec_chunks` alone, while the sweep takes \
         the orphaned `vec_messages` row as well"
    );
    assert!(report.entities > 0, "its entities lost their last mention");

    // The survivor is untouched, checked row by row rather than by a total.
    let after_chunks = fx
        .db
        .with_read(move |c| {
            c.query_row(
                "SELECT count(*) FROM chunks WHERE message_id = ?1",
                [survivor],
                |r| r.get::<_, i64>(0),
            )
        })
        .unwrap();
    assert_eq!(after_chunks, live_chunks, "live chunks survive");
    let live_vectors = fx
        .db
        .with_read(move |c| {
            c.query_row(
                "SELECT count(*) FROM vec_chunks v JOIN chunks c ON c.chunk_id = v.chunk_id
                 WHERE c.message_id = ?1",
                [survivor],
                |r| r.get::<_, i64>(0),
            )
        })
        .unwrap();
    assert_eq!(
        live_vectors, live_chunks,
        "and so does every one of their vectors"
    );
    let survivor_fts = fx
        .db
        .with_read(move |c| {
            c.query_row(
                "SELECT count(*) FROM fts_messages WHERE rowid = ?1",
                [survivor],
                |r| r.get::<_, i64>(0),
            )
        })
        .unwrap();
    assert_eq!(survivor_fts, 1, "and its lexical row");
    let survivor_entities = fx
        .db
        .with_read(move |c| {
            c.query_row(
                "SELECT count(*) FROM entity_mentions WHERE message_id = ?1",
                [survivor],
                |r| r.get::<_, i64>(0),
            )
        })
        .unwrap();
    assert!(survivor_entities > 0, "and its entity mentions");

    let after = fx.admin().verify().await.unwrap();
    assert_eq!(after.semantic.orphaned, 0);
    assert_eq!(after.entity_orphaned, 0);
}

#[tokio::test]
async fn gc_on_a_clean_index_removes_nothing_at_all() {
    let fx = Fixture::open().await;
    let a = fx.message("Invoice", "Invoice INV-2024-0231.").await;
    let b = fx.message("Shipping", "Tracking 1Z999AA10123456784.").await;
    fx.index(&[a, b]).await;

    let before = (
        fx.count("chunks"),
        fx.count("vec_chunks"),
        fx.count("entities"),
        fx.count("fts_messages"),
        fx.count("index_content"),
    );
    let report = fx.admin().gc().await.unwrap();
    assert_eq!(report, GcReport::default(), "nothing was orphaned");
    assert_eq!(
        before,
        (
            fx.count("chunks"),
            fx.count("vec_chunks"),
            fx.count("entities"),
            fx.count("fts_messages"),
            fx.count("index_content"),
        ),
        "and nothing was removed"
    );
}

#[tokio::test]
async fn gc_sweeps_a_lexical_row_whose_message_is_gone() {
    let fx = Fixture::open().await;
    fx.db
        .write(|c| {
            c.execute(
                "INSERT INTO fts_messages(rowid, subject, sender, recipients, body,
                                          attachments, notes, summary)
                 VALUES (424242, 'ghost', '', '', '', '', '', '')",
                [],
            )
        })
        .await
        .unwrap();

    let report = fx.admin().gc().await.unwrap();
    assert_eq!(report.lexical_rows, 1);
    assert_eq!(fx.count("fts_messages"), 0);
}

// ---------------------------------------------------------------------------
// reindex / rebuild
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reindex_over_a_current_index_enqueues_nothing() {
    let fx = Fixture::open().await;
    let a = fx.message("One", "Body one.").await;
    let b = fx.message("Two", "Body two.").await;
    fx.index(&[a, b]).await;

    let enqueued = fx.admin().reindex(&Selection::default()).await.unwrap();
    assert_eq!(
        enqueued, 0,
        "re-running over unchanged mail is free — that is the whole point of the state table"
    );
}

#[tokio::test]
async fn reindex_enqueues_exactly_the_stages_that_drifted() {
    let fx = Fixture::open().await;
    let drifted = fx.message("One", "Body one.").await;
    let current = fx.message("Two", "Body two.").await;
    fx.index(&[drifted, current]).await;

    fx.db
        .write(move |c| {
            c.execute(
                "UPDATE index_content SET text = 'moved', content_hash = X'BEEF'
                 WHERE message_id = ?1 AND part = 'body'",
                [drifted],
            )
        })
        .await
        .unwrap();

    let enqueued = fx.admin().reindex(&Selection::default()).await.unwrap();
    assert_eq!(
        enqueued, 3,
        "the three downstream stages of the one drifted message, and nothing else"
    );
    let queued: Vec<(i64, String)> = fx
        .db
        .with_read(|c| {
            let mut stmt =
                c.prepare("SELECT message_id, kind FROM index_queue WHERE state = 'pending'")?;
            let rows = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect();
            rows
        })
        .unwrap();
    assert!(
        queued.iter().all(|(id, _)| *id == drifted),
        "the untouched message was not requeued: {queued:?}"
    );
}

#[tokio::test]
async fn reindex_narrows_to_the_selection_it_is_given() {
    let fx = Fixture::open().await;
    let first = fx.message("One", "Body one.").await;
    let second = fx.message("Two", "Body two.").await;
    let third = fx.message("Three", "Body three.").await;

    let enqueued = fx
        .admin()
        .reindex(&Selection {
            kinds: vec![IndexKind::Extract],
            message_id: Some(second),
            ..Selection::default()
        })
        .await
        .unwrap();
    assert_eq!(enqueued, 1);

    let queued: Vec<i64> = fx
        .db
        .with_read(|c| {
            let mut stmt = c.prepare("SELECT message_id FROM index_queue")?;
            let rows = stmt.query_map([], |r| r.get(0))?.collect();
            rows
        })
        .unwrap();
    assert_eq!(queued, vec![second]);

    // And by arrival time: `since` is inclusive of the message that arrived
    // exactly then.
    let admin = fx.admin();
    let since = FIRST_ARRIVAL + ARRIVAL_STEP;
    admin
        .reindex(&Selection {
            kinds: vec![IndexKind::Extract],
            since: Some(since),
            ..Selection::default()
        })
        .await
        .unwrap();
    let queued: Vec<i64> = fx
        .db
        .with_read(|c| {
            let mut stmt = c.prepare("SELECT message_id FROM index_queue ORDER BY message_id")?;
            let rows = stmt.query_map([], |r| r.get(0))?.collect();
            rows
        })
        .unwrap();
    assert_eq!(queued, vec![second, third]);
    assert!(!queued.contains(&first));
}

#[tokio::test]
async fn rebuild_drops_the_derived_data_and_requeues_the_work_to_recreate_it() {
    let fx = Fixture::open().await;
    let a = fx.message("Invoice", "Invoice INV-2024-0231.").await;
    let b = fx.message("Shipping", "Tracking 1Z999AA10123456784.").await;
    fx.index(&[a, b]).await;
    assert!(fx.count("chunks") > 0 && fx.count("entities") > 0);

    let report = fx.admin().rebuild(&[]).await.unwrap();
    assert!(report.dropped > 0, "a rebuild is a wipe: {report:?}");
    assert_eq!(fx.count("chunks"), 0);
    assert_eq!(fx.count("vec_chunks"), 0);
    assert_eq!(fx.count("entities"), 0);
    assert_eq!(fx.count("fts_messages"), 0);
    assert_eq!(fx.count("index_state"), 0);
    assert_eq!(
        report.enqueued, 2,
        "one extract job per message; extraction cascades into the rest"
    );

    fx.drain().await;
    assert!(fx.count("chunks") > 0);
    assert!(fx.count("entities") > 0);
    assert_eq!(fx.count("fts_messages"), 2);
    assert!(fx.admin().verify().await.unwrap().is_clean());
}

#[tokio::test]
async fn rebuilding_one_stage_leaves_the_others_intact() {
    let fx = Fixture::open().await;
    let message_id = fx.message("Invoice", "Invoice INV-2024-0231.").await;
    fx.index(&[message_id]).await;
    let chunks = fx.count("chunks");

    fx.admin().rebuild(&[IndexKind::Lexical]).await.unwrap();
    assert_eq!(fx.count("fts_messages"), 0, "the lexical index is gone");
    assert_eq!(fx.count("chunks"), chunks, "the semantic index is not");
    assert!(fx.count("entities") > 0, "nor the entity graph");
}

#[tokio::test]
async fn rebuilding_extraction_keeps_the_parts_other_subsystems_own() {
    // A note is a user's own writing and an attachment's text is minutes of
    // extraction work; neither is the extract stage's to delete, and a rebuild
    // that took them would be silent, unrecoverable data loss.
    let fx = Fixture::open().await;
    let message_id = fx.message("Subject", "Body text.").await;
    fx.index(&[message_id]).await;
    fx.db
        .write(move |c| {
            c.execute(
                "INSERT INTO index_content
                     (message_id, part, text, chars, content_hash, extractor)
                 VALUES (?1, 'note', 'remember to reply', 17, X'01', 'test')",
                [message_id],
            )
        })
        .await
        .unwrap();

    fx.admin().rebuild(&[IndexKind::Extract]).await.unwrap();

    let parts: Vec<String> = fx
        .db
        .with_read(move |c| {
            let mut stmt =
                c.prepare("SELECT part FROM index_content WHERE message_id = ?1 ORDER BY part")?;
            let rows = stmt.query_map([message_id], |r| r.get(0))?.collect();
            rows
        })
        .unwrap();
    assert_eq!(
        parts,
        vec!["note"],
        "only the extractor's own parts were wiped"
    );
}

#[tokio::test]
async fn backfill_reschedules_a_message_whose_vector_went_missing() {
    // The dark-chunk case: `chunk_embeddings` still claims the chunk is
    // embedded and `index_state` still says the message is current, so a plain
    // reindex dedups the repair away and the chunk stays permanently
    // unsearchable. Backfill is the path that has to see through that.
    let fx = Fixture::open().await;
    let message_id = fx.message("Subject", "Body text worth embedding.").await;
    fx.index(&[message_id]).await;

    fx.db
        .write(|c| {
            c.execute(
                "DELETE FROM vec_chunks WHERE chunk_id = (SELECT min(chunk_id) FROM chunks)",
                [],
            )
        })
        .await
        .unwrap();

    assert_eq!(
        fx.admin().reindex(&Selection::default()).await.unwrap(),
        0,
        "an ordinary reindex cannot see this: the recorded state still matches"
    );

    let enqueued = fx.admin().backfill_embeddings().await.unwrap();
    assert_eq!(enqueued, 1);
    fx.drain().await;
    assert!(
        fx.admin().verify().await.unwrap().semantic.is_clean(),
        "and the drain actually repaired it"
    );
}

// ---------------------------------------------------------------------------
// entities
// ---------------------------------------------------------------------------

#[tokio::test]
async fn entities_are_listed_by_kind_with_how_widely_they_are_mentioned() {
    let fx = Fixture::open().await;
    let a = fx
        .message("One", "Write to ada@example.com about it.")
        .await;
    let b = fx
        .message("Two", "Also ada@example.com, and bob@example.com.")
        .await;
    fx.index(&[a, b]).await;

    let rows = fx.admin().list_entities("email", None, 50).await.unwrap();
    let ada = rows
        .iter()
        .find(|r| r.norm == "ada@example.com")
        .expect("ada was extracted");
    assert_eq!(ada.kind, "email");
    assert_eq!(ada.messages, 2, "mentioned in both messages");
    assert!(rows.iter().any(|r| r.norm == "bob@example.com"));

    let filtered = fx
        .admin()
        .list_entities("email", Some("BOB"), 50)
        .await
        .unwrap();
    assert_eq!(filtered.len(), 1, "the value filter folds case");
    assert_eq!(filtered[0].norm, "bob@example.com");
}

#[tokio::test]
async fn the_value_filter_folds_case_against_norms_that_are_not_all_lower_case() {
    // An address normalizes to lower case and an invoice reference to upper,
    // so a filter that only lowercased the *needle* would find one kind and
    // never the other.
    let fx = Fixture::open().await;
    let message_id = fx
        .message("Invoice", "Invoice INV-2024-0231 is due on Friday.")
        .await;
    fx.index(&[message_id]).await;

    let all = fx
        .admin()
        .list_entities("invoice_id", None, 50)
        .await
        .unwrap();
    assert_eq!(all.len(), 1, "{all:?}");
    assert_eq!(all[0].norm, "INV-2024-0231", "stored upper case");

    let typed_lower = fx
        .admin()
        .list_entities("invoice_id", Some("inv-2024"), 50)
        .await
        .unwrap();
    assert_eq!(typed_lower.len(), 1, "a lower-case needle finds it");
}

#[tokio::test]
async fn a_wildcard_in_the_value_filter_is_a_literal_not_a_pattern() {
    // `%` and `_` are LIKE wildcards. Binding the parameter stops injection; it
    // does not stop them being read as syntax, so without escaping a filter for
    // `_` would match every entity of the kind.
    let fx = Fixture::open().await;
    let message_id = fx
        .message("Contacts", "Reach ada@example.com or bob@example.com.")
        .await;
    fx.index(&[message_id]).await;

    let all = fx.admin().list_entities("email", None, 50).await.unwrap();
    assert!(all.len() >= 2, "{all:?}");

    let underscore = fx
        .admin()
        .list_entities("email", Some("_"), 50)
        .await
        .unwrap();
    assert!(
        underscore.is_empty(),
        "no address here contains a literal underscore: {underscore:?}"
    );
    let percent = fx
        .admin()
        .list_entities("email", Some("%"), 50)
        .await
        .unwrap();
    assert!(percent.is_empty(), "nor a literal percent: {percent:?}");
}

#[tokio::test]
async fn an_unknown_entity_kind_is_rejected_rather_than_answered_with_nothing() {
    let fx = Fixture::open().await;
    let error = fx
        .admin()
        .list_entities("not_a_kind", None, 10)
        .await
        .expect_err("an unknown kind is a mistake, not an empty result");
    assert_eq!(error.reason(), ErrorReason::InvalidArgument);
    assert!(
        error.to_string().contains("tracking_no"),
        "and it says what the real kinds are: {error}"
    );
}
