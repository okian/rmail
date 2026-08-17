//! Integration test: drive `IndexService` end-to-end against an in-process
//! tonic server over a Unix domain socket.
//!
//! Nothing here is faked. The stages that run are the real extract/lexical/
//! entity/semantic stages over a real SQLite database; only the embedder is a
//! stub (`HashEmbedder`, deterministic and dependency-free — see
//! `rmail_core::embed::hash`'s own docs), for the same reason every other
//! semantic test in this workspace uses it: what is under test is bookkeeping
//! between tables, and loading a hundred megabytes of ONNX weights would make
//! that slower without making it more decisive.
//!
//! The harness builds `IndexApi` directly rather than booting the whole daemon,
//! which is what lets it hold a clone of the very `IndexPipeline` the service
//! is draining — the only way to tell "the producer stopped" from "the producer
//! ran out of work" without guessing from timings.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rmail_core::config::Bm25Weights;
use rmail_core::embed::hash::HashEmbedder;
use rmail_core::embed::Embedder;
use rmail_core::index::fts::FtsIndex;
use rmail_core::index::semantic::{SemanticIndex, VECTOR_DIM};
use rmail_core::index::{IndexAdmin, IndexPipeline, IndexQueue, QueueOptions};
use rmail_core::{repo, Config, Database};
use rmail_proto::v1::index_service_client::IndexServiceClient;
use rmail_proto::v1::{
    IndexGcRequest, IndexKind as ProtoKind, IndexProgress, IndexStatusRequest, IndexStatusResponse,
    ListEntitiesRequest, RebuildRequest, ReindexMode, ReindexRequest, SetIndexPausedRequest,
    VerifyIndexRequest,
};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::UnixListenerStream;
use tokio_stream::StreamExt;
use tokio_util::sync::CancellationToken;
use tonic::transport::{Channel, Server};
use tonic::Code;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

/// How long a stream assertion waits before failing — generous, since these are
/// liveness checks on spawned tasks, not latency measurements.
const STREAM_TIMEOUT: Duration = Duration::from_secs(30);

/// The arrival time of the first seeded message; later ones step forward.
const FIRST_ARRIVAL: i64 = 1_700_000_000;

struct TestServer {
    socket: PathBuf,
    db_path: PathBuf,
    db: Database,
    /// A clone of the pipeline the service drains, so a test can ask how much
    /// work has actually happened.
    pipeline: IndexPipeline,
    account_id: i64,
    mailbox_id: i64,
    next_uid: std::cell::Cell<i64>,
    shutdown: oneshot::Sender<()>,
    handle: JoinHandle<()>,
}

impl TestServer {
    async fn start() -> Self {
        Self::start_with(Config::default()).await
    }

