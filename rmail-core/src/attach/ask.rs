//! Ask-your-attachment: a question answered from extracted attachment text,
//! cited by page (prd.md feature 56, "streaming an answer with page/section
//! citations, refusing when context doesn't support it").
//!
//! ```text
//! question ──▶ scope ──▶ policy gate ──▶ pack ──▶ permit ──▶ RPM
//!          ──▶ cost gate ──▶ budget ──▶ redact ──▶ Claude ──▶ citations ──▶ audit
//! ```
//!
//! # This is [`crate::ai::rag`] over documents, and deliberately the same shape
//!
//! Everything structural about that module is repeated here on purpose,
//! because each property it guarantees is a property this one has to have
//! too, and every one of them is an ordering constraint rather than a helper
//! that could simply be called:
//!
//! - **The policy gate runs before any attachment text is rendered.**
//!   [`pack`] resolves [`crate::ai::PolicyEngine`] from a metadata-only read
//!   and drops a candidate whose folder does not `permits_network()` *before*
//!   the second read that fetches text at all. A `forbidden`/`local_only`
//!   folder's contract never enters a `String` a later step could send. The
//!   two reads are separate for exactly that reason: with one read there is
//!   no point in the program at which "the text has not been fetched yet" is
//!   a fact rather than a convention.
//! - **Excerpts are fenced.** [`Passage::render`] wraps every excerpt in
//!   [`crate::ai::injection::untrusted_block`] and the system prompt carries
//!   [`crate::ai::injection::DATA_BOUNDARY_CLAUSE`]. An attachment is the
//!   *most* attacker-controlled text in a mailbox — a sender chooses every
//!   byte of a PDF — so this sink needs the fence at least as much as a body
//!   does.
//! - **Citation markers in packed text are neutralized.**
//!   [`crate::ai::rag::cite::neutralize_markers`] rewrites `[12]` to `(12)`
//!   in the excerpt and the filename, so an `[n]` in the answer can only have
//!   been written by the model. Contracts are full of bracketed numbers
//!   (`[1]`, `[Exhibit 2]`), which makes this less of an edge case here than
//!   it is for mail.
//! - **Citations are looked up, not believed.** The model sees positional
//!   labels and [`resolve`] maps a label back to the passage it was given; a
//!   label outside the range yields nothing. A citation naming an attachment
//!   this daemon did not pack is unrepresentable.
//! - **Grounding is a server-side verdict.** [`AskOutcome::grounded`] is true
//!   only when the answer cited a real passage. Nothing the model says sets
//!   it, and the two refusals ([`Refusal`]) fall out of that one rule.
//!
//! # What a citation carries, and why a page is not enough on its own
//!
//! [`AttachmentCitation`] names the message, the MIME part, the page **and**
//! the byte span the excerpt came from. The page is what a human wants; the
//! span is what makes the citation checkable, and it is the only one of the
//! two that always exists. Only PDF extraction and the OCR path write
//! `attachment_pages` rows at all; a plain-text, HTML or OOXML attachment has
//! none, and a citation that could only say "page" would therefore have
//! nothing to say about most of a real corpus. That is why "page/section" is
//! two fields rather than one.
//!
//! # Passages, not whole documents
//!
//! An extracted attachment reaches two megabytes. What is packed is a small
//! number of bounded windows per attachment, chosen around where the
//! question's own words occur — the same offsets [`super::search`] resolves a
//! page from, found by the same SQLite-side `instr` so that neither the
//! document nor a copy of it is ever pulled into this process. An attachment
//! whose text contains none of the question's words contributes its opening,
//! which is the honest thing to show for a match that was semantic.

use std::collections::BTreeSet;
use std::ops::ControlFlow;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

use futures::{Stream, StreamExt};
use rusqlite::{Connection, OptionalExtension};
use tokio::sync::{mpsc, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::ai::audit::{record_call, CallOutcome, CallRecord};
use crate::ai::budget::{
    BudgetEnforcer, BudgetRequest, BudgetVerdict, WorkClass, GLOBAL_ACCOUNT_ID,
};
use crate::ai::injection;
use crate::ai::provider::{ChatRequest, Provider, StopReason, StreamFrame, Usage};
use crate::ai::queue::{payload_bytes, CapDecision, CostGate, RateLimiter};
use crate::ai::rag::cite::neutralize_markers;
use crate::ai::rag::Rehydrator;
use crate::ai::redact::{guard, rehydrate, GuardedRequest, TokenMap};
use crate::ai::{PolicyEngine, PolicyTarget};
use crate::config::{AiAsk, AiLimits, AiPrivacy};
use crate::error::Error;
use crate::index::chunk::estimate_tokens;
use crate::present::snippet;
use crate::retrieve::cancel::interruptible_read;
use crate::storage::Database;

use super::search::{AttachmentQuery, AttachmentSearch};

#[cfg(test)]
mod tests;

/// The pass name recorded in `ai_ledger.pass`, and the tracing field this
/// path is identified by.
pub const PASS: &str = "ask_attachment";

/// How many events may sit between the engine and its consumer before it
/// applies backpressure. Matches [`crate::ai::rag`]'s own buffer.
const EVENT_BUFFER: usize = 64;

/// How large one packed passage is, in bytes of document text.
///
/// Roughly a paragraph. Small enough that a citation points somewhere a
/// reader can find, large enough that a clause is whole inside one — the same
/// trade [`crate::index::chunk`] makes, at the smaller size a *citation* wants
/// rather than the larger one an embedding does.
const PASSAGE_BYTES: usize = 720;

/// How far before a located offset a passage starts, so it opens with lead-in
/// rather than mid-sentence at the match.
const PASSAGE_LEAD: usize = 160;

/// Most passages one attachment may contribute, however large
/// `ai.ask.max_chars_per_message` is.
///
/// One attachment must not be able to fill the whole context on its own: a
/// question over a result set is answered better by three documents seen
/// partially than by one seen thoroughly, and the single-attachment scope
/// already has no competition for the budget.
const MAX_PASSAGES_PER_ATTACHMENT: usize = 6;

/// How many of a question's own words one passage search looks for.
///
/// See [`needles`]: each is a full scan of a document, and the marginal value
/// of the ninth-longest word in a question is near zero.
const MAX_NEEDLES: usize = 8;

/// Hard ceiling on candidate attachments, whatever `top_k` asks for. Mirrors
/// [`crate::ai::rag::context`]'s own fetch cap: `ai.ask.top_k` is the real
/// bound, and this exists so a misconfiguration cannot turn one question into
/// an unbounded scan.
const MAX_CANDIDATES: usize = 50;

/// The system prompt. Frozen text: prompt caching is a byte-identical-prefix
/// match (see [`ChatRequest::system`]'s own docs), so only the per-question
/// user turn should vary between calls.
const SYSTEM_BASE: &str = "You answer questions about documents attached to a user's own email, \
     using only the numbered source passages you are given. \
     Cite every claim with the passage's bracketed label, exactly as given — write [2] inline, \
     immediately after the claim it supports. Never invent a label; only labels that appear in \
     the passages exist. \
     If the passages do not contain the answer, say plainly that the attachment text you were \
     shown does not say, and cite nothing. Never answer from general knowledge, and never guess \
     at a number, date, party, or amount that is not in a passage. \
     Be brief: a few sentences, no preamble, no restating the question.";

/// [`SYSTEM_BASE`] plus [`injection::DATA_BOUNDARY_CLAUSE`].
///
/// Built once into a `static`: the prompt cache is a byte-identical-prefix
/// match, so the clause is appended in exactly one place rather than per
/// request.
static SYSTEM: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| injection::with_data_boundary(SYSTEM_BASE));

