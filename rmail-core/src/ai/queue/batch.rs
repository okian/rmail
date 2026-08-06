//! The Message Batches API path: flips a pass from live per-request calls to
//! one bulk submission once its backlog crosses `ai.batching.threshold`, at
//! 50% of the live per-token price.
//!
//! Two pieces, deliberately separate:
//!
//! - [`BatchClient`] is the thin HTTP client for the three endpoints this
//!   path needs (`POST .../batches`, `GET .../batches/{id}`,
//!   `GET .../batches/{id}/results`) — reqwest in, typed responses out,
//!   nothing about the queue.
//! - [`BatchCoordinator`] is the policy → assemble → redact → submit → poll
//!   → audit orchestration around it, composing [`BatchClient`] with
//!   [`AiQueue`] the same way [`super::AiWorkerPool`] composes
//!   [`crate::ai::provider::Provider`] with it for the live path. The two
//!   share their post-response handling exactly —
//!   [`super::worker::finish_call`] is called from both — so a message
//!   processed once live and once via batch is audited and rehydrated
//!   identically.
//!
//! See the parent module's docs for why a batch's in-flight bookkeeping
//! (the redacted request and token map needed to audit and rehydrate its
//! eventual result) lives only in the coordinator's process memory, and
//! what happens to a batch whose coordinator restarts before it ends.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use rusqlite::OptionalExtension;

use crate::ai::policy::{PolicyEngine, PolicyTarget};
use crate::ai::provider::{ChatRequest, ChatResponse, RawMessage, Role};
use crate::ai::redact::{self, GuardedRequest, TokenMap};
use crate::config::{AiBatching, AiLimits, AiPrivacy};
use crate::credential::{CredentialSource, Secret};
use crate::error::{Error, ErrorReason};
use crate::storage::Database;

use super::content::assemble_content;
use super::worker::{finish_call, DispatchSummary, PassHandler};
use super::{AiLease, AiQueue, CapDecision, CostGate, BATCH_LEASE, BATCH_WORKER};

/// The Message Batches API's discount over the live per-request price.
const BATCH_PRICE_MULTIPLIER: f64 = 0.5;

/// Where batch requests go, unless a test points them somewhere else.
const DEFAULT_ENDPOINT: &str = "https://api.anthropic.com/v1/messages/batches";

/// Bounds the TCP+TLS handshake and the response for every batch-endpoint
/// call. Unlike `provider.rs`'s `send_with_retry`, this client does not
/// retry — a create/status/results call is infrequent (once per
/// threshold-crossing, or once per scheduled poll tick) and a caller-driven
/// retry on the next tick is simpler than a second copy of that module's
/// backoff loop for a path this rarely used.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Most of an upstream error body to repeat back into a log line.
const MAX_UPSTREAM_DETAIL: usize = 200;

// ---------------------------------------------------------------------------
// Public vocabulary
// ---------------------------------------------------------------------------

/// One item of a Message Batches submission.
#[derive(Debug, Clone)]
pub struct BatchRequestItem {
    /// Correlates a result back to the request that produced it. This
    /// module always sets it to the message id (as a decimal string) — the
    /// acceptance criterion's `custom_id = message_id` — which is unique
    /// within one submission because a submission only ever covers jobs of
    /// one `pass` and `(message_id, pass)` is `ai_queue`'s own dedup key.
    pub custom_id: String,
    /// The (already redacted) request.
    pub params: ChatRequest,
}

/// What `POST .../batches` returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchHandle {
    /// The batch's id, used for every subsequent status/results call.
    pub id: String,
    /// `"in_progress"` immediately after submission.
    pub processing_status: String,
}

/// Per-outcome counts on a [`BatchStatus`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BatchRequestCounts {
    /// Still being processed.
    pub processing: i64,
    /// Completed successfully.
    pub succeeded: i64,
    /// Completed with an error.
    pub errored: i64,
    /// Canceled before completion.
    pub canceled: i64,
    /// Expired before completion.
    pub expired: i64,
}

