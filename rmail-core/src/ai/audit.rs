//! The append-only ledger recording every model call, and its cost rollups.
//!
//! # Why the database enforces append-only, not just the API
//!
//! [`record_call`] is the only function in this module that writes to
//! `ai_ledger`, and it only ever `INSERT`s. That would be enough discipline if
//! this file were the only thing that could touch the table — but the whole
//! point of an audit trail is that it survives a bug, a migration mistake, or
//! a future contributor reaching for `UPDATE` to "fix" a bad row. So the
//! invariant is enforced one layer down: `V18__ai_ledger.sql` installs three
//! triggers that `RAISE(ABORT, ...)` on any `UPDATE`, `DELETE`, or
//! id-colliding `INSERT` (SQLite's `INSERT OR REPLACE` resolves a primary-key
//! collision as an internal delete-then-insert, which a `BEFORE DELETE`
//! trigger alone does not see unless `PRAGMA recursive_triggers` is on — this
//! codebase does not set it, so the third trigger is load-bearing) against
//! `ai_ledger`, regardless of which code path issues it. [`tests`] proves this
//! by issuing each of those three statements directly against the table and
//! asserting every one is rejected — not by inspecting this module's own
//! code, which would only prove *this file* never asks for one.
//!
//! # The hash is of what left the machine, not what a caller intended to send
//!
//! [`record_call`] takes the exact bytes transmitted to the provider as a
//! plain `&[u8]` — never a redacted-body type from the redaction firewall
//! (task 44), which is being built concurrently and whose types this module
//! does not depend on. That composes in one direction only: a caller redacts
//! first, *then* calls [`record_call`] with the post-redaction bytes, so the
//! SHA-256 stored here proves what the provider actually received. Hashing
//! the pre-redaction body — or accepting a hash a caller computed earlier in
//! the pipeline, before redaction ran — would let the ledger claim a payload
//! never left the machine when the version that did contained more.
//!
//! # `ai_usage` is a rollup, not a ledger
//!
//! Unlike `ai_ledger`, `ai_usage` is mutated in place: it is a materialized
//! per-day sum, incremented atomically alongside every [`record_call`] insert
//! (same transaction — a crash between the two would otherwise leave a call
//! billed but not counted, or counted but not billed). Its rows are derived,
//! not evidentiary, so nothing here protects it from `UPDATE`.
//!
//! # `work_class` is attribution, and it lives here for a reason
//!
//! `V27__ai_budget.sql` adds `ai_ledger.work_class`, written by
//! [`record_call_charged`]. [`crate::ai::budget`] derives every spend figure
//! it enforces from this table — there is no counter table beside it — and
//! deriving a *bulk* sub-budget requires knowing which calls were bulk. That
//! fact belongs on the evidentiary row, next to the tokens and the payload
//! hash it describes, not in a side table that could drift from it. Nothing
//! in this module reads the column back; [`crate::ai::budget`] filters on it
//! in SQL, which is the only place it is ever needed.
//!
//! # What links an AI artifact to its ledger entry
//!
//! [`record_call`] returns the new row's `id`. Later tasks that persist an AI
//! artifact (a summary, an embedding, a queue completion) are expected to
//! store that `id` as a plain foreign key — `ledger_entry_id` — so every
//! artifact traces back to the exact call that produced it, its cost, and
//! proof of what was sent. No such artifact table exists yet, so there is
//! nothing here to link *to*; this module's job is to make sure the id it
//! hands back is one worth keeping.

use std::time::Duration;

use rusqlite::types::Value as SqlValue;
use rusqlite::OptionalExtension;
use sha2::{Digest, Sha256};

use crate::ai::budget::WorkClass;
use crate::ai::provider::Usage;
use crate::error::{Error, Result};
use crate::storage::Database;

/// Default page size for [`query_calls`] when the caller does not ask for a
/// specific limit.
const DEFAULT_QUERY_LIMIT: i64 = 100;