    async fn start_with(config: Config) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let socket = PathBuf::from("/tmp").join(format!("rmail-index-{pid}-{n}.sock"));
        let db_path = std::env::temp_dir().join(format!("rmail-index-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", db_path.display())));
        }
        let _ = std::fs::remove_file(&socket);

        let db = Database::open(&db_path).unwrap();
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
        let queue = IndexQueue::new(db.clone(), QueueOptions::default());
        let semantic = SemanticIndex::new(db.clone(), embedder, &config.index.semantic);
        let pipeline = IndexPipeline::new(
            db.clone(),
            queue.clone(),
            FtsIndex::new(db.clone(), Bm25Weights::default()),
            semantic.clone(),
            &config.index,
        );
        let admin = IndexAdmin::new(
            db.clone(),
            queue,
            semantic,
            &config.index,
            pipeline.pause_flag(),
        );
        let shutdown_cancel = CancellationToken::new();
        let api = rmaild::IndexApi::new(
            admin,
            pipeline.clone(),
            shutdown_cancel.clone(),
            config.search.cache,
        );

        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let incoming = UnixListenerStream::new(listener);
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            let _ = Server::builder()
                .add_service(rmail_proto::v1::index_service_server::IndexServiceServer::new(api))
                .serve_with_incoming_shutdown(incoming, async move {
                    let _ = shutdown_rx.await;
                    shutdown_cancel.cancel();
                })
                .await;
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
            pipeline,
            account_id,
            mailbox_id,
            next_uid: std::cell::Cell::new(1),
            shutdown: shutdown_tx,
            handle,
        }
    }

    async fn client(&self) -> IndexServiceClient<Channel> {
        IndexServiceClient::new(rmail_core::connect_uds(&self.socket).await.unwrap())
    }

    async fn message(&self, subject: &str, body: &str) -> i64 {
        let uid = self.next_uid.get();
        self.next_uid.set(uid + 1);
        let (account_id, mailbox_id) = (self.account_id, self.mailbox_id);
        let (subject, body) = (subject.to_owned(), body.to_owned());
        let arrival = FIRST_ARRIVAL + uid;
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

    /// Run a full `Reindex` to completion through the RPC, returning the final
    /// frame.
    async fn reindex(&self, request: ReindexRequest) -> IndexProgress {
        let mut client = self.client().await;
        let mut stream = client.reindex(request).await.unwrap().into_inner();
        let mut last = None;
        while let Some(frame) = tokio::time::timeout(STREAM_TIMEOUT, stream.next())
            .await
            .expect("the drain should finish well inside the timeout")
        {
            let frame = frame.unwrap();
            let done = frame.done;
            last = Some(frame);
            if done {
                break;
            }
        }
        last.expect("at least one progress frame")
    }

    async fn index_all(&self) -> IndexProgress {
        self.reindex(ReindexRequest {
            mode: ReindexMode::Selection as i32,
            ..ReindexRequest::default()
        })
        .await
    }

    async fn status(&self) -> IndexStatusResponse {
        self.client()
            .await
            .status(IndexStatusRequest {})
            .await
            .unwrap()
            .into_inner()
    }

    fn count(&self, table: &str) -> i64 {
        let sql = format!("SELECT count(*) FROM {table}");
        self.db
            .with_read(move |c| c.query_row(&sql, [], |r| r.get(0)))
            .unwrap()
    }

    async fn stop(self) {
        let _ = self.shutdown.send(());
        let _ = tokio::time::timeout(STREAM_TIMEOUT, self.handle).await;
        let _ = std::fs::remove_file(&self.socket);
        for suffix in ["", "-wal", "-shm"] {
            let _ =
                std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.db_path.display())));
        }
    }
}

fn kind(status: &IndexStatusResponse, wanted: ProtoKind) -> &rmail_proto::v1::IndexKindStatus {
    status
        .kinds
        .iter()
        .find(|k| k.kind == wanted as i32)
        .unwrap_or_else(|| panic!("no status row for {wanted:?}"))
}

// ---------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------

#[tokio::test]
async fn status_reports_coverage_against_every_message_not_just_the_indexed_ones() {
    let server = TestServer::start().await;
    for n in 0..3 {
        server
            .message(&format!("Subject {n}"), "A body worth indexing.")
            .await;
    }
    server.index_all().await;
    // A fourth message arrives *after* the pass, so it is eligible and
    // unindexed. A denominator taken from `index_state` would still say 100%.
    server.message("Latecomer", "Arrived after the pass.").await;

    let status = server.status().await;
    assert_eq!(status.messages, 4);
    for stage in [
        ProtoKind::Extract,
        ProtoKind::Lexical,
        ProtoKind::Entities,
        ProtoKind::Semantic,
    ] {
        let row = kind(&status, stage);
        assert_eq!(row.eligible, 4, "{stage:?} eligible");
        assert_eq!(row.indexed, 3, "{stage:?} indexed");
        assert!(
            (row.coverage - 0.75).abs() < 1e-9,
            "{stage:?} coverage was {}",
            row.coverage
        );
    }
    assert!(status.chunks > 0 && status.vectors > 0);
    assert_eq!(status.model, "hash-fallback");
    assert_eq!(status.dim, i64::try_from(VECTOR_DIM).unwrap());

    server.stop().await;
}

