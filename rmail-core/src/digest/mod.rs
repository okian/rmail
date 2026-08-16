//! The AI periodic digest (task 70, prd.md feature 57): a scheduled job that
//! clusters a window's mail by thread/topic/sender and has Claude write a
//! ranked markdown briefing — needs-reply, FYI, waiting-on, auto-handled,
//! skipped — in which every line is linked to the message-ids it came from.
//!
//! ```text
//! period ──▶ select window ──▶ cluster ──▶ policy gate + pack ──▶ fence
//!        ──▶ budget ──▶ redact ──▶ Claude ──▶ parse ──▶ cite ──▶ store
//! ```
//!
//! # Three separable pieces
//!
//! - [`schedule`] answers "which periods are owed a briefing", and nothing
//!   else. It is pure arithmetic over a cursor, a cadence and a clock, which
//!   is what makes the two behaviours that matter — a daemon that was off for
//!   three days catches up, and a period already briefed is never briefed
//!   twice — testable without a database, a model or a clock.
//! - [`DigestEngine`] turns one window into one stored briefing.
//! - [`DigestScheduler`] is the loop that asks the first what to do and the
//!   second to do it.
//!
//! # Why the period, not the request, is the unit of identity
//!
//! A digest is expensive (one Sonnet call over a week of mail) and it is
//! *read once*: a second briefing of the same week is not a refreshed answer,
//! it is a second opinion nobody asked for, charged to the operator. So the
//! durable record is keyed on the window (`V41__digests.sql`'s `UNIQUE
//! (account_id, period_start, period_end)`), every entry point resolves to a
//! window before it does anything else, and a window that already has a
//! briefing is *returned*, not regenerated. Only an explicit
//! [`DigestRequest::force`] replaces one.
//!
//! That single rule covers the three failure modes a periodic job has:
//!
//! - **The daemon was off.** [`schedule::due_periods`] walks forward from the
//!   stored cursor, so every completed period since the last briefing is
//!   generated on the next tick — bounded by `digest.max_catchup_periods`, so
//!   a machine that was off for a month produces a week of briefings rather
//!   than thirty model calls in one tick.
//! - **The daemon ticked twice inside one period.** The in-progress period is
//!   never eligible ([`schedule::due_periods`] stops at the last *completed*
//!   one), and a completed one is already stored.
//! - **The window was empty.** No model is called at all — see
//!   [`DigestEngine::generate`]. The empty briefing is still *stored*, which
//!   is what stops the scheduler re-asking about a quiet week on every tick
//!   forever.
//!
//! # This is a model sink, and the fence is not optional
//!
//! Every byte of mail that reaches the prompt goes through
//! [`crate::ai::rag::context::pack`] — the same policy gate, the same
//! per-message and per-context bounds, and the same
//! [`crate::ai::injection::untrusted_block`] fence per source that
//! `AskMailbox` uses — and the system prompt carries
//! [`crate::ai::injection::with_data_boundary`]. Reusing that packer rather
//! than writing a second one is deliberate: a digest reads *more* of the
//! mailbox than any other feature in this codebase, so it is the last place a
//! second, subtly different policy check should exist.
//!
//! The one thing this module renders outside a fence is the cluster header —
//! and it is written so that it *can* be: counts, and enum values validated
//! against [`crate::ai::triage`]'s own closed vocabularies. No subject, no
//! display name, no address, no body. A header carrying "senders:
//! `<attacker>`" would have put sender-controlled text in instruction
//! position, one line above the fence built to keep it out.
//!
//! # Citations are looked up, never believed
//!
//! Sources are labelled positionally and the model cites `[n]`.
//! [`briefing::parse`] maps a label back to the source it was given, drops any
//! bullet whose labels resolve to nothing, and rewrites the survivors to
//! `[msg:<message_id>]`. There is no response that yields a briefing line
//! pointing at a message this daemon did not pack — fabrication is not
//! detected, it is unrepresentable, exactly as in [`crate::ai::rag::cite`].

pub mod briefing;
pub mod repo;
pub mod schedule;

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::ai::audit::{record_call_charged, CallOutcome, CallRecord};
use crate::ai::budget::{
    BudgetEnforcer, BudgetRequest, BudgetVerdict, WorkClass, GLOBAL_ACCOUNT_ID,
};
use crate::ai::injection;
use crate::ai::provider::{ChatRequest, Provider, Usage};
use crate::ai::queue::{payload_bytes, CapDecision, CostGate, RateLimiter};
use crate::ai::rag::context::{self, PackLimits, Source};
use crate::ai::redact::{guard, rehydrate, GuardedRequest};
use crate::ai::PolicyEngine;
use crate::config::{AiLimits, AiPrivacy, DigestConfig};
use crate::error::Error;
use crate::retrieve::cancel::interruptible_read;
use crate::storage::Database;

pub use briefing::{Briefing, Line, Section};
pub use repo::{StoredDigest, StoredSource};
pub use schedule::Period;

/// The pass name recorded in `ai_ledger.pass`, and the tracing field this path
/// is identified by.
pub const PASS: &str = "digest";

/// The account-scope sentinel for "every configured account" — the same value
/// [`crate::ai::budget::GLOBAL_ACCOUNT_ID`] uses, reused rather than
/// re-invented so `digests.account_id = 0` means the same thing everywhere.
pub const ALL_ACCOUNTS: i64 = GLOBAL_ACCOUNT_ID;

/// Hard ceiling on how many candidate rows one window scan will read,
/// regardless of `digest.max_messages`. A mailbox that synced a hundred
/// thousand messages in a week must not pull all of them through the read pool
/// to then discard all but a hundred.
const MAX_SCAN_ROWS: i64 = 5_000;

/// Floor on `digest.tick_interval`, for the reason
/// [`crate::notify::MIN_TICK_INTERVAL`] gives: a `"0s"` typo must degrade to
/// "as fast as is sane", never to a busy loop against the database.
pub const MIN_TICK_INTERVAL: Duration = Duration::from_secs(1);

/// This pass's instructions, with [`injection::DATA_BOUNDARY_CLAUSE`]
/// appended — the paragraph that gives the `⟪untrusted email⟫` delimiters in
/// the user turn their meaning. Built once into a `static` so it stays
/// byte-identical across calls and keeps sitting behind `ClaudeProvider`'s
/// prompt-cache `cache_control` boundary.
static SYSTEM_PROMPT: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| injection::with_data_boundary(SYSTEM_PROMPT_BASE));