/// Ceiling on [`query_calls`]'s page size. `QueryAiCalls` is a UI-facing,
/// paginated read; a bulk consumer (`ExportLedger`) pages through repeated
/// [`query_calls`] calls by `id` cursor instead of asking for more than this
/// in one call — see `rmaild::audit_service::export_ledger`, which walks the
/// ledger the same way `SyncApi::watch_events` walks the durable event log,
/// rather than materializing the whole matching set into one `Vec`.
const MAX_QUERY_LIMIT: i64 = 500;

// ---------------------------------------------------------------------------
// Vocabulary
// ---------------------------------------------------------------------------

/// Why a recorded call ended the way it did.
#[derive(Debug, Clone)]
pub enum CallOutcome {
    /// The provider returned a usable response.
    Ok,
    /// The call failed; the message is the client-safe summary (never a raw
    /// upstream error body — see the caller's own error-mapping contract).
    Error(String),
}

/// Whether a ledger row's [`CallOutcome`] was `Ok` or `Error`.
///
/// A closed, two-value enum rather than reusing [`CallOutcome`] as the read
/// model: a query result has no error message to discard when the call
/// succeeded, and filtering by outcome (`status = 'ok'`) needs a value with no
/// payload to compare, not a `String` a filter would have to leave empty.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallStatus {
    /// The provider returned a usable response.
    Ok,
    /// The call failed.
    Error,
}

impl CallStatus {
    /// The stable wire string stored in `ai_ledger.status`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Error => "error",
        }
    }

    /// Parse a wire string.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] for a value no version of this code wrote.
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "ok" => Ok(Self::Ok),
            "error" => Ok(Self::Error),
            other => Err(Error::internal(format!(
                "unknown ai_ledger status: {other}"
            ))),
        }
    }
}

/// Everything needed to append one entry to the ledger.
///
/// `payload` is the exact bytes actually transmitted to the provider — after
/// redaction, if redaction ran — never what a caller merely intended to send.
/// [`record_call`] hashes it and discards it; only the hash is persisted, so a
/// mistake elsewhere in the pipeline that leaves raw content in `payload`
/// cannot leak it into the audit trail itself.
#[derive(Debug, Clone)]
pub struct CallRecord<'a> {
    /// The account this call was made on behalf of, if any.
    pub account_id: Option<i64>,
    /// The message this call concerned, if any.
    pub message_id: Option<i64>,
    /// The provider's id for the response, if it produced one.
    pub request_id: Option<String>,
    /// The model id actually used, e.g. `claude-haiku-4-5`.
    pub model: String,
    /// Caller-supplied context tag, e.g. `"triage"` or `"deep"`.
    pub pass: Option<String>,
    /// Token accounting for the call.
    pub usage: Usage,
    /// What the redaction pass did to `payload` before it was sent, e.g.
    /// `"none"` or `"redacted"`. This module does not define the vocabulary —
    /// see the module docs on why it does not depend on task 44's types.
    pub redaction_level: String,
    /// Wall-clock time the call took.
    pub latency: Duration,
    /// The exact bytes transmitted to the provider, post-redaction.
    pub payload: &'a [u8],
    /// How the call ended.
    pub outcome: CallOutcome,
}

/// A materialized filter for [`query_calls`].
///
/// Every set field is AND-ed; an unset field is unfiltered on that dimension.
#[derive(Debug, Clone, Default)]
pub struct AuditFilter {
    /// Restrict to this account.
    pub account_id: Option<i64>,
    /// Restrict to this message.
    pub message_id: Option<i64>,
    /// Restrict to this model id.
    pub model: Option<String>,
    /// Inclusive lower bound, unix seconds.
    pub since: Option<i64>,
    /// Inclusive upper bound, unix seconds.
    pub until: Option<i64>,
    /// Restrict to this outcome.
    pub status: Option<CallStatus>,
}

