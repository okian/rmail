//! The triage pass (task 48): the cheap, always-run first pass over every
//! newly synced message.
//!
//! This module is a [`PassHandler`] and nothing else. Everything about *how*
//! a call happens — leasing the job, resolving policy, assembling bounded
//! content, running it through the redaction firewall, pacing and bounding
//! concurrency, calling the provider, and writing the audit ledger entry —
//! belongs to [`crate::ai::queue`] (task 47) and is not reimplemented here.
//! [`TriagePassHandler`] answers exactly the two questions a
//! [`PassHandler`] owns: what request to send for a given message
//! ([`TriagePassHandler::build_request`]), and what to do with the answer
//! once the queue has redacted, dispatched, and audited it
//! ([`TriagePassHandler::on_success`]).
//!
//! # Structured output, not regex
//!
//! [`build_request`](TriagePassHandler::build_request) attaches
//! [`OutputFormat::json_schema`] to every request. The Anthropic Messages
//! API guarantees a response built this way is valid JSON matching
//! [`schema`] — [`ChatResponse::structured`](crate::ai::provider::ChatResponse::structured)
//! (or, as here, a direct `serde_json::from_str`, since [`PassHandler::on_success`]
//! only ever sees the already-rehydrated text, not a [`ChatResponse`]) is
//! how the answer is read, never a pattern match over free-form prose.
//!
//! # A schema-invalid answer is a hard error, not a partial row
//!
//! [`TriageResult::parse`] is the *only* way this module turns model text
//! into data, and it either returns a complete, validated
//! [`TriageResult`] or an [`Error::Internal`] — there is no code path that
//! writes some columns and leaves others blank. Per [`PassHandler::on_success`]'s
//! own contract, returning `Err` here is what lets
//! [`crate::ai::queue::AiQueue::fail`] back the job off and eventually
//! quarantine it to `dead`, exactly the "the queue can dead-letter it"
//! behavior the acceptance criterion asks for — and because the write to
//! [`write_summary`] never runs until parsing and validation both succeed,
//! a failed parse leaves `ai_summaries` for this message exactly as it was
//! before the call, not half-written.
//!
//! # Why `on_success` looks up `thread_id` itself
//!
//! [`crate::ai::queue::MessageContent`] — what
//! [`build_request`](TriagePassHandler::build_request) is handed — does
//! not carry `thread_id`: it is bounded, policy-safe *content*, and a
//! thread id is neither bounded nor sensitive, so including it there would
//! mix concerns for every other [`PassHandler`] this trait might ever grow.
//! [`AiLease`] does not carry it either — a lease is queue bookkeeping,
//! not message metadata. So [`thread_id_for`] reads it directly off
//! `messages` when a result is ready to persist, the one place this module
//! actually needs it.
//!
//! # Two known, narrow races this module does not close
//!
//! Both are left open deliberately rather than papered over, because
//! closing either properly is a [`crate::ai::queue`] (task 47) API change,
//! not something this handler can fix on its own without reaching into that
//! module's internal `ai_queue` state strings:
//!
//! - **A reaped worker can still win the write.** The queue's dispatch tail
//!   (`ai::queue::worker::finish_call`, `pub(super)` — not linkable from
//!   here) calls [`PassHandler::on_success`] *before*
//!   [`crate::ai::queue::AiQueue::complete`], and only `complete` is
//!   lease-fenced. If a call runs past [`crate::ai::queue::DEFAULT_LEASE`],
//!   the reaper can return the job to `pending`, a second worker can lease,
//!   call, and write it first, and then the first (stale) call can finally
//!   return and overwrite that fresher verdict via the same
//!   `(message_id, pass, model)` upsert [`write_summary`] uses. The row
//!   stays internally consistent (never a partial write — see above) but
//!   can end up holding the *older* of two verdicts, with `ai_queue`
//!   recording the newer one's `ledger_entry_id` as the job's completion.
//!   Narrow (it needs a call to outrun a 5-minute lease) and not a
//!   correctness hazard beyond staleness, but real.
//! - **Rehydration runs over the whole response text, not per field.** The
//!   same dispatch tail calls [`crate::ai::redact::rehydrate`] on the
//!   model's entire JSON response before this handler ever sees it. If a
//!   redacted value the model
//!   echoed back (a name, an `api_key`-pattern secret) contains a `"` or
//!   `\`, rehydrating it in place can turn valid JSON into invalid JSON —
//!   [`TriageResult::parse`] then fails every attempt for that one message,
//!   deterministically, until [`crate::ai::queue::AiQueue::revive`] and a
//!   different response happen to avoid the same value. The redaction
//!   firewall itself is unaffected (no raw PII ever reaches the provider);
//!   this is a structured-output availability gap, not a privacy one. The
//!   real fix is [`PassHandler::on_success`] receiving the token map
//!   alongside the *redacted* text and letting a handler parse first,
//!   rehydrate per field — out of scope for this module to change.
use async_trait::async_trait;
use rusqlite::OptionalExtension;
use serde::Deserialize;