#[tokio::test]
async fn status_reports_queue_depth_and_lag_and_the_worker_state() {
    let server = TestServer::start().await;
    let _old = server.message("Older", "Body one.").await;
    server.index_all().await;
    // A newer message that nothing has indexed: the index is now exactly one
    // arrival step behind the store.
    server.message("Newer", "Body two.").await;

    let status = server.status().await;
    let lexical = kind(&status, ProtoKind::Lexical);
    assert_eq!(
        lexical.lag_seconds,
        Some(1),
        "the arrival gap between the newest message and the newest indexed one"
    );
    assert_eq!(status.queue_ready, 0, "nothing is queued yet");
    assert!(!status.paused);

    // Catching up closes the gap, which is the property that makes lag worth
    // reporting at all: it goes to zero on its own when the index is current.
    server.index_all().await;
    let status = server.status().await;
    assert_eq!(kind(&status, ProtoKind::Lexical).lag_seconds, Some(0));
    assert_eq!(status.queue_ready, 0);

    server.stop().await;
}

#[tokio::test]
async fn set_paused_stops_the_worker_and_status_says_so() {
    let server = TestServer::start().await;
    assert!(!server.status().await.paused);

    let mut client = server.client().await;
    let response = client
        .set_paused(SetIndexPausedRequest { paused: true })
        .await
        .unwrap()
        .into_inner();
    assert!(response.paused);
    assert!(
        server.status().await.paused,
        "status reads the same flag the worker does"
    );

    // An explicit drain still runs while the worker is stopped: `mail index
    // stop` stops the *background* worker, not the operator.
    server.message("Subject", "Body").await;
    let final_frame = server.index_all().await;
    assert!(final_frame.done);
    assert_eq!(kind(&server.status().await, ProtoKind::Lexical).indexed, 1);

    client
        .set_paused(SetIndexPausedRequest { paused: false })
        .await
        .unwrap();
    assert!(!server.status().await.paused);

    server.stop().await;
}

#[tokio::test]
async fn a_stage_switched_off_in_config_reports_zero_coverage_rather_than_a_false_full_one() {
    let mut config = Config::default();
    config.index.semantic.enabled = false;
    let server = TestServer::start_with(config).await;
    server.message("Subject", "A body worth indexing.").await;
    server.index_all().await;

    let status = server.status().await;
    assert!(!status.semantic_enabled);
    let semantic = kind(&status, ProtoKind::Semantic);
    assert!(!semantic.enabled);
    assert_eq!(semantic.indexed, 0);
    assert!(semantic.coverage.abs() < f64::EPSILON);
    assert_eq!(semantic.pending, 0, "the job left the queue all the same");
    assert_eq!(status.chunks, 0);
    // The lexical stage still ran: the stages fail and succeed independently.
    assert_eq!(kind(&status, ProtoKind::Lexical).indexed, 1);

    server.stop().await;
}

// ---------------------------------------------------------------------------
// Reindex: streaming, boundedness, cancellation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn reindex_streams_progress_and_ends_with_a_done_frame() {
    let server = TestServer::start().await;
    for n in 0..8 {
        server.message(&format!("Subject {n}"), "Body text.").await;
    }

    let mut client = server.client().await;
    let mut stream = client
        .reindex(ReindexRequest {
            mode: ReindexMode::Selection as i32,
            ..ReindexRequest::default()
        })
        .await
        .unwrap()
        .into_inner();

    let mut frames = Vec::new();
    while let Some(frame) = tokio::time::timeout(STREAM_TIMEOUT, stream.next())
        .await
        .expect("the drain finishes")
    {
        let frame = frame.unwrap();
        let done = frame.done;
        frames.push(frame);
        if done {
            break;
        }
    }

    let last = frames.last().expect("at least one frame");
    assert!(last.done, "the stream ends with a done frame");
    assert_eq!(last.enqueued, 8, "one extract job per message");
    assert_eq!(
        last.remaining, 0,
        "and the queue is empty when it says done"
    );
    assert_eq!(last.failed, 0);
    assert_eq!(
        last.completed, 32,
        "extract plus the three stages it cascades into, for eight messages"
    );
    assert!(
        frames.windows(2).all(|w| w[0].completed <= w[1].completed),
        "progress is monotonic: {frames:?}"
    );

    server.stop().await;
}

