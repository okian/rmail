//! The notification scoring pass: one cheap Haiku call per newly synced
//! message answering "does this deserve to interrupt someone, and why".
//!
//! # Why this is a queue pass and not a bespoke provider call
//!
//! [`NotifyPassHandler`] is a [`PassHandler`], registered with the same
//! [`crate::ai::queue::AiWorkerPool`] as triage and the deep pass, and that is
//! the whole design. Everything a model call in this codebase has to do —
//! resolve the AI policy for the account *and* folder before assembling
//! anything, run the text through the redaction firewall, respect the daily
//! cost gate and the per-account budget, pace itself against the shared
//! semaphore and rate limiter, and land a row in the audit ledger — already
//! lives in [`crate::ai::queue`]. A notification engine that called
//! [`crate::ai::provider::Provider::complete`] itself would have to reproduce
//! all of it, and the first thing it would get wrong is the ordering, which is
//! the security property (see [`crate::rules::gate`]'s module docs on exactly
//! that).
//!
//! # Why this is a second call and not a read of triage's `priority`
//!
//! Triage (task 48) already writes a `priority` for every message, and the
//! obvious saving is to gate notifications on that instead of paying for a
//! second call. It is deliberately not done, for one reason: `priority` and
//! "should this interrupt you" are different questions, and only the second
//! one has a cost attached to being wrong. Triage ranks a message against
//! *mail*; this ranks it against *the user's attention right now*, which is
//! why the prompt below talks about interruption, about newsletters that are
//! genuinely interesting but never urgent, and about the asymmetry between a
//! missed page and a spurious one. Folding the two together would mean either
//! notifications inherit a scale tuned for search ranking, or triage's
//! `priority` drifts toward "would ping" and every `ai:priority>=high` query
//! in the mailbox changes meaning.
//!
//! The cost is bounded and visible: `notify.enabled` is off by default (see
//! [`crate::config::NotifyConfig::enabled`]), the model is
//! `ai.models.notify` (Haiku, per prd.md #62), and the answer is two short
//! fields.
//!
//! # The tier vocabulary is triage's, imported not copied
//!
//! [`Tier`]'s wire strings are exactly [`crate::ai::triage::PRIORITIES`],
//! asserted by `notify::tests::the_tier_vocabulary_is_triages_priorities`. Two
//! hand-maintained priority ladders in one codebase would diverge the first
//! time either grew a value, and an operator reading `threshold = "high"` in
//! `[notify]` has every right to expect it to mean what `high` means in
//! `[ai.deep_pass]`.
//!
//! # A row is written before anything is delivered
//!
//! [`PassHandler::on_success`] persists the verdict to `notifications` and
//! stops. It does not deliver, and it must not: `on_success` runs inside the
//! queue's dispatch tail, its failures are retried by re-running the *model
//! call*, and — as [`crate::ai::triage`]'s own docs set out — a reaped lease
//! can let it run twice for the same message. Delivery is a side effect on a
//! human being; it belongs behind the durable `UNIQUE (message_id)` row and
//! the state machine in [`super::repo`], driven by
//! [`super::NotifyEngine::tick`].

use std::time::Duration;

use async_trait::async_trait;
use rusqlite::OptionalExtension;
use serde::Deserialize;

use crate::ai::provider::{ChatRequest, OutputFormat};
use crate::ai::queue::{AiLease, MessageContent, PassHandler};
use crate::ai::triage::PRIORITIES;
use crate::ai::{injection, triage};
use crate::config::AiInjection;
use crate::error::Error;
use crate::storage::Database;

use super::{repo, NotifyPolicy};

/// The wire value of `ai_queue.pass` / `ai_ledger.pass` this handler answers
/// to.
pub const PASS: &str = "notify";

/// A tier plus a one-line reason is a very small answer; this ceiling exists
/// to stop a runaway generation, not to shape the response.
const DEFAULT_MAX_TOKENS: u32 = 256;

/// The longest reason retained. The model is told "under 100 characters";
/// this is the enforcement, because a notification body is rendered by the
/// desktop, not by us, and an unbounded string here becomes an unbounded
/// argument to `osascript`.
pub const MAX_REASON_CHARS: usize = 200;

/// This pass's instructions, fenced with [`injection::DATA_BOUNDARY_CLAUSE`]
/// so the `⟪untrusted email⟫` delimiters in the user turn mean something.
/// Built once into a `static` so it stays byte-identical across calls and sits
/// behind the provider's prompt-cache boundary, exactly as
/// [`crate::ai::triage`]'s does.
static SYSTEM_PROMPT: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| injection::with_data_boundary(SYSTEM_PROMPT_BASE));

const SYSTEM_PROMPT_BASE: &str =
    "You decide whether a newly arrived email is worth interrupting its \
recipient for. You read one email at a time and answer with a single \
structured JSON object only -- no prose, no markdown, nothing outside the \
schema.

