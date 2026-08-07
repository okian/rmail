//! The deep pass (task 49): the conditional, expensive second pass over a
//! message triage already flagged as worth a closer look.
//!
//! Like [`crate::ai::triage`], this module is a [`PassHandler`] and the
//! small amount of policy that decides when one of its jobs gets created in
//! the first place — everything about *how* a call happens (leasing,
//! redaction, concurrency, the audit ledger) belongs to
//! [`crate::ai::queue`] and is not reimplemented here.
//!
//! # Three things this module owns that triage's didn't need to
//!
//! **Gating.** "Conditional" is the acceptance criterion's own word: a deep
//! pass must not run for every message the way triage's does, only for the
//! ones triage's own verdict flags as worth it. [`DeepPassGate`] is that
//! decision, and it is deliberately *not* folded into
//! [`DeepPassHandler::build_request`] — see that method's docs for why a
//! [`PassHandler`] answering "should this job even exist" from inside the
//! step that assumes it already does would be too late. Instead
//! [`crate::ai::triage::TriagePassHandler`]'s own `write_summary` calls
//! [`DeepPassGate::qualifies`] — a pure, synchronous predicate over the
//! verdict fields it is about to persist — and, if it qualifies, enqueues
//! the deep job in the *same write transaction* as the triage row. See that
//! function's own docs for why atomicity there, not a separate best-effort
//! step afterward, is what closes off "a qualifying verdict whose deep job
//! silently never got created."
//!
//! **Thread-aware folding.** [`DeepPassHandler::build_request`] reads the
//! most recent prior deep-pass row for the same thread and folds its state
//! into the prompt, so `thread_summary` is a rollup that grows with the
//! thread rather than a document re-derived from scratch on every message.
//! This is why [`PassHandler::build_request`] had to become `async` — see
//! [`crate::ai::queue::PassHandler`]'s own docs on that change.
//!
//! **Feeding the lexical and semantic indexes.** Explicitly this module's
//! job, not task 48's: triage's `tl_dr` and this pass's `summary`/
//! `key_points`/`todos` are exactly the kind of AI-derived text
//! `index::extract::Part::Summary` and the `ai_summary` BM25 weight exist
//! for (`rmail-core/src/index/fts.rs`), but nothing before this task ever
//! wrote a `Part::Summary` row. [`feed_index`] does, the same
//! upsert-then-enqueue-follow-on-stages shape
//! [`crate::index::extract::extract_message`] already uses for the parts it
//! owns.
//!
//! # Why entities get a new table instead of a new `ai_summaries` column
//!
//! `prd.md`'s data model specifies `ai_entities` for exactly the
//! dates/amounts/people/organizations a deep pass finds; V21 (task 48)
//! populated only the columns triage needed and deliberately left this pass
//! empty-handed for entities. No task before this one reserved a migration
//! number for it, so `V22__ai_entities.sql` claims the next one — see that
//! migration's own comment for the numbering rationale.
//!
//! # A known, accepted race: two deep passes in the same thread, same cycle
//!
//! [`DeepPassHandler::build_request`] reads the thread's prior state once,
//! before the semaphore permit that bounds concurrency and before the
//! provider call itself — see [`crate::ai::queue::AiWorkerPool::process_one`]'s
//! own ordering. If two messages from the *same* thread are both leased in
//! the same [`crate::ai::queue::AiWorkerPool::dispatch_pending`] cycle (or
//! the same [`crate::ai::queue::BatchCoordinator`] submission, where every
//! item's request is built before any of them is sent), both read the same
//! prior rollup concurrently, and whichever finishes last simply overwrites
//! the other's contribution in `ai_summaries.thread_summary` — the earlier
//! one's content is not lost from the mailbox (its own row still holds its
//! own `summary`), only from the *rollup*.
//!
//! This is left open rather than fixed here because closing it properly
//! means the queue itself serializing dispatch per thread — a leasing-order
//! change that belongs to [`crate::ai::queue`] (tasks 47/50), not something
//! this handler can do by, say, taking a lock local to this process (a
//! second daemon instance, or a second [`crate::ai::queue::AiWorkerPool`],
//! would not see it).
//! It matters most for the batch path — the primary route for backlog and
//! initial-sync catch-up, exactly where a thread is likely to have several
//! messages queued at once — and is the reason the acceptance criterion's
//! incrementality is proven here only across *separate* dispatch cycles
//! (see the tests), not within one. Whoever wires the daemon dispatch loop
//! (task 50) should account for this — e.g. capping concurrent `"deep"`
//! leases to one per thread per cycle — before treating batch-mode deep
//! analysis of a multi-message thread as trustworthy.
use async_trait::async_trait;
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::ai::provider::{ChatRequest, OutputFormat};
use crate::ai::queue::{AiLease, MessageContent, PassHandler};
use crate::ai::triage::PRIORITIES;
use crate::config::AiDeepPass;
use crate::error::Error;
use crate::index::extract::{normalize, Part};
use crate::index::{IndexKind, IndexQueue, NewJob, PRIORITY_NORMAL};
use crate::storage::Database;

