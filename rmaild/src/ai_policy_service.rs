//! The `AiPolicyService` gRPC implementation: the AI governance control
//! plane, which today means budgets ([`rmail_core::ai::budget`], task 76).
//!
//! # Why this is not part of `AiService`
//!
//! `AiService` is the data plane: it answers "what did the pipeline produce
//! for this message" and, for `AnalyzeMessage`/`SuggestReply`, asks it to
//! produce more. The RPCs here change the *rules* that pipeline runs under.
//! That is a different privilege — `SetBudget` is `admin` in
//! [`crate::auth::methods`]'s table, alongside every other daemon-wide
//! control-plane toggle — and a different lifetime: a budget outlives every
//! job it governs, and is read fresh on each dispatch rather than captured at
//! daemon start, which is what makes a `SetBudget` take effect on the very
//! next job instead of at the next restart.
//!
//! # This service stores; it does not enforce
//!
//! Nothing here decides whether a call may be made. It writes `ai_budgets`
//! rows and reads spend back out of `ai_ledger`; the decision happens in
//! [`rmail_core::ai::budget::BudgetEnforcer`], on the dispatch path, before
//! the provider is reached. Keeping the two apart is what stops the enforcer
//! from acquiring an RPC-shaped dependency it would then have to fake in
//! tests, and it is why a budget takes effect without any signal from this
//! service to the worker pool: the worker reads the same table.
//!
//! # Absent means uncapped, and that is not the same as zero
//!
//! Every cap on the wire is `optional`. An absent `hard_usd` means "no dollar
//! ceiling on this window"; `hard_usd = 0` means "spend nothing at all",
//! since the enforcer's boundary is `>=`. `proto3`'s explicit-presence
//! `optional` is what keeps those two distinguishable — without it a client
//! clearing a cap and a client forbidding all spending would send identical
//! bytes.
//
// `tonic::Status` is intentionally the error type throughout a gRPC service
// boundary; its size makes `result_large_err` fire on every
// `Result<_, Status>` helper, so the lint is allowed for this module — the
// same allowance `audit_service.rs`/`hook_service.rs` carry for the identical
// reason.
#![allow(clippy::result_large_err)]

use rmail_core::ai::budget::{
    self, Budget, BudgetCaps as CoreBudgetCaps, BudgetClass as CoreBudgetClass, ClassReport,
    WindowCaps, GLOBAL_ACCOUNT_ID,
};
use rmail_core::config::AiLimits;
use rmail_core::storage::Database;
use rmail_core::Error;
use rmail_proto::v1::ai_policy_service_server::AiPolicyService;
use rmail_proto::v1::{
    BudgetCaps, BudgetClass, BudgetSpend, BudgetWindowCaps, ClassSpend, GetSpendRequest,
    GetSpendResponse, SetBudgetRequest, SetBudgetResponse,
};
use tonic::{Request, Response, Status};

/// The `AiPolicyService` handler.
#[derive(Debug, Clone)]
pub struct AiPolicyApi {
    db: Database,
    /// The configured caps a scope with no stored budget falls back to — the
    /// same `AiLimits` the enforcer resolves against, so `GetSpend` reports
    /// the caps that will actually be applied rather than a second opinion.
    limits: AiLimits,
}

impl AiPolicyApi {
    /// Build a handler over `db`, reporting `limits` as the configured
    /// fallback.
    #[must_use]
    pub fn new(db: Database, limits: AiLimits) -> Self {
        Self { db, limits }
    }
}

#[tonic::async_trait]
impl AiPolicyService for AiPolicyApi {
    #[tracing::instrument(skip(self, request))]
    async fn set_budget(
        &self,
        request: Request<SetBudgetRequest>,
    ) -> Result<Response<SetBudgetResponse>, Status> {
        let request = request.into_inner();
        let class = decode_class(request.class)?;
        let caps = decode_caps(request.caps.as_ref())?;
        let budget = Budget {
            account_id: request.account_id,
            class,
            caps,
        };
        budget::set_budget(&self.db, &budget)
            .await
            .map_err(Status::from)?;

        // Echoed back from the value that was stored, not from the request,
        // so a client sees exactly what took effect.
        Ok(Response::new(SetBudgetResponse {
            account_id: budget.account_id,
            class: encode_class(budget.class).into(),
            caps: Some(encode_caps(&budget.caps)),
        }))
    }

