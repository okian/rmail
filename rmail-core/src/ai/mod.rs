//! The Claude bridge: turning mail into model calls and model calls back into
//! mail.
//!
//! # The pipeline, now that every stage has landed
//!
//! ```text
//! Sync Engine ──▶ AI Queue ──▶ policy ──▶ assemble ──▶ fence ──▶ redact ──▶ Provider ──▶ audit
//! ```
//!
//! [`injection`] (task 77) is the `fence` step, and unlike every other box
//! it is not a stage the queue calls: it is a discipline each request
//! *builder* follows, because only the builder knows which parts of the text
//! it is assembling came from a sender and which it wrote itself. Untrusted
//! text is wrapped in labelled delimiters and the system prompt says what
//! those delimiters mean; the same module's detector records what a message
//! tried, and gates the one path — [`crate::rules`]' `claude_is` — where a
//! model answer mutates the mailbox. See its module docs for why the
//! structural separation, not the detector, is the control that carries the
//! weight.
//!
//! [`queue::AiWorkerPool`]/[`queue::BatchCoordinator`] (task 47) are what
//! actually sequence this: [`policy::PolicyEngine::resolve`] first (a
//! forbidden folder never reaches any later step), then
//! [`queue::assemble_content`] builds bounded request content, then
//! [`redact::guard`] is the mandatory PII firewall, then
//! [`provider::Provider::complete`] is the one step that leaves the
//! machine, then [`audit::record_call`] with the **redacted** payload. See
//! `queue`'s module docs for why that order is load-bearing and what
//! swapping any two steps would break. [`provider::Provider`] stays a plain
//! request/response/stream trait with no opinion about what talks to it —
//! that was deliberate when only it existed, and still holds now that
//! everything around it does too.
//!
//! [`dispatch::AiDispatchLoop`] (task 50) is the piece that makes the
//! diagram at the top of this section literally true rather than aspirational
//! documentation: `Sync Engine ──▶ AI Queue` was, until this module landed,
//! two boxes on a diagram with no arrow between them in code — every stage
//! after enqueue worked and was unit-tested, but nothing ever called
//! [`queue::AiQueue::enqueue`] when a message synced, and nothing called
//! [`queue::AiWorkerPool::dispatch_pending`] on any schedule. See that
//! module's own docs for the wiring and why it polls the durable event log
//! rather than holding a live subscription.
//!
//! One consequence, no longer hypothetical: [`ClaudeProvider`] enforces none
//! of `ai.limits` (`max_concurrency`, `requests_per_minute`, the token/cost
//! caps) and [`provider::build`] does not consult `ai.enabled`. Those are
//! policy — deciding *whether* and *how fast* to call a provider — not the
//! provider itself, and [`queue::AiWorkerPool`]/[`queue::CostGate`] are
//! exactly where they now live, the same way `Provider::complete` does not
//! decide whether a message was allowed to reach it in the first place.
//!
//! # Why a trait at all
//!
//! [`AiProvider::Local`](crate::config::AiProvider::Local) is in the config
//! schema already — a fully on-device inference path is a stated requirement
//! (mail that never leaves the machine still gets triage and summaries, just
//! from a smaller model). [`provider::Provider`] is the seam that path plugs
//! into later without every caller of a `Provider` needing to change.
//! [`provider::build`] is where that switch already lives: it matches on the
//! configured backend today, and a local implementation is a second arm away
//! from wiring in, not a redesign.

pub mod audit;
pub mod budget;
pub mod deep;
pub mod dispatch;
pub mod gate;
pub mod injection;
pub mod policy;
pub mod provider;
pub mod queue;
pub mod rag;
pub mod redact;
pub mod triage;

pub use audit::{
    estimate_cost_usd, query_calls, record_call, record_call_charged, record_call_priced,
    usage_for_day, AuditFilter, CallOutcome, CallRecord, CallStatus, DayUsage, LedgerEntry,
};
pub use budget::{
    get_budget, micros_to_usd, set_budget, spend_report, usd_to_micros, Budget, BudgetCaps,
    BudgetClass, BudgetEnforcer, BudgetRequest, BudgetVerdict, ClassReport, ModelTier, Severity,
    Spend, SpendReport, WindowCaps, WindowSpend, WorkClass, GLOBAL_ACCOUNT_ID,
};
pub use deep::{DeepPassGate, DeepPassHandler};
pub use dispatch::{
    AiDispatchLoop, AiPauseFlag, TickReport, DEFAULT_LEASE_LIMIT, DEFAULT_TICK_INTERVAL,
};
pub use provider::{
    build, ChatMessage, ChatRequest, ChatResponse, ClaudeProvider, OutputFormat, Provider,
    ProviderStream, Role, StopReason, StreamFrame, Usage,
};

// Deliberately not flattened into this module the way `redact`'s items are:
// `injection::Severity` would collide with `budget::Severity` (a spend
// severity and an attack severity are not the same concept and must not read
// as one), and every call site is clearer naming the module — `guard(...)`
// says what it does on its own, `untrusted_block(...)` needs the
// `injection::` prefix to.

pub use policy::{
    AiPolicyMode, PolicyDecision, PolicyEngine, PolicyExplanation, PolicyTarget, PolicyTier,
    RuleMatch,
};
pub use queue::{
    payload_bytes, AiLease, AiQueue, AiWorkerPool, BatchClient, BatchCoordinator, BatchHandle,
    BatchOutcome, BatchPollOutcome, BatchRequestCounts, BatchRequestItem, BatchResult, BatchStatus,
    CapDecision, CostGate, DeadLetter, DispatchSummary, Failure, JobState, MessageContent,
    NewAiJob, PassHandler, QueueOptions, QueueStats, RateLimiter,
};
pub use rag::{
    AskEvent, AskOutcome, AskRequest, AskRetriever, AskStream, Citation, RagEngine, Refusal,
    RetrievalTrace,
};
pub use redact::{
    guard, preview, rehydrate, GuardedRequest, RedactPreview, RedactionKind, TokenMap,
};
pub use triage::TriagePassHandler;
