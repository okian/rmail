//! The one model sink the extractors share.
//!
//! Table structuring, calendar/task extraction and link classification are
//! three different questions, but they are the same *call*: a frozen system
//! prompt, one user turn carrying attacker-authored text, a JSON schema the
//! answer must validate against, and the full gate sequence around it. Writing
//! that three times would be three chances to get the ordering wrong — and the
//! ordering is the security property (see [`crate::ai::gate`]'s module docs).
//!
//! So this module owns the sink and the extractors own their prompts. What
//! stays here, and cannot be opted out of by a caller:
//!
//! - The system prompt is always passed through
//!   [`injection::with_data_boundary`], so the model is always told that the
//!   block below is data.
//! - The message text is always inside [`injection::untrusted_block`] —
//!   [`Ask::untrusted`] is the *only* way to put sender-authored text in the
//!   turn, and it fences on the way in.
//! - `policy → daily cap → budget → redact → concurrency/rate → provider →
//!   audit`, in that order, for every call, including the failed ones. The
//!   redaction firewall runs before the permit is taken rather than after: a
//!   request that redaction refuses outright must not have held capacity while
//!   it was being built.
//! - Everything the model wrote comes back through
//!   [`injection::sanitize_model_text`] before any caller sees it, because
//!   these answers are printed to terminals and rendered in pickers.

use std::sync::Arc;

use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::ai::gate;
use crate::ai::injection;
use crate::ai::policy::PolicyEngine;
use crate::ai::provider::{ChatRequest, OutputFormat, Provider};
use crate::ai::queue::{payload_bytes, RateLimiter};
use crate::ai::redact::GuardedRequest;
use crate::ai::{self, CallOutcome, CallRecord};
use crate::config::{AiLimits, AiPrivacy};
use crate::error::Error;
use crate::storage::Database;

/// The `ai_ledger.pass` value every extraction call is recorded under, so an
/// operator can tell this spend from triage/deep/rules spend.
pub const PASS: &str = "extract";

/// Everything one extraction call needs that is not shared plumbing.
pub struct Ask<'a> {
    /// The extractor's own instructions. Fenced by this module, never by the
    /// caller — a caller that fenced its own would double the clause.
    pub system: &'a str,
    /// The turn's trusted framing: the question, the vocabulary, the format.
    /// Never sender-authored.
    pub instruction: String,
    /// Sender-authored text, wrapped in an untrusted block with this label.
    pub untrusted: Vec<(&'a str, String)>,
    /// The JSON Schema the answer must validate against.
    pub schema: serde_json::Value,
    /// Output-token ceiling.
    pub max_tokens: u32,
    /// The account the call is attributed to, for policy and budget.
    pub account_id: i64,
    /// The folder it concerns, when there is one.
    pub mailbox: Option<String>,
    /// The message it concerns, for the audit ledger.
    pub message_id: Option<i64>,
}

/// The shared model sink.
///
/// Cheap to clone — every field is a handle. `semaphore`/`rate_limiter` must be
/// the running AI pool's own handles; minting fresh ones doubles the operator's
/// configured ceiling, which [`crate::ai::gate`] documents at length.
#[derive(Debug, Clone)]
pub struct ExtractModel {
    db: Database,
    provider: Arc<dyn Provider>,
    policy: Arc<PolicyEngine>,
    privacy: AiPrivacy,
    limits: AiLimits,
    model: String,
    semaphore: Arc<Semaphore>,
    rate_limiter: Arc<RateLimiter>,
}

impl ExtractModel {
    /// Build a sink.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: Database,
        provider: Arc<dyn Provider>,
        policy: Arc<PolicyEngine>,
        privacy: AiPrivacy,
        limits: AiLimits,
        model: impl Into<String>,
        semaphore: Arc<Semaphore>,
        rate_limiter: Arc<RateLimiter>,
    ) -> Self {
        Self {
            db,
            provider,
            policy,
            privacy,
            limits,
            model: model.into(),
            semaphore,
            rate_limiter,
        }
    }

    /// Ask the model one structured question.
    ///
    /// Returns the raw JSON text of the answer, rehydrated (the redaction
    /// firewall's placeholders put back) and with display-hostile characters
    /// stripped. Parsing it is the caller's job, because only the caller knows
    /// its schema.
    ///
    /// # Errors
    ///
    /// [`Error::FailedPrecondition`] if policy forbids a network call for this
    /// account/folder, the daily cap is reached, or redaction left nothing to
    /// send; [`Error::ResourceExhausted`] if a budget hard cap blocks it;
    /// [`Error::DeadlineExceeded`] if `cancel` fires while waiting for
    /// capacity; whatever the provider returns otherwise. Never a degraded
    /// answer — a caller must be able to tell "the model said nothing was
    /// there" from "there was no model".
    #[tracing::instrument(skip(self, ask, cancel), fields(account_id = ask.account_id, model), err)]
    pub async fn ask(&self, ask: &Ask<'_>, cancel: &CancellationToken) -> Result<String, Error> {
        let model = gate::admit(
            &self.db,
            &self.policy,
            &self.limits,
            ask.account_id,
            ask.mailbox.as_deref(),
            &self.model,
        )
        .await?;
        tracing::Span::current().record("model", model.as_str());

        let mut turn = ask.instruction.clone();
        for (label, text) in &ask.untrusted {
            turn.push_str("\n\n");
            turn.push_str(&injection::untrusted_block(label, text));
        }
        let request = ChatRequest::new(model.clone(), ask.max_tokens)
            .system(injection::with_data_boundary(ask.system))
            .user(turn)
            .output_format(OutputFormat::json_schema(ask.schema.clone()));

        let (request, tokens) = match ai::guard(&request, &self.privacy) {
            GuardedRequest::RedactedSkip => {
                return Err(Error::failed_precondition(
                    "nothing was left to extract once PII was redacted from this message"
                        .to_owned(),
                ))
            }
            GuardedRequest::Redacted {
                request, tokens, ..
            } => (request, tokens),
        };
        let payload = payload_bytes(&request);
        let redaction_level = if tokens.is_empty() {
            "none"
        } else {
            "redacted"
        }
        .to_owned();

        let _permit = gate::acquire_capacity(&self.semaphore, &self.rate_limiter, cancel).await?;
        let started = std::time::Instant::now();
        let response = self.provider.complete(&request, cancel).await;
        let latency = started.elapsed();

        let response = match response {
            Ok(response) => response,
            Err(error) => {
                // The ledger records what this machine *tried* to send, not
                // only what succeeded. A failure to record is logged and
                // swallowed so it cannot mask the real error.
                if let Err(audit_error) = ai::record_call(
                    &self.db,
                    CallRecord {
                        account_id: Some(ask.account_id),
                        message_id: ask.message_id,
                        request_id: None,
                        model: model.clone(),
                        pass: Some(PASS.to_owned()),
                        usage: ai::Usage::default(),
                        redaction_level,
                        latency,
                        payload: &payload,
                        outcome: CallOutcome::Error(error.to_string()),
                    },
                )
                .await
                {
                    tracing::warn!(%audit_error, "could not record a failed extraction call");
                }
                return Err(error);
            }
        };

        ai::record_call(
            &self.db,
            CallRecord {
                account_id: Some(ask.account_id),
                message_id: ask.message_id,
                request_id: Some(response.id.clone()),
                model,
                pass: Some(PASS.to_owned()),
                usage: response.usage,
                redaction_level,
                latency,
                payload: &payload,
                outcome: CallOutcome::Ok,
            },
        )
        .await?;

        Ok(ai::rehydrate(&response.text, &tokens))
    }
}
