//! Mailbox RAG — the engine behind `AiService.AskMailbox` and `mail ask`
//! (prd.md: "Answers natural-language questions over the whole local mailbox
//! by fusing FTS5 recall with embeddings, feeding top-k chunks to Claude, and
//! streaming a cited answer linked back to message-ids").
//!
//! ```text
//! question ──▶ retrieve (hybrid + L2 rerank) ──▶ policy gate ──▶ pack
//!          ──▶ redact ──▶ Claude (stream) ──▶ tokens ──▶ citations ──▶ audit
//! ```
//!
//! # Retrieval is injected, not reimplemented
//!
//! [`AskRetriever`] is a two-line trait, and `rmaild`'s `SearchApi` is its one
//! implementation. That is deliberate: the pipeline this feature needs — plan,
//! fan out, fuse, extract features, rank, **rerank**, present — already exists
//! and is already assembled once, in the daemon. A second assembly here would
//! be a second search engine whose relevance could drift from the one every
//! other surface uses, and prd.md is explicit that `ask_mailbox` is "built on
//! the same pipeline (retrieve → rerank → generate)."
//!
//! The retriever declares the search deep ([`crate::rank::l2::SearchKind::Deep`]),
//! which is the seam task 51 built for exactly this caller: under the default
//! `search.rerank = "auto"`, deep is what routes to the Claude listwise
//! reranker instead of the interactive cross-encoder.
//!
//! # Citations are looked up, not believed
//!
//! Sources are labelled `[1]`, `[2]`, ... in the prompt and the model cites
//! those labels inline. [`cite::resolve`] maps a label back to the source it
//! was given; a label outside the range resolves to nothing and is dropped.
//! There is therefore no answer the model can produce that yields a citation
//! naming a message this daemon did not retrieve — see [`cite`]'s own docs.
//! The quote on a citation is extracted locally from the exact text that was
//! packed, never taken from the model.
//!
//! # Grounding is a server-side verdict
//!
//! [`AskOutcome::grounded`] is `true` only when the answer cited at least one
//! real source. Nothing the model says sets it. Two refusals fall out of that
//! one rule:
//!
//! - **Nothing to answer from.** Retrieval found nothing, or everything it
//!   found is withheld by `ai.policy`. No request is built and the provider is
//!   never called — the cheapest and most important refusal, since it is also
//!   the path a `forbidden`/`local_only` folder takes.
//! - **An answer that cites nothing.** The model wrote prose but pointed at no
//!   source (or only at labels that do not exist). The prose still streams —
//!   it is usually the model correctly saying it cannot find anything — but
//!   the terminal frame reports it ungrounded, so no client can present an
//!   uncited answer as a sourced one.
//!
//! # Order of operations, and why it is not negotiable
//!
//! retrieve → **policy** → pack → concurrency permit → RPM → cost gate →
//! budget → **redact** → provider → audit.
//!
//! The policy gate comes before any text is assembled ([`context::pack`]'s own
//! docs), because a `forbidden`/`local_only` folder's body must never be in a
//! string that a later step could send. The concurrency permit and the RPM
//! token come *before* the budget check, not after — the order
//! [`crate::ai::queue`] documents at length: a budget check taken before an
//! unbounded wait can be arbitrarily stale by the time the call is made, and
//! what bounds the overshoot is how many checks can be outstanding at once,
//! which is exactly what the shared semaphore caps. Redaction is the last
//! thing before the call, so nothing can reintroduce raw text afterwards, and
//! the audit entry records the *redacted* payload because that is what was
//! actually transmitted.
//!
//! # Rehydration has to survive being streamed
//!
//! [`crate::ai::redact::rehydrate`] turns `⟦CARD_1⟧` back into the real value,
//! and every other caller runs it over a complete response. This one cannot:
//! tokens arrive in arbitrary slices, and a token split across two frames
//! would be rehydrated as two pieces of literal garbage. [`Rehydrator`] holds
//! back the shortest possible suffix that could still be the start of a token
//! and flushes the rest — so the stream stays live and no token is ever cut in
//! half.

