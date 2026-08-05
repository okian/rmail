//! The `SyncService` gRPC implementation.
//!
//! Four unary RPCs wrap [`rmail_core::sync::SyncEngine`]; the fifth,
//! `WatchEvents`, is the one with real design in it.
//!
//! # Streaming without a gap and without a leak
//!
//! A client resuming a stream needs two things that pull in opposite
//! directions: everything it missed, and everything that happens from now on,
//! with no event falling between them and none delivered twice. The handler
//! subscribes to the live tail *before* reading the durable backlog
//! ([`rmail_core::events::EventLog::catch_up`]), replays the backlog, then
//! follows the tail while discarding anything at or below the cursor the
//! backlog reached. Draining first and subscribing after would drop whatever
//! committed in between — a window that is empty on a quiet mailbox and wide
//! open on a busy one.
//!
//! # Cancellation
//!
//! A dropped client must stop the work behind it, not merely stop being read.
//! The stream is driven by a spawned task feeding a bounded channel; dropping
//! the receiver — which is what a disconnect does — closes the channel and the
//! task observes that on its next send and exits. Nothing polls a subscription
//! nobody is listening to.
//!
//! Backpressure is deliberate too: the channel is bounded, so a slow client
//! slows its own stream rather than growing a queue in the daemon. If it falls
//! far enough behind that the broadcast tail laps it, it is told
//! `OUT_OF_RANGE` with a cursor to resume from rather than silently skipping
//! events.
//
// `tonic::Status` is intentionally the error type throughout a gRPC service
// boundary; its size makes `result_large_err` fire on every `Result<_, Status>`
// helper, so the lint is allowed for this module.
#![allow(clippy::result_large_err)]

use std::collections::HashSet;
use std::pin::Pin;

use rmail_core::events::{Event as CoreEvent, EventKind};
use rmail_core::sync::{SyncEngine, SyncMode as CoreSyncMode};
use rmail_core::Error;
use rmail_proto::v1::sync_service_server::SyncService;
use rmail_proto::v1::{
    Event as ProtoEvent, EventKind as ProtoEventKind, FolderStatus, FolderSyncResult, PauseRequest,
    PauseResponse, ResumeRequest, ResumeResponse, SyncFolderRequest, SyncFolderResponse, SyncMode,
    SyncStatusRequest, SyncStatusResponse, WatchEventsRequest,
};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};

/// How many events may sit between the log and a client before the stream
/// applies backpressure.
///
/// Bounded on purpose: an unbounded channel turns one slow client into daemon
/// memory growth, and the client cannot tell the difference.
const STREAM_BUFFER: usize = 256;

/// How many backlog events one durable read fetches while catching a client up.
const REPLAY_PAGE: i64 = 500;

/// The `SyncService` handler.
#[derive(Clone)]
pub struct SyncApi {
    engine: SyncEngine,
    /// Cancelled when the daemon shuts down, so in-flight passes and open
    /// streams stop with it rather than holding shutdown open.
    shutdown: CancellationToken,
}

impl SyncApi {
    /// Create a handler over a sync engine.
    #[must_use]
    pub fn new(engine: SyncEngine, shutdown: CancellationToken) -> Self {
        Self { engine, shutdown }
    }
}

#[tonic::async_trait]
impl SyncService for SyncApi {
    async fn sync_folder(
        &self,
        request: Request<SyncFolderRequest>,
    ) -> Result<Response<SyncFolderResponse>, Status> {
        // The RPC's own cancellation, not just the daemon's: a client that goes
        // away should stop the sync it asked for.
        let cancel = rpc_cancel(&self.shutdown);
        let req = request.into_inner();
        let mode = match req.mode() {
            SyncMode::Full => CoreSyncMode::Full,
            SyncMode::Unspecified | SyncMode::Auto => CoreSyncMode::Auto,
        };

        let report = self
            .engine
            .sync(req.account_id, req.mailbox_id, mode, &cancel)
            .await?;

        Ok(Response::new(SyncFolderResponse {
            folders: report
                .folders
                .into_iter()
                .map(|folder| FolderSyncResult {
                    mailbox_id: folder.mailbox_id,
                    mailbox_name: folder.name,
                    strategy: folder.strategy,
                    new_messages: cast(folder.new_messages),
                    flag_updates: cast(folder.flag_updates),
                    expunged: cast(folder.expunged),
                    error: folder.error,
                })
                .collect(),
            latest_seq: report.latest_seq,
        }))
    }

