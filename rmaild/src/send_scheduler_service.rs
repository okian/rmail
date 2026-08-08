//! The `SendScheduler` gRPC implementation.
//!
//! Mostly proto⇄domain translation over [`rmail_core::outbox`], which owns the
//! outbox state machine, the at-most-once fence, and the scheduler loop. Three
//! things here are decisions rather than mapping:
//!
//! # Resolving *when*
//!
//! A request can name an instant three ways — `send_at` (absolute),
//! `send_at_nl` (an expression), or `optimal` (ask the suggester) — and the
//! precedence is exactly that order, most explicit first. Whichever wins,
//! [`rmail_core::outbox::SendPolicy::resolve`] gets the last word, because
//! that is where the rule an AI send cannot talk its way out of an
//! interception window lives. Putting the floor there rather than here is
//! deliberate: this file is one of several possible front doors (CLI, MCP, a
//! future TUI), and a safety property enforced at a front door is enforced
//! only at that door.
//!
//! # Resolving *what*
//!
//! Two paths, one renderer. With `draft_id` the message is
//! [`rmail_core::compose::DraftStore::render`]'s output, attachments and all.
//! Without one, the inline fields become an in-memory draft and go through the
//! identical `compose::mime::build`. There is no second serializer, and the
//! outbox stores what came out either way.
//!
//! # Resolving *who from*
//!
//! prd.md's `ScheduleSendRequest` has no `from` field: the account is the
//! identity. A draft carries its own (frozen at compose time — see
//! `compose`'s docs on why); an inline send takes the account's `username`,
//! and fails with `FAILED_PRECONDITION` if that is not an address. Guessing a
//! sender is not a recoverable mistake once the message is delivered.
#![allow(clippy::result_large_err)]

use std::pin::Pin;

use rmail_core::compose::{Draft, Mailbox};
use rmail_core::config::SendConfig;
use rmail_core::outbox::followup::{
    Followup as CoreFollowup, FollowupState as CoreFollowupState, FollowupStore, NewFollowup,
};
use rmail_core::outbox::schedule::{parse_timezone, suggest_send_time};
use rmail_core::outbox::{
    inline_draft, resolve_send_at, InlineMessage, NewSend, Origin, OutboxEntry as CoreEntry,
    OutboxState as CoreState, OutboxStore, SendPolicy,
};
use rmail_core::{Database, Error};
use rmail_proto::v1::send_scheduler_service_server::SendSchedulerService;
use rmail_proto::v1::{
    CancelRequest, CreateFollowupRequest, Followup as ProtoFollowup,
    FollowupState as ProtoFollowupState, IdRequest, ListFollowupsRequest, ListFollowupsResponse,
    ListOutboxRequest, ListOutboxResponse, OutboxEntry as ProtoEntry, OutboxEvent,
    OutboxState as ProtoState, RescheduleRequest, ScheduleSendRequest, SendOrigin,
    SuggestSendTimeRequest, SuggestSendTimeResponse, UpdateBodyRequest, WatchOutboxRequest,
};
use tokio::sync::broadcast;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};
use tracing::Instrument;

/// How many live outbox changes may sit between the store's fan-out and a
/// client before `WatchOutbox` applies backpressure. Matches
/// `note_service::STREAM_BUFFER`; the two streams are independent.
const STREAM_BUFFER: usize = 64;

/// The `SendScheduler` handler.
#[derive(Clone)]
pub struct SendSchedulerApi {
    store: OutboxStore,
    followups: FollowupStore,
    db: Database,
    config: SendConfig,
    policy: SendPolicy,
    /// Cancelled when the daemon shuts down, so an open `WatchOutbox` stream
    /// stops with it rather than holding shutdown open.
    shutdown: CancellationToken,
}

impl SendSchedulerApi {
    /// Build a handler over an outbox.
    #[must_use]
    pub fn new(
        store: OutboxStore,
        followups: FollowupStore,
        db: Database,
        config: SendConfig,
        shutdown: CancellationToken,
    ) -> Self {
        let policy = SendPolicy::from_config(&config);
        Self {
            store,
            followups,
            db,
            config,
            policy,
            shutdown,
        }
    }

    /// The zone a request's bare wall-clock expressions are read in.
    fn zone(&self, requested: &str) -> Result<chrono_tz::Tz, Status> {
        let name = if requested.trim().is_empty() {
            self.config.default_timezone.as_str()
        } else {
            requested
        };
        Ok(parse_timezone(name)?)
    }