/// What `GET .../batches/{id}` returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchStatus {
    /// The batch's id.
    pub id: String,
    /// `"in_progress"` | `"canceling"` | `"ended"`.
    pub processing_status: String,
    /// Per-outcome counts, updated as the batch processes.
    pub request_counts: BatchRequestCounts,
}

impl BatchStatus {
    /// Whether every item in the batch has reached a terminal outcome and
    /// `GET .../batches/{id}/results` has something to return.
    #[must_use]
    pub fn is_ended(&self) -> bool {
        self.processing_status == "ended"
    }
}

/// One item's outcome from `GET .../batches/{id}/results`.
#[derive(Debug)]
pub enum BatchOutcome {
    /// The call succeeded; this is the same [`ChatResponse`] a live
    /// [`crate::ai::provider::Provider::complete`] call would have
    /// produced, decoded through the identical
    /// [`ChatResponse::from_raw`](crate::ai::provider::ChatResponse::from_raw)
    /// conversion — refusal handling included.
    Succeeded(ChatResponse),
    /// The call failed; the message is the upstream error's own message.
    Errored(String),
    /// The batch was canceled before this item ran.
    Canceled,
    /// This item expired before the batch finished.
    Expired,
}

/// One decoded line from the batch results JSONL body.
#[derive(Debug)]
pub struct BatchResult {
    /// Echoes the [`BatchRequestItem::custom_id`] this result answers.
    pub custom_id: String,
    /// What happened.
    pub outcome: BatchOutcome,
}

/// What a [`BatchCoordinator::poll`] call did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchPollOutcome {
    /// The batch has not ended yet; nothing to do.
    StillRunning,
    /// The batch ended and every result this coordinator had a record for
    /// was processed.
    Completed(DispatchSummary),
}

// ---------------------------------------------------------------------------
// BatchClient: the raw HTTP surface
// ---------------------------------------------------------------------------

/// A thin, single-attempt HTTP client for the three Message Batches
/// endpoints. Knows nothing about `ai_queue`, redaction, or audit — see the
/// module docs for why that orchestration lives in [`BatchCoordinator`]
/// instead.
#[derive(Debug)]
pub struct BatchClient {
    endpoint: String,
    client: reqwest::Client,
}

