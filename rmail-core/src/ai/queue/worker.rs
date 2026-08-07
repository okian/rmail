//! The live worker pool: leases pending [`super::AiLease`]s and drives each
//! through policy → assemble → redact → provider → audit, bounded by
//! `Semaphore(max_concurrency)`, paced by [`super::RateLimiter`], and gated
//! by [`super::CostGate`].
//!
//! This is the surface tasks 48/49 (the triage and deep passes) are meant
//! to call: implement [`PassHandler`] for a pass, hand it to
//! [`AiWorkerPool::new`], and call [`AiWorkerPool::dispatch_pending`] on
//! whatever schedule the daemon runs its AI loop. Everything about *how
//! fast* and *whether at all* — concurrency, pacing, the cost gate, the
//! policy check, redaction, the audit trail — is this module's job, not
//! the handler's; a handler only decides *what to ask Claude* and *what to
//! do with the answer*.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use rusqlite::OptionalExtension;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::ai::audit::{self, CallOutcome, CallRecord};
use crate::ai::policy::{PolicyEngine, PolicyTarget};
use crate::ai::provider::{ChatRequest, ChatResponse, Provider};
use crate::ai::redact::{self, GuardedRequest, TokenMap};
use crate::config::{AiLimits, AiPrivacy};
use crate::error::{Error, ErrorReason};
use crate::storage::Database;

use super::content::assemble_content;
use super::{AiLease, AiQueue, CapDecision, CostGate, RateLimiter};

/// What a pass (triage, deep, ...) contributes to the pipeline this module
/// otherwise fully owns: turning bounded [`MessageContent`](super::MessageContent)
/// into a request, and persisting whatever the model answered.
///
/// Implementations must be cheap to clone or share behind an [`Arc`] — one
/// instance is registered once and called from every worker task that leases
/// a job of its [`PassHandler::pass`].
#[async_trait]
pub trait PassHandler: Send + Sync + std::fmt::Debug {
    /// The wire value of `ai_queue.pass`/`ai_ledger.pass` this handler
    /// answers to, e.g. `"triage"` or `"deep"`.
    fn pass(&self) -> &str;

    /// Build the request Claude should see, from content this pool already
    /// assembled and bounded (see [`super::assemble_content`]). The system
    /// prompt, output schema, and model choice are this handler's call —
    /// this pool contributes only the redaction, dispatch, and audit that
    /// wrap around whatever request comes back.
    ///
    /// Async so a handler may consult its own durable state while building
    /// the request — e.g. [`crate::ai::deep::DeepPassHandler`] reads a
    /// prior thread summary off `ai_summaries` before it can render the user
    /// turn. A handler with nothing to look up, like triage's, simply
    /// returns immediately; nothing about this signature requires a handler
    /// to actually await anything.
    ///
    /// # Errors
    /// Any handler-specific failure building the request. Classified by
    /// [`crate::error::ErrorReason`] the same way
    /// [`super::content::assemble_content`]'s own failures are, two steps
    /// earlier in [`AiWorkerPool::process_one`]: [`ErrorReason::NotFound`]
    /// (the message vanished in the same narrow window) is terminated via
    /// [`AiQueue::terminate`], since a later attempt cannot succeed against
    /// content that no longer exists; anything else is backed off via
    /// [`AiQueue::fail`] and retried. A handler with a purely structural
    /// `build_request` (triage's, which never actually errors) is
    /// unaffected either way; a handler whose `build_request` also does a
    /// durable-state lookup (deep's) gets a transient storage hiccup
    /// retried rather than the job being lost to it permanently.
    async fn build_request(&self, content: &super::MessageContent) -> Result<ChatRequest, Error>;

    /// Persist whatever this pass produces, once the provider call
    /// succeeded, its response has been through [`crate::ai::redact::rehydrate`],
    /// and the call itself has already been recorded in the audit ledger.
    /// `ledger_entry_id` is what a persisted artifact should store as its
    /// own `ledger_entry_id` foreign key, per `audit.rs`'s module docs on
    /// what links an AI artifact back to its ledger entry.
    ///
    /// # Errors
    /// Any handler-specific persistence failure. Treated as retryable —
    /// the queue calls [`AiQueue::fail`] on an `on_success` error, since the
    /// provider call itself already succeeded and only the write failed,
    /// which a later attempt (a fresh provider call, since nothing here
    /// caches the response) may not repeat.
    async fn on_success(
        &self,
        lease: &AiLease,
        text: &str,
        ledger_entry_id: i64,
    ) -> Result<(), Error>;
}