/// The wire value of `ai_queue.pass` / `ai_ledger.pass` / `ai_summaries.pass`
/// this handler answers to.
pub const PASS: &str = "deep";

/// `ai_summaries.schema_version` for the shape [`schema`] describes today.
/// Independent of [`crate::ai::triage`]'s own version counter — a different
/// pass, a different shape, a different number to bump when it changes.
const SCHEMA_VERSION: i64 = 1;

/// Generous relative to triage's ceiling: a deep pass asks for a synopsis,
/// a point list, a to-do list and an entity list in one answer, not one
/// short verdict.
const DEFAULT_MAX_TOKENS: u32 = 4096;

/// The vocabulary [`schema`] constrains `entities[].kind` to, and
/// [`DeepResult::parse`] re-checks explicitly for the same reason
/// [`crate::ai::triage::TriageResult::parse`] re-checks its own enum
/// fields: `output_config.format` guarantees shape, never that a value is
/// one of these five, and this vocabulary is exactly what
/// `ai_entities.kind` (V22) claims to hold.
const ENTITY_KINDS: [&str; 5] = ["date", "amount", "person", "organization", "other"];

/// Upper bound, in `char`s, on the prior-thread synopsis
/// [`prior_thread_state`] folds into a request — a hard, code-enforced cap
/// rather than trusting the prompt's own "keep it to a few sentences"
/// instruction (`SYSTEM_PROMPT` above). Every other piece of model-facing
/// text in this pipeline has one: the body via `ai.privacy.max_body_chars`
/// (`queue/content.rs`), the whole redacted request via
/// `redact::MAX_SCAN_BYTES`. Without this one, a single verbose or
/// adversarial `thread_summary` would sit at the head of *every* later
/// request in that thread, growing the cost of a subsystem that otherwise
/// has an explicit daily/monthly spend cap, and — because
/// `redact::bounded`'s own truncation runs after this text is already
/// prepended, and does not set `MessageContent::truncated` — could silently
/// eat into the real message body's tail without the `[body truncated]`
/// marker the model is told to expect. ~4,000 chars is generous next to the
/// "a few sentences" the prompt asks for while still bounding the worst
/// case to roughly 1,000 tokens.
const MAX_PRIOR_STATE_CHARS: usize = 4_000;

/// Frozen, cacheable system prompt — see
/// [`crate::ai::triage::SYSTEM_PROMPT`]'s own docs for why keeping this
/// byte-identical across calls matters for `ClaudeProvider`'s prompt cache.
/// Nothing about a specific message or its thread belongs here; that is
/// what the user turn is for.
const SYSTEM_PROMPT: &str = "You are the deep-analysis stage of an email client's AI pipeline, \
run only for messages triage has already flagged as high-priority, needing a \
reply, or in a category the operator always wants analyzed in depth. You \
read one email at a time -- optionally with a short synopsis of the same \
thread's prior messages -- and answer with a single structured JSON object \
only -- no prose, no markdown, nothing outside the schema.