impl BatchClient {
    /// A client pointed at the real Message Batches API.
    ///
    /// # Errors
    /// [`Error::FailedPrecondition`] if the underlying HTTP client cannot be
    /// built.
    pub fn new() -> Result<Self, Error> {
        crate::transport::install_crypto_provider();
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| {
                Error::failed_precondition(format!(
                    "could not build an HTTP client for the batch API: {e}"
                ))
            })?;
        Ok(Self {
            endpoint: DEFAULT_ENDPOINT.to_owned(),
            client,
        })
    }

    /// Point this client at another endpoint. Exists so tests can drive a
    /// local mock server; nothing in production calls it.
    #[must_use]
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    /// Submit a batch. `custom_id`s must be unique within `requests` — see
    /// [`BatchRequestItem::custom_id`] for why this queue's caller always
    /// satisfies that.
    ///
    /// # Errors
    /// A mapped HTTP or decode failure.
    pub async fn submit(
        &self,
        key: &str,
        requests: &[BatchRequestItem],
    ) -> Result<BatchHandle, Error> {
        let body = serde_json::json!({
            "requests": requests
                .iter()
                .map(|item| serde_json::json!({
                    "custom_id": item.custom_id,
                    "params": batch_params(&item.params),
                }))
                .collect::<Vec<_>>(),
        });
        let response = self
            .client
            .post(&self.endpoint)
            .header("x-api-key", key)
            .header("anthropic-version", crate::ai::provider::ANTHROPIC_VERSION)
            .json(&body)
            .send()
            .await
            .map_err(|e| Error::unavailable(format!("claude batch submit failed: {e}")))?;
        let status = response.status();
        if !status.is_success() {
            let detail = response.text().await.unwrap_or_default();
            return Err(map_batch_http_error(status, &detail));
        }
        let raw: RawBatchHandle = response.json().await.map_err(|e| {
            Error::unavailable(format!("could not read the claude batch response: {e}"))
        })?;
        Ok(BatchHandle {
            id: raw.id,
            processing_status: raw.processing_status,
        })
    }

    /// Fetch a batch's current status.
    ///
    /// # Errors
    /// A mapped HTTP or decode failure.
    pub async fn status(&self, key: &str, batch_id: &str) -> Result<BatchStatus, Error> {
        let url = format!("{}/{batch_id}", self.endpoint);
        let response = self
            .client
            .get(&url)
            .header("x-api-key", key)
            .header("anthropic-version", crate::ai::provider::ANTHROPIC_VERSION)
            .send()
            .await
            .map_err(|e| Error::unavailable(format!("claude batch status check failed: {e}")))?;
        let status = response.status();
        if !status.is_success() {
            let detail = response.text().await.unwrap_or_default();
            return Err(map_batch_http_error(status, &detail));
        }
        let raw: RawBatchStatus = response.json().await.map_err(|e| {
            Error::unavailable(format!("could not read the claude batch status: {e}"))
        })?;
        Ok(BatchStatus {
            id: raw.id,
            processing_status: raw.processing_status,
            request_counts: BatchRequestCounts {
                processing: raw.request_counts.processing,
                succeeded: raw.request_counts.succeeded,
                errored: raw.request_counts.errored,
                canceled: raw.request_counts.canceled,
                expired: raw.request_counts.expired,
            },
        })
    }

    /// Fetch and decode a batch's results (JSONL — one JSON object per
    /// line). Results arrive in no particular order; callers key by
    /// [`BatchResult::custom_id`], never by position.
    ///
    /// # Errors
    /// A mapped HTTP or decode failure.
    pub async fn results(&self, key: &str, batch_id: &str) -> Result<Vec<BatchResult>, Error> {
        let url = format!("{}/{batch_id}/results", self.endpoint);
        let response = self
            .client
            .get(&url)
            .header("x-api-key", key)
            .header("anthropic-version", crate::ai::provider::ANTHROPIC_VERSION)
            .send()
            .await
            .map_err(|e| Error::unavailable(format!("claude batch results fetch failed: {e}")))?;
        let status = response.status();
        if !status.is_success() {
            let detail = response.text().await.unwrap_or_default();
            return Err(map_batch_http_error(status, &detail));
        }
        let text = response.text().await.map_err(|e| {
            Error::unavailable(format!("could not read the claude batch results body: {e}"))
        })?;
        let mut out = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let raw: RawBatchResultLine = serde_json::from_str(line).map_err(|e| {
                Error::unavailable(format!("claude batch results line was not valid JSON: {e}"))
            })?;
            let outcome = match raw.result.kind.as_str() {
                "succeeded" => {
                    let message = raw.result.message.ok_or_else(|| {
                        Error::internal(
                            "claude batch result was 'succeeded' but carried no message".to_owned(),
                        )
                    })?;
                    BatchOutcome::Succeeded(ChatResponse::from_raw(message)?)
                }
                "errored" => BatchOutcome::Errored(
                    raw.result
                        .error
                        .and_then(|e| e.message)
                        .unwrap_or_else(|| "no further detail".to_owned()),
                ),
                "canceled" => BatchOutcome::Canceled,
                "expired" => BatchOutcome::Expired,
                other => {
                    return Err(Error::internal(format!(
                        "unrecognized claude batch result type: {other}"
                    )))
                }
            };
            out.push(BatchResult {
                custom_id: raw.custom_id,
                outcome,
            });
        }
        Ok(out)
    }
}

