//! The Claude listwise reranker (prd.md Stage 5, backend 2): "send the top
//! ~30 candidates (subject + snippet + key metadata, redacted) to
//! `claude-haiku-4-5`/`claude-sonnet-5` and ask for a listwise ordering
//! **plus a one-line 'why this matched'** per result."
//!
//! # Listwise, not pointwise
//!
//! The model is shown every candidate at once and asked for an ordering, not
//! asked to score each candidate independently. That is the whole reason this
//! backend can beat the local cross-encoder: relevance in a mailbox is
//! comparative ("the invoice, not the invoice reminder"), and a pointwise
//! score has nothing to compare against. It is also why the candidate count
//! is capped (`search.reranker.claude_max_candidates`, default 30) — the
//! prompt has to hold every candidate's text at once.
//!
//! # Positional labels, never message ids
//!
//! Candidates are labelled `[1]`, `[2]`, ... and the model answers in those
//! labels; [`ClaudeReranker`] maps them back to `messages.id` itself. Two
//! reasons, both load-bearing:
//!
//! - A row id is an unbounded integer, and [`crate::ai::redact`] scans for
//!   digit runs (card numbers, phone numbers). A long id could be tokenized
//!   mid-prompt and come back as a token the model echoed, which is a
//!   response this backend would have to guess about.
//! - The model never needs the id. Nothing it can say about `messages.id`
//!   is more useful than "the fourth one," and not sending it keeps the
//!   local schema out of a third party's context entirely.
//!
//! # Order of operations, and why it is not negotiable
//!
//! concurrency permit → RPM → cost gate → budget → cache → **redact** →
//! provider → audit. The AI *policy* decision (whether this mail may leave
//! the host at all) is [`super::L2Stage`]'s, one layer up, because only it
//! knows which accounts and folders the candidates came from.
//!
//! This is [`crate::ai::queue::AiWorkerPool::process_one`]'s order, and each
//! position is load-bearing for a reason that module documents:
//!
//! - Concurrency and pacing come **before** the budget check, not after. A
//!   check taken before an unbounded wait can be arbitrarily stale by the
//!   time the call is actually made; what bounds the overshoot is how many
//!   checks can be outstanding at once, which is exactly what the shared
//!   semaphore caps.
//! - The cache is consulted after the budget resolves a model, because a soft
//!   cap can *downgrade* it and a cached ordering is only valid for the model
//!   that produced it.
//! - Redaction is the last thing before the call, so nothing can reintroduce
//!   raw text afterwards, and the audit entry records the *redacted* payload
//!   because that is what was actually transmitted.
//!
//! # Parse first, rehydrate per field
//!
//! [`crate::ai::triage`] documents a real hazard in the queue's dispatch
//! tail: it rehydrates the model's whole response *before* parsing, so a
//! redacted value containing a `"` can turn valid JSON into invalid JSON.
//! This backend owns both ends of its own call, so it does the safe thing
//! instead — `serde_json` parses the raw response, and
//! [`crate::ai::rehydrate`] is applied to each `why` string individually,
//! after parsing. A PII-bearing reason is restored for the user; a quote
//! inside it can no longer break the parse of everything else.
//!
//! # Failure is never fatal
//!
//! Every error path here returns `Err` and [`super::L2Stage`] turns that into
//! "keep the L1 order." Nothing in this module can fail a search: not a
//! spend cap, not a provider outage, not a malformed answer, not a model that
//! returns six results for thirty candidates.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use serde::Deserialize;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use super::cache::{CacheKey, RerankCache};
use super::{RerankCandidate, RerankVerdict, Reranker};
use crate::ai::audit::{record_call, CallOutcome, CallRecord};
use crate::ai::budget::{
    BudgetEnforcer, BudgetRequest, BudgetVerdict, WorkClass, GLOBAL_ACCOUNT_ID,
};
use crate::ai::provider::{ChatRequest, OutputFormat, Provider};
use crate::ai::queue::{payload_bytes, CapDecision, CostGate, RateLimiter};
use crate::ai::redact::{guard, rehydrate, GuardedRequest};
use crate::config::{AiLimits, AiPrivacy, RerankerConfig};
use crate::error::Error;
use crate::storage::Database;