pub mod cite;
pub mod context;

use std::ops::ControlFlow;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use futures::{Stream, StreamExt};
use tokio::sync::{mpsc, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::ai::audit::{record_call, CallOutcome, CallRecord};
use crate::ai::budget::{
    BudgetEnforcer, BudgetRequest, BudgetVerdict, WorkClass, GLOBAL_ACCOUNT_ID,
};
use crate::ai::provider::{ChatRequest, Provider, StopReason, StreamFrame, Usage};
use crate::ai::queue::{payload_bytes, CapDecision, CostGate, RateLimiter};
use crate::ai::redact::{guard, rehydrate, GuardedRequest, TokenMap};
use crate::ai::PolicyEngine;
use crate::config::{AiAsk, AiLimits, AiPrivacy};
use crate::error::Error;
use crate::storage::Database;

pub use cite::Citation;
pub use context::{Packed, Source};

/// The pass name recorded in `ai_ledger.pass`, and the tracing field this
/// path is identified by.
pub const PASS: &str = "ask_mailbox";

/// How many events may sit between the engine and its consumer before the
/// engine applies backpressure. Matches the daemon's own stream buffers.
const EVENT_BUFFER: usize = 64;

/// The system prompt. Frozen text: prompt caching is a byte-identical-prefix
/// match (see [`ChatRequest::system`]'s own docs), and only the per-question
/// user turn should vary between calls.
const SYSTEM_BASE: &str = "You answer questions about a user's own email, using only the numbered \
     source messages you are given. \
     Cite every claim with the source's bracketed label, exactly as given — write [2] inline, \
     immediately after the claim it supports. Never invent a label; only labels that appear in \
     the sources exist. \
     If the sources do not contain the answer, say plainly that you could not find it in the \
     mail you were shown, and cite nothing. Never answer from general knowledge, and never \
     guess at a number, date, or amount that is not in a source. \
     Be brief: a few sentences, no preamble, no restating the question.";

/// [`SYSTEM_BASE`] plus [`injection::DATA_BOUNDARY_CLAUSE`].
///
/// Built once: prompt caching is a byte-identical-prefix match, so the clause
/// has to be appended in exactly one place rather than per request.
static SYSTEM: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| crate::ai::injection::with_data_boundary(SYSTEM_BASE));

/// A question to answer over the mailbox.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AskRequest {
    /// The natural-language question.
    pub question: String,
    /// Restrict retrieval to one account; `0` means every configured account.
    pub account_id: i64,
    /// Extra operator-DSL terms folded into retrieval (`in:`, `from:`,
    /// `after:` ...), exactly as `SearchRequest.filter` is.
    pub filter: String,
    /// How many messages to retrieve; `0` means `ai.ask.top_k`.
    pub top_k: u32,
}

/// What retrieval this engine draws on. One implementation
/// (`rmaild::SearchApi`) — see the module docs for why the pipeline is
/// injected rather than rebuilt here.
#[async_trait]
pub trait AskRetriever: Send + Sync + std::fmt::Debug {
    /// Message ids for `question`, best first, at most `top_k` of them.
    ///
    /// # Errors
    ///
    /// Whatever retrieval could not do — an unknown account, a failed plan.
    /// An empty result is a success, and the caller turns it into a refusal.
    async fn retrieve(
        &self,
        question: &str,
        filter: &str,
        account_id: i64,
        top_k: usize,
        cancel: &CancellationToken,
    ) -> Result<Vec<i64>, Error>;
}