/// Build one item's `params` object — the same request shape
/// `ClaudeProvider::build_body` sends for a non-streaming call, minus the
/// `stream` field (batches are inherently non-streaming) and minus prompt
/// caching: a batch's items typically execute out of order and in parallel
/// server-side, so a `cache_control` breakpoint placed for a live
/// request/response round trip has no equivalent benefit here, and is left
/// out rather than carried over unexamined.
fn batch_params(request: &ChatRequest) -> serde_json::Value {
    let messages: Vec<serde_json::Value> = request
        .messages
        .iter()
        .map(|m| {
            serde_json::json!({
                "role": match m.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                },
                "content": m.content,
            })
        })
        .collect();
    let mut body = serde_json::json!({
        "model": request.model,
        "max_tokens": request.max_tokens,
        "messages": messages,
    });
    if let Some(system) = &request.system {
        body["system"] = serde_json::Value::String(system.clone());
    }
    if let Some(format) = &request.output_format {
        body["output_config"] = serde_json::json!({
            "format": { "type": "json_schema", "schema": format.schema },
        });
    }
    body
}

/// Map a non-success batch-endpoint response to an [`Error`], distinguished
/// by status the same way `provider.rs`'s `map_http_error` is.
fn map_batch_http_error(status: reqwest::StatusCode, body: &str) -> Error {
    let detail = clip(body);
    match status.as_u16() {
        401 | 403 => Error::unauthenticated(format!(
            "claude rejected the API key on the batch endpoint ({status}): {detail}"
        )),
        408 | 409 | 429 => Error::unavailable(format!(
            "claude batch endpoint is still failing ({status}): {detail}"
        )),
        400..=499 => Error::invalid_argument(format!(
            "claude rejected the batch request ({status}): {detail}"
        )),
        _ => Error::unavailable(format!("claude batch endpoint returned {status}: {detail}")),
    }
}

/// Cut an upstream detail down to something safe to log.
fn clip(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.chars().count() <= MAX_UPSTREAM_DETAIL {
        return trimmed.to_owned();
    }
    let clipped: String = trimmed.chars().take(MAX_UPSTREAM_DETAIL).collect();
    format!("{clipped}… ({} bytes)", trimmed.len())
}

// ---------------------------------------------------------------------------
// Wire shapes
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize)]
struct RawBatchHandle {
    id: String,
    processing_status: String,
}

#[derive(Debug, Default, serde::Deserialize)]
struct RawRequestCounts {
    #[serde(default)]
    processing: i64,
    #[serde(default)]
    succeeded: i64,
    #[serde(default)]
    errored: i64,
    #[serde(default)]
    canceled: i64,
    #[serde(default)]
    expired: i64,
}

#[derive(Debug, serde::Deserialize)]
struct RawBatchStatus {
    id: String,
    processing_status: String,
    #[serde(default)]
    request_counts: RawRequestCounts,
}

#[derive(Debug, serde::Deserialize)]
struct RawBatchResultLine {
    custom_id: String,
    result: RawBatchResultBody,
}

#[derive(Debug, serde::Deserialize)]
struct RawBatchResultBody {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    message: Option<RawMessage>,
    #[serde(default)]
    error: Option<RawBatchError>,
}

#[derive(Debug, serde::Deserialize)]
struct RawBatchError {
    #[serde(default)]
    message: Option<String>,
}

// ---------------------------------------------------------------------------
// BatchCoordinator: policy -> assemble -> redact -> submit -> poll -> audit
// ---------------------------------------------------------------------------

/// One item's redacted payload and token map, kept only in process memory —
/// see the module docs on why this cannot be persisted, and what it costs.
struct InFlightItem {
    job_id: i64,
    message_id: i64,
    account_id: i64,
    /// The exact redacted content submitted, for
    /// [`crate::ai::audit::record_call_priced`]'s payload hash.
    payload: Vec<u8>,
    tokens: TokenMap,
}

/// Orchestrates the batch path: decides when to flip (`maybe_submit`), and
/// resolves a submitted batch's results back into completed/failed
/// [`AiQueue`] rows (`poll`).
pub struct BatchCoordinator {
    db: Database,
    queue: AiQueue,
    client: BatchClient,
    key_source: CredentialSource,
    policy: Arc<PolicyEngine>,
    limits: AiLimits,
    privacy: AiPrivacy,
    batching: AiBatching,
    handlers: Arc<HashMap<String, Arc<dyn PassHandler>>>,
    pending: Mutex<HashMap<String, Vec<InFlightItem>>>,
}

