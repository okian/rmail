//! The pre-send guardian: the last look at a message before it becomes
//! irreversible (prd.md #20, task 63).
//!
//! ```text
//! draft ──▶ inspect (deterministic, pure) ──┬──▶ report ──▶ verdict
//!                                           │
//!                 review (Claude, fenced) ──┘
//! ```
//!
//! # This module can stop mail, so its failure modes are the design
//!
//! Every other AI-adjacent module in this crate produces something a user
//! reads. This one produces a *refusal*, and a refusal has two ways to be
//! catastrophic that a wrong summary does not: it can stop a message that
//! should have gone, and — the worse one — it can leave a user believing a
//! message was checked when nothing checked it. Three rules follow, and they
//! are structural rather than conventions anyone has to remember:
//!
//! **1. [`PreflightGuardian::check`] cannot fail.** It returns a
//! [`PreflightReport`], not a `Result`. There is no error path, no timeout
//! that propagates, and no state in which a caller is left holding a question
//! mark: a guardian that could return `Err` would force every caller to
//! invent a policy for "the check broke", and the two policies available are
//! "swallow the send" and "send unchecked without saying so". Both are the
//! failure this design exists to make unrepresentable.
//!
//! **2. The model can never block.** [`inspect`] is pure, synchronous, and
//! offline, and it is the *only* producer of [`Severity::Block`].
//! [`review`]'s findings are clamped to [`Severity::Warn`] by
//! [`ModelFinding::into_finding`] no matter what the model answers, *and*
//! [`PreflightReport::blocking_severity`] ignores them outright — the clamp
//! alone is not enough, because `send.preflight.block_at` accepts `warn`, at
//! which a clamped tone finding would refuse a message. With both, the set of
//! messages this daemon refuses is a function of the message and the config
//! alone — reproducible, explainable, and identical whether the provider is
//! up, down, throttled, or switched off. That is what makes rule 3
//! affordable.
//!
//! **3. An unavailable model layer fails *open*, and says so.** Policy
//! forbids it, the budget is spent, the provider is down, the deadline
//! passes, the answer does not parse — every one of those sets
//! [`PreflightReport::degraded`] and leaves the deterministic findings
//! standing. Nothing that could have blocked was lost (rule 2), so failing
//! closed would refuse mail over an outage that could not have changed the
//! verdict. Failing open *silently* would be the real bug, which is why the
//! reason is carried in the report and — on `PreflightCheck`, which is where
//! a client asks the question — returned over gRPC. On the automatic path
//! (`ScheduleSend`) it reaches the daemon's log rather than the response:
//! that response is an `OutboxEntry`, and a message that was going to be sent
//! either way is not made different by how thoroughly it was reviewed.
//! "Checked, and the tone pass was unavailable" and "checked" are different
//! sentences and this module never conflates them.
//!
//! And the fourth, which is about time rather than truth: the *whole* model
//! layer — the policy and budget lookups, the wait for concurrency and rate
//! capacity, and the call itself — runs inside one
//! `send.preflight.timeout`, and the call additionally races the caller's
//! cancellation token. Bounding only the network call would not be enough:
//! `gate::acquire_capacity` waits on a semaphore shared with the AI worker
//! pool, so a busy triage backlog could hold a send open with the provider
//! never having been dialled at all.
//!
//! # Why a blocked send is a refusal and not a silent hold
//!
//! When [`PreflightReport::blocks`] is true the send path returns
//! `FAILED_PRECONDITION` naming the findings, and the user re-sends with the
//! override. It does not queue the message in a "held" state, and there is
//! deliberately no such state: a message parked somewhere for a human to
//! notice is exactly the "silently swallowed send" this task's constitution
//! forbids, and the outbox has no shortage of evidence about what happens to
//! rows nobody is watching.
//!
//! # An outgoing message is not trusted input
//!
//! [`render_for_model`] fences the whole rendering in
//! [`injection::untrusted_block`] and the system prompt carries
//! [`injection::DATA_BOUNDARY_CLAUSE`]. That looks paranoid for text the user
//! wrote until you notice that a reply *quotes the message it answers*: the
//! quoted region is the correspondent's bytes, and on a hostile thread it is
//! an attacker's. The user's own words are in the same fence rather than
//! outside it because splitting them would require deciding where the quote
//! starts, and that decision is made by parsing text the attacker also
//! controls.
//!
//! # What is *not* quoted back
//!
//! [`FindingKind::ApparentSecret`] reports a count and a kind and never an
//! excerpt. A preflight report is returned over gRPC, printed to a terminal,
//! and — on the automatic path — summarized into a `tracing` field and a
//! `Status` message. A guardian that proved it had found an API key by
//! echoing the API key would have moved the secret from a message the user
//! was about to think twice about into a log nobody will ever redact.

use std::sync::{Arc, LazyLock};

use regex::Regex;
use serde::Deserialize;
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::ai::gate;
use crate::ai::injection;
use crate::ai::policy::PolicyEngine;
use crate::ai::provider::{ChatRequest, OutputFormat, Provider};
use crate::ai::queue::{payload_bytes, RateLimiter};
use crate::ai::{self, CallOutcome, CallRecord, GuardedRequest, RedactPreview, RedactionKind};
use crate::config::{AiLimits, AiPrivacy, SendPreflight};
use crate::error::Error;
use crate::storage::Database;

#[cfg(test)]
mod tests;

/// The `ai_ledger.pass` value a guardian review is recorded under, so an
/// operator can tell what the send path spends from what triage spends.
pub const PASS: &str = "preflight";

/// A short list of short findings; this ceiling stops a runaway generation
/// rather than shaping the answer.
const MAX_TOKENS: u32 = 1_024;

/// The most findings one model answer contributes, whatever it returns.
///
/// The deterministic layer is separately bounded by construction (one finding
/// per check), so this is the only unbounded producer in the module.
const MAX_MODEL_FINDINGS: usize = 8;

/// The longest `detail` any finding carries. A finding is a sentence a user
/// reads next to a send button.
pub const MAX_DETAIL_CHARS: usize = 300;