/// What retrieval produced, for the client's own display and for an operator
/// reading a trace — prd.md's `RetrievalTrace`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetrievalTrace {
    /// Candidates retrieval returned.
    pub retrieved: usize,
    /// Candidates that made it into the prompt.
    pub packed: usize,
    /// Candidates dropped because `ai.policy` does not let their folder reach
    /// a network provider.
    pub withheld_by_policy: usize,
    /// Candidates dropped because the context budget was already full.
    pub dropped_for_budget: usize,
    /// Estimated tokens of context sent.
    pub context_tokens: usize,
    /// The model actually called — which a soft budget cap may have
    /// downgraded from `ai.ask.model`. Empty when no call was made.
    pub model: String,
}

/// Why an answer was not grounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Retrieval found nothing, or `ai.policy` withheld everything it found.
    /// No provider call was made.
    NoContext,
    /// The model answered but cited no source this daemon retrieved.
    Uncited,
}

impl Refusal {
    /// A one-line explanation for a user.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::NoContext => {
                "no message in your mailbox could be used to answer this — either nothing \
                 matched, or the AI policy withholds the folders that did"
            }
            Self::Uncited => {
                "the answer cited no message in your mailbox, so it is not grounded in your mail"
            }
        }
    }
}

/// How an answer ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskOutcome {
    /// Whether the answer cited at least one real source. Set by this engine
    /// from the resolved citations — never by the model.
    pub grounded: bool,
    /// Why not, when `grounded` is `false`.
    pub refusal: Option<Refusal>,
    /// The model's own stop reason, when a call was made.
    pub stop_reason: Option<StopReason>,
}

/// One frame of an answer.
///
/// The order is fixed and total: exactly one [`AskEvent::Trace`] first, then
/// zero or more [`AskEvent::Token`]s, then zero or more
/// [`AskEvent::Citation`]s, then at most one [`AskEvent::Usage`], then exactly
/// one [`AskEvent::Done`] — unless the stream fails, in which case the error
/// is terminal and no `Done` follows it.
#[derive(Debug, Clone, PartialEq)]
pub enum AskEvent {
    /// What retrieval found. Always first.
    Trace(RetrievalTrace),
    /// A slice of the answer, in arrival order. Concatenating every `Token`
    /// reproduces the answer.
    Token(String),
    /// A source the answer cited. Emitted after the prose, because a citation
    /// is only resolvable once the whole answer has been seen.
    Citation(Citation),
    /// Final token accounting for the call.
    Usage(Usage),
    /// How the answer ended. Always last.
    Done(AskOutcome),
}

/// A live answer.
pub type AskStream = Pin<Box<dyn Stream<Item = Result<AskEvent, Error>> + Send>>;

/// The mailbox-RAG engine.
///
/// Cheap to clone (a `Database` handle and `Arc`s), because [`Self::ask`]
/// drives the answer from a spawned task.
#[derive(Clone)]
pub struct RagEngine {
    db: Database,
    provider: Arc<dyn Provider>,
    policy: Arc<PolicyEngine>,
    retriever: Arc<dyn AskRetriever>,
    privacy: AiPrivacy,
    limits: AiLimits,
    config: AiAsk,
    /// `ai.limits.max_concurrency`, **shared** with the daemon's
    /// `AiWorkerPool` rather than a second semaphore of this engine's own —
    /// the same reasoning [`crate::rank::l2::ClaudeReranker`]'s identical
    /// field gives: one process must not exceed one configured ceiling
    /// because it has three call sites.
    semaphore: Arc<Semaphore>,
    /// `ai.limits.requests_per_minute`, shared for the identical reason.
    rate_limiter: Arc<RateLimiter>,
}

impl std::fmt::Debug for RagEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RagEngine")
            .field("model", &self.config.model)
            .field("top_k", &self.config.top_k)
            .finish_non_exhaustive()
    }
}