/// Frozen, cacheable system prompt. Everything that varies per call — the
/// window, the clusters, the mail — belongs in the user turn.
///
/// The section names are interpolated from [`Section::heading`] at first use
/// rather than spelled twice, so the vocabulary the model is asked for and the
/// vocabulary [`briefing::parse`] recognizes cannot drift apart.
const SYSTEM_PROMPT_BASE: &str =
    "You are the periodic-digest stage of an email client. You are given one \
window of a user's mail, already grouped into clusters, and you write a short \
briefing about it.

Answer with GitHub-flavoured markdown and nothing else -- no preamble, no \
closing remarks, no tables, no code fences.

Use exactly these five level-two headings, in this order, and invent no \
others:

## Needs reply
## FYI
## Waiting on
## Auto-handled
## Skipped

Under each heading write zero or more `- ` bullets, most important first. \
Write `_none_` under a heading with nothing to report.

- Needs reply: mail a reasonable recipient is expected to answer, and what it \
asks of them.
- FYI: worth knowing, nothing to do.
- Waiting on: the user has already answered or asked, and is waiting on \
somebody else.
- Auto-handled: receipts, confirmations, calendar accepts and similar mail \
that needed no attention.
- Skipped: newsletters, marketing and notifications not worth the reader's \
time. One summarizing bullet is better than ten.

Every bullet MUST end with the bracketed labels of the sources it is drawn \
from, exactly as they were given to you -- write [3] or [3, 7]. A bullet with \
no label is discarded, so never write one. Only labels that appear in the \
sources exist; never invent one, and never write a number in brackets for any \
other purpose.

One bullet per distinct thing that happened. Merge a cluster into one bullet \
rather than writing one per message. Be specific -- who wants what, by when -- \
and never guess at a name, number, date or amount that is not in a source. \
Judge only from the sources given; if a body looks redacted or truncated, \
judge from what remains.";

// ---------------------------------------------------------------------------
// Requests and reports
// ---------------------------------------------------------------------------

/// One briefing to produce.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DigestRequest {
    /// Restrict to one account, or [`ALL_ACCOUNTS`] for every configured one.
    pub account_id: i64,
    /// The window to brief, half-open: `since <= t < until`.
    pub period: Period,
    /// The cadence that produced this window, `0` for an ad-hoc request. Kept
    /// on the row for reporting only — see `V41__digests.sql`.
    pub interval_seconds: i64,
    /// Replace a briefing already stored for this window rather than
    /// returning it. The scheduler never sets this.
    pub force: bool,
    /// Whether a user is waiting on the answer. Decides the budget
    /// [`WorkClass`]: a scheduled briefing is background work and is charged
    /// against the bulk sub-budget, so it cannot starve interactive calls.
    pub interactive: bool,
}

/// What [`DigestEngine::generate`] produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DigestReport {
    /// `digests.id`.
    pub id: i64,
    /// The window briefed.
    pub period: Period,
    /// The scope: 0 for every account.
    pub account_id: i64,
    /// When the stored briefing was written.
    pub generated_at: i64,
    /// The model that wrote it, empty when the window was empty.
    pub model: String,
    /// The rendered markdown.
    pub markdown: String,
    /// The parsed briefing behind that markdown.
    pub briefing: Briefing,
    /// The sources it was built from, by ascending label.
    pub sources: Vec<StoredSource>,
    /// Messages this briefing put forward, before the policy gate and the
    /// token budget cut them further. Not the size of the window — see
    /// `V41__digests.sql`.
    pub considered: u64,
    /// Messages that entered the prompt.
    pub packed: u64,
    /// Messages the AI policy withheld from the prompt.
    pub withheld: u64,
    /// Clusters the packed messages were grouped into.
    pub clusters: u64,
    /// Whether this briefing was read back rather than generated.
    pub cached: bool,
    /// Whether the window held nothing to brief, and so no model was called.
    pub empty: bool,
}

// ---------------------------------------------------------------------------
// Candidate selection and clustering
// ---------------------------------------------------------------------------

/// One message in the window, with the enum signals ranking and clustering
/// draw on. Deliberately carries no sender-authored text beyond what is needed
/// to *group* by it — nothing here is rendered outside a fence.
#[derive(Debug, Clone)]
struct Candidate {
    message_id: i64,
    cluster: ClusterKey,
    /// Triage's `priority`, when the message has a triage row and its value is
    /// in [`crate::ai::triage::PRIORITIES`]. `None` for an untriaged message.
    priority: Option<usize>,
    needs_reply: bool,
    category: Option<String>,
    date: i64,
}

/// What makes two messages "the same topic" for the purposes of one briefing.
///
/// Thread first, because a thread is the strongest topical signal in a mailbox
/// and it is already computed (task 4). Normalized subject next, which catches
/// the mail that *should* have threaded and did not (a `Re:` from a client
/// that dropped `References`). Sender last, which is what groups the
/// notification and newsletter traffic the `Skipped` section exists for.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum ClusterKey {
    Thread(i64),
    Subject(String),
    Sender(String),
    /// A message with no thread, no subject and no sender is its own cluster.
    Alone(i64),
}

/// A cluster of the window's mail, ranked.
#[derive(Debug, Clone)]
struct Cluster {
    members: Vec<Candidate>,
    /// The strongest triage priority any member carries.
    priority: Option<usize>,
    needs_reply: bool,
    /// The first member's category, taking the members in the arrival order
    /// `candidates` produced (newest first). Not a majority vote: this is a
    /// hint in the prompt header, and a hint that costs a second pass over
    /// every cluster to compute is not worth the difference.
    category: Option<String>,
    newest: i64,
}

impl Cluster {
    /// Ranking key, best first: mail that wants an answer, then by triage
    /// priority, then by recency, then by size. `message_id` of the newest
    /// member is the final tie-break so the order is total and a truncated
    /// selection is reproducible.
    fn rank(&self) -> (bool, usize, i64, usize, std::cmp::Reverse<i64>) {
        (
            self.needs_reply,
            self.priority.unwrap_or(1),
            self.newest,
            self.members.len(),
            // `Reverse`, not unary minus: negation is not total over `i64`
            // (`-i64::MIN` overflows) and this value comes from a database
            // column. The whole key is sorted descending, so wrapping this one
            // element is what makes the smallest id win the tie-break.
            std::cmp::Reverse(self.members.first().map_or(0, |m| m.message_id)),
        )
    }
}