/// How much of a body the model reviews.
///
/// Tighter than `ai.privacy.max_body_chars` for the reason
/// [`crate::rules::classify::MAX_BODY_CHARS`] is: what this pass judges —
/// tone, and whether the opening promises something the message does not
/// carry — is decided in the first screenfuls, and this multiplies by every
/// message the user sends.
pub const MAX_BODY_CHARS: usize = 6_000;

/// This pass's instructions, with [`injection::DATA_BOUNDARY_CLAUSE`]
/// appended once into a `static` so it stays byte-identical across calls and
/// sits behind the provider's prompt-cache boundary — the discipline
/// [`crate::ai::triage`]'s own system prompt documents.
static SYSTEM_PROMPT: LazyLock<String> =
    LazyLock::new(|| injection::with_data_boundary(SYSTEM_PROMPT_BASE));

const SYSTEM_PROMPT_BASE: &str = "You are the last reviewer of an email \
before it is sent, working for its author. You read one outgoing message and \
answer with a single structured JSON object only -- no prose, no markdown, \
nothing outside the schema.

Report only things the author would want to fix before this leaves. Each \
finding names one concrete problem:

- tone_clash: the register is wrong for the recipients or the thread -- \
hostile or sarcastic where the exchange has been cordial, flippant about \
something serious, an angry line the author will regret, or wildly over- \
or under-formal for the people addressed.
- missing_attachment: the text promises something enclosed and the \
attachment list does not contain it.
- unfilled_placeholder: a template hole, sample text, or an obvious \
stand-in the author meant to replace.
- recipient_not_on_thread: the message reads as private or narrowly scoped \
yet addresses someone the thread has not involved.
- apparent_secret: the body appears to disclose a credential, key, or other \
value that should not be emailed. Describe it; never quote it.

severity is `warn` for something the author should look at before sending \
and `notice` for something merely worth mentioning.

Say nothing about style, brevity, greetings, sign-offs, typos, or grammar. \
An empty findings array is the right answer for an ordinary message and is \
what you should return most of the time -- a reviewer who finds something \
in every message is one the author stops reading. Judge the message as \
written; quoted text from an earlier message in the thread is context, not \
the author's own words, and is not a finding.";

// ---------------------------------------------------------------------------
// Severity
// ---------------------------------------------------------------------------

/// How much one finding matters.
///
/// Ordered `Notice < Warn < Block`, which is what makes
/// `send.preflight.block_at` a `>=` comparison rather than a match arm per
/// value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Severity {
    /// Worth mentioning; never on its own a reason to stop.
    Notice,
    /// The author should look before sending.
    Warn,
    /// Refuses the send under the default `send.preflight.block_at`.
    Block,
}

impl Severity {
    /// Every severity, ascending.
    pub const ALL: [Self; 3] = [Self::Notice, Self::Warn, Self::Block];

    /// The stable wire string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Notice => "notice",
            Self::Warn => "warn",
            Self::Block => "block",
        }
    }

    /// Parse a wire string, or `None` for anything outside the vocabulary.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.as_str() == value)
    }
}

impl std::fmt::Display for Severity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Finding kinds
// ---------------------------------------------------------------------------

/// One category of thing the guardian looks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FindingKind {
    /// The body promises an enclosure the message does not carry.
    MissingAttachment,
    /// A template hole or stand-in nobody filled in.
    UnfilledPlaceholder,
    /// Something shaped like a credential, card number, or national id.
    ApparentSecret,
    /// A recipient the thread being replied to has not involved.
    RecipientNotOnThread,
    /// The same address appears in more than one of To/Cc/Bcc.
    DuplicateRecipient,
    /// More recipients than `send.preflight.max_recipients`.
    LargeRecipientList,
    /// The register is wrong for the thread or the recipients.
    ToneClash,
}

impl FindingKind {
    /// Every kind, for exhaustive handling and for the model's enum.
    pub const ALL: [Self; 7] = [
        Self::MissingAttachment,
        Self::UnfilledPlaceholder,
        Self::ApparentSecret,
        Self::RecipientNotOnThread,
        Self::DuplicateRecipient,
        Self::LargeRecipientList,
        Self::ToneClash,
    ];

    /// The stable wire string, sent over gRPC and named in the model's schema.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::MissingAttachment => "missing_attachment",
            Self::UnfilledPlaceholder => "unfilled_placeholder",
            Self::ApparentSecret => "apparent_secret",
            Self::RecipientNotOnThread => "recipient_not_on_thread",
            Self::DuplicateRecipient => "duplicate_recipient",
            Self::LargeRecipientList => "large_recipient_list",
            Self::ToneClash => "tone_clash",
        }
    }

    /// Parse a wire string, or `None` for one this build does not know.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == value)
    }

    /// The severity [`inspect`] assigns this kind.
    ///
    /// The two that block are the two where a false negative is
    /// *irreversible from the sender's side*: a credential in a delivered
    /// message has to be rotated, and a template hole reaches every recipient
    /// at once. Everything else is a warning, including the model's entire
    /// output — see the module docs.
    #[must_use]
    pub const fn default_severity(self) -> Severity {
        match self {
            Self::ApparentSecret | Self::UnfilledPlaceholder => Severity::Block,
            Self::MissingAttachment | Self::RecipientNotOnThread | Self::LargeRecipientList => {
                Severity::Warn
            }
            Self::DuplicateRecipient => Severity::Notice,
            // Never reached: a tone finding can only come from the model, and
            // the model's severities are clamped rather than defaulted. Stated
            // so the match is total and so a future deterministic tone check
            // inherits an answer rather than a panic.
            Self::ToneClash => Severity::Warn,
        }
    }
}

impl std::fmt::Display for FindingKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One thing the guardian noticed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// What kind of problem this is.
    pub kind: FindingKind,
    /// How much it matters.
    pub severity: Severity,
    /// One sentence a user reads next to a send button, bounded by
    /// [`MAX_DETAIL_CHARS`].
    pub detail: String,
    /// Whether a model produced this finding, rather than [`inspect`].
    ///
    /// Carried through to the wire because it is the honest answer to "why
    /// did it say that": a deterministic finding is reproducible from the
    /// message, and a model finding is a judgement that may differ next time.
    pub from_model: bool,
}