impl RagEngine {
    /// Build the engine over an already-constructed provider, policy engine
    /// and retriever.
    ///
    /// Every dependency is injected for the reason `ClaudeReranker::new`
    /// documents: the daemon owns exactly one `Provider` (one API-key
    /// resolution, one HTTP client) and one `ai.limits` concurrency/pacing
    /// budget for the whole process, and a component that built its own would
    /// make both a fiction.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: Database,
        provider: Arc<dyn Provider>,
        policy: Arc<PolicyEngine>,
        retriever: Arc<dyn AskRetriever>,
        privacy: AiPrivacy,
        limits: AiLimits,
        config: AiAsk,
        semaphore: Arc<Semaphore>,
        rate_limiter: Arc<RateLimiter>,
    ) -> Self {
        Self {
            db,
            provider,
            policy,
            retriever,
            privacy,
            limits,
            config,
            semaphore,
            rate_limiter,
        }
    }

    /// Answer `req` over the mailbox, streaming the result.
    ///
    /// Retrieval, the policy gate and packing run before this returns, so a
    /// caller that cannot even build a context learns so from the returned
    /// stream's very first frames rather than after an open stream stalls.
    /// Everything from the concurrency permit onward runs in a spawned task
    /// — see the module docs for why the permit cannot move ahead of it.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] for an empty question, and whatever
    /// retrieval or the context read failed with. A context that is merely
    /// *empty* is not an error: it produces a refusal on the stream.
    #[tracing::instrument(
        skip(self, req, cancel),
        fields(
            account_id = req.account_id,
            top_k = tracing::field::Empty,
            retrieved = tracing::field::Empty,
            packed = tracing::field::Empty,
            withheld = tracing::field::Empty,
        )
    )]
    pub async fn ask(
        &self,
        req: &AskRequest,
        cancel: &CancellationToken,
    ) -> Result<AskStream, Error> {
        let question = req.question.trim().to_owned();
        if question.is_empty() {
            return Err(Error::invalid_argument("a question is required"));
        }
        let span = tracing::Span::current();
        let top_k = if req.top_k == 0 {
            self.config.top_k as usize
        } else {
            req.top_k as usize
        }
        .max(1);
        span.record("top_k", top_k);

        let ids = self
            .retriever
            .retrieve(&question, &req.filter, req.account_id, top_k, cancel)
            .await?;
        let packed = context::pack(
            &self.db,
            &ids,
            &self.policy,
            &self.config,
            self.privacy.max_body_chars as usize,
            cancel,
        )
        .await?;
        span.record("retrieved", packed.retrieved);
        span.record("packed", packed.sources.len());
        span.record("withheld", packed.withheld_by_policy);

        let mut trace = RetrievalTrace {
            retrieved: packed.retrieved,
            packed: packed.sources.len(),
            withheld_by_policy: packed.withheld_by_policy,
            dropped_for_budget: packed.dropped_for_budget,
            context_tokens: packed.context_tokens,
            model: String::new(),
        };

        if packed.sources.is_empty() {
            // The refusal that never touches a provider. Both halves matter:
            // nothing matched, or everything that matched is `local_only`/
            // `forbidden` — and in the second case the whole point is that no
            // request is built at all.
            tracing::info!(
                retrieved = packed.retrieved,
                withheld = packed.withheld_by_policy,
                "ask-mailbox has no usable context; refusing without calling the provider"
            );
            return Ok(refusal_stream(trace, Refusal::NoContext));
        }
        trace.model = self.config.model.clone();

        let (tx, rx) = mpsc::channel(EVENT_BUFFER);
        let this = self.clone();
        let cancel = cancel.clone();
        tokio::spawn(
            async move {
                this.run(question, packed, trace, cancel, tx).await;
            }
            .instrument(tracing::Span::current()),
        );
        Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }

    /// The half of an answer that can reach the network: pace, budget,
    /// redact, stream, cite, audit.
    async fn run(
        self,
        question: String,
        packed: Packed,
        mut trace: RetrievalTrace,
        cancel: CancellationToken,
        tx: mpsc::Sender<Result<AskEvent, Error>>,
    ) {
        // Concurrency and pacing first, then the budget, then the call — see
        // the module docs. A caller that gave up while we waited gets nothing
        // further; `send` below detects that on its own.
        let _permit = {
            let semaphore = Arc::clone(&self.semaphore);
            tokio::select! {
                () = cancel.cancelled() => {
                    let _ = tx.send(Err(Error::deadline_exceeded(
                        "cancelled while waiting for AI concurrency capacity".to_owned(),
                    ))).await;
                    return;
                }
                permit = semaphore.acquire_owned() => match permit {
                    Ok(permit) => permit,
                    Err(_) => {
                        let _ = tx.send(Err(Error::internal(
                            "the ai concurrency semaphore was closed".to_owned(),
                        ))).await;
                        return;
                    }
                },
            }
        };
        tokio::select! {
            () = cancel.cancelled() => {
                let _ = tx.send(Err(Error::deadline_exceeded(
                    "cancelled while waiting for AI rate-limit capacity".to_owned(),
                ))).await;
                return;
            }
            () = self.rate_limiter.acquire() => {}
        }

        let model = match self.budgeted_model().await {
            Ok(model) => model,
            Err(error) => {
                let _ = tx.send(Err(error)).await;
                return;
            }
        };
        trace.model.clone_from(&model);
        if send(&tx, &cancel, Ok(AskEvent::Trace(trace)))
            .await
            .is_break()
        {
            return;
        }

        let request = ChatRequest::new(model, self.config.max_tokens.max(256))
            .system(SYSTEM.as_str())
            .user(prompt(&question, &packed.sources));

        // The firewall. Nothing between here and `provider.stream` may add
        // text to the request — see the module docs.
        let GuardedRequest::Redacted {
            request, tokens, ..
        } = guard(&request, &self.privacy)
        else {
            let _ = tx
                .send(Err(Error::failed_precondition(
                    "nothing was left to answer from once PII was redacted from the retrieved \
                     messages"
                        .to_owned(),
                )))
                .await;
            return;
        };
        let payload = payload_bytes(&request);
        let redaction_level = if tokens.is_empty() {
            "none"
        } else {
            "redacted"
        };

        let started = Instant::now();
        let stream = match self.provider.stream(&request, &cancel).await {
            Ok(stream) => stream,
            Err(error) => {
                self.audit(
                    &request.model,
                    &payload,
                    redaction_level,
                    started.elapsed(),
                    None,
                    CallOutcome::Error(error.to_string()),
                )
                .await;
                let _ = tx.send(Err(error)).await;
                return;
            }
        };

        self.relay(
            RelayContext {
                question: &question,
                packed: &packed,
                model: &request.model,
                payload: &payload,
                redaction_level,
                tokens: &tokens,
                started,
            },
            stream,
            &cancel,
            &tx,
        )
        .await;
    }

    /// Pump the provider's frames out as [`AskEvent`]s, then resolve
    /// citations over the completed answer and finish the stream.
    async fn relay(
        &self,
        ctx: RelayContext<'_>,
        mut stream: crate::ai::provider::ProviderStream,
        cancel: &CancellationToken,
        tx: &mpsc::Sender<Result<AskEvent, Error>>,
    ) {
        let mut answer = String::new();
        let mut usage = Usage::default();
        let mut rehydrator = Rehydrator::new(ctx.tokens);

        loop {
            let next = tokio::select! {
                () = cancel.cancelled() => {
                    self.audit_incomplete(&ctx, "cancelled").await;
                    return;
                }
                // Detected the instant the consumer goes away — the same race
                // `rmaild::ai_service::run_analyze_stream` documents. Returning
                // here drops `stream`, which closes the provider's own channel,
                // which is what aborts the upstream HTTP request rather than
                // merely abandoning the local relay.
                () = tx.closed() => {
                    self.audit_incomplete(&ctx, "client disconnected").await;
                    return;
                }
                next = stream.next() => next,
            };
            let Some(frame) = next else {
                let error = Error::unavailable("the provider closed the stream before it finished");
                self.audit(
                    ctx.model,
                    ctx.payload,
                    ctx.redaction_level,
                    ctx.started.elapsed(),
                    Some(usage),
                    CallOutcome::Error(error.to_string()),
                )
                .await;
                let _ = tx.send(Err(error)).await;
                return;
            };
            match frame {
                Ok(StreamFrame::Token(token)) => {
                    answer.push_str(&token);
                    let ready = rehydrator.push(&token);
                    if !ready.is_empty()
                        && send(tx, cancel, Ok(AskEvent::Token(ready)))
                            .await
                            .is_break()
                    {
                        self.audit_incomplete(&ctx, "client disconnected").await;
                        return;
                    }
                }
                // Nothing in this request gives the model a tool to call, so
                // a tool-use block is not something a client of `AskMailbox`
                // has any frame for. Ignored rather than surfaced.
                Ok(StreamFrame::ToolUseStart { .. }) => {}
                Ok(StreamFrame::Usage(u)) => usage = u,
                Ok(StreamFrame::Done { stop_reason }) => {
                    let tail = rehydrator.flush();
                    if !tail.is_empty()
                        && send(tx, cancel, Ok(AskEvent::Token(tail))).await.is_break()
                    {
                        self.audit_incomplete(&ctx, "client disconnected").await;
                        return;
                    }
                    self.finish(&ctx, &answer, usage, stop_reason, cancel, tx)
                        .await;
                    return;
                }
                Err(error) => {
                    self.audit(
                        ctx.model,
                        ctx.payload,
                        ctx.redaction_level,
                        ctx.started.elapsed(),
                        Some(usage),
                        CallOutcome::Error(error.to_string()),
                    )
                    .await;
                    let _ = tx.send(Err(error)).await;
                    return;
                }
            }
        }
    }

    /// Resolve the answer's citations, audit the call, and send the terminal
    /// frames.
    async fn finish(
        &self,
        ctx: &RelayContext<'_>,
        answer: &str,
        usage: Usage,
        stop_reason: StopReason,
        cancel: &CancellationToken,
        tx: &mpsc::Sender<Result<AskEvent, Error>>,
    ) {
        // Resolved over the *rehydrated* answer, so a label the redaction
        // firewall happened to tokenize still resolves. `⟦...⟧` tokens are
        // never digit runs in brackets, so this cannot mint a label either
        // way — it is done for the citation quotes' benefit, not the labels'.
        let rehydrated = rehydrate(answer, ctx.tokens);
        let (citations, dangling) = cite::resolve(&rehydrated, &ctx.packed.sources, ctx.question);
        self.audit(
            ctx.model,
            ctx.payload,
            ctx.redaction_level,
            ctx.started.elapsed(),
            Some(usage),
            CallOutcome::Ok,
        )
        .await;

        let grounded = !citations.is_empty();
        tracing::info!(
            citations = citations.len(),
            dangling,
            grounded,
            sources = ctx.packed.sources.len(),
            "ask-mailbox answered"
        );
        for citation in citations {
            if send(tx, cancel, Ok(AskEvent::Citation(citation)))
                .await
                .is_break()
            {
                return;
            }
        }
        if send(tx, cancel, Ok(AskEvent::Usage(usage)))
            .await
            .is_break()
        {
            return;
        }
        let _ = send(
            tx,
            cancel,
            Ok(AskEvent::Done(AskOutcome {
                grounded,
                refusal: (!grounded).then_some(Refusal::Uncited),
                stop_reason: Some(stop_reason),
            })),
        )
        .await;
    }

    /// Consult the daemon-wide spend cap and this call's own budget, and
    /// return the model to actually use.
    ///
    /// # Errors
    ///
    /// [`Error::ResourceExhausted`] when either says no. Unlike the L2 rerank
    /// — which degrades to the L1 order — there is no cheaper answer to
    /// degrade to here: the whole RPC *is* the model call.
    async fn budgeted_model(&self) -> Result<String, Error> {
        let gate = CostGate {
            db: &self.db,
            limits: &self.limits,
        };
        match gate.decide().await? {
            CapDecision::Open => {}
            other => {
                return Err(Error::resource_exhausted(format!(
                    "the AI spend cap is closed ({other:?}); ask-mailbox cannot run until it \
                     resets or an operator raises the cap"
                )));
            }
        }
        // Charged to the global budget rather than a per-account one: a
        // question spans every configured account by default, so there is no
        // single account this call is "for" — the identical reasoning
        // `rank::l2::claude::ClaudeReranker::budgeted_model` gives. A user is
        // waiting on it, so `Interactive` is both what the check uses and what
        // `record_call` attributes it as.
        let verdict = BudgetEnforcer {
            db: &self.db,
            limits: &self.limits,
        }
        .evaluate(&BudgetRequest {
            account_id: GLOBAL_ACCOUNT_ID,
            model: &self.config.model,
            work_class: WorkClass::Interactive,
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
                    "ai budget soft cap: downgrading the ask-mailbox model"
                );
                Ok(model)
            }
            // The detailed reason names aggregate spend figures, and this path
            // is reachable with `ai.invoke` while reading spend needs `admin`
            // — so the detail goes to the log and the caller is told only that
            // a cap was reached. Same split `rmaild::ai_service` applies to a
            // forced analysis.
            BudgetVerdict::Block { reason, .. } => {
                tracing::info!(reason = %reason, "ai budget hard cap: refusing ask-mailbox");
                Err(Error::resource_exhausted(
                    "an AI spend budget has been reached; ask-mailbox cannot run until the \
                     window resets or an operator raises the budget"
                        .to_owned(),
                ))
            }
        }
    }

    /// One ledger row for a call that did not complete — a cancelled stream
    /// or a client that hung up. Recorded because prd.md's ledger is a record
    /// of what left the machine, and an aborted call still consumed one.
    async fn audit_incomplete(&self, ctx: &RelayContext<'_>, why: &str) {
        tracing::debug!(why, "ask-mailbox stream ended early");
        self.audit(
            ctx.model,
            ctx.payload,
            ctx.redaction_level,
            ctx.started.elapsed(),
            None,
            CallOutcome::Error(format!("ask-mailbox stream ended early: {why}")),
        )
        .await;
    }

    /// Write one ledger row. Never propagates: an audit write that fails must
    /// not turn a served answer into an error, and the failure is logged where
    /// an operator will see it.
    async fn audit(
        &self,
        model: &str,
        payload: &[u8],
        redaction_level: &str,
        latency: std::time::Duration,
        usage: Option<Usage>,
        outcome: CallOutcome,
    ) {
        let record = CallRecord {
            account_id: None,
            // A question is about a *mailbox*, not a message: attributing it
            // to one of the retrieved sources would make `mail ai audit
            // --message <id>` claim a call was made "for" a message that
            // merely appeared in somebody's context.
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
        if let Err(error) = record_call(&self.db, record).await {
            tracing::warn!(%error, "could not write the ask-mailbox audit entry");
        }
    }
}

