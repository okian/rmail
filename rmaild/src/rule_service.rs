//! The `RuleService` gRPC implementation (task 66): create, list, evaluate,
//! synthesize, backtest, and correct rules.
//!
//! # This file is a boundary, not logic
//!
//! Every handler here does the same three things: validate what only the wire
//! shape can be wrong about, call one [`rmail_core::rules::RuleEngine`] (or
//! [`rmail_core::rules::RuleSynthesizer`]) method, and project the result onto
//! protos. Predicate evaluation, the `claude_is` cache, the at-most-once
//! action claim, and the untrusted-regex bounds all live in `rmail-core` and
//! are tested there against a real database; duplicating any of that decision
//! here would mean the daemon and the library could disagree.
//!
//! [`rmail_core::Error`] converts to [`Status`] with the right code and a
//! stable `ErrorInfo.reason` (see `rmail_core::error`), so no handler builds a
//! `Status` by hand except where the wire shape itself is the problem.
//!
//! # Why `EvaluateRules` and `BacktestRule` are separate RPCs
//!
//! `EvaluateRules` fires actions: it can move mail, run a configured hook, and
//! create a reply draft. `BacktestRule` fires none of them — it still writes
//! the `claude_is` classification cache and an `ai_ledger` row, and still
//! spends real money at the provider, but no mail, tag, draft, event, or
//! process is touched by it. The
//! capability-scope table (`auth::methods`) is keyed by method path and cannot
//! see a request's fields, so a single RPC with a `dry_run` flag would force
//! every read-only backtest to be granted the mutating privilege — exactly the
//! argument `IndexService`'s `Reindex`/`Rebuild` split already records. The
//! split is therefore a security boundary rather than an API preference, and
//! it is also why `BacktestRule` cannot fire an action even by mistake: the
//! engine's dry-run path never claims and never calls the action runner's
//! `apply`.
//!
//! # A disabled AI subsystem still serves most of this
//!
//! `RuleService` is registered unconditionally, the convention
//! `AiService`/`HookService` established. With no usable provider, everything
//! deterministic still works — create, list, evaluate and backtest rules with
//! no `claude_is` — and only the paths that genuinely need the model fail,
//! with the `FAILED_PRECONDITION` the null provider produces. That is the
//! right degradation: a rule filing mail by sender should not stop working
//! because an API key expired.
//
// `tonic::Status` is intentionally the error type throughout a gRPC service
// boundary; its size makes `result_large_err` fire on every `Result<_,
// Status>` helper, so the lint is allowed for this module — the same
// allowance `hook_service.rs` carries for the identical reason.
#![allow(clippy::result_large_err)]

use rmail_core::rules::{
    ActionOutcome as CoreActionOutcome, EvaluationReport, MessageReport, RuleEngine, RuleReport,
    RuleSelector, RuleSynthesizer,
};
use rmail_core::Error;
use rmail_proto::v1::rule_service_server::RuleService;
use rmail_proto::v1::{
    ActionOutcome, BacktestRuleRequest, BacktestRuleResponse, CreateRuleRequest,
    CreateRuleResponse, EvaluateRulesRequest, EvaluateRulesResponse, EvaluationStats,
    ListRulesRequest, ListRulesResponse, MessageOutcome, PredicateOutcome, RecordCorrectionRequest,
    RecordCorrectionResponse, Rule, RuleOutcome, SynthesizeRuleRequest, SynthesizeRuleResponse,
};
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};

/// Ceiling on how many messages one `EvaluateRules` call names.
///
/// The engine already bounds a *backtest* window (`rules.max_window_messages`),
/// but `EvaluateRules` takes an explicit id list, so its bound has to be here.
/// Without it a single call could ask for a hundred thousand classifications —
/// which the budget gates would eventually refuse, but only after spending up
/// to the cap first.
const MAX_EVALUATE_MESSAGES: usize = 500;

/// Ceiling on how many rules one request may name.
///
/// `RuleSelector::Named` costs one blocking-pool lookup per name, so an
/// unbounded list is an unbounded number of round trips issued from one
/// request. The engine checks cancellation between them, but refusing an
/// absurd list outright is cheaper than draining it.
const MAX_NAMED_RULES: usize = 64;

/// The `RuleService` handler.
///
/// Cheap to clone: the engine and synthesizer are handles. Both are the *same*
/// instances the background `RuleEvaluator` uses, which is what makes the
/// at-most-once action claim meaningful between an RPC and a tick.
#[derive(Debug, Clone)]
pub struct RuleApi {
    engine: RuleEngine,
    synthesizer: RuleSynthesizer,
    /// The configured default backtest/dry-run window, used when a request
    /// leaves `days` at 0.
    default_days: u32,
    /// Cancelled when the daemon shuts down, so an in-flight evaluation stops
    /// with it rather than outliving it.
    shutdown: CancellationToken,
}

