//! The `AiService` gRPC implementation: cached reads over `ai_summaries`
//! (`GetSummary`, `StreamEnrichments`) plus two on-demand, model-calling RPCs
//! (`AnalyzeMessage`, `SuggestReply`) that run *outside* `ai_queue` entirely,
//! and the daemon-wide usage/pause surface (`GetUsage`, `SetPaused`).
//!
//! # Why `AnalyzeMessage`/`SuggestReply` bypass the queue
//!
//! Every other AI artifact in this codebase is produced by
//! [`rmail_core::ai::queue::AiWorkerPool`]/[`rmail_core::ai::queue::BatchCoordinator`]
//! leasing a durable [`rmail_core::ai::queue::AiQueue`] row. These two RPCs
//! do not: a client calling `AnalyzeMessage` wants *this* message analyzed
//! *now*, streamed back over *this* connection — folding that into the
//! queue's lease/dispatch cycle would mean either blocking the RPC on an
//! unrelated worker's schedule or inventing a second, RPC-specific leasing
//! path inside the queue for exactly one caller. Instead both call
//! [`rmail_core::ai::deep::DeepPassHandler::build_request`]/`on_success`
//! directly — the identical prompt construction and persistence the queued
//! deep pass uses — wrapped in the same policy → assemble → build → redact
//! → provider → audit sequence [`rmail_core::ai::queue`]'s own module docs
//! describe, just driven by this file instead of `AiWorkerPool::process_one`.
//! A synthetic [`AiLease`] (job_id `0`, never a real queue row) is what lets
//! [`DeepPassHandler::on_success`] be reused unmodified — it only ever reads
//! `lease.message_id`/`lease.account_id`, never the lease-fencing fields.
//!
//! **Known, narrow race**: because this path bypasses `ai_queue`, a message
//! with an already-`pending` (queue-driven) deep job *and* a concurrent
//! `AnalyzeMessage`/`SuggestReply` call on the same message can have two
//! deep passes running for it at once — the same class of hazard
//! `ai::queue::AiQueue::lease_with_ttl`'s per-thread cap closes for two
//! *queued* deep jobs, but that cap has no visibility into a call that never
//! took a lease. Both writes still land safely (the last one to finish wins
//! the `(message_id, pass, model)` upsert — never a partial row), so this is
//! staleness, not corruption, and is narrow enough (it needs a queued deep
//! job and a manual `mail ai process`/`mail ai reply` racing it within the
//! same few seconds) that closing it is left as follow-on work rather than
//! blocking this task on integrating two independently-designed dispatch
//! paths.
//!
//! # Token-streaming and cancellation
//!
//! `AnalyzeMessage` relays [`rmail_core::ai::provider::StreamFrame`]s as
//! typed [`AnalyzeEvent`](rmail_proto::v1::AnalyzeEvent) frames — the PRD's
//! "Token-streaming AI RPCs." The `CancellationToken` handed to
//! [`Provider::stream`] is a child of the daemon's shutdown token; when the
//! client disconnects, `tx.send` in the producer task starts failing, which
//! this file's own `send` helper turns into cancelling that token, which
//! [`rmail_core::ai::provider::spawn_sse_reader`]'s own cancellation race
//! (see that function's docs) drops the upstream HTTP response — the request
//! to Claude is aborted, not merely the local relay.
//!
//! # `AskMailbox` is a thin adapter over `rmail_core::ai::rag`
//!
//! Mailbox RAG (task 52) has none of its logic here. [`rmail_core::ai::rag`]
//! owns retrieval, the AI policy gate, context packing, the model call,
//! citation resolution and the grounding verdict; this file converts its
//! [`AskEvent`](rmail_core::ai::AskEvent)s to wire
//! [`AskChunk`](rmail_proto::v1::AskChunk)s and nothing else. That split is
//! deliberate: every property task 52 has to guarantee — no `forbidden`/
//! `local_only` text reaching a provider, no citation naming a message that
//! was not retrieved — is provable without a gRPC server, and a transport
//! layer that could weaken one of them by accident is a transport layer that
//! would have to be re-audited every time it changed.
//!
//! The engine is built via [`AiApi::with_ask`] from this handler's *own*
//! provider, policy engine, privacy settings, limits, semaphore and rate
//! limiter, so `ask` draws on exactly the one `ai.limits` budget every other
//! AI path in this process does.
#![allow(clippy::result_large_err)] // see mail_service.rs's identical note on `Result<_, Status>`

use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::StreamExt;
use rmail_core::ai::provider::StreamFrame;
use rmail_core::ai::queue::{assemble_content, payload_bytes, AiLease, PassHandler};
use rmail_core::ai::rag::{AskEvent, AskOutcome, RagEngine};
use rmail_core::ai::{
    self, deep, triage, AiPauseFlag, AiQueue, AskRequest as CoreAskRequest, AskRetriever,
    BudgetEnforcer, BudgetRequest, BudgetVerdict, CallOutcome, CallRecord, CapDecision, CostGate,
    DeepPassHandler, GuardedRequest, PolicyEngine, PolicyTarget, Provider, RateLimiter, TokenMap,
    WorkClass,
};
use rmail_core::config::{AiAsk, AiLimits, AiPrivacy};
use rmail_core::events::{Event as CoreEvent, EventKind, EventLog, NewEvent};
use rmail_core::{Database, Error};
use rmail_proto::v1::ai_service_server::AiService;
use rmail_proto::v1::{
    analyze_event, ask_chunk, AnalyzeEvent, AnalyzeMessageRequest, AskChunk,
    AskDone as ProtoAskDone, AskRequest, Citation as ProtoCitation, DayUsage as ProtoDayUsage,
    Done as ProtoDone, Enrichment, Entity as ProtoEntity, GetSummaryRequest, GetUsageRequest,
    QueueStats as ProtoQueueStats, RetrievalTrace as ProtoRetrievalTrace, RetryFailedRequest,
    RetryFailedResponse, SetPausedRequest, SetPausedResponse, StreamEnrichmentsRequest,
    SuggestReplyRequest, Summary as ProtoSummary, SummaryStatus, Todo as ProtoTodo,
    ToolUseStart as ProtoToolUseStart, Usage as ProtoUsage, UsageStats,
};
use tokio::sync::{broadcast, mpsc, Semaphore};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};
use tracing::Instrument;

/// Backpressure between a stream's producer task and its consumer — see
/// `sync_service::STREAM_BUFFER`'s identical reasoning.
const STREAM_BUFFER: usize = 64;

/// How many `ai_summaries` rows [`StreamEnrichments`] reads per backlog
/// round trip.
const BACKLOG_PAGE: i64 = 200;

/// The `AiService` handler.
///
/// Cheap to clone: every field is already `Clone` (a `Database`/`AiQueue`/
/// `EventLog` handle, an `Arc`, or a small value) — the same "clone into the
/// spawned producer task" shape `SearchApi`/`MailApi` use for their own
/// streaming RPCs.
#[derive(Clone)]
pub struct AiApi {
    db: Database,
    queue: AiQueue,
    events: EventLog,
    deep: Arc<DeepPassHandler>,
    provider: Arc<dyn Provider>,
    policy: Arc<PolicyEngine>,
    privacy: AiPrivacy,
    limits: AiLimits,
    pause: AiPauseFlag,
    /// Whether the AI subsystem is actually active on this daemon
    /// (`ai.enabled = true` and a provider was built successfully) — kept
    /// separate from `pause` so a disabled daemon reports itself as such via
    /// `GetUsage.enabled` rather than as merely "paused," which would
    /// misleadingly suggest `mail ai resume` could make it start running.
    enabled: bool,
    /// The daemon's `AiWorkerPool`'s own `Semaphore(max_concurrency)`,
    /// shared rather than a second independent one — see
    /// [`AiWorkerPool::semaphore`]'s own docs for why a forced
    /// `AnalyzeMessage`/`SuggestReply` call must be bounded by the *same*
    /// concurrency budget the queue's own dispatch already is, not a
    /// parallel budget that could double `ai.limits.max_concurrency` in
    /// practice.
    semaphore: Arc<Semaphore>,
    /// The daemon's `AiWorkerPool`'s own RPM limiter, shared for the
    /// identical reason `semaphore` is.
    rate_limiter: Arc<RateLimiter>,
    /// Cancelled when the daemon shuts down, so in-flight forced analyses and
    /// open streams stop with it rather than holding shutdown open.
    shutdown: CancellationToken,
    /// Mailbox RAG (task 52). `None` on a daemon built without a retriever,
    /// in which case `AskMailbox` is registered — the reflection set and the
    /// scope table must see every RPC regardless of runtime wiring — but
    /// answers `FAILED_PRECONDITION` rather than pretending to search.
    rag: Option<Arc<RagEngine>>,
}