    #[tracing::instrument(skip(self, request))]
    async fn get_spend(
        &self,
        request: Request<GetSpendRequest>,
    ) -> Result<Response<GetSpendResponse>, Status> {
        let account_id = request.into_inner().account_id;
        if account_id < 0 {
            return Err(Status::from(Error::invalid_argument(format!(
                "account_id must be {GLOBAL_ACCOUNT_ID} (global) or a real account id, got \
                 {account_id}"
            ))));
        }
        let report = budget::spend_report(
            &self.db,
            &self.limits,
            account_id,
            chrono::Utc::now().timestamp(),
        )
        .await
        .map_err(Status::from)?;

        Ok(Response::new(GetSpendResponse {
            account_id: report.account_id,
            day: report.day,
            month: report.month,
            all: Some(encode_class_spend(CoreBudgetClass::All, &report.all)),
            bulk: Some(encode_class_spend(CoreBudgetClass::Bulk, &report.bulk)),
        }))
    }
}

/// Map the wire enum onto the domain one.
///
/// `UNSPECIFIED` is rejected rather than defaulted to `ALL`: a client that
/// forgot to set the field would otherwise silently rewrite the *whole*
/// scope's budget when it meant to set only the bulk sub-budget, and a
/// budget is exactly the kind of setting where guessing is worse than
/// erroring.
fn decode_class(value: i32) -> Result<CoreBudgetClass, Status> {
    match BudgetClass::try_from(value) {
        Ok(BudgetClass::All) => Ok(CoreBudgetClass::All),
        Ok(BudgetClass::Bulk) => Ok(CoreBudgetClass::Bulk),
        Ok(BudgetClass::Unspecified) | Err(_) => Err(Status::from(Error::invalid_argument(
            "class must be BUDGET_CLASS_ALL or BUDGET_CLASS_BULK".to_owned(),
        ))),
    }
}

fn encode_class(class: CoreBudgetClass) -> BudgetClass {
    match class {
        CoreBudgetClass::All => BudgetClass::All,
        CoreBudgetClass::Bulk => BudgetClass::Bulk,
    }
}

/// Absent caps are uncapped, not zero — see the module docs.
fn decode_caps(caps: Option<&BudgetCaps>) -> Result<CoreBudgetCaps, Status> {
    let Some(caps) = caps else {
        return Ok(CoreBudgetCaps::default());
    };
    Ok(CoreBudgetCaps {
        daily: decode_window(caps.daily.as_ref(), "daily")?,
        monthly: decode_window(caps.monthly.as_ref(), "monthly")?,
    })
}

fn decode_window(caps: Option<&BudgetWindowCaps>, window: &str) -> Result<WindowCaps, Status> {
    let Some(caps) = caps else {
        return Ok(WindowCaps::default());
    };
    Ok(WindowCaps {
        soft_usd_micros: decode_usd(caps.soft_usd, window, "soft_usd")?,
        hard_usd_micros: decode_usd(caps.hard_usd, window, "hard_usd")?,
        soft_tokens: caps.soft_tokens,
        hard_tokens: caps.hard_tokens,
    })
}

/// Convert a wire dollar amount to the integer micro-dollars the enforcer
/// compares in, rejecting the values a `double` can carry but a cap cannot
/// mean.
///
/// `NaN`/infinity are caught here rather than left to
/// [`budget::usd_to_micros`]'s saturating fallback: at the RPC boundary a
/// client sending one has made a mistake worth telling them about, where the
/// library-level saturation exists so an internal caller cannot accidentally
/// produce a cap of zero.
fn decode_usd(value: Option<f64>, window: &str, field: &str) -> Result<Option<i64>, Status> {
    let Some(value) = value else {
        return Ok(None);
    };
    if !value.is_finite() {
        return Err(Status::from(Error::invalid_argument(format!(
            "{window} {field} must be a finite dollar amount"
        ))));
    }
    Ok(Some(budget::usd_to_micros(value)))
}

fn encode_caps(caps: &CoreBudgetCaps) -> BudgetCaps {
    BudgetCaps {
        daily: Some(encode_window(&caps.daily)),
        monthly: Some(encode_window(&caps.monthly)),
    }
}

fn encode_window(caps: &WindowCaps) -> BudgetWindowCaps {
    BudgetWindowCaps {
        soft_usd: caps.soft_usd_micros.map(budget::micros_to_usd),
        hard_usd: caps.hard_usd_micros.map(budget::micros_to_usd),
        soft_tokens: caps.soft_tokens,
        hard_tokens: caps.hard_tokens,
    }
}

fn encode_class_spend(class: CoreBudgetClass, report: &ClassReport) -> ClassSpend {
    ClassSpend {
        class: encode_class(class).into(),
        daily: Some(BudgetSpend {
            usd: budget::micros_to_usd(report.spend.daily.usd_micros),
            tokens: report.spend.daily.tokens,
        }),
        monthly: Some(BudgetSpend {
            usd: budget::micros_to_usd(report.spend.monthly.usd_micros),
            tokens: report.spend.monthly.tokens,
        }),
        caps: Some(encode_caps(&report.caps)),
        stored: report.stored,
    }
}