impl RuleApi {
    /// Build a handler.
    #[must_use]
    pub fn new(
        engine: RuleEngine,
        synthesizer: RuleSynthesizer,
        default_days: u32,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            engine,
            synthesizer,
            // A zero configured default would make every request that omits
            // `days` cover nothing at all and report an empty backtest as a
            // success, which reads as "this rule matches nothing."
            default_days: default_days.max(1),
            shutdown,
        }
    }

    fn window_days(&self, requested: u32) -> u32 {
        if requested == 0 {
            self.default_days
        } else {
            requested
        }
    }
}

#[tonic::async_trait]
impl RuleService for RuleApi {
    #[tracing::instrument(skip(self, request), fields(account_id, rule))]
    async fn create_rule(
        &self,
        request: Request<CreateRuleRequest>,
    ) -> Result<Response<CreateRuleResponse>, Status> {
        let req = request.into_inner();
        tracing::Span::current().record("account_id", req.account_id);
        let rule = self
            .engine
            .create(req.account_id, &req.toml)
            .await
            .map_err(Status::from)?;
        tracing::Span::current().record("rule", rule.name.as_str());
        Ok(Response::new(CreateRuleResponse {
            rule: Some(to_proto_rule(rule)),
        }))
    }

    #[tracing::instrument(skip(self, request), fields(account_id, rules))]
    async fn list_rules(
        &self,
        request: Request<ListRulesRequest>,
    ) -> Result<Response<ListRulesResponse>, Status> {
        let req = request.into_inner();
        tracing::Span::current().record("account_id", req.account_id);
        let rules = self
            .engine
            .list(req.account_id)
            .await
            .map_err(Status::from)?;
        tracing::Span::current().record("rules", rules.len());
        Ok(Response::new(ListRulesResponse {
            rules: rules.into_iter().map(to_proto_rule).collect(),
        }))
    }

    #[tracing::instrument(
        skip(self, request),
        fields(account_id, messages, matches, actions_applied)
    )]
    async fn evaluate_rules(
        &self,
        request: Request<EvaluateRulesRequest>,
    ) -> Result<Response<EvaluateRulesResponse>, Status> {
        let req = request.into_inner();
        let span = tracing::Span::current();
        span.record("account_id", req.account_id);
        span.record("messages", req.message_ids.len());

        if req.message_ids.is_empty() {
            return Err(Status::from(Error::invalid_argument(
                "message_ids must name at least one message; there is no \
                 evaluate-everything mode on the RPC that fires actions",
            )));
        }
        if req.message_ids.len() > MAX_EVALUATE_MESSAGES {
            return Err(Status::from(Error::invalid_argument(format!(
                "message_ids names {} messages; the limit is {MAX_EVALUATE_MESSAGES} per call",
                req.message_ids.len()
            ))));
        }
        if req.rule_names.len() > MAX_NAMED_RULES {
            return Err(Status::from(Error::invalid_argument(format!(
                "rule_names names {} rules; the limit is {MAX_NAMED_RULES} per call",
                req.rule_names.len()
            ))));
        }
        let selector = if req.rule_names.is_empty() {
            RuleSelector::AllEnabled
        } else {
            RuleSelector::Named(req.rule_names)
        };

        let report = self
            .engine
            .evaluate(
                req.account_id,
                &req.message_ids,
                &selector,
                false,
                &self.shutdown.child_token(),
            )
            .await
            .map_err(Status::from)?;
        span.record("matches", report.matches);
        span.record("actions_applied", report.actions_applied);

        let stats = to_proto_stats(&report);
        Ok(Response::new(EvaluateRulesResponse {
            messages: report.messages.into_iter().map(to_proto_message).collect(),
            stats: Some(stats),
        }))
    }

    #[tracing::instrument(skip(self, request), fields(account_id, days, dropped))]
    async fn synthesize_rule(
        &self,
        request: Request<SynthesizeRuleRequest>,
    ) -> Result<Response<SynthesizeRuleResponse>, Status> {
        let req = request.into_inner();
        let span = tracing::Span::current();
        span.record("account_id", req.account_id);
        let days = self.window_days(req.days);
        span.record("days", days);

        let synthesis = self
            .synthesizer
            .synthesize(
                req.account_id,
                &req.instruction,
                days,
                &self.shutdown.child_token(),
            )
            .await
            .map_err(Status::from)?;
        span.record("dropped", synthesis.claude_is_dropped.is_some());

        let stats = to_proto_stats(&synthesis.dry_run);
        Ok(Response::new(SynthesizeRuleResponse {
            toml: synthesis.toml,
            name: synthesis.rule.name,
            uses_claude_is: synthesis.rule.when.claude_is.is_some(),
            claude_is_dropped: synthesis.claude_is_dropped.unwrap_or_default(),
            notes: synthesis.notes,
            dry_run: synthesis
                .dry_run
                .messages
                .into_iter()
                .map(to_proto_message)
                .collect(),
            stats: Some(stats),
            window_days: synthesis.window_days,
        }))
    }

    #[tracing::instrument(skip(self, request), fields(account_id, days, matches))]
    async fn backtest_rule(
        &self,
        request: Request<BacktestRuleRequest>,
    ) -> Result<Response<BacktestRuleResponse>, Status> {
        let req = request.into_inner();
        let span = tracing::Span::current();
        span.record("account_id", req.account_id);
        let days = self.window_days(req.days);
        span.record("days", days);

        let named = !req.rule_name.trim().is_empty();
        let ad_hoc = !req.rule_toml.trim().is_empty();
        // Rejected rather than resolved by precedence: a caller that sent both
        // has a bug, and silently backtesting one of them would report a
        // confident answer about a rule they did not ask about.
        let selector = match (named, ad_hoc) {
            (true, false) => RuleSelector::Named(vec![req.rule_name]),
            (false, true) => RuleSelector::Ad(Box::new(
                rmail_core::rules::parse_single(&req.rule_toml, self.engine.limits())
                    .map_err(Status::from)?,
            )),
            _ => {
                return Err(Status::from(Error::invalid_argument(
                    "set exactly one of rule_name (backtest a stored rule) or rule_toml \
                     (backtest an unsaved document)",
                )))
            }
        };

        let report = self
            .engine
            .backtest(
                req.account_id,
                &selector,
                days,
                &self.shutdown.child_token(),
            )
            .await
            .map_err(Status::from)?;
        span.record("matches", report.matches);

        let stats = to_proto_stats(&report);
        Ok(Response::new(BacktestRuleResponse {
            messages: report.messages.into_iter().map(to_proto_message).collect(),
            stats: Some(stats),
            window_days: days,
        }))
    }

    #[tracing::instrument(
        skip(self, request),
        fields(account_id, message_id, expected, example_count)
    )]
    async fn record_correction(
        &self,
        request: Request<RecordCorrectionRequest>,
    ) -> Result<Response<RecordCorrectionResponse>, Status> {
        let req = request.into_inner();
        let span = tracing::Span::current();
        span.record("account_id", req.account_id);
        span.record("message_id", req.message_id);
        span.record("expected", req.expected);
        let count = self
            .engine
            .record_correction(req.account_id, req.message_id, &req.prompt, req.expected)
            .await
            .map_err(Status::from)?;
        span.record("example_count", count);
        Ok(Response::new(RecordCorrectionResponse {
            example_count: count,
        }))
    }
}