// ---------------------------------------------------------------------------
// The engine
// ---------------------------------------------------------------------------

/// Produces one briefing per window.
///
/// Cheap to clone: a [`Database`] handle plus `Arc`s, the same "share by
/// cloning" contract every other long-lived handle in this crate follows.
#[derive(Clone)]
pub struct DigestEngine {
    db: Database,
    provider: Arc<dyn Provider>,
    policy: Arc<PolicyEngine>,
    privacy: AiPrivacy,
    limits: AiLimits,
    config: DigestConfig,
    /// `ai.limits.max_concurrency`, **shared** with the daemon's
    /// `AiWorkerPool` rather than a second semaphore of this engine's own —
    /// the same reasoning [`crate::ai::rag::RagEngine`]'s identical field
    /// gives: one process must not exceed one configured ceiling because it
    /// has four call sites.
    semaphore: Arc<Semaphore>,
    /// `ai.limits.requests_per_minute`, shared for the identical reason.
    rate_limiter: Arc<RateLimiter>,
}

impl std::fmt::Debug for DigestEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DigestEngine")
            .field("model", &self.config.model)
            .field("interval", &self.config.interval)
            .finish_non_exhaustive()
    }
}

impl DigestEngine {
    /// Build the engine over an already-constructed provider and policy
    /// engine.
    ///
    /// Every dependency is injected for the reason `RagEngine::new`
    /// documents: the daemon owns exactly one `Provider` and one `ai.limits`
    /// concurrency/pacing budget for the whole process, and a component that
    /// built its own would make both a fiction.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: Database,
        provider: Arc<dyn Provider>,
        policy: Arc<PolicyEngine>,
        privacy: AiPrivacy,
        limits: AiLimits,
        config: DigestConfig,
        semaphore: Arc<Semaphore>,
        rate_limiter: Arc<RateLimiter>,
    ) -> Self {
        Self {
            db,
            provider,
            policy,
            privacy,
            limits,
            config,
            semaphore,
            rate_limiter,
        }
    }

    /// The configured cadence, in seconds, floored at one minute.
    #[must_use]
    pub fn interval_seconds(&self) -> i64 {
        schedule::clamp_interval(
            i64::try_from(self.config.interval.as_duration().as_secs()).unwrap_or(i64::MAX),
        )
    }

    /// Produce (or return) the briefing for one window.
    ///
    /// # The empty window never becomes an empty prompt
    ///
    /// A window whose mail is entirely absent — a quiet week, or one whose
    /// every folder is `local_only` — produces a locally-authored briefing
    /// with all five sections empty, no provider call, no ledger row and no
    /// spend. Asking a model to summarize nothing is the single most
    /// avoidable cost in a periodic job: it happens on exactly the schedule
    /// the job runs on, forever, and it cannot produce anything useful. The
    /// row is still written, because the alternative is re-discovering the
    /// same emptiness on every subsequent tick.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] for a window that is not strictly ordered.
    /// [`Error::ResourceExhausted`] when a spend cap or budget refuses the
    /// call. [`Error::Internal`] when the model answered but not one line of
    /// it cited a message that was packed — a briefing with no sourced line is
    /// not a thinner briefing, it is an unusable one, and storing it would
    /// consume the window's one shot at being briefed.
    #[tracing::instrument(
        skip(self, cancel),
        fields(
            account_id = req.account_id,
            since = req.period.start,
            until = req.period.end,
            considered = tracing::field::Empty,
            packed = tracing::field::Empty,
            withheld = tracing::field::Empty,
            clusters = tracing::field::Empty,
            cached = tracing::field::Empty,
        )
    )]
    pub async fn generate(
        &self,
        req: DigestRequest,
        cancel: &CancellationToken,
    ) -> Result<DigestReport, Error> {
        if req.period.end <= req.period.start {
            return Err(Error::invalid_argument(
                "a digest window must end after it starts",
            ));
        }
        let span = tracing::Span::current();

        if !req.force {
            if let Some(stored) =
                repo::load_window(&self.db, req.account_id, req.period.start, req.period.end)
                    .await?
            {
                span.record("cached", true);
                tracing::debug!(digest_id = stored.id, "returning the stored briefing");
                return Ok(cached_report(stored));
            }
        }
        span.record("cached", false);

        let candidates = self.candidates(&req, cancel).await?;
        let clusters = cluster(candidates, &self.config);
        let ordered: Vec<i64> = clusters
            .iter()
            .flat_map(|c| c.members.iter().map(|m| m.message_id))
            .collect();
        debug_assert!(
            ordered.len() <= context::MAX_FETCH,
            "`cluster` must not hand the packer more ids than it will fetch"
        );
        let packed = context::pack(
            &self.db,
            &ordered,
            &self.policy,
            PackLimits {
                max_context_tokens: self.config.max_context_tokens as usize,
                max_chars_per_message: self.config.max_chars_per_message as usize,
            },
            self.privacy.max_body_chars as usize,
            cancel,
        )
        .await?;
        span.record("considered", ordered.len());
        span.record("packed", packed.sources.len());
        span.record("withheld", packed.withheld_by_policy);

        // Regrouped *after* packing, not before: the policy gate and the token
        // budget both drop messages, and a cluster header claiming three
        // messages while showing one would be this module lying to the model
        // about its own context.
        let grouped = regroup(&clusters, &packed.sources);
        span.record("clusters", grouped.len());

        if packed.sources.is_empty() {
            // An empty pack is about to be written down as "nothing happened
            // this period", under a UNIQUE key that makes it the *final* word
            // on the window — only `force` can ever replace it. So it has to
            // be the truth, and there are two ways for it not to be.
            //
            // `candidates` already guards its own scan against cancellation
            // for exactly this reason. `pack` needs the same guard and cannot
            // provide it: `context::pack`'s fetch answers a cancelled read
            // with an *empty map*, not an error (see its own docs), so a
            // shutdown token firing between the candidate scan and the fetch
            // — i.e. any daemon restart mid-tick — silently turns a busy week
            // into a quiet one, permanently.
            //
            // The second check is the general form and catches more than
            // cancellation: `pack` reaches a verdict (packed, withheld, or
            // dropped for budget) on every candidate it actually read, so a
            // candidate with no verdict at all is a row the fetch did not
            // return. One or two of those are ordinary (a message expunged
            // between selection and packing); *all* of them is the fetch
            // having failed to happen.
            let accounted =
                packed.sources.len() + packed.withheld_by_policy + packed.dropped_for_budget;
            if !empty_pack_is_credible(cancel.is_cancelled(), ordered.len(), accounted) {
                return Err(Error::unavailable(format!(
                    "the digest selected {} message(s) for this window but the context could not \
                     be assembled (cancelled: {}); the window is left unbriefed rather than \
                     recorded as empty",
                    ordered.len(),
                    cancel.is_cancelled(),
                )));
            }
            tracing::info!(
                considered = ordered.len(),
                withheld = packed.withheld_by_policy,
                "the digest window holds nothing to brief; recording an empty briefing without \
                 calling the provider"
            );
            return self
                .store(
                    &req,
                    briefing::empty_briefing(),
                    &packed,
                    &grouped,
                    "",
                    None,
                )
                .await;
        }

        let prompt = render_prompt(&req.period, &grouped, &packed.sources);
        let (text, model, ledger_entry_id) = self.call(&req, prompt, cancel).await?;
        let parsed = briefing::parse(&text, &packed.sources);
        if parsed.is_empty() {
            // Deliberately an error rather than a stored empty briefing: the
            // window *had* mail, so "nothing to report" is not a true
            // statement about it, and storing it would burn this window's one
            // briefing on a response that said nothing. Erroring leaves the
            // window unbriefed, which the scheduler retries on its next tick.
            return Err(Error::internal(format!(
                "the digest model produced no line citing any of the {} messages it was given \
                 ({} bullets were dropped for citing nothing)",
                packed.sources.len(),
                parsed.dropped_uncited
            )));
        }
        tracing::info!(
            lines = parsed.line_count(),
            dropped_uncited = parsed.dropped_uncited,
            dangling = parsed.dangling,
            sources = packed.sources.len(),
            "digest briefing written"
        );
        self.store(&req, parsed, &packed, &grouped, &model, ledger_entry_id)
            .await
    }

    /// The window's messages, ranked, with the signals clustering needs.
    async fn candidates(
        &self,
        req: &DigestRequest,
        cancel: &CancellationToken,
    ) -> Result<Vec<Candidate>, Error> {
        let account_id = req.account_id;
        let (start, end) = (req.period.start, req.period.end);
        // The most recent triage row per message, chosen by a correlated
        // subquery rather than a plain join: `ai_summaries` is keyed
        // `(message_id, pass, model)`, so an operator who changed
        // `ai.models.triage` has two triage rows for the same message and a
        // join would duplicate it into two candidates.
        let sql = format!(
            "SELECT m.id, m.thread_id, m.subject, m.from_addr, \
                    COALESCE(m.date, m.internaldate, 0), s.priority, s.needs_reply, s.category \
             FROM messages m \
             LEFT JOIN ai_summaries s ON s.id = ( \
                 SELECT id FROM ai_summaries \
                 WHERE message_id = m.id AND pass = 'triage' \
                 ORDER BY created_at DESC, id DESC LIMIT 1 \
             ) \
             WHERE (?1 = 0 OR m.account_id = ?1) \
               AND COALESCE(m.date, m.internaldate, 0) >= ?2 \
               AND COALESCE(m.date, m.internaldate, 0) < ?3 \
             ORDER BY COALESCE(m.date, m.internaldate, 0) DESC, m.id DESC \
             LIMIT {MAX_SCAN_ROWS}"
        );
        let rows = interruptible_read(&self.db, cancel, move |conn| {
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map(rusqlite::params![account_id, start, end], |row| {
                let thread_id: Option<i64> = row.get(1)?;
                let subject: Option<String> = row.get(2)?;
                let from_addr: Option<String> = row.get(3)?;
                let message_id: i64 = row.get(0)?;
                Ok(Candidate {
                    message_id,
                    cluster: cluster_key(
                        message_id,
                        thread_id,
                        subject.as_deref(),
                        from_addr.as_deref(),
                    ),
                    priority: row
                        .get::<_, Option<String>>(5)?
                        .and_then(|p| priority_rank(&p)),
                    needs_reply: row.get::<_, Option<i64>>(6)?.unwrap_or(0) != 0,
                    category: row.get::<_, Option<String>>(7)?.and_then(known_category),
                    date: row.get(4)?,
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
        .await?;
        // A cancelled scan is an empty window, and an empty window would be
        // *stored* as a briefing saying nothing happened. That is a lie the
        // caller must not be allowed to tell, so cancellation surfaces.
        rows.ok_or_else(|| {
            Error::cancelled("the digest window scan was cancelled before it finished".to_owned())
        })
    }

    /// Pace, budget, redact, call, rehydrate. Returns the answer text, the
    /// model that actually produced it, and the ledger row it was audited
    /// under.
    async fn call(
        &self,
        req: &DigestRequest,
        prompt: String,
        cancel: &CancellationToken,
    ) -> Result<(String, String, Option<i64>), Error> {
        // Concurrency and pacing first, then the budget, then the call — the
        // order `ai::queue` and `ai::rag` both document at length: a budget
        // check taken before an unbounded wait can be arbitrarily stale by the
        // time the call is made, and what bounds the overshoot is how many
        // checks can be outstanding at once, which is what the shared
        // semaphore caps.
        let _permit = tokio::select! {
            () = cancel.cancelled() => {
                return Err(Error::cancelled(
                    "cancelled while waiting for AI concurrency capacity".to_owned(),
                ));
            }
            permit = Arc::clone(&self.semaphore).acquire_owned() => permit
                .map_err(|_| Error::internal("the ai concurrency semaphore was closed"))?,
        };
        tokio::select! {
            () = cancel.cancelled() => {
                return Err(Error::cancelled(
                    "cancelled while waiting for AI rate-limit capacity".to_owned(),
                ));
            }
            () = self.rate_limiter.acquire() => {}
        }

        let work_class = if req.interactive {
            WorkClass::Interactive
        } else {
            WorkClass::Bulk
        };
        let model = self.budgeted_model(work_class).await?;
        let request = ChatRequest::new(model, self.config.max_tokens.max(512))
            .system(SYSTEM_PROMPT.as_str())
            .user(prompt);

        // The firewall. Nothing between here and `provider.complete` may add
        // text to the request.
        let GuardedRequest::Redacted {
            request, tokens, ..
        } = guard(&request, &self.privacy)
        else {
            return Err(Error::failed_precondition(
                "nothing was left to brief on once PII was redacted from the window's messages"
                    .to_owned(),
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
        match response {
            Ok(response) => {
                let ledger = self
                    .audit(
                        req,
                        &request.model,
                        &payload,
                        redaction_level,
                        work_class,
                        started.elapsed(),
                        Some(response.usage),
                        CallOutcome::Ok,
                    )
                    .await;
                Ok((rehydrate(&response.text, &tokens), request.model, ledger))
            }
            Err(error) => {
                self.audit(
                    req,
                    &request.model,
                    &payload,
                    redaction_level,
                    work_class,
                    started.elapsed(),
                    None,
                    CallOutcome::Error(error.to_string()),
                )
                .await;
                Err(error)
            }
        }
    }

    /// Consult the daemon-wide spend cap and this call's own budget, and
    /// return the model to actually use.
    async fn budgeted_model(&self, work_class: WorkClass) -> Result<String, Error> {
        let gate = CostGate {
            db: &self.db,
            limits: &self.limits,
        };
        match gate.decide().await? {
            CapDecision::Open => {}
            other => {
                return Err(Error::resource_exhausted(format!(
                    "the AI spend cap is closed ({other:?}); the digest cannot run until it \
                     resets or an operator raises the cap"
                )));
            }
        }
        // Charged to the global budget: a digest spans every configured
        // account by default, so there is no single account it is "for" — the
        // identical reasoning `ai::rag` gives.
        let verdict = BudgetEnforcer {
            db: &self.db,
            limits: &self.limits,
        }
        .evaluate(&BudgetRequest {
            account_id: GLOBAL_ACCOUNT_ID,
            model: &self.config.model,
            work_class,
            now: chrono::Utc::now().timestamp(),
        })
        .await?;
        match verdict {
            BudgetVerdict::Allow => Ok(self.config.model.clone()),
            BudgetVerdict::Downgrade { model, reason } => {
                tracing::info!(
                    from = %self.config.model,
                    to = %model,
                    reason = %reason,
                    "ai budget soft cap: downgrading the digest model"
                );
                Ok(model)
            }
            // The detailed reason names aggregate spend figures and reading
            // spend needs `admin`, so the detail goes to the log and the caller
            // is told only that a cap was reached — the same split
            // `ai::rag`/`rmaild::ai_service` apply.
            BudgetVerdict::Block { reason, .. } => {
                tracing::info!(reason = %reason, "ai budget hard cap: refusing the digest");
                Err(Error::resource_exhausted(
                    "an AI spend budget has been reached; the digest cannot run until the \
                     window resets or an operator raises the budget"
                        .to_owned(),
                ))
            }
        }
    }

    /// One ledger row. Never propagates: an audit write that fails must not
    /// turn a produced briefing into an error.
    #[allow(clippy::too_many_arguments)]
    async fn audit(
        &self,
        req: &DigestRequest,
        model: &str,
        payload: &[u8],
        redaction_level: &str,
        work_class: WorkClass,
        latency: Duration,
        usage: Option<Usage>,
        outcome: CallOutcome,
    ) -> Option<i64> {
        let record = CallRecord {
            account_id: (req.account_id != ALL_ACCOUNTS).then_some(req.account_id),
            // A digest is about a *window*, not a message: attributing it to
            // one of its sources would make `mail ai audit --message <id>`
            // claim a call was made "for" a message that merely appeared in a
            // briefing's context.
            message_id: None,
            request_id: None,
            model: model.to_owned(),
            pass: Some(PASS.to_owned()),
            usage: usage.unwrap_or_default(),
            redaction_level: redaction_level.to_owned(),
            latency,
            payload,
            outcome,
        };
        // `record_call_charged`, not `record_call`: a scheduled briefing is
        // `WorkClass::Bulk`, and a bulk call recorded as interactive is a call
        // the bulk sub-budget never sees — which is precisely the accounting
        // that stops a background job from starving the interactive one it
        // shares `ai.limits` with.
        match record_call_charged(&self.db, record, 1.0, work_class).await {
            Ok(id) => Some(id),
            Err(error) => {
                tracing::warn!(%error, "could not write the digest audit entry");
                None
            }
        }
    }

    /// Persist one briefing and read it back as a report.
    async fn store(
        &self,
        req: &DigestRequest,
        parsed: Briefing,
        packed: &context::Packed,
        grouped: &[GroupedCluster],
        model: &str,
        ledger_entry_id: Option<i64>,
    ) -> Result<DigestReport, Error> {
        let markdown = parsed.render();
        let sources: Vec<StoredSource> = packed
            .sources
            .iter()
            .enumerate()
            .map(|(index, source)| StoredSource {
                label: u32::try_from(index + 1).unwrap_or(u32::MAX),
                message_id: source.message_id,
                message_uid: source.message_uid,
                account_id: source.account_id,
                mailbox: source.mailbox.clone(),
                subject: source.subject.clone(),
                from_addr: source.from_addr.clone(),
                date: source.date,
                cited: parsed.cited.contains(&index),
            })
            .collect();
        // `retrieved`, not `packed + withheld + dropped_for_budget`: those
        // three account for candidates the packer had a *verdict* on, and a
        // candidate whose row vanished between selection and packing gets no
        // verdict at all (`context::pack` skips it silently). Summing the
        // verdicts would then under-report what this window actually held,
        // which is the one number a reader uses to tell a thin briefing from a
        // quiet week.
        let considered = packed.retrieved;
        let new = repo::NewDigest {
            account_id: req.account_id,
            period_start: req.period.start,
            period_end: req.period.end,
            interval_seconds: req.interval_seconds,
            model: model.to_owned(),
            markdown: markdown.clone(),
            considered: i64::try_from(considered).unwrap_or(i64::MAX),
            packed: i64::try_from(packed.sources.len()).unwrap_or(i64::MAX),
            withheld: i64::try_from(packed.withheld_by_policy).unwrap_or(i64::MAX),
            clusters: i64::try_from(grouped.len()).unwrap_or(i64::MAX),
            dropped_uncited: i64::try_from(parsed.dropped_uncited).unwrap_or(i64::MAX),
            ledger_entry_id,
            sources: sources.clone(),
        };
        let id = repo::store(&self.db, new).await?;
        Ok(DigestReport {
            id,
            period: req.period,
            account_id: req.account_id,
            generated_at: chrono::Utc::now().timestamp(),
            model: model.to_owned(),
            markdown,
            empty: packed.sources.is_empty(),
            briefing: parsed,
            sources,
            considered: considered as u64,
            packed: packed.sources.len() as u64,
            withheld: packed.withheld_by_policy as u64,
            clusters: grouped.len() as u64,
            cached: false,
        })
    }
}

/// A stored briefing, projected back as a report.
///
/// The parsed [`Briefing`] is re-derived from the stored markdown against the
/// stored sources, so a cached report carries the same structured sections a
/// freshly generated one does. Re-parsing is sound because the markdown was
/// rendered by [`Briefing::render`] — the citations in it are already
/// `[msg:<id>]` rather than positional labels, which no longer resolve, so the
/// section structure is what re-parsing recovers.
fn cached_report(stored: StoredDigest) -> DigestReport {
    let mut sections: Vec<(Section, Vec<Line>)> = Section::ALL
        .into_iter()
        .map(|section| (section, Vec::new()))
        .collect();
    let by_id: BTreeMap<i64, ()> = stored.sources.iter().map(|s| (s.message_id, ())).collect();
    let mut current: Option<Section> = None;
    for raw in stored.markdown.lines() {
        let line = raw.trim();
        if let Some(rest) = line.strip_prefix("## ") {
            current = Section::from_heading(rest.trim());
            continue;
        }
        let (Some(section), Some(text)) = (current, line.strip_prefix("- ")) else {
            continue;
        };
        let ids: Vec<i64> = stored_ids(text)
            .into_iter()
            .filter(|id| by_id.contains_key(id))
            .collect();
        if ids.is_empty() {
            continue;
        }
        if let Some((_, lines)) = sections.iter_mut().find(|(s, _)| *s == section) {
            lines.push(Line {
                text: text.to_owned(),
                message_ids: ids,
            });
        }
    }
    DigestReport {
        id: stored.id,
        period: Period {
            start: stored.period_start,
            end: stored.period_end,
        },
        account_id: stored.account_id,
        generated_at: stored.generated_at,
        model: stored.model,
        markdown: stored.markdown,
        briefing: Briefing {
            sections,
            dropped_uncited: usize::try_from(stored.dropped_uncited).unwrap_or(0),
            dangling: 0,
            cited: std::collections::BTreeSet::new(),
        },
        empty: stored.packed == 0,
        sources: stored.sources,
        considered: stored.considered.try_into().unwrap_or(0),
        packed: stored.packed.try_into().unwrap_or(0),
        withheld: stored.withheld.try_into().unwrap_or(0),
        clusters: stored.clusters.try_into().unwrap_or(0),
        cached: true,
    }
}

/// Whether an empty pack can be believed as "this window held nothing".
///
/// An empty pack is about to be written down under a UNIQUE key that makes it
/// the *final* word on the window — only `force` can ever replace it — so it
/// has to be the truth, and there are two ways for it not to be.
///
/// - **Cancelled.** [`DigestEngine::candidates`] already guards its own scan.
///   [`context::pack`] needs the same guard and cannot provide it: its fetch
///   answers a cancelled read with an *empty map*, not an error (see its own
///   docs), so a shutdown token firing between the candidate scan and the
///   fetch — that is, any daemon restart landing mid-tick — would silently
///   turn a busy week into a quiet one, permanently.
/// - **Nothing was accounted for.** [`context::pack`] reaches a verdict
///   (packed, withheld by policy, or dropped for budget) on every candidate it
///   actually read, so a candidate with no verdict at all is a row the fetch
///   did not return. One or two of those are ordinary — a message expunged
///   between selection and packing — but *all* of them is the fetch having
///   failed to happen, cancellation being only the likeliest cause.
///
/// A pure predicate rather than two inline `if`s so both arms are provable
/// without arranging a cancellation to land inside a specific await point: the
/// wiring is covered by the engine test that cancels a digest outright, and
/// every combination of the inputs is covered here.
const fn empty_pack_is_credible(cancelled: bool, retrieved: usize, accounted: usize) -> bool {
    !cancelled && (retrieved == 0 || accounted > 0)
}

/// Every `msg:<id>` a rendered briefing line names.
fn stored_ids(text: &str) -> Vec<i64> {
    let mut out = Vec::new();
    let mut rest = text;
    // Only inside a `[...]` group, which is the one form
    // [`Briefing::render`] emits for a resolved citation. A bare `msg:20`
    // anywhere else in the prose is *not* one — most importantly the
    // `(msg:20)` [`briefing::rewrite`] produces when it neutralizes a citation
    // the model wrote itself. Scanning the whole line would let that
    // neutralized text become a citation again on the read-back path, undoing
    // on the cached side exactly what the fresh side just prevented.
    while let Some(open) = rest.find('[') {
        let after = rest.get(open + 1..).unwrap_or_default();
        let Some(close) = after.find(']') else {
            break;
        };
        let inner = after.get(..close).unwrap_or_default();
        for part in inner.split(',') {
            let Some(digits) = part.trim().strip_prefix("msg:") else {
                continue;
            };
            if let Ok(id) = digits.parse::<i64>() {
                if !out.contains(&id) {
                    out.push(id);
                }
            }
        }
        rest = after.get(close + 1..).unwrap_or_default();
    }
    out
}

// ---------------------------------------------------------------------------
// Clustering
// ---------------------------------------------------------------------------

/// This message's cluster: thread, else normalized subject, else sender.
fn cluster_key(
    message_id: i64,
    thread_id: Option<i64>,
    subject: Option<&str>,
    from_addr: Option<&str>,
) -> ClusterKey {
    if let Some(thread) = thread_id {
        return ClusterKey::Thread(thread);
    }
    let normalized = subject.map(normalize_subject).unwrap_or_default();
    if !normalized.is_empty() {
        return ClusterKey::Subject(normalized);
    }
    match from_addr.map(str::trim).filter(|s| !s.is_empty()) {
        Some(addr) => ClusterKey::Sender(addr.to_lowercase()),
        None => ClusterKey::Alone(message_id),
    }
}

/// A subject with its reply/forward prefixes and case removed, so `Re: Re:
/// Invoice` and `FWD: invoice` land in the same cluster.
fn normalize_subject(subject: &str) -> String {
    let mut rest = subject.trim();
    loop {
        let lower = rest.to_ascii_lowercase();
        let stripped = ["re:", "fw:", "fwd:", "aw:", "sv:", "vs:", "re :"]
            .iter()
            .find_map(|prefix| lower.starts_with(prefix).then_some(prefix.len()));
        match stripped {
            Some(len) => rest = rest.get(len..).unwrap_or_default().trim_start(),
            None => break,
        }
    }
    rest.split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Triage's priority ladder as a rank, or `None` for a value outside it.
///
/// Validated against [`crate::ai::triage::PRIORITIES`] rather than parsed
/// loosely: this value is rendered into the prompt *outside* the untrusted
/// fence, so it has to come from a closed vocabulary this codebase controls. A
/// row holding anything else (an older schema, a hand-edited database) simply
/// has no priority signal.
fn priority_rank(value: &str) -> Option<usize> {
    crate::ai::triage::PRIORITIES
        .iter()
        .position(|p| *p == value)
}

/// A triage category, if it is one this codebase recognizes. Same reasoning as
/// [`priority_rank`]: the category is rendered outside the fence.
fn known_category(value: String) -> Option<String> {
    crate::ai::triage::CATEGORIES
        .contains(&value.as_str())
        .then_some(value)
}

/// Group ranked candidates into clusters, then take the best of them within
/// `digest.max_clusters` / `digest.max_messages`.
fn cluster(candidates: Vec<Candidate>, config: &DigestConfig) -> Vec<Cluster> {
    let mut by_key: BTreeMap<ClusterKey, Cluster> = BTreeMap::new();
    for candidate in candidates {
        let entry = by_key
            .entry(candidate.cluster.clone())
            .or_insert_with(|| Cluster {
                members: Vec::new(),
                priority: None,
                needs_reply: false,
                category: None,
                newest: i64::MIN,
            });
        entry.priority = entry.priority.max(candidate.priority);
        entry.needs_reply |= candidate.needs_reply;
        entry.newest = entry.newest.max(candidate.date);
        if entry.category.is_none() {
            entry.category.clone_from(&candidate.category);
        }
        entry.members.push(candidate);
    }

    let mut clusters: Vec<Cluster> = by_key.into_values().collect();
    for cluster in &mut clusters {
        // Newest first within a cluster, so a truncated cluster keeps the
        // latest state of a thread rather than its opening message.
        cluster
            .members
            .sort_by_key(|m| (std::cmp::Reverse(m.date), m.message_id));
    }
    clusters.sort_by_key(|cluster| std::cmp::Reverse(cluster.rank()));

    let max_clusters = (config.max_clusters as usize).max(1);
    // Clamped to what `context::pack` will actually fetch. Everything past
    // that ceiling is *silently* absent from the pack — not withheld, not
    // dropped for budget, simply never read — so a `digest.max_messages` above
    // it would make this module report a coverage it never had.
    let max_messages = (config.max_messages as usize).clamp(1, context::MAX_FETCH);
    let mut taken = 0usize;
    let mut out = Vec::new();
    for mut cluster in clusters.into_iter().take(max_clusters) {
        if taken >= max_messages {
            break;
        }
        // A cluster is truncated rather than dropped when the message budget
        // runs out mid-way: losing the tail of a thread still leaves the
        // briefing able to say what the thread is about, where dropping the
        // whole cluster would lose the topic entirely.
        cluster.members.truncate(max_messages - taken);
        taken += cluster.members.len();
        if !cluster.members.is_empty() {
            out.push(cluster);
        }
    }
    out
}

/// A cluster as the prompt presents it: the labels its packed messages got,
/// plus the enum signals its header carries.
#[derive(Debug, Clone, PartialEq, Eq)]
struct GroupedCluster {
    /// 1-based labels into [`context::Packed::sources`].
    labels: Vec<usize>,
    priority: Option<usize>,
    needs_reply: bool,
    category: Option<String>,
}

/// Re-attach packed sources to their clusters, dropping clusters the policy
/// gate or the token budget emptied.
fn regroup(clusters: &[Cluster], sources: &[Source]) -> Vec<GroupedCluster> {
    let position: BTreeMap<i64, usize> = sources
        .iter()
        .enumerate()
        .map(|(index, source)| (source.message_id, index + 1))
        .collect();
    clusters
        .iter()
        .filter_map(|cluster| {
            let labels: Vec<usize> = cluster
                .members
                .iter()
                .filter_map(|m| position.get(&m.message_id).copied())
                .collect();
            (!labels.is_empty()).then(|| GroupedCluster {
                labels,
                priority: cluster.priority,
                needs_reply: cluster.needs_reply,
                category: cluster.category.clone(),
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// The prompt
// ---------------------------------------------------------------------------

/// The user turn: the window, then one engine-authored header per cluster with
/// its sources fenced beneath it.
///
/// # Nothing sender-authored appears outside a fence
///
/// The header carries a message count and enum values only — see this module's
/// own docs. `Source::render` is what emits the label line and the
/// [`injection::untrusted_block`] around the subject, address, date and body,
/// and it is reused verbatim rather than re-implemented so a digest's fencing
/// cannot drift from `AskMailbox`'s.
fn render_prompt(period: &Period, clusters: &[GroupedCluster], sources: &[Source]) -> String {
    let mut out = String::with_capacity(sources.len() * 1_024 + 512);
    out.push_str("Window: ");
    out.push_str(&format_day(period.start));
    out.push_str(" to ");
    out.push_str(&format_day(period.end));
    out.push_str(" (UTC).\n");
    out.push_str(&format!(
        "{} message(s) in {} cluster(s).\n\n",
        sources.len(),
        clusters.len()
    ));
    for (index, cluster) in clusters.iter().enumerate() {
        out.push_str(&format!(
            "### Cluster {} — {} message(s)",
            index + 1,
            cluster.labels.len()
        ));
        let mut signals: Vec<String> = Vec::new();
        if let Some(rank) = cluster.priority {
            if let Some(name) = crate::ai::triage::PRIORITIES.get(rank) {
                signals.push(format!("triage priority: {name}"));
            }
        }
        if cluster.needs_reply {
            signals.push("triage flagged needs-reply".to_owned());
        }
        if let Some(category) = &cluster.category {
            signals.push(format!("triage category: {category}"));
        }
        if !signals.is_empty() {
            out.push_str(" (");
            out.push_str(&signals.join("; "));
            out.push(')');
        }
        out.push('\n');
        for label in &cluster.labels {
            if let Some(source) = sources.get(label - 1) {
                out.push_str(&source.render(*label));
            }
        }
        out.push('\n');
    }
    out.push_str(
        "Write the briefing for this window, using only the sources above and citing every \
         bullet with the bracketed labels it came from.\n",
    );
    out
}

/// `YYYY-MM-DD` for a unix second, or the raw number if it is out of range.
fn format_day(at: i64) -> String {
    chrono::DateTime::from_timestamp(at, 0)
        .map_or_else(|| at.to_string(), |dt| dt.format("%Y-%m-%d").to_string())
}

// ---------------------------------------------------------------------------
// The scheduler
// ---------------------------------------------------------------------------

/// What one [`DigestScheduler::tick`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DigestTickReport {
    /// Periods that were owed a briefing when the tick started.
    pub due: usize,
    /// Briefings generated this tick.
    pub generated: usize,
    /// Periods whose briefing already existed (a manual `mail digest` for the
    /// same window, or a previous tick that raced this one).
    pub already_briefed: usize,
    /// Periods whose generation failed; they stay due and are retried.
    pub failed: usize,
}

/// The loop that keeps the digest on its cadence.
///
/// Polls rather than subscribing, for the reason [`crate::notify`]'s delivery
/// loop gives: a period becomes due because *time passed*, not because
/// anything happened, so a subscription to mailbox events would sleep straight
/// through a quiet week's boundary.
#[derive(Clone)]
pub struct DigestScheduler {
    engine: DigestEngine,
    db: Database,
    account_id: i64,
    tick_interval: Duration,
    max_catchup: usize,
}

impl std::fmt::Debug for DigestScheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DigestScheduler")
            .field("account_id", &self.account_id)
            .field("tick_interval", &self.tick_interval)
            .field("max_catchup", &self.max_catchup)
            .finish_non_exhaustive()
    }
}

impl DigestScheduler {
    /// A scheduler driving `engine` over every configured account.
    #[must_use]
    pub fn new(engine: DigestEngine, db: Database) -> Self {
        let tick_interval = engine
            .config
            .tick_interval
            .as_duration()
            .max(MIN_TICK_INTERVAL);
        let max_catchup = (engine.config.max_catchup_periods as usize).max(1);
        Self {
            engine,
            db,
            account_id: ALL_ACCOUNTS,
            tick_interval,
            max_catchup,
        }
    }

    /// How often [`Self::spawn`]'s loop ticks.
    #[must_use]
    pub fn tick_interval(&self) -> Duration {
        self.tick_interval
    }

    /// Generate every completed period that has not been briefed yet.
    ///
    /// The in-progress period is never eligible: briefing a week before it has
    /// finished would consume that week's one briefing on partial data, and
    /// `UNIQUE (account_id, period_start, period_end)` means there is no second
    /// chance at it.
    ///
    /// # A failed period stops the tick, rather than being skipped past
    ///
    /// Periods are generated oldest-first and the cursor is `MAX(period_end)`,
    /// so a later period that succeeds moves the cursor *past* an earlier one
    /// that failed — and `due_periods` would never offer the failed one again.
    /// Catching up on three days with day one failing would lose day one
    /// permanently, while the log claimed it would be retried.
    ///
    /// So the first failure ends the tick. The periods behind it stay due,
    /// the cursor stays where it was, and the next tick starts again from the
    /// failed one. The cost is that a single persistently-bad window stalls
    /// the catch-up behind it — which is the right trade for a job whose
    /// output is a permanent, un-revisitable record: a stalled backlog is
    /// visible in the logs and recoverable, a silently-dropped period is
    /// neither.
    ///
    /// # Errors
    ///
    /// Only a failure to read the cursor. A failure to generate one period is
    /// logged and counted, not returned.
    #[tracing::instrument(skip(self, cancel), fields(due, generated, failed))]
    pub async fn tick(&self, cancel: &CancellationToken) -> Result<DigestTickReport, Error> {
        let interval = self.engine.interval_seconds();
        let now = chrono::Utc::now().timestamp();
        let cursor = repo::latest_period_end(&self.db, self.account_id, now).await?;
        let due = schedule::due_periods(cursor, now, interval, self.max_catchup);
        let mut report = DigestTickReport {
            due: due.len(),
            ..DigestTickReport::default()
        };
        for period in due {
            if cancel.is_cancelled() {
                break;
            }
            let req = DigestRequest {
                account_id: self.account_id,
                period,
                interval_seconds: interval,
                force: false,
                interactive: false,
            };
            match self.engine.generate(req, cancel).await {
                Ok(digest) if digest.cached => report.already_briefed += 1,
                Ok(digest) => {
                    report.generated += 1;
                    tracing::info!(
                        digest_id = digest.id,
                        since = period.start,
                        until = period.end,
                        packed = digest.packed,
                        empty = digest.empty,
                        "periodic digest generated"
                    );
                }
                Err(error) => {
                    report.failed += 1;
                    tracing::warn!(
                        %error,
                        since = period.start,
                        until = period.end,
                        remaining = report.due - report.generated - report.already_briefed
                            - report.failed,
                        "periodic digest failed for this period; this tick stops here so the \
                         cursor cannot advance past it, and it is retried next tick"
                    );
                    break;
                }
            }
        }
        let span = tracing::Span::current();
        span.record("due", report.due);
        span.record("generated", report.generated);
        span.record("failed", report.failed);
        Ok(report)
    }

    /// Spawn the periodic loop, running once immediately — so a daemon
    /// restarted more often than its own tick interval still makes progress,
    /// the same reasoning every other loop in this workspace applies to itself
    /// — and then on `digest.tick_interval`, until `cancel` fires.
    pub fn spawn(self, cancel: CancellationToken) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                match self.tick(&cancel).await {
                    Ok(report) => tracing::debug!(?report, "digest tick"),
                    Err(error) => tracing::warn!(%error, "digest tick failed"),
                }
                tokio::select! {
                    () = cancel.cancelled() => return,
                    () = tokio::time::sleep(self.tick_interval) => {}
                }
            }
        })
    }
}