Produce:
- summary: two to four sentences capturing what this message says and what, \
if anything, it asks of the recipient.
- key_points: the message's distinct, substantive points, each its own \
short string -- omit anything already implied by the summary.
- todos: concrete actions this message asks the recipient to take, each \
with a due date (a plain description such as \"by Friday\", or null if none \
was given) and an owner (who the message expects to act, or null if \
unclear). Empty if the message asks for nothing.
- entities: dates, amounts, people and organizations the message names that \
would help someone triaging it later. kind is date, amount, person, \
organization, or other; value is the text as written; iso is the \
normalized ISO-8601 date when kind is date, else null; amount and currency \
are the normalized number and ISO 4217 code when kind is amount, else null.
- suggested_reply: a short, ready-to-send draft reply if one would \
plausibly help the recipient respond faster, or null if a reply needs \
information only the recipient has, or none is warranted.
- thread_summary: an updated synopsis of the whole thread so far. If a \
prior thread synopsis is given below, extend it with what this message \
adds rather than restating it, and keep it to a few sentences regardless of \
how long the thread has run. If no prior synopsis is given, this message's \
own contribution is the thread's synopsis so far.

Base every field only on the message content given to you. If the body \
looks redacted or truncated, judge from what remains rather than guessing \
at what was removed.";

// ---------------------------------------------------------------------------
// Gating: deciding a job should exist, not answering one that already does
// ---------------------------------------------------------------------------

/// Whether `actual` is at or above `threshold` in [`crate::ai::triage::PRIORITIES`]'s
/// ascending order — the same vocabulary [`crate::ai::triage::TriageResult::parse`]
/// validates `actual` against before it is ever persisted, imported rather
/// than duplicated so the two can never silently drift apart.
///
/// Fails *closed*: if either side is not one of those four values, this
/// returns `false` rather than treating the unrecognized side as the lowest
/// rank. That distinction matters most for `threshold` —
/// `ai.deep_pass.on_priority` is an operator-supplied, unvalidated
/// [`String`] (see [`AiDeepPass`]'s own field), and a typo there
/// (`"High"`, `"none"`, `"off"`) must not silently turn into "every
/// priority satisfies an unrecognized threshold," which is what comparing
/// against a threshold ranked `0` would do — the exact opposite of what a
/// spend-gating threshold is for.
fn priority_at_least(actual: &str, threshold: &str) -> bool {
    let rank = |p: &str| PRIORITIES.iter().position(|x| *x == p);
    match (rank(actual), rank(threshold)) {
        (Some(a), Some(t)) => a >= t,
        _ => false,
    }
}

/// Whether a triage verdict with these fields earns a deep pass under
/// `gate`'s configured thresholds. Any one condition is enough — this is an
/// OR of the three, not an AND, matching `ai.deep_pass`'s own doc comment
/// ("Trigger a deep pass when triage flags priority ≥ high / needs_reply /
/// allowlisted category").
///
/// Pure and synchronous — no DB access — so [`crate::ai::triage::TriagePassHandler`]
/// can call this from inside the very write transaction that persists the
/// triage verdict being judged, on the same in-memory values about to be
/// written, rather than as a separate step with its own failure mode. See
/// [`DeepPassGate::qualifies`]'s own docs for why that atomicity is the
/// point.
fn qualifies(priority: &str, needs_reply: bool, category: &str, gate: &AiDeepPass) -> bool {
    (gate.on_needs_reply && needs_reply)
        || priority_at_least(priority, &gate.on_priority)
        || gate.categories.iter().any(|c| c == category)
}

/// What [`crate::ai::triage::TriagePassHandler::on_success`] needs to decide
/// whether a message just earns a deep pass.
///
/// A separate type rather than folding this into [`DeepPassHandler`]'s own
/// [`PassHandler`] impl: gating happens the instant a fresh triage verdict
/// is known, which is a property of *triage's* success path, not of
/// whether (or when) a deep job is ever leased — see the module docs.
/// Handed to [`crate::ai::triage::TriagePassHandler`] via
/// `with_deep_pass_gate` so triage's own module does not need to know
/// `ai.deep_pass`'s shape, only that this type can answer "does this
/// verdict qualify."
#[derive(Debug, Clone)]
pub struct DeepPassGate {
    config: AiDeepPass,
}

impl DeepPassGate {
    /// A gate that judges verdicts under `config`'s thresholds.
    ///
    /// Warns once, here, if `config.on_priority` is not one of
    /// [`crate::ai::triage::PRIORITIES`]'s recognized values — not on every
    /// [`Self::qualifies`] call, which runs inside triage's write
    /// transaction on every verdict and would turn one operator typo into a
    /// log line per message forever. [`priority_at_least`] itself still
    /// fails closed either way (an unrecognized threshold never qualifies
    /// via the priority trigger, only via `on_needs_reply`/`categories`) —
    /// this warning exists so a misconfigured threshold is visible at
    /// startup instead of silently gating nothing.
    #[must_use]
    pub fn new(config: AiDeepPass) -> Self {
        if !PRIORITIES.contains(&config.on_priority.as_str()) {
            tracing::warn!(
                on_priority = %config.on_priority,
                recognized = ?PRIORITIES,
                "ai.deep_pass.on_priority is not a recognized priority; the priority trigger \
                 will never qualify a message for a deep pass until this is fixed (needs_reply \
                 and categories triggers are unaffected)"
            );
        }
        Self { config }
    }

    /// Whether a triage verdict with these fields earns a deep pass.
    ///
    /// Deliberately synchronous and DB-free: the caller
    /// ([`crate::ai::triage::TriagePassHandler::write_summary`]) already
    /// holds the verdict fields it is about to persist and is inside the
    /// same write transaction that will persist them. Making this method
    /// touch the database — even just to re-read the row its own caller is
    /// mid-way through writing — would reintroduce exactly the
    /// two-separately-failable-steps problem atomicity exists to close:
    /// a durable triage verdict whose matching deep job silently never got
    /// created because *something else* (not this decision) failed in
    /// between.
    #[must_use]
    pub(crate) fn qualifies(&self, priority: &str, needs_reply: bool, category: &str) -> bool {
        qualifies(priority, needs_reply, category, &self.config)
    }
}

// ---------------------------------------------------------------------------
// The handler
// ---------------------------------------------------------------------------

/// The deep pass's [`PassHandler`].
///
/// Cheap to clone/share: every field either already is (`Database`,
/// `IndexQueue`) or is a short owned value. Register alongside
/// [`crate::ai::triage::TriagePassHandler`] with
/// [`crate::ai::queue::AiWorkerPool::new`] and
/// [`crate::ai::queue::BatchCoordinator::new`] the same way — both call
/// [`PassHandler::build_request`]/[`PassHandler::on_success`] identically,
/// so a message deep-analyzed live and one deep-analyzed via a Message
/// Batches submission are written the same way.
#[derive(Debug, Clone)]
pub struct DeepPassHandler {
    db: Database,
    index_queue: IndexQueue,
    model: String,
    max_tokens: u32,
    deep_pass: AiDeepPass,
}

impl DeepPassHandler {
    /// A handler that queries `model` (typically `ai.models.deep`, e.g.
    /// `claude-opus-4-8` or `claude-sonnet-5`), writes its answers into
    /// `db`, and feeds `index_queue`'s lexical/semantic follow-on stages —
    /// see the module docs.
    #[must_use]
    pub fn new(
        db: Database,
        index_queue: IndexQueue,
        model: impl Into<String>,
        deep_pass: AiDeepPass,
    ) -> Self {
        Self {
            db,
            index_queue,
            model: model.into(),
            max_tokens: DEFAULT_MAX_TOKENS,
            deep_pass,
        }
    }

    /// Override the default output token ceiling — mainly for tests that
    /// want a tight bound.
    #[must_use]
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }
}