/// A question to answer from attachment text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AskAttachmentRequest {
    /// The natural-language question.
    pub question: String,
    /// Scope the answer to one attachment. When set, `part_id` must be set
    /// too and no search is run.
    pub message_id: i64,
    /// The MIME part id of that attachment.
    pub part_id: String,
    /// Restrict a searched scope to one account; `0` means every account.
    /// Ignored when `message_id` names one attachment.
    pub account_id: i64,
    /// How many attachments a searched scope retrieves; `0` means
    /// `ai.ask.top_k`.
    pub top_k: u32,
}

impl AskAttachmentRequest {
    /// Whether this asks about exactly one, named attachment.
    #[must_use]
    pub fn is_single(&self) -> bool {
        self.message_id != 0 && !self.part_id.trim().is_empty()
    }
}

/// One bounded window of one attachment, as the answering model sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Passage {
    /// The message carrying the attachment.
    pub message_id: i64,
    /// `messages.uid`.
    pub message_uid: i64,
    /// Owning account.
    pub account_id: i64,
    /// Owning mailbox name, for display on a citation.
    pub mailbox: String,
    /// The MIME part id.
    pub part_id: String,
    /// The attachment's filename, empty when the part declared none.
    pub filename: String,
    /// The page this window falls on, when the format has pages.
    pub page: Option<i64>,
    /// Byte offset of the window in the attachment's extracted text.
    pub span_start: i64,
    /// Byte offset just past it.
    pub span_end: i64,
    /// The window's text — the only text a citation quote may be drawn from.
    pub text: String,
}

impl Passage {
    /// The labelled block this passage contributes to the prompt.
    ///
    /// `label` is 1-based and positional; the model never sees a row id, for
    /// the reason [`crate::ai::rag::cite`] documents at length. The label
    /// stays *outside* the fence — it is the one token in the document this
    /// engine authored, and [`resolve`] resolves answers against it, so
    /// putting it inside would let document text that reproduced the block
    /// shape appear to open a passage of its own.
    #[must_use]
    pub fn render(&self, label: usize) -> String {
        let mut inner = String::with_capacity(self.text.len() + 160);
        // The filename is sender-controlled — it arrives in a MIME header —
        // so it is neutralized exactly as the text is. A file called
        // `report [1].pdf` would otherwise mint a citation marker.
        if !self.filename.is_empty() {
            inner.push_str("File: ");
            inner.push_str(&neutralize_markers(&self.filename));
            inner.push('\n');
        }
        match self.page {
            Some(page) => {
                inner.push_str("Page: ");
                inner.push_str(&page.to_string());
                inner.push('\n');
            }
            None => {
                // No pages in this format. Said explicitly so the model does
                // not read the field's absence as an omission it should fill.
                inner.push_str("Page: not paginated\n");
            }
        }
        if !self.text.is_empty() {
            inner.push('\n');
            inner.push_str(&neutralize_markers(&self.text));
            inner.push('\n');
        }

        let mut out = String::with_capacity(inner.len() + 128);
        out.push_str(&format!("[{label}]\n"));
        out.push_str(&injection::untrusted_block(
            &format!("attachment-{label}"),
            &inner,
        ));
        out.push('\n');
        out
    }
}

/// What [`pack`] built, and an honest account of what it left out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Packed {
    /// Best-first, already policy-cleared and budget-bounded.
    pub passages: Vec<Passage>,
    /// How many attachments the scope produced.
    pub retrieved: usize,
    /// How many attachments contributed at least one passage.
    pub attachments: usize,
    /// How many were dropped because `ai.policy` does not let their folder
    /// reach a network provider.
    pub withheld_by_policy: usize,
    /// How many were dropped because the context budget was already full.
    pub dropped_for_budget: usize,
    /// Estimated tokens across [`Packed::passages`].
    pub context_tokens: usize,
}

/// One resolved citation: a passage the answer actually pointed at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentCitation {
    /// The 1-based label the answer used.
    pub label: u32,
    /// The message carrying the attachment.
    pub message_id: i64,
    /// `messages.uid`.
    pub message_uid: i64,
    /// Owning account.
    pub account_id: i64,
    /// Owning mailbox name.
    pub mailbox: String,
    /// The MIME part id.
    pub part_id: String,
    /// The attachment's filename, empty when it has none.
    pub filename: String,
    /// The page the cited passage came from, when the format has pages.
    pub page: Option<i64>,
    /// Byte offset of the cited passage in the attachment's extracted text —
    /// the "section" half of a page/section citation, and the half that
    /// exists for unpaginated formats.
    pub span_start: i64,
    /// Byte offset just past it.
    pub span_end: i64,
    /// A verbatim excerpt of the passage that was in the prompt, chosen for
    /// relevance to the question. Extracted locally, never taken from the
    /// model.
    pub quote: String,
}