impl Finding {
    /// A deterministic finding at its kind's default severity.
    fn deterministic(kind: FindingKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            severity: kind.default_severity(),
            detail: bounded_detail(detail.into()),
            from_model: false,
        }
    }
}

/// Truncate a detail by *characters* — model prose is routinely non-ASCII and
/// slicing UTF-8 at a byte index that is not a char boundary panics.
fn bounded_detail(detail: String) -> String {
    let trimmed = detail.trim();
    if trimmed.chars().count() <= MAX_DETAIL_CHARS {
        return trimmed.to_owned();
    }
    trimmed.chars().take(MAX_DETAIL_CHARS).collect()
}

// ---------------------------------------------------------------------------
// Degradation
// ---------------------------------------------------------------------------

/// Why the model layer did not contribute to a report.
///
/// Present on a report is the whole point — see the module docs' rule 3. A
/// caller that ignores this field is claiming a message was fully reviewed
/// when part of the review did not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Degradation {
    /// `send.preflight.ai = false`. Not a failure; the operator asked for
    /// the deterministic checks only.
    Disabled,
    /// AI policy, the daily cost gate, or a spend budget refused the call.
    Refused(String),
    /// The provider returned an error, or could not be reached.
    Unavailable(String),
    /// The call did not answer within `send.preflight.timeout`.
    TimedOut,
    /// The caller's cancellation token fired — a dropped RPC, a shutdown.
    Cancelled,
    /// The model answered something this build cannot read.
    Unreadable(String),
    /// The redaction firewall left nothing to review.
    NothingToReview,
}