#[async_trait]
impl PassHandler for DeepPassHandler {
    fn pass(&self) -> &str {
        PASS
    }

    /// Async — unlike [`crate::ai::triage::TriagePassHandler`]'s — because
    /// folding a thread's prior state requires a durable-state read this
    /// handler cannot answer from `content` alone; see
    /// [`crate::ai::queue::PassHandler::build_request`]'s own docs for why
    /// the trait method changed shape to allow it.
    #[tracing::instrument(skip(self, content), fields(message_id = content.message_id))]
    async fn build_request(&self, content: &MessageContent) -> Result<ChatRequest, Error> {
        let prior = match thread_id_for(&self.db, content.message_id).await? {
            Some(thread_id) => prior_thread_state(&self.db, thread_id, content.message_id).await?,
            None => None,
        };
        Ok(ChatRequest::new(self.model.clone(), self.max_tokens)
            .system(SYSTEM_PROMPT)
            .user(render_user_message(content, prior.as_deref()))
            .output_format(OutputFormat::json_schema(schema())))
    }

    #[tracing::instrument(skip(self, lease, text), fields(message_id = lease.message_id))]
    async fn on_success(
        &self,
        lease: &AiLease,
        text: &str,
        ledger_entry_id: i64,
    ) -> Result<(), Error> {
        let mut result = DeepResult::parse(text)?;
        if !self.deep_pass.suggest_reply {
            // The prompt asks for a reply whenever one would help; an
            // operator who turned `suggest_reply` off gets the rest of the
            // analysis regardless, just never a drafted reply persisted
            // where a careless `mail ai reply --draft` could send it.
            result.suggested_reply = None;
        }
        let thread_id = thread_id_for(&self.db, lease.message_id).await?;
        write_summary(
            &self.db,
            lease,
            thread_id,
            &self.model,
            &result,
            ledger_entry_id,
        )
        .await?;
        write_entities(&self.db, lease.message_id, &self.model, &result.entities).await?;
        feed_index(&self.db, &self.index_queue, lease.message_id, &result).await?;
        tracing::debug!("deep pass verdict written");
        Ok(())
    }
}