impl AiApi {
    /// Build a handler. `pause` should be the same [`AiPauseFlag`] handed to
    /// the daemon's [`AiDispatchLoop`] (via
    /// [`AiDispatchLoop::with_pause_flag`]) — `SetPaused` and the dispatch
    /// loop must observe one shared switch, not two independent ones.
    /// `semaphore`/`rate_limiter` should be the daemon's
    /// [`AiWorkerPool`](rmail_core::ai::AiWorkerPool)'s own
    /// ([`AiWorkerPool::semaphore`]/[`AiWorkerPool::rate_limiter`]), for the
    /// same "one shared budget, not two" reason.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: Database,
        queue: AiQueue,
        events: EventLog,
        deep: Arc<DeepPassHandler>,
        provider: Arc<dyn Provider>,
        policy: Arc<PolicyEngine>,
        privacy: AiPrivacy,
        limits: AiLimits,
        pause: AiPauseFlag,
        enabled: bool,
        semaphore: Arc<Semaphore>,
        rate_limiter: Arc<RateLimiter>,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            db,
            queue,
            events,
            deep,
            provider,
            policy,
            privacy,
            limits,
            pause,
            enabled,
            semaphore,
            rate_limiter,
            shutdown,
            rag: None,
        }
    }

    /// Give this handler the mailbox-RAG engine behind `AskMailbox`, over
    /// `retriever`'s search pipeline.
    ///
    /// A builder method rather than another `new` parameter because the
    /// retriever is `rmaild::SearchApi`, which is constructed *after* the AI
    /// provider it shares — and because the engine is assembled here, from
    /// this handler's own fields, so there is no way to hand it a second
    /// provider, a second policy engine, or a second concurrency budget.
    #[must_use]
    pub fn with_ask(mut self, retriever: Arc<dyn AskRetriever>, config: AiAsk) -> Self {
        self.rag = Some(Arc::new(RagEngine::new(
            self.db.clone(),
            Arc::clone(&self.provider),
            Arc::clone(&self.policy),
            retriever,
            self.privacy.clone(),
            self.limits.clone(),
            config,
            Arc::clone(&self.semaphore),
            Arc::clone(&self.rate_limiter),
        )));
        self
    }
}

#[tonic::async_trait]
impl AiService for AiApi {
    async fn get_summary(
        &self,
        request: Request<GetSummaryRequest>,
    ) -> Result<Response<ProtoSummary>, Status> {
        let message_id = request.into_inner().message_id;
        let rows = read_summary(&self.db, message_id)
            .await
            .map_err(Status::from)?;
        if !rows.message_exists {
            return Err(Status::from(Error::not_found(format!(
                "message {message_id} not found"
            ))));
        }
        Ok(Response::new(to_proto_summary(message_id, &rows)))
    }

    type AnalyzeMessageStream =
        Pin<Box<dyn tokio_stream::Stream<Item = Result<AnalyzeEvent, Status>> + Send + 'static>>;