impl Degradation {
    /// The stable wire string for the *kind* of degradation.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Refused(_) => "refused",
            Self::Unavailable(_) => "unavailable",
            Self::TimedOut => "timed_out",
            Self::Cancelled => "cancelled",
            Self::Unreadable(_) => "unreadable",
            Self::NothingToReview => "nothing_to_review",
        }
    }

    /// One line naming what was skipped and why, for a user or a log.
    #[must_use]
    pub fn describe(&self) -> String {
        match self {
            Self::Disabled => {
                "the model review is switched off (send.preflight.ai); the deterministic checks \
                 ran"
                .to_owned()
            }
            Self::Refused(why) => format!("the model review was not permitted: {why}"),
            Self::Unavailable(why) => format!("the model review could not be made: {why}"),
            Self::TimedOut => {
                "the model review did not answer within send.preflight.timeout".to_owned()
            }
            Self::Cancelled => "the model review was cancelled before it answered".to_owned(),
            Self::Unreadable(why) => format!("the model review could not be read back: {why}"),
            Self::NothingToReview => {
                "nothing was left to review once PII was redacted from this message".to_owned()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The message under review
// ---------------------------------------------------------------------------

/// The message a check is about.
///
/// Assembled by the caller from a draft or from an outbox row; this module
/// never reads either, so the same check runs over a stored draft, an inline
/// send, and a message a test made up.
#[derive(Debug, Clone, Default)]
pub struct PreflightMessage {
    /// Owning account, for policy resolution and the audit ledger.
    pub account_id: i64,
    /// The sending identity, as a bare addr-spec.
    pub from: String,
    /// `To` recipients, bare addr-specs.
    pub to: Vec<String>,
    /// `Cc` recipients.
    pub cc: Vec<String>,
    /// `Bcc` recipients.
    pub bcc: Vec<String>,
    /// Subject, decoded.
    pub subject: String,
    /// The plain-text body as the author wrote it, quoted reply text and all.
    pub body: String,
    /// Attachment filenames, in author order.
    pub attachments: Vec<String>,
    /// Every address already on the thread being replied to, when this is a
    /// reply and the parent is known locally. Empty means "not a reply, or
    /// the parent is not on this machine" — and an empty list disables the
    /// recipient-not-on-thread check rather than flagging every recipient,
    /// which is the difference between a useful warning and noise.
    pub thread_participants: Vec<String>,
    /// The folder the AI policy is resolved against, when there is one.
    pub mailbox: Option<String>,
}

impl PreflightMessage {
    /// Every distinct envelope recipient, lowercased, in To/Cc/Bcc order.
    fn recipients(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for address in self.to.iter().chain(&self.cc).chain(&self.bcc) {
            let address = normalize_address(address);
            if !address.is_empty() && !out.contains(&address) {
                out.push(address);
            }
        }
        out
    }

    /// The body with quoted reply text and the signature block removed — what
    /// the author actually wrote this time.
    ///
    /// Both checks that read prose ([`FindingKind::MissingAttachment`] and
    /// [`FindingKind::UnfilledPlaceholder`]) run over this rather than the raw
    /// body, because a reply that quotes "see attached" is not promising an
    /// attachment of its own, and a signature containing `[COMPANY]` in a
    /// legal footer is not an unfilled hole.
    fn authored_body(&self) -> String {
        let mut out = String::with_capacity(self.body.len());
        for line in self.body.lines() {
            let trimmed = line.trim_end();
            // RFC 3676's signature separator. Everything after it is boilerplate
            // the author did not type into this message.
            if trimmed == "--" || trimmed == "-- " {
                break;
            }
            // A quoted line, and the attribution that introduces one
            // ("On Tuesday, Bob wrote:"), which is generated rather than
            // authored and routinely restates the parent's first words.
            if line.trim_start().starts_with('>') {
                continue;
            }
            out.push_str(line);
            out.push('\n');
        }
        out
    }

    /// Subject and authored body together — one string, because a promise
    /// made in a subject line ("Q3 numbers attached") and one made in a body
    /// are the same promise.
    fn authored_text(&self) -> String {
        format!("{}\n{}", self.subject, self.authored_body())
    }

    /// The part of the body [`Self::authored_body`] drops: quoted reply text
    /// and everything past the signature separator.
    ///
    /// It exists for exactly one check. Quoted text is not the author's
    /// words, which is why the prose checks ignore it — but a credential
    /// inside it is still a credential this message is about to *carry*, and
    /// a reply-all adds recipients who were never sent it. So
    /// [`secret_findings`] scans here too, at [`Severity::Warn`] rather than
    /// [`Severity::Block`]: forwarding a value the thread already contains is
    /// a thing people do deliberately, and blocking every reply to a thread
    /// that once mentioned an API key would get the guardian switched off
    /// within a day.
    fn quoted_body(&self) -> String {
        let mut out = String::new();
        let mut past_signature = false;
        for line in self.body.lines() {
            let trimmed = line.trim_end();
            if trimmed == "--" || trimmed == "-- " {
                past_signature = true;
                continue;
            }
            if past_signature || line.trim_start().starts_with('>') {
                out.push_str(line);
                out.push('\n');
            }
        }
        out
    }

    /// The rendering the model reviews.
    ///
    /// Headers are inside the fence with the body, not above it: the whole
    /// point of fencing is that no part of what a sender influenced sits in
    /// instruction position, and a display name is sender-chosen text on a
    /// reply. Same reasoning [`crate::ai::triage::render_user_message`]
    /// documents.
    fn render_for_model(&self) -> String {
        let mut rendered = String::new();
        rendered.push_str(&format!("From: {}\n", self.from));
        rendered.push_str(&format!("To: {}\n", self.to.join(", ")));
        if !self.cc.is_empty() {
            rendered.push_str(&format!("Cc: {}\n", self.cc.join(", ")));
        }
        if !self.bcc.is_empty() {
            rendered.push_str(&format!("Bcc: {}\n", self.bcc.join(", ")));
        }
        rendered.push_str(&format!("Subject: {}\n", self.subject));
        rendered.push_str(&format!(
            "Attachments: {}\n",
            if self.attachments.is_empty() {
                "(none)".to_owned()
            } else {
                self.attachments.join(", ")
            }
        ));
        if !self.thread_participants.is_empty() {
            rendered.push_str(&format!(
                "Already on this thread: {}\n",
                self.thread_participants.join(", ")
            ));
        }
        rendered.push('\n');
        rendered.push_str(&truncate_chars(&self.body, MAX_BODY_CHARS));
        injection::untrusted_block("outgoing-email", &rendered)
    }
}

/// Lowercase and trim one address for comparison. Local-parts are formally
/// case-sensitive; no mail system anyone sends to treats them that way, and a
/// duplicate check that missed `Bob@x` next to `bob@x` would be worse than
/// useless.
fn normalize_address(address: &str) -> String {
    address.trim().trim_matches(['<', '>']).to_lowercase()
}

/// Truncate by `char`, never by byte — see [`bounded_detail`].
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    text.chars().take(max).collect()
}

// ---------------------------------------------------------------------------
// The report
// ---------------------------------------------------------------------------

/// What one check concluded.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PreflightReport {
    /// Everything found, deterministic findings first and then in the order
    /// the model returned them.
    pub findings: Vec<Finding>,
    /// Why the model layer did not contribute, when it did not. `None` means
    /// the full check ran.
    pub degraded: Option<Degradation>,
    /// The model that actually answered, when one did. May differ from
    /// `send.preflight.model` — a spend budget can downgrade it.
    pub model: Option<String>,
}

impl PreflightReport {
    /// The highest severity found, or `None` for a clean message.
    ///
    /// This is what a *user* is shown. It is not what decides a refusal —
    /// see [`Self::blocking_severity`], which is the same question asked only
    /// of the findings that are allowed to stop mail.
    #[must_use]
    pub fn severity(&self) -> Option<Severity> {
        self.findings.iter().map(|f| f.severity).max()
    }

    /// The highest severity among findings that may refuse a send: the
    /// deterministic ones.
    ///
    /// The module's second rule, made mechanical at the point it actually
    /// matters. Clamping a model finding to [`Severity::Warn`] is not on its
    /// own enough — `send.preflight.block_at` accepts `warn` and `notice`,
    /// and under those a clamped tone finding would refuse a message, making
    /// the outcome depend on whether a provider happened to be reachable.
    /// Filtering here is what makes "the model can never block" true at
    /// *every* threshold rather than only at the default one.
    #[must_use]
    pub fn blocking_severity(&self) -> Option<Severity> {
        self.findings
            .iter()
            .filter(|f| !f.from_model)
            .map(|f| f.severity)
            .max()
    }

    /// Whether this report refuses the send under `config`.
    ///
    /// `None` — nothing deterministic found — never blocks. An unrecognized
    /// `send.preflight.block_at` never blocks either, and is warned about at
    /// startup by [`SendPreflight::warn_if_unrecognized`]; see that field's
    /// docs for why a typo must not be able to stop a mailbox.
    #[must_use]
    pub fn blocks(&self, config: &SendPreflight) -> bool {
        match (self.blocking_severity(), config.block_severity()) {
            (Some(found), Some(threshold)) => found >= threshold,
            _ => false,
        }
    }

    /// The findings at or above `threshold` that contributed to a refusal,
    /// newline-joined — the text a `FAILED_PRECONDITION` carries so the user
    /// knows what to fix.
    ///
    /// Deterministic findings only, matching [`Self::blocks`]: naming a model
    /// finding in a refusal would tell the user to fix something that was not
    /// why the message was refused.
    #[must_use]
    pub fn summary(&self, threshold: Severity) -> String {
        self.findings
            .iter()
            .filter(|f| !f.from_model && f.severity >= threshold)
            .map(|f| format!("{} [{}]: {}", f.kind, f.severity, f.detail))
            .collect::<Vec<_>>()
            .join("; ")
    }
}

// ---------------------------------------------------------------------------
// The deterministic layer
// ---------------------------------------------------------------------------

/// Run every offline check over `message`.
///
/// Pure, synchronous, and total: no database, no configuration beyond
/// `config.max_recipients`, no I/O, no failure mode. This function is the
/// only producer of [`Severity::Block`] in the module, which is what lets the
/// model layer fail open without weakening what the guardian refuses — see
/// the module docs.
#[must_use]
pub fn inspect(message: &PreflightMessage, config: &SendPreflight) -> Vec<Finding> {
    let mut findings = Vec::new();
    let authored = message.authored_text();

    if message.attachments.is_empty() {
        // Two patterns, because a subject and a body are different evidence.
        // A subject is a handful of words the author chose to summarize the
        // message, so any mention of an attachment in one is a promise ("Q3
        // numbers attached"); a body is prose, where the same words appear
        // inside sentences that promise nothing ("I'll send the attachment
        // tomorrow", "thanks for the deck you attached"), so it needs the
        // narrow shapes.
        let promise = first_match(&ATTACHMENT_PROMISE, &message.authored_body()).or_else(|| {
            // Not on a reply. A reply inherits its parent's subject verbatim,
            // so on a thread titled "Q3 numbers attached" the loose subject
            // rule would flag every message in it — the same false positive
            // `authored_body` exists to avoid for the body, arriving by the
            // other door.
            (!is_reply_subject(&message.subject))
                .then(|| first_match(&SUBJECT_ATTACHMENT, &message.subject))
                .flatten()
        });
        if let Some(promise) = promise {
            findings.push(Finding::deterministic(
                FindingKind::MissingAttachment,
                format!("the message says {promise:?} but carries no attachment"),
            ));
        }
    }

    if let Some(hole) = first_match(&PLACEHOLDER, &authored) {
        findings.push(Finding::deterministic(
            FindingKind::UnfilledPlaceholder,
            format!("{hole:?} looks like a template placeholder nobody filled in"),
        ));
    }

    findings.extend(secret_findings(message));
    findings.extend(recipient_findings(message, config));
    findings
}

/// Findings for values that must not be emailed.
///
/// Delegates to [`crate::ai::redact`]'s own detectors rather than growing a
/// second set: that module is where the regexes for a Luhn-valid card, an
/// SSA-issued SSN, a JWT and a labelled `api_key` are maintained and tested,
/// and two independently-drifting definitions of "this is a credential" is
/// exactly how one of them ends up weaker than the other.
///
/// It is run under a *fixed* [`AiPrivacy`], never the operator's: `ai.privacy`
/// governs what leaves this machine for a model, and an operator who has
/// decided their model provider may see raw text has said nothing whatsoever
/// about whether a credential should go out in mail.
///
/// Counts only, never excerpts — see the module docs.
///
/// Scans the author's own text and the quoted remainder separately, at
/// different severities — see [`PreflightMessage::quoted_body`].
fn secret_findings(message: &PreflightMessage) -> Vec<Finding> {
    let authored = counts(&message.authored_text());
    let quoted = counts(&message.quoted_body());
    let mut findings = Vec::new();
    for (kind, noun) in [
        (RedactionKind::Secret, "an API key, token or password"),
        (RedactionKind::Card, "a payment card number"),
        (RedactionKind::Ssn, "a US Social Security Number"),
        (RedactionKind::Iban, "a bank account number"),
    ] {
        let in_authored = authored.get(&kind).copied().unwrap_or(0);
        let in_quoted = quoted.get(&kind).copied().unwrap_or(0);
        if in_authored > 0 {
            findings.push(Finding::deterministic(
                FindingKind::ApparentSecret,
                format!(
                    "the message appears to contain {noun} ({} distinct value{}); it is not \
                     quoted here on purpose",
                    in_authored,
                    plural(in_authored)
                ),
            ));
        } else if in_quoted > 0 {
            findings.push(Finding {
                kind: FindingKind::ApparentSecret,
                severity: Severity::Warn,
                detail: bounded_detail(format!(
                    "the quoted text below appears to contain {noun} ({} distinct value{}); \
                     sending this forwards it on",
                    in_quoted,
                    plural(in_quoted)
                )),
                from_model: false,
            });
        }
    }
    findings
}

/// Per-kind counts of what [`crate::ai::redact`] would tokenize in `text`.
fn counts(text: &str) -> std::collections::BTreeMap<RedactionKind, usize> {
    if text.trim().is_empty() {
        return std::collections::BTreeMap::new();
    }
    let preview: RedactPreview = ai::preview(text, &guardian_privacy());
    preview.counts
}

/// `""` or `"s"`.
const fn plural(count: usize) -> &'static str {
    if count == 1 {
        ""
    } else {
        "s"
    }
}