    /// Resolve the three ways a request can name an instant, most explicit
    /// first. `None` means "now", which the policy turns into an undo window.
    fn resolve_when(
        &self,
        tz: chrono_tz::Tz,
        send_at: Option<i64>,
        send_at_nl: Option<&str>,
        optimal: bool,
    ) -> Result<Option<i64>, Status> {
        let now = chrono::Utc::now();
        if let Some(at) = send_at {
            return Ok(Some(at));
        }
        if let Some(expression) = send_at_nl.map(str::trim).filter(|e| !e.is_empty()) {
            return Ok(Some(resolve_send_at(expression, tz, now)?.at));
        }
        if optimal {
            if !self.config.optimal.enabled {
                return Err(Status::from(Error::failed_precondition(
                    "send.optimal.enabled is false; name a time instead",
                )));
            }
            return Ok(Some(suggest_send_time(&self.config.optimal, tz, now)?.at));
        }
        Ok(None)
    }

    /// The message to send: a stored draft, or the request's inline fields.
    async fn render(&self, req: &ScheduleSendRequest) -> Result<Rendered, Status> {
        if let Some(draft_id) = req.draft_id {
            let draft = self.store.drafts().get(draft_id).await?;
            if draft.account_id != req.account_id {
                // Scoped for the reason `compose::resolve_threading` scopes
                // its own lookup: sending one account's draft as another
                // would put the wrong identity on the wire and cross-link two
                // mailboxes that were meant to stay separate.
                return Err(Status::from(Error::not_found(format!(
                    "draft {draft_id} not found in account {}",
                    req.account_id
                ))));
            }
            let rendered = self.store.drafts().render(draft_id).await?;
            return Ok(Rendered {
                raw_mime: rendered.mime,
                draft_id: Some(draft_id),
                from_addr: draft.from.address().to_owned(),
                to: addresses(&draft.to),
                cc: addresses(&draft.cc),
                bcc: addresses(&draft.bcc),
                subject: draft.subject.clone(),
                body_preview: draft.body_text.clone(),
                in_reply_to: draft.in_reply_to.clone(),
            });
        }

        let from = self.account_identity(req.account_id).await?;
        let to = mailboxes(&req.to)?;
        let cc = mailboxes(&req.cc)?;
        let bcc = mailboxes(&req.bcc)?;
        let subject = req.subject.clone().unwrap_or_default();
        let body = req.body.clone().unwrap_or_default();
        let in_reply_to = req
            .in_reply_to
            .clone()
            .map(|id| {
                id.trim()
                    .trim_start_matches('<')
                    .trim_end_matches('>')
                    .to_owned()
            })
            .filter(|id| !id.is_empty());

        let draft = inline_draft(InlineMessage {
            account_id: req.account_id,
            from: from.clone(),
            to: to.clone(),
            cc: cc.clone(),
            bcc: bcc.clone(),
            subject: subject.clone(),
            body_text: body.clone(),
            in_reply_to: in_reply_to.clone(),
            // A reply composed inline names its parent but has no local chain
            // to inherit; a caller that wants a full `References` composes
            // through `ComposeService`, which resolves and freezes one.
            references: in_reply_to.clone().into_iter().collect(),
        })?;
        let raw_mime = render_inline(draft)?;

        Ok(Rendered {
            raw_mime,
            draft_id: None,
            from_addr: from.address().to_owned(),
            to: addresses(&to),
            cc: addresses(&cc),
            bcc: addresses(&bcc),
            subject,
            body_preview: body,
            in_reply_to,
        })
    }

    /// The account's own sending identity.
    async fn account_identity(&self, account_id: i64) -> Result<Mailbox, Status> {
        let account = rmail_core::account::get(&self.db, account_id).await?;
        let username = account.username.as_deref().unwrap_or_default();
        Mailbox::new(username, None).map_err(|_| {
            Status::from(Error::failed_precondition(format!(
                "account {account_id} has no usable sending address (its username is \
                 {username:?}); set one, or schedule a draft that names its own From"
            )))
        })
    }
}

/// Everything the outbox needs about a message, whichever path produced it.
struct Rendered {
    raw_mime: Vec<u8>,
    draft_id: Option<i64>,
    from_addr: String,
    to: Vec<String>,
    cc: Vec<String>,
    bcc: Vec<String>,
    subject: String,
    body_preview: String,
    in_reply_to: Option<String>,
}