/// Everything [`RagEngine::relay`]/[`RagEngine::finish`] need that does not
/// change frame to frame. Grouped so both stay inside
/// `clippy::too_many_arguments`' limit without either taking a parameter it
/// does not use.
struct RelayContext<'a> {
    question: &'a str,
    packed: &'a Packed,
    model: &'a str,
    payload: &'a [u8],
    redaction_level: &'a str,
    tokens: &'a TokenMap,
    started: Instant,
}

/// A stream that carries a trace and a refusal and nothing else — the answer
/// to a question with no usable context, produced without a provider call.
fn refusal_stream(trace: RetrievalTrace, refusal: Refusal) -> AskStream {
    Box::pin(tokio_stream::iter(vec![
        Ok(AskEvent::Trace(trace)),
        Ok(AskEvent::Done(AskOutcome {
            grounded: false,
            refusal: Some(refusal),
            stop_reason: None,
        })),
    ]))
}

/// Send one event, treating a cancelled token as a closed channel.
///
/// [`ControlFlow::Break`] means "stop": either the consumer is gone or this
/// answer has been superseded, and in both cases nothing further should be
/// produced. Shaped like `rmaild`'s own stream `send` helpers so the call
/// sites read the same way.
async fn send(
    tx: &mpsc::Sender<Result<AskEvent, Error>>,
    cancel: &CancellationToken,
    event: Result<AskEvent, Error>,
) -> ControlFlow<()> {
    if cancel.is_cancelled() {
        return ControlFlow::Break(());
    }
    tokio::select! {
        () = cancel.cancelled() => ControlFlow::Break(()),
        sent = tx.send(event) => match sent {
            Ok(()) => ControlFlow::Continue(()),
            Err(_) => ControlFlow::Break(()),
        },
    }
}