/// The privacy configuration [`secret_findings`] scans under: redaction on,
/// every optional detector enabled, whatever the operator has configured for
/// the model path. See that function's docs.
fn guardian_privacy() -> AiPrivacy {
    AiPrivacy {
        redact: true,
        redact_patterns: ["ssn", "credit_card", "iban", "api_key", "otp"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect(),
        ..AiPrivacy::default()
    }
}

/// Findings about who the message is addressed to.
fn recipient_findings(message: &PreflightMessage, config: &SendPreflight) -> Vec<Finding> {
    let mut findings = Vec::new();
    let recipients = message.recipients();

    // A duplicate is only visible across the three header lists — `recipients`
    // has already deduplicated, so it is counted from the raw lists.
    let mut seen: Vec<(String, &'static str)> = Vec::new();
    let mut duplicates: Vec<String> = Vec::new();
    for (list, name) in [
        (&message.to, "To"),
        (&message.cc, "Cc"),
        (&message.bcc, "Bcc"),
    ] {
        for address in list {
            let address = normalize_address(address);
            if address.is_empty() {
                continue;
            }
            if let Some((_, first)) = seen.iter().find(|(seen, _)| *seen == address) {
                let note = if *first == name {
                    format!("{address} is listed twice in {name}")
                } else {
                    format!("{address} is in both {first} and {name}")
                };
                if !duplicates.contains(&note) {
                    duplicates.push(note);
                }
                continue;
            }
            seen.push((address, name));
        }
    }
    if !duplicates.is_empty() {
        findings.push(Finding::deterministic(
            FindingKind::DuplicateRecipient,
            duplicates.join("; "),
        ));
    }

    let max = config.max_recipients as usize;
    if max > 0 && recipients.len() > max {
        findings.push(Finding::deterministic(
            FindingKind::LargeRecipientList,
            format!(
                "this message names {} recipients (send.preflight.max_recipients is {max})",
                recipients.len()
            ),
        ));
    }

    if !message.thread_participants.is_empty() {
        let known: Vec<String> = message
            .thread_participants
            .iter()
            .map(|a| normalize_address(a))
            .chain(std::iter::once(normalize_address(&message.from)))
            .collect();
        let added: Vec<String> = recipients
            .iter()
            .filter(|address| !known.contains(address))
            .cloned()
            .collect();
        if !added.is_empty() {
            findings.push(Finding::deterministic(
                FindingKind::RecipientNotOnThread,
                format!(
                    "{} {} not on this thread before now",
                    added.join(", "),
                    if added.len() == 1 { "was" } else { "were" }
                ),
            ));
        }
    }

    findings
}

/// The first thing `pattern` matches in `text`, quoted, or `None`.
///
/// The quote is bounded and stripped of invisible/bidi characters by
/// [`injection::sanitize_model_text`]: a finding's detail is printed to a
/// terminal, and a placeholder containing a right-to-left override would
/// otherwise reorder the warning that describes it.
fn first_match(pattern: &LazyLock<Option<Regex>>, text: &str) -> Option<String> {
    let hit = pattern.as_ref()?.find(text)?;
    let quoted = injection::sanitize_model_text(hit.as_str())
        .trim()
        .to_owned();
    Some(truncate_chars(&quoted, 80))
}

/// Prose that promises an enclosure.
///
/// Shapes rather than sentences, the discipline
/// [`crate::ai::injection`]'s `OVERRIDE` documents: each alternative pairs a
/// verb of enclosure with a noun for the thing enclosed, so "please find
/// attached" and "I have attached" both match without a row per rewording. A
/// bare mention of the word "attachment" deliberately does not match — "I'll
/// send the attachment tomorrow" is a promise about the future, and a
/// guardian that fired on it would be turned off within a week.
static ATTACHMENT_PROMISE: LazyLock<Option<Regex>> = LazyLock::new(|| {
    compile(
        r"(?xi)
        (?: see | find | check | review | open ) \s (?: the \s | my \s )?
            attach (?: ed | ment | ments )
      | (?: please \s )? find \s (?: the \s )? attached
      | i \s (?: have \s | 've \s ) attached
      | (?: i \s )? attach (?: ed | ing ) \s (?: is | are | herewith | the | my | a )
      | attached \s (?: is | are | you'll \s find | herewith | please | to \s this )
      | enclosed \s (?: is | are | herewith | please | you'll \s find )
      | \b pfa \b
      | attachment s? \s (?: below | included | enclosed | attached )
        ",
    )
});

/// Whether a subject was inherited from a message being replied to.
///
/// The ASCII prefixes only: they are what every mail client this daemon talks
/// to emits, and a localization this misses costs one spurious warning rather
/// than a missed check.
fn is_reply_subject(subject: &str) -> bool {
    let lower = subject.trim().to_lowercase();
    ["re:", "re :", "fw:", "fwd:", "aw:", "sv:"]
        .iter()
        .any(|prefix| lower.starts_with(prefix))
}

/// Any mention of an attachment in a *subject line*.
///
/// Deliberately far looser than [`ATTACHMENT_PROMISE`] — see [`inspect`]'s
/// comment. A subject is a deliberate summary a handful of words long; a
/// person who writes "attached" in one is telling the recipient something is
/// attached.
static SUBJECT_ATTACHMENT: LazyLock<Option<Regex>> =
    LazyLock::new(|| compile(r"(?i)\battach(?:ed|ment|ments)\b"));

/// Template holes and stand-in text.
///
/// Every alternative is a shape no ordinary sentence produces. `{{...}}` is
/// the mail-merge convention; `%%FIELD%%` is the other one; a bracketed
/// all-caps word is what a hand-written template uses; `TKTK` and `lorem
/// ipsum` are the two filler markers that mean nothing else. Single braces
/// are deliberately absent: `{}` appears in every message that quotes code.
static PLACEHOLDER: LazyLock<Option<Regex>> = LazyLock::new(|| {
    compile(
        r"(?x)
        \{\{ [^{}\n]{1,80} \}\}
      | %% [A-Za-z0-9_]{2,40} %%
      | < \s* (?i: insert | your \s name | your \s company | client \s name
              | recipient | firstname | first \s name | date \s here ) [^<>\n]{0,40} >
      | \[ (?: TODO | FIXME | TBD | XXX | NAME | FIRST_?NAME | DATE | COMPANY
             | CLIENT | AMOUNT | LINK | INSERT [^\]\n]{0,40} ) \]
      | (?i: \b tktk \b | \b lorem \s ipsum \b )
        ",
    )
});

/// Compile a pattern, or record why it could not be compiled.
///
/// The same discipline [`crate::ai::injection::scan`]'s own `compile` applies:
/// every pattern here is a literal, so a failure is a typo, and a detector
/// that silently returns nothing is a guardian that silently stopped
/// guarding. `every_pattern_compiles` in this module's tests is what makes
/// that loud.
fn compile(pattern: &str) -> Option<Regex> {
    match Regex::new(pattern) {
        Ok(re) => Some(re),
        Err(error) => {
            tracing::error!(%error, "preflight pattern failed to compile; that check is disabled");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// The guardian
// ---------------------------------------------------------------------------

/// The pre-send guardian: the deterministic checks plus, when it is available,
/// a model review on top.
///
/// Cheap to clone — every field is a handle. One instance serves
/// `PreflightCheck` and the automatic check on `ScheduleSend`, which is what
/// keeps them inside one concurrency budget.
#[derive(Clone)]
pub struct PreflightGuardian {
    db: Database,
    provider: Arc<dyn Provider>,
    policy: Arc<PolicyEngine>,
    privacy: AiPrivacy,
    limits: AiLimits,
    config: SendPreflight,
    semaphore: Arc<Semaphore>,
    rate_limiter: Arc<RateLimiter>,
}

impl std::fmt::Debug for PreflightGuardian {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreflightGuardian")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl PreflightGuardian {
    /// Build a guardian.
    ///
    /// `semaphore`/`rate_limiter` must be the running `AiWorkerPool`'s own
    /// handles, for the reason [`crate::rules::gate`] gives at length: fresh
    /// ones would double the ceiling `ai.limits` configures.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: Database,
        provider: Arc<dyn Provider>,
        policy: Arc<PolicyEngine>,
        privacy: AiPrivacy,
        limits: AiLimits,
        config: SendPreflight,
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

    /// The guardian's configuration, for a caller deciding what to do with a
    /// report.
    #[must_use]
    pub fn config(&self) -> &SendPreflight {
        &self.config
    }

    /// Check one message.
    ///
    /// Infallible by construction — see the module docs' rule 1. The
    /// deterministic checks always run; the model review runs when it can and
    /// records why it could not when it cannot.
    #[tracing::instrument(
        skip(self, message, cancel),
        fields(
            account_id = message.account_id,
            findings,
            severity,
            degraded,
            model,
        )
    )]
    pub async fn check(
        &self,
        message: &PreflightMessage,
        cancel: &CancellationToken,
    ) -> PreflightReport {
        let mut report = PreflightReport {
            findings: inspect(message, &self.config),
            ..PreflightReport::default()
        };

        if self.config.ai {
            // The deadline covers the *whole* review, not only the provider
            // call. Everything before it can block for an unbounded time too:
            // `gate::admit` is three database round trips, and
            // `gate::acquire_capacity` waits on a semaphore this process
            // shares with the AI worker pool and the rules engine, and on a
            // rate limiter that sleeps essentially forever when
            // `ai.limits.requests_per_minute` is 0. A timeout wrapped around
            // only the network call would leave a busy AI queue holding a
            // send open indefinitely — the exact failure this bound exists to
            // rule out.
            let outcome = tokio::time::timeout(
                self.config.timeout.as_duration(),
                self.review(message, cancel),
            )
            .await;
            match outcome {
                Ok(Ok((model, findings))) => {
                    report.model = Some(model);
                    merge(&mut report.findings, findings);
                }
                Ok(Err(degradation)) => report.degraded = Some(degradation),
                Err(_elapsed) => report.degraded = Some(Degradation::TimedOut),
            }
        } else {
            report.degraded = Some(Degradation::Disabled);
        }

        let span = tracing::Span::current();
        span.record("findings", report.findings.len());
        if let Some(severity) = report.severity() {
            span.record("severity", severity.as_str());
        }
        if let Some(degraded) = &report.degraded {
            span.record("degraded", degraded.as_str());
            if matches!(degraded, Degradation::Disabled | Degradation::Refused(_)) {
                // The two degradations that are a *decision* rather than a
                // fault: the operator switched the model layer off, or their
                // AI policy/budget says this account may not make the call.
                // Both are steady states, so warning on them would put a line
                // in the log for every message the mailbox sends — the fastest
                // way to teach someone to filter out this module's warnings
                // entirely, which would then hide the ones below that matter.
                tracing::debug!(
                    account_id = message.account_id,
                    reason = %degraded.describe(),
                    "the pre-send guardian's model review did not run"
                );
            } else {
                // Warn: a report the caller may act on is missing a layer, and
                // the one thing this module must never do is let that pass
                // unremarked.
                tracing::warn!(
                    account_id = message.account_id,
                    reason = %degraded.describe(),
                    "the pre-send guardian ran degraded; the deterministic checks still applied"
                );
            }
        }
        if let Some(model) = &report.model {
            span.record("model", model.as_str());
        }
        report
    }

    /// The model layer. `Err` is a [`Degradation`], never something a caller
    /// has to turn into a failure.
    async fn review(
        &self,
        message: &PreflightMessage,
        cancel: &CancellationToken,
    ) -> Result<(String, Vec<Finding>), Degradation> {
        let model = gate::admit(
            &self.db,
            &self.policy,
            &self.limits,
            message.account_id,
            message.mailbox.as_deref(),
            &self.config.model,
        )
        .await
        .map_err(|error| match error.reason() {
            // A refusal is a decision (policy, cost gate, budget); anything
            // else — a missing account, a storage error — is the machinery
            // failing, and the two read very differently in a report.
            crate::ErrorReason::FailedPrecondition | crate::ErrorReason::ResourceExhausted => {
                Degradation::Refused(error.to_string())
            }
            _ => Degradation::Unavailable(error.to_string()),
        })?;

        let request = ChatRequest::new(model.clone(), MAX_TOKENS)
            .system(SYSTEM_PROMPT.as_str())
            .user(message.render_for_model())
            .output_format(OutputFormat::json_schema(schema()));
        let (request, tokens) = match ai::guard(&request, &self.privacy) {
            GuardedRequest::RedactedSkip => return Err(Degradation::NothingToReview),
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

        let _permit = gate::acquire_capacity(&self.semaphore, &self.rate_limiter, cancel)
            .await
            .map_err(|error| match error.reason() {
                crate::ErrorReason::DeadlineExceeded => Degradation::Cancelled,
                _ => Degradation::Unavailable(error.to_string()),
            })?;

        let started = std::time::Instant::now();
        // `cancel` here, on top of the deadline [`Self::check`] wraps this
        // whole function in: the two answer different questions. The deadline
        // bounds how long a *send* waits; this stops the call the moment the
        // client hangs up or the daemon starts shutting down, rather than
        // holding a permit and a connection for the rest of the window.
        //
        // `biased`, so a cancelled call is reported as cancelled rather than
        // as whatever error the provider happened to return while unwinding.
        // Without it the two arms race and the same shutdown reads as an
        // outage half the time, which is exactly the kind of ambiguity this
        // module's report exists to remove.
        let outcome = tokio::select! {
            biased;
            () = cancel.cancelled() => Err(Degradation::Cancelled),
            result = self.provider.complete(&request, cancel) => match result {
                Err(error) => Err(Degradation::Unavailable(error.to_string())),
                Ok(response) => Ok(response),
            },
        };
        let latency = started.elapsed();

        let response = match outcome {
            Ok(response) => response,
            Err(degradation) => {
                // A degraded call is still audited: the ledger is the record
                // of what this machine tried to send to a provider, not only
                // of what succeeded. A failure to record is logged and
                // swallowed rather than replacing the real reason.
                //
                // The one gap is a deadline: [`Self::check`] wraps this whole
                // function in `send.preflight.timeout`, so an expiry drops
                // this future before it reaches here and no row is written.
                // That is the price of bounding the *whole* review rather than
                // only the network call, and it is the right way round — a
                // missing ledger row is an accounting gap, an unbounded wait
                // is a stalled send.
                self.audit(
                    message,
                    &model,
                    &payload,
                    redaction_level,
                    latency,
                    ai::Usage::default(),
                    None,
                    CallOutcome::Error(degradation.describe()),
                )
                .await;
                return Err(degradation);
            }
        };

        self.audit(
            message,
            &model,
            &payload,
            redaction_level,
            latency,
            response.usage,
            Some(response.id.clone()),
            CallOutcome::Ok,
        )
        .await;

        let findings = parse_review(&ai::rehydrate(&response.text, &tokens))
            .map_err(|error| Degradation::Unreadable(error.to_string()))?;
        Ok((model, findings))
    }

    /// Record one call in the AI ledger. Never fails the caller — see its one
    /// call site's comment.
    #[allow(clippy::too_many_arguments)]
    async fn audit(
        &self,
        message: &PreflightMessage,
        model: &str,
        payload: &[u8],
        redaction_level: String,
        latency: std::time::Duration,
        usage: ai::Usage,
        request_id: Option<String>,
        outcome: CallOutcome,
    ) {
        if let Err(error) = ai::record_call(
            &self.db,
            CallRecord {
                account_id: Some(message.account_id),
                // No local `messages` row exists for something that has not
                // been sent, and inventing one would corrupt every join the
                // ledger supports.
                message_id: None,
                request_id,
                model: model.to_owned(),
                pass: Some(PASS.to_owned()),
                usage,
                redaction_level,
                latency,
                payload,
                outcome,
            },
        )
        .await
        {
            tracing::warn!(%error, "could not record a pre-send guardian call");
        }
    }
}

/// Fold model findings into the deterministic ones.
///
/// A kind already reported deterministically is not reported twice: the
/// offline finding is reproducible and its severity is authoritative, so the
/// model's restatement of it adds a line and no information. Order is
/// preserved — deterministic first — so a report reads the way the checks ran.
fn merge(findings: &mut Vec<Finding>, from_model: Vec<Finding>) {
    for finding in from_model {
        if findings
            .iter()
            .any(|existing| existing.kind == finding.kind)
        {
            continue;
        }
        findings.push(finding);
    }
}

/// The JSON Schema every review is constrained to. Byte-stable across calls,
/// for the prompt-cache reason [`SYSTEM_PROMPT`] documents.
fn schema() -> serde_json::Value {
    let kinds: Vec<&str> = FindingKind::ALL.iter().map(|k| k.as_str()).collect();
    serde_json::json!({
        "type": "object",
        "properties": {
            "findings": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "kind": {"type": "string", "enum": kinds},
                        "severity": {"type": "string", "enum": ["notice", "warn"]},
                        "detail": {"type": "string"},
                    },
                    "required": ["kind", "severity", "detail"],
                    "additionalProperties": false,
                },
            },
        },
        "required": ["findings"],
        "additionalProperties": false,
    })
}

/// The raw shape [`schema`] describes, before validation.
#[derive(Deserialize)]
struct RawReview {
    findings: Vec<ModelFinding>,
}

/// One finding as the model wrote it.
#[derive(Deserialize)]
struct ModelFinding {
    kind: String,
    severity: String,
    detail: String,
}

impl ModelFinding {
    /// Validate one model finding, or drop it.
    ///
    /// Two clamps, and they are the module's second rule made mechanical:
    ///
    /// - A `kind` outside [`FindingKind::ALL`] yields `None`. The structured
    ///   output mode makes it unlikely; it is re-checked because `enum`
    ///   membership is a claim about *values*, and a kind this build cannot
    ///   read would otherwise become a finding with no meaning.
    /// - `severity` is capped at [`Severity::Warn`], and an unreadable one
    ///   becomes [`Severity::Notice`]. A model answering "block" — because
    ///   it decided a message was important, or because a quoted attacker
    ///   told it to — must not be able to stop mail.
    fn into_finding(self) -> Option<Finding> {
        let kind = FindingKind::parse(self.kind.trim())?;
        let severity = match Severity::parse(self.severity.trim()) {
            Some(severity) => severity.min(Severity::Warn),
            None => Severity::Notice,
        };
        let detail = injection::sanitize_model_text(&self.detail).into_owned();
        let detail = bounded_detail(detail);
        if detail.is_empty() {
            return None;
        }
        Some(Finding {
            kind,
            severity,
            detail,
            from_model: true,
        })
    }
}

/// Parse and validate one review response.
///
/// # Errors
/// [`Error::Internal`] if `text` is not valid JSON for [`schema`]. Findings
/// inside a well-formed answer are validated individually and dropped rather
/// than failing the whole review: one unreadable line must not throw away the
/// tone finding next to it.
pub fn parse_review(text: &str) -> Result<Vec<Finding>, Error> {
    let raw: RawReview = serde_json::from_str(text).map_err(|e| {
        Error::internal(format!(
            "the pre-send review did not match the requested schema: {e}"
        ))
    })?;
    let total = raw.findings.len();
    let findings: Vec<Finding> = raw
        .findings
        .into_iter()
        .filter_map(ModelFinding::into_finding)
        .take(MAX_MODEL_FINDINGS)
        .collect();
    if findings.len() < total.min(MAX_MODEL_FINDINGS) {
        tracing::debug!(
            kept = findings.len(),
            returned = total,
            "dropped pre-send findings this build cannot read"
        );
    }
    Ok(findings)
}
