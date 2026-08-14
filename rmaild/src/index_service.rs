//! The `IndexService` gRPC implementation.
//!
//! Five unary RPCs are thin wrappers over [`rmail_core::index::IndexAdmin`];
//! `Reindex` and `Rebuild` are the two with design in them, and they share a
//! producer.
//!
//! # Streaming a drain rather than blocking on one
//!
//! A first index of a large mailbox is hours of work. A unary RPC would give
//! the caller one bit of information — it finished, or the deadline killed it —
//! and no way to tell a slow index from a wedged one. So the drain runs in a
//! spawned task feeding a bounded channel, and a progress frame goes out after
//! every batch.
//!
//! The channel is bounded and every frame is *awaited*, so a client that stops
//! reading parks the drain rather than having its frames quietly dropped while
//! the work it walked away from runs to completion. That is the same
//! backpressure `sync_service`'s event stream applies, and here it is load
//! bearing rather than merely tidy: "the client stopped reading" and "stop
//! indexing" have to be the same event for the contract below to hold.
//!
//! # Cancellation
//!
//! Dropping the response stream drops the receiver, which closes the channel,
//! which fails the next send and stops the drain at the batch boundary. That is
//! a real stop, not merely an unread stream: the producer stops leasing, hands
//! back the leases it is holding, and leaves the rest queued for the background
//! worker. The daemon's own shutdown token is the parent of every RPC's, so a
//! graceful shutdown ends an open drain rather than waiting on it.
//!
//! In-flight leases are handed back rather than left to lapse — see
//! [`rmail_core::index::IndexQueue::release`] for why the attempt is rolled
//! back with them.
//!
//! # Why `Rebuild` is its own RPC and not a mode of `Reindex`
//!
//! The scope table (`auth::methods`) is keyed by method path and cannot see a
//! request's fields. Folding a destructive wipe into `Reindex` as an enum value
//! would mean the whole RPC had to carry the wipe's scope — making every
//! routine `mail index run` require `admin` — or, far worse, that the wipe was
//! reachable with `mail.write`. Two methods is the only way this table can
//! express "one of these is much more dangerous than the other."
//
// `tonic::Status` is intentionally the error type throughout a gRPC service
// boundary; its size makes `result_large_err` fire on every `Result<_, Status>`
// helper, so the lint is allowed for this module.
#![allow(clippy::result_large_err)]

use std::pin::Pin;
use std::sync::atomic::AtomicU64;
use std::sync::Arc;

use rmail_core::index::{
    IndexAdmin, IndexKind as CoreKind, IndexPauseFlag, IndexPipeline, Selection,
};
use rmail_core::Error;
use rmail_proto::v1::index_service_server::IndexService;
use rmail_proto::v1::{
    IndexDrift, IndexEntity, IndexGcReport, IndexGcRequest, IndexKind as ProtoKind,
    IndexKindStatus, IndexProgress, IndexStatusRequest, IndexStatusResponse, ListEntitiesRequest,
    ListEntitiesResponse, RebuildRequest, ReindexMode, ReindexRequest, SetIndexPausedRequest,
    SetIndexPausedResponse, VerifyIndexRequest,
};
use tokio::sync::{mpsc, OwnedSemaphorePermit, Semaphore};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};
use tracing::Instrument;

/// How many progress frames may sit between the drain and a client.
///
/// Bounded, like every other stream in this daemon: an unbounded channel turns
/// one slow client into daemon memory growth. Small, because one frame is one
/// batch of work — a client that has not read four of them is not reading, and
/// the sooner the drain notices that the less work it does for nobody.
const STREAM_BUFFER: usize = 4;

/// Jobs leased per batch, and therefore per progress frame.
///
/// Deliberately the pipeline's own default rather than a larger number: the
/// batch is the granularity at which a disconnected client stops the work, and
/// a big batch would keep indexing for a while after the client had gone.
const LEASE_LIMIT: i64 = rmail_core::index::pipeline::DEFAULT_LEASE_LIMIT;

/// Default page size for `ListEntities`.
const DEFAULT_ENTITY_LIMIT: i64 = 50;

