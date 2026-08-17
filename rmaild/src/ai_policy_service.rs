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

use std::sync::Arc;

use rmail_core::ai::budget::{
    self, Budget, BudgetCaps as CoreBudgetCaps, BudgetClass as CoreBudgetClass, ClassReport,
    WindowCaps, GLOBAL_ACCOUNT_ID,
};
use rmail_core::ai::local::{self, LocalProvider};
use rmail_core::ai::{PolicyEngine, PolicyTarget};
use rmail_core::config::{AiConfig, AiLimits, AiProvider as CoreAiProvider};
use rmail_core::storage::Database;
use rmail_core::Error;
use rmail_proto::v1::ai_policy_service_server::AiPolicyService;
use rmail_proto::v1::{
    AiProviderKind, BudgetCaps, BudgetClass, BudgetSpend, BudgetWindowCaps, ClassSpend,
    GetAiProviderRequest, GetAiProviderResponse, GetSpendRequest, GetSpendResponse,
    SetAiProviderRequest, SetAiProviderResponse, SetBudgetRequest, SetBudgetResponse,
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
    /// The AI section as configured, for the backend-routing RPCs: the
    /// daemon-wide default an account with no override inherits, and the
    /// `ai.local` settings a readiness check reads.
    ai: AiConfig,
    /// The same engine the dispatch path resolves against, so
    /// `GetAiProvider`'s `policy_mode` is the policy actually in force rather
    /// than a second reading of the config file.
    policy: Arc<PolicyEngine>,
    /// Whether this process holds a network-capable provider at all — see
    /// `rmail_core::ai::local`'s module docs. Passed in rather than derived,
    /// because only the wiring that built (or declined to build) the provider
    /// knows the answer, including the injected-provider case.
    network_provider_built: bool,
}

impl AiPolicyApi {
    /// The account's name, or `None` for the daemon-wide scope.
    ///
    /// # Errors
    /// [`Error::NotFound`] if `account_id` names no account — a scope that
    /// does not exist is never a scope a routing row may be written for.
    async fn require_account(&self, account_id: i64) -> Result<Option<String>, Status> {
        if account_id == GLOBAL_ACCOUNT_ID {
            return Ok(None);
        }
        let name = rmail_core::rules::repo::account_name(&self.db, account_id)
            .await
            .map_err(Status::from)?
            .ok_or_else(|| Status::from(Error::not_found(format!("account {account_id}"))))?;
        Ok(Some(name))
    }