- tier: how much this message deserves to break the recipient's attention \
right now.
  * critical -- something is on fire and the recipient is the one who has to \
act: an outage, a security alert, a payment failing, a deadline today.
  * high -- a real person is waiting on this recipient specifically, or \
something time-bound needs a decision soon.
  * normal -- genuine correspondence that can wait until the recipient next \
looks at their mail.
  * low -- newsletters, marketing, automated receipts, notifications, \
digests, social updates. Interesting is not urgent: a newsletter the \
recipient enjoys is still low.
- reason: one plain clause, under 100 characters, saying why -- what a \
person would glance at on a lock screen to decide whether to open their \
laptop. Name the concrete thing (\"invoice 4821 is overdue today\"), never \
restate the tier (\"this is important\").

Judge the message on its own content, not on how urgently it describes \
itself: a subject line shouting URGENT is evidence about the sender, not \
about the message. Bulk mail addressed to a list is low even when its \
subject claims otherwise. If the body looks redacted or truncated, judge \
from what remains.";

/// How much a message deserves to interrupt its recipient.
///
/// The wire strings are [`crate::ai::triage::PRIORITIES`] — see the module
/// docs. Ordered least to most interrupting, which is what makes
/// `tier >= threshold` the whole gating rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Tier {
    /// Bulk: newsletters, receipts, notifications. Never pings under any
    /// sane threshold.
    Low,
    /// Ordinary correspondence.
    Normal,
    /// Someone is waiting on the recipient.
    High,
    /// Needs action now.
    Critical,
}

impl Tier {
    /// Every tier, least to most interrupting.
    pub const ALL: [Self; 4] = [Self::Low, Self::Normal, Self::High, Self::Critical];

    /// The stable wire string stored in `notifications.tier`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Normal => "normal",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }

    /// Parse a wire string. `None` for anything outside the vocabulary —
    /// callers must fail closed on it rather than guessing a rank (see
    /// [`Threshold`]).
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.as_str() == value)
    }
}

impl std::fmt::Display for Tier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The minimum [`Tier`] that fires a notification.
///
/// A separate type from `Tier` because it has a state `Tier` does not: an
/// operator-supplied string that is not a tier at all. [`Threshold::parse`]
/// resolves that to [`Threshold::Unrecognized`], which
/// [`Threshold::admits`] answers `false` for at *every* tier — the same
/// fail-closed choice [`crate::ai::deep`]'s `priority_at_least` makes, and for
/// the same reason: a typo in a threshold must not silently become "everything
/// qualifies". Here the stakes point the other way (a wide-open notification
/// threshold pings on every newsletter rather than spending money), but the
/// principle is identical — an unreadable policy grants nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Threshold {
    /// Notify at or above this tier.
    At(Tier),
    /// The configured string names no tier; nothing is ever delivered.
    Unrecognized,
}

impl Threshold {
    /// Resolve a configured threshold string.
    #[must_use]
    pub fn parse(value: &str) -> Self {
        Tier::parse(value.trim()).map_or(Self::Unrecognized, Self::At)
    }

    /// Whether `tier` clears this threshold.
    #[must_use]
    pub fn admits(self, tier: Tier) -> bool {
        match self {
            Self::At(threshold) => tier >= threshold,
            Self::Unrecognized => false,
        }
    }
}

impl std::fmt::Display for Threshold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::At(tier) => f.write_str(tier.as_str()),
            Self::Unrecognized => f.write_str("<unrecognized>"),
        }
    }
}

/// The scoring pass's [`PassHandler`].
///
/// Cheap to clone/share, like every other handler: a [`Database`] handle, a
/// short owned model name, and the resolved [`NotifyPolicy`].
#[derive(Debug, Clone)]
pub struct NotifyPassHandler {
    db: Database,
    model: String,
    max_tokens: u32,
    injection: AiInjection,
    /// The same per-account policy the delivery loop applies, consulted one
    /// step earlier — see [`Self::with_policy`].
    policy: NotifyPolicy,
    /// How recently a message must have arrived to be worth scoring — see
    /// [`Self::with_max_message_age`].
    max_message_age: Duration,
}

impl NotifyPassHandler {
    /// A handler that queries `model` (`ai.models.notify`) and writes its
    /// verdicts into `db`.
    ///
    /// Defaults to the *permissive* policy (every account notifies) and
    /// [`crate::config::NotifyConfig::default`]'s `max_message_age`, so a
    /// caller that only wants the pass itself — a test, a one-off backfill —
    /// need not assemble config. The daemon always supplies both.
    #[must_use]
    pub fn new(db: Database, model: impl Into<String>) -> Self {
        Self {
            db,
            model: model.into(),
            max_tokens: DEFAULT_MAX_TOKENS,
            injection: AiInjection::default(),
            policy: NotifyPolicy::from_config(&crate::config::NotifyConfig::always_on(), &[]),
            max_message_age: crate::config::NotifyConfig::default()
                .max_message_age
                .as_duration(),
        }
    }