/// The `IndexService` handler.
#[derive(Clone)]
pub struct IndexApi {
    admin: IndexAdmin,
    pipeline: IndexPipeline,
    paused: IndexPauseFlag,
    /// One permit: at most one RPC-driven drain at a time.
    ///
    /// Not throughput management — a second concurrent drain does no extra
    /// work, since both lease from the same queue behind the same single
    /// writer. It is fencing. `complete`/`fail`/`release` are all guarded by
    /// `leased_by = worker`, and every clone of a pipeline carries the same
    /// worker name, so two drains under this handler could otherwise complete
    /// each other's leases after a reap. Refusing the second call is both the
    /// simpler answer and the more honest one.
    drain_permit: Arc<Semaphore>,
    /// Cancelled when the daemon shuts down, so an open drain stops with it
    /// rather than holding shutdown open.
    shutdown: CancellationToken,
}

impl IndexApi {
    /// Build the handler over the same admin surface and pipeline the
    /// background worker uses.
    ///
    /// Both are passed in rather than constructed here so that the `Reindex`
    /// this serves and the worker running on a timer are draining one queue
    /// through one set of stages — and, for a real ONNX embedder, one copy of
    /// the model weights.
    ///
    /// The pipeline is re-branded with a worker name of its own. It shares
    /// everything else with the caller's — the queue, the stages, the pause
    /// flag, the job counter — but a lease this handler takes must be
    /// distinguishable from one the background loop takes, or the
    /// `leased_by`-fencing on `complete`/`fail`/`release` compares two workers
    /// that call themselves the same thing and lets a stalled drain finish work
    /// that was reaped out from under it.
    #[must_use]
    pub fn new(admin: IndexAdmin, pipeline: IndexPipeline, shutdown: CancellationToken) -> Self {
        Self {
            paused: pipeline.pause_flag(),
            pipeline: pipeline.with_worker(format!(
                "rmaild-index-rpc-{}-{}",
                std::process::id(),
                DRAINS.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            )),
            admin,
            drain_permit: Arc::new(Semaphore::new(1)),
            shutdown,
        }
    }

    /// Take the drain permit, or say who has it.
    fn claim_drain(&self) -> Result<OwnedSemaphorePermit, Status> {
        Arc::clone(&self.drain_permit)
            .try_acquire_owned()
            .map_err(|_| {
                Status::from(Error::failed_precondition(
                    "an index pass is already running on this daemon; watch its stream or wait \
                     for it to finish",
                ))
            })
    }
}

/// Distinguishes one `IndexApi`'s leases from another's within a process.
static DRAINS: AtomicU64 = AtomicU64::new(0);

#[tonic::async_trait]
impl IndexService for IndexApi {
    async fn status(
        &self,
        _request: Request<IndexStatusRequest>,
    ) -> Result<Response<IndexStatusResponse>, Status> {
        let status = self.admin.status().await?;
        Ok(Response::new(IndexStatusResponse {
            kinds: status
                .kinds
                .iter()
                .map(|kind| IndexKindStatus {
                    kind: to_proto_kind(kind.kind) as i32,
                    enabled: kind.enabled,
                    eligible: kind.eligible,
                    indexed: kind.indexed,
                    coverage: kind.coverage(),
                    pending: kind.pending,
                    quarantined: kind.quarantined,
                    lag_seconds: kind.lag_seconds,
                })
                .collect(),
            messages: status.messages,
            queue_ready: status.queue.ready,
            queue_backing_off: status.queue.backing_off,
            queue_leased: status.queue.leased,
            queue_dead: status.queue.dead,
            model: status.model,
            dim: status.dim,
            chunks: status.chunks,
            vectors: status.vectors,
            paused: status.paused,
            semantic_enabled: status.semantic_enabled,
        }))
    }

    type ReindexStream =
        Pin<Box<dyn tokio_stream::Stream<Item = Result<IndexProgress, Status>> + Send + 'static>>;