/// The backend name recorded in `ai_ledger.pass` and in this stage's tracing
/// fields.
pub const PASS: &str = "search_rerank";

/// The system prompt. Deliberately terse and deliberately explicit about the
/// two failure modes a listwise reranker actually has: inventing labels, and
/// quietly dropping candidates it did not rank.
const SYSTEM: &str = "You re-rank email search results for relevance to a user's query. \
     You are given a query and a numbered list of candidate messages. \
     Return every candidate exactly once, ordered best-match first, with a \
     one-line reason (at most 12 words) explaining why that message matches \
     the query. Use only the numeric labels you were given; never invent a \
     label and never omit one. Judge relevance to the query only — not \
     recency, importance, or how well written the message is.";

/// The Claude-backed listwise reranker.
pub struct ClaudeReranker {
    provider: Arc<dyn Provider>,
    db: Database,
    model: String,
    max_tokens: u32,
    limits: AiLimits,
    privacy: AiPrivacy,
    /// `ai.privacy.max_body_chars`, applied to each candidate's document
    /// again here. [`super::document`] already cuts bodies to a much smaller
    /// budget for both backends, so this only bites when an operator has
    /// tightened the privacy setting *below* that — which is exactly the case
    /// where silently overriding it would be wrong.
    max_body_chars: usize,
    cache: RerankCache,
    /// `ai.limits.max_concurrency`, **shared** with
    /// [`crate::ai::queue::AiWorkerPool`] rather than a second semaphore of
    /// this backend's own. A search rerank and a queued triage call draw from
    /// one concurrency budget; two independent ones would let the process
    /// exceed the configured ceiling in practice, which is the same reasoning
    /// `AiWorkerPool::semaphore`'s own doc comment gives for sharing it with
    /// `rmaild::AiApi`.
    semaphore: Arc<Semaphore>,
    /// `ai.limits.requests_per_minute`, shared for the identical reason:
    /// Anthropic's own rate limiter sees one process, not three call sites.
    rate_limiter: Arc<RateLimiter>,
}

impl std::fmt::Debug for ClaudeReranker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeReranker")
            .field("model", &self.model)
            .field("cached", &self.cache.len())
            .finish_non_exhaustive()
    }
}

impl ClaudeReranker {
    /// Build the backend over an already-constructed provider.
    ///
    /// The provider is injected rather than built here so the daemon has
    /// exactly one Anthropic client (and one API-key resolution) for the
    /// whole process, and so tests can drive this against a mock without a
    /// network.
    ///
    /// `semaphore`/`rate_limiter` come from the daemon's one
    /// [`crate::ai::queue::AiWorkerPool`]-shaped pair (see the fields' own
    /// docs) so this path draws on the same `ai.limits` budget every other
    /// provider caller does.
    #[must_use]
    pub fn new(
        provider: Arc<dyn Provider>,
        db: Database,
        config: &RerankerConfig,
        limits: AiLimits,
        privacy: AiPrivacy,
        semaphore: Arc<Semaphore>,
        rate_limiter: Arc<RateLimiter>,
    ) -> Self {
        Self {
            provider,
            db,
            model: config.claude_model.clone(),
            max_tokens: config.claude_max_tokens.max(256),
            limits,
            max_body_chars: privacy.max_body_chars as usize,
            privacy,
            cache: RerankCache::new(config.claude_cache_entries as usize),
            semaphore,
            rate_limiter,
        }
    }