use crate::ai::deep::DeepPassGate;
use crate::ai::provider::{ChatRequest, OutputFormat};
use crate::ai::queue::{AiLease, MessageContent, NewAiJob, PassHandler};
use crate::error::Error;
use crate::storage::Database;

/// The wire value of `ai_queue.pass` / `ai_ledger.pass` / `ai_summaries.pass`
/// this handler answers to.
pub const PASS: &str = "triage";

/// `ai_summaries.schema_version` for the shape [`schema`] describes today.
/// Bump this — and only this — if the JSON schema's fields ever change, so a
/// row written under an older shape stays distinguishable from one written
/// under this one.
const SCHEMA_VERSION: i64 = 1;

/// Generous for a small structured-JSON answer; triage prompts are
/// deliberately terse (no `key_points`/`todos`/`entities` — those are the
/// deep pass, task 49) so this ceiling is rarely approached.
const DEFAULT_MAX_TOKENS: u32 = 1024;

const CATEGORIES: [&str; 8] = [
    "personal",
    "work",
    "newsletter",
    "receipt",
    "invoice",
    "notification",
    "spam",
    "other",
];

/// `pub(crate)`, not private: [`crate::ai::deep`]'s own priority-threshold
/// gating (`DeepPassGate`/`priority_at_least`) ranks a triage verdict's
/// `priority` field against an operator-configured threshold, and must rank
/// it against exactly this vocabulary — a second, hand-maintained copy would
/// silently drift the moment this one gains or renames a value.
pub(crate) const PRIORITIES: [&str; 4] = ["low", "normal", "high", "critical"];

const SENTIMENTS: [&str; 4] = ["positive", "neutral", "negative", "urgent"];

/// The prompt's own "zero to five" instruction, enforced in
/// [`TriageResult::parse`] since the schema itself cannot express it (see
/// that function's docs).
const MAX_SUGGESTED_TAGS: usize = 5;

/// Frozen, cacheable system prompt — kept byte-identical across calls so it
/// forms the stable prefix `ClaudeProvider`'s prompt-cache `cache_control`
/// covers (see [`ChatRequest::system`](crate::ai::provider::ChatRequest::system)'s
/// own docs on why that matters). Everything that varies per call — the
/// message itself — belongs in the user turn, never here.
const SYSTEM_PROMPT: &str = "You are the triage stage of an email client's AI pipeline. You read \
one email at a time and answer with a single structured JSON object only \
-- no prose, no markdown, nothing outside the schema.