    async fn analyze_message(
        &self,
        request: Request<AnalyzeMessageRequest>,
    ) -> Result<Response<Self::AnalyzeMessageStream>, Status> {
        let message_id = request.into_inner().message_id;
        let prepared = self.prepare_forced_analysis(message_id).await?;

        let cancel = self.shutdown.child_token();
        let this = self.clone();
        let (tx, rx) = mpsc::channel(STREAM_BUFFER);
        tokio::spawn(
            async move {
                this.run_analyze_stream(message_id, prepared, cancel, tx)
                    .await;
            }
            .instrument(tracing::Span::current()),
        );
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    type StreamEnrichmentsStream =
        Pin<Box<dyn tokio_stream::Stream<Item = Result<Enrichment, Status>> + Send + 'static>>;

    async fn stream_enrichments(
        &self,
        request: Request<StreamEnrichmentsRequest>,
    ) -> Result<Response<Self::StreamEnrichmentsStream>, Status> {
        let req = request.into_inner();
        if req.since_message_id < 0 {
            return Err(Status::from(Error::invalid_argument(
                "since_message_id must not be negative",
            )));
        }
        let account_filter = (req.account_id != 0).then_some(req.account_id);
        let cancel = self.shutdown.child_token();

        // Subscribe before reading the backlog — the same discipline
        // `SyncApi::watch_events` uses and for the same reason: the other
        // order leaves a window in which an enrichment written in between is
        // in neither the backlog nor the tail.
        let mut live = self.events.subscribe();
        let db = self.db.clone();

        let (tx, rx) = mpsc::channel(STREAM_BUFFER);
        tokio::spawn(
            async move {
                let mut cursor = req.since_message_id;
                'stream: loop {
                    loop {
                        let (page, next_cursor, more) =
                            match backlog_page(&db, account_filter, cursor, BACKLOG_PAGE).await {
                                Ok(page) => page,
                                Err(e) => {
                                    let _ = send(&tx, &cancel, Err(Status::from(e))).await;
                                    return;
                                }
                            };
                        for (message_id, pass, summary) in page {
                            let item = Enrichment {
                                message_id,
                                pass,
                                summary: Some(summary),
                            };
                            if send(&tx, &cancel, Ok(item)).await.is_break() {
                                return;
                            }
                        }
                        // Advance past what this page *scanned*, not merely
                        // what it returned — see `backlog_page`'s own docs
                        // for why that is what keeps this loop from
                        // re-querying an all-deleted page forever.
                        cursor = next_cursor;
                        if !more {
                            break;
                        }
                    }

                    loop {
                        let received = tokio::select! {
                            () = cancel.cancelled() => return,
                            received = live.recv() => received,
                        };
                        match received {
                            Ok(event) => {
                                let Some(item) =
                                    enrichment_for_event(&db, &event, account_filter).await
                                else {
                                    continue;
                                };
                                let item = match item {
                                    Ok(item) => item,
                                    Err(e) => {
                                        let _ = send(&tx, &cancel, Err(Status::from(e))).await;
                                        return;
                                    }
                                };
                                // Tracked for a later `Lagged` recovery's
                                // backlog re-scan to resume from somewhere
                                // recent — *not* used to filter what gets
                                // delivered here. Deep and triage passes for
                                // different messages complete in whatever
                                // order their provider calls happen to
                                // finish, not in message_id order (the
                                // per-thread "deep" serialization this same
                                // task adds makes that *more* true, not
                                // less — a deep pass can easily finish after
                                // several newer messages' triage passes
                                // already advanced this past their ids), so
                                // gating live delivery on "message_id >=
                                // cursor" would silently and permanently
                                // drop exactly the enrichments most likely
                                // to be for older, already-flagged-important
                                // mail. See `StreamEnrichmentsRequest`'s own
                                // proto docs for the resulting contract: at
                                // least once, never silently dropped.
                                cursor = cursor.max(item.message_id);
                                if send(&tx, &cancel, Ok(item)).await.is_break() {
                                    return;
                                }
                            }
                            // The client has not lost data — `ai_summaries`
                            // is never pruned — only its place in the tail;
                            // re-scanning the backlog from `cursor` recovers
                            // it, the same recovery `SyncApi::watch_events`
                            // performs against the durable event log.
                            Err(broadcast::error::RecvError::Lagged(missed)) => {
                                tracing::debug!(
                                    missed,
                                    cursor,
                                    "enrichment stream lagged; re-reading from ai_summaries"
                                );
                                continue 'stream;
                            }
                            Err(broadcast::error::RecvError::Closed) => return,
                        }
                    }
                }
            }
            .instrument(tracing::Span::current()),
        );

        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    async fn suggest_reply(
        &self,
        request: Request<SuggestReplyRequest>,
    ) -> Result<Response<ProtoSummary>, Status> {
        let message_id = request.into_inner().message_id;
        let cached = read_summary(&self.db, message_id)
            .await
            .map_err(Status::from)?;
        if !cached.message_exists {
            return Err(Status::from(Error::not_found(format!(
                "message {message_id} not found"
            ))));
        }
        if cached.deep.is_some() {
            // Already analyzed — a deep row exists whether or not it settled
            // on an actual suggested_reply text (the model may correctly
            // have decided none was warranted), so this is the complete
            // cached answer either way.
            return Ok(Response::new(to_proto_summary(message_id, &cached)));
        }

        let prepared = self.prepare_forced_analysis(message_id).await?;
        let cancel = self.shutdown.child_token();
        let _permit = self.acquire_capacity(&cancel).await?;
        let start = Instant::now();
        let result = self.provider.complete(&prepared.request, &cancel).await;
        let latency = start.elapsed();
        match result {
            Ok(response) => {
                let summary = self
                    .persist_forced_result(
                        message_id,
                        prepared.account_id,
                        &response.model,
                        &prepared.payload,
                        &prepared.redaction_level,
                        &prepared.tokens,
                        response.usage,
                        latency,
                        &response.text,
                    )
                    .await?;
                Ok(Response::new(summary))
            }
            Err(e) => {
                self.audit_forced_failure(
                    message_id,
                    prepared.account_id,
                    &prepared.request.model,
                    &prepared.payload,
                    &prepared.redaction_level,
                    latency,
                    &e,
                )
                .await;
                Err(Status::from(e))
            }
        }
    }

    async fn get_usage(
        &self,
        _request: Request<GetUsageRequest>,
    ) -> Result<Response<UsageStats>, Status> {
        let today_day = today_key();
        let today = ai::usage_for_day(&self.db, &today_day)
            .await
            .map_err(Status::from)?
            .map(to_proto_day_usage)
            .unwrap_or_else(|| empty_day_usage(&today_day));
        let month_key = today_day[..7.min(today_day.len())].to_owned();
        let month = month_usage(&self.db, &month_key).await?;
        let queue_stats = self.queue.stats().await.map_err(Status::from)?;

        Ok(Response::new(UsageStats {
            today: Some(today),
            month: Some(month),
            queue: Some(ProtoQueueStats {
                ready: queue_stats.ready,
                backing_off: queue_stats.backing_off,
                leased: queue_stats.leased,
                done: queue_stats.done,
                error: queue_stats.error,
                dead: queue_stats.dead,
            }),
            paused: self.pause.get(),
            daily_cost_cap_usd: self.limits.daily_cost_cap_usd,
            monthly_cost_cap_usd: self.limits.monthly_cost_cap_usd,
            daily_token_cap: self.limits.daily_token_cap,
            enabled: self.enabled,
        }))
    }

    async fn set_paused(
        &self,
        request: Request<SetPausedRequest>,
    ) -> Result<Response<SetPausedResponse>, Status> {
        self.pause.set(request.into_inner().paused);
        Ok(Response::new(SetPausedResponse {
            paused: self.pause.get(),
        }))
    }

    async fn retry_failed(
        &self,
        _request: Request<RetryFailedRequest>,
    ) -> Result<Response<RetryFailedResponse>, Status> {
        let revived = self.queue.revive_all_dead().await.map_err(Status::from)?;
        Ok(Response::new(RetryFailedResponse {
            revived: i64::try_from(revived).unwrap_or(i64::MAX),
        }))
    }

    type AskMailboxStream =
        Pin<Box<dyn tokio_stream::Stream<Item = Result<AskChunk, Status>> + Send + 'static>>;

    #[tracing::instrument(skip(self, request), fields(account_id, top_k))]
    async fn ask_mailbox(
        &self,
        request: Request<AskRequest>,
    ) -> Result<Response<Self::AskMailboxStream>, Status> {
        let req = request.into_inner();
        tracing::Span::current()
            .record("account_id", req.account_id)
            .record("top_k", req.top_k);

        // Checked before retrieval rather than left to `NullProvider`: a
        // disabled daemon should decline in microseconds, not after running a
        // hybrid search whose only possible use was a call it was never going
        // to make.
        if !self.enabled {
            return Err(Status::from(Error::failed_precondition(
                "AI is disabled on this daemon (ai.enabled = false, or no provider could be \
                 built), so ask-mailbox cannot answer"
                    .to_owned(),
            )));
        }
        let Some(rag) = self.rag.as_ref() else {
            return Err(Status::from(Error::failed_precondition(
                "ask-mailbox is not wired on this daemon (no search pipeline)".to_owned(),
            )));
        };

        // A child of the shutdown token, exactly as `AnalyzeMessage`'s is, so
        // daemon shutdown ends an open answer — and so dropping the response
        // stream propagates to the provider rather than merely to the relay.
        let cancel = self.shutdown.child_token();
        let stream = rag
            .ask(
                &CoreAskRequest {
                    question: req.question,
                    account_id: req.account_id,
                    filter: req.filter,
                    top_k: req.top_k,
                },
                &cancel,
            )
            .await
            .map_err(Status::from)?;

        // The token is cancelled when the mapped stream is dropped — which is
        // what tonic does the instant a client disconnects. Without this the
        // engine's own `tx.closed()` race would still fire, but the *upstream*
        // HTTP request would only be dropped once the engine noticed; carrying
        // the guard on the stream makes the cancellation unconditional.
        let guard = CancelOnDrop(cancel);
        let stream = stream.map(move |event| {
            let _ = &guard;
            event.map(to_proto_chunk).map_err(Status::from)
        });
        Ok(Response::new(Box::pin(stream)))
    }
}

/// Cancels its token when dropped. Carried by the `AskMailbox` response
/// stream so a client disconnect — which tonic signals by dropping the stream
/// — aborts the upstream provider call rather than only ending the local
/// relay.
struct CancelOnDrop(CancellationToken);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

/// One core [`AskEvent`] as a wire [`AskChunk`].
fn to_proto_chunk(event: AskEvent) -> AskChunk {
    let body = match event {
        AskEvent::Trace(trace) => ask_chunk::Body::Trace(ProtoRetrievalTrace {
            retrieved: clamp_u32(trace.retrieved),
            packed: clamp_u32(trace.packed),
            withheld_by_policy: clamp_u32(trace.withheld_by_policy),
            dropped_for_budget: clamp_u32(trace.dropped_for_budget),
            context_tokens: clamp_u32(trace.context_tokens),
            model: trace.model,
        }),
        AskEvent::Token(token) => ask_chunk::Body::Token(token),
        AskEvent::Citation(citation) => ask_chunk::Body::Citation(ProtoCitation {
            label: citation.label,
            message_id: citation.message_id,
            message_uid: citation.message_uid,
            account_id: citation.account_id,
            mailbox: citation.mailbox,
            subject: citation.subject,
            from_addr: citation.from_addr,
            date: citation.date,
            quote: citation.quote,
        }),
        AskEvent::Usage(usage) => ask_chunk::Body::Usage(to_proto_usage(usage)),
        AskEvent::Done(outcome) => ask_chunk::Body::Done(to_proto_ask_done(outcome)),
    };
    AskChunk { body: Some(body) }
}

fn to_proto_ask_done(outcome: AskOutcome) -> ProtoAskDone {
    ProtoAskDone {
        grounded: outcome.grounded,
        // Empty exactly when grounded, per the proto's own contract — the
        // refusal text is the engine's, so a client never has to compose one.
        refusal: outcome
            .refusal
            .map(|refusal| refusal.message().to_owned())
            .unwrap_or_default(),
        stop_reason: outcome
            .stop_reason
            .map(|reason| stop_reason_str(reason).to_owned())
            .unwrap_or_default(),
    }
}

/// A count as a wire `uint32`. Saturating rather than wrapping: these are
/// display counters bounded by `ai.ask.top_k` in practice, and a wrapped one
/// would be a lie rather than a large number.
fn clamp_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

// ---------------------------------------------------------------------------
// Forced analysis: the pipeline AnalyzeMessage/SuggestReply share
// ---------------------------------------------------------------------------

/// What [`AiApi::prepare_forced_analysis`] resolved, ready to hand to either
/// [`Provider::complete`] (`SuggestReply`) or [`Provider::stream`]
/// (`AnalyzeMessage`).
struct PreparedAnalysis {
    account_id: i64,
    request: ai::ChatRequest,
    tokens: TokenMap,
    payload: Vec<u8>,
    redaction_level: String,
}

impl AiApi {
    /// Policy → assemble → build → redact for a forced deep-pass call —
    /// everything [`rmail_core::ai::queue::AiWorkerPool::process_one`] does
    /// before the provider call, run here instead since this path never
    /// takes a queue lease. Returns a [`Status`] directly (rather than an
    /// [`Error`]) for the same reason `SearchApi::effective_query` does: a
    /// policy-forbidden or spend-capped request should fail the RPC call
    /// itself, before any stream (or, for `SuggestReply`, any provider call)
    /// ever opens.
    async fn prepare_forced_analysis(&self, message_id: i64) -> Result<PreparedAnalysis, Status> {
        let Some((account_name, mailbox_name)) = target_names(&self.db, message_id)
            .await
            .map_err(Status::from)?
        else {
            return Err(Status::from(Error::not_found(format!(
                "message {message_id} not found"
            ))));
        };
        let decision = self
            .policy
            .resolve(&PolicyTarget::account(account_name).mailbox(mailbox_name));
        if !decision.is_visible() || !decision.permits_network() {
            return Err(Status::from(Error::failed_precondition(format!(
                "ai policy resolved {:?} for this account/folder; no network call is permitted",
                decision.mode
            ))));
        }

        let cap = CostGate {
            db: &self.db,
            limits: &self.limits,
        }
        .decide()
        .await
        .map_err(Status::from)?;
        if !matches!(cap, CapDecision::Open) {
            return Err(Status::from(Error::failed_precondition(
                "the AI daily/monthly spend cap has been reached; a forced deep analysis \
                 cannot run until it resets, on_cap changes, or an operator raises the cap"
                    .to_owned(),
            )));
        }

        let content = assemble_content(&self.db, message_id, &self.privacy)
            .await
            .map_err(Status::from)?;
        let account_id = content.account_id;
        let mut request = self
            .deep
            .build_request(&content)
            .await
            .map_err(Status::from)?;

        // The per-call budget enforcer (task 76), at the same point in the
        // pipeline `ai::queue::worker::process_one` consults it: after
        // `build_request` has named a model (so a soft cap has something to
        // downgrade) and before anything that could reach the network. The
        // `CostGate` check above is not a substitute — it can only speak for
        // this daemon's global daily spend, while this account may have a
        // budget of its own, and it has no notion of a model tier at all.
        // Without this, `mail ai process` would be a documented way to spend
        // past a per-account cap the queue enforces.
        //
        // Charged as `WorkClass::Interactive`: a user is waiting on this
        // call, which is also what `record_call` (used by
        // `persist_forced_result`) attributes it as, so the check and the
        // charge agree.
        let verdict = BudgetEnforcer {
            db: &self.db,
            limits: &self.limits,
        }
        .evaluate(&BudgetRequest {
            account_id,
            model: &request.model,
            work_class: WorkClass::Interactive,
            now: chrono::Utc::now().timestamp(),
        })
        .await
        .map_err(Status::from)?;
        match verdict {
            BudgetVerdict::Allow => {}
            BudgetVerdict::Downgrade { model, reason } => {
                tracing::info!(
                    message_id,
                    from = %request.model,
                    to = %model,
                    reason = %reason,
                    "ai budget soft cap: downgrading this forced analysis"
                );
                request.model = model;
            }
            BudgetVerdict::Block { reason, .. } => {
                // The detailed reason names the scope and the figures
                // (`global all budget: daily usd hard cap reached (4500000
                // of 5000000)`) — that is aggregate spend, and this RPC only
                // requires `ai.invoke`, while `GetSpend`/`GetUsage` require
                // `admin` precisely so a token minted to summarize mail
                // cannot read the account's total AI dollar spend. So the
                // detail goes to the log and the client is told only that a
                // cap was reached, matching what the `CostGate` rejection
                // above already discloses.
                tracing::info!(
                    message_id,
                    reason = %reason,
                    "ai budget hard cap: refusing a forced deep analysis"
                );
                // `RESOURCE_EXHAUSTED`, per prd.md's error table, which maps
                // budget exhaustion to exactly that code. The `CostGate`
                // rejection above still returns `FAILED_PRECONDITION`: that
                // is task 50's existing contract for a condition clients may
                // already branch on, and changing it is not this task's to
                // make.
                return Err(Status::from(Error::resource_exhausted(
                    "an AI spend budget has been reached; a forced deep analysis cannot run \
                     until the window resets or an operator raises the budget"
                        .to_owned(),
                )));
            }
        }
        let request = request;

        match rmail_core::ai::guard(&request, &self.privacy) {
            GuardedRequest::RedactedSkip => Err(Status::from(Error::failed_precondition(
                "nothing was left to analyze once PII was redacted from this message".to_owned(),
            ))),
            GuardedRequest::Redacted {
                request, tokens, ..
            } => {
                let payload = payload_bytes(&request);
                let redaction_level = if tokens.is_empty() {
                    "none"
                } else {
                    "redacted"
                }
                .to_owned();
                Ok(PreparedAnalysis {
                    account_id,
                    request,
                    tokens,
                    payload,
                    redaction_level,
                })
            }
        }
    }