    /// Consult the daemon-wide spend cap and this call's own budget, and
    /// return the model to actually use.
    ///
    /// # Errors
    ///
    /// [`Error::ResourceExhausted`] when either says no — which
    /// [`super::L2Stage`] degrades to the L1 order, exactly as prd.md's
    /// "degrades to the L1 order on error/budget" requires.
    async fn budgeted_model(&self) -> Result<String, Error> {
        let gate = CostGate {
            db: &self.db,
            limits: &self.limits,
        };
        match gate.decide().await? {
            CapDecision::Open => {}
            other => {
                return Err(Error::resource_exhausted(format!(
                    "the AI spend cap is closed ({other:?}); search rerank degrades to the L1 order"
                )));
            }
        }

        // Charged to the global budget rather than to a per-account one: a
        // search spans every configured account by default, so there is no
        // single account this call is "for." `WorkClass::Interactive` is what
        // `record_call` below attributes it as, so the check and the charge
        // agree — the same discipline `rmaild::AiApi` applies to a forced
        // analysis.
        let verdict = BudgetEnforcer {
            db: &self.db,
            limits: &self.limits,
        }
        .evaluate(&BudgetRequest {
            account_id: GLOBAL_ACCOUNT_ID,
            model: &self.model,
            work_class: WorkClass::Interactive,
            now: chrono::Utc::now().timestamp(),
        })
        .await?;
        match verdict {
            BudgetVerdict::Allow => Ok(self.model.clone()),
            BudgetVerdict::Downgrade { model, reason } => {
                tracing::info!(
                    from = %self.model,
                    to = %model,
                    reason = %reason,
                    "ai budget soft cap: downgrading the listwise rerank model"
                );
                Ok(model)
            }
            BudgetVerdict::Block { reason, .. } => Err(Error::resource_exhausted(format!(
                "an AI spend budget is exhausted ({reason}); search rerank degrades to the \
                 L1 order"
            ))),
        }
    }
}

#[async_trait]
impl Reranker for ClaudeReranker {
    fn name(&self) -> &'static str {
        "claude"
    }

    fn needs_network(&self) -> bool {
        true
    }

    #[tracing::instrument(
        skip(self, query, candidates, cancel),
        fields(
            backend = "claude",
            candidates = candidates.len(),
            model = %self.model,
            cache_key,
            cached,
            elapsed_ms,
        )
    )]
    async fn rerank(
        &self,
        query: &str,
        candidates: &[RerankCandidate],
        cancel: &CancellationToken,
    ) -> Result<Vec<RerankVerdict>, Error> {
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<i64> = candidates.iter().map(|c| c.message_id).collect();
        let span = tracing::Span::current();

        // Concurrency and pacing first, then budget, then the call — the
        // order `ai::queue::worker::process_one` documents at length: a
        // budget check taken *before* an unbounded wait can be arbitrarily
        // stale by the time the call is made, and what bounds the overshoot
        // is how many checks can be outstanding at once, which is exactly
        // what the semaphore caps.
        let _permit = tokio::select! {
            () = cancel.cancelled() => {
                return Err(Error::deadline_exceeded(
                    "cancelled while waiting for rerank capacity".to_owned(),
                ));
            }
            permit = Arc::clone(&self.semaphore).acquire_owned() => permit.map_err(|_| {
                Error::internal("the ai concurrency semaphore was closed".to_owned())
            })?,
        };
        tokio::select! {
            () = cancel.cancelled() => {
                return Err(Error::deadline_exceeded(
                    "cancelled while waiting for rerank rate-limit capacity".to_owned(),
                ));
            }
            () = self.rate_limiter.acquire() => {}
        }

        let model = self.budgeted_model().await?;

        // Keyed on the model that will *actually* be called, not the
        // configured one: `budgeted_model` may have downgraded under a soft
        // cap, and caching a cheap model's ordering under the expensive
        // model's key would replay it long after the cap lifted.
        let key = CacheKey::new(&model, query, &ids);
        span.record("cache_key", key.short());
        if let Some(hit) = self.cache.get(&key) {
            span.record("cached", true);
            return Ok(hit);
        }
        span.record("cached", false);

        let request = ChatRequest::new(model, self.max_tokens)
            .system(SYSTEM)
            .user(prompt(query, candidates, self.max_body_chars))
            .output_format(OutputFormat::json_schema(schema()));

        // The firewall. Nothing between here and `provider.complete` may add
        // text to the request — see the module docs.
        let GuardedRequest::Redacted {
            request, tokens, ..
        } = guard(&request, &self.privacy)
        else {
            return Err(Error::failed_precondition(
                "nothing was left to rerank once PII was redacted from the candidates".to_owned(),
            ));
        };
        let payload = payload_bytes(&request);
        let redaction_level = if tokens.is_empty() {
            "none"
        } else {
            "redacted"
        };

        let started = Instant::now();
        let response = self.provider.complete(&request, cancel).await;
        let latency = started.elapsed();
        span.record("elapsed_ms", latency.as_millis());

        let response = match response {
            Ok(response) => response,
            Err(error) => {
                // Audited even though it failed: prd.md's ledger is a record
                // of what left the machine, and a failed call still consumed
                // an attempt (and, on a mid-stream failure, real tokens).
                self.audit(
                    &request.model,
                    &payload,
                    redaction_level,
                    latency,
                    None,
                    CallOutcome::Error(error.to_string()),
                )
                .await;
                return Err(error);
            }
        };

        let usage = response.usage;
        let request_id = response.id.clone();
        let parsed = parse(&response.text, candidates, &tokens);
        self.audit(
            &request.model,
            &payload,
            redaction_level,
            latency,
            Some((request_id, usage)),
            match &parsed {
                Ok(_) => CallOutcome::Ok,
                Err(error) => CallOutcome::Error(error.to_string()),
            },
        )
        .await;
        let verdicts = parsed?;
        self.cache.insert(key, verdicts.clone());
        Ok(verdicts)
    }
}

