//! The `HookService` gRPC implementation: read-only inspection of the
//! configured hooks (`ListHooks`) plus an on-demand dry run (`TestHook`).
//!
//! # No Create/Update/Delete RPC
//!
//! Hooks are config-driven (`rmail_core::config::HooksConfig`, loaded from
//! the master TOML at daemon startup) — this service only ever reads what
//! [`rmail_core::hooks::resolve`] already resolved once at construction
//! time. Adding, editing, or removing a hook is `mail hook add`/editing the
//! config file directly, not a gRPC call: see `rmail-cli::hook_cli`'s own
//! module docs for why writing straight to the operator's config file is
//! the right place for that, and `rmail_core::config::HookConfig`'s docs for
//! the schema it writes. This mirrors `AiPolicyConfig`'s rules and every
//! other TOML-only settings table in this crate — there is no service in
//! this codebase that round-trips `[[accounts]]` either.
//!
//! # `ListHooks` shows every hook; the dispatcher only fires enabled ones
//!
//! [`HookApi`] resolves the *whole* configured list once — including
//! disabled hooks — because an operator inspecting `mail hook list` wants
//! to see a hook they turned off, not have it silently vanish. The daemon's
//! own [`rmail_core::hooks::HookDispatcher`] resolves independently and
//! filters to `enabled` hooks only (see that type's own docs); the two are
//! deliberately built from the same [`rmail_core::config::HooksConfig`]
//! separately rather than one being derived from the other, since a
//! dispatcher's filtered view is not what a listing RPC should show.
//!
//! # `TestHook` shares the dispatcher's own concurrency budget
//!
//! It calls [`rmail_core::hooks::run_hook`] directly — the same function a
//! real dispatch tick calls — with either the caller's `event_json` or a
//! synthetic sample shaped identically to what a real event would carry
//! (see [`rmail_core::hooks::sample_event_json`]). It never touches the
//! event log or the dispatcher's own matching logic (an operator validating
//! a hook should not need a real mail event to exist), but it *does* draw
//! from the *same* `Semaphore(max_concurrency)` the dispatcher enforces for
//! matched hooks — [`HookApi::new`] takes the dispatcher's own
//! [`rmail_core::hooks::HookDispatcher::semaphore`] handle rather than
//! minting a second, independent one, which is exactly what would let
//! `TestHook` traffic plus a real event burst together exceed
//! `hooks.max_concurrency`'s actual ceiling in practice. Mirrors
//! `AiWorkerPool::semaphore`/`AiApi::new`'s identical reasoning for
//! `AnalyzeMessage`/`SuggestReply` sharing the AI dispatch loop's own pool.
//
// `tonic::Status` is intentionally the error type throughout a gRPC service
// boundary; its size makes `result_large_err` fire on every `Result<_, Status>`
// helper, so the lint is allowed for this module — the same allowance
// `audit_service.rs` carries for the identical reason.
#![allow(clippy::result_large_err)]

use std::sync::Arc;

use rmail_core::config::{HookEvent as CoreHookEvent, HooksConfig};
use rmail_core::hooks::{self, ResolvedHook};
use rmail_core::Error;
use rmail_proto::v1::hook_service_server::HookService;
use rmail_proto::v1::{
    HookEvent as ProtoHookEvent, HookInfo, ListHooksRequest, ListHooksResponse, TestHookRequest,
    TestHookResponse,
};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};

/// The `HookService` handler, backed by a hook list resolved once from
/// config at daemon startup.
#[derive(Debug, Clone)]
pub struct HookApi {
    /// Every configured hook, enabled or not — see the module docs on why
    /// this differs from what `HookDispatcher` itself drives.
    hooks: Vec<ResolvedHook>,
    max_output_bytes: usize,
    /// The dispatcher's own semaphore — see the module docs' "`TestHook`
    /// shares the dispatcher's own concurrency budget."
    semaphore: Arc<Semaphore>,
    /// Cancelled when the daemon shuts down, so an in-flight `TestHook` run
    /// is killed with it rather than outliving the daemon — see
    /// `rmail_core::hooks`'s own docs on why a killed (not abandoned) child
    /// is the whole point of threading this through.
    shutdown: CancellationToken,
}

impl HookApi {
    /// Build a handler over `config`'s hooks, bounded by `semaphore` — pass
    /// the running `HookDispatcher`'s own `semaphore()` handle (see the
    /// module docs for why sharing rather than minting a second one is
    /// required for the concurrency bound to mean anything).
    #[must_use]
    pub fn new(
        config: &HooksConfig,
        shutdown: CancellationToken,
        semaphore: Arc<Semaphore>,
    ) -> Self {
        Self {
            hooks: hooks::resolve(config),
            max_output_bytes: usize::try_from(config.max_output_bytes).unwrap_or(usize::MAX),
            semaphore,
            shutdown,
        }
    }
}