/// What retrieval found, for a client's display and an operator's trace.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetrievalTrace {
    /// Attachments the scope produced.
    pub retrieved: usize,
    /// Attachments that contributed at least one passage.
    pub attachments: usize,
    /// Passages packed into the prompt.
    pub passages: usize,
    /// Attachments withheld by `ai.policy`.
    pub withheld_by_policy: usize,
    /// Attachments dropped because the context budget was full.
    pub dropped_for_budget: usize,
    /// Estimated tokens of context sent.
    pub context_tokens: usize,
    /// The model actually called, which a soft budget cap may have
    /// downgraded. Empty when no call was made.
    pub model: String,
}

/// Why an answer was not grounded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Nothing was retrieved, the named attachment has no extracted text, or
    /// `ai.policy` withheld everything. No provider call was made.
    NoContext,
    /// The model answered but cited no passage this daemon packed.
    Uncited,
}

impl Refusal {
    /// A one-line explanation for a user.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::NoContext => {
                "no extracted attachment text could be used to answer this — either nothing \
                 matched, the attachment has no text yet, or the AI policy withholds the folder \
                 it is in"
            }
            Self::Uncited => {
                "the answer cited no attachment passage, so it is not grounded in the documents \
                 you were shown"
            }
        }
    }
}

/// How an answer ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AskOutcome {
    /// Whether the answer cited at least one real passage. Set by this engine
    /// from the resolved citations — never by the model.
    pub grounded: bool,
    /// Why not, when `grounded` is `false`.
    pub refusal: Option<Refusal>,
    /// The model's own stop reason, when a call was made.
    pub stop_reason: Option<StopReason>,
}

/// One frame of an answer.
///
/// The order is fixed and total: exactly one [`AskEvent::Trace`], then zero or
/// more [`AskEvent::Token`]s, then zero or more [`AskEvent::Citation`]s, then
/// at most one [`AskEvent::Usage`], then exactly one [`AskEvent::Done`] —
/// unless the stream fails, in which case the error is terminal.
#[derive(Debug, Clone, PartialEq)]
pub enum AskEvent {
    /// What the scope found. Always first.
    Trace(RetrievalTrace),
    /// A slice of the answer, in arrival order.
    Token(String),
    /// A passage the answer cited. Emitted after the prose, because a
    /// citation is only resolvable once the whole answer has been seen.
    Citation(AttachmentCitation),
    /// Final token accounting for the call.
    Usage(Usage),
    /// How the answer ended. Always last.
    Done(AskOutcome),
}

/// A live answer.
pub type AskStream = Pin<Box<dyn Stream<Item = Result<AskEvent, Error>> + Send>>;

/// The ask-your-attachment engine.
///
/// Cheap to clone (a `Database` handle and `Arc`s), because [`Self::ask`]
/// drives the answer from a spawned task.
#[derive(Clone)]
pub struct AttachAskEngine {
    db: Database,
    provider: Arc<dyn Provider>,
    policy: Arc<PolicyEngine>,
    search: AttachmentSearch,
    privacy: AiPrivacy,
    limits: AiLimits,
    config: AiAsk,
    /// `ai.limits.max_concurrency`, **shared** with the daemon's own AI
    /// worker pool rather than a second semaphore — one process must not
    /// exceed one configured ceiling because it has four call sites.
    semaphore: Arc<Semaphore>,
    /// `ai.limits.requests_per_minute`, shared for the identical reason.
    rate_limiter: Arc<RateLimiter>,
}

impl std::fmt::Debug for AttachAskEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AttachAskEngine")
            .field("model", &self.config.model)
            .field("top_k", &self.config.top_k)
            .finish_non_exhaustive()
    }
}