Classify the message:
- category: the single best fit from personal, work, newsletter, receipt, \
invoice, notification, spam, other.
- priority: low, normal, high, or critical, judged by how much attention \
this message plausibly deserves -- a marketing newsletter is low, an \
outage notice or a message from a manager is high or critical.
- sentiment: positive, neutral, negative, or urgent -- the tone of the \
message itself, not your opinion of it.
- needs_reply: true only if a reasonable recipient would be expected to \
write back (a question, a request, an invitation) -- false for \
notifications, receipts, and mail that is purely informational.
- suggested_tags: zero to five short, lowercase, single-word-or-hyphenated \
tags a mail client could apply automatically (e.g. \"travel\", \
\"follow-up\"). Do not invent a tag for information the category already \
captures.
- tl_dr: one plain sentence, well under 140 characters, that lets someone \
decide whether to open this message without reading it.

Base every field only on the message content given to you. If the body \
looks redacted or truncated, judge from what remains rather than guessing \
at what was removed.";

/// The triage pass's [`PassHandler`].
///
/// Cheap to clone/share: [`Database`] already is (see its own docs), and
/// `model` is a short owned string. One instance is registered with
/// [`crate::ai::queue::AiWorkerPool::new`] and, for backlog/offline-gap
/// catch-up, [`crate::ai::queue::BatchCoordinator::new`] — both call
/// [`PassHandler::build_request`]/[`PassHandler::on_success`] identically,
/// so a message triaged live and one triaged via a Message Batches
/// submission are written the same way.
#[derive(Debug, Clone)]
pub struct TriagePassHandler {
    db: Database,
    model: String,
    max_tokens: u32,
    /// Enqueues a deep pass once a triage verdict this handler just wrote
    /// qualifies under `ai.deep_pass`'s thresholds — see
    /// [`crate::ai::deep::DeepPassGate`]. `None` when no gate was
    /// configured (every call site before task 49, and any test that only
    /// cares about the triage verdict itself): `on_success` simply skips
    /// the check, exactly as if this field did not exist.
    deep_gate: Option<DeepPassGate>,
}

impl TriagePassHandler {
    /// A handler that queries `model` (typically `ai.models.triage`, e.g.
    /// `claude-haiku-4-5`) and writes its answers into `db`.
    #[must_use]
    pub fn new(db: Database, model: impl Into<String>) -> Self {
        Self {
            db,
            model: model.into(),
            max_tokens: DEFAULT_MAX_TOKENS,
            deep_gate: None,
        }
    }

    /// Override the default output token ceiling — mainly for tests that
    /// want a tight bound.
    #[must_use]
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// Wire this handler to enqueue a deep pass whenever a triage verdict
    /// it just wrote qualifies under `gate`'s configured thresholds — what
    /// the daemon dispatch loop must call once it registers both this
    /// handler and [`crate::ai::deep::DeepPassHandler`] with the same
    /// [`crate::ai::queue::AiWorkerPool`]. Triage itself needs to know
    /// nothing about the deep pass beyond "call this when done" — see
    /// [`crate::ai::deep`]'s module docs for why gating lives there, not
    /// here.
    #[must_use]
    pub fn with_deep_pass_gate(mut self, gate: DeepPassGate) -> Self {
        self.deep_gate = Some(gate);
        self
    }
}

#[async_trait]
impl PassHandler for TriagePassHandler {
    fn pass(&self) -> &str {
        PASS
    }

    async fn build_request(&self, content: &MessageContent) -> Result<ChatRequest, Error> {
        Ok(ChatRequest::new(self.model.clone(), self.max_tokens)
            .system(SYSTEM_PROMPT)
            .user(render_user_message(content))
            .output_format(OutputFormat::json_schema(schema())))
    }

    #[tracing::instrument(
        skip(self, lease, text),
        fields(message_id = lease.message_id, category, priority, needs_reply, deep_queued)
    )]
    async fn on_success(
        &self,
        lease: &AiLease,
        text: &str,
        ledger_entry_id: i64,
    ) -> Result<(), Error> {
        let result = TriageResult::parse(text)?;
        let span = tracing::Span::current();
        span.record("category", tracing::field::display(&result.category));
        span.record("priority", tracing::field::display(&result.priority));
        span.record("needs_reply", result.needs_reply);
        let thread_id = thread_id_for(&self.db, lease.message_id).await?;
        let deep_queued = write_summary(
            &self.db,
            lease,
            thread_id,
            &self.model,
            &result,
            ledger_entry_id,
            self.deep_gate.as_ref(),
        )
        .await?;
        span.record("deep_queued", deep_queued);
        tracing::debug!("triage verdict written");
        Ok(())
    }
}

/// The user turn: everything about the message the model needs to triage
/// it, and nothing it does not — `content` is already bounded and
/// policy-safe by the time this runs (see [`crate::ai::queue::assemble_content`]),
/// and by the time it reaches [`Provider::complete`](crate::ai::provider::Provider::complete)
/// it will also have been through the redaction firewall; this function's
/// only job is to render it into readable text.
fn render_user_message(content: &MessageContent) -> String {
    let from = match (&content.from_name, &content.from_addr) {
        (Some(name), Some(addr)) => format!("{name} <{addr}>"),
        (Some(name), None) => name.clone(),
        (None, Some(addr)) => addr.clone(),
        (None, None) => "(unknown sender)".to_owned(),
    };
    let subject = content.subject.as_deref().unwrap_or("(no subject)");
    let mut out = format!("From: {from}\nSubject: {subject}\n\n{}", content.body);
    if content.truncated {
        out.push_str("\n\n[body truncated]");
    }
    out
}