#[tokio::test]
async fn reindex_over_a_current_index_enqueues_nothing_and_does_nothing() {
    let server = TestServer::start().await;
    server.message("Subject", "Body text.").await;
    server.index_all().await;

    let again = server.index_all().await;
    assert_eq!(again.enqueued, 0);
    assert_eq!(again.completed, 0);
    assert!(again.done);

    server.stop().await;
}

#[tokio::test]
async fn max_jobs_bounds_a_drain_and_the_final_frame_says_what_is_left() {
    let server = TestServer::start().await;
    for n in 0..8 {
        server.message(&format!("Subject {n}"), "Body text.").await;
    }

    let frame = server
        .reindex(ReindexRequest {
            mode: ReindexMode::Selection as i32,
            max_jobs: 4,
            ..ReindexRequest::default()
        })
        .await;
    assert!(frame.done);
    assert_eq!(frame.completed, 4);
    assert!(
        frame.remaining > 0,
        "the work it did not do is still queued: {frame:?}"
    );

    server.stop().await;
}

#[tokio::test]
async fn a_negative_max_jobs_is_rejected_rather_than_read_as_unbounded() {
    let server = TestServer::start().await;
    let status = server
        .client()
        .await
        .reindex(ReindexRequest {
            mode: ReindexMode::Selection as i32,
            max_jobs: -1,
            ..ReindexRequest::default()
        })
        .await
        .expect_err("a negative bound is a mistake");
    assert_eq!(status.code(), Code::InvalidArgument);
    server.stop().await;
}

#[tokio::test]
async fn a_default_valued_stage_alongside_a_real_one_is_refused_rather_than_widened() {
    // Proto3 cannot tell "the client meant every stage" from "a default-valued
    // enum got appended", and for Rebuild the two readings differ by three
    // whole stages of deleted data. Reading it as "all of them" would turn a
    // request to rebuild the lexical index into a full wipe with `confirm`
    // already satisfied.
    let server = TestServer::start().await;
    server.message("Invoice", "Invoice INV-2024-0231.").await;
    server.index_all().await;
    let chunks = server.count("chunks");
    assert!(chunks > 0);

    let mut client = server.client().await;
    let status = client
        .rebuild(RebuildRequest {
            kinds: vec![ProtoKind::Lexical as i32, ProtoKind::Unspecified as i32],
            confirm: true,
            max_jobs: 0,
        })
        .await
        .expect_err("an ambiguous stage list must not be guessed at");
    assert_eq!(status.code(), Code::InvalidArgument);
    assert_eq!(
        server.count("chunks"),
        chunks,
        "and nothing outside the named stage was touched"
    );
    assert_eq!(server.count("fts_messages"), 1, "nor inside it");

    // Reindex rejects it too — same helper, and a silently widened selection is
    // wasted work even where it is not destructive.
    let status = client
        .reindex(ReindexRequest {
            mode: ReindexMode::Selection as i32,
            kinds: vec![ProtoKind::Semantic as i32, ProtoKind::Unspecified as i32],
            ..ReindexRequest::default()
        })
        .await
        .expect_err("same helper, same refusal");
    assert_eq!(status.code(), Code::InvalidArgument);

    // An empty list still means every stage, which is the supported spelling.
    let frame = server.index_all().await;
    assert!(frame.done);

    server.stop().await;
}

#[tokio::test]
async fn a_second_concurrent_drain_is_refused_rather_than_racing_the_first() {
    // Two drains under one handler share a worker name, and every
    // `complete`/`fail`/`release` is fenced on `leased_by = worker` — so a
    // stalled drain whose lease was reaped could finish the other's work.
    // Refusing is both simpler and more honest than a second pass that leases
    // from the same queue and does no extra work.
    let server = TestServer::start().await;
    for n in 0..60 {
        server.message(&format!("Subject {n}"), "Body text.").await;
    }

    let mut first = server.client().await;
    let held = first
        .reindex(ReindexRequest {
            mode: ReindexMode::Selection as i32,
            ..ReindexRequest::default()
        })
        .await
        .unwrap()
        .into_inner();

    let mut second = server.client().await;
    let status = second
        .reindex(ReindexRequest {
            mode: ReindexMode::Drain as i32,
            ..ReindexRequest::default()
        })
        .await
        .expect_err("a second pass while one is running");
    assert_eq!(status.code(), Code::FailedPrecondition);
    assert!(status.message().contains("already running"));

    // And the permit comes back when the first pass ends.
    drop(held);
    drop(first);
    let mut freed = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if second
            .reindex(ReindexRequest {
                mode: ReindexMode::Drain as i32,
                max_jobs: 1,
                ..ReindexRequest::default()
            })
            .await
            .is_ok()
        {
            freed = true;
            break;
        }
    }
    assert!(freed, "the drain permit was never released");

    server.stop().await;
}