impl std::fmt::Debug for BatchCoordinator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BatchCoordinator")
            .field("client", &self.client)
            .field("batching", &self.batching)
            .finish_non_exhaustive()
    }
}

impl BatchCoordinator {
    /// Build a coordinator. `client` is taken already-constructed (rather
    /// than built internally, as [`AiWorkerPool`](super::AiWorkerPool) builds
    /// its own [`crate::ai::provider::Provider`]) so tests can hand in one
    /// pointed at a mock server via [`BatchClient::with_endpoint`].
    ///
    /// # Errors
    /// [`Error::FailedPrecondition`] if `api_key_command` is empty — caught
    /// here rather than on the first threshold crossing.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: Database,
        queue: AiQueue,
        client: BatchClient,
        api_key_command: impl Into<String>,
        policy: Arc<PolicyEngine>,
        limits: AiLimits,
        privacy: AiPrivacy,
        batching: AiBatching,
        handlers: Vec<Arc<dyn PassHandler>>,
    ) -> Result<Self, Error> {
        let api_key_command = api_key_command.into();
        if api_key_command.trim().is_empty() {
            return Err(Error::failed_precondition(
                "the claude batch path needs `ai.api_key_command`".to_owned(),
            ));
        }
        let handlers = handlers
            .into_iter()
            .map(|h| (h.pass().to_owned(), h))
            .collect();
        Ok(Self {
            db,
            queue,
            client,
            key_source: CredentialSource::Command(api_key_command),
            policy,
            limits,
            privacy,
            batching,
            handlers: Arc::new(handlers),
            pending: Mutex::new(HashMap::new()),
        })
    }

    /// If `ai.batching.enabled` and `pass`'s pending depth has reached
    /// `ai.batching.threshold`, lease up to `ai.batching.max_batch` of its
    /// pending jobs, run policy → assemble → redact over each, and submit
    /// the survivors as one Message Batches request. Returns the new
    /// batch's id, or `None` if nothing was submitted (batching disabled,
    /// depth below threshold, or every leased job was terminated before a
    /// request could be built for it — e.g. every one turned out to be
    /// `redacted_skip`).
    ///
    /// **The cost gate is consulted here, before anything is leased** — the
    /// same "blocks before dispatch" discipline
    /// [`super::AiWorkerPool::dispatch_pending`] applies to the live path.
    /// Without this, a spend cap under `on_cap = "pause"`/`"triage_only"`
    /// would stop the live path from draining the queue while doing nothing
    /// to stop depth from crossing `threshold` — turning the moment the
    /// account is *supposed* to be throttled into the moment this path
    /// submits its single largest batch instead. `on_cap = "drop"` mirrors
    /// [`super::AiWorkerPool::dispatch_pending`]'s `Dropping` arm exactly:
    /// the backlog is leased and terminated rather than submitted.
    ///
    /// # Errors
    /// A mapped storage error, or the HTTP/credential failure from actually
    /// submitting — in which case every job this call leased is returned to
    /// `pending` (via [`AiQueue::fail`], so it still counts as an attempt
    /// and still backs off) rather than left `leased` for the full
    /// [`BATCH_LEASE`] duration on a failure nothing will ever come back to
    /// resolve.
    #[tracing::instrument(skip(self))]
    pub async fn maybe_submit(&self, pass: &str) -> Result<Option<String>, Error> {
        if !self.batching.enabled {
            return Ok(None);
        }
        let depth = self.queue.depth_for_pass(pass).await?;
        if depth < i64::from(self.batching.threshold) {
            return Ok(None);
        }

        let decision = CostGate {
            db: &self.db,
            limits: &self.limits,
        }
        .decide()
        .await?;
        match decision {
            CapDecision::Paused => return Ok(None),
            CapDecision::TriageOnly if pass != "triage" => return Ok(None),
            CapDecision::Dropping => {
                let take = depth.min(i64::from(self.batching.max_batch));
                let leases = self
                    .queue
                    .lease_with_ttl(BATCH_WORKER, take, Some(pass), BATCH_LEASE)
                    .await?;
                for lease in &leases {
                    self.queue
                        .terminate(lease, "daily/monthly AI spend cap exceeded (on_cap = drop)")
                        .await?;
                }
                return Ok(None);
            }
            CapDecision::Open | CapDecision::TriageOnly => {}
        }

        let take = depth.min(i64::from(self.batching.max_batch));
        let leases = self
            .queue
            .lease_with_ttl(BATCH_WORKER, take, Some(pass), BATCH_LEASE)
            .await?;
        if leases.is_empty() {
            return Ok(None);
        }

        let Some(handler) = self.handlers.get(pass).cloned() else {
            for lease in &leases {
                self.queue
                    .terminate(
                        lease,
                        &format!("no PassHandler registered for pass {pass:?}"),
                    )
                    .await?;
            }
            return Err(Error::failed_precondition(format!(
                "no PassHandler registered for pass {pass:?}; batch submission aborted"
            )));
        };

        let mut items = Vec::with_capacity(leases.len());
        let mut in_flight = Vec::with_capacity(leases.len());
        for lease in &leases {
            match self.prepare_item(lease, &handler).await {
                Ok(Some((item, tokens, payload))) => {
                    in_flight.push(InFlightItem {
                        job_id: lease.job_id,
                        message_id: lease.message_id,
                        account_id: lease.account_id,
                        payload,
                        tokens,
                    });
                    items.push(item);
                }
                Ok(None) => {
                    // Terminated inside `prepare_item` (message gone,
                    // policy, or `redacted_skip`) — nothing to submit for
                    // this one.
                }
                Err(e) => {
                    self.queue.fail(lease, &e.to_string()).await?;
                }
            }
        }
        if items.is_empty() {
            return Ok(None);
        }

        let key = match self.resolve_key().await {
            Ok(key) => key,
            Err(e) => return Err(self.fail_in_flight(in_flight, e).await),
        };
        let handle = match self.client.submit(key.expose(), &items).await {
            Ok(handle) => handle,
            Err(e) => return Err(self.fail_in_flight(in_flight, e).await),
        };

        let job_ids: Vec<i64> = in_flight.iter().map(|i| i.job_id).collect();
        // Recorded in memory *before* `mark_batched` runs: the submission
        // already succeeded and will be billed regardless of what happens
        // next, so the in-memory record — the only thing that lets `poll`
        // audit and rehydrate this batch's results — must survive even if
        // the `ai_queue.batch_id` stamp that follows does not.
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(handle.id.clone(), in_flight);
        if let Err(e) = self.queue.mark_batched(&job_ids, &handle.id).await {
            tracing::error!(
                batch_id = %handle.id,
                error = %e,
                "failed to stamp batch_id onto ai_queue rows after a successful submission; \
                 the batch is still trackable via this coordinator's in-memory record"
            );
        }
        tracing::info!(
            batch_id = %handle.id,
            pass,
            items = job_ids.len(),
            "submitted an ai batch"
        );
        Ok(Some(handle.id))
    }

    /// Return every leased-but-not-yet-submitted item to `pending` (via
    /// [`AiQueue::fail`], so the attempt is charged and backed off like any
    /// other transient failure) after `resolve_key`/`submit` failed, and
    /// hand back `error` — the original failure, not a fencing error from
    /// the cleanup — so the caller sees why the submission did not happen.
    async fn fail_in_flight(&self, in_flight: Vec<InFlightItem>, error: Error) -> Error {
        for item in &in_flight {
            let lease = AiLease {
                job_id: item.job_id,
                message_id: item.message_id,
                account_id: item.account_id,
                pass: String::new(),
                attempts: 0,
                lease_expires_at: 0,
                worker: BATCH_WORKER.to_owned(),
            };
            if let Err(fail_err) = self.queue.fail(&lease, &error.to_string()).await {
                tracing::error!(
                    job_id = item.job_id,
                    error = %fail_err,
                    "failed to return a job to pending after a batch submission failure"
                );
            }
        }
        error
    }

    /// Check a submitted batch, and if it has ended, resolve every result
    /// this coordinator has an in-memory record for through
    /// [`super::worker::finish_call`] — the identical audit/rehydrate/
    /// complete-or-fail tail the live dispatch path uses.
    ///
    /// # Errors
    /// A mapped HTTP/credential failure from checking status or fetching
    /// results, or [`Error::FailedPrecondition`] if this coordinator holds
    /// no in-memory record of `batch_id` — see the module docs for when
    /// that happens (a coordinator restart mid-batch) and what recovers it
    /// (the batch lease eventually expiring and the jobs returning to the
    /// live queue).
    #[tracing::instrument(skip(self))]
    pub async fn poll(&self, batch_id: &str) -> Result<BatchPollOutcome, Error> {
        let key = self.resolve_key().await?;
        let status = self.client.status(key.expose(), batch_id).await?;
        if !status.is_ended() {
            return Ok(BatchPollOutcome::StillRunning);
        }

        let tracked = self
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(batch_id);
        if !tracked {
            return Err(Error::failed_precondition(format!(
                "batch {batch_id} ended but this coordinator holds no in-memory record of its \
                 redacted payloads — likely because the process that submitted it has since \
                 restarted; its jobs remain leased under the batch worker and will return to \
                 the live queue once their lease expires"
            )));
        }

        // Fetched *before* the record is removed from `pending`: a
        // transient failure here (a 5xx, a malformed JSONL line) must leave
        // the in-memory record intact for a later `poll` to retry, rather
        // than discard the only copy of an already-billed batch's redacted
        // payloads and token maps on the first hiccup.
        let results = self.client.results(key.expose(), batch_id).await?;

        let items = self
            .pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(batch_id);
        let Some(items) = items else {
            return Err(Error::failed_precondition(format!(
                "batch {batch_id}'s in-memory record disappeared while fetching its results \
                 (a concurrent poll of the same batch?); nothing was processed this call"
            )));
        };
        let by_message: HashMap<i64, &InFlightItem> =
            items.iter().map(|item| (item.message_id, item)).collect();

        let mut summary = DispatchSummary::default();
        for result in results {
            let Ok(message_id) = result.custom_id.parse::<i64>() else {
                tracing::warn!(
                    batch_id,
                    custom_id = %result.custom_id,
                    "claude batch result had a non-numeric custom_id; skipped"
                );
                continue;
            };
            let Some(item) = by_message.get(&message_id) else {
                tracing::warn!(
                    batch_id,
                    message_id,
                    "claude batch result did not match any in-flight job; skipped"
                );
                continue;
            };
            let Some(handler) = self.find_handler_for(item.job_id).await else {
                continue;
            };
            let lease = AiLease {
                job_id: item.job_id,
                message_id: item.message_id,
                account_id: item.account_id,
                pass: handler.pass().to_owned(),
                attempts: 0,
                lease_expires_at: 0,
                worker: BATCH_WORKER.to_owned(),
            };
            let provider_result = match result.outcome {
                BatchOutcome::Succeeded(response) => Ok(response),
                BatchOutcome::Errored(message) => Err(Error::unavailable(message)),
                BatchOutcome::Canceled => Err(Error::unavailable(
                    "claude batch item was canceled".to_owned(),
                )),
                BatchOutcome::Expired => Err(Error::unavailable(
                    "claude batch item expired before it ran".to_owned(),
                )),
            };
            let outcome = finish_call(
                &self.db,
                &self.queue,
                &lease,
                &handler,
                &item.tokens,
                item.payload.clone(),
                BATCH_PRICE_MULTIPLIER,
                std::time::Duration::ZERO,
                provider_result,
            )
            .await;
            summary.record(outcome);
        }
        Ok(BatchPollOutcome::Completed(summary))
    }

    /// Which registered handler owns `job_id`'s pass — read straight off
    /// `ai_queue` rather than trusting a `pass` carried in from the
    /// in-memory record, since that record only ever stores what
    /// `prepare_item` needs to audit and rehydrate, not the pass itself
    /// (every item in one batch already shares it, but reading the row
    /// keeps this lookup correct even if that ever changes).
    async fn find_handler_for(&self, job_id: i64) -> Option<Arc<dyn PassHandler>> {
        let pass: Option<String> = self
            .db
            .read(move |conn| {
                conn.query_row(
                    "SELECT pass FROM ai_queue WHERE job_id = ?1",
                    [job_id],
                    |row| row.get(0),
                )
                .optional()
            })
            .await
            .ok()
            .flatten();
        pass.and_then(|pass| self.handlers.get(&pass).cloned())
    }

    /// Policy → assemble → redact for one leased job, on its way into a
    /// batch submission. Returns `Ok(None)` for a job that was terminated
    /// in the process (policy-forbidden, or nothing left after redaction) —
    /// not an error, since the job has already been dealt with.
    async fn prepare_item(
        &self,
        lease: &AiLease,
        handler: &Arc<dyn PassHandler>,
    ) -> Result<Option<(BatchRequestItem, TokenMap, Vec<u8>)>, Error> {
        let Some((account_name, mailbox_name)) = target_names(&self.db, lease.message_id).await?
        else {
            self.queue
                .terminate(lease, "message no longer exists")
                .await?;
            return Ok(None);
        };
        let decision = self
            .policy
            .resolve(&PolicyTarget::account(account_name).mailbox(mailbox_name));
        if !decision.is_visible() || !decision.permits_network() {
            self.queue
                .terminate(
                    lease,
                    &format!(
                        "ai policy resolved {:?} for this account/folder; no network call is permitted",
                        decision.mode
                    ),
                )
                .await?;
            return Ok(None);
        }

        // A message deleted between lease and here is the same
        // never-succeeds-on-retry case as the `target_names` lookup
        // returning `None` above — terminated, not backed off, matching
        // `worker.rs`'s identical handling of the same condition on the
        // live path.
        let content = match assemble_content(&self.db, lease.message_id, &self.privacy).await {
            Ok(content) => content,
            Err(e) if e.reason() == ErrorReason::NotFound => {
                self.queue.terminate(lease, &e.to_string()).await?;
                return Ok(None);
            }
            Err(e) => return Err(e),
        };
        let request = handler.build_request(&content)?;
        match redact::guard(&request, &self.privacy) {
            GuardedRequest::RedactedSkip => {
                self.queue.terminate(lease, "redacted_skip").await?;
                Ok(None)
            }
            GuardedRequest::Redacted {
                request, tokens, ..
            } => {
                let payload = super::payload_bytes(&request);
                let item = BatchRequestItem {
                    custom_id: lease.message_id.to_string(),
                    params: request,
                };
                Ok(Some((item, tokens, payload)))
            }
        }
    }

    /// Resolved once per submit/poll call, not held, the same discipline
    /// `ClaudeProvider::resolve_key` follows.
    async fn resolve_key(&self) -> Result<Secret, Error> {
        let source = self.key_source.clone();
        tokio::task::spawn_blocking(move || source.resolve(None))
            .await
            .map_err(|e| Error::internal(format!("credential command task failed: {e}")))??
            .ok_or_else(|| {
                Error::unauthenticated("the claude api_key_command produced nothing".to_owned())
            })
    }
}

/// The account/mailbox names a policy resolution needs — the same query
/// `worker.rs`'s `target_names` runs, kept as a separate private copy
/// because it is three lines and pulling it into a shared location would
/// cost a `pub(super)` surface wider than the one call site in each module
/// justifies.
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