/// The JSON Schema every triage request constrains its response to. Kept
/// byte-stable across calls for the same reason [`SYSTEM_PROMPT`] is: a
/// request whose shape changes every call gets no benefit from prompt
/// caching regardless of which bytes are technically covered by
/// `cache_control` (see [`ChatRequest::system`](crate::ai::provider::ChatRequest::system)'s
/// docs).
fn schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "category": {"type": "string", "enum": CATEGORIES},
            "priority": {"type": "string", "enum": PRIORITIES},
            "needs_reply": {"type": "boolean"},
            "sentiment": {"type": "string", "enum": SENTIMENTS},
            "suggested_tags": {"type": "array", "items": {"type": "string"}},
            "tl_dr": {"type": "string"},
        },
        "required": [
            "category",
            "priority",
            "needs_reply",
            "sentiment",
            "suggested_tags",
            "tl_dr",
        ],
        "additionalProperties": false,
    })
}

/// The triage pass's structured answer, once parsed and validated.
#[derive(Debug, Clone, PartialEq, Deserialize)]
struct TriageResult {
    category: String,
    priority: String,
    needs_reply: bool,
    sentiment: String,
    suggested_tags: Vec<String>,
    tl_dr: String,
}

impl TriageResult {
    /// Parse and validate one triage response.
    ///
    /// The Messages API's `output_config.format` guarantees `text` is valid
    /// JSON matching [`schema`]'s shape — but `enum` membership for
    /// `category`/`priority`/`sentiment` is a claim about *values*, and this
    /// still checks it explicitly rather than trusting the guarantee blind:
    /// an API-side regression that let an out-of-vocabulary value through
    /// must surface as a loud, dead-letterable error here, not silently
    /// become a row `retrieve::filtermask`'s `ai:priority>high`/`ai:category:invoice`
    /// predicates can never match.
    ///
    /// # Errors
    /// [`Error::Internal`] if `text` is not valid JSON for [`TriageResult`],
    /// or any enum field holds a value outside its documented vocabulary.
    /// Never a partial result — this either returns a value with every field
    /// populated, or nothing at all.
    fn parse(text: &str) -> Result<Self, Error> {
        let mut parsed: Self = serde_json::from_str(text).map_err(|e| {
            Error::internal(format!(
                "triage structured output did not match the requested schema: {e}"
            ))
        })?;
        if !CATEGORIES.contains(&parsed.category.as_str()) {
            return Err(Error::internal(format!(
                "triage returned an unrecognized category {:?}",
                parsed.category
            )));
        }
        if !PRIORITIES.contains(&parsed.priority.as_str()) {
            return Err(Error::internal(format!(
                "triage returned an unrecognized priority {:?}",
                parsed.priority
            )));
        }
        if !SENTIMENTS.contains(&parsed.sentiment.as_str()) {
            return Err(Error::internal(format!(
                "triage returned an unrecognized sentiment {:?}",
                parsed.sentiment
            )));
        }
        // The prompt asks for "zero to five" tags, but nothing in the JSON
        // Schema subset `output_config.format` accepts can express
        // `maxItems` (see `schema`'s docs) — so this is the enforcement
        // point. Truncating rather than rejecting: an over-long tag list is
        // the model being generous, not a broken contract worth
        // dead-lettering a whole message over.
        parsed.suggested_tags.truncate(MAX_SUGGESTED_TAGS);
        Ok(parsed)
    }
}

/// `messages.thread_id` for `message_id`, or `None` if the message has no
/// thread (or, in the same race the queue's dispatch tail already documents
/// for `target_names`, no longer exists — [`write_summary`]'s foreign key
/// on `message_id` is what turns that race into a clean, retryable failure
/// rather than a row with a dangling reference).
async fn thread_id_for(db: &Database, message_id: i64) -> Result<Option<i64>, Error> {
    Ok(db
        .read(move |conn| {
            conn.query_row(
                "SELECT thread_id FROM messages WHERE id = ?1",
                [message_id],
                |row| row.get::<_, Option<i64>>(0),
            )
            .optional()
        })
        .await?
        .flatten())
}