    /// Build a handler over `db`, reporting `limits` as the configured
    /// fallback and `ai`/`policy` as the routing context.
    #[must_use]
    pub fn new(
        db: Database,
        limits: AiLimits,
        ai: AiConfig,
        policy: Arc<PolicyEngine>,
        network_provider_built: bool,
    ) -> Self {
        Self {
            db,
            limits,
            ai,
            policy,
            network_provider_built,
        }
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

    #[tracing::instrument(skip(self, request))]
    async fn set_ai_provider(
        &self,
        request: Request<SetAiProviderRequest>,
    ) -> Result<Response<SetAiProviderResponse>, Status> {
        let request = request.into_inner();
        check_scope(request.account_id)?;
        // Checked here as well as in `GetAiProvider`, and for a sharper
        // reason: `accounts.id` is a rowid alias SQLite reuses, so a row
        // written for an account that does not exist is a routing decision
        // waiting to attach itself to whatever account is created next — and
        // V52's delete trigger cannot clean up a row whose account never
        // existed to be deleted. In the `claude` direction that is a silent
        // widening of egress for an account nobody set it for.
        self.require_account(request.account_id).await?;
        let provider = decode_provider(request.provider)?;

        // Configuration, not provisioning. Routing an account to a backend
        // this daemon could not build under any circumstances is a mistake
        // worth catching at the moment it is made — the alternative is every
        // subsequent message failing with the same precondition. Missing
        // *weights* are deliberately not checked: an operator may legitimately
        // set the routing first and provision after, which is why
        // `GetAiProvider` reports readiness separately.
        if provider == Some(CoreAiProvider::Local) {
            local::check_config(&self.ai.local).map_err(Status::from)?;
        }

        local::set_override(&self.db, request.account_id, provider)
            .await
            .map_err(Status::from)?;
        let effective = local::effective_provider(&self.db, request.account_id, self.ai.provider)
            .await
            .map_err(Status::from)?;

        Ok(Response::new(SetAiProviderResponse {
            // Echoed from what was decoded and stored, not from the request's
            // raw enum value, so a client sees exactly what took effect.
            provider: encode_provider(provider).into(),
            account_id: request.account_id,
            effective: encode_provider(Some(effective)).into(),
        }))
    }

    #[tracing::instrument(skip(self, request))]
    async fn get_ai_provider(
        &self,
        request: Request<GetAiProviderRequest>,
    ) -> Result<Response<GetAiProviderResponse>, Status> {
        let account_id = request.into_inner().account_id;
        check_scope(account_id)?;

        let account_override = local::stored_override(&self.db, account_id)
            .await
            .map_err(Status::from)?;
        let effective = local::effective_provider(&self.db, account_id, self.ai.provider)
            .await
            .map_err(Status::from)?;

        // The daemon-wide scope names no account, so it is resolved against
        // the empty target: no rule can match it, which reports exactly the
        // `ai.policy.default_mode` an account with no rules of its own would
        // get. That is the honest answer for a scope that is not an account.
        let name = self.require_account(account_id).await?.unwrap_or_default();
        let policy_mode = self
            .policy
            .resolve(&PolicyTarget::account(name))
            .mode
            .as_str()
            .to_owned();

        // Cheap: constructing the local provider does no I/O, and the
        // readiness check is a `stat` on a blocking thread.
        let readiness = LocalProvider::new(&self.ai.local).readiness().await;

        Ok(Response::new(GetAiProviderResponse {
            account_id,
            configured: encode_provider(Some(self.ai.provider)).into(),
            account_override: encode_provider(account_override).into(),
            effective: encode_provider(Some(effective)).into(),
            policy_mode,
            network_provider_built: self.network_provider_built,
            local_model: format!("{}{}", local::LOCAL_MODEL_PREFIX, readiness.model),
            local_ready: readiness.ready,
            local_detail: readiness.detail,
        }))
    }
}

/// Both routing RPCs take the same scope, with the same sentinel.
fn check_scope(account_id: i64) -> Result<(), Status> {
    if account_id < 0 {
        return Err(Status::from(Error::invalid_argument(format!(
            "account_id must be {GLOBAL_ACCOUNT_ID} (daemon-wide) or a real account id, got \
             {account_id}"
        ))));
    }
    Ok(())
}

/// Map the wire enum onto the domain one, `None` for "clear the override".
///
/// `UNSPECIFIED` means *clear* here, where `BudgetClass::Unspecified` is
/// rejected — the two are not inconsistent. A budget's class picks which of
/// two rows to overwrite, so guessing loses data; an override is a single
/// nullable setting, and "unset" is a value a client legitimately needs to
/// send. There is no other way to express it: the field is a plain enum, and
/// a second "clear" RPC for one field would be a worse surface.
fn decode_provider(value: i32) -> Result<Option<CoreAiProvider>, Status> {
    match AiProviderKind::try_from(value) {
        Ok(AiProviderKind::Unspecified) => Ok(None),
        Ok(AiProviderKind::Claude) => Ok(Some(CoreAiProvider::Claude)),
        Ok(AiProviderKind::Local) => Ok(Some(CoreAiProvider::Local)),
        Err(_) => Err(Status::from(Error::invalid_argument(format!(
            "unknown AiProviderKind {value}; expected UNSPECIFIED (clear), CLAUDE or LOCAL"
        )))),
    }
}

fn encode_provider(provider: Option<CoreAiProvider>) -> AiProviderKind {
    match provider {
        None => AiProviderKind::Unspecified,
        Some(CoreAiProvider::Claude) => AiProviderKind::Claude,
        Some(CoreAiProvider::Local) => AiProviderKind::Local,
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