    async fn status(
        &self,
        request: Request<SyncStatusRequest>,
    ) -> Result<Response<SyncStatusResponse>, Status> {
        let account_id = request.into_inner().account_id;
        let folders = self.engine.status(account_id).await?;
        Ok(Response::new(SyncStatusResponse {
            folders: folders
                .into_iter()
                .map(|f| FolderStatus {
                    mailbox_id: f.mailbox_id,
                    name: f.name,
                    uidvalidity: f.uidvalidity,
                    uidnext: f.uidnext,
                    highestmodseq: f.highestmodseq,
                    last_synced_uid: f.last_synced_uid,
                    walked_down_to: f.walked_down_to,
                    full_sync_done: f.full_sync_done,
                    last_sync_at: f.last_sync_at,
                    message_count: f.message_count,
                })
                .collect(),
            paused: self.engine.is_paused(account_id),
        }))
    }

    async fn pause(
        &self,
        request: Request<PauseRequest>,
    ) -> Result<Response<PauseResponse>, Status> {
        let account_id = request.into_inner().account_id;
        self.engine.pause(account_id);
        Ok(Response::new(PauseResponse {
            paused: self.engine.is_paused(account_id),
        }))
    }

    async fn resume(
        &self,
        request: Request<ResumeRequest>,
    ) -> Result<Response<ResumeResponse>, Status> {
        let account_id = request.into_inner().account_id;
        self.engine.resume(account_id);
        Ok(Response::new(ResumeResponse {
            paused: self.engine.is_paused(account_id),
        }))
    }

    type WatchEventsStream =
        Pin<Box<dyn tokio_stream::Stream<Item = Result<ProtoEvent, Status>> + Send + 'static>>;