/// The user turn: the question, then one labelled block per source.
fn prompt(question: &str, sources: &[Source]) -> String {
    let mut out = String::with_capacity(sources.len() * 1_024);
    out.push_str("Question: ");
    out.push_str(question);
    out.push_str("\n\nSources:\n\n");
    for (index, source) in sources.iter().enumerate() {
        out.push_str(&source.render(index + 1));
        out.push('\n');
    }
    out.push_str(
        "Answer the question using only the sources above, citing each claim with its bracketed \
         label.\n",
    );
    out
}

/// Rehydrates [`crate::ai::redact`] tokens across a token stream without ever
/// cutting one in half.
///
/// A redaction token is `⟦TAG_123⟧`. The invariant this keeps is simple: never
/// emit text that could still turn out to be the *start* of a token. So the
/// buffer holds back everything from the last unterminated `⟦` onward, and
/// releases it the moment the closing bracket arrives — or the moment the
/// candidate grows past the longest a real token could be, at which point it
/// was never a token and holding it back would stall the stream forever.
struct Rehydrator<'a> {
    tokens: &'a TokenMap,
    pending: String,
}

/// The longest a redaction token can be, in bytes: `⟦` and `⟧` are three
/// bytes each, the tag is ASCII, and the counter is bounded by how many
/// distinct values one request can contain. Generous on purpose — the cost of
/// overestimating is a few bytes of latency on a stream that contains a `⟦`
/// at all, and the cost of underestimating is a token emitted in two halves.
const MAX_TOKEN_BYTES: usize = 64;