/// The user turn: everything about the message the model needs, plus a
/// prior-thread section when one exists. Mirrors
/// [`crate::ai::triage::render_user_message`]'s shape for the parts they
/// share.
fn render_user_message(content: &MessageContent, prior_thread_state: Option<&str>) -> String {
    let from = match (&content.from_name, &content.from_addr) {
        (Some(name), Some(addr)) => format!("{name} <{addr}>"),
        (Some(name), None) => name.clone(),
        (None, Some(addr)) => addr.clone(),
        (None, None) => "(unknown sender)".to_owned(),
    };
    let subject = content.subject.as_deref().unwrap_or("(no subject)");
    let mut out = String::new();
    // Only the prior *synopsis* goes here — never another message's body.
    // This is the whole of what makes the fold incremental: the request
    // this handler builds never grows with the thread's length, only with
    // however long `thread_summary` itself is, which the prompt above
    // explicitly asks the model to keep short.
    if let Some(prior) = prior_thread_state {
        out.push_str(
            "Prior thread synopsis so far (from earlier messages in this thread -- extend it, \
             do not restate it):\n",
        );
        out.push_str(prior);
        out.push_str("\n\n---\n\n");
    }
    out.push_str(&format!(
        "From: {from}\nSubject: {subject}\n\n{}",
        content.body
    ));
    if content.truncated {
        out.push_str("\n\n[body truncated]");
    }
    out
}

/// The JSON Schema every deep-pass request constrains its response to. See
/// [`crate::ai::triage::schema`]'s own docs for why this is kept
/// byte-stable across calls (prompt caching) and why every optional field
/// is expressed as a required-but-nullable property rather than an omitted
/// one — the subset `output_config.format` accepts has no notion of an
/// optional key, only a required one whose value may be `null`.
fn schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "summary": {"type": "string"},
            "key_points": {"type": "array", "items": {"type": "string"}},
            "todos": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "text": {"type": "string"},
                        "due": {"type": ["string", "null"]},
                        "owner": {"type": ["string", "null"]},
                    },
                    "required": ["text", "due", "owner"],
                    "additionalProperties": false,
                },
            },
            "entities": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "kind": {"type": "string", "enum": ENTITY_KINDS},
                        "value": {"type": "string"},
                        "iso": {"type": ["string", "null"]},
                        "amount": {"type": ["number", "null"]},
                        "currency": {"type": ["string", "null"]},
                    },
                    "required": ["kind", "value", "iso", "amount", "currency"],
                    "additionalProperties": false,
                },
            },
            "suggested_reply": {"type": ["string", "null"]},
            "thread_summary": {"type": "string"},
        },
        "required": [
            "summary",
            "key_points",
            "todos",
            "entities",
            "suggested_reply",
            "thread_summary",
        ],
        "additionalProperties": false,
    })
}

/// One to-do the deep pass extracted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Todo {
    text: String,
    due: Option<String>,
    owner: Option<String>,
}

/// One entity the deep pass extracted — the in-memory shape [`DeepResult`]
/// parses into; [`write_entities`] is what turns these into `ai_entities`
/// rows.
#[derive(Debug, Clone, PartialEq, Deserialize)]
struct EntityOut {
    kind: String,
    value: String,
    iso: Option<String>,
    amount: Option<f64>,
    currency: Option<String>,
}

/// The deep pass's structured answer, once parsed and validated.
#[derive(Debug, Clone, PartialEq, Deserialize)]
struct DeepResult {
    summary: String,
    key_points: Vec<String>,
    todos: Vec<Todo>,
    entities: Vec<EntityOut>,
    suggested_reply: Option<String>,
    thread_summary: String,
}