/// One ledger row, as read back.
#[derive(Debug, Clone, PartialEq)]
pub struct LedgerEntry {
    /// Stable row id — what a future AI artifact links to.
    pub id: i64,
    /// Unix seconds.
    pub created_at: i64,
    /// The account this call was made on behalf of, if any.
    pub account_id: Option<i64>,
    /// The message this call concerned, if any.
    pub message_id: Option<i64>,
    /// The provider's id for the response, if it produced one.
    pub request_id: Option<String>,
    /// The model id used.
    pub model: String,
    /// Caller-supplied context tag.
    pub pass: Option<String>,
    /// Token accounting.
    pub usage: Usage,
    /// Estimated USD cost.
    pub cost_usd: f64,
    /// What the redaction pass did to the payload.
    pub redaction_level: String,
    /// Wall-clock latency, in milliseconds.
    pub latency_ms: i64,
    /// SHA-256 of the exact bytes transmitted.
    pub payload_sha256: Vec<u8>,
    /// How the call ended.
    pub status: CallStatus,
    /// The error, if `status` is [`CallStatus::Error`].
    pub error: Option<String>,
}

/// One day's rolled-up usage, keyed by UTC calendar day (`"YYYY-MM-DD"`).
#[derive(Debug, Clone, PartialEq)]
pub struct DayUsage {
    /// The day, e.g. `"2026-08-05"`.
    pub day: String,
    /// Calls recorded that day.
    pub requests: i64,
    /// Full-price input tokens.
    pub input_tokens: i64,
    /// Output tokens.
    pub output_tokens: i64,
    /// Tokens written to the prompt cache.
    pub cache_creation_input_tokens: i64,
    /// Tokens read from the prompt cache.
    pub cache_read_input_tokens: i64,
    /// Total estimated USD cost.
    pub cost_usd: f64,
}

// ---------------------------------------------------------------------------
// Pricing
// ---------------------------------------------------------------------------

/// Per-token USD pricing for one model.
struct ModelPricing {
    /// USD per full-price input token.
    input: f64,
    /// USD per output token.
    output: f64,
    /// USD per token written to the prompt cache.
    cache_write: f64,
    /// USD per token read from the prompt cache.
    cache_read: f64,
}

/// Cache-write price as a multiple of the base input price, for a 1-hour
/// `cache_control` TTL.
///
/// This codebase's provider always requests the 1-hour TTL (see
/// `config::AiPromptCache`'s default and `ClaudeProvider::build_body`), not
/// the 5-minute one Anthropic also offers (1.25x instead of 2x). There is no
/// per-call TTL recorded in [`Usage`] to disambiguate from, so this table
/// prices every cache write at the TTL this codebase actually sends.
const CACHE_WRITE_1H_MULTIPLIER: f64 = 2.0;

/// Cache-read price as a multiple of the base input price. Fixed regardless
/// of TTL — Anthropic prices a cache hit the same way either way.
const CACHE_READ_MULTIPLIER: f64 = 0.1;

/// Convert a price quoted per million tokens into a price per token.
fn per_million(usd_per_million: f64) -> f64 {
    usd_per_million / 1_000_000.0
}

/// Look up a model's per-token pricing.
///
/// Source: Anthropic's published API pricing (verified via the `claude-api`
/// skill at the time this module was written — $1.00/$5.00 per MTok for
/// `claude-haiku-4-5`, $3.00/$15.00 for `claude-sonnet-5`, $5.00/$25.00 for
/// `claude-opus-4-8`). `claude-sonnet-5` also has a lower introductory rate
/// through 2026-08-31; this table prices it at its standard rate rather than
/// the temporary one, since a cost calculator that silently changes its
/// answer on a promotion's end date is worse than one that is a few cents
/// conservative during it.
fn pricing_for(model: &str) -> Option<ModelPricing> {
    let (input_per_million, output_per_million) = match model {
        "claude-haiku-4-5" => (1.00, 5.00),
        "claude-sonnet-5" => (3.00, 15.00),
        "claude-opus-4-8" => (5.00, 25.00),
        _ => return None,
    };
    let input = per_million(input_per_million);
    Some(ModelPricing {
        input,
        output: per_million(output_per_million),
        cache_write: input * CACHE_WRITE_1H_MULTIPLIER,
        cache_read: input * CACHE_READ_MULTIPLIER,
    })
}