#[tonic::async_trait]
impl HookService for HookApi {
    async fn list_hooks(
        &self,
        _request: Request<ListHooksRequest>,
    ) -> Result<Response<ListHooksResponse>, Status> {
        Ok(Response::new(ListHooksResponse {
            hooks: self.hooks.iter().map(to_proto).collect(),
        }))
    }

    #[tracing::instrument(skip(self, request), fields(hook, timed_out, cancelled, exit_code))]
    async fn test_hook(
        &self,
        request: Request<TestHookRequest>,
    ) -> Result<Response<TestHookResponse>, Status> {
        let req = request.into_inner();
        tracing::Span::current().record("hook", req.name.as_str());
        let hook = self
            .hooks
            .iter()
            .find(|h| h.name == req.name)
            .ok_or_else(|| {
                Status::from(Error::not_found(format!("no hook named {:?}", req.name)))
            })?;

        let payload = match req.event_json {
            Some(json) => {
                // Validated, not merely forwarded: a caller handing this
                // RPC malformed JSON should get a clear `InvalidArgument`
                // now, not a hook script blamed for choking on garbage
                // stdin. This is the only thing this service does with the
                // string beyond piping it — it is never interpreted, never
                // touches the command/argv (see `rmail_core::hooks`'s own
                // module docs on why that boundary matters).
                let _: serde_json::Value = serde_json::from_str(&json).map_err(|e| {
                    Status::from(Error::invalid_argument(format!(
                        "event_json is not valid JSON: {e}"
                    )))
                })?;
                json.into_bytes()
            }
            None => hooks::sample_event_json(hook.event),
        };

        // Bounded by the same budget a real dispatch tick draws from — see
        // the module docs. The semaphore is never explicitly closed, so an
        // `Err` here is unreachable in practice; propagated as
        // `Unavailable` rather than assumed-away.
        let Ok(_permit) = Arc::clone(&self.semaphore).acquire_owned().await else {
            return Err(Status::from(Error::unavailable(
                "hook concurrency budget is unavailable",
            )));
        };

        tracing::info!(hook = %hook.name, "running TestHook");
        let cancel = self.shutdown.child_token();
        let outcome = hooks::run_hook(
            &hook.command,
            &hook.args,
            hook.timeout,
            self.max_output_bytes,
            &payload,
            &cancel,
        )
        .await;

        let span = tracing::Span::current();
        span.record("timed_out", outcome.timed_out);
        span.record("cancelled", outcome.cancelled);
        span.record("exit_code", tracing::field::debug(outcome.exit_code));
        tracing::info!(
            hook = %hook.name,
            timed_out = outcome.timed_out,
            cancelled = outcome.cancelled,
            exit_code = ?outcome.exit_code,
            duration_ms = outcome.duration.as_millis(),
            "TestHook finished"
        );

        Ok(Response::new(TestHookResponse {
            timed_out: outcome.timed_out,
            cancelled: outcome.cancelled,
            exit_code: outcome.exit_code,
            stdout: outcome.stdout,
            stderr: outcome.stderr,
            duration_ms: i64::try_from(outcome.duration.as_millis()).unwrap_or(i64::MAX),
        }))
    }
}

/// Project a resolved hook onto its proto representation.
fn to_proto(hook: &ResolvedHook) -> HookInfo {
    HookInfo {
        name: hook.name.clone(),
        event: hook_event_to_proto(hook.event) as i32,
        command: hook.command.clone(),
        args: hook.args.clone(),
        enabled: hook.enabled,
        timeout_ms: i64::try_from(hook.timeout.as_millis()).unwrap_or(i64::MAX),
    }
}

/// Map the domain [`CoreHookEvent`] onto the wire [`ProtoHookEvent`].
const fn hook_event_to_proto(event: CoreHookEvent) -> ProtoHookEvent {
    match event {
        CoreHookEvent::OnNewMessage => ProtoHookEvent::OnNewMessage,
        CoreHookEvent::OnLabel => ProtoHookEvent::OnLabel,
        CoreHookEvent::OnMove => ProtoHookEvent::OnMove,
        CoreHookEvent::OnRuleMatch => ProtoHookEvent::OnRuleMatch,
        CoreHookEvent::OnSyncError => ProtoHookEvent::OnSyncError,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_hook_event_maps_to_a_distinct_non_unspecified_proto_value() {
        let mut seen = std::collections::HashSet::new();
        for event in [
            CoreHookEvent::OnNewMessage,
            CoreHookEvent::OnLabel,
            CoreHookEvent::OnMove,
            CoreHookEvent::OnRuleMatch,
            CoreHookEvent::OnSyncError,
        ] {
            let proto = hook_event_to_proto(event);
            assert_ne!(proto, ProtoHookEvent::Unspecified);
            assert!(
                seen.insert(proto as i32),
                "duplicate proto mapping for {event:?}"
            );
        }
    }
}