impl DeepResult {
    /// Parse and validate one deep-pass response — see
    /// [`crate::ai::triage::TriageResult::parse`]'s own docs for why this
    /// still checks `entities[].kind` against [`ENTITY_KINDS`] even though
    /// `output_config.format` already guarantees the response's shape: a
    /// guarantee about *shape* is not a guarantee about *enum membership*,
    /// and an API-side regression that let one through must surface as a
    /// loud, dead-letterable error here rather than a row `ai_entities`
    /// silently mis-typed.
    ///
    /// # Errors
    /// [`Error::Internal`] if `text` is not valid JSON for [`DeepResult`],
    /// or any entity's `kind` holds a value outside [`ENTITY_KINDS`]. Never
    /// a partial result.
    fn parse(text: &str) -> Result<Self, Error> {
        let parsed: Self = serde_json::from_str(text).map_err(|e| {
            Error::internal(format!(
                "deep pass structured output did not match the requested schema: {e}"
            ))
        })?;
        if let Some(bad) = parsed
            .entities
            .iter()
            .find(|e| !ENTITY_KINDS.contains(&e.kind.as_str()))
        {
            return Err(Error::internal(format!(
                "deep pass returned an unrecognized entity kind {:?}",
                bad.kind
            )));
        }
        Ok(parsed)
    }
}

// ---------------------------------------------------------------------------
// Thread-aware folding
// ---------------------------------------------------------------------------

/// `messages.thread_id` for `message_id`, or `None` if the message has no
/// thread — the same lookup
/// [`crate::ai::triage::thread_id_for`] performs, duplicated rather than
/// imported for the reason that function's own docs give: it is
/// intentionally private to a sibling module this one only reads persisted
/// rows from.
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

/// The prior deep-pass state for `thread_id`: `COALESCE(thread_summary,
/// summary)` of the most recent other message already deep-analyzed in
/// this thread. `thread_summary` is preferred because, once one exists, it
/// already embeds every message before it — it was itself built by this
/// same fold — so a caller here never has to walk more than one row back
/// regardless of how long the thread has run. `summary` is the fallback
/// only for the thread's first analyzed message, whose own `thread_summary`
/// may not exist yet at the instant a second message's request is being
/// built.
///
/// Joins through `messages.thread_id` — the live value — rather than
/// filtering on `ai_summaries.thread_id` directly. That column is a
/// denormalized snapshot with no foreign key (V21's own docs are explicit
/// about this), and `thread::merge_threads` reassigns `messages.thread_id`
/// and deletes the old `threads` row without ever touching `ai_summaries` —
/// an ordinary consequence of a late-arriving parent message joining two
/// threads together. `threads.id` has no
/// `AUTOINCREMENT`, so a deleted thread's id can be reused by an unrelated
/// later thread. Trusting the snapshot would mean two failure modes at
/// once: a merged thread's prior rows silently stop being found (folding
/// quietly resets to "no prior state"), and — worse — a *new*, unrelated
/// thread that happens to reuse a stale id would fold a stranger's
/// rollup into its own outbound prompt. Joining through `messages.thread_id`
/// is immune to both, and reaches `ai_summaries` through the same
/// `message_id`-leading index (`sqlite_autoindex_ai_summaries_1`, from
/// V21's `UNIQUE(message_id, pass, model)`) the `idx_messages_thread`-driven
/// join already narrows to just this thread's messages — never a scan of
/// every `ai_summaries` row in the database.
///
/// `s.message_id != ?2` excludes the current message's own (possibly
/// stale, from a prior force-reanalysis) row — this fold is always about
/// *other* messages in the thread, never a message folding its own history
/// into itself.
async fn prior_thread_state(
    db: &Database,
    thread_id: i64,
    message_id: i64,
) -> Result<Option<String>, Error> {
    let prior = db
        .read(move |conn| {
            conn.query_row(
                "SELECT COALESCE(s.thread_summary, s.summary)
                 FROM ai_summaries s
                 JOIN messages m ON m.id = s.message_id
                 WHERE m.thread_id = ?1 AND s.pass = ?3 AND s.message_id != ?2
                   AND COALESCE(s.thread_summary, s.summary) IS NOT NULL
                   AND TRIM(COALESCE(s.thread_summary, s.summary)) != ''
                 ORDER BY s.created_at DESC, s.id DESC LIMIT 1",
                rusqlite::params![thread_id, message_id, PASS],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()
        })
        .await?
        .flatten();

    // Enforce the cap this module documents. Without it a thread's folded
    // state grows unbounded across a long conversation, and because
    // `redact`'s own truncation runs *after* this text has been prepended it
    // would start eating the real message body's tail — silently, since that
    // path does not set the `[body truncated]` marker the prompt tells the
    // model to expect. Cutting on a character boundary rather than a byte one
    // keeps the string valid UTF-8.
    Ok(prior.map(|text| {
        if text.chars().count() <= MAX_PRIOR_STATE_CHARS {
            return text;
        }
        let mut truncated: String = text.chars().take(MAX_PRIOR_STATE_CHARS).collect();
        truncated.push_str("\n[prior thread state truncated]");
        truncated
    }))
}

// ---------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------

/// Persist one deep-pass verdict, upserting on `(message_id, pass, model)` —
/// the same "re-analyze under the same model replaces its own prior
/// verdict" rule [`crate::ai::triage::write_summary`] applies, so a deep row
/// and a triage row for the same message coexist as distinct rows (`pass`
/// differs) while a second deep run under the same model replaces the
/// first. `tl_dr`/`sentiment`/`category`/`priority`/`needs_reply`/
/// `suggested_tags` are triage-only columns and are simply never mentioned
/// here — they stay whatever they already are (`NULL` on first insert,
/// since triage's own row is a different `(pass, model)` key entirely).
async fn write_summary(
    db: &Database,
    lease: &AiLease,
    thread_id: Option<i64>,
    model: &str,
    result: &DeepResult,
    ledger_entry_id: i64,
) -> Result<(), Error> {
    let message_id = lease.message_id;
    let account_id = lease.account_id;
    let model = model.to_owned();
    // Same reasoning as `triage::write_summary`'s identical fallback: a
    // `Vec`/struct this process itself just deserialized can only fail to
    // re-serialize on an allocation failure, not a data problem.
    let key_points = serde_json::to_string(&result.key_points).unwrap_or_else(|_| "[]".to_owned());
    let todos = serde_json::to_string(&result.todos).unwrap_or_else(|_| "[]".to_owned());
    let summary = result.summary.clone();
    let thread_summary = result.thread_summary.clone();
    let suggested_reply = result.suggested_reply.clone();
    db.write(move |conn| {
        conn.execute(
            "INSERT INTO ai_summaries (
                 message_id, account_id, thread_id, model, pass, schema_version,
                 summary, thread_summary, key_points, todos, suggested_reply,
                 ledger_entry_id, created_at
             ) VALUES (
                 ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, unixepoch()
             )
             ON CONFLICT(message_id, pass, model) DO UPDATE SET
                 account_id = excluded.account_id,
                 thread_id = excluded.thread_id,
                 schema_version = excluded.schema_version,
                 summary = excluded.summary,
                 thread_summary = excluded.thread_summary,
                 key_points = excluded.key_points,
                 todos = excluded.todos,
                 suggested_reply = excluded.suggested_reply,
                 ledger_entry_id = excluded.ledger_entry_id,
                 created_at = excluded.created_at",
            rusqlite::params![
                message_id,
                account_id,
                thread_id,
                model,
                PASS,
                SCHEMA_VERSION,
                summary,
                thread_summary,
                key_points,
                todos,
                suggested_reply,
                ledger_entry_id,
            ],
        )
    })
    .await?;
    Ok(())
}

