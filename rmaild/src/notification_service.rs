//! The `NotificationService` gRPC implementation: what this daemon has
//! decided about a message (`ScoreMessage`) and a live, resumable stream of
//! what it actually fired (`StreamAlerts`).
//!
//! # `ScoreMessage` does not call a model
//!
//! It reads the durable decision if there is one and enqueues a scoring pass
//! if there is not, and that is deliberate rather than a shortcut. Scoring
//! runs through [`rmail_core::ai::queue`] so it inherits policy resolution,
//! the redaction firewall, the cost gate, the per-account budget, the shared
//! concurrency/rate limits and the audit ledger — see
//! `rmail_core::notify::score`'s module docs on why none of that may be
//! re-implemented at a call site. An RPC that scored synchronously would have
//! to either bypass those gates or hold a client connection open across a
//! queue lease, and both are worse than answering `QUEUED` honestly.
//!
//! # There is no `SetThreshold`
//!
//! Thresholds live in the operator's TOML (`[notify] threshold`, per-account
//! `[[accounts]] notify.threshold`), the same choice `HookService` made for
//! hook definitions and for the same reason: a setting that lives in a file
//! must not also live in a database this service then has to keep in sync
//! with it. What this service *does* expose is the **effective** threshold
//! after the per-account override, so a caller can explain a suppression
//! without parsing config itself.
//!
//! # `StreamAlerts` subscribes before it reads the backlog
//!
//! The other order leaves a window — empty on a quiet mailbox, wide open on a
//! busy one — in which an alert is neither in the backlog nor on the live
//! tail. Same shape, same reasoning, as `SyncService::WatchEvents`.
//
// `tonic::Status` is intentionally the error type throughout a gRPC service
// boundary; its size makes `result_large_err` fire on every
// `Result<_, Status>` helper, so the lint is allowed for this module — the
// same allowance `hook_service.rs`/`audit_service.rs` carry.
#![allow(clippy::result_large_err)]

use std::pin::Pin;

use rmail_core::ai::queue::{AiQueue, NewAiJob};
use rmail_core::notify::{self, NotifyEngine, Tier};
use rmail_core::storage::Database;
use rmail_core::Error;
use rmail_proto::v1::notification_service_server::NotificationService;
use rmail_proto::v1::{
    Alert as ProtoAlert, NotificationState, NotificationTier, ScoreMessageRequest,
    ScoreMessageResponse, StreamAlertsRequest,
};
use tokio::sync::broadcast;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};
use tracing::Instrument;

/// Bound on the per-stream channel. Matches `SyncService`'s own
/// `STREAM_BUFFER`: enough that a client doing real work per item is not
/// throttled, small enough that a stopped client cannot pin unbounded memory.
const STREAM_BUFFER: usize = 256;

/// How many alerts one backlog page reads.
const REPLAY_PAGE: i64 = 200;

/// The `NotificationService` handler.
#[derive(Debug, Clone)]
pub struct NotificationApi {
    db: Database,
    engine: NotifyEngine,
    queue: AiQueue,
    /// Whether `notify.enabled` is on. `ScoreMessage` refuses to *enqueue* a
    /// scoring pass when it is off — an RPC must not be a way to spend money
    /// the operator switched off at the config level — but still reports any
    /// decision already on record.
    scoring_enabled: bool,
    shutdown: CancellationToken,
}

impl NotificationApi {
    /// Build the handler.
    #[must_use]
    pub fn new(
        db: Database,
        engine: NotifyEngine,
        queue: AiQueue,
        scoring_enabled: bool,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            db,
            engine,
            queue,
            scoring_enabled,
            shutdown,
        }
    }
}