#[tokio::test]
async fn dropping_the_reindex_stream_stops_the_work_behind_it() {
    // The acceptance case. The producer applies backpressure on a bounded
    // channel, so a client that reads one frame and leaves parks it within a
    // few batches — and the check below is that it *stopped*, not merely that
    // it finished, which is why the queue must still hold work at the end.
    let server = TestServer::start().await;
    for n in 0..60 {
        server.message(&format!("Subject {n}"), "Body text.").await;
    }

    let mut client = server.client().await;
    let mut stream = client
        .reindex(ReindexRequest {
            mode: ReindexMode::Selection as i32,
            ..ReindexRequest::default()
        })
        .await
        .unwrap()
        .into_inner();
    let first = tokio::time::timeout(STREAM_TIMEOUT, stream.next())
        .await
        .expect("a first frame")
        .expect("a first frame")
        .unwrap();
    assert!(!first.done, "there is far more work than one batch");

    drop(stream);
    drop(client);

    // Wait for the producer to settle. `QUIET_SAMPLES` consecutive identical
    // readings, not one: a single stable interval only proves no job *finished*
    // in it, and under load one job can easily outlast one sample — which would
    // have this call the producer stopped while a batch of sixteen was still
    // mid-flight, and then blame the leases it was legitimately still holding.
    const QUIET_SAMPLES: u32 = 8;
    let mut previous = server.pipeline.jobs_run();
    let mut quiet = 0u32;
    let mut settled = false;
    for _ in 0..300 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let now = server.pipeline.jobs_run();
        quiet = if now == previous { quiet + 1 } else { 0 };
        previous = now;
        if quiet >= QUIET_SAMPLES {
            settled = true;
            break;
        }
    }
    assert!(settled, "the producer never stopped");

    let outstanding = server.pipeline.queue().stats().await.unwrap();
    assert!(
        outstanding.outstanding() > 0,
        "it stopped because the client left, not because it ran out of work: {outstanding:?}"
    );
    assert_eq!(
        outstanding.leased, 0,
        "and it handed its in-flight leases back rather than stranding them"
    );

    server.stop().await;
}

#[tokio::test]
async fn a_shutdown_closes_an_open_reindex_stream_rather_than_holding_it() {
    let server = TestServer::start().await;
    for n in 0..60 {
        server.message(&format!("Subject {n}"), "Body text.").await;
    }
    let mut client = server.client().await;
    let mut stream = client
        .reindex(ReindexRequest {
            mode: ReindexMode::Selection as i32,
            ..ReindexRequest::default()
        })
        .await
        .unwrap()
        .into_inner();
    // Deliberately left unread: the producer parks on the bounded channel, and
    // a token not wired to the daemon's shutdown would keep this connection —
    // and therefore the graceful shutdown — open indefinitely.
    let shutdown = tokio::time::timeout(STREAM_TIMEOUT, server.stop());
    assert!(
        shutdown.await.is_ok(),
        "shutdown must not wait on an open drain"
    );
    let ended = tokio::time::timeout(STREAM_TIMEOUT, stream.next()).await;
    assert!(ended.is_ok(), "the stream ends when the server does");
}

// ---------------------------------------------------------------------------
// Verify and Gc
// ---------------------------------------------------------------------------