fn to_proto_rule(rule: rmail_core::rules::StoredRule) -> Rule {
    Rule {
        id: rule.id,
        account_id: rule.account_id,
        name: rule.name,
        toml: rule.toml,
        enabled: rule.enabled,
        created_at: rule.created_at,
        updated_at: rule.updated_at,
    }
}

fn to_proto_stats(report: &EvaluationReport) -> EvaluationStats {
    EvaluationStats {
        matches: as_i64(report.matches),
        model_calls: as_i64(report.model_calls),
        cache_hits: as_i64(report.cache_hits),
        actions_applied: as_i64(report.actions_applied),
        actions_failed: as_i64(report.actions_failed),
        errors: as_i64(report.errors),
        messages: as_i64(report.messages.len()),
    }
}

fn as_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn to_proto_message(report: MessageReport) -> MessageOutcome {
    MessageOutcome {
        message_id: report.message_id,
        rfc_message_id: report.rfc_message_id.unwrap_or_default(),
        subject: report.subject,
        from: report.from,
        rules: report
            .rules
            .into_iter()
            .map(to_proto_rule_outcome)
            .collect(),
        error: report.error.unwrap_or_default(),
    }
}

fn to_proto_rule_outcome(report: RuleReport) -> RuleOutcome {
    RuleOutcome {
        rule: report.rule,
        matched: report.matched,
        predicates: report
            .outcomes
            .into_iter()
            .map(|outcome| PredicateOutcome {
                predicate: outcome.predicate,
                evaluated: outcome.evaluated,
                matched: outcome.matched,
                detail: outcome.detail.unwrap_or_default(),
            })
            .collect(),
        actions: report.actions.into_iter().map(to_proto_action).collect(),
        already_fired: report.already_fired,
        model_called: report.model_called,
        cache_hit: report.cache_hit,
        explanation: report.explanation.unwrap_or_default(),
    }
}

fn to_proto_action(outcome: CoreActionOutcome) -> ActionOutcome {
    ActionOutcome {
        action: outcome.action,
        applied: outcome.applied,
        detail: outcome.detail,
    }
}