#[tonic::async_trait]
impl NotificationService for NotificationApi {
    #[tracing::instrument(skip(self, request), fields(message_id, state))]
    async fn score_message(
        &self,
        request: Request<ScoreMessageRequest>,
    ) -> Result<Response<ScoreMessageResponse>, Status> {
        let message_id = request.into_inner().message_id;
        tracing::Span::current().record("message_id", message_id);
        if message_id <= 0 {
            return Err(Status::from(Error::invalid_argument(
                "message_id must be positive",
            )));
        }
        let Some(scope) = message_scope(&self.db, message_id).await? else {
            return Err(Status::from(Error::not_found(format!(
                "message {message_id}"
            ))));
        };
        let (account_enabled, threshold) = self.engine.policy_for(&scope.account);

        let decided = notify::repo::state_of(&self.db, message_id)
            .await
            .map_err(Status::from)?;
        let mut response = ScoreMessageResponse {
            state: NotificationState::Unspecified as i32,
            tier: None,
            reason: None,
            suppressed_reason: String::new(),
            effective_threshold: threshold.to_string(),
            account_enabled,
            would_notify: false,
        };

        match decided {
            Some((state, id)) => {
                let row = notify::repo::decision(&self.db, id)
                    .await
                    .map_err(Status::from)?;
                response.state = to_proto_state(&state) as i32;
                if let Some(row) = row {
                    response.would_notify = threshold.admits(row.tier) && account_enabled;
                    response.tier = Some(to_proto_tier(row.tier) as i32);
                    response.reason = Some(row.reason);
                    response.suppressed_reason = row.suppressed_reason.unwrap_or_default();
                }
            }
            None => {
                if !self.scoring_enabled {
                    // Refusing to enqueue is the point: an RPC must not be a
                    // side door around `notify.enabled`, which is the switch
                    // that governs whether this daemon spends money per
                    // message at all.
                    return Err(Status::from(Error::failed_precondition(
                        "notify.enabled is false (or this daemon's AI subsystem is inactive); \
                         no message is scored for notification here",
                    )));
                }
                if !account_enabled {
                    // The same rule one level down. `NotifyPassHandler` would
                    // decline this job anyway — which is what protects the
                    // background enqueue path — but letting the RPC queue it
                    // first would mean answering QUEUED to a caller whose
                    // account can never produce a notification.
                    return Err(Status::from(Error::failed_precondition(format!(
                        "notifications are disabled for account {:?}",
                        scope.account
                    ))));
                }
                self.queue
                    .enqueue(vec![NewAiJob::new(
                        message_id,
                        scope.account_id,
                        notify::PASS,
                    )])
                    .await
                    .map_err(Status::from)?;
                response.state = NotificationState::Queued as i32;
            }
        }
        tracing::Span::current().record("state", response.state);
        Ok(Response::new(response))
    }

    type StreamAlertsStream =
        Pin<Box<dyn tokio_stream::Stream<Item = Result<ProtoAlert, Status>> + Send + 'static>>;

