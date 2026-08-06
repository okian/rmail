//! The Claude bridge: turning mail into model calls and model calls back into
//! mail.
//!
//! # The pipeline this module is one stage of
//!
//! ```text
//! Sync Engine ──▶ AI Queue ──▶ redact ──▶ Provider ──▶ audit ──▶ policy
//! ```
//!
//! Every later stage in that pipeline is a separate task: the PII redaction
//! firewall, the append-only audit ledger, the data-residency policy engine,
//! and the durable worker queue all land after this one and all depend on it.
//! What they depend on is narrow on purpose — [`provider::Provider`], a plain
//! request/response/stream trait with no opinion about what talks to it or
//! what it is talking about. Redaction runs *before* a request reaches a
//! `Provider`; audit and policy wrap *around* a call to one. None of that
//! needs to exist yet for the trait to be right, and building it here would
//! mean guessing at their contracts three tasks early.
//!
//! One consequence: [`ClaudeProvider`] enforces none of `ai.limits`
//! (`max_concurrency`, `requests_per_minute`, the token/cost caps) and
//! [`provider::build`] does not consult `ai.enabled`. Those are policy —
//! deciding *whether* and *how fast* to call a provider — not the provider
//! itself, and belong to the queue/policy tasks that sit around this one in
//! the pipeline above, the same way `Provider::complete` does not decide
//! whether a message was allowed to reach it in the first place.
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
pub mod policy;
pub mod provider;
pub mod redact;

pub use audit::{
    estimate_cost_usd, query_calls, record_call, usage_for_day, AuditFilter, CallOutcome,
    CallRecord, CallStatus, DayUsage, LedgerEntry,
};
pub use provider::{
    build, ChatMessage, ChatRequest, ChatResponse, ClaudeProvider, OutputFormat, Provider,
    ProviderStream, Role, StopReason, StreamFrame, Usage,
};