/// What one [`AiWorkerPool::dispatch_pending`] cycle did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DispatchSummary {
    /// Jobs that completed successfully.
    pub completed: u64,
    /// Jobs that failed and were backed off for retry.
    pub retried: u64,
    /// Jobs that failed and were quarantined to `dead`.
    pub dead: u64,
    /// Jobs terminated as unrecoverable (policy, `redacted_skip`, refusal, or
    /// a handler's `build_request` failing with [`ErrorReason::NotFound`] —
    /// see that method's own docs for why only that reason terminates and
    /// every other `build_request` failure is retried instead).
    pub terminated: u64,
    /// Jobs leased and immediately terminated because `on_cap = "drop"` and
    /// today's/this month's spend cap was reached.
    pub dropped: u64,
    /// The cost gate held this cycle back entirely (`on_cap = "pause"`).
    pub paused: bool,
}

/// The live worker pool.
///
/// Cheap to clone: every clone shares the same database handle, provider,
/// policy engine, semaphore, and rate limiter — cloning is how one instance
/// is handed into `N` concurrently-spawned per-job tasks.
#[derive(Debug, Clone)]
pub struct AiWorkerPool {
    db: Database,
    queue: AiQueue,
    provider: Arc<dyn Provider>,
    policy: Arc<PolicyEngine>,
    limits: AiLimits,
    privacy: AiPrivacy,
    semaphore: Arc<Semaphore>,
    rate_limiter: Arc<RateLimiter>,
    handlers: Arc<HashMap<String, Arc<dyn PassHandler>>>,
    worker: Arc<str>,
}