#[tonic::async_trait]
impl SendSchedulerService for SendSchedulerApi {
    #[tracing::instrument(skip(self, request), fields(account_id, outbox_id, origin))]
    async fn schedule_send(
        &self,
        request: Request<ScheduleSendRequest>,
    ) -> Result<Response<ProtoEntry>, Status> {
        let req = request.into_inner();
        tracing::Span::current().record(rmail_core::telemetry::FIELD_ACCOUNT, req.account_id);
        let origin = origin_from_proto(req.origin);
        tracing::Span::current().record("origin", origin.as_str());

        let tz = self.zone(&req.tz)?;
        let requested_at = self.resolve_when(
            tz,
            req.send_at,
            req.send_at_nl.as_deref(),
            req.optimal.unwrap_or(false),
        )?;
        let requested_undo = req
            .undo_window_secs
            .map(|secs| std::time::Duration::from_secs(u64::try_from(secs).unwrap_or(0)));
        let schedule = self.policy.resolve(
            origin,
            requested_at,
            requested_undo,
            chrono::Utc::now().timestamp(),
        );

        let rendered = self.render(&req).await?;
        let entry = self
            .store
            .schedule(NewSend {
                account_id: req.account_id,
                draft_id: rendered.draft_id,
                from_addr: rendered.from_addr,
                to: rendered.to,
                cc: rendered.cc,
                bcc: rendered.bcc,
                subject: rendered.subject,
                raw_mime: rendered.raw_mime,
                body_preview: rendered.body_preview,
                in_reply_to: rendered.in_reply_to,
                thread_id: None,
                send_at: schedule.send_at,
                tz: tz.name().to_owned(),
                origin,
                undo_deadline: schedule.undo_deadline,
                max_retries: self.policy.max_retries(),
            })
            .await?;
        tracing::Span::current().record("outbox_id", entry.id);
        Ok(Response::new(entry_to_proto(&entry)))
    }

    #[tracing::instrument(skip(self, request))]
    async fn cancel_scheduled(
        &self,
        request: Request<CancelRequest>,
    ) -> Result<Response<ProtoEntry>, Status> {
        let req = request.into_inner();
        // No id is a bare `mail undo`: whatever is most cancelable right now.
        let id = match req.id {
            Some(id) => id,
            None => self.store.newest_cancelable(req.account_id).await?.id,
        };
        Ok(Response::new(entry_to_proto(&self.store.cancel(id).await?)))
    }

    #[tracing::instrument(skip(self, request))]
    async fn reschedule_send(
        &self,
        request: Request<RescheduleRequest>,
    ) -> Result<Response<ProtoEntry>, Status> {
        let req = request.into_inner();
        let tz = self.zone(&req.tz)?;
        let send_at = self
            .resolve_when(tz, req.send_at, req.send_at_nl.as_deref(), false)?
            .ok_or_else(|| {
                Status::from(Error::invalid_argument(
                    "rescheduling needs a time: set send_at or send_at_nl",
                ))
            })?;
        let entry = self.store.reschedule(req.id, send_at, tz.name()).await?;
        Ok(Response::new(entry_to_proto(&entry)))
    }

    #[tracing::instrument(skip(self, request))]
    async fn update_scheduled_body(
        &self,
        request: Request<UpdateBodyRequest>,
    ) -> Result<Response<ProtoEntry>, Status> {
        let req = request.into_inner();
        let entry = self.store.update_body(req.id, req.body).await?;
        Ok(Response::new(entry_to_proto(&entry)))
    }

    #[tracing::instrument(skip(self, request))]
    async fn send_now(&self, request: Request<IdRequest>) -> Result<Response<ProtoEntry>, Status> {
        let entry = self.store.send_now(request.into_inner().id).await?;
        Ok(Response::new(entry_to_proto(&entry)))
    }

    #[tracing::instrument(skip(self, request))]
    async fn retry_failed(
        &self,
        request: Request<IdRequest>,
    ) -> Result<Response<ProtoEntry>, Status> {
        let entry = self.store.retry(request.into_inner().id).await?;
        Ok(Response::new(entry_to_proto(&entry)))
    }

