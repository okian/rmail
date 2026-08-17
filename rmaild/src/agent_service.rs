//! The `AgentService` gRPC implementation: task 69's autonomous inbox agent
//! (prd.md feature 47).
//!
//! Thin, deliberately. [`rmail_core::agent`] owns the loop, the bounds, the
//! closed action vocabulary, the prompt fencing, the injection gate, the
//! dry-run guarantee and the action log. What lives here is the translation
//! between its types and the wire, plus the three decisions that really are a
//! transport concern:
//!
//! - **Zero means "default".** proto3 has no field presence for scalars, so a
//!   client that leaves `mailbox` or `limit` alone sends the zero value.
//!   Treating that as a literal would make `agent.mailbox` unreachable and
//!   `limit = 0` mean "no runs". Zero selects the configured default, and the
//!   response echoes the mailbox actually walked.
//! - **The agent is `None` when AI is off**, not an engine over a
//!   `NullProvider`. `RunInboxAgent` then declines with `FAILED_PRECONDITION`
//!   before opening a run row, rather than opening one and failing at the
//!   provider on the first message. The RPCs stay registered either way —
//!   reflection and the fail-closed scope table must see every RPC regardless
//!   of runtime config, the convention `AiService`/`HookService` established.
//! - **The policy is bounded here.** It is caller-authored text that reaches
//!   the model *outside* the untrusted-content fence, which is what makes it a
//!   policy rather than another piece of evidence — so its length is the
//!   caller's own input to validate, and an over-long one is
//!   `INVALID_ARGUMENT` rather than a silently truncated instruction the
//!   operator never sees.
//!
//! # What this file deliberately does not do
//!
//! It does not decide whether the caller may mutate. That is two other
//! things' job and duplicating either here would create a second source of
//! truth to drift: `crate::auth::methods` gates `RunInboxAgent` behind
//! `mail.read` + `mail.write` + `ai.invoke` + `automation`, and
//! [`rmail_core::agent::InboxAgent::run`] refuses a mutating run when
//! `agent.allow_mutations` is off. Both answers arrive here as a `Status` and
//! are passed through.
#![allow(clippy::result_large_err)] // see mail_service.rs's note on `Result<_, Status>`

use rmail_core::agent::{
    ActionKind, ActionRecord, InboxAgent, LoggedAction, LoggedRun, Outcome, RunReport, RunRequest,
    StopReason,
};
use rmail_core::{Database, Error};
use rmail_proto::v1::agent_service_server::AgentService;
use rmail_proto::v1::{
    AgentAction as ProtoAction, AgentActionEntry, AgentActionOutcome as ProtoOutcome, AgentRun,
    AgentStopReason as ProtoStopReason, GetAgentRunLogRequest, GetAgentRunLogResponse,
    RunInboxAgentRequest, RunInboxAgentResponse,
};
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};

/// The longest policy accepted.
///
/// A policy is the owner's standing instruction and reaches the model in
/// instruction position. Bounded because it is prepended to every message's
/// prompt in the loop — an enormous one multiplies by `agent.max_iterations`
/// and is paid for on every run — and because "instruction position" is
/// exactly where a caller-supplied wall of text is worth refusing rather than
/// silently cutting in half.
pub const MAX_POLICY_CHARS: usize = 4_000;

/// Ceiling on `GetAgentRunLog`'s page.
///
/// Each run carries its whole action log inline, and an action entry carries a
/// subject and a sender of no fixed length. `agent.max_iterations` bounds one
/// run to `MAX_ITERATIONS_CEILING` entries, so a page is bounded by
/// `MAX_LOG_RUNS × 200` entries — 50 keeps that comfortably inside tonic's
/// default 4 MiB decode limit, where 200 would not.
pub const MAX_LOG_RUNS: u32 = 50;

/// The `AgentService` handler.
#[derive(Debug, Clone)]
pub struct AgentApi {
    /// Read directly for the run log, so that history survives the AI
    /// subsystem being switched off — see [`AgentApi::get_agent_run_log`].
    db: Database,
    /// The engine, `None` on a daemon whose AI subsystem is off.
    agent: Option<InboxAgent>,
    /// How many runs `GetAgentRunLog` returns when the request asks for none.
    log_limit: u32,
    /// Cancelled when the daemon shuts down. A child of it reaches the model
    /// call and the loop, so shutdown stops an in-flight run rather than
    /// waiting behind it — and the run reports `CANCELLED` with everything it
    /// had already done, which for a mutating loop is the difference between
    /// an auditable partial run and a silent one.
    shutdown: CancellationToken,
}