impl AiWorkerPool {
    /// Build a pool. `worker` is this pool's identity in `ai_queue.leased_by`
    /// — give each running daemon (or, in tests, each pool instance) a
    /// distinct one, the same discipline `index::queue`'s callers follow.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: Database,
        queue: AiQueue,
        provider: Arc<dyn Provider>,
        policy: Arc<PolicyEngine>,
        limits: AiLimits,
        privacy: AiPrivacy,
        handlers: Vec<Arc<dyn PassHandler>>,
        worker: impl Into<Arc<str>>,
    ) -> Self {
        let max_concurrency = limits.max_concurrency.max(1);
        let requests_per_minute = limits.requests_per_minute;
        let handlers = handlers
            .into_iter()
            .map(|h| (h.pass().to_owned(), h))
            .collect();
        Self {
            db,
            queue,
            provider,
            policy,
            limits,
            privacy,
            semaphore: Arc::new(Semaphore::new(max_concurrency as usize)),
            rate_limiter: Arc::new(RateLimiter::new(requests_per_minute)),
            handlers: Arc::new(handlers),
            worker: worker.into(),
        }
    }

    /// Reap expired leases, consult the cost gate, lease up to `limit`
    /// eligible jobs accordingly, and drive each through the full pipeline —
    /// bounded by `Semaphore(max_concurrency)` and paced by the RPM limiter.
    ///
    /// # Errors
    /// A mapped storage error from reaping or leasing. Per-job pipeline
    /// failures are captured in the returned [`DispatchSummary`], not
    /// propagated — one message's failure must not stop the rest of the
    /// cycle from running.
    #[tracing::instrument(skip(self, cancel))]
    pub async fn dispatch_pending(
        &self,
        limit: i64,
        cancel: &CancellationToken,
    ) -> Result<DispatchSummary, Error> {
        self.queue.reap_expired().await?;
        let decision = CostGate {
            db: &self.db,
            limits: &self.limits,
        }
        .decide()
        .await?;

        match decision {
            CapDecision::Paused => Ok(DispatchSummary {
                paused: true,
                ..DispatchSummary::default()
            }),
            CapDecision::Open => {
                let leases = self.queue.lease(&self.worker, limit, None).await?;
                Ok(self.run_leases(leases, cancel).await)
            }
            CapDecision::TriageOnly => {
                let leases = self
                    .queue
                    .lease(&self.worker, limit, Some("triage"))
                    .await?;
                Ok(self.run_leases(leases, cancel).await)
            }
            CapDecision::Dropping => {
                let leases = self.queue.lease(&self.worker, limit, None).await?;
                let mut summary = DispatchSummary::default();
                for lease in &leases {
                    if self
                        .queue
                        .terminate(lease, "daily/monthly AI spend cap exceeded (on_cap = drop)")
                        .await?
                    {
                        summary.dropped += 1;
                    }
                }
                Ok(summary)
            }
        }
    }

    /// Run every lease concurrently, bounded by the semaphore, and fold the
    /// per-job outcomes into one summary.
    async fn run_leases(
        &self,
        leases: Vec<AiLease>,
        cancel: &CancellationToken,
    ) -> DispatchSummary {
        let mut set = tokio::task::JoinSet::new();
        for lease in leases {
            let pool = self.clone();
            let cancel = cancel.clone();
            let span = tracing::info_span!(
                "ai_job",
                job_id = lease.job_id,
                message_id = lease.message_id,
                pass = %lease.pass,
            );
            // `tokio::spawn` (which `JoinSet::spawn` wraps) does not carry the
            // calling span into the new task on its own — everything inside
            // `process_one` (redaction's span, the provider call's span, the
            // ledger write's span) would otherwise be orphaned from
            // `dispatch_pending`'s span and from each other, the same fix
            // `provider.rs`'s own `spawn_sse_reader` applies for the same
            // reason.
            set.spawn(async move { pool.process_one(lease, &cancel).await }.instrument(span));
        }
        let mut summary = DispatchSummary::default();
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok(outcome) => summary.record(outcome),
                Err(join_error) => {
                    tracing::error!(error = %join_error, "an ai worker task panicked or was aborted");
                }
            }
        }
        summary
    }

    /// Process one leased job through the whole pipeline: policy → assemble
    /// → redact → (semaphore + RPM pace) → provider → audit.
    async fn process_one(&self, lease: AiLease, cancel: &CancellationToken) -> Outcome {
        // A cancellation that arrived between `lease` claiming this row and
        // this task actually starting (a graceful-shutdown race, most often)
        // must not be treated as a failed attempt: `lease` already
        // incremented `attempts`, and a job that never got as far as a
        // policy check must not lose a retry to a shutdown it had nothing to
        // do with. `release` undoes exactly that increment.
        if cancel.is_cancelled() {
            return self.release_outcome(&lease).await;
        }

        let Some(handler) = self.handlers.get(&lease.pass).cloned() else {
            return self
                .terminate_outcome(
                    &lease,
                    format!("no PassHandler registered for pass {:?}", lease.pass),
                )
                .await;
        };

        // 1. Policy — before anything else touches this message's content.
        let (account_name, mailbox_name) = match target_names(&self.db, lease.message_id).await {
            Ok(Some(names)) => names,
            Ok(None) => {
                return self
                    .terminate_outcome(&lease, "message no longer exists".to_owned())
                    .await
            }
            Err(e) => return self.fail_outcome(&lease, e.to_string()).await,
        };
        let decision = self
            .policy
            .resolve(&PolicyTarget::account(account_name).mailbox(mailbox_name));
        if !decision.is_visible() || !decision.permits_network() {
            return self
                .terminate_outcome(
                    &lease,
                    format!(
                        "ai policy resolved {:?} for this account/folder; no network call is permitted",
                        decision.mode
                    ),
                )
                .await;
        }

        // 2. Assemble. A message deleted between lease and here
        // (`Error::NotFound`) is exactly the never-succeeds-on-retry case
        // the module docs describe for `JobState::Error` — it is
        // terminated, not backed off, the same as the `target_names`
        // lookup returning `None` two steps above for the identical
        // condition. Anything else (a storage hiccup) is retried.
        let content = match assemble_content(&self.db, lease.message_id, &self.privacy).await {
            Ok(content) => content,
            Err(e) if e.reason() == ErrorReason::NotFound => {
                return self.terminate_outcome(&lease, e.to_string()).await
            }
            Err(e) => return self.fail_outcome(&lease, e.to_string()).await,
        };

        // 3. The handler turns bounded content into a request. Classified
        // the same way step 2 just was, immediately above — see
        // `PassHandler::build_request`'s own docs for why this method can
        // now fail transiently (a durable-state lookup, not just a
        // structural build) and why that must not be lumped in with a
        // definite "this content can never produce a request."
        let request = match handler.build_request(&content).await {
            Ok(request) => request,
            Err(e) if e.reason() == ErrorReason::NotFound => {
                return self.terminate_outcome(&lease, e.to_string()).await
            }
            Err(e) => return self.fail_outcome(&lease, e.to_string()).await,
        };

        // 4. Redact — mandatory, unconditional.
        let (redacted_request, tokens) = match redact::guard(&request, &self.privacy) {
            GuardedRequest::RedactedSkip => {
                return self
                    .terminate_outcome(&lease, "redacted_skip".to_owned())
                    .await
            }
            GuardedRequest::Redacted {
                request, tokens, ..
            } => (request, tokens),
        };
        let payload = super::payload_bytes(&redacted_request);

        // Pace and bound concurrency *before* the provider call — never
        // after. Both waits are raced against `cancel`: a shutdown must not
        // leave a task blocked indefinitely on a full semaphore or a slow
        // RPM refill, and a job cancelled here — like one cancelled above —
        // gets its attempt back via `release` rather than being charged for
        // work it never got to attempt.
        let Ok(_permit) = self.semaphore.clone().acquire_owned().await else {
            // The semaphore is never explicitly closed by this pool; this
            // is unreachable in practice and left in the queue for a later
            // cycle rather than guessed at.
            return self.release_outcome(&lease).await;
        };
        tokio::select! {
            () = cancel.cancelled() => return self.release_outcome(&lease).await,
            () = self.rate_limiter.acquire() => {}
        }

        // 5. Provider, then audit — with the redacted payload, at the live
        // (unmultiplied) price.
        let start = Instant::now();
        let result = self.provider.complete(&redacted_request, cancel).await;
        let latency = start.elapsed();
        finish_call(
            &self.db,
            &self.queue,
            &lease,
            &handler,
            &tokens,
            payload,
            1.0,
            latency,
            result,
        )
        .await
    }

    async fn terminate_outcome(&self, lease: &AiLease, reason: String) -> Outcome {
        match self.queue.terminate(lease, &reason).await {
            Ok(_) => Outcome::Terminated,
            Err(e) => {
                tracing::error!(job_id = lease.job_id, error = %e, "failed to terminate ai job");
                Outcome::Skipped
            }
        }
    }

    async fn fail_outcome(&self, lease: &AiLease, reason: String) -> Outcome {
        match self.queue.fail(lease, &reason).await {
            Ok(Some(super::Failure::Quarantined { .. })) => Outcome::Dead,
            Ok(Some(super::Failure::Retrying { .. })) => Outcome::Retried,
            Ok(None) | Err(_) => Outcome::Skipped,
        }
    }

    async fn release_outcome(&self, lease: &AiLease) -> Outcome {
        if let Err(e) = self.queue.release(lease).await {
            tracing::error!(job_id = lease.job_id, error = %e, "failed to release a cancelled ai job back to pending");
        }
        Outcome::Skipped
    }
}