#[tokio::test]
async fn verify_reports_drift_and_leaves_it_exactly_where_it_found_it() {
    let server = TestServer::start().await;
    let message_id = server.message("Subject", "The original body.").await;
    server.index_all().await;

    let clean = server
        .client()
        .await
        .verify(VerifyIndexRequest {})
        .await
        .unwrap()
        .into_inner();
    assert!(clean.clean, "a freshly indexed mailbox is clean: {clean:?}");

    server
        .db
        .write(move |c| {
            c.execute(
                "UPDATE index_content SET text = 'moved', content_hash = X'DEADBEEF'
                 WHERE message_id = ?1 AND part = 'body'",
                [message_id],
            )
        })
        .await
        .unwrap();

    let drift = server
        .client()
        .await
        .verify(VerifyIndexRequest {})
        .await
        .unwrap()
        .into_inner();
    assert!(!drift.clean);
    assert_eq!(
        drift.content_hash_drift, 3,
        "one per downstream stage: {drift:?}"
    );

    // Read-only: nothing was enqueued and nothing was repaired.
    assert_eq!(server.status().await.queue_ready, 0);
    let again = server
        .client()
        .await
        .verify(VerifyIndexRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        again.content_hash_drift, 3,
        "verify reports, it does not fix"
    );

    // And `reindex` is the repair path the drift points at.
    let repaired = server.index_all().await;
    assert_eq!(repaired.enqueued, 3);
    let after = server
        .client()
        .await
        .verify(VerifyIndexRequest {})
        .await
        .unwrap()
        .into_inner();
    assert_eq!(after.content_hash_drift, 0);

    server.stop().await;
}