/// Whether this table can price `model` at all.
///
/// Exposed because an unpriced model is not merely a cosmetic gap for
/// [`crate::ai::budget`]: a call recorded at `cost_usd = 0.0` never moves a
/// dollar cap, so routing traffic to an unpriced model would make the budget
/// enforcer's hard cap unreachable. The enforcer checks this before
/// downgrading to a configured ladder id and refuses the downgrade rather
/// than spending against a ceiling that has stopped counting.
#[must_use]
pub fn is_priced(model: &str) -> bool {
    pricing_for(model).is_some()
}

/// Estimate the USD cost of one call from its token usage.
///
/// Returns `0.0` (and logs a warning) for a model id this table does not
/// recognize, rather than failing. A pricing gap for a model added to
/// Anthropic's lineup after this table was last updated should not stop the
/// call from being audited at all — the ledger's first job is to record that
/// the call happened and what it sent; an unpriced `cost_usd = 0.0` is
/// visibly wrong in a way a budget dashboard can flag, where a failed write
/// would instead lose the entry — and its payload hash — entirely.
#[must_use]
pub fn estimate_cost_usd(model: &str, usage: Usage) -> f64 {
    let Some(pricing) = pricing_for(model) else {
        tracing::warn!(
            model,
            "no pricing entry for model; recording cost_usd = 0.0"
        );
        return 0.0;
    };
    f64::from(usage.input_tokens) * pricing.input
        + f64::from(usage.output_tokens) * pricing.output
        + f64::from(usage.cache_creation_input_tokens) * pricing.cache_write
        + f64::from(usage.cache_read_input_tokens) * pricing.cache_read
}

// ---------------------------------------------------------------------------
// Writes
// ---------------------------------------------------------------------------

/// Append one entry to the ledger and fold it into that day's `ai_usage`
/// rollup (see the module docs on why that table is mutated in place while
/// `ai_ledger` is not).
///
/// The insert and the rollup update run in one transaction: either both
/// happen or neither does, so a crash between them can never leave a call
/// counted in the ledger but missing from `ai_usage`, or the reverse.
///
/// # Errors
///
/// A mapped storage error.
pub async fn record_call(db: &Database, record: CallRecord<'_>) -> Result<i64> {
    record_call_priced(db, record, 1.0).await
}

/// As [`record_call_priced`], but says which budget the call is charged to
/// ([`crate::ai::budget`], task 76) — written to `ai_ledger.work_class`.
///
/// The work class is a *parameter* rather than a [`CallRecord`] field so that
/// [`record_call`] and [`record_call_priced`] keep their existing signatures
/// and semantics: every call site that predates budgets — including
/// `rmaild::AiApi`'s forced `AnalyzeMessage`/`SuggestReply`, which are
/// interactive by definition — is charged as
/// [`WorkClass::Interactive`] without needing to say so. The only caller that
/// passes something else is the queue's dispatch path, which is the only one
/// that knows a job's priority.
///
/// Attribution here is what makes the bulk sub-budget enforceable at all: a
/// call recorded without it is charged to the ordinary caps, which
/// under-consumes the bulk reservation (conservative) rather than escaping
/// every cap (not).
///
/// # Errors
///
/// A mapped storage error.
pub async fn record_call_charged(
    db: &Database,
    record: CallRecord<'_>,
    price_multiplier: f64,
    work_class: WorkClass,
) -> Result<i64> {
    record_call_inner(db, record, price_multiplier, work_class).await
}