impl AgentApi {
    /// Build a handler with no engine: both RPCs answer, `RunInboxAgent`
    /// declines with `FAILED_PRECONDITION` and the log still reads.
    #[must_use]
    pub fn new(db: Database, log_limit: u32, shutdown: CancellationToken) -> Self {
        Self {
            db,
            agent: None,
            log_limit,
            shutdown,
        }
    }

    /// Wire the engine in.
    #[must_use]
    pub fn with_agent(mut self, agent: InboxAgent) -> Self {
        self.agent = Some(agent);
        self
    }

    /// The engine, or the `FAILED_PRECONDITION` a daemon without one owes.
    fn engine(&self) -> Result<&InboxAgent, Status> {
        self.agent.as_ref().ok_or_else(|| {
            Status::from(Error::failed_precondition(
                "AI is disabled on this daemon (ai.enabled = false, or no provider could be \
                 built), so the inbox agent has nothing to decide with"
                    .to_owned(),
            ))
        })
    }
}

#[tonic::async_trait]
impl AgentService for AgentApi {
    #[tracing::instrument(skip(self, request), fields(account_id, mutate, stop_reason))]
    async fn run_inbox_agent(
        &self,
        request: Request<RunInboxAgentRequest>,
    ) -> Result<Response<RunInboxAgentResponse>, Status> {
        let cancel = self.shutdown.child_token();
        let request = request.into_inner();
        let account_id = validate_account(request.account_id)?;
        let policy = request.policy;
        if policy.chars().count() > MAX_POLICY_CHARS {
            return Err(Status::from(Error::invalid_argument(format!(
                "policy is longer than {MAX_POLICY_CHARS} characters; it is prepended to every \
                 message's prompt in the loop, so it must be an instruction rather than a \
                 document"
            ))));
        }

        let span = tracing::Span::current();
        span.record("account_id", account_id);
        span.record("mutate", request.mutate);

        let report = self
            .engine()?
            .run(
                &RunRequest {
                    account_id,
                    mailbox: request.mailbox,
                    policy,
                    mutate: request.mutate,
                },
                &cancel,
            )
            .await
            .map_err(Status::from)?;

        span.record("stop_reason", report.stop_reason.as_str());
        Ok(Response::new(report_to_proto(report)))
    }

    #[tracing::instrument(skip(self, request), fields(account_id, runs))]
    async fn get_agent_run_log(
        &self,
        request: Request<GetAgentRunLogRequest>,
    ) -> Result<Response<GetAgentRunLogResponse>, Status> {
        let request = request.into_inner();
        let account_id = validate_account(request.account_id)?;
        // Zero means the configured default; anything above the ceiling is
        // clamped rather than refused, because a client asking for too many
        // runs wants runs, not an error.
        let limit = if request.limit == 0 {
            self.log_limit
        } else {
            request.limit
        }
        .min(MAX_LOG_RUNS);

        // Read straight off the database rather than through the engine: an
        // operator who has since set `ai.enabled = false` must still be able
        // to see what their agent did while it was on, and that is exactly the
        // moment they are most likely to look. Routing this through
        // `InboxAgent` would make the history disappear with the engine.
        let runs = rmail_core::agent::store::recent_runs(&self.db, account_id, i64::from(limit))
            .await
            .map_err(Status::from)?;

        let span = tracing::Span::current();
        span.record("account_id", account_id);
        span.record("runs", runs.len());
        Ok(Response::new(GetAgentRunLogResponse {
            runs: runs.into_iter().map(run_to_proto).collect(),
        }))
    }
}

// ---------------------------------------------------------------------------
// Conversions
// ---------------------------------------------------------------------------

fn report_to_proto(report: RunReport) -> RunInboxAgentResponse {
    RunInboxAgentResponse {
        run_id: report.run_id.unwrap_or_default(),
        mutated: report.mutated,
        mailbox: report.mailbox,
        stop_reason: stop_reason_to_proto(report.stop_reason) as i32,
        iterations: report.iterations,
        model_calls: report.model_calls,
        actions_applied: report.actions_applied,
        actions: report.actions.into_iter().map(record_to_proto).collect(),
    }
}

