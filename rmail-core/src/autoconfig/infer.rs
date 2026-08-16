//! The fallback: when every probe misses, ask Claude what the settings
//! probably are.
//!
//! # The model proposes; it does not commit
//!
//! Its answer is treated exactly like an autoconfig document served by a
//! stranger, because that is what it is: a string that arrived from outside
//! and claims to be a hostname. It goes through [`super::validate`] on the way
//! out of this module — same hostname grammar, same port range, same refusal
//! of anything that is not encrypted. Nothing here writes an account, and
//! nothing downstream treats [`Source::Model`] as settled:
//! `AccountService.Autoconfigure` marks a model-derived proposal in its
//! `warnings`.
//!
//! It is treated as *less* trusted than a document in exactly one place, and
//! it is the place that matters: a model-proposed host is never logged into.
//! Verification would mean presenting the user's real password to a hostname
//! assembled from attacker-controlled evidence, and validation cannot tell a
//! provider's name from a plausible-looking one an attacker owns. See
//! [`super::Autoconfigurator::verify`].
//!
//! # What it is shown, and what it is not
//!
//! The evidence is the *domain*, its MX hosts, and the probe responses.
//! Deliberately not the email address: the local part is dropped by
//! [`super::Autoconfigurator`] before the evidence is built, so the model
//! never sees whose mailbox this is. There is no mail content in the prompt at
//! all — there cannot be, because [`Evidence`] has no field that could carry
//! any — which is why the redaction firewall
//! ([`crate::ai::redact`], which exists to keep message text out of a payload)
//! has nothing to do here.
//!
//! What *is* in the prompt is untrusted: an autoconfig document from a
//! hostile domain is text an attacker wrote, sitting in a model's context.
//! So the system prompt carries
//! [`crate::ai::injection::with_data_boundary`] and every piece of evidence is
//! wrapped in [`crate::ai::injection::untrusted_block`] — and, more
//! fundamentally, the *only* thing this call can influence is a proposal that
//! then has to survive validation and a login.

use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use crate::ai::audit::{record_call, CallOutcome, CallRecord};
use crate::ai::gate;
use crate::ai::injection;
use crate::ai::policy::PolicyEngine;
use crate::ai::provider::{ChatRequest, OutputFormat, Provider};
use crate::ai::queue::{payload_bytes, RateLimiter};
use crate::config::AiLimits;
use crate::error::Error;
use crate::storage::Database;

use super::probe::{ProbeResponse, RawCandidate, RawServer};
use super::Source;

/// The `ai_ledger.pass` this call is recorded under.
pub const PASS: &str = "autoconfig";

/// A settings guess is four short fields per server; this ceiling stops a
/// runaway generation rather than shaping the answer.
const MAX_TOKENS: u32 = 512;

/// How much of one probe response the model is shown.
///
/// An autoconfig document that matters is small. This is a bound on someone
/// else's text in a prompt, so it is a *cap*, not a target.
const MAX_EVIDENCE_CHARS: usize = 4_000;

/// The most probe responses to include, newest probes last.
const MAX_EVIDENCE_ITEMS: usize = 6;

const SYSTEM_PROMPT: &str = "You are helping configure an IMAP email client. Given a mail \
domain, its MX records, and the responses from failed autoconfiguration probes, infer the \
most likely IMAP and SMTP settings for that provider.\n\n\
Rules:\n\
- Answer only with the JSON object described by the schema. No prose.\n\
- Hostnames must be real, fully-qualified names for that provider (for example \
\"imap.fastmail.com\"). Never an IP address, never a name you are not reasonably confident in.\n\
- Use encrypted transports only: \"tls\" (implicit TLS, usually IMAP 993 / SMTP 465) or \
\"starttls\" (usually SMTP 587). Never propose an unencrypted connection.\n\
- Prefer what the MX records imply about who actually runs the mail for this domain over the \
domain's own name.\n\
- If you do not know, say so with \"confident\": false rather than inventing a hostname.";

/// Everything the model is shown. No field can carry mail content or the
/// user's address — see the module docs.
#[derive(Debug, Clone)]
pub struct Evidence {
    /// The domain part of the address being configured.
    pub domain: String,
    /// Its MX hosts, best first.
    pub mx: Vec<String>,
    /// What each probe answered.
    pub responses: Vec<ProbeResponse>,
}

/// The model's answer, before validation.
#[derive(Debug, Clone, Deserialize)]
struct Proposal {
    imap_host: String,
    imap_port: i64,
    imap_security: String,
    smtp_host: String,
    smtp_port: i64,
    smtp_security: String,
    #[serde(default)]
    confident: bool,
}

/// The model fallback, with its gate.
#[derive(Clone)]
pub struct SettingsInferrer {
    db: Database,
    provider: Arc<dyn Provider>,
    policy: Arc<PolicyEngine>,
    limits: AiLimits,
    semaphore: Arc<tokio::sync::Semaphore>,
    rate_limiter: Arc<RateLimiter>,
    model: String,
}

impl std::fmt::Debug for SettingsInferrer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SettingsInferrer")
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

impl SettingsInferrer {
    /// Wire an inferrer to the one provider, policy engine, budget and pacing
    /// pair this process already has.
    #[must_use]
    pub fn new(
        db: Database,
        provider: Arc<dyn Provider>,
        policy: Arc<PolicyEngine>,
        limits: AiLimits,
        semaphore: Arc<tokio::sync::Semaphore>,
        rate_limiter: Arc<RateLimiter>,
        model: impl Into<String>,
    ) -> Self {
        Self {
            db,
            provider,
            policy,
            limits,
            semaphore,
            rate_limiter,
            model: model.into(),
        }
    }