/// As [`record_call`], but scales the computed cost by `price_multiplier`
/// before it is stored. The Message Batches API's 50% discount
/// ([`crate::ai::queue::BatchCoordinator`], task 47) is the motivating case,
/// applied at `0.5` — `record_call` itself stays the `1.0` common path so
/// every call site that predates batching, including this module's own
/// tests, needs no change.
///
/// Only `cost_usd` is scaled. Token counts (`input_tokens`, `output_tokens`,
/// ...) are recorded as reported — a batch call processes exactly as many
/// tokens as a live one would, so `ai.limits.daily_token_cap` and any
/// token-level accounting must see the real count; the discount is a price
/// term, not a token-count fiction.
///
/// # Errors
///
/// A mapped storage error.
pub async fn record_call_priced(
    db: &Database,
    record: CallRecord<'_>,
    price_multiplier: f64,
) -> Result<i64> {
    record_call_inner(db, record, price_multiplier, WorkClass::Interactive).await
}

/// The one function that writes `ai_ledger`. Both public entry points above
/// funnel through it so there is exactly one `INSERT` statement to keep in
/// step with the schema — see the module docs on why this module is the only
/// thing that appends to the ledger.
#[tracing::instrument(skip(db, record), fields(model = %record.model, cost_usd, id))]
async fn record_call_inner(
    db: &Database,
    record: CallRecord<'_>,
    price_multiplier: f64,
    work_class: WorkClass,
) -> Result<i64> {
    let payload_sha256 = Sha256::digest(record.payload).to_vec();
    let cost_usd = estimate_cost_usd(&record.model, record.usage) * price_multiplier;
    // Saturating rather than truncating: a latency that overflows i64
    // milliseconds is not a value this ledger should misrepresent as small.
    let latency_ms = i64::try_from(record.latency.as_millis()).unwrap_or(i64::MAX);
    let created_at = chrono::Utc::now().timestamp();
    let day = day_key(created_at);
    let (status, error) = match record.outcome {
        CallOutcome::Ok => (CallStatus::Ok, None),
        CallOutcome::Error(message) => (CallStatus::Error, Some(message)),
    };

    let account_id = record.account_id;
    let message_id = record.message_id;
    let request_id = record.request_id;
    let model = record.model;
    let pass = record.pass;
    let redaction_level = record.redaction_level;
    let usage = record.usage;

    let id = db
        .write(move |conn| {
            let tx = conn.transaction()?;
            let id: i64 = tx.query_row(
                "INSERT INTO ai_ledger (
                created_at, account_id, message_id, request_id, model, pass,
                input_tokens, output_tokens, cache_creation_input_tokens,
                cache_read_input_tokens, cost_usd, redaction_level, latency_ms,
                payload_sha256, status, error, work_class
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)
            RETURNING id",
                rusqlite::params![
                    created_at,
                    account_id,
                    message_id,
                    request_id,
                    model,
                    pass,
                    usage.input_tokens,
                    usage.output_tokens,
                    usage.cache_creation_input_tokens,
                    usage.cache_read_input_tokens,
                    cost_usd,
                    redaction_level,
                    latency_ms,
                    payload_sha256,
                    status.as_str(),
                    error,
                    work_class.as_str(),
                ],
                |row| row.get(0),
            )?;

            tx.execute(
                "INSERT INTO ai_usage (
                day, requests, input_tokens, output_tokens,
                cache_creation_input_tokens, cache_read_input_tokens, cost_usd
             ) VALUES (?1, 1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(day) DO UPDATE SET
                requests = requests + 1,
                input_tokens = input_tokens + excluded.input_tokens,
                output_tokens = output_tokens + excluded.output_tokens,
                cache_creation_input_tokens =
                    cache_creation_input_tokens + excluded.cache_creation_input_tokens,
                cache_read_input_tokens =
                    cache_read_input_tokens + excluded.cache_read_input_tokens,
                cost_usd = cost_usd + excluded.cost_usd",
                rusqlite::params![
                    day,
                    usage.input_tokens,
                    usage.output_tokens,
                    usage.cache_creation_input_tokens,
                    usage.cache_read_input_tokens,
                    cost_usd,
                ],
            )?;

            tx.commit()?;
            Ok(id)
        })
        .await
        .map_err(Error::from)?;

    tracing::Span::current().record("cost_usd", cost_usd);
    tracing::Span::current().record("id", id);
    tracing::debug!(id, cost_usd, latency_ms, "recorded AI call");
    Ok(id)
}