    /// Acquire this pool's shared `Semaphore(max_concurrency)` permit and an
    /// RPM token, racing both against `cancel` — the same "pace and bound
    /// concurrency *before* the provider call, never after" discipline
    /// `ai::queue::worker::AiWorkerPool::process_one` applies to its own
    /// queued dispatch. Held by the caller for the duration of the provider
    /// call (a streaming `AnalyzeMessage` holds it for the whole stream, not
    /// just until the first frame), exactly like a queued job's own permit.
    ///
    /// # Errors
    /// [`Status`] (`DEADLINE_EXCEEDED`) if `cancel` fires before capacity is
    /// available.
    async fn acquire_capacity(
        &self,
        cancel: &CancellationToken,
    ) -> Result<tokio::sync::OwnedSemaphorePermit, Status> {
        let permit = tokio::select! {
            () = cancel.cancelled() => {
                return Err(Status::from(Error::deadline_exceeded(
                    "cancelled while waiting for AI concurrency capacity".to_owned(),
                )));
            }
            permit = Arc::clone(&self.semaphore).acquire_owned() => permit,
        };
        let permit = permit.map_err(|_| {
            // The semaphore is never explicitly closed by this pool or the
            // `AiWorkerPool` it is shared with — see
            // `AiWorkerPool::process_one`'s identical note; unreachable in
            // practice, not guessed at.
            Status::from(Error::internal(
                "ai concurrency semaphore closed unexpectedly".to_owned(),
            ))
        })?;
        tokio::select! {
            () = cancel.cancelled() => Err(Status::from(Error::deadline_exceeded(
                "cancelled while waiting for AI rate-limit capacity".to_owned(),
            ))),
            () = self.rate_limiter.acquire() => Ok(permit),
        }
    }