    #[tracing::instrument(skip(self, request))]
    async fn list_outbox(
        &self,
        request: Request<ListOutboxRequest>,
    ) -> Result<Response<ListOutboxResponse>, Status> {
        let req = request.into_inner();
        // A negative page size is nonsense rather than a request for zero, so
        // it is rejected instead of silently becoming the default.
        let page_size = usize::try_from(req.page_size)
            .map_err(|_| Status::from(Error::invalid_argument("page_size must not be negative")))?;
        let entries = self
            .store
            .list(req.account_id, state_from_proto(req.state), page_size)
            .await?;
        Ok(Response::new(ListOutboxResponse {
            entries: entries.iter().map(entry_to_proto).collect(),
        }))
    }

    type WatchOutboxStream =
        Pin<Box<dyn tokio_stream::Stream<Item = Result<OutboxEvent, Status>> + Send + 'static>>;

    #[tracing::instrument(skip(self, request))]
    async fn watch_outbox(
        &self,
        request: Request<WatchOutboxRequest>,
    ) -> Result<Response<Self::WatchOutboxStream>, Status> {
        let cancel = self.shutdown.child_token();
        let account_id = request.into_inner().account_id;
        let mut changes = self.store.watch();
        let (tx, rx) = tokio::sync::mpsc::channel(STREAM_BUFFER);

        tokio::spawn(
            async move {
                loop {
                    let received = tokio::select! {
                        () = cancel.cancelled() => return,
                        received = changes.recv() => received,
                    };
                    match received {
                        Ok(change) => {
                            if account_id.is_some_and(|id| change.entry.account_id != id) {
                                continue;
                            }
                            let event = OutboxEvent {
                                entry: Some(entry_to_proto(&change.entry)),
                            };
                            if send(&tx, &cancel, Ok(event)).await.is_break() {
                                return;
                            }
                        }
                        // No durable backlog to recover from — the outbox
                        // table is the record, and a lagging client re-reads
                        // it with ListOutbox. Resuming the live tail is the
                        // same thing a reconnect does.
                        Err(broadcast::error::RecvError::Lagged(missed)) => {
                            tracing::debug!(
                                missed,
                                "outbox change stream lagged; resuming the live tail"
                            );
                        }
                        Err(broadcast::error::RecvError::Closed) => return,
                    }
                }
            }
            .instrument(tracing::Span::current()),
        );

        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    #[tracing::instrument(skip(self, request))]
    async fn suggest_send_time(
        &self,
        request: Request<SuggestSendTimeRequest>,
    ) -> Result<Response<SuggestSendTimeResponse>, Status> {
        let req = request.into_inner();
        tracing::Span::current().record(rmail_core::telemetry::FIELD_ACCOUNT, req.account_id);
        if !self.config.optimal.enabled {
            return Err(Status::from(Error::failed_precondition(
                "send.optimal.enabled is false",
            )));
        }
        let tz = self.zone(&req.tz)?;
        let not_before = match req.not_before {
            Some(at) => chrono::DateTime::from_timestamp(at, 0).ok_or_else(|| {
                Status::from(Error::invalid_argument("not_before is not a valid instant"))
            })?,
            None => chrono::Utc::now(),
        };
        let resolved = suggest_send_time(&self.config.optimal, tz, not_before)?;
        let rationale = format!(
            "the next moment inside the configured {}–{} window in {}",
            self.config.optimal.earliest, self.config.optimal.latest, resolved.tz
        );
        Ok(Response::new(SuggestSendTimeResponse {
            send_at: resolved.at,
            tz: resolved.tz,
            display: resolved.display,
            rationale,
        }))
    }

    #[tracing::instrument(skip(self, request), fields(account_id, followup_id))]
    async fn create_followup(
        &self,
        request: Request<CreateFollowupRequest>,
    ) -> Result<Response<ProtoFollowup>, Status> {
        let req = request.into_inner();
        tracing::Span::current().record(rmail_core::telemetry::FIELD_ACCOUNT, req.account_id);
        let tz = self.zone(&req.tz)?;
        let now = chrono::Utc::now();

        let remind_at = match (req.remind_at, req.remind_in.as_deref()) {
            (Some(at), _) => at,
            (None, Some(expression)) if !expression.trim().is_empty() => {
                // "3d" is a duration, not a time expression, so it is read as
                // one first — prd.md's own CLI spells the flag `--in "3d"`.
                match rmail_core::config::parse_human_duration(expression.trim()) {
                    Ok(delay) => now
                        .timestamp()
                        .saturating_add(i64::try_from(delay.as_secs()).unwrap_or(i64::MAX)),
                    Err(_) => resolve_send_at(expression, tz, now)?.at,
                }
            }
            _ => now.timestamp().saturating_add(
                i64::try_from(self.config.followup.default_delay.as_duration().as_secs())
                    .unwrap_or(i64::MAX),
            ),
        };

        let followup = self
            .followups
            .create(NewFollowup {
                account_id: req.account_id,
                thread_id: req.thread_id,
                message_id: req.message_id,
                remind_at,
                tz: tz.name().to_owned(),
                cancel_on_reply: req
                    .cancel_on_reply
                    .unwrap_or(self.config.followup.cancel_on_reply),
                note: req.note.filter(|n| !n.trim().is_empty()),
            })
            .await?;
        tracing::Span::current().record("followup_id", followup.id);
        Ok(Response::new(followup_to_proto(&followup)))
    }

    #[tracing::instrument(skip(self, request))]
    async fn list_followups(
        &self,
        request: Request<ListFollowupsRequest>,
    ) -> Result<Response<ListFollowupsResponse>, Status> {
        let req = request.into_inner();
        let page_size = usize::try_from(req.page_size)
            .map_err(|_| Status::from(Error::invalid_argument("page_size must not be negative")))?;
        let followups = self
            .followups
            .list(
                req.account_id,
                followup_state_from_proto(req.state),
                page_size,
            )
            .await?;
        Ok(Response::new(ListFollowupsResponse {
            followups: followups.iter().map(followup_to_proto).collect(),
        }))
    }

    #[tracing::instrument(skip(self, request))]
    async fn dismiss_followup(
        &self,
        request: Request<IdRequest>,
    ) -> Result<Response<ProtoFollowup>, Status> {
        let followup = self.followups.dismiss(request.into_inner().id).await?;
        Ok(Response::new(followup_to_proto(&followup)))
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Render an in-memory draft on a blocking thread.
///
/// Off the runtime for the reason [`rmail_core::compose::DraftStore::render`]
/// gives: rendering is a base64 pass, a SHA-256, and several full copies of
/// the result, with no await point in it.
fn render_inline(draft: Draft) -> Result<Vec<u8>, Status> {
    let envelope = rmail_core::compose::mime::Envelope::now(&draft);
    rmail_core::compose::mime::build(&draft, &envelope)
        .inspect_err(|error| {
            if matches!(error.reason(), rmail_core::ErrorReason::Internal) {
                // Logged here because the boundary cannot: an `Internal`'s
                // detail is replaced with a generic message on its way to a
                // `Status`, and for a line-length violation that detail is
                // the only evidence an operator would ever get.
                tracing::error!(%error, "rendering an inline send violated an RFC invariant");
            }
        })
        .map_err(Status::from)
}

// ---------------------------------------------------------------------------
// Proto ⇄ domain
// ---------------------------------------------------------------------------

fn mailboxes(addresses: &[String]) -> Result<Vec<Mailbox>, Status> {
    addresses
        .iter()
        .map(|addr| Mailbox::parse(addr).map_err(Status::from))
        .collect()
}

fn addresses(mailboxes: &[Mailbox]) -> Vec<String> {
    mailboxes.iter().map(|m| m.address().to_owned()).collect()
}

/// An unspecified origin is a user send.
///
/// Not an error: `origin` is a proto3 enum, so "unset" and "USER" are the same
/// bytes on the wire, and every caller that is *not* the MCP bridge simply
/// leaves it alone. The direction of the default is what matters — falling
/// back to `Ai` would put a mandatory undo window on every send, and falling
/// back to anything else would be inventing a provenance.
fn origin_from_proto(origin: i32) -> Origin {
    match SendOrigin::try_from(origin) {
        Ok(SendOrigin::Ai) => Origin::Ai,
        Ok(SendOrigin::Followup) => Origin::Followup,
        Ok(SendOrigin::Undo) => Origin::Undo,
        Ok(SendOrigin::User | SendOrigin::Unspecified) | Err(_) => Origin::User,
    }
}

fn origin_to_proto(origin: Origin) -> SendOrigin {
    match origin {
        Origin::User => SendOrigin::User,
        Origin::Ai => SendOrigin::Ai,
        Origin::Followup => SendOrigin::Followup,
        Origin::Undo => SendOrigin::Undo,
    }
}

/// `UNSPECIFIED` is "every state", not a state.
fn state_from_proto(state: i32) -> Option<CoreState> {
    match ProtoState::try_from(state) {
        Ok(ProtoState::Scheduled) => Some(CoreState::Scheduled),
        Ok(ProtoState::Sending) => Some(CoreState::Sending),
        Ok(ProtoState::Sent) => Some(CoreState::Sent),
        Ok(ProtoState::Failed) => Some(CoreState::Failed),
        Ok(ProtoState::Canceled) => Some(CoreState::Canceled),
        Ok(ProtoState::Unspecified) | Err(_) => None,
    }
}

fn state_to_proto(state: CoreState) -> ProtoState {
    match state {
        CoreState::Scheduled => ProtoState::Scheduled,
        CoreState::Sending => ProtoState::Sending,
        CoreState::Sent => ProtoState::Sent,
        CoreState::Failed => ProtoState::Failed,
        CoreState::Canceled => ProtoState::Canceled,
    }
}

fn followup_state_from_proto(state: i32) -> Option<CoreFollowupState> {
    match ProtoFollowupState::try_from(state) {
        Ok(ProtoFollowupState::Armed) => Some(CoreFollowupState::Armed),
        Ok(ProtoFollowupState::Fired) => Some(CoreFollowupState::Fired),
        Ok(ProtoFollowupState::Dismissed) => Some(CoreFollowupState::Dismissed),
        Ok(ProtoFollowupState::Unspecified) | Err(_) => None,
    }
}

fn followup_state_to_proto(state: CoreFollowupState) -> ProtoFollowupState {
    match state {
        CoreFollowupState::Armed => ProtoFollowupState::Armed,
        CoreFollowupState::Fired => ProtoFollowupState::Fired,
        CoreFollowupState::Dismissed => ProtoFollowupState::Dismissed,
    }
}

fn entry_to_proto(entry: &CoreEntry) -> ProtoEntry {
    ProtoEntry {
        id: entry.id,
        account_id: entry.account_id,
        draft_id: entry.draft_id,
        from_addr: entry.from_addr.clone(),
        to: entry.to.clone(),
        cc: entry.cc.clone(),
        bcc: entry.bcc.clone(),
        subject: entry.subject.clone(),
        body_preview: entry.body_preview.clone(),
        in_reply_to: entry.in_reply_to.clone(),
        thread_id: entry.thread_id,
        send_at: entry.send_at,
        tz: entry.tz.clone(),
        state: state_to_proto(entry.state) as i32,
        origin: origin_to_proto(entry.origin) as i32,
        attempts: entry.attempts,
        max_retries: entry.max_retries,
        next_attempt_at: entry.next_attempt_at,
        last_error: entry.last_error.clone(),
        smtp_message_id: entry.smtp_message_id.clone(),
        sent_at: entry.sent_at,
        sent_late: entry.sent_late,
        undo_deadline: entry.undo_deadline,
        created_at: entry.created_at,
        updated_at: entry.updated_at,
    }
}

fn followup_to_proto(followup: &CoreFollowup) -> ProtoFollowup {
    ProtoFollowup {
        id: followup.id,
        account_id: followup.account_id,
        thread_id: followup.thread_id,
        message_id: followup.message_id.clone(),
        remind_at: followup.remind_at,
        tz: followup.tz.clone(),
        cancel_on_reply: followup.cancel_on_reply,
        state: followup_state_to_proto(followup.state) as i32,
        note: followup.note.clone(),
        created_at: followup.created_at,
    }
}

/// Send one stream item, giving up if the client went away or the daemon is
/// shutting down. Mirrors `note_service::send`.
async fn send(
    tx: &tokio::sync::mpsc::Sender<Result<OutboxEvent, Status>>,
    cancel: &CancellationToken,
    item: Result<OutboxEvent, Status>,
) -> std::ops::ControlFlow<()> {
    tokio::select! {
        () = cancel.cancelled() => std::ops::ControlFlow::Break(()),
        sent = tx.send(item) => {
            if sent.is_ok() {
                std::ops::ControlFlow::Continue(())
            } else {
                std::ops::ControlFlow::Break(())
            }
        }
    }
}