/// Persist one triage verdict, upserting on `(message_id, pass, model)` —
/// "re-triage this message under the same model" replaces its own prior
/// verdict rather than accumulating a second row next to it (unlike
/// `ai_ledger`, which is deliberately append-only for every call
/// regardless of outcome). `ai_fts` is kept in sync by the migration's own
/// triggers, not by this function — see V21's module comment for why that
/// is a trigger's job rather than application code's.
///
/// # Atomic with the deep-pass gate
///
/// When `deep_gate` is `Some` and `result`'s fields qualify (see
/// [`DeepPassGate::qualifies`]), the resulting `ai_queue` row for the deep
/// pass is inserted (via [`crate::ai::queue::enqueue_one`]) in the *same*
/// write transaction as this triage row, not as a separate step afterward.
/// This is deliberate: an earlier version of this function wrote the triage
/// row, then made a best-effort, log-and-continue call to enqueue the deep
/// job as a second, independently-failable step. That trades one problem
/// for a worse one — a transient failure in the *enqueue* (not the
/// already-successful triage write) would silently and *permanently* lose a
/// message's deep pass, since nothing re-evaluates a `done` triage job and
/// [`crate::ai::queue::AiQueue::enqueue`]'s own dedup on `(message_id,
/// pass)` means a later retry attempt would not even try again. Folding
/// both writes into one transaction removes that failure mode entirely
/// rather than shrinking its window: either both rows land, or (on any
/// error) neither does and the whole triage call is retried from scratch,
/// which will re-evaluate the gate identically next time.
///
/// Returns whether a deep pass was queued, for [`TriagePassHandler::on_success`]
/// to record on its own tracing span.
async fn write_summary(
    db: &Database,
    lease: &AiLease,
    thread_id: Option<i64>,
    model: &str,
    result: &TriageResult,
    ledger_entry_id: i64,
    deep_gate: Option<&DeepPassGate>,
) -> Result<bool, Error> {
    let message_id = lease.message_id;
    let account_id = lease.account_id;
    let model = model.to_owned();
    // `unwrap_or_else` rather than `?`: a `Vec<String>` this process itself
    // just deserialized can only fail to re-serialize on an allocation
    // failure, not a data problem — falling back to an empty array here is
    // not hiding a data-integrity bug the way a silent parse failure
    // elsewhere in this module would be.
    let suggested_tags =
        serde_json::to_string(&result.suggested_tags).unwrap_or_else(|_| "[]".to_owned());
    let tl_dr = result.tl_dr.clone();
    let category = result.category.clone();
    let priority = result.priority.clone();
    let sentiment = result.sentiment.clone();
    let needs_reply = result.needs_reply;
    let deep_job = deep_gate
        .filter(|gate| gate.qualifies(&result.priority, result.needs_reply, &result.category))
        .map(|_| NewAiJob::new(message_id, account_id, crate::ai::deep::PASS));
    Ok(db
        .write(move |conn| {
            let tx = conn.transaction()?;
            tx.execute(
                "INSERT INTO ai_summaries (
                     message_id, account_id, thread_id, model, pass, schema_version,
                     tl_dr, sentiment, category, priority, needs_reply, suggested_tags,
                     ledger_entry_id, created_at
                 ) VALUES (
                     ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, unixepoch()
                 )
                 ON CONFLICT(message_id, pass, model) DO UPDATE SET
                     account_id = excluded.account_id,
                     thread_id = excluded.thread_id,
                     schema_version = excluded.schema_version,
                     tl_dr = excluded.tl_dr,
                     sentiment = excluded.sentiment,
                     category = excluded.category,
                     priority = excluded.priority,
                     needs_reply = excluded.needs_reply,
                     suggested_tags = excluded.suggested_tags,
                     ledger_entry_id = excluded.ledger_entry_id,
                     created_at = excluded.created_at",
                rusqlite::params![
                    message_id,
                    account_id,
                    thread_id,
                    model,
                    PASS,
                    SCHEMA_VERSION,
                    tl_dr,
                    sentiment,
                    category,
                    priority,
                    needs_reply,
                    suggested_tags,
                    ledger_entry_id,
                ],
            )?;
            let queued = match &deep_job {
                Some(job) => crate::ai::queue::enqueue_one(&tx, job)?,
                None => false,
            };
            tx.commit()?;
            Ok(queued)
        })
        .await?)
}

#[cfg(test)]
mod tests;