#[tokio::test]
async fn gc_removes_orphans_and_leaves_live_rows_alone() {
    let server = TestServer::start().await;
    let doomed = server
        .message("Invoice", "Invoice INV-2024-0231 from ada@example.com.")
        .await;
    let survivor = server
        .message(
            "Shipping",
            "Tracking 1Z999AA10123456784 from bob@example.com.",
        )
        .await;
    server.index_all().await;

    let live_chunks = server
        .db
        .with_read(move |c| {
            c.query_row(
                "SELECT count(*) FROM chunks WHERE message_id = ?1",
                [survivor],
                |r| r.get::<_, i64>(0),
            )
        })
        .unwrap();
    assert!(live_chunks > 0);

    server
        .db
        .write(move |c| c.execute("DELETE FROM messages WHERE id = ?1", [doomed]))
        .await
        .unwrap();

    let mut client = server.client().await;
    let report = client
        .gc(IndexGcRequest {
            purge_search_caches: false,
        })
        .await
        .unwrap()
        .into_inner();
    assert!(
        report.vectors > 0,
        "the deleted message left vectors behind"
    );
    assert!(report.entities > 0, "and entities with no mention left");

    // The negative half, which matters more: a collector that takes a live
    // vector is catastrophic and silent — search simply stops returning a
    // message, with nothing anywhere to say why.
    let after_chunks = server
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
    let live_vectors = server
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
    assert_eq!(
        server.count("fts_messages"),
        1,
        "and the survivor's lexical row"
    );

    let drift = client
        .verify(VerifyIndexRequest {})
        .await
        .unwrap()
        .into_inner();
    assert!(drift.clean, "gc left a clean index: {drift:?}");

    // And a second run finds nothing to do.
    let again = client
        .gc(IndexGcRequest {
            purge_search_caches: false,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        (
            again.entities,
            again.vectors,
            again.lexical_rows,
            again.content_rows
        ),
        (0, 0, 0, 0)
    );

    server.stop().await;
}

// ---------------------------------------------------------------------------
// Rebuild
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rebuild_without_confirm_deletes_nothing() {
    let server = TestServer::start().await;
    server.message("Subject", "Body text.").await;
    server.index_all().await;
    let chunks = server.count("chunks");
    assert!(chunks > 0);

    let status = server
        .client()
        .await
        .rebuild(RebuildRequest {
            kinds: Vec::new(),
            confirm: false,
            max_jobs: 0,
        })
        .await
        .expect_err("an unconfirmed wipe is refused");
    assert_eq!(status.code(), Code::FailedPrecondition);
    assert!(
        status.message().contains("confirm"),
        "and it says how to ask for it: {}",
        status.message()
    );
    assert_eq!(server.count("chunks"), chunks, "nothing was deleted");
    assert_eq!(server.count("fts_messages"), 1);

    server.stop().await;
}

#[tokio::test]
async fn rebuild_wipes_the_derived_data_and_recomputes_it() {
    let server = TestServer::start().await;
    for n in 0..3 {
        server
            .message(&format!("Invoice {n}"), "Invoice INV-2024-0231 is due.")
            .await;
    }
    server.index_all().await;
    assert!(server.count("chunks") > 0);

    let mut client = server.client().await;
    let mut stream = client
        .rebuild(RebuildRequest {
            kinds: Vec::new(),
            confirm: true,
            max_jobs: 0,
        })
        .await
        .unwrap()
        .into_inner();
    let mut last = None;
    while let Some(frame) = tokio::time::timeout(STREAM_TIMEOUT, stream.next())
        .await
        .expect("the rebuild finishes")
    {
        let frame = frame.unwrap();
        let done = frame.done;
        last = Some(frame);
        if done {
            break;
        }
    }
    let last = last.expect("a final frame");
    assert!(last.done);
    assert!(last.dropped > 0, "a rebuild is a wipe: {last:?}");
    assert_eq!(last.enqueued, 3, "one extract job per message");
    assert_eq!(last.remaining, 0);

    // Rebuilt from scratch: same coverage, and a clean verify.
    let status = server.status().await;
    for stage in [ProtoKind::Lexical, ProtoKind::Entities, ProtoKind::Semantic] {
        assert_eq!(kind(&status, stage).indexed, 3, "{stage:?}");
    }
    assert!(server.count("chunks") > 0);
    let drift = client
        .verify(VerifyIndexRequest {})
        .await
        .unwrap()
        .into_inner();
    assert!(drift.clean, "{drift:?}");

    server.stop().await;
}

#[tokio::test]
async fn rebuilding_one_stage_leaves_the_others_intact() {
    let server = TestServer::start().await;
    server
        .message("Invoice", "Invoice INV-2024-0231 is due.")
        .await;
    server.index_all().await;
    let chunks = server.count("chunks");

    let mut client = server.client().await;
    let mut stream = client
        .rebuild(RebuildRequest {
            kinds: vec![ProtoKind::Lexical as i32],
            confirm: true,
            max_jobs: 0,
        })
        .await
        .unwrap()
        .into_inner();
    while let Some(frame) = stream.next().await {
        if frame.unwrap().done {
            break;
        }
    }

    assert_eq!(
        server.count("chunks"),
        chunks,
        "the semantic index survived"
    );
    assert!(server.count("entities") > 0, "and the entity graph");
    assert_eq!(
        server.count("fts_messages"),
        1,
        "and the lexical index was rebuilt, not merely emptied"
    );

    server.stop().await;
}

// ---------------------------------------------------------------------------
// Entities
// ---------------------------------------------------------------------------

#[tokio::test]
async fn entities_are_listed_by_kind_and_an_unknown_kind_is_an_error() {
    let server = TestServer::start().await;
    server
        .message("One", "Write to ada@example.com about the invoice.")
        .await;
    server
        .message("Two", "Also ada@example.com, plus bob@example.com.")
        .await;
    server.index_all().await;

    let mut client = server.client().await;
    let response = client
        .list_entities(ListEntitiesRequest {
            kind: "email".to_owned(),
            value: None,
            limit: 0,
        })
        .await
        .unwrap()
        .into_inner();
    let ada = response
        .entities
        .iter()
        .find(|e| e.norm == "ada@example.com")
        .expect("ada was extracted");
    assert_eq!(ada.kind, "email");
    assert_eq!(ada.messages, 2);

    let filtered = client
        .list_entities(ListEntitiesRequest {
            kind: "email".to_owned(),
            value: Some("BOB".to_owned()),
            limit: 10,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(filtered.entities.len(), 1, "the filter folds case");

    let status = client
        .list_entities(ListEntitiesRequest {
            kind: "not_a_kind".to_owned(),
            value: None,
            limit: 10,
        })
        .await
        .expect_err("an unknown kind is a mistake, not an empty page");
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("tracking_no"),
        "and it says what the real kinds are: {}",
        status.message()
    );

    server.stop().await;
}

/// The operator surface for task 36's caches: `Status` reports what they hold
/// and `Gc` is the way to clear them.
///
/// A cache nobody outside `rmail-core` can inspect or clear is a capability
/// with no surface, and the person who needs both is an operator asking "why
/// is search still returning the old ordering" — the corpus version is the
/// number that answers it.
#[tokio::test]
async fn status_reports_the_search_caches_and_gc_clears_them() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let before = client
        .status(IndexStatusRequest {})
        .await
        .unwrap()
        .into_inner()
        .cache
        .expect("Status must carry the cache block");

    // Mail is what moves the corpus version, and a moved version is what makes
    // every result page cached before it unreadable.
    server.message("hello", "a body").await;
    let after = client
        .status(IndexStatusRequest {})
        .await
        .unwrap()
        .into_inner()
        .cache
        .expect("cache block");
    assert!(
        after.corpus_version > before.corpus_version,
        "new mail must move the corpus version an operator can see: \
         {} -> {}",
        before.corpus_version,
        after.corpus_version
    );

    // Seed one row in each cache with raw SQL — this test is about the
    // operator surface, not about how the search path fills them.
    let account_id = server.account_id;
    // Stamped with the version *before* the message above landed, which is
    // what a page cached a moment earlier would carry. Derived rather than
    // hardcoded: the fixture's starting version is not this test's to assume,
    // and a literal that happened to equal the current version would make the
    // "stale" assertion below vacuously test nothing.
    let superseded = after.corpus_version - 1;
    server
        .db
        .write(move |c| {
            c.execute(
                "INSERT INTO query_plan_cache
                     (account_id, query_hash, raw, compiled, intent, notes, model)
                 VALUES (?1, 'abc123', 'who owes me', 'invoice', 'lookup', '', 'test')",
                rusqlite::params![account_id],
            )?;
            c.execute(
                "INSERT INTO embedding_cache (model, dim, text_hash, vector)
                 VALUES ('test', 2, X'aa', X'0000000000000000')",
                [],
            )?;
            c.execute(
                "INSERT INTO search_result_cache
                     (cache_key, corpus_version, ranker_fingerprint, message_ids)
                 VALUES (X'bb', ?1, X'cc', X'0100000000000000')",
                rusqlite::params![superseded],
            )
        })
        .await
        .unwrap();

    let seeded = client
        .status(IndexStatusRequest {})
        .await
        .unwrap()
        .into_inner()
        .cache
        .expect("cache block");
    assert_eq!(seeded.query_plans, 1);
    assert_eq!(seeded.embeddings, 1);
    assert_eq!(seeded.results, 1);
    assert_eq!(
        seeded.stale_results, 1,
        "the seeded page is stamped with a superseded version, so no lookup \
         can address it again — and an operator can see that"
    );

    // A plain Gc sweeps what is already unreachable and leaves the paid
    // artifacts alone.
    let swept = client
        .gc(IndexGcRequest {
            purge_search_caches: false,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(swept.cache_results, 1, "the stranded page goes");
    assert_eq!(
        swept.cache_query_plans, 0,
        "a compiled plan costs a provider call to rebuild, so a routine gc \
         must never discard one"
    );
    let after_sweep = client
        .status(IndexStatusRequest {})
        .await
        .unwrap()
        .into_inner()
        .cache
        .expect("cache block");
    assert_eq!(after_sweep.query_plans, 1, "still there");
    assert_eq!(after_sweep.results, 0);

    // The purge is the opt-in half, and it does say what it cost.
    let purged = client
        .gc(IndexGcRequest {
            purge_search_caches: true,
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(purged.cache_query_plans, 1);
    assert_eq!(purged.cache_embeddings, 1);
    let empty = client
        .status(IndexStatusRequest {})
        .await
        .unwrap()
        .into_inner()
        .cache
        .expect("cache block");
    assert_eq!(
        (empty.query_plans, empty.embeddings, empty.results),
        (0, 0, 0)
    );

    server.stop().await;
}