    async fn watch_events(
        &self,
        request: Request<WatchEventsRequest>,
    ) -> Result<Response<Self::WatchEventsStream>, Status> {
        let cancel = rpc_cancel(&self.shutdown);
        let req = request.into_inner();
        let filter = Filter::new(req.account_id, &req.kinds);

        // Subscribe before reading the backlog. The other order leaves a window
        // — empty on a quiet mailbox, wide open on a busy one — in which an
        // event is neither in the backlog nor on the tail.
        let log = self.engine.events().clone();
        let mut catchup = log.catch_up(req.since_seq, REPLAY_PAGE).await?;

        let (tx, rx) = tokio::sync::mpsc::channel(STREAM_BUFFER);
        tokio::spawn(async move {
            let mut cursor = req.since_seq;
            let mut page = std::mem::take(&mut catchup.backlog);
            let mut scanned_to = catchup.next_seq;

            // Replay the durable backlog, paging until it is exhausted. A
            // client that was away for a week is caught up a page at a time
            // rather than in one allocation.
            loop {
                let drained = page.is_empty();
                for event in page {
                    cursor = cursor.max(event.seq);
                    if filter.admits(&event) && tx.send(Ok(to_proto(&event))).await.is_err() {
                        return;
                    }
                }
                // Advance past what the read *scanned*, not merely what it
                // returned: a filtered subscription would otherwise re-read the
                // same unmatched span forever.
                cursor = cursor.max(scanned_to);
                if drained {
                    break;
                }
                match log.since(cursor, REPLAY_PAGE).await {
                    Ok(next) => {
                        page = next.events;
                        scanned_to = next.next_seq;
                    }
                    Err(error) => {
                        let _ = tx.send(Err(Status::from(error))).await;
                        return;
                    }
                }
            }

            // Then follow the tail, discarding what the backlog already
            // delivered.
            loop {
                let received = tokio::select! {
                    () = cancel.cancelled() => return,
                    received = catchup.live.recv() => received,
                };
                match received {
                    Ok(event) => {
                        if event.seq <= cursor {
                            continue;
                        }
                        cursor = event.seq;
                        if filter.admits(&event) && tx.send(Ok(to_proto(&event))).await.is_err() {
                            return;
                        }
                    }
                    // Lagged past the broadcast buffer: the client has not lost
                    // data, it has lost its place. Telling it so with a cursor
                    // is the only honest answer — silently skipping ahead would
                    // leave it believing it had seen everything.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(missed)) => {
                        tracing::warn!(missed, cursor, "event stream subscriber lagged");
                        let error = Error::resume_gap(
                            format!("stream fell {missed} events behind; resume from the cursor"),
                            cursor + 1,
                        );
                        let _ = tx.send(Err(Status::from(error))).await;
                        return;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                }
            }
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }
}

/// Which events a subscription wants.
struct Filter {
    account_id: Option<i64>,
    kinds: HashSet<EventKind>,
}

impl Filter {
    fn new(account_id: i64, kinds: &[i32]) -> Self {
        // 0 is the proto default for an unset int64, and no account has id 0,
        // so it is unambiguous as "every account".
        let account_id = (account_id != 0).then_some(account_id);
        let kinds: HashSet<EventKind> = kinds
            .iter()
            .filter_map(|k| ProtoEventKind::try_from(*k).ok())
            .filter_map(from_proto_kind)
            .collect();
        Self { account_id, kinds }
    }

    fn admits(&self, event: &CoreEvent) -> bool {
        if let Some(account_id) = self.account_id {
            if event.account_id != Some(account_id) {
                return false;
            }
        }
        // An empty set means every kind, not no kinds — an unset repeated field
        // is how a client says "no preference".
        self.kinds.is_empty() || self.kinds.contains(&event.kind)
    }
}

/// The cancellation token an RPC's work runs under.
///
/// A child of the daemon's shutdown token, so nothing outlives the process
/// stopping. Client disconnect is handled by two mechanisms that need no token:
/// tonic drops a unary handler's future when the peer goes away, and a dropped
/// response stream closes the channel its producer task sends on, which that
/// task observes on its next send. A token layered on top of those would be a
/// third way to say the same thing and a fourth thing to keep consistent.
fn rpc_cancel(shutdown: &CancellationToken) -> CancellationToken {
    shutdown.child_token()
}

fn to_proto(event: &CoreEvent) -> ProtoEvent {
    ProtoEvent {
        seq: event.seq,
        kind: to_proto_kind(event.kind) as i32,
        at: event.at,
        account_id: event.account_id,
        mailbox_id: event.mailbox_id,
        message_id: event.message_id,
        payload: event.payload.to_string(),
    }
}

fn to_proto_kind(kind: EventKind) -> ProtoEventKind {
    match kind {
        EventKind::NewMail => ProtoEventKind::NewMail,
        EventKind::FlagChanged => ProtoEventKind::FlagChanged,
        EventKind::Moved => ProtoEventKind::Moved,
        EventKind::Deleted => ProtoEventKind::Deleted,
        EventKind::SyncState => ProtoEventKind::SyncState,
        EventKind::SendResult => ProtoEventKind::SendResult,
        EventKind::RuleFired => ProtoEventKind::RuleFired,
        EventKind::AiSummary => ProtoEventKind::AiSummary,
    }
}

fn from_proto_kind(kind: ProtoEventKind) -> Option<EventKind> {
    Some(match kind {
        // An unspecified kind in a filter list is a client that meant nothing
        // by it; ignoring it is friendlier than rejecting the whole request.
        ProtoEventKind::Unspecified => return None,
        ProtoEventKind::NewMail => EventKind::NewMail,
        ProtoEventKind::FlagChanged => EventKind::FlagChanged,
        ProtoEventKind::Moved => EventKind::Moved,
        ProtoEventKind::Deleted => EventKind::Deleted,
        ProtoEventKind::SyncState => EventKind::SyncState,
        ProtoEventKind::SendResult => EventKind::SendResult,
        ProtoEventKind::RuleFired => EventKind::RuleFired,
        ProtoEventKind::AiSummary => EventKind::AiSummary,
    })
}

/// Counts cross the wire as `int64`; saturating is right because a count that
/// large is already a bug and wrapping it would hide one.
fn cast(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}