    /// Relay `provider.stream`'s frames as [`AnalyzeEvent`]s, then persist and
    /// audit the completed turn exactly like [`AiApi::suggest_reply`]'s
    /// unary path does, folding the final [`ProtoSummary`] into the terminal
    /// `Done` frame.
    async fn run_analyze_stream(
        &self,
        message_id: i64,
        prepared: PreparedAnalysis,
        cancel: CancellationToken,
        tx: mpsc::Sender<Result<AnalyzeEvent, Status>>,
    ) {
        let start = Instant::now();
        // Pace and bound concurrency *before* the provider call, never
        // after — the same discipline
        // `ai::queue::worker::AiWorkerPool::process_one` documents for its
        // own queued dispatch, applied here against the *same* shared
        // budget (see `AiApi::new`'s own docs on why this pool's semaphore/
        // rate limiter are shared with the queue's, not a second,
        // independent pair).
        let _permit = match self.acquire_capacity(&cancel).await {
            Ok(permit) => permit,
            Err(status) => {
                let _ = send(&tx, &cancel, Err(status)).await;
                return;
            }
        };
        let mut stream = match self.provider.stream(&prepared.request, &cancel).await {
            Ok(stream) => stream,
            Err(e) => {
                self.audit_forced_failure(
                    message_id,
                    prepared.account_id,
                    &prepared.request.model,
                    &prepared.payload,
                    &prepared.redaction_level,
                    start.elapsed(),
                    &e,
                )
                .await;
                let _ = send(&tx, &cancel, Err(Status::from(e))).await;
                return;
            }
        };

        let mut text = String::new();
        let mut usage = ai::provider::Usage::default();
        loop {
            let next = tokio::select! {
                () = cancel.cancelled() => {
                    self.audit_cancelled_analysis(message_id, &prepared, start).await;
                    return;
                }
                // Detected the instant the client disconnects (tonic drops
                // the receiving end of `tx`), not merely on this task's next
                // attempt to send — without this race, a disconnect while
                // waiting on the *next* upstream frame (nothing to send yet)
                // would go unnoticed until Claude happened to produce one.
                // Cancelling here is what "aborts upstream on cancel" means
                // in practice: dropping `stream` on return closes its
                // internal channel, which is exactly the signal
                // `provider::spawn_sse_reader`'s own `tx.closed()` race is
                // watching to drop the live `reqwest::Response` and end the
                // HTTP request to Claude, not just the local relay.
                () = tx.closed() => {
                    cancel.cancel();
                    self.audit_cancelled_analysis(message_id, &prepared, start).await;
                    return;
                }
                next = stream.next() => next,
            };
            let Some(frame) = next else {
                let e =
                    Error::unavailable("claude closed the stream before it finished".to_owned());
                self.audit_forced_failure(
                    message_id,
                    prepared.account_id,
                    &prepared.request.model,
                    &prepared.payload,
                    &prepared.redaction_level,
                    start.elapsed(),
                    &e,
                )
                .await;
                let _ = send(&tx, &cancel, Err(Status::from(e))).await;
                return;
            };
            match frame {
                Ok(StreamFrame::Token(token)) => {
                    text.push_str(&token);
                    let event = analyze_event(analyze_event::Event::Token(token));
                    if send(&tx, &cancel, Ok(event)).await.is_break() {
                        self.audit_cancelled_analysis(message_id, &prepared, start)
                            .await;
                        return;
                    }
                }
                Ok(StreamFrame::ToolUseStart { id, name }) => {
                    let event =
                        analyze_event(analyze_event::Event::ToolUseStart(ProtoToolUseStart {
                            id,
                            name,
                        }));
                    if send(&tx, &cancel, Ok(event)).await.is_break() {
                        self.audit_cancelled_analysis(message_id, &prepared, start)
                            .await;
                        return;
                    }
                }
                Ok(StreamFrame::Usage(u)) => {
                    usage = u;
                    let event = analyze_event(analyze_event::Event::Usage(to_proto_usage(u)));
                    if send(&tx, &cancel, Ok(event)).await.is_break() {
                        self.audit_cancelled_analysis(message_id, &prepared, start)
                            .await;
                        return;
                    }
                }
                Ok(StreamFrame::Done { stop_reason }) => {
                    let latency = start.elapsed();
                    let outcome = match self
                        .persist_forced_result(
                            message_id,
                            prepared.account_id,
                            &prepared.request.model,
                            &prepared.payload,
                            &prepared.redaction_level,
                            &prepared.tokens,
                            usage,
                            latency,
                            &text,
                        )
                        .await
                    {
                        Ok(summary) => Ok(analyze_event(analyze_event::Event::Done(ProtoDone {
                            stop_reason: stop_reason_str(stop_reason).to_owned(),
                            result: Some(summary),
                        }))),
                        Err(status) => Err(status),
                    };
                    let _ = send(&tx, &cancel, outcome).await;
                    return;
                }
                Err(e) => {
                    self.audit_forced_failure(
                        message_id,
                        prepared.account_id,
                        &prepared.request.model,
                        &prepared.payload,
                        &prepared.redaction_level,
                        start.elapsed(),
                        &e,
                    )
                    .await;
                    let _ = send(&tx, &cancel, Err(Status::from(e))).await;
                    return;
                }
            }
        }
    }

    /// Persist a successful forced deep-pass result — audit the call, write
    /// the artifact via [`DeepPassHandler::on_success`] (the identical
    /// write the queued path uses), publish the `AiSummary` event, and
    /// return the freshly merged [`ProtoSummary`]. Shared tail for
    /// `AnalyzeMessage`'s `Done` frame and `SuggestReply`'s unary response —
    /// the same "one function both callers share" discipline
    /// `ai::queue::worker::finish_call` uses for its own two callers (live
    /// dispatch and batch poll).
    #[allow(clippy::too_many_arguments)]
    async fn persist_forced_result(
        &self,
        message_id: i64,
        account_id: i64,
        model: &str,
        payload: &[u8],
        redaction_level: &str,
        tokens: &TokenMap,
        usage: ai::provider::Usage,
        latency: Duration,
        text: &str,
    ) -> Result<ProtoSummary, Status> {
        let record = CallRecord {
            account_id: Some(account_id),
            message_id: Some(message_id),
            request_id: None,
            model: model.to_owned(),
            pass: Some(deep::PASS.to_owned()),
            usage,
            redaction_level: redaction_level.to_owned(),
            latency,
            payload,
            outcome: CallOutcome::Ok,
        };
        let ledger_entry_id = ai::record_call(&self.db, record)
            .await
            .map_err(Status::from)?;
        let rehydrated = ai::rehydrate(text, tokens);
        let lease = synthetic_lease(message_id, account_id);
        self.deep
            .on_success(&lease, &rehydrated, ledger_entry_id)
            .await
            .map_err(Status::from)?;
        announce(&self.events, account_id, message_id, deep::PASS).await;

        let rows = read_summary(&self.db, message_id)
            .await
            .map_err(Status::from)?;
        Ok(to_proto_summary(message_id, &rows))
    }