    /// Decline to score messages for accounts `policy` says will never
    /// notify.
    ///
    /// This is a *cost* gate, and it belongs here rather than at the delivery
    /// step because that is where the money is spent. An account with
    /// `notify.enabled = false` whose messages were scored anyway would pay
    /// the full per-message Haiku bill for notifications it has switched off
    /// — silently, forever, with the only evidence a line item on an invoice.
    #[must_use]
    pub fn with_policy(mut self, policy: NotifyPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Only score messages that arrived within `age`.
    ///
    /// The reason this exists is the moment an operator first sets
    /// `notify.enabled = true`. `crate::ai::dispatch::AiDispatchLoop` holds
    /// its cursor in memory and restarts it at `0`, so the next boot replays
    /// the whole event-log retention window (a week, by default). For triage
    /// that replay is a no-op — every one of those messages already has a
    /// triage row, so `AiQueue::enqueue` dedups it away. For this pass none of
    /// them has a `notify` row, so without this gate enabling the feature
    /// would score a week of already-read mail *and then interrupt the user
    /// about all of it*, twenty desktop notifications at a time.
    ///
    /// Bounded by arrival (`messages.created_at`, when this machine synced
    /// it) rather than by the `Date:` header: a header is sender-controlled,
    /// and a genuinely old message that has only just arrived — a mailbox
    /// migration, a message moved into the inbox — is still news to the
    /// recipient.
    #[must_use]
    pub fn with_max_message_age(mut self, age: Duration) -> Self {
        self.max_message_age = age;
        self
    }

    /// Run the injection detector under `injection` rather than its defaults.
    /// The *fence* is unconditional either way — see [`crate::ai::injection`].
    #[must_use]
    pub fn with_injection_config(mut self, injection: AiInjection) -> Self {
        self.injection = injection;
        self
    }

    /// The request for an already-rendered user turn.
    ///
    /// Split out from [`PassHandler::build_request`] so the rendering can be
    /// scanned before it is wrapped — and so a test can assert the fencing
    /// without standing a whole queue up.
    pub(super) fn request_for(&self, user: String) -> ChatRequest {
        ChatRequest::new(self.model.clone(), self.max_tokens)
            .system(SYSTEM_PROMPT.as_str())
            .user(user)
            .output_format(OutputFormat::json_schema(schema()))
    }

    /// Why this message must not be scored, or `None` to go ahead.
    ///
    /// One query, answering both gates: the account's name (to resolve the
    /// policy against) and the message's arrival time. Reading them together
    /// keeps this to a single round trip on a path that runs once per newly
    /// synced message.
    ///
    /// # Errors
    /// A mapped storage error. A message that has vanished is reported as a
    /// decline rather than an error, since the outcome — terminate the job —
    /// is the same and the queue already treats `NotFound` that way.
    async fn declines(
        &self,
        account_id: i64,
        message_id: i64,
    ) -> Result<Option<&'static str>, Error> {
        let max_age = i64::try_from(self.max_message_age.as_secs()).unwrap_or(i64::MAX);
        let row: Option<(String, i64)> = self
            .db
            .read(move |conn| {
                conn.query_row(
                    "SELECT a.name, unixepoch() - m.created_at
                     FROM messages m JOIN accounts a ON a.id = ?2
                     WHERE m.id = ?1",
                    rusqlite::params![message_id, account_id],
                    |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
                )
                .optional()
            })
            .await?;
        let Some((account, age_secs)) = row else {
            return Ok(Some("the message or its account no longer exists"));
        };
        if !self.policy.notifies(&account) {
            return Ok(Some("this account has notifications disabled"));
        }
        // `>` rather than `>=`, and a negative age (a clock that moved
        // backwards, a row written with a future timestamp) is never stale.
        if age_secs > max_age {
            return Ok(Some(
                "the message arrived longer ago than notify.max_message_age",
            ));
        }
        Ok(None)
    }
}

#[async_trait]
impl PassHandler for NotifyPassHandler {
    fn pass(&self) -> &str {
        PASS
    }