/// The account/mailbox names a policy resolution needs, joined from the
/// message row so a worker never needs a second round trip.
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

/// Per-job outcome, folded into a [`DispatchSummary`] by [`AiWorkerPool::run_leases`].
///
/// `pub(super)`, not private: [`finish_call`] is the shared tail both this
/// module's live dispatch and `super::batch`'s poll path call, so its return
/// type — and [`DispatchSummary::record`] below, which consumes it — must be
/// visible to that sibling module too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Outcome {
    Completed,
    Retried,
    Dead,
    Terminated,
    /// Cancelled before dispatch, or a fencing/logging failure already
    /// reported — nothing left for the summary to count.
    Skipped,
}

impl DispatchSummary {
    pub(super) fn record(&mut self, outcome: Outcome) {
        match outcome {
            Outcome::Completed => self.completed += 1,
            Outcome::Retried => self.retried += 1,
            Outcome::Dead => self.dead += 1,
            Outcome::Terminated => self.terminated += 1,
            Outcome::Skipped => {}
        }
    }
}

/// Whether `error` is definitely never going to succeed on retry — a model
/// refusal (`FailedPrecondition`, per `provider.rs`'s own docs) or a request
/// the provider rejected as malformed (`InvalidArgument`). Everything else —
/// `Unavailable` (429/5xx after the provider's own internal retries),
/// `DeadlineExceeded`, `Unauthenticated` (a credential command that may
/// recover) — goes through the ordinary backoff-then-`dead` path instead,
/// matching the acceptance criterion's "provider 429/5xx → backoff then
/// dead" and erring toward giving a possibly-transient fault the chance a
/// retry offers.
fn is_terminal(error: &Error) -> bool {
    matches!(
        error.reason(),
        ErrorReason::FailedPrecondition | ErrorReason::InvalidArgument
    )
}