    /// Audit a forced call that never produced a result — mirrors
    /// `ai::queue::worker::finish_call`'s own error-path auditing, so a
    /// forced call that failed still leaves a ledger row (`cost_usd = 0`,
    /// `status = error`) proving the attempt was made.
    #[allow(clippy::too_many_arguments)]
    async fn audit_forced_failure(
        &self,
        message_id: i64,
        account_id: i64,
        model: &str,
        payload: &[u8],
        redaction_level: &str,
        latency: Duration,
        error: &Error,
    ) {
        let record = CallRecord {
            account_id: Some(account_id),
            message_id: Some(message_id),
            request_id: None,
            model: model.to_owned(),
            pass: Some(deep::PASS.to_owned()),
            usage: ai::provider::Usage::default(),
            redaction_level: redaction_level.to_owned(),
            latency,
            payload,
            outcome: CallOutcome::Error(error.to_string()),
        };
        if let Err(e) = ai::record_call(&self.db, record).await {
            tracing::error!(message_id, error = %e, "failed to audit a failed forced ai call");
        }
    }

    /// [`Self::audit_forced_failure`], for the specific case of
    /// [`Self::run_analyze_stream`] cutting a call short because the client
    /// disconnected or the request was cancelled *after* the provider call
    /// had already started (and, for however many `Token` frames already
    /// arrived, already been billed by the provider). Every early return in
    /// that method for exactly this reason routes through here, rather than
    /// a bare `return` — without it, tokens Claude had already generated by
    /// the moment a client hung up were never recorded anywhere, so
    /// `ai.limits.daily_cost_cap_usd`/`daily_token_cap` would undercount
    /// them and a client could spend real money invisibly by starting and
    /// aborting `AnalyzeMessage` repeatedly.
    async fn audit_cancelled_analysis(
        &self,
        message_id: i64,
        prepared: &PreparedAnalysis,
        start: Instant,
    ) {
        self.audit_forced_failure(
            message_id,
            prepared.account_id,
            &prepared.request.model,
            &prepared.payload,
            &prepared.redaction_level,
            start.elapsed(),
            &Error::deadline_exceeded(
                "the analysis was cancelled or the client disconnected before it finished"
                    .to_owned(),
            ),
        )
        .await;
    }
}

/// One `AnalyzeEvent` wrapping `event`.
fn analyze_event(event: analyze_event::Event) -> AnalyzeEvent {
    AnalyzeEvent { event: Some(event) }
}

fn to_proto_usage(usage: ai::provider::Usage) -> ProtoUsage {
    ProtoUsage {
        input_tokens: i64::from(usage.input_tokens),
        output_tokens: i64::from(usage.output_tokens),
        cache_creation_input_tokens: i64::from(usage.cache_creation_input_tokens),
        cache_read_input_tokens: i64::from(usage.cache_read_input_tokens),
    }
}

fn stop_reason_str(reason: ai::provider::StopReason) -> &'static str {
    match reason {
        ai::provider::StopReason::EndTurn => "end_turn",
        ai::provider::StopReason::MaxTokens => "max_tokens",
        ai::provider::StopReason::StopSequence => "stop_sequence",
        ai::provider::StopReason::ToolUse => "tool_use",
        ai::provider::StopReason::PauseTurn => "pause_turn",
    }
}

/// A synthetic lease for a forced (queue-bypassing) call —
/// [`DeepPassHandler::on_success`] only ever reads `message_id`/`account_id`
/// off it (see `ai_service.rs`'s own module docs), never the lease-fencing
/// fields, so a placeholder `job_id`/`worker` here is safe.
fn synthetic_lease(message_id: i64, account_id: i64) -> AiLease {
    AiLease {
        job_id: 0,
        message_id,
        account_id,
        pass: deep::PASS.to_owned(),
        // A forced `AnalyzeMessage`/`SuggestReply` is the definition of
        // interactive work — a user is waiting on it — so it carries the
        // priority the queue reserves for exactly that, and the budget
        // enforcer charges it to the interactive side of the ledger.
        priority: rmail_core::ai::queue::PRIORITY_RECENT,
        attempts: 0,
        lease_expires_at: 0,
        worker: "ai-service-forced".to_owned(),
    }
}

/// Publish the `AiSummary` event for a forced (queue-bypassing) result — the
/// same event [`rmail_core::ai::queue`]'s own `finish_call` publishes for a
/// queued completion, duplicated rather than shared across the crate
/// boundary (that function is `pub(super)` to `rmail_core::ai::queue`, not
/// reachable from `rmaild`) so `StreamEnrichments` sees a forced analysis
/// exactly as promptly as a queued one.
async fn announce(events: &EventLog, account_id: i64, message_id: i64, pass: &str) {
    let event = NewEvent::new(EventKind::AiSummary)
        .account(account_id)
        .message(message_id)
        .payload(serde_json::json!({ "pass": pass }));
    if let Err(e) = events.append(event).await {
        tracing::warn!(
            message_id,
            error = %e,
            "failed to publish the ai summary event for a forced analysis"
        );
    }
}

/// The account/mailbox names a policy resolution needs — the same query
/// `ai::queue::worker`'s own `target_names` runs, duplicated for the reason
/// that function's own docs give for its sibling in `ai::queue::batch`: it
/// is three lines, and it is `pub(super)` to a module this crate cannot
/// reach into.
async fn target_names(db: &Database, message_id: i64) -> Result<Option<(String, String)>, Error> {
    db.read(move |conn| {
        conn.query_row(
            "SELECT a.name, mb.name
             FROM messages m
             JOIN accounts a ON a.id = m.account_id
             JOIN mailboxes mb ON mb.id = m.mailbox_id
             WHERE m.id = ?1",
            [message_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
    })
    .await
    .map_err(Error::from)
}

// ---------------------------------------------------------------------------
// Reading and merging ai_summaries
// ---------------------------------------------------------------------------

struct RawTriage {
    model: String,
    thread_id: Option<i64>,
    tl_dr: Option<String>,
    sentiment: Option<String>,
    category: Option<String>,
    priority: Option<String>,
    needs_reply: Option<bool>,
    suggested_tags: Option<String>,
}

struct RawDeep {
    model: String,
    thread_id: Option<i64>,
    summary: Option<String>,
    thread_summary: Option<String>,
    key_points: Option<String>,
    todos: Option<String>,
    suggested_reply: Option<String>,
}

struct RawEntity {
    kind: String,
    value: String,
    iso: Option<String>,
    amount: Option<f64>,
    currency: Option<String>,
}

/// Everything [`to_proto_summary`] needs, read in one round trip.
struct SummaryRows {
    message_exists: bool,
    triage: Option<RawTriage>,
    deep: Option<RawDeep>,
    entities: Vec<RawEntity>,
    /// Whether a `pending`/`leased` `ai_queue` row exists for this message —
    /// what tells [`to_proto_summary`] apart PENDING from NOT_QUEUED when
    /// neither pass has produced a result yet.
    queued: bool,
}

async fn read_summary(db: &Database, message_id: i64) -> Result<SummaryRows, Error> {
    db.read(move |conn| {
        let message_exists: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM messages WHERE id = ?1)",
            [message_id],
            |row| row.get(0),
        )?;
        if !message_exists {
            return Ok(SummaryRows {
                message_exists: false,
                triage: None,
                deep: None,
                entities: Vec::new(),
                queued: false,
            });
        }

        let triage = conn
            .query_row(
                "SELECT model, thread_id, tl_dr, sentiment, category, priority, needs_reply, \
                        suggested_tags
                 FROM ai_summaries WHERE message_id = ?1 AND pass = 'triage'
                 ORDER BY created_at DESC, id DESC LIMIT 1",
                [message_id],
                |row| {
                    Ok(RawTriage {
                        model: row.get(0)?,
                        thread_id: row.get(1)?,
                        tl_dr: row.get(2)?,
                        sentiment: row.get(3)?,
                        category: row.get(4)?,
                        priority: row.get(5)?,
                        needs_reply: row.get(6)?,
                        suggested_tags: row.get(7)?,
                    })
                },
            )
            .optional()?;

        let deep = conn
            .query_row(
                "SELECT model, thread_id, summary, thread_summary, key_points, todos, \
                        suggested_reply
                 FROM ai_summaries WHERE message_id = ?1 AND pass = 'deep'
                 ORDER BY created_at DESC, id DESC LIMIT 1",
                [message_id],
                |row| {
                    Ok(RawDeep {
                        model: row.get(0)?,
                        thread_id: row.get(1)?,
                        summary: row.get(2)?,
                        thread_summary: row.get(3)?,
                        key_points: row.get(4)?,
                        todos: row.get(5)?,
                        suggested_reply: row.get(6)?,
                    })
                },
            )
            .optional()?;

        let entities = if let Some(deep) = &deep {
            let mut stmt = conn.prepare(
                "SELECT kind, value, iso, amount, currency FROM ai_entities \
                 WHERE message_id = ?1 AND model = ?2",
            )?;
            let mapped = stmt.query_map(rusqlite::params![message_id, deep.model], |row| {
                Ok(RawEntity {
                    kind: row.get(0)?,
                    value: row.get(1)?,
                    iso: row.get(2)?,
                    amount: row.get(3)?,
                    currency: row.get(4)?,
                })
            })?;
            let rows: Vec<RawEntity> = mapped.collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        } else {
            Vec::new()
        };

        let queued: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM ai_queue WHERE message_id = ?1 AND state IN \
             ('pending', 'leased'))",
            [message_id],
            |row| row.get(0),
        )?;

        Ok(SummaryRows {
            message_exists: true,
            triage,
            deep,
            entities,
            queued,
        })
    })
    .await
    .map_err(Error::from)
}