/// Replace `model`'s entity set for `message_id` — delete then bulk-insert
/// in one write transaction, so a reader never observes a half-replaced
/// set. See `V22__ai_entities.sql`'s own docs for why this is scoped by
/// `model` rather than wiping every model's rows for the message.
async fn write_entities(
    db: &Database,
    message_id: i64,
    model: &str,
    entities: &[EntityOut],
) -> Result<(), Error> {
    let model = model.to_owned();
    let entities = entities.to_vec();
    db.write(move |conn| {
        let tx = conn.transaction()?;
        tx.execute(
            "DELETE FROM ai_entities WHERE message_id = ?1 AND model = ?2",
            rusqlite::params![message_id, model],
        )?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO ai_entities (message_id, model, kind, value, iso, amount, currency)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            )?;
            for entity in &entities {
                stmt.execute(rusqlite::params![
                    message_id,
                    model,
                    entity.kind,
                    entity.value,
                    entity.iso,
                    entity.amount,
                    entity.currency,
                ])?;
            }
        }
        tx.commit()
    })
    .await?;
    Ok(())
}

/// Extractor name recorded in `index_content.extractor` for the
/// `Part::Summary` row this writes — distinguishing this pass's output from
/// [`crate::index::extract::EXTRACTOR`]'s the same way an attachment's
/// extractor name distinguishes attachment text (`rmail-core/src/attach/mod.rs`).
const EXTRACTOR: &str = "rmail/ai-deep@1";