/// Audit the call, then rehydrate and hand a success to the handler or fail
/// the job — the shared tail of the pipeline both the live dispatch path
/// ([`AiWorkerPool::process_one`]) and the batch poll path
/// ([`super::batch::BatchCoordinator::poll`]) run once a provider outcome is
/// known. `pub(super)` so `super::batch` — a sibling module, not a
/// descendant of this one — can call it too; see that module's docs for why
/// a batch result's per-item "latency" is `Duration::ZERO` when it reaches
/// here. `price_multiplier` is `1.0` for the live path and `0.5` for a
/// batch result — the Message Batches API's discount — passed straight
/// through to [`audit::record_call_priced`], which is what actually prices
/// `ai_ledger.cost_usd`/`ai_usage.cost_usd`.
#[allow(clippy::too_many_arguments)]
pub(super) async fn finish_call(
    db: &Database,
    queue: &AiQueue,
    lease: &AiLease,
    handler: &Arc<dyn PassHandler>,
    tokens: &TokenMap,
    payload: Vec<u8>,
    price_multiplier: f64,
    latency: Duration,
    result: Result<ChatResponse, Error>,
) -> Outcome {
    let redaction_level = if tokens.is_empty() {
        "none"
    } else {
        "redacted"
    }
    .to_owned();
    match result {
        Ok(response) => {
            let record = CallRecord {
                account_id: Some(lease.account_id),
                message_id: Some(lease.message_id),
                request_id: Some(response.id.clone()),
                model: response.model.clone(),
                pass: Some(lease.pass.clone()),
                usage: response.usage,
                redaction_level,
                latency,
                payload: &payload,
                outcome: CallOutcome::Ok,
            };
            match audit::record_call_priced(db, record, price_multiplier).await {
                Ok(ledger_entry_id) => {
                    let text = redact::rehydrate(&response.text, tokens);
                    if let Err(e) = handler.on_success(lease, &text, ledger_entry_id).await {
                        tracing::error!(job_id = lease.job_id, error = %e, "ai pass artifact write failed");
                        return fail_after_success(queue, lease, &e.to_string()).await;
                    }
                    match queue.complete(lease, Some(ledger_entry_id)).await {
                        Ok(true) => Outcome::Completed,
                        Ok(false) | Err(_) => Outcome::Skipped,
                    }
                }
                Err(e) => {
                    tracing::error!(job_id = lease.job_id, error = %e, "ai audit ledger write failed");
                    fail_after_success(queue, lease, &format!("audit ledger write failed: {e}"))
                        .await
                }
            }
        }
        Err(e) => {
            let record = CallRecord {
                account_id: Some(lease.account_id),
                message_id: Some(lease.message_id),
                request_id: None,
                // The model that was *asked for*, since a failed call never
                // gets to tell us which model actually ran it — the same
                // gap every provider error leaves.
                model: String::new(),
                pass: Some(lease.pass.clone()),
                usage: crate::ai::provider::Usage::default(),
                redaction_level,
                latency,
                payload: &payload,
                outcome: CallOutcome::Error(e.to_string()),
            };
            if let Err(audit_err) = audit::record_call_priced(db, record, price_multiplier).await {
                tracing::error!(job_id = lease.job_id, error = %audit_err, "ai audit ledger write failed");
            }
            if is_terminal(&e) {
                match queue.terminate(lease, &e.to_string()).await {
                    Ok(_) => Outcome::Terminated,
                    Err(_) => Outcome::Skipped,
                }
            } else {
                fail_after_success(queue, lease, &e.to_string()).await
            }
        }
    }
}

/// `queue.fail`, mapped to the [`Outcome`] variant a summary counts.
async fn fail_after_success(queue: &AiQueue, lease: &AiLease, reason: &str) -> Outcome {
    match queue.fail(lease, reason).await {
        Ok(Some(super::Failure::Quarantined { .. })) => Outcome::Dead,
        Ok(Some(super::Failure::Retrying { .. })) => Outcome::Retried,
        Ok(None) | Err(_) => Outcome::Skipped,
    }
}