/// `rows.message_exists` must already be `true` — callers check that
/// separately (and return NOT_FOUND) before reaching here.
fn to_proto_summary(message_id: i64, rows: &SummaryRows) -> ProtoSummary {
    let status = if rows.triage.is_some() || rows.deep.is_some() {
        SummaryStatus::Ok
    } else if rows.queued {
        SummaryStatus::Pending
    } else {
        SummaryStatus::NotQueued
    };
    let thread_id = rows
        .triage
        .as_ref()
        .and_then(|t| t.thread_id)
        .or_else(|| rows.deep.as_ref().and_then(|d| d.thread_id));

    let (triage_model, tl_dr, sentiment, category, priority, needs_reply, suggested_tags) =
        match &rows.triage {
            Some(t) => (
                Some(t.model.clone()),
                t.tl_dr.clone(),
                t.sentiment.clone(),
                t.category.clone(),
                t.priority.clone(),
                t.needs_reply,
                parse_string_array(t.suggested_tags.as_deref()),
            ),
            None => (None, None, None, None, None, None, Vec::new()),
        };

    let (deep_model, summary, thread_summary, key_points, todos, suggested_reply) = match &rows.deep
    {
        Some(d) => (
            Some(d.model.clone()),
            d.summary.clone(),
            d.thread_summary.clone(),
            parse_string_array(d.key_points.as_deref()),
            parse_todos(d.todos.as_deref()),
            d.suggested_reply.clone(),
        ),
        None => (None, None, None, Vec::new(), Vec::new(), None),
    };

    ProtoSummary {
        message_id,
        thread_id,
        status: status as i32,
        triage_model,
        tl_dr,
        sentiment,
        category,
        priority,
        needs_reply,
        suggested_tags,
        deep_model,
        summary,
        thread_summary,
        key_points,
        todos,
        entities: rows.entities.iter().map(to_proto_entity).collect(),
        suggested_reply,
    }
}

fn to_proto_entity(entity: &RawEntity) -> ProtoEntity {
    ProtoEntity {
        kind: entity.kind.clone(),
        value: entity.value.clone(),
        iso: entity.iso.clone(),
        amount: entity.amount,
        currency: entity.currency.clone(),
    }
}

/// Parse a JSON-array `ai_summaries` column (`suggested_tags`/`key_points`).
/// A parse failure here means the column holds something
/// `TriagePassHandler`/`DeepPassHandler` never wrote — corrupt data, not a
/// normal empty case — so it is logged rather than silently read back as an
/// empty (and therefore indistinguishable from "the model said nothing")
/// list.
fn parse_string_array(raw: Option<&str>) -> Vec<String> {
    let Some(raw) = raw else {
        return Vec::new();
    };
    match serde_json::from_str(raw) {
        Ok(values) => values,
        Err(error) => {
            tracing::warn!(%error, raw, "ai_summaries column was not a JSON string array");
            Vec::new()
        }
    }
}

#[derive(serde::Deserialize)]
struct WireTodo {
    text: String,
    due: Option<String>,
    owner: Option<String>,
}

/// As [`parse_string_array`], for the `todos` column's richer shape.
fn parse_todos(raw: Option<&str>) -> Vec<ProtoTodo> {
    let Some(raw) = raw else {
        return Vec::new();
    };
    let todos: Vec<WireTodo> = match serde_json::from_str(raw) {
        Ok(todos) => todos,
        Err(error) => {
            tracing::warn!(%error, raw, "ai_summaries.todos was not the expected JSON shape");
            return Vec::new();
        }
    };
    todos
        .into_iter()
        .map(|t| ProtoTodo {
            text: t.text,
            due: t.due,
            owner: t.owner,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// StreamEnrichments backlog/live helpers
// ---------------------------------------------------------------------------

/// Every enrichment for the next `limit` distinct message ids past
/// `since_message_id`, each resolved to the merged [`ProtoSummary`] as of
/// this read; the cursor to resume from (the largest message id this page
/// *scanned*, whether or not it produced an `Enrichment`); and whether this
/// page hit `limit` distinct ids (so the caller knows to fetch another page
/// rather than assuming the backlog is exhausted).
///
/// Paginating by *distinct message id* rather than by raw `ai_summaries` row
/// is deliberate: a message with both a triage and a deep row sorts as two
/// adjacent rows under `ORDER BY message_id, pass`, and an earlier version
/// of this function paginated over that raw row list directly. A page
/// boundary landing between those two rows (triage as the last row of one
/// page, deep as the first row of the next) advanced the cursor to that
/// message's id *before* its second row had been read, and the next page's
/// `message_id > cursor` then excluded that row forever — silently losing
/// exactly the deep-pass enrichment for whichever message happened to sit
/// on a page boundary. Grouping by message id first means a page's cursor
/// only ever advances past ids whose *every* row was already included in
/// this page.
///
/// The returned cursor advances to the last *scanned* id even when a given
/// id produced no `Enrichment` (deleted between the listing read and this
/// function returning) — the same "advance past what was scanned, not just
/// what matched" discipline [`crate::events::EventLog::since`] documents for
/// itself, and for an identical reason: without it, a page whose every id
/// happened to be a since-deleted message would report `more = true` (it hit
/// `limit` distinct ids) with an empty result and a stalled cursor, and the
/// caller would re-issue the exact same query forever.
async fn backlog_page(
    db: &Database,
    account_filter: Option<i64>,
    since_message_id: i64,
    limit: i64,
) -> Result<(Vec<(i64, String, ProtoSummary)>, i64, bool), Error> {
    let message_ids: Vec<i64> = db
        .read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT DISTINCT message_id FROM ai_summaries
                 WHERE message_id > ?1 AND (?2 IS NULL OR account_id = ?2)
                 ORDER BY message_id
                 LIMIT ?3",
            )?;
            let rows = stmt
                .query_map(
                    rusqlite::params![since_message_id, account_filter, limit],
                    |row| row.get(0),
                )?
                .collect::<rusqlite::Result<Vec<i64>>>()?;
            Ok(rows)
        })
        .await?;
    // Computed from the id count, before the per-message existence/pass
    // filtering below (which can only ever shrink the output, never the
    // scanned range): a page that found `limit` distinct ids may still have
    // more beyond it even if every one of those ids turned out to have
    // nothing left to report.
    let more = message_ids.len() as i64 >= limit;
    let next_cursor = message_ids.last().copied().unwrap_or(since_message_id);

    let mut out = Vec::with_capacity(message_ids.len());
    for message_id in message_ids {
        let full = read_summary(db, message_id).await?;
        if !full.message_exists {
            // Deleted between the listing read and here — nothing left to
            // report for it, but the cursor above already accounts for
            // having scanned it.
            continue;
        }
        if full.triage.is_some() {
            out.push((
                message_id,
                triage::PASS.to_owned(),
                to_proto_summary(message_id, &full),
            ));
        }
        if full.deep.is_some() {
            out.push((
                message_id,
                deep::PASS.to_owned(),
                to_proto_summary(message_id, &full),
            ));
        }
    }
    Ok((out, next_cursor, more))
}