    /// Ask for a proposal. `subject` is the address being configured — the
    /// policy target only; it is never put in the prompt.
    ///
    /// # Errors
    ///
    /// [`Error::FailedPrecondition`] if policy, the spend cap, or the model's
    /// own lack of confidence rules the call out;
    /// [`Error::ResourceExhausted`] on a budget hard cap;
    /// [`Error::DeadlineExceeded`] if `cancel` fires while waiting for
    /// capacity; otherwise whatever the provider returned.
    #[tracing::instrument(skip(self, evidence, cancel), fields(domain = %evidence.domain), err)]
    pub async fn infer(
        &self,
        subject: &str,
        evidence: &Evidence,
        cancel: &CancellationToken,
    ) -> Result<RawCandidate, Error> {
        // Policy, then the daily cap, then the budget — `ai::gate` owns the
        // order, and this call has no account to attribute to.
        let model =
            gate::admit_unattributed(&self.db, &self.policy, &self.limits, subject, &self.model)
                .await?;
        // Only now: a permit held across a refused call is capacity taken from
        // work that would have been allowed.
        let _permit = gate::acquire_capacity(&self.semaphore, &self.rate_limiter, cancel).await?;

        let request = ChatRequest::new(model, MAX_TOKENS)
            .system(injection::with_data_boundary(SYSTEM_PROMPT))
            .user(render_evidence(evidence))
            .output_format(OutputFormat::json_schema(schema()));

        let started = Instant::now();
        let payload = payload_bytes(&request);
        let response = self.provider.complete(&request, cancel).await;
        match &response {
            Ok(ok) => {
                self.audit(
                    &request.model,
                    &payload,
                    started.elapsed(),
                    Some(ok.usage),
                    CallOutcome::Ok,
                )
                .await;
            }
            Err(error) => {
                self.audit(
                    &request.model,
                    &payload,
                    started.elapsed(),
                    None,
                    CallOutcome::Error(error.to_string()),
                )
                .await;
            }
        }
        let proposal: Proposal = response?.structured()?;

        if !proposal.confident {
            return Err(Error::not_found(format!(
                "no configuration could be discovered for {} and the model would not guess",
                evidence.domain
            )));
        }
        // Handed back as strings, in the same shape a probe produces, so it
        // goes through exactly the same validation. A pre-validated struct
        // here would be a second door into the settings type.
        Ok(RawCandidate {
            source: Source::Model,
            imap: RawServer {
                host: proposal.imap_host,
                port: proposal.imap_port.to_string(),
                security: proposal.imap_security,
                username: None,
            },
            smtp: Some(RawServer {
                host: proposal.smtp_host,
                port: proposal.smtp_port.to_string(),
                security: proposal.smtp_security,
                username: None,
            }),
        })
    }

    /// Record the call in the AI ledger.
    ///
    /// `record_call` (interactive), not `record_call_charged`: this is
    /// somebody waiting at a prompt for one answer, which is exactly what
    /// `WorkClass::Interactive` means — and the ledger is what makes the spend
    /// visible to the budget the next call is checked against.
    async fn audit(
        &self,
        model: &str,
        payload: &[u8],
        latency: Duration,
        usage: Option<crate::ai::provider::Usage>,
        outcome: CallOutcome,
    ) {
        let record = CallRecord {
            // No account: this call runs before one exists, which is exactly
            // what a `None` here says.
            account_id: None,
            message_id: None,
            request_id: None,
            model: model.to_owned(),
            pass: Some(PASS.to_owned()),
            usage: usage.unwrap_or_default(),
            // Nothing was redacted because nothing redactable was ever in the
            // payload — see the module docs.
            redaction_level: "none".to_owned(),
            latency,
            payload,
            outcome,
        };
        if let Err(error) = record_call(&self.db, record).await {
            tracing::warn!(%error, "could not write the autoconfig audit entry");
        }
    }
}

/// Render the evidence as fenced, labelled untrusted blocks.
fn render_evidence(evidence: &Evidence) -> String {
    let mut out = String::new();
    out.push_str(&injection::untrusted_block("mail-domain", &evidence.domain));
    out.push('\n');
    out.push_str(&injection::untrusted_block(
        "mx-records",
        &evidence.mx.join("\n"),
    ));
    for response in evidence.responses.iter().take(MAX_EVIDENCE_ITEMS) {
        let body: String = response.body.chars().take(MAX_EVIDENCE_CHARS).collect();
        out.push('\n');
        out.push_str(&injection::untrusted_block(
            response.probe,
            &format!("url: {}\nstatus: {}\n{body}", response.url, response.status),
        ));
    }
    out.push_str(
        "\n\nInfer the IMAP and SMTP settings for this domain, following the rules above.",
    );
    out
}

/// The JSON schema the answer must satisfy.
fn schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "imap_host": { "type": "string" },
            "imap_port": { "type": "integer" },
            "imap_security": { "type": "string", "enum": ["tls", "starttls"] },
            "smtp_host": { "type": "string" },
            "smtp_port": { "type": "integer" },
            "smtp_security": { "type": "string", "enum": ["tls", "starttls"] },
            "confident": { "type": "boolean" }
        },
        "required": [
            "imap_host", "imap_port", "imap_security",
            "smtp_host", "smtp_port", "smtp_security", "confident"
        ],
        "additionalProperties": false
    })
}