/// The UTC calendar day a unix timestamp falls on, as `"YYYY-MM-DD"`.
///
/// Falls back to the epoch day only if `unix_ts` is so far out of range that
/// `chrono` cannot represent it as a date at all — not a case a real call
/// timestamp (`chrono::Utc::now()`) can hit, but a `String`-returning
/// function with no `Result` needs a total answer.
fn day_key(unix_ts: i64) -> String {
    chrono::DateTime::from_timestamp(unix_ts, 0)
        .map(|dt| dt.format("%Y-%m-%d").to_string())
        .unwrap_or_else(|| "1970-01-01".to_owned())
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

/// List ledger entries matching `filter`, newest first, paginated.
///
/// `before_id` resumes after the smallest id already seen by the caller (so
/// passing back the last row's `id` from a previous page continues correctly
/// even if new calls have been recorded since); `None` starts from the
/// newest. `limit` is clamped to `1..=`[`MAX_QUERY_LIMIT`] — a non-positive or
/// absent limit falls back to [`DEFAULT_QUERY_LIMIT`].
///
/// # Errors
///
/// A mapped storage error.
pub async fn query_calls(
    db: &Database,
    filter: &AuditFilter,
    limit: i64,
    before_id: Option<i64>,
) -> Result<Vec<LedgerEntry>> {
    let limit = if limit <= 0 {
        DEFAULT_QUERY_LIMIT
    } else {
        limit.min(MAX_QUERY_LIMIT)
    };
    select_calls(db, filter, limit, before_id).await
}

/// Shared implementation behind [`query_calls`]: build a dynamic `WHERE`
/// clause from `filter` and `before_id`, then run it.
///
/// Deliberately not exposed with an unbounded/default limit of its own for
/// bulk callers to reach for — that shape (run one query, hold the whole
/// matching set in memory) is exactly what `ExportLedger` used to do and was
/// rewritten away from; a bulk consumer pages through [`query_calls`] by `id`
/// cursor instead.
async fn select_calls(
    db: &Database,
    filter: &AuditFilter,
    limit: i64,
    before_id: Option<i64>,
) -> Result<Vec<LedgerEntry>> {
    let filter = filter.clone();
    let rows = db
        .read(move |conn| {
            let mut clauses: Vec<&str> = Vec::new();
            let mut params: Vec<SqlValue> = Vec::new();

            if let Some(id) = before_id {
                clauses.push("id < ?");
                params.push(SqlValue::from(id));
            }
            if let Some(account_id) = filter.account_id {
                clauses.push("account_id = ?");
                params.push(SqlValue::from(account_id));
            }
            if let Some(message_id) = filter.message_id {
                clauses.push("message_id = ?");
                params.push(SqlValue::from(message_id));
            }
            if let Some(model) = &filter.model {
                clauses.push("model = ?");
                params.push(SqlValue::from(model.clone()));
            }
            if let Some(since) = filter.since {
                clauses.push("created_at >= ?");
                params.push(SqlValue::from(since));
            }
            if let Some(until) = filter.until {
                clauses.push("created_at <= ?");
                params.push(SqlValue::from(until));
            }
            if let Some(status) = filter.status {
                clauses.push("status = ?");
                params.push(SqlValue::from(status.as_str().to_owned()));
            }

            let where_clause = if clauses.is_empty() {
                String::new()
            } else {
                format!(" WHERE {}", clauses.join(" AND "))
            };
            params.push(SqlValue::from(limit));

            let sql = format!(
                "SELECT id, created_at, account_id, message_id, request_id, model, pass,
                        input_tokens, output_tokens, cache_creation_input_tokens,
                        cache_read_input_tokens, cost_usd, redaction_level, latency_ms,
                        payload_sha256, status, error
                 FROM ai_ledger{where_clause}
                 ORDER BY id DESC
                 LIMIT ?"
            );

            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt
                .query_map(rusqlite::params_from_iter(params.iter()), row_to_raw)?
                .collect::<rusqlite::Result<Vec<RawEntry>>>()?;
            Ok(rows)
        })
        .await?;

    rows.into_iter().map(LedgerEntry::try_from).collect()
}

/// Look up one day's rolled-up usage.
///
/// # Errors
///
/// A mapped storage error.
pub async fn usage_for_day(db: &Database, day: &str) -> Result<Option<DayUsage>> {
    let day = day.to_owned();
    db.read(move |conn| {
        conn.query_row(
            "SELECT day, requests, input_tokens, output_tokens,
                    cache_creation_input_tokens, cache_read_input_tokens, cost_usd
             FROM ai_usage WHERE day = ?1",
            rusqlite::params![day],
            |row| {
                Ok(DayUsage {
                    day: row.get(0)?,
                    requests: row.get(1)?,
                    input_tokens: row.get(2)?,
                    output_tokens: row.get(3)?,
                    cache_creation_input_tokens: row.get(4)?,
                    cache_read_input_tokens: row.get(5)?,
                    cost_usd: row.get(6)?,
                })
            },
        )
        .optional()
    })
    .await
    .map_err(Error::from)
}

/// A ledger row exactly as `rusqlite` hands it back — status still a raw wire
/// string. Kept separate from [`LedgerEntry`] so a corrupt/foreign status
/// value fails to parse at a clear boundary ([`LedgerEntry::try_from`])
/// instead of inside the row-mapping closure, where `rusqlite::Result`
/// cannot carry this crate's [`Error`].
struct RawEntry {
    id: i64,
    created_at: i64,
    account_id: Option<i64>,
    message_id: Option<i64>,
    request_id: Option<String>,
    model: String,
    pass: Option<String>,
    input_tokens: u32,
    output_tokens: u32,
    cache_creation_input_tokens: u32,
    cache_read_input_tokens: u32,
    cost_usd: f64,
    redaction_level: String,
    latency_ms: i64,
    payload_sha256: Vec<u8>,
    status: String,
    error: Option<String>,
}

fn row_to_raw(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawEntry> {
    Ok(RawEntry {
        id: row.get(0)?,
        created_at: row.get(1)?,
        account_id: row.get(2)?,
        message_id: row.get(3)?,
        request_id: row.get(4)?,
        model: row.get(5)?,
        pass: row.get(6)?,
        input_tokens: row.get(7)?,
        output_tokens: row.get(8)?,
        cache_creation_input_tokens: row.get(9)?,
        cache_read_input_tokens: row.get(10)?,
        cost_usd: row.get(11)?,
        redaction_level: row.get(12)?,
        latency_ms: row.get(13)?,
        payload_sha256: row.get(14)?,
        status: row.get(15)?,
        error: row.get(16)?,
    })
}

impl TryFrom<RawEntry> for LedgerEntry {
    type Error = Error;

    fn try_from(raw: RawEntry) -> Result<Self> {
        Ok(Self {
            id: raw.id,
            created_at: raw.created_at,
            account_id: raw.account_id,
            message_id: raw.message_id,
            request_id: raw.request_id,
            model: raw.model,
            pass: raw.pass,
            usage: Usage {
                input_tokens: raw.input_tokens,
                output_tokens: raw.output_tokens,
                cache_creation_input_tokens: raw.cache_creation_input_tokens,
                cache_read_input_tokens: raw.cache_read_input_tokens,
            },
            cost_usd: raw.cost_usd,
            redaction_level: raw.redaction_level,
            latency_ms: raw.latency_ms,
            payload_sha256: raw.payload_sha256,
            status: CallStatus::parse(&raw.status)?,
            error: raw.error,
        })
    }
}

#[cfg(test)]
mod tests;