impl<'a> Rehydrator<'a> {
    const fn new(tokens: &'a TokenMap) -> Self {
        Self {
            tokens,
            pending: String::new(),
        }
    }

    /// Absorb `chunk` and return whatever is now safe to emit.
    fn push(&mut self, chunk: &str) -> String {
        if self.tokens.is_empty() {
            // No tokens were minted, so nothing in the stream can be one and
            // there is nothing to hold back.
            return chunk.to_owned();
        }
        self.pending.push_str(chunk);
        let hold_from = self.hold_from();
        let ready = self.pending.get(..hold_from).unwrap_or_default().to_owned();
        self.pending = self.pending.get(hold_from..).unwrap_or_default().to_owned();
        rehydrate(&ready, self.tokens)
    }

    /// Emit whatever is still held back — the stream is over, so a partial
    /// token is just text.
    fn flush(&mut self) -> String {
        let tail = std::mem::take(&mut self.pending);
        rehydrate(&tail, self.tokens)
    }

    /// The byte offset from which `pending` must be held back: the last `⟦`
    /// with no `⟧` after it, when that candidate is still short enough to
    /// become a real token.
    fn hold_from(&self) -> usize {
        let Some(open) = self.pending.rfind('⟦') else {
            return self.pending.len();
        };
        let tail = self.pending.get(open..).unwrap_or_default();
        if tail.contains('⟧') || tail.len() > MAX_TOKEN_BYTES {
            // Already closed (so `rehydrate` can resolve it), or too long to
            // ever be one.
            return self.pending.len();
        }
        open
    }
}

#[cfg(test)]
mod tests;