impl AttachAskEngine {
    /// Build the engine over an already-constructed provider, policy engine
    /// and attachment search.
    ///
    /// Every dependency is injected for the reason
    /// [`crate::ai::rag::RagEngine::new`] documents: the daemon owns exactly
    /// one `Provider` and one `ai.limits` budget for the whole process, and a
    /// component that built its own would make both a fiction.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        db: Database,
        provider: Arc<dyn Provider>,
        policy: Arc<PolicyEngine>,
        search: AttachmentSearch,
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
            search,
            privacy,
            limits,
            config,
            semaphore,
            rate_limiter,
        }
    }

    /// Answer `req` from attachment text, streaming the result.
    ///
    /// Scope resolution, the policy gate and packing run before this returns,
    /// so a caller that cannot even build a context learns so from the
    /// stream's first frames rather than after an open stream stalls.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] for an empty question or a `message_id`
    /// with no `part_id`, and whatever search or the context read failed
    /// with. A context that is merely *empty* is not an error: it produces a
    /// refusal on the stream.
    #[tracing::instrument(
        skip(self, req, cancel),
        fields(
            message_id = req.message_id,
            single = req.is_single(),
            retrieved = tracing::field::Empty,
            passages = tracing::field::Empty,
            withheld = tracing::field::Empty,
        )
    )]
    pub async fn ask(
        &self,
        req: &AskAttachmentRequest,
        cancel: &CancellationToken,
    ) -> Result<AskStream, Error> {
        let question = req.question.trim().to_owned();
        if question.is_empty() {
            return Err(Error::invalid_argument("a question is required"));
        }
        if req.message_id != 0 && req.part_id.trim().is_empty() {
            return Err(Error::invalid_argument(
                "asking about one message needs the attachment's part id too; omit both to ask \
                 over search results",
            ));
        }
        if req.message_id == 0 && !req.part_id.trim().is_empty() {
            return Err(Error::invalid_argument(
                "a part id names an attachment of a message; give the message id as well",
            ));
        }

        let candidates = self.candidates(req, &question, cancel).await?;
        let packed = pack(
            &self.db,
            &candidates,
            &self.policy,
            &self.config,
            &question,
            self.privacy.max_body_chars as usize,
            cancel,
        )
        .await?;

        let span = tracing::Span::current();
        span.record("retrieved", packed.retrieved);
        span.record("passages", packed.passages.len());
        span.record("withheld", packed.withheld_by_policy);

        let mut trace = RetrievalTrace {
            retrieved: packed.retrieved,
            attachments: packed.attachments,
            passages: packed.passages.len(),
            withheld_by_policy: packed.withheld_by_policy,
            dropped_for_budget: packed.dropped_for_budget,
            context_tokens: packed.context_tokens,
            model: String::new(),
        };

        if packed.passages.is_empty() {
            // The refusal that never touches a provider. Both halves matter:
            // nothing matched, or everything that matched is `local_only`/
            // `forbidden` — and in the second case the whole point is that no
            // request is built at all.
            tracing::info!(
                retrieved = packed.retrieved,
                withheld = packed.withheld_by_policy,
                "ask-attachment has no usable context; refusing without calling the provider"
            );
            return Ok(refusal_stream(trace, Refusal::NoContext));
        }
        trace.model.clone_from(&self.config.model);

        let (tx, rx) = mpsc::channel(EVENT_BUFFER);
        let this = self.clone();
        let cancel = cancel.clone();
        let scoped_message = req.is_single().then_some(req.message_id);
        tokio::spawn(
            async move {
                this.run(question, packed, trace, scoped_message, cancel, tx)
                    .await;
            }
            .instrument(tracing::Span::current()),
        );
        Ok(Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)))
    }

    /// Which attachments this question is answered from.
    ///
    /// One named attachment, or the best `top_k` the attachment search
    /// returns for the question itself. The searched form reuses
    /// [`AttachmentSearch`] rather than a retrieval of its own, for the
    /// reason [`crate::ai::rag`] gives for injecting `SearchApi`: a second
    /// assembly would be a second search engine whose relevance could drift
    /// from the one `SearchAttachments` serves.
    async fn candidates(
        &self,
        req: &AskAttachmentRequest,
        question: &str,
        cancel: &CancellationToken,
    ) -> Result<Vec<Candidate>, Error> {
        if req.is_single() {
            return Ok(vec![Candidate {
                message_id: req.message_id,
                part_id: req.part_id.trim().to_owned(),
                span: None,
            }]);
        }
        let top_k = match req.top_k {
            0 => self.config.top_k as usize,
            n => n as usize,
        }
        .clamp(1, MAX_CANDIDATES);
        let hits = self
            .search
            .search(
                &AttachmentQuery {
                    query: question.to_owned(),
                    account_id: req.account_id,
                    message_id: 0,
                    limit: u32::try_from(top_k).unwrap_or(u32::MAX),
                },
                cancel,
            )
            .await?;
        Ok(hits
            .into_iter()
            .map(|hit| Candidate {
                message_id: hit.message_id,
                part_id: hit.part_id,
                // The offset search already resolved. Reusing it means the
                // passage the model reads opens where the evidence is,
                // rather than at whichever term this engine would have found
                // first — and for a semantic-only hit it is the *only* offset
                // anything knows.
                span: Some(hit.span_start),
            })
            .collect())
    }

    /// The half of an answer that can reach the network: pace, budget,
    /// redact, stream, cite, audit.
    async fn run(
        self,
        question: String,
        packed: Packed,
        mut trace: RetrievalTrace,
        scoped_message: Option<i64>,
        cancel: CancellationToken,
        tx: mpsc::Sender<Result<AskEvent, Error>>,
    ) {
        // Concurrency and pacing first, then the budget, then the call — the
        // order `crate::ai::queue` documents at length: a budget check taken
        // before an unbounded wait can be arbitrarily stale by the time the
        // call is made, and what bounds the overshoot is how many checks can
        // be outstanding at once.
        let _permit = {
            let semaphore = Arc::clone(&self.semaphore);
            tokio::select! {
                () = cancel.cancelled() => {
                    let _ = tx.send(Err(Error::cancelled(
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
                let _ = tx.send(Err(Error::cancelled(
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
            .user(prompt(&question, &packed.passages));

        // The firewall. Nothing between here and `provider.stream` may add
        // text to the request.
        let GuardedRequest::Redacted {
            request, tokens, ..
        } = guard(&request, &self.privacy)
        else {
            let _ = tx
                .send(Err(Error::failed_precondition(
                    "nothing was left to answer from once PII was redacted from the attachment \
                     text"
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
                    scoped_message,
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
                scoped_message,
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
                    // A terminal frame, not a clean end: returning silently
                    // closes the channel, which tonic turns into an `OK` with
                    // no `Done` — so a client keeps half an answer, sees
                    // success, and exits 0.
                    let _ = tx.try_send(Err(Error::cancelled(
                        "the answer was cancelled before it finished".to_owned(),
                    )));
                    self.audit_incomplete(&ctx, "cancelled").await;
                    return;
                }
                // Detected the instant the consumer goes away. Returning here
                // drops `stream`, which closes the provider's own channel,
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
                    ctx.scoped_message,
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
                // a tool-use block is not something a client has a frame for.
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
                        ctx.scoped_message,
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
        // firewall happened to tokenize still resolves.
        let rehydrated = rehydrate(answer, ctx.tokens);
        let (citations, dangling) = resolve(&rehydrated, &ctx.packed.passages, ctx.question);
        self.audit(
            ctx.model,
            ctx.scoped_message,
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
            passages = ctx.packed.passages.len(),
            "ask-attachment answered"
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
    /// [`Error::ResourceExhausted`] when either says no. There is no cheaper
    /// answer to degrade to: the whole RPC *is* the model call.
    async fn budgeted_model(&self) -> Result<String, Error> {
        let gate = CostGate {
            db: &self.db,
            limits: &self.limits,
        };
        match gate.decide().await? {
            CapDecision::Open => {}
            other => {
                return Err(Error::resource_exhausted(format!(
                    "the AI spend cap is closed ({other:?}); ask-attachment cannot run until it \
                     resets or an operator raises the cap"
                )));
            }
        }
        // Charged to the global budget rather than a per-account one, exactly
        // as `rag::RagEngine::budgeted_model` is: a searched scope spans every
        // configured account, so there is no single account the call is "for",
        // and splitting the attribution by scope would make the two shapes of
        // this one RPC bill differently. A user is waiting on it, so
        // `Interactive` is what the check uses and what the ledger records.
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
                    "ai budget soft cap: downgrading the ask-attachment model"
                );
                Ok(model)
            }
            // The detailed reason names aggregate spend figures, and this path
            // is reachable with `ai.invoke` while reading spend needs `admin`
            // — so the detail goes to the log and the caller is told only that
            // a cap was reached.
            BudgetVerdict::Block { reason, .. } => {
                tracing::info!(reason = %reason, "ai budget hard cap: refusing ask-attachment");
                Err(Error::resource_exhausted(
                    "an AI spend budget has been reached; ask-attachment cannot run until the \
                     window resets or an operator raises the budget"
                        .to_owned(),
                ))
            }
        }
    }

    /// One ledger row for a call that did not complete — a cancelled stream
    /// or a client that hung up. Recorded because the ledger is a record of
    /// what left the machine, and an aborted call still consumed one.
    async fn audit_incomplete(&self, ctx: &RelayContext<'_>, why: &str) {
        tracing::debug!(why, "ask-attachment stream ended early");
        self.audit(
            ctx.model,
            ctx.scoped_message,
            ctx.payload,
            ctx.redaction_level,
            ctx.started.elapsed(),
            None,
            CallOutcome::Error(format!("ask-attachment stream ended early: {why}")),
        )
        .await;
    }

    /// Write one ledger row. Never propagates: an audit write that fails must
    /// not turn a served answer into an error.
    #[allow(clippy::too_many_arguments)]
    async fn audit(
        &self,
        model: &str,
        message_id: Option<i64>,
        payload: &[u8],
        redaction_level: &str,
        latency: std::time::Duration,
        usage: Option<Usage>,
        outcome: CallOutcome,
    ) {
        let record = CallRecord {
            account_id: None,
            // Attributed to a message only when the caller named one. A
            // searched scope is a question about a *corpus*: recording it
            // against one of the attachments it happened to retrieve would
            // make `mail ai audit --message <id>` claim a call was made "for"
            // a message that merely appeared in somebody's context — the same
            // reasoning `ai::rag`'s own audit gives for never attributing.
            message_id,
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
            tracing::warn!(%error, "could not write the ask-attachment audit entry");
        }
    }
}

/// Everything [`AttachAskEngine::relay`]/[`AttachAskEngine::finish`] need that
/// does not change frame to frame.
struct RelayContext<'a> {
    question: &'a str,
    packed: &'a Packed,
    model: &'a str,
    scoped_message: Option<i64>,
    payload: &'a [u8],
    redaction_level: &'a str,
    tokens: &'a TokenMap,
    started: Instant,
}

/// One attachment the answer may draw on.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Candidate {
    message_id: i64,
    part_id: String,
    /// A byte offset search already resolved, when there was a search.
    span: Option<i64>,
}

/// One candidate's metadata — everything the policy gate needs, and nothing
/// from the document itself.
#[derive(Debug, Clone)]
struct CandidateMeta {
    message_id: i64,
    message_uid: i64,
    account_id: i64,
    account: String,
    mailbox: String,
    part_id: String,
    filename: String,
}

/// Build the context for `candidates` (best first), honoring the AI policy and
/// the configured budgets.
///
/// **The policy gate runs before any attachment text is read.** The metadata
/// pass and the text pass are two separate database reads with the gate
/// between them, so there is a point in this function at which "no document
/// text has been fetched" is a property of the program rather than a comment
/// — see the module docs.
///
/// # Errors
///
/// [`Error`] only for a failed database read. A cancelled read, a missing
/// attachment, and a fully-withheld candidate set are ordinary outcomes that
/// produce a smaller (possibly empty) [`Packed`] — the caller's cue to refuse,
/// not to fail.
async fn pack(
    db: &Database,
    candidates: &[Candidate],
    policy: &Arc<PolicyEngine>,
    config: &AiAsk,
    question: &str,
    max_body_chars: usize,
    cancel: &CancellationToken,
) -> Result<Packed, Error> {
    let mut packed = Packed {
        retrieved: candidates.len(),
        ..Packed::default()
    };
    if candidates.is_empty() {
        return Ok(packed);
    }
    let capped = candidates
        .get(..candidates.len().min(MAX_CANDIDATES))
        .unwrap_or(candidates);

    // Pass one: metadata only. Not one byte of any document is read here.
    let Some(meta) = fetch_meta(db, capped, cancel).await? else {
        // A cancelled read is not an empty context. Collapsing the two would
        // end the RPC `OK` carrying a refusal that says the documents do not
        // answer the question — which is false, and which the proto's own
        // cancellation contract forbids ("terminates with CANCELLED, never a
        // clean OK"). `AttachmentSearch` may report a cancelled scan as an
        // empty page because a superseded *search* has no answer to give; an
        // answer that was never assembled is a different thing.
        return Err(Error::cancelled(
            "the question was cancelled before its context could be read".to_owned(),
        ));
    };

    // The gate, before the text pass below exists to be reached.
    let mut cleared: Vec<(CandidateMeta, Option<i64>)> = Vec::with_capacity(meta.len());
    for candidate in capped {
        let Some(row) = meta
            .iter()
            .find(|row| row.message_id == candidate.message_id && row.part_id == candidate.part_id)
        else {
            // Extracted text went away between ranking and now, or the part
            // was never extracted. Not an error — the answer is grounded on
            // what is still there.
            continue;
        };
        let target = PolicyTarget::account(row.account.clone()).mailbox(row.mailbox.clone());
        let decision = policy.resolve(&target);
        if !decision.permits_network() {
            packed.withheld_by_policy += 1;
            tracing::debug!(
                message_id = row.message_id,
                part = %row.part_id,
                mode = ?decision.mode,
                "ai policy withholds this attachment from the ask-attachment context"
            );
            continue;
        }
        cleared.push((row.clone(), candidate.span));
    }
    if cleared.is_empty() {
        return Ok(packed);
    }

    // One attachment may contribute no more than `ai.ask.max_chars_per_message`
    // and never more than `ai.privacy.max_body_chars` — the operator's own
    // ceiling on what any single document may hand a provider, which this
    // path must not exceed just because it packs several at once.
    let per_attachment = (config.max_chars_per_message as usize).min(max_body_chars);
    let per_attachment_passages =
        (per_attachment / PASSAGE_BYTES).clamp(1, MAX_PASSAGES_PER_ATTACHMENT);
    let budget = config.max_context_tokens as usize;
    let terms = snippet::query_terms(question);
    let needles = needles(&terms);

    // Pass two: the text, for the candidates that survived the gate.
    let plan: Vec<(CandidateMeta, Option<i64>)> = cleared;
    let Some(windows) = fetch_windows(
        db,
        plan.clone(),
        needles,
        per_attachment_passages,
        per_attachment,
        cancel,
    )
    .await?
    else {
        return Err(Error::cancelled(
            "the question was cancelled before its context could be read".to_owned(),
        ));
    };

    let mut full = false;
    for (row, _) in &plan {
        let Some(found) = windows
            .iter()
            .find(|w| w.message_id == row.message_id && w.part_id == row.part_id)
        else {
            continue;
        };
        if full {
            packed.dropped_for_budget += 1;
            continue;
        }
        let mut contributed = false;
        for window in &found.windows {
            let passage = Passage {
                message_id: row.message_id,
                message_uid: row.message_uid,
                account_id: row.account_id,
                mailbox: row.mailbox.clone(),
                part_id: row.part_id.clone(),
                filename: row.filename.clone(),
                page: window.page,
                span_start: window.span_start,
                span_end: window.span_end,
                text: window.text.clone(),
            };
            let cost = estimate_tokens(&passage.render(packed.passages.len() + 1));
            // Packing stops at the first passage that would cross the budget
            // rather than skipping it for a shorter one further down: filling
            // the remainder with *less* relevant text purely because it is
            // shorter is a worse context than a smaller one built strictly
            // from the best matches. The same rule `ai::rag::context` applies.
            if !packed.passages.is_empty() && packed.context_tokens + cost > budget {
                full = true;
                break;
            }
            packed.context_tokens += cost;
            packed.passages.push(passage);
            contributed = true;
        }
        if contributed {
            packed.attachments += 1;
        } else if full {
            packed.dropped_for_budget += 1;
        }
    }

    Ok(packed)
}

/// The metadata behind `candidates` — everything the policy gate is keyed on,
/// and deliberately no document text.
async fn fetch_meta(
    db: &Database,
    candidates: &[Candidate],
    cancel: &CancellationToken,
) -> Result<Option<Vec<CandidateMeta>>, Error> {
    let wanted: Vec<(i64, String)> = candidates
        .iter()
        .map(|c| (c.message_id, c.part_id.clone()))
        .collect();
    let rows = interruptible_read(db, cancel, move |conn| {
        let mut out: Vec<CandidateMeta> = Vec::with_capacity(wanted.len());
        // Joined against `index_content` so a part with no extracted text is
        // absent rather than present-and-empty: an attachment nothing could
        // read is not a candidate, and treating it as one would make the
        // refusal say "the policy withheld it".
        let mut stmt = conn.prepare(
            "SELECT m.id, m.uid, m.account_id, acc.name, mb.name, a.filename
             FROM messages m
             JOIN index_content ic ON ic.message_id = m.id AND ic.part = ?2 AND ic.text <> ''
             LEFT JOIN accounts acc ON acc.id = m.account_id
             LEFT JOIN mailboxes mb ON mb.id = m.mailbox_id
             LEFT JOIN attachments a ON a.message_id = m.id AND a.part_id = ?3
             WHERE m.id = ?1",
        )?;
        for (message_id, part_id) in &wanted {
            let key = format!("attachment:{part_id}");
            let row = stmt
                .query_row(rusqlite::params![message_id, key, part_id], |row| {
                    Ok(CandidateMeta {
                        message_id: row.get(0)?,
                        message_uid: row.get(1)?,
                        account_id: row.get(2)?,
                        // An unresolvable account or mailbox name yields an
                        // empty one, which `PolicyEngine::resolve` answers for
                        // out of `ai.policy`'s defaults like any other unnamed
                        // target — the same treatment `ai::rag::context` gives.
                        account: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                        mailbox: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                        part_id: part_id.clone(),
                        filename: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
                    })
                })
                .optional()?;
            if let Some(row) = row {
                out.push(row);
            }
        }
        Ok(out)
    })
    .await?;
    // `None` means the scan was cancelled, which the caller must not read as
    // an empty context — see [`pack`]'s own branch.
    Ok(rows)
}

/// One attachment's packed windows.
struct Windows {
    message_id: i64,
    part_id: String,
    windows: Vec<Window>,
}

/// One bounded window of document text.
struct Window {
    page: Option<i64>,
    span_start: i64,
    span_end: i64,
    text: String,
}

/// Read the passages for every cleared candidate.
///
/// Reached only for candidates the policy gate already cleared — that is the
/// whole reason this is a separate read rather than more columns on
/// [`fetch_meta`]'s query.
async fn fetch_windows(
    db: &Database,
    plan: Vec<(CandidateMeta, Option<i64>)>,
    needles: Vec<String>,
    passages: usize,
    max_chars: usize,
    cancel: &CancellationToken,
) -> Result<Option<Vec<Windows>>, Error> {
    let rows = interruptible_read(db, cancel, move |conn| {
        let mut out: Vec<Windows> = Vec::with_capacity(plan.len());
        for (row, hint) in &plan {
            let offsets = passage_offsets(conn, row, *hint, &needles, passages)?;
            let mut windows = Vec::with_capacity(offsets.len());
            for at in offsets {
                // Clipped to the page the *evidence* is on, not merely started
                // near it. A passage that straddles a page boundary can only
                // be attributed to one of them, and whichever is chosen the
                // citation is then wrong about text on the other — which was
                // not hypothetical: a three-page contract whose whole text fit
                // inside one window cited page 1 for a clause on page 2, and
                // the citation looked entirely plausible. For a format with no
                // pages there is one span covering the text, so this clamps to
                // nothing and the window is whatever fits.
                let (page, lower, upper) =
                    match page_span_at(conn, row.message_id, &row.part_id, at)? {
                        Some((page, lower, upper)) => (Some(page), lower, upper),
                        None => (None, 0, i64::MAX),
                    };
                let start = at.saturating_sub(PASSAGE_LEAD as i64).max(lower);
                // `max_chars` is `min(ai.ask.max_chars_per_message,
                // ai.privacy.max_body_chars)`, and it bounds a passage's own
                // length as well as how many are packed. Without it an
                // operator who set `max_body_chars = 500` still got a
                // 720-byte passage: the ceiling decided the passage *count*
                // (which clamps to at least one) and nothing capped the one
                // that was left.
                let length = (PASSAGE_BYTES as i64)
                    .min(max_chars as i64)
                    .min(upper.saturating_sub(start))
                    .max(1);
                let Some((skipped, text)) =
                    read_window(conn, row.message_id, &row.part_id, start, length)?
                else {
                    continue;
                };
                if text.trim().is_empty() {
                    continue;
                }
                // `skipped` is what the decoder trimmed off the front to reach
                // a character boundary. Without adding it back the citation
                // reports a span shifted by up to three bytes at *both* ends,
                // which is not cosmetic: `span_start` is then not a character
                // boundary, and a client slicing the stored text by it panics.
                // The span is the half of a page/section citation that is
                // supposed to be checkable.
                let start = start + skipped as i64;
                let span_end = start + text.len() as i64;
                windows.push(Window {
                    page,
                    span_start: start,
                    span_end,
                    text,
                });
            }
            out.push(Windows {
                message_id: row.message_id,
                part_id: row.part_id.clone(),
                windows,
            });
        }
        Ok(out)
    })
    .await?;
    Ok(rows)
}

/// Where this attachment's evidence is, in document order.
///
/// Offsets of the *evidence*, not of the windows around it: the caller turns
/// each into a window clipped to the page the offset falls on, and doing the
/// lead-back here would move an offset near the top of a page onto the one
/// before it.
///
/// The search's own offset first when there was a search, then successive
/// occurrences of the question's words, and finally the document's opening —
/// which is what a purely semantic match, or a question sharing no word with
/// the text, honestly has to offer.
fn passage_offsets(
    conn: &Connection,
    row: &CandidateMeta,
    hint: Option<i64>,
    needles: &[String],
    limit: usize,
) -> rusqlite::Result<Vec<i64>> {
    let mut starts: Vec<i64> = Vec::with_capacity(limit);
    let push = |at: i64, starts: &mut Vec<i64>| {
        // Overlapping windows are the same passage twice: the model pays for
        // both and a citation could name either.
        if starts
            .iter()
            .any(|seen| (seen - at).abs() < PASSAGE_BYTES as i64)
        {
            return;
        }
        starts.push(at);
    };

    if let Some(at) = hint {
        push(at, &mut starts);
    }
    let mut from = 0i64;
    'outer: for needle in needles {
        loop {
            if starts.len() >= limit {
                break 'outer;
            }
            let Some(at) = locate_from(conn, row.message_id, &row.part_id, needle, from)? else {
                break;
            };
            push(at, &mut starts);
            from = at + needle.len() as i64;
        }
        from = 0;
    }
    if starts.is_empty() {
        starts.push(0);
    }
    starts.truncate(limit);
    starts.sort_unstable();
    Ok(starts)
}

/// The byte offset of `needle` at or after `from`, or `None`.
///
/// `instr` over two **blobs** counts bytes rather than characters, and
/// SQLite's built-in `lower()` folds only ASCII `A-Z` — a 1:1 byte mapping —
/// so an offset found in the folded text is valid in the original. Searching
/// in SQLite rather than here is what keeps a two-megabyte contract out of
/// this process; see [`super::search`]'s own note.
fn locate_from(
    conn: &Connection,
    message_id: i64,
    part_id: &str,
    needle: &str,
    from: i64,
) -> rusqlite::Result<Option<i64>> {
    let key = format!("attachment:{part_id}");
    let lowered = needle.to_lowercase();
    let at: Option<i64> = conn
        .prepare_cached(
            "SELECT instr(
                        substr(CAST(lower(text) AS BLOB), ?3),
                        CAST(?4 AS BLOB))
             FROM index_content WHERE message_id = ?1 AND part = ?2",
        )?
        .query_row(
            // `substr` and `instr` are both 1-based.
            rusqlite::params![message_id, key, from.max(0) + 1, lowered],
            |row| row.get(0),
        )
        .optional()?;
    Ok(at
        .filter(|found| *found > 0)
        .map(|found| from.max(0) + found - 1))
}

/// A `length`-byte window of an attachment's text starting at `start`, plus
/// how many leading bytes had to be trimmed to reach a character boundary —
/// which the caller adds back, because a citation's span has to describe the
/// text it actually carries.
///
/// Read as a bounded blob: `substr` over a BLOB counts bytes, the unit every
/// offset here is in, while `substr` over TEXT counts characters and would
/// drift from the spans the moment a document contained one non-ASCII
/// character.
fn read_window(
    conn: &Connection,
    message_id: i64,
    part_id: &str,
    start: i64,
    length: i64,
) -> rusqlite::Result<Option<(usize, String)>> {
    let key = format!("attachment:{part_id}");
    let bytes: Option<Vec<u8>> = conn
        .prepare_cached(
            "SELECT substr(CAST(text AS BLOB), ?3, ?4)
             FROM index_content WHERE message_id = ?1 AND part = ?2",
        )?
        .query_row(
            rusqlite::params![message_id, key, start + 1, length],
            |row| row.get(0),
        )
        .optional()?;
    Ok(bytes.map(|bytes| {
        let (skipped, text) = super::search::decode_window(&bytes);
        (skipped, text.to_owned())
    }))
}

/// The page a byte offset falls on, and that page's own byte span.
///
/// [`super::search::page_at`] answers the first half; a passage needs the
/// second as well, because the span is what it is clipped to. `None` for a
/// format with no pages at all, which the caller reads as "no clipping to
/// do" rather than as "page unknown".
fn page_span_at(
    conn: &Connection,
    message_id: i64,
    part_id: &str,
    offset: i64,
) -> rusqlite::Result<Option<(i64, i64, i64)>> {
    conn.prepare_cached(
        "SELECT page, span_start, span_end FROM attachment_pages
         WHERE message_id = ?1 AND part_id = ?2
           AND span_start <= ?3 AND span_end > ?3
         ORDER BY page LIMIT 1",
    )?
    .query_row(rusqlite::params![message_id, part_id, offset], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?))
    })
    .optional()
}

/// The literal strings a passage offset is looked for, longest first — a
/// phrase is a more precise statement about where the answer is than any of
/// the words in it.
fn needles(terms: &snippet::QueryTerms) -> Vec<String> {
    let mut out: Vec<String> = terms
        .phrases
        .iter()
        .chain(terms.terms.iter())
        .filter(|text| !text.trim().is_empty())
        .cloned()
        .collect();
    out.sort_by(|a, b| b.len().cmp(&a.len()).then_with(|| a.cmp(b)));
    out.dedup();
    // Bounded, because each one costs SQLite a full lowered copy of a
    // document that reaches two megabytes, once per candidate: a hundred-word
    // question over fifty attachments would be five thousand of those inside
    // one blocking closure holding a pooled read connection — and packing runs
    // *before* the AI concurrency semaphore, so nothing else caps it. Longest
    // first, so what is dropped is the least specific evidence.
    out.truncate(MAX_NEEDLES);
    out
}

/// Citations for `answer`, in the order the answer first names them, plus how
/// many markers named nothing.
///
/// The identical discipline [`crate::ai::rag::cite::resolve`] applies, over
/// passages instead of messages: a label outside `1..=passages.len()` resolves
/// to nothing and is dropped, so no value the model can emit produces a
/// citation naming an attachment this daemon did not pack. Repeated markers
/// collapse to one citation.
#[must_use]
pub fn resolve(
    answer: &str,
    passages: &[Passage],
    question: &str,
) -> (Vec<AttachmentCitation>, usize) {
    let terms = snippet::query_terms(question);
    let mut seen: BTreeSet<usize> = BTreeSet::new();
    let mut out = Vec::new();
    let mut dangling = 0usize;

    for label in markers(answer) {
        let Some(index) = label.checked_sub(1) else {
            // `[0]` — not a label this prompt ever offered.
            dangling += 1;
            continue;
        };
        let Some(passage) = passages.get(index) else {
            dangling += 1;
            continue;
        };
        if !seen.insert(index) {
            continue;
        }
        out.push(AttachmentCitation {
            label: u32::try_from(label).unwrap_or(u32::MAX),
            message_id: passage.message_id,
            message_uid: passage.message_uid,
            account_id: passage.account_id,
            mailbox: passage.mailbox.clone(),
            part_id: passage.part_id.clone(),
            filename: passage.filename.clone(),
            page: passage.page,
            span_start: passage.span_start,
            span_end: passage.span_end,
            quote: quote_for(passage, &terms),
        });
    }
    if dangling > 0 {
        tracing::warn!(
            dangling,
            passages = passages.len(),
            "the answer cited labels this question never offered; those citations were dropped"
        );
    }
    (out, dangling)
}

/// Every `[n]` (and `[n, m]`) label in `text`, in order of appearance.
///
/// Duplicated from [`crate::ai::rag::cite`]'s own private scanner rather than
/// exported from it: it is twenty lines, and making one module's answer-
/// scanning an item of another's public surface to save them would be the
/// worse trade — the same call `fuse::source_ordinal` and `present::snippet`
/// already make in this crate. `the_two_marker_scanners_agree` pins the two
/// against each other so a fix to either is a failing test until it is a fix
/// to both.
fn markers(text: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'[' {
            i += 1;
            continue;
        }
        let Some(close) = bytes[i + 1..].iter().position(|b| *b == b']') else {
            break;
        };
        let inner = text.get(i + 1..i + 1 + close).unwrap_or_default();
        // A bracketed run has to be entirely digits, commas and spaces to be
        // a citation; `[see clause 4]` and `[2024-01-02]` are prose.
        if !inner.is_empty()
            && inner
                .bytes()
                .all(|b| b.is_ascii_digit() || b == b',' || b == b' ')
        {
            for part in inner.split(',') {
                if let Ok(label) = part.trim().parse::<usize>() {
                    out.push(label);
                }
            }
        }
        i += close + 2;
    }
    out
}

/// The supporting quote for `passage`: the question-relevant window of the
/// text that was in the prompt, or its leading excerpt when the question's
/// terms appear nowhere in it.
///
/// Extracted from [`Passage::text`] — the exact bytes that were packed —
/// never taken from the model. Asking the model for the quote would
/// reintroduce exactly the fabrication the labelling scheme removes.
fn quote_for(passage: &Passage, terms: &snippet::QueryTerms) -> String {
    if passage.text.trim().is_empty() {
        return String::new();
    }
    snippet::extract(&passage.text, &terms.terms, &terms.phrases)
        .unwrap_or_else(|| snippet::plain_excerpt(&passage.text))
        .text
}

/// The user turn: the question, then one labelled block per passage.
fn prompt(question: &str, passages: &[Passage]) -> String {
    let mut out = String::with_capacity(passages.len() * 1_024);
    out.push_str("Question: ");
    out.push_str(question);
    out.push_str("\n\nAttachment passages:\n\n");
    for (index, passage) in passages.iter().enumerate() {
        out.push_str(&passage.render(index + 1));
        out.push('\n');
    }
    out.push_str(
        "Answer the question using only the passages above, citing each claim with its bracketed \
         label.\n",
    );
    out
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
/// [`ControlFlow::Break`] means "stop". A break *caused by cancellation*
/// leaves a terminal error behind, so a caller that stops here has still told
/// the client the answer is incomplete — without it, a cancelled call ends
/// `OK` with no `Done` frame and a partial answer reads as a whole one.
async fn send(
    tx: &mpsc::Sender<Result<AskEvent, Error>>,
    cancel: &CancellationToken,
    event: Result<AskEvent, Error>,
) -> ControlFlow<()> {
    if cancel.is_cancelled() {
        terminate_cancelled(tx);
        return ControlFlow::Break(());
    }
    tokio::select! {
        () = cancel.cancelled() => {
            terminate_cancelled(tx);
            ControlFlow::Break(())
        }
        sent = tx.send(event) => match sent {
            Ok(()) => ControlFlow::Continue(()),
            Err(_) => ControlFlow::Break(()),
        },
    }
}

/// Best-effort terminal frame for an answer the daemon cut short.
fn terminate_cancelled(tx: &mpsc::Sender<Result<AskEvent, Error>>) {
    let _ = tx.try_send(Err(Error::cancelled(
        "the answer was cancelled before it finished".to_owned(),
    )));
}
