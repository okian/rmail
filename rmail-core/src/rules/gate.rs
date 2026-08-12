//! The gates every rules-engine provider call passes through, in the one
//! order `crate::ai::queue`'s module docs establish: policy first, then the
//! daily cost gate, then the per-account budget, then concurrency and rate
//! pacing — and only then the network.
//!
//! Both callers that can reach a provider from this subsystem
//! ([`super::classify::ClaudeClassifier`] and [`super::synth::RuleSynthesizer`])
//! go through here rather than each re-deriving the sequence. That is not
//! only deduplication: the ordering is the security property. Resolving
//! policy *after* assembling a request would mean a forbidden folder's
//! content had already been built into a payload; consulting the budget
//! *after* acquiring a permit would hold capacity for a call that is about to
//! be refused.

use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::ai::budget::{BudgetEnforcer, BudgetRequest, BudgetVerdict, WorkClass};
use crate::ai::policy::{PolicyEngine, PolicyTarget};
use crate::ai::queue::{CapDecision, CostGate, RateLimiter};
use crate::config::AiLimits;
use crate::error::Error;
use crate::rules::repo;
use crate::storage::Database;

/// Resolve policy, the daily cost gate, and the per-account budget for one
/// intended call, returning the model to actually use (which may be a
/// budget-driven downgrade of `model`).
///
/// `mailbox` is the folder the call concerns, when there is one — policy is
/// resolved per account *and* folder, and a synthesis call (which reads no
/// message) legitimately has none.
///
/// # Errors
/// [`Error::NotFound`] if `account_id` names no account;
/// [`Error::FailedPrecondition`] if policy forbids a network call for this
/// account/folder or the daily/monthly cap is reached;
/// [`Error::ResourceExhausted`] if a budget hard cap blocks it.
pub async fn admit(
    db: &Database,
    policy: &PolicyEngine,
    limits: &AiLimits,
    account_id: i64,
    mailbox: Option<&str>,
    model: &str,
) -> Result<String, Error> {
    let account = repo::account_name(db, account_id)
        .await?
        .ok_or_else(|| Error::not_found(format!("account {account_id}")))?;
    let mut target = PolicyTarget::account(account);
    if let Some(mailbox) = mailbox {
        target = target.mailbox(mailbox.to_owned());
    }
    let decision = policy.resolve(&target);
    if !decision.is_visible() || !decision.permits_network() {
        return Err(Error::failed_precondition(format!(
            "ai policy resolved {:?} for this account/folder; no network call is permitted",
            decision.mode
        )));
    }

    let cap = CostGate { db, limits }.decide().await?;
    if !matches!(cap, CapDecision::Open) {
        return Err(Error::failed_precondition(
            "the AI daily/monthly spend cap has been reached; rules cannot call the model \
             until it resets or an operator raises the cap"
                .to_owned(),
        ));
    }

    let verdict = BudgetEnforcer { db, limits }
        .evaluate(&BudgetRequest {
            account_id,
            model,
            // Interactive, never Bulk: the `bulk` sub-budget exists to keep
            // backlog walks from starving user-facing work, and rule
            // evaluation *is* the user-facing work — a message arriving and
            // being filed, or a human waiting on a backtest.
            work_class: WorkClass::Interactive,
            now: chrono::Utc::now().timestamp(),
        })
        .await?;
    match verdict {
        BudgetVerdict::Allow => Ok(model.to_owned()),
        BudgetVerdict::Downgrade {
            model: downgraded,
            reason,
        } => {
            tracing::info!(
                account_id,
                from = model,
                to = %downgraded,
                reason = %reason,
                "ai budget soft cap: downgrading this rules call"
            );
            Ok(downgraded)
        }
        BudgetVerdict::Block { reason, .. } => {
            // The detailed reason names aggregate spend figures, which the
            // scope table deliberately keeps behind `admin` (see
            // `AiService/GetUsage`'s row); it goes to the log, not the caller.
            tracing::info!(account_id, reason = %reason, "ai budget hard cap: refusing a rules call");
            Err(Error::resource_exhausted(
                "an AI spend budget has been reached; rules cannot call the model until the \
                 window resets or an operator raises the budget"
                    .to_owned(),
            ))
        }
    }
}

/// Acquire the shared concurrency permit and an RPM token, racing both
/// against `cancel`.
///
/// `semaphore`/`rate_limiter` are the running `crate::ai::AiWorkerPool`'s own
/// handles — minting fresh ones would double the ceiling `ai.limits`
/// configures, the same reasoning `rmaild::AiApi` and `rmaild::HookApi`
/// already document for their own shared budgets.
///
/// # Errors
/// [`Error::DeadlineExceeded`] if `cancel` fires while waiting;
/// [`Error::Unavailable`] if the semaphore has been closed.
pub async fn acquire_capacity(
    semaphore: &Arc<Semaphore>,
    rate_limiter: &RateLimiter,
    cancel: &CancellationToken,
) -> Result<OwnedSemaphorePermit, Error> {
    let permit = tokio::select! {
        () = cancel.cancelled() => {
            return Err(Error::deadline_exceeded(
                "cancelled while waiting for AI concurrency capacity".to_owned(),
            ));
        }
        permit = Arc::clone(semaphore).acquire_owned() => permit,
    };
    // The pool this handle comes from never closes its semaphore, so this arm
    // is unreachable in practice; surfaced rather than assumed away.
    let permit = permit
        .map_err(|_| Error::unavailable("the AI concurrency budget is unavailable".to_owned()))?;
    tokio::select! {
        () = cancel.cancelled() => Err(Error::deadline_exceeded(
            "cancelled while waiting for AI rate-limit capacity".to_owned(),
        )),
        () = rate_limiter.acquire() => Ok(permit),
    }
}