    #[tracing::instrument(skip(self, request), fields(since_id))]
    async fn stream_alerts(
        &self,
        request: Request<StreamAlertsRequest>,
    ) -> Result<Response<Self::StreamAlertsStream>, Status> {
        let since_id = request.into_inner().since_id;
        tracing::Span::current().record("since_id", since_id);
        if since_id.is_some_and(|id| id < 0) {
            return Err(Status::from(Error::invalid_argument(
                "since_id must not be negative",
            )));
        }
        let cancel = self.shutdown.child_token();

        // Subscribe first — see the module docs.
        let mut live = self.engine.subscribe();
        // An absent cursor means "from now on", so the backlog starts at the
        // current head rather than at the beginning of time; a present one —
        // including `0`, which replays everything, since ids start at 1 — is
        // an explicit request for history. See `StreamAlertsRequest`'s own
        // proto comment on why this is `optional` rather than a bare `int64`.
        let mut cursor = match since_id {
            Some(id) => id,
            None => self.engine.latest_alert_id().await.map_err(Status::from)?,
        };

        let engine = self.engine.clone();
        let (tx, rx) = tokio::sync::mpsc::channel(STREAM_BUFFER);
        tokio::spawn(
            async move {
                // Replay the durable backlog, paging until it is exhausted.
                loop {
                    let page = match engine.alerts_since(cursor, REPLAY_PAGE).await {
                        Ok(page) => page,
                        Err(error) => {
                            let _ = send(&tx, &cancel, Err(Status::from(error))).await;
                            return;
                        }
                    };
                    let drained = i64::try_from(page.len()).unwrap_or(i64::MAX) < REPLAY_PAGE;
                    for alert in page {
                        cursor = cursor.max(alert.id);
                        if send(&tx, &cancel, Ok(to_proto(&alert))).await.is_break() {
                            return;
                        }
                    }
                    if drained {
                        break;
                    }
                }

                // Then follow the live tail, discarding what the backlog
                // already delivered.
                loop {
                    let received = tokio::select! {
                        () = cancel.cancelled() => {
                            crate::stream::terminate_cancelled(&tx).await;
                            return;
                        }
                        // A client that has gone away is only *noticed* on the
                        // next send, and alerts are rare by construction —
                        // that rarity is the whole feature. Without this arm a
                        // task parked on `recv()` would outlive its client
                        // until the next notification fires, which can be
                        // hours. Watched explicitly rather than inferred from
                        // a failed send.
                        () = tx.closed() => return,
                        received = live.recv() => received,
                    };
                    match received {
                        Ok(alert) => {
                            if alert.id <= cursor {
                                continue;
                            }
                            cursor = alert.id;
                            if send(&tx, &cancel, Ok(to_proto(&alert))).await.is_break() {
                                return;
                            }
                        }
                        // Lagged past the broadcast buffer. Nothing is lost —
                        // every delivered alert is still a row — so the right
                        // answer is to go back and read them, not to fail the
                        // stream. Same recovery `WatchEvents` uses.
                        Err(broadcast::error::RecvError::Lagged(skipped)) => {
                            tracing::debug!(
                                skipped,
                                cursor,
                                "alert subscriber lagged; replaying from the durable table"
                            );
                            loop {
                                let page = match engine.alerts_since(cursor, REPLAY_PAGE).await {
                                    Ok(page) => page,
                                    Err(error) => {
                                        let _ = send(&tx, &cancel, Err(Status::from(error))).await;
                                        return;
                                    }
                                };
                                let drained =
                                    i64::try_from(page.len()).unwrap_or(i64::MAX) < REPLAY_PAGE;
                                for alert in page {
                                    cursor = cursor.max(alert.id);
                                    if send(&tx, &cancel, Ok(to_proto(&alert))).await.is_break() {
                                        return;
                                    }
                                }
                                if drained {
                                    break;
                                }
                            }
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            // The engine was dropped; nothing more will ever
                            // arrive. Ending silently here is correct — this
                            // is not a cancellation, it is the end of the
                            // stream's source.
                            return;
                        }
                    }
                }
            }
            .instrument(tracing::Span::current()),
        );

        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }
}

/// The account a message belongs to.
struct MessageScope {
    account_id: i64,
    account: String,
}

async fn message_scope(db: &Database, message_id: i64) -> Result<Option<MessageScope>, Status> {
    let row = db
        .read(move |conn| {
            use rusqlite::OptionalExtension;
            conn.query_row(
                "SELECT m.account_id, a.name
                 FROM messages m JOIN accounts a ON a.id = m.account_id
                 WHERE m.id = ?1",
                [message_id],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()
        })
        .await
        .map_err(|e| Status::from(Error::from(e)))?;
    Ok(row.map(|(account_id, account)| MessageScope {
        account_id,
        account,
    }))
}

async fn send(
    tx: &tokio::sync::mpsc::Sender<Result<ProtoAlert, Status>>,
    cancel: &CancellationToken,
    item: Result<ProtoAlert, Status>,
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

fn to_proto(alert: &notify::Alert) -> ProtoAlert {
    ProtoAlert {
        id: alert.id,
        message_id: alert.message_id,
        account: alert.account.clone(),
        tier: to_proto_tier(alert.tier) as i32,
        reason: alert.reason.clone(),
        subject: alert.subject.clone(),
        from: alert.from.clone(),
        delivered_at: alert.delivered_at,
    }
}

fn to_proto_tier(tier: Tier) -> NotificationTier {
    match tier {
        Tier::Low => NotificationTier::Low,
        Tier::Normal => NotificationTier::Normal,
        Tier::High => NotificationTier::High,
        Tier::Critical => NotificationTier::Critical,
    }
}

fn to_proto_state(state: &str) -> NotificationState {
    match state {
        notify::repo::STATE_PENDING => NotificationState::Pending,
        notify::repo::STATE_DELIVERED => NotificationState::Delivered,
        notify::repo::STATE_SUPPRESSED => NotificationState::Suppressed,
        notify::repo::STATE_FAILED => NotificationState::Failed,
        // A state this build does not know is a row from a newer daemon.
        // Reported as unspecified rather than guessed at.
        _ => NotificationState::Unspecified,
    }
}
