//! The gates every *ad-hoc* provider call passes through, in the one order
//! [`crate::ai::queue`]'s module docs establish: policy first, then the daily
//! cost gate, then the per-account budget, then concurrency and rate pacing —
//! and only then the network.
//!
//! [`crate::ai::queue`] applies the same sequence to everything that goes
//! through the AI worker pool. This module is that sequence for the calls that
//! cannot: a request-scoped call answering a human who is waiting, with no
//! queue row and no lease. Four callers use it —
//! [`crate::rules::classify::ClaudeClassifier`],
//! [`crate::rules::synth::RuleSynthesizer`],
//! [`crate::send::preflight::PreflightGuardian`] and
//! [`crate::outbox::followup::track::FollowupTracker`] — and each one of them
//! going through here rather than re-deriving the sequence is not only
//! deduplication: the ordering is the security property. Resolving policy
//! *after* assembling a request would mean a forbidden folder's content had
//! already been built into a payload; consulting the budget *after* acquiring
//! a permit would hold capacity for a call that is about to be refused.
//!
//! It lives under `ai` rather than under `rules`, where it was first written,
//! because the second and third subsystem to need it made "the rules engine's
//! gate" a name that no longer described what it gates.

use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;

use crate::ai::budget::{BudgetEnforcer, BudgetRequest, BudgetVerdict, WorkClass};
use crate::ai::policy::{PolicyEngine, PolicyTarget};
use crate::ai::queue::{CapDecision, CostGate, RateLimiter};
use crate::config::AiLimits;
use crate::error::Error;
use crate::storage::Database;

/// Resolve policy, the daily cost gate, and the per-account budget for one
/// intended call, returning the model to actually use (which may be a
/// budget-driven downgrade of `model`).
///
/// `mailbox` is the folder the call concerns, when there is one — policy is
/// resolved per account *and* folder, and a synthesis call (which reads no
/// message), or a check on a message that has not been filed anywhere yet,
/// legitimately has none.
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
    let account = crate::rules::repo::account_name(db, account_id)
        .await?
        .ok_or_else(|| Error::not_found(format!("account {account_id}")))?;
    let mut target = PolicyTarget::account(account);
    if let Some(mailbox) = mailbox {
        target = target.mailbox(mailbox.to_owned());
    }
    admit_target(db, policy, limits, &target, account_id, model).await
}

/// [`admit`] for a call that belongs to no account row — today, task 80's
/// autoconfig inference, which runs *before* an account exists.
///
/// The two halves that were keyed by `account_id` get the only answers that
/// are honest for a call with no account:
///
/// - **Policy** resolves against `subject`, the address being configured.
///   Accounts are conventionally named for their address, so an operator who
///   set `accounts.ai.enabled = false` for `ada@example.com` and then asks to
///   reconfigure `ada@example.com` gets their opt-out honored rather than
///   bypassed by the one call that happens to run before the account is
///   loaded. An address that names no account resolves the daemon-wide
///   default — including `ai.enabled = false`, which forbids it outright.
/// - **Budget** charges [`GLOBAL_ACCOUNT_ID`], which is what that sentinel is
///   for ("a call tied to no account"), and is what [`crate::digest`] already
///   does for work that spans every account. Spend still counts; it simply
///   counts against the only budget that can apply.
///
/// The ordering is unchanged, because it is the same code: policy, then the
/// daily cap, then the budget. See the module docs for why that order is the
/// security property.
///
/// # Errors
/// [`Error::FailedPrecondition`] if policy forbids a network call for
/// `subject` or the daily/monthly cap is reached; [`Error::ResourceExhausted`]
/// if a budget hard cap blocks it.
pub async fn admit_unattributed(
    db: &Database,
    policy: &PolicyEngine,
    limits: &AiLimits,
    subject: &str,
    model: &str,
) -> Result<String, Error> {
    let target = PolicyTarget::account(subject.to_owned());
    admit_target(
        db,
        policy,
        limits,
        &target,
        crate::ai::budget::GLOBAL_ACCOUNT_ID,
        model,
    )
    .await
}

/// The shared sequence: policy, daily cap, budget. One definition, because
/// the order is the property being enforced.
async fn admit_target(
    db: &Database,
    policy: &PolicyEngine,
    limits: &AiLimits,
    target: &PolicyTarget,
    account_id: i64,
    model: &str,
) -> Result<String, Error> {
    let decision = policy.resolve(target);
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
            // backlog walks from starving user-facing work, and every caller
            // here *is* the user-facing work — a message arriving and being
            // filed, a human waiting on a backtest, a send waiting on a
            // review.
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
                "ai budget soft cap: downgrading this request-scoped call"
            );
            Ok(downgraded)
        }
        BudgetVerdict::Block { reason, .. } => {
            // The detailed reason names aggregate spend figures, which the
            // scope table deliberately keeps behind `admin` (see
            // `AiService/GetUsage`'s row); it goes to the log, not the caller.
            tracing::info!(
                account_id,
                reason = %reason,
                "ai budget hard cap: refusing a request-scoped call"
            );
            Err(Error::resource_exhausted(
                "an AI spend budget has been reached; no model call can be made until the \
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