/// Fold this message's own deep-pass text into `index_content` as
/// [`crate::index::extract::Part::Summary`], and enqueue the lexical and
/// semantic follow-on stages so both indexes eventually pick it up — the
/// acceptance criterion's "enrichments feeding the lexical + semantic
/// indexes," and explicitly this module's job (see the module docs).
///
/// Only `summary`, `key_points` and each todo's `text` go in — not
/// `thread_summary` (that describes the *thread*, and tagging every message
/// in it with identical text would inflate every one of their scores
/// identically for a term that is really about only one of them) and not
/// `suggested_reply` (a drafted reply is not something a search for "what
/// does this message say" should match on).
///
/// This writes `index_content` directly and enqueues [`IndexKind::Lexical`]/
/// [`IndexKind::Semantic`] rather than driving `FtsIndex`/`SemanticIndex`
/// itself, the same division [`crate::index::extract::extract_message`]
/// already draws: this pass decides *what* text belongs in the index, the
/// queue and its (future) workers decide *when* the index actually gets
/// rebuilt from it. Both follow-on jobs are enqueued unconditionally —
/// including when this message's own contribution is empty and the
/// `Part::Summary` row is *removed* — because a removal has to reach the
/// lexical and semantic indexes exactly as much as an addition does; a stale
/// `Part::Summary` row's text staying searchable forever once the AI content
/// that produced it is gone is the same failure
/// [`crate::index::extract::extract_message`]'s own removal path exists to
/// prevent, applied here to one part instead of a whole message.
///
/// The `content_hash` handed to both jobs is
/// [`crate::index::extract::message_hash`] over *every* part now stored for
/// the message, read back inside the same transaction as the write — not a
/// hash of this part alone. `index_state.content_hash` (what
/// `index::queue::enqueue_one`'s dedup compares against) is always a
/// whole-message hash, because that is what a routine extract sweep writes;
/// handing it a hash of one part would never agree with that record, and
/// every deep pass would force an unconditional re-index (a real embedding
/// cost) whether or not the message's indexed content actually changed.
async fn feed_index(
    db: &Database,
    index_queue: &IndexQueue,
    message_id: i64,
    result: &DeepResult,
) -> Result<(), Error> {
    let mut raw = result.summary.clone();
    for point in &result.key_points {
        raw.push('\n');
        raw.push_str(point);
    }
    for todo in &result.todos {
        raw.push('\n');
        raw.push_str(&todo.text);
    }
    let text = normalize(&raw);
    let is_empty = text.is_empty();
    // The key comes from `Part::Summary`, not a literal, so this row and the
    // `ai_summary` BM25 column that reads it cannot drift apart if the enum's
    // stored spelling ever changes.
    let summary_key = Part::Summary.as_key();

    let content_hash = db
        .write(move |conn| {
            let tx = conn.transaction()?;
            if is_empty {
                tx.execute(
                    "DELETE FROM index_content WHERE message_id = ?1 AND part = ?2",
                    rusqlite::params![message_id, summary_key],
                )?;
            } else {
                let chars = i64::try_from(text.chars().count()).unwrap_or(i64::MAX);
                let part_hash = Sha256::digest(text.as_bytes()).to_vec();
                tx.execute(
                    "INSERT INTO index_content
                         (message_id, part, text, chars, content_hash, extracted_at, extractor)
                     VALUES (?1, ?6, ?2, ?3, ?4, unixepoch(), ?5)
                     ON CONFLICT(message_id, part) DO UPDATE SET
                         text = excluded.text,
                         chars = excluded.chars,
                         content_hash = excluded.content_hash,
                         extracted_at = excluded.extracted_at,
                         extractor = excluded.extractor",
                    rusqlite::params![message_id, text, chars, part_hash, EXTRACTOR, summary_key],
                )?;
            }

            // Read the *stored* set back inside this same transaction, so
            // the hash describes exactly what a follow-on stage will find —
            // the identical discipline `index::extract::store` documents
            // for its own version of this read.
            let stored: Vec<(String, Vec<u8>)> = {
                let mut stmt = tx.prepare(
                    "SELECT part, content_hash FROM index_content WHERE message_id = ?1",
                )?;
                let rows = stmt
                    .query_map([message_id], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                rows
            };
            let hash = crate::index::extract::message_hash(&stored);
            tx.commit()?;
            Ok(hash)
        })
        .await?;

    index_queue
        .enqueue(
            vec![
                NewJob::new(message_id, IndexKind::Lexical)
                    .content_hash(content_hash.clone())
                    .priority(PRIORITY_NORMAL),
                NewJob::new(message_id, IndexKind::Semantic)
                    .content_hash(content_hash)
                    .priority(PRIORITY_NORMAL),
            ],
            None,
        )
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests;