/// Turn one live event into an `Enrichment`, or `None` if it should be
/// skipped (not an `AiSummary` event, filtered out by account, or the
/// message vanished in the meantime). Deliberately does *not* filter by
/// message_id against a resume cursor — see the call site's own comment for
/// why the live tail favors an occasional duplicate over ever silently
/// dropping an enrichment.
async fn enrichment_for_event(
    db: &Database,
    event: &CoreEvent,
    account_filter: Option<i64>,
) -> Option<Result<Enrichment, Error>> {
    if event.kind != EventKind::AiSummary {
        return None;
    }
    let message_id = event.message_id?;
    if let Some(account_id) = account_filter {
        if event.account_id != Some(account_id) {
            return None;
        }
    }
    let pass = event
        .payload
        .get("pass")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    match read_summary(db, message_id).await {
        Ok(rows) if rows.message_exists => Some(Ok(Enrichment {
            message_id,
            pass,
            summary: Some(to_proto_summary(message_id, &rows)),
        })),
        Ok(_) => None,
        Err(e) => Some(Err(e)),
    }
}

// ---------------------------------------------------------------------------
// Usage
// ---------------------------------------------------------------------------

fn to_proto_day_usage(usage: ai::DayUsage) -> ProtoDayUsage {
    ProtoDayUsage {
        day: usage.day,
        requests: usage.requests,
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_creation_input_tokens: usage.cache_creation_input_tokens,
        cache_read_input_tokens: usage.cache_read_input_tokens,
        cost_usd: usage.cost_usd,
    }
}

fn empty_day_usage(day: &str) -> ProtoDayUsage {
    ProtoDayUsage {
        day: day.to_owned(),
        requests: 0,
        input_tokens: 0,
        output_tokens: 0,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
        cost_usd: 0.0,
    }
}

/// This calendar month's rolled-up usage, summed across every `ai_usage` row
/// whose `day` falls in it — `ai::usage_for_day` only ever answers for one
/// day, so this is the RPC-local equivalent of `ai::queue`'s own private
/// `month_cost_usd`, extended to every token field `GetUsage` reports rather
/// than just cost.
async fn month_usage(db: &Database, month: &str) -> Result<ProtoDayUsage, Status> {
    let prefix = format!("{month}%");
    let month_owned = month.to_owned();
    db.read(move |conn| {
        conn.query_row(
            "SELECT COALESCE(SUM(requests), 0), COALESCE(SUM(input_tokens), 0), \
                    COALESCE(SUM(output_tokens), 0), \
                    COALESCE(SUM(cache_creation_input_tokens), 0), \
                    COALESCE(SUM(cache_read_input_tokens), 0), COALESCE(SUM(cost_usd), 0)
             FROM ai_usage WHERE day LIKE ?1",
            [prefix],
            |row| {
                Ok(ProtoDayUsage {
                    day: month_owned.clone(),
                    requests: row.get(0)?,
                    input_tokens: row.get(1)?,
                    output_tokens: row.get(2)?,
                    cache_creation_input_tokens: row.get(3)?,
                    cache_read_input_tokens: row.get(4)?,
                    cost_usd: row.get(5)?,
                })
            },
        )
    })
    .await
    .map_err(Error::from)
    .map_err(Status::from)
}

/// The UTC calendar day `now` falls on, as `"YYYY-MM-DD"` — the same format
/// `ai::audit`'s own (private) `day_key` uses, duplicated for the reason
/// this file's other small duplicated helpers give: it is a few lines, and
/// the original is private to a crate this one only calls into.
fn today_key() -> String {
    chrono::Utc::now().format("%Y-%m-%d").to_string()
}

// ---------------------------------------------------------------------------
// Streaming plumbing
// ---------------------------------------------------------------------------

/// Send one stream item, giving up if the client went away or the daemon is
/// stopping — identical to `sync_service::send`/`search_service::send`;
/// duplicated per this codebase's own established precedent for this exact
/// helper (see `search_service::send`'s doc comment).
async fn send<T>(
    tx: &mpsc::Sender<Result<T, Status>>,
    cancel: &CancellationToken,
    item: Result<T, Status>,
) -> std::ops::ControlFlow<()> {
    tokio::select! {
        () = cancel.cancelled() => std::ops::ControlFlow::Break(()),
        sent = tx.send(item) => {
            if sent.is_ok() {
                std::ops::ControlFlow::Continue(())
            } else {
                // The receiver dropped -- the client disconnected. Cancelling
                // here (idempotent if `cancel` was already cancelled) is what
                // lets `AnalyzeMessage`'s upstream Claude call notice and stop
                // rather than only the local relay — see `run_analyze_stream`'s
                // own `tx.closed()` race for the fuller reasoning; harmless for
                // callers with nothing "upstream" to abort (`StreamEnrichments`),
                // since they were about to return on this same `Break` anyway.
                cancel.cancel();
                std::ops::ControlFlow::Break(())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// A provider that always declines — the fallback when AI is disabled
// ---------------------------------------------------------------------------

/// Stands in for the real provider when `ai.enabled = false` or the
/// configured provider failed to build (an empty `api_key_command`, the only
/// way that happens today — see `provider::build`'s own docs). Every call
/// fails fast with the same [`Error::FailedPrecondition`] a policy-forbidden
/// target would, so `AnalyzeMessage`/`SuggestReply` degrade gracefully
/// rather than needing a separate "is AI even on" branch in every handler —
/// and `GetSummary`/`StreamEnrichments`/`GetUsage` keep working regardless,
/// since none of them ever reach a `Provider` at all, matching prd.md's
/// "AI down → AiService health NOT_SERVING, mail features unaffected."
#[derive(Debug)]
pub struct NullProvider;

#[tonic::async_trait]
impl Provider for NullProvider {
    async fn complete(
        &self,
        _request: &ai::ChatRequest,
        _cancel: &CancellationToken,
    ) -> Result<ai::ChatResponse, Error> {
        Err(disabled_error())
    }

    async fn stream(
        &self,
        _request: &ai::ChatRequest,
        _cancel: &CancellationToken,
    ) -> Result<ai::ProviderStream, Error> {
        Err(disabled_error())
    }
}

fn disabled_error() -> Error {
    Error::failed_precondition(
        "AI is disabled on this daemon (ai.enabled = false, or no provider could be built)"
            .to_owned(),
    )
}

/// So `serve_uds_with_engine_and_mail_store`'s wiring can name these two
/// pass identifiers without reaching into `rmail_core::ai::{triage,deep}`
/// itself just for two string constants.
pub const TRIAGE_PASS: &str = triage::PASS;
pub const DEEP_PASS: &str = deep::PASS;

use rusqlite::OptionalExtension;