impl ClaudeReranker {
    /// Write one ledger row. Never propagates: an audit write that fails must
    /// not turn a usable rerank into a degraded search, and the failure is
    /// logged where an operator will see it.
    async fn audit(
        &self,
        model: &str,
        payload: &[u8],
        redaction_level: &str,
        latency: std::time::Duration,
        response: Option<(String, crate::ai::provider::Usage)>,
        outcome: CallOutcome,
    ) {
        let (request_id, usage) = match response {
            Some((id, usage)) => (Some(id), usage),
            None => (None, crate::ai::provider::Usage::default()),
        };
        let record = CallRecord {
            account_id: None,
            // A rerank is about a *query*, not a message: attributing it to
            // one of the thirty candidates would make `mail ai audit
            // --message <id>` claim a call was made "for" a message that
            // merely appeared in someone else's result list.
            message_id: None,
            request_id,
            model: model.to_owned(),
            pass: Some(PASS.to_owned()),
            usage,
            redaction_level: redaction_level.to_owned(),
            latency,
            payload,
            outcome,
        };
        if let Err(error) = record_call(&self.db, record).await {
            tracing::warn!(%error, "could not write the rerank audit entry");
        }
    }
}

/// The user turn: the query, then one labelled block per candidate, each cut
/// to `max_chars` (`ai.privacy.max_body_chars`).
fn prompt(query: &str, candidates: &[RerankCandidate], max_chars: usize) -> String {
    let mut out = String::with_capacity(candidates.len() * 512);
    out.push_str("Query: ");
    out.push_str(query);
    out.push_str("\n\nCandidates:\n");
    for (index, candidate) in candidates.iter().enumerate() {
        out.push_str(&format!("\n[{}]\n", index + 1));
        // By `char`, not by byte: slicing a UTF-8 string at an arbitrary byte
        // offset panics, and mail is full of multi-byte text.
        let document = match candidate.document.char_indices().nth(max_chars) {
            Some((cut, _)) => candidate.document.get(..cut).unwrap_or_default(),
            None => candidate.document.as_str(),
        };
        out.push_str(document);
        out.push('\n');
    }
    out.push_str(&format!(
        "\nRank all {} candidates, best match first.\n",
        candidates.len()
    ));
    out
}