    #[tracing::instrument(
        skip(self, content),
        fields(message_id = content.message_id, injection_severity)
    )]
    async fn build_request(&self, content: &MessageContent) -> Result<ChatRequest, Error> {
        // Both gates run *before* a request is built, let alone sent, and both
        // report `NotFound` — the one `ErrorReason` `PassHandler`'s own
        // contract says terminates a job rather than retrying it, which is
        // exactly right for "this message is never going to be scored". A
        // later attempt cannot make an old message young or an opted-out
        // account opted in.
        //
        // Terminating also means no `notifications` row is ever written for
        // them, which is why this table has no `stale` state: a message that
        // fails these checks is not a suppressed notification, it is not a
        // notification at all.
        if let Some(reason) = self
            .declines(content.account_id, content.message_id)
            .await?
        {
            tracing::debug!(
                message_id = content.message_id,
                reason,
                "declining to score this message for notification"
            );
            return Err(Error::not_found(format!(
                "message {} is not a notification candidate: {reason}",
                content.message_id
            )));
        }
        // `render_user_message` is triage's, imported rather than
        // reimplemented: it is the function that puts the whole rendering —
        // display name, address, subject and body alike — inside one
        // `injection::untrusted_block`, and a second copy of that logic here
        // is exactly how one of them would later grow a field outside the
        // fence. See its own docs for why the fence must cover the headers
        // and not only the body.
        let user = triage::render_user_message(content);
        // Scanned over the rendered user turn, not the database row — the
        // same reasoning `ai::triage::build_request` gives: what the shield
        // has to reason about is exactly the bytes the model will read, and a
        // payload split across the subject and the body is only visible once
        // they are next to each other.
        let report = injection::scan_if_enabled(&user, &self.injection);
        if let Some(severity) = report.severity() {
            tracing::Span::current().record("injection_severity", severity.as_str());
        }
        injection::store::record(&self.db, content.message_id, content.account_id, &report).await;
        Ok(self.request_for(user))
    }

    #[tracing::instrument(
        skip(self, lease, text),
        fields(message_id = lease.message_id, tier, deduped)
    )]
    async fn on_success(
        &self,
        lease: &AiLease,
        text: &str,
        ledger_entry_id: i64,
    ) -> Result<(), Error> {
        let score = NotifyScore::parse(text)?;
        let span = tracing::Span::current();
        span.record("tier", tracing::field::display(score.tier));
        // `record_score` is an insert that does nothing on conflict, so a
        // second run of this handler for the same message — a reaped lease, a
        // re-enqueued pass, a daemon restarted mid-call — cannot displace the
        // decision already made about it, let alone re-arm a delivery that
        // already happened. See `super::repo::record_score`.
        let inserted = repo::record_score(
            &self.db,
            lease.message_id,
            lease.account_id,
            &score,
            &self.model,
            Some(ledger_entry_id),
        )
        .await?;
        span.record("deduped", !inserted);
        tracing::debug!(inserted, "notification score written");
        Ok(())
    }
}

/// The JSON Schema every scoring request constrains its answer to. Byte-stable
/// across calls, for the prompt-cache reason [`SYSTEM_PROMPT`] is.
fn schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "tier": {"type": "string", "enum": PRIORITIES},
            "reason": {"type": "string"},
        },
        "required": ["tier", "reason"],
        "additionalProperties": false,
    })
}

/// One parsed, validated scoring answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NotifyScore {
    /// How much this message deserves to interrupt.
    pub tier: Tier,
    /// The model's one-line justification, trimmed and bounded to
    /// [`MAX_REASON_CHARS`].
    pub reason: String,
}

/// The raw shape [`schema`] describes, before validation.
#[derive(Deserialize)]
struct RawScore {
    tier: String,
    reason: String,
}

impl NotifyScore {
    /// Parse and validate one scoring response.
    ///
    /// The Messages API's structured-output mode guarantees `text` is valid
    /// JSON matching [`schema`], but `enum` membership is a claim about
    /// *values* and is re-checked here rather than trusted — an
    /// out-of-vocabulary tier must surface as a loud, dead-letterable error,
    /// not become a `notifications.tier` that [`Tier::parse`] can never read
    /// back and that therefore silently never notifies.
    ///
    /// # Errors
    /// [`Error::Internal`] if `text` is not valid JSON for this shape, if
    /// `tier` is outside [`Tier::ALL`], or if `reason` is blank. Never a
    /// partial result.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let raw: RawScore = serde_json::from_str(text).map_err(|e| {
            Error::internal(format!(
                "notification score did not match the requested schema: {e}"
            ))
        })?;
        let tier = Tier::parse(raw.tier.trim()).ok_or_else(|| {
            Error::internal(format!(
                "notification score returned tier {:?}, which is not one of {:?}",
                raw.tier,
                Tier::ALL.map(Tier::as_str)
            ))
        })?;
        let reason = raw.reason.trim();
        if reason.is_empty() {
            return Err(Error::internal(
                "notification score returned an empty reason".to_owned(),
            ));
        }
        Ok(Self {
            tier,
            // Truncated by *characters*, never by bytes: `reason` is model
            // prose and routinely non-ASCII, and slicing a UTF-8 string at a
            // byte index that is not a char boundary panics.
            reason: reason.chars().take(MAX_REASON_CHARS).collect(),
        })
    }
}