fn run_to_proto(run: LoggedRun) -> AgentRun {
    AgentRun {
        id: run.id,
        account_id: run.account_id,
        mailbox: run.mailbox,
        policy: run.policy,
        started_at: run.started_at,
        finished_at: run.finished_at.unwrap_or_default(),
        stop_reason: stop_reason_to_proto(run.stop_reason) as i32,
        iterations: run.iterations,
        model_calls: run.model_calls,
        actions_applied: run.actions_applied,
        actions: run.actions.into_iter().map(logged_to_proto).collect(),
    }
}

/// A live run's record. `id`/`decided_at` are zero — a dry run has no row, and
/// a mutating run's row id is not threaded back through the report because the
/// caller already has the run id and the actions are in order.
fn record_to_proto(record: ActionRecord) -> AgentActionEntry {
    AgentActionEntry {
        id: 0,
        message_id: record.message_id,
        rfc_message_id: record.rfc_message_id,
        subject: record.subject,
        sender: record.sender,
        action: action_to_proto(record.action) as i32,
        argument: record.argument,
        reason: record.reason,
        outcome: outcome_to_proto(record.outcome) as i32,
        detail: record.detail,
        decided_at: 0,
    }
}

fn logged_to_proto(action: LoggedAction) -> AgentActionEntry {
    AgentActionEntry {
        id: action.id,
        // Zero, not absent: proto3 has no null, and the message being gone is
        // exactly what a zero here means (an archive drops the local row).
        // The frozen `rfc_message_id`/`subject`/`sender` are what identify it.
        message_id: action.message_id.unwrap_or_default(),
        rfc_message_id: action.rfc_message_id,
        subject: action.subject,
        sender: action.sender,
        action: action_to_proto(action.action) as i32,
        argument: action.argument,
        reason: action.reason,
        outcome: outcome_to_proto(action.outcome) as i32,
        detail: action.detail,
        decided_at: action.decided_at,
    }
}

fn action_to_proto(action: ActionKind) -> ProtoAction {
    match action {
        ActionKind::Archive => ProtoAction::Archive,
        ActionKind::Label => ProtoAction::Label,
        ActionKind::Snooze => ProtoAction::Snooze,
        ActionKind::DraftReply => ProtoAction::DraftReply,
        ActionKind::Escalate => ProtoAction::Escalate,
        ActionKind::None => ProtoAction::None,
    }
}

fn outcome_to_proto(outcome: Outcome) -> ProtoOutcome {
    match outcome {
        Outcome::Attempted => ProtoOutcome::Attempted,
        Outcome::Applied => ProtoOutcome::Applied,
        Outcome::Failed => ProtoOutcome::Failed,
        Outcome::Withheld => ProtoOutcome::Withheld,
        Outcome::Refused => ProtoOutcome::Refused,
        Outcome::Planned => ProtoOutcome::Planned,
    }
}

fn stop_reason_to_proto(reason: StopReason) -> ProtoStopReason {
    match reason {
        StopReason::Running => ProtoStopReason::Running,
        StopReason::Completed => ProtoStopReason::Completed,
        StopReason::IterationCap => ProtoStopReason::IterationCap,
        StopReason::ActionCap => ProtoStopReason::ActionCap,
        StopReason::Deadline => ProtoStopReason::Deadline,
        StopReason::Cancelled => ProtoStopReason::Cancelled,
        StopReason::Error => ProtoStopReason::Error,
    }
}

/// Reject a non-positive account id before it reaches a query.
///
/// `accounts.id` is a SQLite `INTEGER PRIMARY KEY` and is always positive, so
/// a zero or negative id is a client bug; answering `INVALID_ARGUMENT` says so,
/// where letting it through would answer `NOT_FOUND` and read as "that account
/// was deleted".
fn validate_account(account_id: i64) -> Result<i64, Status> {
    if account_id <= 0 {
        return Err(Status::from(Error::invalid_argument(
            "account_id must be a positive account id",
        )));
    }
    Ok(account_id)
}