    #[tracing::instrument(skip_all, fields(mode, max_jobs, kinds, message_id))]
    async fn reindex(
        &self,
        request: Request<ReindexRequest>,
    ) -> Result<Response<Self::ReindexStream>, Status> {
        let cancel = self.shutdown.child_token();
        let req = request.into_inner();
        let mode = req.mode();
        let max_jobs = non_negative("max_jobs", req.max_jobs)?;
        if let Some(since) = req.since {
            non_negative("since", since)?;
        }
        let selection = Selection {
            kinds: parse_kinds(&req.kinds)?,
            account_id: req.account_id,
            mailbox_id: req.mailbox_id,
            message_id: req.message_id,
            since: req.since,
        };
        let span = tracing::Span::current();
        span.record("mode", mode.as_str_name());
        span.record("max_jobs", max_jobs);
        span.record("kinds", tracing::field::debug(&selection.kinds));
        span.record("message_id", selection.message_id);
        let permit = self.claim_drain()?;

        let (tx, rx) = mpsc::channel(STREAM_BUFFER);
        let admin = self.admin.clone();
        let pipeline = self.pipeline.clone();
        tokio::spawn(
            async move {
                // Held for the life of the pass, so the permit is released
                // whether this returns, errors, or is dropped mid-drain.
                let _permit = permit;
                // The enqueue phase runs before any frame goes out and can be
                // minutes of paging on a large store. Racing it against the
                // token is what makes "dropping the stream stops the work" true
                // of the whole RPC rather than only of the drain that follows —
                // and what stops a graceful shutdown waiting on a selection
                // scan nobody is listening for.
                let enqueued = tokio::select! {
                    () = cancel.cancelled() => {
                        crate::stream::terminate_cancelled(&tx).await;
                        return;
                    }
                    enqueued = async {
                        match mode {
                            ReindexMode::Unspecified | ReindexMode::Drain => Ok(0),
                            ReindexMode::Selection => admin.reindex(&selection).await,
                            ReindexMode::EmbedBackfill => admin.backfill_embeddings().await,
                        }
                    } => enqueued,
                };
                let enqueued = match enqueued {
                    Ok(enqueued) => enqueued,
                    Err(error) => {
                        let _ = tx.send(Err(Status::from(error))).await;
                        return;
                    }
                };
                drain(&pipeline, &tx, &cancel, enqueued, 0, max_jobs).await;
            }
            .instrument(tracing::Span::current()),
        );

        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    type RebuildStream =
        Pin<Box<dyn tokio_stream::Stream<Item = Result<IndexProgress, Status>> + Send + 'static>>;

    #[tracing::instrument(skip_all, fields(kinds, max_jobs, confirm = request.get_ref().confirm))]
    async fn rebuild(
        &self,
        request: Request<RebuildRequest>,
    ) -> Result<Response<Self::RebuildStream>, Status> {
        let cancel = self.shutdown.child_token();
        let req = request.into_inner();
        let max_jobs = non_negative("max_jobs", req.max_jobs)?;
        let kinds = parse_kinds(&req.kinds)?;
        let span = tracing::Span::current();
        span.record("kinds", tracing::field::debug(&kinds));
        span.record("max_jobs", max_jobs);
        if !req.confirm {
            // Checked before anything is deleted, and reported as a
            // precondition rather than an argument error: the request is
            // well-formed, the *daemon* is refusing to wipe an index nobody
            // said out loud they wanted wiped.
            return Err(Status::from(Error::failed_precondition(
                "rebuild deletes the index for the named stages and search over them returns \
                 nothing until it is recomputed; set confirm to proceed",
            )));
        }

        let permit = self.claim_drain()?;
        let (tx, rx) = mpsc::channel(STREAM_BUFFER);
        let admin = self.admin.clone();
        let pipeline = self.pipeline.clone();
        tokio::spawn(
            async move {
                let _permit = permit;
                // The wipe and its re-enqueue are deliberately *not* raced
                // against the token the way `Reindex`'s selection scan is.
                // Abandoning them halfway is the one outcome worse than either
                // end of this operation: derived data gone with nothing queued
                // to recompute it, and no event in the log to make the
                // background loop notice. `IndexAdmin::rebuild` documents the
                // residual window between its own two transactions and the
                // one-command recovery for it.
                let report = match admin.rebuild(&kinds).await {
                    Ok(report) => report,
                    Err(error) => {
                        let _ = tx.send(Err(Status::from(error))).await;
                        return;
                    }
                };
                drain(
                    &pipeline,
                    &tx,
                    &cancel,
                    report.enqueued,
                    report.dropped,
                    max_jobs,
                )
                .await;
            }
            .instrument(tracing::Span::current()),
        );

        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    async fn verify(
        &self,
        _request: Request<VerifyIndexRequest>,
    ) -> Result<Response<IndexDrift>, Status> {
        // Raced against shutdown like the streams are, and for the same
        // reason: reconciling a large mailbox walks every `index_state` row
        // against every `index_content` row, which is minutes, and a graceful
        // shutdown should not wait for a reconciliation nobody will read.
        let drift = stoppable(&self.shutdown, self.admin.verify()).await?;
        Ok(Response::new(IndexDrift {
            content_hash_drift: drift.content_hash_drift,
            extract_missing: drift.extract_missing,
            lexical_missing: drift.lexical_missing,
            lexical_orphaned: drift.lexical_orphaned,
            entity_orphaned: drift.entity_orphaned,
            chunks_unembedded: drift.semantic.missing,
            chunks_unvectored: drift.semantic.unvectored,
            chunks_wrong_model: drift.semantic.wrong_model,
            chunks_stale: drift.semantic.stale,
            vectors_orphaned: drift.semantic.orphaned,
            message_vectors_stale: drift.semantic.message_vectors,
            quarantined: drift.quarantined,
            clean: drift.is_clean(),
        }))
    }

    async fn gc(
        &self,
        _request: Request<IndexGcRequest>,
    ) -> Result<Response<IndexGcReport>, Status> {
        // Batched deletes over the whole store; same reasoning as `verify`,
        // with the extra note that stopping between batches is safe — each one
        // commits on its own and only ever removes rows whose parent is gone.
        let report = stoppable(&self.shutdown, self.admin.gc()).await?;
        Ok(Response::new(IndexGcReport {
            entities: cast(report.entities),
            vectors: cast(report.vectors),
            lexical_rows: cast(report.lexical_rows),
            content_rows: cast(report.content_rows),
        }))
    }

    async fn set_paused(
        &self,
        request: Request<SetIndexPausedRequest>,
    ) -> Result<Response<SetIndexPausedResponse>, Status> {
        let paused = request.into_inner().paused;
        self.paused.set(paused);
        tracing::info!(paused, "background indexing switched");
        Ok(Response::new(SetIndexPausedResponse {
            paused: self.paused.get(),
        }))
    }

    async fn list_entities(
        &self,
        request: Request<ListEntitiesRequest>,
    ) -> Result<Response<ListEntitiesResponse>, Status> {
        let req = request.into_inner();
        let limit = if req.limit <= 0 {
            DEFAULT_ENTITY_LIMIT
        } else {
            req.limit
        };
        let value = req
            .value
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty());
        let rows = self.admin.list_entities(&req.kind, value, limit).await?;
        Ok(Response::new(ListEntitiesResponse {
            entities: rows
                .into_iter()
                .map(|row| IndexEntity {
                    entity_id: row.entity_id,
                    kind: row.kind,
                    value: row.value,
                    norm: row.norm,
                    meta: row.meta.unwrap_or_default(),
                    mentions: row.mentions,
                    messages: row.messages,
                })
                .collect(),
        }))
    }
}

/// Run a drain, emitting a progress frame per batch and a final one carrying
/// `done`.
///
/// Shared by `Reindex` and `Rebuild` because the only thing that differs
/// between them is what happened before the first batch.
async fn drain(
    pipeline: &IndexPipeline,
    tx: &mpsc::Sender<Result<IndexProgress, Status>>,
    cancel: &CancellationToken,
    enqueued: u64,
    dropped: u64,
    max_jobs: u64,
) {
    let outcome = pipeline
        .drain(LEASE_LIMIT, max_jobs, cancel, |report, outstanding| {
            // A clone, not a borrow: the returned future outlives the closure
            // call, and an `mpsc::Sender` clone is a refcount bump.
            let tx = tx.clone();
            let cancel = cancel.clone();
            async move {
                let frame = IndexProgress {
                    enqueued: cast(enqueued),
                    completed: cast(report.retired()),
                    failed: cast(report.failed),
                    remaining: outstanding,
                    dropped: cast(dropped),
                    done: false,
                };
                send(&tx, &cancel, Ok(frame)).await
            }
        })
        .await;

    match outcome {
        Ok(report) => {
            // Re-read rather than remembering the last frame's figure: the
            // drain may have stopped on `max_jobs` with work still queued, and
            // "remaining" on the final frame is the number an operator uses to
            // decide whether to run it again.
            let remaining = match pipeline.queue().stats().await {
                Ok(stats) => stats.outstanding(),
                Err(error) => {
                    tracing::warn!(%error, "could not read the queue depth for the final frame");
                    0
                }
            };
            let frame = IndexProgress {
                enqueued: cast(enqueued),
                completed: cast(report.retired()),
                failed: cast(report.failed),
                remaining,
                dropped: cast(dropped),
                done: true,
            };
            let _ = send(tx, cancel, Ok(frame)).await;
        }
        Err(error) => {
            let _ = send(tx, cancel, Err(Status::from(error))).await;
        }
    }
}

/// Send one progress frame, giving up if the client went away or the daemon is
/// stopping.
///
/// `tx.send` parks when the client stops reading — normal HTTP/2 flow control —
/// and shutdown is invisible from inside it. Racing the two is what keeps a
/// parked stream from holding a graceful shutdown open until its connection
/// times out, the same helper `sync_service` documents for its event stream.
async fn send(
    tx: &mpsc::Sender<Result<IndexProgress, Status>>,
    cancel: &CancellationToken,
    item: Result<IndexProgress, Status>,
) -> std::ops::ControlFlow<()> {
    tokio::select! {
        () = cancel.cancelled() => {
            // Never end a cancelled stream silently — see `crate::stream`.
            crate::stream::terminate_cancelled(tx).await;
            std::ops::ControlFlow::Break(())
        }
        sent = tx.send(item) => {
            if sent.is_ok() {
                std::ops::ControlFlow::Continue(())
            } else {
                std::ops::ControlFlow::Break(())
            }
        }
    }
}

/// Translate a repeated proto stage list into core stages.
///
/// An **empty** list means "every stage", which is what [`Selection`] and
/// [`IndexAdmin::rebuild`] both already read an empty `Vec` as.
///
/// `UNSPECIFIED` *inside* a non-empty list is rejected rather than folded into
/// that meaning, and the reason is `Rebuild`. Proto3 has no way to tell "the
/// client meant every stage" from "a default-valued enum got appended" — and
/// under the folding reading, `[LEXICAL, UNSPECIFIED]` would quietly widen a
/// request to wipe the lexical index into a request to wipe the extracted
/// text, the entity graph and every vector as well, with `confirm` already
/// satisfied. A request that can be read two ways, where one of the readings
/// destroys four times as much as the other, is not one to guess at.
fn parse_kinds(values: &[i32]) -> Result<Vec<CoreKind>, Status> {
    let mut kinds = Vec::with_capacity(values.len());
    for value in values {
        let kind = ProtoKind::try_from(*value).map_err(|_| {
            Status::from(Error::invalid_argument(format!(
                "unknown index kind {value} in the request"
            )))
        })?;
        match kind {
            ProtoKind::Unspecified => {
                return Err(Status::from(Error::invalid_argument(
                    "INDEX_KIND_UNSPECIFIED cannot be named alongside other stages; \
                     send an empty list to mean every stage",
                )))
            }
            ProtoKind::Extract => kinds.push(CoreKind::Extract),
            ProtoKind::Lexical => kinds.push(CoreKind::Lexical),
            ProtoKind::Entities => kinds.push(CoreKind::Entities),
            ProtoKind::Semantic => kinds.push(CoreKind::Semantic),
        }
    }
    Ok(kinds)
}

fn to_proto_kind(kind: CoreKind) -> ProtoKind {
    match kind {
        CoreKind::Extract => ProtoKind::Extract,
        CoreKind::Lexical => ProtoKind::Lexical,
        CoreKind::Entities => ProtoKind::Entities,
        CoreKind::Semantic => ProtoKind::Semantic,
        // Never reported: `IndexKind::PER_MESSAGE` is what `status` iterates,
        // and the thread rollup is not in it (see its own docs).
        CoreKind::Thread => ProtoKind::Unspecified,
    }
}

/// Run a long unary body, giving up if the daemon is stopping.
///
/// tonic drops a unary handler's future when the *peer* goes away, so client
/// disconnect needs no help; what it cannot see is the daemon shutting down
/// underneath it. Without this, a `Verify` or `Gc` over a large mailbox holds
/// its connection open — and therefore graceful shutdown — for however long the
/// scan takes.
async fn stoppable<T, F>(shutdown: &CancellationToken, work: F) -> Result<T, Status>
where
    F: std::future::Future<Output = Result<T, Error>>,
{
    tokio::select! {
        () = shutdown.cancelled() => Err(Status::from(Error::unavailable(
            "the daemon is shutting down; retry when it is back",
        ))),
        done = work => done.map_err(Status::from),
    }
}

/// Reject a negative count rather than saturating it to zero, which would turn
/// "I asked for at most -1 jobs" into an unbounded drain.
fn non_negative(field: &str, value: i64) -> Result<u64, Status> {
    u64::try_from(value).map_err(|_| {
        Status::from(Error::invalid_argument(format!(
            "{field} must not be negative"
        )))
    })
}

/// Counts cross the wire as `int64`; saturating is right because a count that
/// large is already a bug and wrapping it would hide one.
fn cast(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}