/// The structured-output schema. `additionalProperties: false` and the
/// `required` list are what make [`parse`] a total function of a
/// schema-conforming answer rather than a hopeful `serde` call.
fn schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "results": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "label": {
                            "type": "integer",
                            "description":
                                "The candidate's numeric label, exactly as given in the prompt.",
                        },
                        "why": {
                            "type": "string",
                            "description":
                                "One line, at most 12 words, on why this matches the query.",
                        },
                    },
                    "required": ["label", "why"],
                    "additionalProperties": false,
                },
            },
        },
        "required": ["results"],
        "additionalProperties": false,
    })
}

#[derive(Debug, Deserialize)]
struct RawResponse {
    results: Vec<RawResult>,
}

#[derive(Debug, Deserialize)]
struct RawResult {
    label: i64,
    #[serde(default)]
    why: String,
}

/// Turn one listwise answer into verdicts, best-first.
///
/// The model is asked for a permutation and this does not assume it produced
/// one. Out-of-range and repeated labels are dropped, and any candidate the
/// answer never mentioned is appended in its incoming (L1) order — so a
/// truncated or partially hallucinated answer degrades to "the part the model
/// did rank, then the rest as they were," never to a short page.
///
/// # Errors
///
/// [`Error::Internal`] if the response is not valid JSON for the schema, or
/// if it named no usable label at all — at that point there is no ordering to
/// apply and the caller should keep the L1 order rather than apply a
/// meaningless one.
fn parse(
    text: &str,
    candidates: &[RerankCandidate],
    tokens: &crate::ai::redact::TokenMap,
) -> Result<Vec<RerankVerdict>, Error> {
    let raw: RawResponse = serde_json::from_str(text).map_err(|e| {
        Error::internal(format!(
            "claude's listwise rerank did not match the requested schema: {e}"
        ))
    })?;

    let mut seen: BTreeSet<usize> = BTreeSet::new();
    let mut ordered: Vec<RerankVerdict> = Vec::with_capacity(candidates.len());
    for entry in raw.results {
        let Ok(label) = usize::try_from(entry.label) else {
            continue;
        };
        let Some(index) = label.checked_sub(1) else {
            continue;
        };
        let Some(candidate) = candidates.get(index) else {
            continue;
        };
        if !seen.insert(index) {
            continue;
        }
        // Scores are assigned from the *position* the model chose, not from
        // anything it said: a listwise answer is an ordering, and inventing a
        // magnitude for it would imply a confidence the model never
        // expressed. `super::L2Stage` only ever reads the relative order.
        let score = candidates.len().saturating_sub(ordered.len()) as f64;
        let why = rehydrate(entry.why.trim(), tokens);
        ordered.push(RerankVerdict {
            message_id: candidate.message_id,
            score,
            why: (!why.is_empty()).then_some(why),
        });
    }

    if ordered.is_empty() {
        return Err(Error::internal(
            "claude's listwise rerank named no candidate this query had".to_owned(),
        ));
    }

    let unranked = candidates.len().saturating_sub(ordered.len());
    if unranked > 0 {
        tracing::warn!(
            unranked,
            total = candidates.len(),
            "the listwise rerank omitted candidates; they keep their L1 order below the ranked ones"
        );
        for (index, candidate) in candidates.iter().enumerate() {
            if seen.contains(&index) {
                continue;
            }
            let score = candidates.len().saturating_sub(ordered.len()) as f64;
            ordered.push(RerankVerdict {
                message_id: candidate.message_id,
                score,
                why: None,
            });
        }
    }
    Ok(ordered)
}
