//! The `SearchService` gRPC implementation: the wiring that finally makes
//! the search pipeline (tasks 26-32, 51) reachable — `query::QueryPlanner` ->
//! `retrieve::Fanout` -> `fuse::Fuser` -> `features::FeatureExtractor` ->
//! `rank::l1::L1Ranker` -> `rank::l2::L2Stage` -> `present::Presenter`,
//! streamed back as [`SearchHit`](rmail_proto::v1::SearchHit)s.
//!
//! # Stage 5 sits between ranking and presentation, and cannot fail
//!
//! `rank::l2::L2Stage::rerank` returns a ranking, never a `Result`: an
//! unprovisioned cross-encoder, a provider outage, an exhausted AI budget or
//! a blown deadline all return the L1 order unchanged (see that module's own
//! docs). Nothing in this file therefore branches on whether a rerank
//! succeeded — there is one code path through ranking whether or not this
//! daemon can rerank at all.
//!
//! Two consequences are load-bearing here. First, `SearchRequest.rerank`
//! overrides `search.rerank` per request and `SearchRequest.deep` is what
//! `auto` resolves against, so both are decoded into a request-scoped
//! `L2Stage` rather than read from config at the call site. Second, a
//! reranked hit's `score` is a *permuted* L1 score (the mechanism `rank::l2`
//! uses to make a new order survive `Presenter`'s own score sort), so
//! `RankExplanation.score` is recomputed from the candidate's own feature
//! vector — otherwise the wire invariant "summing every contribution
//! reproduces `score`" would hold only when no rerank ran.
//!
//! # Streaming the first hit before the rest is computed
//!
//! prd.md's Stage 6 budget is explicit: "the top result paints in <30 ms
//! even while lower ranks are still being reranked." [`Presenter::present`]
//! itself has no incremental/streaming API — it is one batched call that
//! diversifies, snippets, and annotates its whole input in one pass — so
//! [`SearchApi::run_stream`] gets the same effect a different way: it calls
//! `present` **twice**. First over just `ranked[..1]` (the single
//! best-scoring candidate), which is cheap — one message's worth of
//! metadata fetch and snippet extraction instead of up to `limit`'s — and
//! flushed to the client the moment it is ready. Then over the full `ranked`
//! slice for the rest of the page, filtered to skip whatever the first call
//! already sent.
//!
//! This is safe for every intent, not just navigational, because of an
//! invariant [`present::mmr`] documents on itself: "the first pick is always
//! the single most relevant candidate" — with nothing yet selected, MMR's
//! `max_similarity_to_already_picked` term is `0.0` for every candidate
//! regardless of `λ`, so the very first slot is decided by relevance alone,
//! identical to what a length-1 input would also produce. The two `present`
//! calls therefore always agree on which message is "first"; the second
//! call's own per-item work (snippet extraction, thread/dup annotation) is
//! independent of how many other candidates are in the batch, so the
//! duplicate-but-skipped item costs a little redundant work, never a
//! different answer.
//!
//! # Cancellation: a generation-token slot, not a per-connection one
//!
//! prd.md calls for "a query-generation token to cancel superseded scans"
//! and, in the TUI, for "each keystroke cancels the prior in-flight
//! ranking." [`Generation`] is exactly that: a single shared "currently
//! streaming" slot — not a map keyed by client/connection — because this
//! daemon serves one interactive search session at a time from the CLI/
//! TUI's perspective; the newest `Search`/`Semantic` call is always the one
//! worth finishing, and an older one still in flight is, by construction,
//! stale. `Search` and `Semantic` share the slot (both are "the current
//! query"); `Explain` does not participate — it is a one-shot lookup for a
//! specific message a client already knows about, not a session-shaped
//! stream, so it neither cancels nor is cancelled by a live search.
//!
//! Every pipeline stage between `Generation::begin`'s token and the final
//! `send` already honors cancellation *for real*: `Fanout`'s retrievers,
//! `Fuser::fetch_meta`, `FeatureExtractor`'s batched fetches, and
//! `Presenter`'s own metadata/snippet fetches all route their SQLite reads
//! through `retrieve::cancel::interruptible_read`, which turns the token
//! into an actual `sqlite3_interrupt()` call on the in-flight statement (see
//! that module's own docs) — a superseded scan stops, it does not merely
//! get its output discarded while it keeps consuming a blocking-pool thread
//! and a read-pool connection underneath a caller that has already moved
//! on. This file's own job is only to *create* that token per stream and
//! cancel the previous one; the interruption itself is inherited for free
//! from tasks 27-32's own plumbing.
//!
//! # Feedback: impressions are written here, actions arrive by RPC
//!
//! prd.md's learning loop (task 64) needs two things from a search, and only
//! one of them is something a client can supply.
//!
//! The *impression* — which message was shown at which position, and the
//! exact feature vector it was ranked by — is only knowable in this process,
//! at this moment. `features::FeatureExtractor` reads the live corpus, so a
//! vector re-derived later is a different vector: BM25 moves as the index
//! grows, `recency_decay` moves by definition, and `is_unread` flips the
//! instant the user opens the result, which is precisely the row a trainer
//! cares most about. So [`SearchApi::run_stream`] carries the
//! `CandidateFeatures` it *actually ranked with* into a
//! [`rmail_core::feedback::Impression`] and writes it here.
//!
//! The *action* — opened, replied, archived, dwelled, scrolled past — is only
//! knowable at the client, and arrives through `LogFeedback` keyed by the
//! `query_id` stamped on every `SearchHit`.
//!
//! Two properties this file is responsible for, both structural:
//!
//! - **Logging never delays a search.** The `query_id` is minted in-process
//!   (`feedback::new_query_id`), so nothing is written before the first hit
//!   streams. `run_stream` *returns* what it wants logged rather than writing
//!   it, and [`SearchApi::start_stream`]'s spawned task writes it only after
//!   `run_stream` has returned — which is after the response channel has been
//!   dropped and the client has already seen end-of-stream. A slow writer
//!   connection therefore delays nothing a user can perceive.
//! - **Logging never fails a search.** Every failure is a `warn`, never a
//!   `Status`. The page was already served; a lost log line costs one
//!   training example.
//!
//! A superseded stream logs nothing at all. That is a data-quality decision
//! as much as a correctness one: the impressions of a query the user replaced
//! mid-keystroke are results nobody looked at, and feeding them to a
//! position-bias model as "shown but not clicked" is teaching it from noise.
//!
//! # `Explain` re-derives, it does not replay
//!
//! `Explain` re-runs query planning -> fan-out -> fuse -> feature
//! extraction for `ExplainRequest.query`, then looks the requested
//! `message_id` up directly in the *feature* list — not in a ranked/
//! presented page. This matters: `rank::Ranker::rank`'s `top_k` cut (Stage
//! 4) and `Presenter::present`'s `limit` cut (Stage 6) both intentionally
//! drop candidates that did not make the page, but Stage 3's feature
//! extraction runs over *every* fused candidate — so a message a client
//! already has (from a previous page, or named directly) can still be
//! explained even when it would not have made this request's own
//! `top_k_rerank`/`limit`. The reported score comes from
//! `L1Ranker::score` — the identical pure function `Ranker::rank` uses
//! internally — applied directly to that one feature vector, and
//! `L1Ranker::contributions` decomposes the identical computation into
//! per-feature terms that are guaranteed (see `rank::l1`'s own tests) to
//! sum back to it.

#![allow(clippy::result_large_err)] // see mail_service.rs's identical note on `Result<_, Status>`

use std::collections::{BTreeMap, BTreeSet};
use std::ops::ControlFlow;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};

use chrono::Utc;
use rmail_core::ai::AskRetriever;
use rmail_core::attach::search::{AttachmentQuery, AttachmentSearch};
use rmail_core::config::{IndexSemanticConfig, Rerank, RetrieversConfig, SearchConfig, SearchMode};
use rmail_core::embed::Embedder;
use rmail_core::eval::{
    Evaluator, GoldenQuery as CoreGoldenQuery, GoldenSet, JudgedMessage, Metrics as CoreMetrics,
    RankedSearch, RECALL_K,
};
use rmail_core::features::{CandidateFeatures, FeatureExtractor};
use rmail_core::feedback::{
    Action as FeedbackActionRecord, ActionKind, FeedbackStore, Impression, QueryRecord,
};
use rmail_core::fuse::{FusedCandidate, Fuser};
use rmail_core::index::entities::{self, EntityHit, EntityKind, EntityQuery};
use rmail_core::index::fts::FtsIndex;
use rmail_core::index::semantic::SemanticIndex;
use rmail_core::present::{PresentedResult, Presenter};
use rmail_core::query::{Intent, QueryCompiler, QueryPlan, QueryPlanner};
use rmail_core::rank::l1::{L1Ranker, Weights};
use rmail_core::rank::l2::{L2Stage, SearchKind};
use rmail_core::rank::Ranker;
use rmail_core::retrieve::{Candidate, DenseRetriever, Fanout, Source};
use rmail_core::{present, repo, Database, Error as RmailError};
use rmail_proto::v1::search_service_server::SearchService;
use rmail_proto::v1::{
    ByteRange as ProtoByteRange, CompileQueryRequest, EntityHit as ProtoEntityHit,
    EntityMention as ProtoEntityMention, EvalMetrics as ProtoEvalMetrics,
    EvalReport as ProtoEvalReport, EvaluateRequest, ExplainRequest,
    FeatureContribution as ProtoFeatureContribution, FeedbackAction as ProtoFeedbackAction,
    FeedbackRequest, Intent as ProtoIntent, Message as ProtoMessage, Mode as ProtoMode,
    QueryEval as ProtoQueryEval, QueryPlan as ProtoQueryPlan,
    RankExplanation as ProtoRankExplanation, Rerank as ProtoRerank,
    ResultAction as ProtoResultAction, SearchAttachmentsRequest, SearchAttachmentsResponse,
    SearchEntitiesRequest, SearchEntitiesResponse, SearchHit as ProtoSearchHit, SearchRequest,
    Snippet as ProtoSnippet,
};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};
use tracing::Instrument;

/// How many hits may sit between the pipeline and a client before `Search`/
/// `Semantic` applies backpressure. Search pages are bounded well under this
/// (`search.default_limit`/`SearchRequest.limit`, capped in practice by
/// `top_k_rerank`), so this is generous headroom, not a tuned value — see
/// `mail_service::STREAM_BUFFER`'s identical reasoning.
const STREAM_BUFFER: usize = 64;

/// The single "currently streaming" `Search`/`Semantic` slot — see the
/// module docs' "Cancellation" section for why this is one shared slot
/// rather than a per-connection map.
#[derive(Clone, Default)]
struct Generation {
    current: Arc<Mutex<Option<CancellationToken>>>,
}

impl Generation {
    /// Register a new stream as "current," cancelling whichever stream held
    /// the slot before it. Returns the token this stream's own pipeline work
    /// must honor — a child of `shutdown`, so daemon shutdown cancels it too
    /// even if no later request ever supersedes it.
    fn begin(&self, shutdown: &CancellationToken) -> CancellationToken {
        let token = shutdown.child_token();
        let previous = {
            let mut guard = self.current.lock().unwrap_or_else(PoisonError::into_inner);
            guard.replace(token.clone())
        };
        if let Some(previous) = previous {
            previous.cancel();
        }
        token
    }
}

/// One served page's worth of feedback, handed from
/// [`SearchApi::run_stream`] to [`SearchApi::log_page`] after the response
/// stream has closed.
///
/// Built only when `search.learning` is on — the opt-out is the absence of
/// this value, not a flag inside it, so a code path that forgets to check has
/// nothing to write in the first place.
struct LoggedPage {
    record: QueryRecord,
    impressions: Vec<Impression>,
}

/// Append one successfully-sent hit to the page's impressions.
///
/// The feature vector is looked up from the exact `CandidateFeatures` slice
/// this request's ranker scored — never re-extracted — which is the whole
/// point of logging here rather than deriving it later; see the module docs'
/// "Feedback" section.
///
/// `position` is the caller's own count of hits put on the wire, **not**
/// `page.impressions.len() + 1`. The two agree today, and would diverge the
/// moment the vector lookup below misses: every impression after the miss
/// would be logged one rank too high, silently corrupting exactly the
/// position-bias signal this table exists to capture. Deriving the rank from
/// what was *sent* rather than from what was *recorded* makes that class of
/// drift impossible rather than merely unlikely.
///
/// A hit with no matching vector is skipped rather than logged with a
/// placeholder: every presented result came from a ranked candidate, so this
/// cannot happen, and a zero vector would be indistinguishable from a
/// genuinely unremarkable one to whatever trains on it.
fn record_impression(
    page: Option<&mut LoggedPage>,
    vectors: &BTreeMap<i64, &CandidateFeatures>,
    message_id: i64,
    position: u32,
    score: f64,
) {
    let Some(page) = page else {
        return;
    };
    let Some(cf) = vectors.get(&message_id) else {
        tracing::warn!(
            message_id,
            position,
            "a presented hit had no feature vector; impression not logged"
        );
        return;
    };
    page.impressions.push(Impression {
        message_id,
        position,
        features: cf.features.clone(),
        l1_score: score,
        // Stage 5 (task 51) has not landed; `None` says "no reranker ran",
        // which is not the same fact as a rerank score of zero.
        l2_score: None,
    });
}

/// Whether a stream that got this far should log what it collected.
///
/// Both terms matter, and the first is the one that would be easy to lose:
///
/// - **`!cancelled`** — a superseded stream logs nothing *even when hits were
///   already sent*. The query was replaced by a fresher keystroke (or the
///   daemon is stopping), so those results are ones nobody looked at, and
///   feeding them to a position-bias model as "shown but not clicked" is
///   training on noise. Less data beats wrong data.
/// - **`impressions > 0`** — nothing was shown, so there is nothing to learn
///   from, and a bare `search_log` row would be a record of what the user
///   searched for and nothing else.
///
/// Split out from [`finish_page`] as a plain predicate so both terms are
/// testable without constructing a whole 34-feature page: the end-to-end
/// consequence is timing-dependent (a superseded stream may be cut before it
/// sends anything at all, in which case the second term already covers it),
/// which is exactly the shape of test that passes whether or not the rule
/// survives.
const fn should_log(cancelled: bool, impressions: usize) -> bool {
    !cancelled && impressions > 0
}

/// How a streamed search ended, and what it left to log.
///
/// `cancelled` is recorded at each exit rather than re-read from the token
/// afterwards — see [`SearchApi::run_stream`] for the race that makes the
/// difference visible to a client.
struct StreamOutcome {
    logged: Option<LoggedPage>,
    cancelled: bool,
}

impl StreamOutcome {
    /// Ran to the end of the page.
    const fn completed(logged: Option<LoggedPage>) -> Self {
        Self {
            logged,
            cancelled: false,
        }
    }

    /// An error frame is already on the wire; it is the terminal status.
    const fn failed() -> Self {
        Self {
            logged: None,
            cancelled: false,
        }
    }

    /// Stopped early — cancelled, or the consumer went away. Only the first
    /// warrants a terminal frame, and only the token knows which, *here*.
    fn stopped(cancel: &CancellationToken) -> Self {
        Self::stopped_with(None, cancel)
    }

    fn stopped_with(logged: Option<LoggedPage>, cancel: &CancellationToken) -> Self {
        Self {
            logged,
            cancelled: cancel.is_cancelled(),
        }
    }
}

/// The page to log, or `None` if there is nothing worth logging — see
/// [`should_log`] for the rule and why it lives there.
fn finish_page(page: Option<LoggedPage>, cancel: &CancellationToken) -> Option<LoggedPage> {
    let cancelled = cancel.is_cancelled();
    page.filter(|page| should_log(cancelled, page.impressions.len()))
}

/// Which candidate sources a request restricts itself to, resolved from
/// [`SearchRequest::mode`] (falling back to `search.default_mode` when
/// unspecified). `Semantic` (the dense-only RPC) never goes through this —
/// it always uses [`SearchApi::dense_only_candidates`] directly, regardless
/// of what `mode` a caller happened to set.
#[derive(Clone, Copy)]
enum WireMode {
    Lexical,
    Semantic,
    Hybrid,
}

fn resolve_mode(raw: i32, default_mode: SearchMode) -> WireMode {
    match ProtoMode::try_from(raw).unwrap_or(ProtoMode::Unspecified) {
        ProtoMode::Unspecified => match default_mode {
            SearchMode::Lexical => WireMode::Lexical,
            SearchMode::Semantic => WireMode::Semantic,
            SearchMode::Hybrid => WireMode::Hybrid,
        },
        ProtoMode::Lexical => WireMode::Lexical,
        ProtoMode::Semantic => WireMode::Semantic,
        ProtoMode::Hybrid => WireMode::Hybrid,
    }
}

/// `None` means "let `search.rerank` stand" — [`ProtoRerank::Unspecified`]
/// is the wire default, not a real override.
fn decode_rerank(raw: i32) -> Option<Rerank> {
    match ProtoRerank::try_from(raw).unwrap_or(ProtoRerank::Unspecified) {
        ProtoRerank::Unspecified => None,
        ProtoRerank::Off => Some(Rerank::Off),
        ProtoRerank::CrossEncoder => Some(Rerank::CrossEncoder),
        ProtoRerank::Claude => Some(Rerank::Claude),
        ProtoRerank::Auto => Some(Rerank::Auto),
    }
}

/// `None` means "let the query planner's own classification stand" —
/// [`ProtoIntent::Unspecified`] is the wire default, not a real override.
fn decode_intent(raw: i32) -> Option<Intent> {
    match ProtoIntent::try_from(raw).unwrap_or(ProtoIntent::Unspecified) {
        ProtoIntent::Unspecified => None,
        ProtoIntent::Navigational => Some(Intent::Navigational),
        ProtoIntent::Exploratory => Some(Intent::Exploratory),
        ProtoIntent::Lookup => Some(Intent::Lookup),
    }
}

/// The wire form of a classified intent. The inverse of [`decode_intent`]'s
/// non-`Unspecified` arms; never emits `Unspecified`, because a *compiled*
/// plan always carries a classification.
fn to_proto_intent(intent: Intent) -> ProtoIntent {
    match intent {
        Intent::Navigational => ProtoIntent::Navigational,
        Intent::Exploratory => ProtoIntent::Exploratory,
        Intent::Lookup => ProtoIntent::Lookup,
    }
}

/// A stable lowercase name per [`Source`] — duplicated rather than reaching
/// into `features::vector`'s private `source_serde` (a shared type gets no
/// capability added by a downstream consumer that only needs one specific
/// thing from it; see that module's own doc comment, and `fuse::mod`'s
/// `source_ordinal` for the identical precedent).
const fn source_name(source: Source) -> &'static str {
    match source {
        Source::Lexical => "lexical",
        Source::Dense => "dense",
        Source::Fuzzy => "fuzzy",
        Source::Entity => "entity",
        Source::Structured => "structured",
        Source::Prefix => "prefix",
        Source::Recency => "recency",
    }
}

fn to_proto_snippet(snippet: &present::Snippet) -> ProtoSnippet {
    ProtoSnippet {
        text: snippet.text.clone(),
        highlights: snippet
            .highlights
            .iter()
            .map(|range| ProtoByteRange {
                start: u32::try_from(range.start).unwrap_or(u32::MAX),
                end: u32::try_from(range.end).unwrap_or(u32::MAX),
            })
            .collect(),
    }
}

/// `pub(crate)` so `saved_search_service`'s member stream renders a message
/// identically to a search hit's, rather than growing a second translation
/// that could drift field by field.
pub(crate) fn to_proto_message(message: &repo::Message, flags: Vec<String>) -> ProtoMessage {
    ProtoMessage {
        id: message.id,
        account_id: message.account_id,
        mailbox_id: message.mailbox_id,
        thread_id: message.thread_id,
        message_id: message.message_id.clone(),
        subject: message.subject.clone(),
        from_addr: message.from_addr.clone(),
        from_name: message.from_name.clone(),
        to_addrs: message.to_addrs.clone(),
        cc_addrs: message.cc_addrs.clone(),
        date: message.date,
        internaldate: message.internaldate,
        size: message.size,
        has_attachments: message.has_attachments,
        flags,
        created_at: message.created_at,
        updated_at: message.updated_at,
    }
}

/// One compiled natural-language plan on the wire.
///
/// `pub(crate)` so `saved_search_service` renders the plan it stored a folder
/// from identically to the one `CompileQuery` returns — a client confirming a
/// plan and then defining a folder from it must not be shown two different
/// descriptions of the same compile.
pub(crate) fn to_proto_query_plan(compiled: &rmail_core::query::CompiledQuery) -> ProtoQueryPlan {
    ProtoQueryPlan {
        raw: compiled.raw.clone(),
        compiled: compiled.query.clone(),
        filters: compiled.filters.clone(),
        semantic_query: compiled.semantic_query.clone(),
        intent: to_proto_intent(compiled.intent).into(),
        notes: compiled.notes.clone(),
        cached: compiled.cached,
        model: compiled.model.clone(),
        compiled_at: compiled.compiled_at,
    }
}

/// One core [`rmail_core::index::entities::EntityHit`] on the wire.
fn to_proto_entity_hit(hit: &EntityHit) -> ProtoEntityHit {
    ProtoEntityHit {
        entity_id: hit.entity_id,
        kind: hit.kind.as_str().to_owned(),
        value: hit.value.clone(),
        norm: hit.norm.clone(),
        meta: hit.meta.clone().unwrap_or_default(),
        mentions: hit.mentions,
        messages: hit.messages,
        last_seen: hit.last_seen.unwrap_or_default(),
        examples: hit
            .examples
            .iter()
            .map(|example| ProtoEntityMention {
                message_id: example.message_id,
                subject: example.subject.clone(),
                date: example.date.unwrap_or_default(),
                part: example.part.clone(),
                span_start: example.span_start,
                span_end: example.span_end,
            })
            .collect(),
    }
}

/// One core [`rmail_core::attach::search::AttachmentHit`] on the wire.
fn to_proto_attachment_hit(
    hit: rmail_core::attach::search::AttachmentHit,
) -> rmail_proto::v1::AttachmentHit {
    rmail_proto::v1::AttachmentHit {
        message_id: hit.message_id,
        message_uid: hit.message_uid,
        account_id: hit.account_id,
        mailbox: hit.mailbox,
        subject: hit.subject,
        from_addr: hit.from_addr,
        date: hit.date,
        part_id: hit.part_id,
        filename: hit.filename,
        content_type: hit.content_type,
        bytes: hit.bytes,
        pages: hit.pages,
        page: hit.page,
        span_start: hit.span_start,
        span_end: hit.span_end,
        excerpt: hit.excerpt,
        provenance: hit.provenance.as_str().to_owned(),
        score: hit.score,
        lexical_rank: hit.lexical_rank,
        dense_rank: hit.dense_rank,
    }
}

/// The `SearchService` handler.
///
/// Cheap to clone: every field either shares a `Database`/connection pool
/// (the same pattern every pipeline-stage type already uses) or is a small
/// `Arc`-backed handle — the same "clone into the spawned producer task"
/// shape `MailApi`'s streaming RPCs use, just applied to the whole struct
/// since every field already supports it.
#[derive(Clone)]
pub struct SearchApi {
    db: Database,
    fts: FtsIndex,
    semantic_index: SemanticIndex,
    /// `None` when `search.retrievers.dense` is off — `Semantic` then
    /// degrades to zero candidates rather than erroring, the same graceful
    /// degradation `retrieve::fanout::Fanout` gives a disabled source.
    dense: Option<DenseRetriever>,
    /// Attachment-granular retrieval (task 74), over the *same* embedder this
    /// handler plans queries with. A second embedder here would embed the
    /// query with one model and the corpus with another, which produces
    /// cosines that mean nothing and still sort.
    attachments: AttachmentSearch,
    planner: QueryPlanner,
    fuser: Fuser,
    feature_extractor: FeatureExtractor,
    ranker: L1Ranker,
    /// Stage 5. Always present — a stage with no usable backend is a
    /// passthrough, not an absent one, so there is exactly one code path
    /// through ranking whether or not this daemon can rerank.
    rerank: L2Stage,
    presenter: Presenter,
    /// The implicit-feedback log (task 64). Always present; the
    /// `search.learning` opt-out lives inside it, so this file has one code
    /// path rather than an `Option` every call site has to remember to check
    /// — see `rmail_core::feedback`'s own module docs.
    feedback: FeedbackStore,
    search: SearchConfig,
    /// Cancelled when the daemon shuts down, so every generation's token —
    /// and therefore every open stream — stops with it. Same pattern as
    /// `MailApi`/`SyncApi`.
    shutdown: CancellationToken,
    generation: Generation,
    /// Stage 0's NL→plan compiler (task 58), when the daemon has a provider.
    ///
    /// `Option` for the same reason `dense` is one: a handler built without it
    /// is a real configuration (this crate's own unit fixtures, which have no
    /// provider), and `CompileQuery` then answers `UNIMPLEMENTED` rather than
    /// this file growing a second constructor. `rmaild::serve_*` always sets
    /// it — the capability would otherwise have no surface at all.
    compiler: Option<QueryCompiler>,
}

/// The half of [`SearchApi::build_hits`]' inputs that does not change between
/// the two calls one streamed page makes.
///
/// `build_hits` is called twice per page — once for the first-hit fast path,
/// once for the remainder — and only `presented` differs. Grouping the rest
/// keeps that fact visible at both call sites, and keeps the parameter list
/// inside `clippy::too_many_arguments`' limit now that ranking (`reasons`) and
/// feedback logging (`query_id`) each contribute one.
#[derive(Clone, Copy)]
struct HitContext<'a> {
    fused: &'a [FusedCandidate],
    features: &'a [CandidateFeatures],
    plan: &'a QueryPlan,
    explain: bool,
    /// `Some` only when this page is being logged for the learning loop.
    query_id: Option<i64>,
    /// Per-message one-line rationale from an L2 reranker, when one ran.
    reasons: &'a BTreeMap<i64, String>,
}

impl SearchApi {
    /// Build the handler over `db`, the daemon's search-configured embedder,
    /// an already-validated ranker `weights` table, and the loaded `[search]`
    /// config.
    ///
    /// `weights` is a parameter rather than derived here from
    /// `search.rank_weights` internally so validation happens exactly once,
    /// at the single call site (`rmaild::serve_uds_with_engine_and_mail_store`)
    /// that needs to fail the *daemon's* startup — before any socket is
    /// bound — on a bad `[search.rank_weights]` override; see
    /// `rank::l1::Weights::from_config`'s own docs for why nothing validated
    /// this automatically before `SearchService` existed to call it. A
    /// fallible constructor here would just push that same `?` one frame
    /// later for no benefit.
    ///
    /// `rerank` is likewise built by the caller: the Claude backend needs the
    /// daemon's single `ai::Provider` (one API-key resolution, one HTTP
    /// client for the process), which is a thing `rmaild::serve_*` owns and
    /// this constructor has no way to reach. Pass
    /// [`L2Stage::disabled`](rmail_core::rank::l2::L2Stage::disabled) for a
    /// daemon that should never rerank.
    #[must_use]
    pub fn new(
        db: Database,
        embedder: Arc<dyn Embedder>,
        weights: Weights,
        search: SearchConfig,
        semantic_config: &IndexSemanticConfig,
        rerank: L2Stage,
        shutdown: CancellationToken,
    ) -> Self {
        let fts = FtsIndex::new(db.clone(), search.bm25_weights.clone());
        let semantic_index = SemanticIndex::new(db.clone(), Arc::clone(&embedder), semantic_config);
        let dense = search
            .retrievers
            .dense
            .then(|| DenseRetriever::new(db.clone(), &semantic_index));
        let attachments = AttachmentSearch::new(db.clone(), Arc::clone(&embedder), &search);
        let planner = QueryPlanner::new(db.clone(), embedder, search.expansion.clone());
        let fuser = Fuser::new(db.clone());
        let feature_extractor = FeatureExtractor::new(
            db.clone(),
            search.bm25_weights.clone(),
            search.retrievers.recency_half_life_days,
        );
        let ranker = L1Ranker::new(weights);
        let presenter = Presenter::new(db.clone());
        let feedback = FeedbackStore::new(db.clone(), search.learning, search.feedback);

        Self {
            db,
            fts,
            semantic_index,
            dense,
            attachments,
            planner,
            fuser,
            feature_extractor,
            ranker,
            rerank,
            presenter,
            feedback,
            search,
            shutdown,
            generation: Generation::default(),
            compiler: None,
        }
    }

    /// Attach the natural-language query compiler `CompileQuery` serves from.
    ///
    /// A builder rather than a constructor parameter because the compiler
    /// needs the daemon's single `ai::Provider` and its shared AI
    /// semaphore/rate limiter — handles `rmaild::serve_*` owns and this
    /// constructor cannot reach, exactly as with `rerank`.
    #[must_use]
    pub fn with_query_compiler(mut self, compiler: QueryCompiler) -> Self {
        self.compiler = Some(compiler);
        self
    }

    /// The feedback log this handler writes through, so the daemon can drive
    /// its retention sweep against the same store (and therefore the same
    /// `search.learning`/`[search.feedback]` policy) the search path uses,
    /// rather than constructing a second one that could be configured
    /// differently.
    #[must_use]
    pub fn feedback(&self) -> &FeedbackStore {
        &self.feedback
    }

    /// The attachment-search surface this handler serves `SearchAttachments`
    /// from, so `AttachmentService.AskAttachment` retrieves through the same
    /// object rather than a second one of its own.
    ///
    /// The point is not to save an allocation. A second `AttachmentSearch`
    /// could be built with a different embedder or a different `[search]`
    /// table, and then "ask draws on what search found" would be a claim
    /// about two independently-configured rankers rather than a fact.
    #[must_use]
    pub fn attachments(&self) -> &AttachmentSearch {
        &self.attachments
    }

    /// Kick off a `Search`/`Semantic` stream: register it as the current
    /// generation (cancelling whichever stream held the slot before),
    /// spawn the pipeline as a background task, and return the response
    /// stream immediately — the client sees this call resolve before any
    /// planning/retrieval work has even started, which is what makes the
    /// generation-token race in the module docs' "Cancellation" section
    /// actually favor the newer request.
    /// Run `req` through the pipeline and stream its hits, stopping when
    /// `cancel` fires.
    ///
    /// The token is a parameter rather than taken from [`Generation`] here so
    /// the *supersede* policy stays with the caller. `Search`/`Semantic` pass
    /// `self.generation.begin(&self.shutdown)`, which cancels whichever
    /// stream held the interactive slot before — the right answer for a
    /// search box. `SavedSearchService.RunSavedSearch` (`pub(crate)` is what
    /// lets it reuse *this* pipeline rather than assembling a second, subtly
    /// different one) passes a plain shutdown child instead: a named query is
    /// not a keystroke, and a cancelled stream ends cleanly rather than
    /// erroring, so sharing the slot would silently return a short page under
    /// an `OK`.
    pub(crate) async fn start_stream(
        &self,
        req: SearchRequest,
        dense_only: bool,
        cancel: CancellationToken,
    ) -> Result<
        Response<
            Pin<
                Box<
                    dyn tokio_stream::Stream<Item = Result<ProtoSearchHit, Status>>
                        + Send
                        + 'static,
                >,
            >,
        >,
        Status,
    > {
        let (tx, rx) = tokio::sync::mpsc::channel(STREAM_BUFFER);
        let this = self.clone();
        // Kept alive past `run_stream` purely to carry the terminal frame — see
        // below. It also means the response channel does not close until this
        // task decides it should, which is what makes the ordering explicit
        // rather than incidental.
        let terminator = tx.clone();
        tokio::spawn(
            async move {
                // `tx` is moved into `run_stream` and dropped when it
                // returns, so the client sees end-of-stream *before* the
                // feedback write below even starts. That ordering is the
                // whole reason the logging lives out here rather than at the
                // bottom of `run_stream`: a page held open while the writer
                // connection is busy would be a search this task made slower.
                let outcome = this.run_stream(req, dense_only, cancel, tx).await;
                // A superseded pipeline bails at any of half a dozen
                // `cancel.is_cancelled()` checks *between* stages, most of
                // which never reach a `send` — so the terminal frame is
                // emitted here, at the one point every path converges. Without
                // it the stream ends `OK` and the client that lost the
                // generation slot cannot tell a short page from a whole one;
                // the slot is daemon-wide, so that client is not necessarily
                // the one that took it.
                if outcome.cancelled {
                    crate::stream::terminate_cancelled(&terminator).await;
                }
                drop(terminator);
                if let Some(page) = outcome.logged {
                    this.log_page(page).await;
                }
            }
            .instrument(tracing::Span::current()),
        );
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    /// Persist one served page's impressions, downgrading every failure to a
    /// warning.
    ///
    /// Nothing upstream of here can fail a search by this point — the hits
    /// are already on the wire — so an error is logged and dropped. The
    /// alternative (propagating it) has nowhere to go: there is no response
    /// left to attach it to.
    async fn log_page(&self, page: LoggedPage) {
        let query_id = page.record.query_id;
        let shown = page.impressions.len();
        match self.feedback.log_query(page.record, page.impressions).await {
            Ok(written) => {
                tracing::debug!(query_id, shown, written, "logged a search impression batch");
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    query_id,
                    shown,
                    "logging search impressions failed; this page contributes no training data"
                );
            }
        }
    }

    /// Run the pipeline end to end and stream every hit it produces — the
    /// body of both `Search` and `Semantic`. See the module docs' "Streaming
    /// the first hit" section for the two-phase `present` call.
    ///
    /// Returns the impressions this page actually put on the wire, for the
    /// caller to log *after* the response stream has closed — see the module
    /// docs' "Feedback" section for why logging happens there rather than
    /// here, and why a cancelled stream returns `None` rather than an empty
    /// batch.
    ///
    /// It also reports **why** it stopped. That cannot be re-derived by reading
    /// the token afterwards: between the last hit and that read, a fresh
    /// `Search` can take the daemon-wide generation slot, and a client that
    /// received the whole page would then be told `CANCELLED`. Each exit
    /// records the answer where it is still true.
    #[tracing::instrument(skip(self, req, cancel, tx), fields(dense_only, explain = req.explain))]
    async fn run_stream(
        &self,
        req: SearchRequest,
        dense_only: bool,
        cancel: CancellationToken,
        tx: tokio::sync::mpsc::Sender<Result<ProtoSearchHit, Status>>,
    ) -> StreamOutcome {
        let now = Utc::now();
        // Minted before anything is written, because it has to be on every
        // hit as it streams; `None` when `search.learning` is off, and then
        // nothing below builds an impression at all.
        let query_id = self.feedback.new_query_id();

        let query = match self
            .effective_query(&req.query, &req.filter, req.account_id)
            .await
        {
            Ok(query) => query,
            Err(status) => {
                let _ = send(&tx, &cancel, Err(status)).await;
                // An error frame is already terminal; a CANCELLED after it
                // would be a second terminal status for one call.
                return StreamOutcome::failed();
            }
        };

        let mut plan = match self.planner.plan_at(&query, now).await {
            Ok(plan) => plan,
            Err(error) => {
                let _ = send(&tx, &cancel, Err(Status::from(error))).await;
                return StreamOutcome::failed();
            }
        };
        if let Some(intent) = decode_intent(req.intent) {
            plan.intent = intent;
        }

        let candidates = if dense_only {
            self.dense_only_candidates(&plan, self.search.candidates_per_source, &cancel)
                .await
        } else {
            let mode = resolve_mode(req.mode, self.search.default_mode);
            self.candidates_for_mode(mode, &plan, self.search.candidates_per_source, &cancel)
                .await
        };
        if cancel.is_cancelled() {
            return StreamOutcome::stopped(&cancel);
        }

        let fused = self
            .fuser
            .fuse(
                candidates,
                &plan,
                &self.search,
                req.thread_collapse,
                &cancel,
            )
            .await;
        if cancel.is_cancelled() {
            return StreamOutcome::stopped(&cancel);
        }

        let features = self
            .feature_extractor
            .extract_at(&fused, &plan, now, &cancel)
            .await;
        let ranked = self
            .ranker
            .rank(&features, plan.intent, self.search.top_k_rerank as usize);
        if ranked.is_empty() || cancel.is_cancelled() {
            return StreamOutcome::stopped(&cancel);
        }

        // Stage 5, over the L1 top-K. Never fails: `L2Stage::rerank` returns
        // the L1 order unchanged on every error, budget exhaustion, and
        // timeout — see `rank::l2`'s own docs.
        let reranked = self
            .rerank_for(req.rerank)
            .rerank(&plan.raw, &ranked, search_kind(req.deep), &cancel)
            .await;
        let ranked = reranked.ranked;
        let reasons = reranked.reasons;
        // `None`, not `finish_page`: this is still ahead of the first hit, so
        // there are no impressions to log — the same answer the cancellation
        // checks above give. (Task 51 wrote a bare `return` here against a
        // `()`-returning `run_stream`; task 64 gave it a return value.)
        if cancel.is_cancelled() {
            return StreamOutcome::stopped(&cancel);
        }

        let limit = if req.limit == 0 {
            self.search.default_limit as usize
        } else {
            req.limit as usize
        };
        let lambda = self.search.mmr_lambda;

        // Impressions are accumulated as hits are *successfully handed to the
        // response channel*, not as they are built. That is the strongest
        // "was this shown?" signal this side of the wire has — it is not
        // proof the client rendered it, but a hit that never left the daemon
        // definitely was not shown, and logging one would teach a
        // position-bias model that a result was displayed and ignored when it
        // was in fact never displayed. (`scroll_past` is how a client reports
        // the finer-grained truth about what it actually painted.)
        //
        // `None` when `search.learning` is off, so the opt-out costs not even
        // the allocation — and, more to the point, so there is no populated
        // batch sitting around for a later refactor to accidentally write.
        let mut page = query_id.map(|query_id| LoggedPage {
            record: QueryRecord {
                query_id,
                // `SearchRequest.account_id = 0` means "every configured
                // account" (see `effective_query`), which is a NULL scope
                // rather than account 0.
                account_id: (req.account_id != 0).then_some(req.account_id),
                raw_query: query.clone(),
                intent: plan.intent,
                issued_at: now.timestamp(),
            },
            impressions: Vec::new(),
        });
        let vectors: BTreeMap<i64, &CandidateFeatures> =
            features.iter().map(|cf| (cf.message_id, cf)).collect();

        // Phase 1: the single best-scoring candidate, presented and flushed
        // alone — see the module docs for why this is provably the same
        // message the full-page call below would also pick first.
        let head = &ranked[..1];
        let first_presented = self
            .presenter
            .present(head, &fused, &plan, lambda, 1, &cancel)
            .await;
        // Built once and shared by both `build_hits` calls below — see
        // `HitContext`.
        let hit_ctx = HitContext {
            fused: &fused,
            features: &features,
            plan: &plan,
            explain: req.explain,
            query_id,
            reasons: &reasons,
        };
        let first_hits = self.build_hits(&first_presented, &hit_ctx).await;
        let mut sent: BTreeSet<i64> = BTreeSet::new();
        // The 1-based rank of the last hit put on the wire — the impression's
        // `position`. Counted here rather than inside `record_impression` so
        // it stays the *wire* ordinal even if an impression is ever skipped;
        // see that function's own docs.
        let mut rank: u32 = 0;
        for (id, hit) in first_hits {
            sent.insert(id);
            let score = hit.score;
            if send(&tx, &cancel, Ok(hit)).await.is_break() {
                return StreamOutcome::stopped_with(finish_page(page, &cancel), &cancel);
            }
            rank = rank.saturating_add(1);
            record_impression(page.as_mut(), &vectors, id, rank, score);
        }
        if cancel.is_cancelled() {
            return StreamOutcome::stopped(&cancel);
        }

        // Phase 2: the rest of the page.
        let presented = self
            .presenter
            .present(&ranked, &fused, &plan, lambda, limit, &cancel)
            .await;
        let rest: Vec<PresentedResult> = presented
            .into_iter()
            .filter(|p| !sent.contains(&p.message_id))
            .collect();
        let rest_hits = self.build_hits(&rest, &hit_ctx).await;
        for (id, hit) in rest_hits {
            let score = hit.score;
            if send(&tx, &cancel, Ok(hit)).await.is_break() {
                return StreamOutcome::stopped_with(finish_page(page, &cancel), &cancel);
            }
            rank = rank.saturating_add(1);
            record_impression(page.as_mut(), &vectors, id, rank, score);
        }
        StreamOutcome::completed(finish_page(page, &cancel))
    }

    /// Run the pipeline to completion and return the presented message ids,
    /// best first — the collected counterpart of [`Self::run_stream`], and
    /// what `Evaluate` and `AskMailbox` both need.
    ///
    /// Deliberately measured *after* `present`, not after `rank`: MMR
    /// diversification and thread collapsing reorder and drop results, and
    /// what the metrics have to score is the page a user would actually see.
    /// Scoring the pre-presentation ranking would produce a number that
    /// could improve while the shipped experience got worse, which is the
    /// one failure mode an eval harness exists to prevent.
    ///
    /// # Errors
    ///
    /// [`Status`] from account resolution or query planning. Candidate
    /// generation degrades to an empty result rather than erroring, matching
    /// the streaming path.
    async fn ranked_page(&self, req: &PageRequest<'_>) -> Result<Vec<i64>, Status> {
        let PageRequest {
            query,
            filter,
            account_id,
            mode,
            limit,
            rerank,
            kind,
            cancel,
        } = *req;
        let now = Utc::now();
        let text = self.effective_query(query, filter, account_id).await?;
        let plan = self
            .planner
            .plan_at(&text, now)
            .await
            .map_err(Status::from)?;

        let candidates = self
            .candidates_for_mode(mode, &plan, self.search.candidates_per_source, cancel)
            .await;
        let fused = self
            .fuser
            .fuse(candidates, &plan, &self.search, false, cancel)
            .await;
        let features = self
            .feature_extractor
            .extract_at(&fused, &plan, now, cancel)
            .await;

        // `top_k_rerank` bounds what the ranker hands downstream, and it
        // defaults to 50 — exactly `RECALL_K`. An eval asking for more
        // results than that would otherwise be silently truncated and would
        // report the truncation as missing recall, so the rank cut is
        // widened to whatever this run actually asked for.
        let keep = self.search.top_k_rerank as usize;
        let ranked = self.ranker.rank(&features, plan.intent, keep.max(limit));
        if ranked.is_empty() {
            return Ok(Vec::new());
        }
        // Stage 5 runs here too, under whichever policy/kind the caller
        // asked for. Both of this method's callers pick deliberately and
        // differently — see `PageRequest`'s own fields.
        let ranked = self
            .rerank
            .clone()
            .with_policy(rerank)
            .rerank(&plan.raw, &ranked, kind, cancel)
            .await
            .ranked;

        let presented = self
            .presenter
            .present(
                &ranked,
                &fused,
                &plan,
                self.search.mmr_lambda,
                limit,
                cancel,
            )
            .await;
        Ok(presented.into_iter().map(|p| p.message_id).collect())
    }

    /// Candidate generation for `Search`, honoring `mode`. `WireMode::Hybrid`
    /// and `WireMode::Lexical` both run through a fresh [`Fanout`] — cheap to
    /// construct (no I/O; see `Fanout::new`'s own docs) — built from a
    /// request-scoped [`RetrieversConfig`] for `Lexical` (every optional
    /// source off; the lexical retriever itself is never toggleable, per
    /// `retrieve::fanout`'s own docs) or the daemon's configured one for
    /// `Hybrid`. `WireMode::Semantic` delegates to
    /// [`Self::dense_only_candidates`] — the identical path `Semantic` (the
    /// RPC) always takes regardless of `mode`.
    async fn candidates_for_mode(
        &self,
        mode: WireMode,
        plan: &QueryPlan,
        limit: u32,
        cancel: &CancellationToken,
    ) -> Vec<Candidate> {
        match mode {
            WireMode::Semantic => self.dense_only_candidates(plan, limit, cancel).await,
            WireMode::Lexical => {
                let cfg = RetrieversConfig {
                    dense: false,
                    fuzzy: false,
                    entity: false,
                    structured: false,
                    prefix: false,
                    recency: false,
                    recency_half_life_days: self.search.retrievers.recency_half_life_days,
                };
                let fanout = Fanout::new(
                    self.db.clone(),
                    self.fts.clone(),
                    &self.semantic_index,
                    &cfg,
                );
                fanout.generate(plan, limit, cancel).await
            }
            WireMode::Hybrid => {
                let fanout = Fanout::new(
                    self.db.clone(),
                    self.fts.clone(),
                    &self.semantic_index,
                    &self.search.retrievers,
                );
                fanout.generate(plan, limit, cancel).await
            }
        }
    }

    /// This request's Stage 5, honoring `SearchRequest.rerank`'s override of
    /// the daemon's `search.rerank`.
    ///
    /// Returns an owned stage rather than a borrow because an override has to
    /// produce a *different* policy without mutating the shared one — and
    /// cloning is what [`L2Stage`] is built for (two `Arc`s and a pooled
    /// database handle; the Claude backend's verdict cache is shared, not
    /// copied, so an override never costs a cache).
    fn rerank_for(&self, raw: i32) -> L2Stage {
        let Some(requested) = decode_rerank(raw) else {
            return self.rerank.clone();
        };
        self.rerank
            .clone()
            .with_policy(clamp_rerank(requested, self.search.rerank))
    }

    /// Dense-vector-only candidate generation — `Semantic`'s whole job
    /// (prd.md: "dense only"), and `Search`'s own `mode = SEMANTIC` path.
    /// Degrades to no candidates, never an error, both when dense retrieval
    /// is disabled by config and when the retriever itself fails — the same
    /// graceful-degradation contract `Fanout::run_dense` gives its own
    /// caller.
    async fn dense_only_candidates(
        &self,
        plan: &QueryPlan,
        limit: u32,
        cancel: &CancellationToken,
    ) -> Vec<Candidate> {
        let Some(dense) = &self.dense else {
            tracing::debug!(
                "dense retriever disabled by config; semantic-only search returns no candidates"
            );
            return Vec::new();
        };
        match dense.retrieve(plan, i64::from(limit), cancel).await {
            Ok(candidates) => candidates,
            Err(error) => {
                tracing::warn!(%error, "dense retriever failed; degrading to no candidates");
                Vec::new()
            }
        }
    }

    /// `query`, with `filter` and (when `account_id` names a real account) an
    /// `account:"<name>"` operator appended — the operator grammar has no
    /// separate "filter language" of its own (see `query::parse`'s own
    /// docs), so folding both onto one string before parsing is the whole
    /// integration.
    ///
    /// # Errors
    ///
    /// [`Status`] (`NOT_FOUND`) if `account_id` is nonzero and names no
    /// configured account.
    async fn effective_query(
        &self,
        query: &str,
        filter: &str,
        account_id: i64,
    ) -> Result<String, Status> {
        let mut text = query.to_owned();
        let filter = filter.trim();
        if !filter.is_empty() {
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(filter);
        }
        if account_id != 0 {
            let name = self.account_name(account_id).await?;
            // The operator grammar has no escape sequence for a literal `"`
            // inside a quoted value (see `query::parse::unquote`'s own doc
            // comment) — stripped defensively rather than risking the quote
            // terminating early and the rest of the name leaking into the
            // query as free text.
            let sanitized = name.replace('"', "");
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(&format!("account:\"{sanitized}\""));
        }
        Ok(text)
    }

    async fn account_name(&self, account_id: i64) -> Result<String, Status> {
        let account = self
            .db
            .read(move |conn| repo::get_account(conn, account_id))
            .await
            .map_err(|error| Status::from(RmailError::from(error)))?;
        account.map(|a| a.name).ok_or_else(|| {
            Status::from(RmailError::not_found(format!(
                "account {account_id} not found"
            )))
        })
    }

    /// Turn presented results into wire [`ProtoSearchHit`]s, batch-fetching
    /// their `Message` rows in one round trip rather than one per hit.
    ///
    /// Everything except `presented` is identical between the two calls a
    /// streamed page makes, which is why it travels as one [`HitContext`]
    /// rather than six more parameters.
    /// Returns `(message_id, hit)` pairs, in `presented`'s own order, so a
    /// caller can both stream them and track which ids it already sent.
    ///
    /// A `message_id` [`Self::fetch_messages`] found no row for (deleted
    /// between ranking and now) is silently dropped — a hit with no
    /// `Message` payload is not something any client can render.
    async fn build_hits(
        &self,
        presented: &[PresentedResult],
        ctx: &HitContext<'_>,
    ) -> Vec<(i64, ProtoSearchHit)> {
        let HitContext {
            fused,
            features,
            plan,
            explain,
            query_id,
            reasons,
        } = *ctx;
        if presented.is_empty() {
            return Vec::new();
        }
        let ids: Vec<i64> = presented.iter().map(|p| p.message_id).collect();
        let messages = self.fetch_messages(&ids).await;
        let fused_by_id: BTreeMap<i64, &FusedCandidate> =
            fused.iter().map(|f| (f.message_id, f)).collect();
        let features_by_id: BTreeMap<i64, &CandidateFeatures> =
            features.iter().map(|f| (f.message_id, f)).collect();

        let mut out = Vec::with_capacity(presented.len());
        for p in presented {
            let Some(message) = messages.get(&p.message_id).cloned() else {
                continue;
            };
            let sources: Vec<String> = fused_by_id
                .get(&p.message_id)
                .map(|f| {
                    f.hits
                        .iter()
                        .map(|h| source_name(h.source).to_owned())
                        .collect()
                })
                .unwrap_or_default();
            let snippet = to_proto_snippet(&p.snippet);
            let claude_reason = reasons.get(&p.message_id).cloned().unwrap_or_default();
            let cf = features_by_id.get(&p.message_id);
            // The L1 score for *this* candidate, recomputed from its own
            // feature vector — not `p.score`. After a Stage 5 rerank the two
            // differ by design (`rank::l2` permutes the window's scores to
            // express the new order), and `RankExplanation`'s documented
            // invariant is that its per-feature contributions sum to its
            // `score`. Only the recomputed value satisfies that; `p.score`
            // would make the breakdown fail to add up exactly when a rerank
            // ran.
            let l1_score = cf.map_or(p.score, |cf| self.ranker.score(&cf.features, plan.intent));
            let why = if explain {
                cf.map(|cf| {
                    self.explanation(
                        cf,
                        plan.intent,
                        l1_score,
                        sources.clone(),
                        Some(snippet.clone()),
                        claude_reason.clone(),
                    )
                })
            } else if claude_reason.is_empty() {
                None
            } else {
                // prd.md's `mail search "invoice" --rerank claude` carries no
                // `--explain`, and its whole point is the one-line "why this
                // matched". A request that paid for a listwise call must get
                // the reasons back; the feature breakdown stays behind
                // `explain`, since that is the expensive, verbose part.
                Some(ProtoRankExplanation {
                    features: Vec::new(),
                    score: l1_score,
                    sources: sources.clone(),
                    matched: None,
                    claude_reason: claude_reason.clone(),
                })
            };
            let hit = ProtoSearchHit {
                message: Some(message),
                score: p.score,
                snippet: Some(snippet),
                sources,
                why,
                thread_id: p.thread_id,
                thread_collapsed: p.thread_collapsed.clone(),
                near_duplicates: p.near_duplicates.clone(),
                // 0 is the wire sentinel for "this search was not logged, so
                // there is nothing to attribute feedback to" — which is
                // exactly what `None` means here (the `search.learning`
                // opt-out).
                query_id: query_id.unwrap_or(0),
            };
            out.push((p.message_id, hit));
        }
        out
    }

    /// `Message` rows for `ids`, in two batched round trips (rows, then
    /// flags — mirroring `repo::get_messages`/`repo::list_flags_by_message`'s
    /// own split). Never fails outright: a fetch error degrades to an empty
    /// map, dropping the affected hits from the stream rather than failing
    /// the whole page — matching the graceful-degradation contract every
    /// batched fetch in `fuse`/`features`/`present` already gives its own
    /// callers.
    ///
    /// Not routed through `retrieve::cancel::interruptible_read` (not
    /// reachable outside `rmail-core` — see `repo::get_body_text`'s own doc
    /// comment for why this crate has no `rusqlite` dependency to call it
    /// with anyway): this fetch runs only *after* ranking already decided
    /// the final, small (`limit`-or-`1`-bounded) result set, not as part of
    /// an open-ended scan, so a plain read is the right cost/complexity
    /// trade here even though it cannot be `sqlite3_interrupt()`-ed
    /// mid-flight the way a retriever's own scan can.
    async fn fetch_messages(&self, ids: &[i64]) -> BTreeMap<i64, ProtoMessage> {
        if ids.is_empty() {
            return BTreeMap::new();
        }
        let ids_for_rows = ids.to_vec();
        let rows = match self
            .db
            .read(move |conn| repo::get_messages(conn, &ids_for_rows))
            .await
        {
            Ok(rows) => rows,
            Err(error) => {
                tracing::warn!(%error, "search result message fetch failed; affected hits dropped");
                return BTreeMap::new();
            }
        };
        let ids_for_flags = ids.to_vec();
        let flags = match self
            .db
            .read(move |conn| repo::list_flags_by_message(conn, &ids_for_flags))
            .await
        {
            Ok(flags) => flags,
            Err(error) => {
                tracing::warn!(%error, "search result flag fetch failed; hits carry no flags");
                BTreeMap::new()
            }
        };
        rows.into_iter()
            .map(|message| {
                let id = message.id;
                let message_flags = flags.get(&id).cloned().unwrap_or_default();
                (id, to_proto_message(&message, message_flags))
            })
            .collect()
    }

    /// Build a [`ProtoRankExplanation`] from `cf`'s own feature vector —
    /// shared by the inline per-hit `why` (when `SearchRequest.explain` is
    /// set) and the dedicated `Explain` RPC. `score` is taken from the
    /// caller rather than recomputed here so both call sites can pass the
    /// exact number their own pipeline already produced (`PresentedResult`'s
    /// carried-through `RankedCandidate::score` for the inline case,
    /// `L1Ranker::score` for the standalone `Explain` case) rather than a
    /// third, independently-derived value that happens to agree.
    fn explanation(
        &self,
        cf: &CandidateFeatures,
        intent: Intent,
        score: f64,
        sources: Vec<String>,
        matched: Option<ProtoSnippet>,
        claude_reason: String,
    ) -> ProtoRankExplanation {
        let contributions = self.ranker.contributions(&cf.features, intent);
        let features = contributions
            .into_iter()
            .map(
                |(name, value, weight, weighted_contribution)| ProtoFeatureContribution {
                    name: name.as_str().to_owned(),
                    value,
                    weight,
                    weighted_contribution,
                },
            )
            .collect();
        ProtoRankExplanation {
            features,
            score,
            sources,
            matched,
            claude_reason,
        }
    }

    /// The matched span(s) for `message_id` against `plan.raw` — `Explain`'s
    /// own "matched" field, computed the same way `Presenter` builds every
    /// other hit's snippet (`present::snippet::extract`, falling back to
    /// `plain_excerpt`), just for one message rather than a batch.
    async fn matched_snippet(
        &self,
        message_id: i64,
        plan: &QueryPlan,
        cancel: &CancellationToken,
    ) -> Option<ProtoSnippet> {
        if cancel.is_cancelled() {
            return None;
        }
        let body = match self
            .db
            .read(move |conn| repo::get_body_text(conn, message_id))
            .await
        {
            Ok(body) => body,
            Err(error) => {
                tracing::warn!(%error, "explain: body fetch failed; matched span omitted");
                None
            }
        }?;
        let terms = present::snippet::query_terms(&plan.raw);
        let snippet = present::snippet::extract(&body, &terms.terms, &terms.phrases)
            .unwrap_or_else(|| present::snippet::plain_excerpt(&body));
        Some(to_proto_snippet(&snippet))
    }
}

/// Adapts [`SearchApi`] to `rmail_core::eval::RankedSearch`, pinning the
/// mode and cancellation token for one `Evaluate` call.
///
/// The adapter exists because [`RankedSearch`] is deliberately narrow —
/// "query in, ranked ids out" — while the pipeline also needs a mode and a
/// cancellation token that are constant across an evaluation run. Binding
/// them here keeps `rmail-core` from having to know either concept, and
/// keeps evaluation on the identical `SearchApi` that serves `Search`
/// rather than on a second, drift-prone copy of the pipeline.
struct EvalSearch<'a> {
    api: &'a SearchApi,
    mode: WireMode,
    cancel: CancellationToken,
}

#[async_trait::async_trait]
impl RankedSearch for EvalSearch<'_> {
    async fn ranked_ids(
        &self,
        query: &str,
        account_id: i64,
        limit: usize,
    ) -> Result<Vec<i64>, RmailError> {
        self.api
            .ranked_page(&PageRequest {
                query,
                filter: "",
                account_id,
                mode: self.mode,
                limit,
                // An eval run must score the configuration this daemon
                // actually ships (prd.md's "relevance is measured, not
                // asserted"), so Stage 5 runs — but with the hosted backend
                // substituted (see `eval_rerank`), and as `Interactive`,
                // because a golden-set sweep is not a user asking one
                // expensive question.
                rerank: eval_rerank(self.api.search.rerank),
                kind: SearchKind::Interactive,
                cancel: &self.cancel,
            })
            .await
            // `ranked_page` speaks `Status` because every other caller is a
            // gRPC handler; the trait speaks the domain error. Going back
            // through `RmailError` rather than carrying a `Status` into
            // `rmail-core` keeps the transport type out of the domain crate,
            // which is the whole reason the trait is shaped this way.
            //
            // `from_status`, not `Internal(...)`: the round trip must preserve
            // the reason. An `Evaluate` run over a golden set naming an
            // account that does not exist should say `NOT_FOUND`, not report
            // the daemon as broken.
            .map_err(|status| RmailError::from_status(&status).context("search pipeline"))
    }
}

/// The inputs [`SearchApi::ranked_page`] needs that its two callers disagree
/// about.
///
/// A struct rather than eight positional parameters because the two
/// disagreements that matter — the rerank policy and the [`SearchKind`] — are
/// exactly the two a reader would otherwise have to count commas to find, and
/// getting either wrong is a silent behaviour change (an eval run that bills a
/// Claude call per judged query; an `ask` that quietly used the interactive
/// reranker).
#[derive(Clone, Copy)]
struct PageRequest<'a> {
    query: &'a str,
    /// Extra operator-DSL terms, folded onto `query` exactly as
    /// `SearchRequest.filter` is. Empty for `Evaluate`.
    filter: &'a str,
    account_id: i64,
    mode: WireMode,
    limit: usize,
    /// The Stage 5 policy for this run.
    rerank: Rerank,
    /// What `search.rerank = "auto"` resolves against.
    kind: SearchKind,
    cancel: &'a CancellationToken,
}

/// Adapts [`SearchApi`] to `rmail_core::ai::AskRetriever` — the retrieval half
/// of `AiService.AskMailbox` (task 52).
///
/// The adapter is what keeps mailbox RAG on the *same* pipeline every other
/// surface uses rather than a second assembly of it (prd.md: "built on the
/// same pipeline (retrieve → rerank → generate)"). Two choices in it are
/// deliberate and load-bearing:
///
/// - **[`SearchKind::Deep`]**, which is the seam task 51 built for exactly
///   this caller: under the default `search.rerank = "auto"`, deep is what
///   routes to the Claude listwise reranker instead of the interactive
///   cross-encoder. A question is the quality-bound case prd.md names.
/// - **The daemon's configured `search.rerank`, unclamped** — unlike
///   `SearchRequest.rerank`, which `clamp_rerank` only ever lets *reduce* the
///   configured backend. There is nothing to clamp here: `AskMailbox` already
///   requires `ai.invoke` (see `auth::methods`) precisely because calling a
///   provider is the whole RPC, so a rerank cannot escalate a caller past an
///   authority it does not already hold.
///
/// [`rmail_core::ai::RagEngine`] owns everything after this point — the policy
/// gate, packing, the model call, citations. This type's whole job is "a
/// question in, ranked message ids out."
#[derive(Clone)]
pub struct AskSearch {
    api: SearchApi,
}

impl std::fmt::Debug for AskSearch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AskSearch").finish_non_exhaustive()
    }
}

impl AskSearch {
    /// Wrap the daemon's one [`SearchApi`].
    #[must_use]
    pub const fn new(api: SearchApi) -> Self {
        Self { api }
    }
}

#[async_trait::async_trait]
impl AskRetriever for AskSearch {
    async fn retrieve(
        &self,
        question: &str,
        filter: &str,
        account_id: i64,
        top_k: usize,
        cancel: &CancellationToken,
    ) -> Result<Vec<i64>, RmailError> {
        self.api
            .ranked_page(&PageRequest {
                query: question,
                filter,
                account_id,
                // Hybrid regardless of `search.default_mode`: a question is
                // the case dense recall exists for ("how much did AWS bill
                // me" shares few literal words with the invoice), and a
                // lexical-only default would silently make RAG worse than the
                // search box over the same corpus.
                mode: WireMode::Hybrid,
                limit: top_k,
                rerank: self.api.search.rerank,
                kind: SearchKind::Deep,
                cancel,
            })
            .await
            // `ranked_page` speaks `Status` because every other caller is a
            // gRPC handler; the trait speaks the domain error — the identical
            // round trip `EvalSearch` makes, and for the identical reason:
            // the transport type stays out of `rmail-core`.
            //
            // The reason survives the round trip. Flattening it to `Internal`
            // (which this used to do) cost the caller twice: the code became
            // `INTERNAL`, and the boundary then scrubbed the message — so
            // `mail ask --account 999` reported "internal error" and looked
            // exactly like a daemon bug.
            .map_err(|status| RmailError::from_status(&status).context("ask retrieval"))
    }
}

/// What a request's `SearchRequest.rerank` actually resolves to, given the
/// daemon's configured `search.rerank`.
///
/// A request may only ever ask for *less* than the daemon is configured to
/// do. `Search` is authorized by `Scope::MailRead` (see `auth::methods`), and
/// a mail-reading token must not be able to turn a read of the local index
/// into a paid, egressing provider call the operator's own `search.rerank`
/// never sanctioned. `off` and `cross_encoder` only reduce cost and are
/// always honored; Claude is honored only where the configured policy already
/// reaches it.
fn clamp_rerank(requested: Rerank, configured: Rerank) -> Rerank {
    match requested {
        Rerank::Off | Rerank::CrossEncoder => requested,
        Rerank::Claude | Rerank::Auto => match configured {
            Rerank::Claude | Rerank::Auto => requested,
            configured => {
                tracing::debug!(
                    ?requested,
                    ?configured,
                    "SearchRequest.rerank asked for a backend search.rerank does not \
                     sanction; using the configured one"
                );
                configured
            }
        },
    }
}

/// The rerank policy an `Evaluate` run may use.
///
/// A golden-set sweep runs the whole pipeline once per judged query, so a
/// configured `search.rerank = "claude"` would turn one `Evaluate` RPC into
/// N provider calls — serialized behind the AI concurrency budget, at up to
/// `search.reranker.timeout` each. Measuring the shipped configuration is the
/// point (prd.md: "Relevance is measured, not asserted"), but not at the cost
/// of an unbounded bill from a regression guard, so the hosted backend is
/// substituted with the local one and everything else is measured as
/// configured.
const fn eval_rerank(configured: Rerank) -> Rerank {
    match configured {
        Rerank::Off => Rerank::Off,
        Rerank::CrossEncoder | Rerank::Claude | Rerank::Auto => Rerank::CrossEncoder,
    }
}

/// Which kind of search this is, for `search.rerank = "auto"` — prd.md's
/// "cross-encoder for interactive typing, Claude for explicit deep search."
const fn search_kind(deep: bool) -> SearchKind {
    if deep {
        SearchKind::Deep
    } else {
        SearchKind::Interactive
    }
}

fn to_proto_metrics(metrics: &CoreMetrics) -> ProtoEvalMetrics {
    ProtoEvalMetrics {
        ndcg_at_10: metrics.ndcg_at_10,
        mrr: metrics.mrr,
        recall_at_50: metrics.recall_at_50,
        p_at_3: metrics.p_at_3,
    }
}

#[tonic::async_trait]
impl SearchService for SearchApi {
    type SearchStream =
        Pin<Box<dyn tokio_stream::Stream<Item = Result<ProtoSearchHit, Status>> + Send + 'static>>;

    async fn search(
        &self,
        request: Request<SearchRequest>,
    ) -> Result<Response<Self::SearchStream>, Status> {
        let cancel = self.generation.begin(&self.shutdown);
        self.start_stream(request.into_inner(), false, cancel).await
    }

    type SemanticStream =
        Pin<Box<dyn tokio_stream::Stream<Item = Result<ProtoSearchHit, Status>> + Send + 'static>>;

    async fn semantic(
        &self,
        request: Request<SearchRequest>,
    ) -> Result<Response<Self::SemanticStream>, Status> {
        let cancel = self.generation.begin(&self.shutdown);
        self.start_stream(request.into_inner(), true, cancel).await
    }

    #[tracing::instrument(skip(self, request), fields(account_id, cached, filters))]
    async fn compile_query(
        &self,
        request: Request<CompileQueryRequest>,
    ) -> Result<Response<ProtoQueryPlan>, Status> {
        let req = request.into_inner();
        tracing::Span::current().record("account_id", req.account_id);
        if req.account_id <= 0 {
            // Not defaulted to "every account": the plan cache is per account
            // and AI policy/budget resolve against one, so guessing here would
            // charge a budget the caller never named.
            return Err(Status::from(RmailError::invalid_argument(
                "account_id is required to compile a query: the plan cache and the AI \
                 policy/budget that admits the call are both per account",
            )));
        }
        let Some(compiler) = &self.compiler else {
            return Err(Status::unimplemented(
                "this daemon was built without an AI provider, so natural-language \
                 queries cannot be compiled; use the operator grammar directly",
            ));
        };

        // A plain shutdown child, not `self.generation.begin(...)`: compiling
        // is not the interactive search slot, and a compile that a later
        // keystroke cancelled would have spent money for nothing.
        let cancel = self.shutdown.child_token();
        let compiled = compiler
            .compile(req.account_id, &req.query, req.refresh, &cancel)
            .await
            .map_err(Status::from)?;
        let span = tracing::Span::current();
        span.record("cached", compiled.cached);
        span.record("filters", compiled.filters.len());
        Ok(Response::new(to_proto_query_plan(&compiled)))
    }

    #[tracing::instrument(skip(self, request), fields(account_id, message_id, hits))]
    async fn search_attachments(
        &self,
        request: Request<SearchAttachmentsRequest>,
    ) -> Result<Response<SearchAttachmentsResponse>, Status> {
        let req = request.into_inner();
        tracing::Span::current()
            .record("account_id", req.account_id)
            .record("message_id", req.message_id);

        // A plain shutdown child, not `self.generation.begin(...)`. Attachment
        // search does not share the interactive `Search`/`Semantic` slot: a
        // client that searches attachments while a message search is still
        // streaming would otherwise cancel it, and the two are answers to
        // different questions that a UI legitimately shows side by side.
        let cancel = self.shutdown.child_token();
        let hits = self
            .attachments
            .search(
                &AttachmentQuery {
                    query: req.query,
                    account_id: req.account_id,
                    message_id: req.message_id,
                    limit: req.limit,
                },
                &cancel,
            )
            .await
            .map_err(Status::from)?;
        if cancel.is_cancelled() {
            // `AttachmentSearch` reports a cancelled scan as an empty page,
            // which is right for a *superseded* search — but nothing
            // supersedes this one: the only thing that cancels this token is
            // daemon shutdown, so an empty page here would present a
            // truncated result as a complete one.
            return Err(Status::from(RmailError::unavailable(
                "the daemon is shutting down; the attachment search did not complete",
            )));
        }
        tracing::Span::current().record("hits", hits.len());
        Ok(Response::new(SearchAttachmentsResponse {
            hits: hits.into_iter().map(to_proto_attachment_hit).collect(),
        }))
    }

    #[tracing::instrument(skip(self, request), fields(account_id, hits))]
    async fn search_entities(
        &self,
        request: Request<SearchEntitiesRequest>,
    ) -> Result<Response<SearchEntitiesResponse>, Status> {
        let req = request.into_inner();
        tracing::Span::current().record("account_id", req.account_id);
        if req.account_id < 0 {
            return Err(Status::from(RmailError::invalid_argument(
                "account_id must be a positive account id or 0 for every one",
            )));
        }
        // A kind this build does not know is INVALID_ARGUMENT rather than an
        // empty page, the same call `IndexService.ListEntities` makes: a
        // typo'd kind and a kind with no entities are very different answers.
        let mut kinds = Vec::with_capacity(req.kinds.len());
        for kind in &req.kinds {
            kinds.push(EntityKind::parse(kind.trim()).map_err(|_| {
                Status::from(RmailError::invalid_argument(format!(
                    "unknown entity kind {kind:?}"
                )))
            })?);
        }
        let hits = entities::search(
            &self.db,
            &EntityQuery {
                text: req.query,
                kinds,
                account_id: (req.account_id > 0).then_some(req.account_id),
                since: (req.since > 0).then_some(req.since),
                limit: req.limit,
            },
        )
        .await
        .map_err(Status::from)?;
        tracing::Span::current().record("hits", hits.len());
        Ok(Response::new(SearchEntitiesResponse {
            hits: hits.iter().map(to_proto_entity_hit).collect(),
        }))
    }

    async fn explain(
        &self,
        request: Request<ExplainRequest>,
    ) -> Result<Response<ProtoRankExplanation>, Status> {
        let req = request.into_inner();
        let now = Utc::now();
        // A one-shot lookup, not a session-shaped stream — see the module
        // docs' "Cancellation" section for why `Explain` neither joins nor
        // cancels the Search/Semantic generation slot.
        let cancel = self.shutdown.child_token();

        let query = self
            .effective_query(&req.query, &req.filter, req.account_id)
            .await?;
        let mut plan = self
            .planner
            .plan_at(&query, now)
            .await
            .map_err(Status::from)?;
        if let Some(intent) = decode_intent(req.intent) {
            plan.intent = intent;
        }

        let candidates = self
            .candidates_for_mode(
                WireMode::Hybrid,
                &plan,
                self.search.candidates_per_source,
                &cancel,
            )
            .await;
        let fused = self
            .fuser
            .fuse(
                candidates,
                &plan,
                &self.search,
                req.thread_collapse,
                &cancel,
            )
            .await;
        let features = self
            .feature_extractor
            .extract_at(&fused, &plan, now, &cancel)
            .await;

        let Some(cf) = features.iter().find(|cf| cf.message_id == req.message_id) else {
            return Err(Status::from(RmailError::not_found(format!(
                "message {} did not match this query in any retrieval source",
                req.message_id
            ))));
        };

        let score = self.ranker.score(&cf.features, plan.intent);
        let sources: Vec<String> = fused
            .iter()
            .find(|f| f.message_id == req.message_id)
            .map(|f| {
                f.hits
                    .iter()
                    .map(|h| source_name(h.source).to_owned())
                    .collect()
            })
            .unwrap_or_default();
        let matched = self.matched_snippet(req.message_id, &plan, &cancel).await;

        // No `claude_reason`: `Explain` re-derives one message's ranking
        // rationale and deliberately does not run Stage 5 (see the module
        // docs on why it re-derives rather than replays). A listwise reason
        // is a property of one *page* of results — it says why this message
        // beat the others it was ranked against — so synthesizing one here,
        // for a message explained on its own, would be inventing a
        // comparison that never happened.
        Ok(Response::new(self.explanation(
            cf,
            plan.intent,
            score,
            sources,
            matched,
            String::new(),
        )))
    }

    #[tracing::instrument(skip(self, request), fields(queries, corpus))]
    async fn evaluate(
        &self,
        request: Request<EvaluateRequest>,
    ) -> Result<Response<ProtoEvalReport>, Status> {
        let req = request.into_inner();
        tracing::Span::current()
            .record("queries", req.queries.len())
            .record("corpus", req.corpus.as_str());

        // A one-shot report, not a session-shaped stream: `Evaluate` neither
        // joins nor cancels the Search/Semantic generation slot, for the
        // same reason `Explain` does not. An eval run superseding an
        // interactive search — or being superseded by one — would be a
        // surprising interaction between a background measurement and a
        // user's search box.
        let cancel = self.shutdown.child_token();

        let set = GoldenSet {
            version: rmail_core::eval::golden::SCHEMA_VERSION,
            corpus: req.corpus,
            queries: req
                .queries
                .into_iter()
                .map(|q| CoreGoldenQuery {
                    name: q.name,
                    query: q.query,
                    account_id: q.account_id,
                    judgments: q
                        .judgments
                        .into_iter()
                        .map(|j| JudgedMessage {
                            message_id: j.message_id,
                            // proto3 cannot tell an absent scalar from a zero
                            // one, so a `gain` of 0 on the wire is "unset"
                            // and means plainly relevant. A golden set that
                            // genuinely wanted to mark something irrelevant
                            // would omit the judgment entirely — there is no
                            // reason to enumerate non-answers.
                            gain: if j.gain == 0 { 1 } else { j.gain },
                        })
                        .collect(),
                })
                .collect(),
        };
        // Surfaces a malformed request as INVALID_ARGUMENT with the specific
        // violation, rather than letting it become a mystery zero downstream.
        set.validate().map_err(Status::from)?;

        let limit = if req.limit == 0 {
            RECALL_K
        } else {
            req.limit as usize
        };
        let search = EvalSearch {
            api: self,
            mode: resolve_mode(req.mode, self.search.default_mode),
            cancel,
        };

        let report = Evaluator::new(self.db.clone())
            .with_limit(limit)
            .run(&set, &search)
            .await
            .map_err(Status::from)?;

        tracing::info!(
            corpus = %report.corpus,
            queries = report.per_query.len(),
            ndcg_at_10 = report.aggregate.ndcg_at_10,
            mrr = report.aggregate.mrr,
            recall_at_50 = report.aggregate.recall_at_50,
            p_at_3 = report.aggregate.p_at_3,
            "golden set evaluated"
        );

        Ok(Response::new(ProtoEvalReport {
            corpus: report.corpus,
            aggregate: Some(to_proto_metrics(&report.aggregate)),
            per_query: report
                .per_query
                .into_iter()
                .map(|q| ProtoQueryEval {
                    name: q.name,
                    query: q.query,
                    metrics: Some(to_proto_metrics(&q.metrics)),
                    returned: u32::try_from(q.returned).unwrap_or(u32::MAX),
                    relevant: u32::try_from(q.relevant).unwrap_or(u32::MAX),
                    unresolved: q.unresolved,
                })
                .collect(),
        }))
    }

    #[tracing::instrument(skip(self, request), fields(query_id, actions))]
    async fn log_feedback(
        &self,
        request: Request<FeedbackRequest>,
    ) -> Result<Response<()>, Status> {
        let req = request.into_inner();
        tracing::Span::current()
            .record("query_id", req.query_id)
            .record("actions", req.actions.len());

        // Decoded before the store is called, so a malformed enum or a
        // missing `dwell_ms` is an INVALID_ARGUMENT naming the offending
        // action rather than a generic failure from three layers down.
        let actions = req
            .actions
            .iter()
            .map(decode_action)
            .collect::<Result<Vec<_>, Status>>()?;

        // Every domain error maps through the one `Error -> Status`
        // conversion the whole daemon shares: INVALID_ARGUMENT for a
        // malformed batch, NOT_FOUND for a `query_id` this daemon never
        // logged (or that retention has since dropped). Opting out is
        // neither — `log_actions` returns `Ok(0)` without writing, and the
        // caller sees a plain success.
        let written = self
            .feedback
            .log_actions(req.query_id, &actions)
            .await
            .map_err(Status::from)?;
        tracing::debug!(written, "recorded search feedback");

        Ok(Response::new(()))
    }
}

/// Translate one wire action into the domain's own, rejecting anything
/// outside prd.md's vocabulary.
///
/// `FEEDBACK_ACTION_UNSPECIFIED` and an unrecognized enum number are both
/// refused rather than defaulted to `open`: a client that sent the wrong
/// number is a client whose whole batch is suspect, and inventing a plausible
/// action for it would write a training label nobody asked for.
fn decode_action(action: &ProtoResultAction) -> Result<FeedbackActionRecord, Status> {
    let kind = match ProtoFeedbackAction::try_from(action.action)
        .unwrap_or(ProtoFeedbackAction::Unspecified)
    {
        ProtoFeedbackAction::Unspecified => {
            return Err(Status::from(RmailError::invalid_argument(format!(
                "action for message {} is unspecified; feedback with no action is not a signal",
                action.message_id
            ))));
        }
        ProtoFeedbackAction::Open => ActionKind::Open,
        ProtoFeedbackAction::Reply => ActionKind::Reply,
        ProtoFeedbackAction::Archive => ActionKind::Archive,
        ProtoFeedbackAction::Dwell => ActionKind::Dwell,
        ProtoFeedbackAction::ScrollPast => ActionKind::ScrollPast,
    };
    Ok(FeedbackActionRecord {
        message_id: action.message_id,
        kind,
        dwell_ms: action.dwell_ms,
        // proto3 cannot tell an absent scalar from a zero one, and a local
        // client sharing this machine's clock has no reason to stamp its own:
        // 0 means "use the daemon's". A unix timestamp of 0 is 1970, which is
        // not a time any feedback was generated.
        at: if action.at == 0 {
            Utc::now().timestamp()
        } else {
            action.at
        },
    })
}

/// Send one stream item, giving up if the client went away, the daemon is
/// stopping, or a fresher request superseded this one. See
/// `mail_service::send` — identical reasoning; duplicated rather than
/// shared because the two streams are independent (same precedent as that
/// module's own `WatchEvents`/`SyncService::watch_events` split).
async fn send<T>(
    tx: &tokio::sync::mpsc::Sender<Result<T, Status>>,
    cancel: &CancellationToken,
    item: Result<T, Status>,
) -> ControlFlow<()> {
    tokio::select! {
        // No terminal frame here, unlike the other services' `send` helpers:
        // this stream's producer bails on cancellation at points that never
        // reach `send`, so `stream_search` emits it once at the single place
        // every exit path converges. Emitting it in both would be a duplicate.
        () = cancel.cancelled() => ControlFlow::Break(()),
        sent = tx.send(item) => {
            if sent.is_ok() {
                ControlFlow::Continue(())
            } else {
                ControlFlow::Break(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{clamp_rerank, eval_rerank, search_kind, should_log, Generation};
    use rmail_core::config::Rerank;
    use rmail_core::rank::l2::SearchKind;
    use tokio_util::sync::CancellationToken;

    /// A superseded stream contributes nothing to the training corpus, even
    /// when it had already put hits on the wire.
    ///
    /// Pinned here rather than only end-to-end because the integration path
    /// cannot force the interesting case: cancellation usually cuts a stream
    /// at one of `run_stream`'s checkpoints *before* the first hit is sent,
    /// and a page with no impressions is dropped by the second term anyway —
    /// so an end-to-end assertion passes whether or not the cancellation rule
    /// exists. This one fails the moment the `!cancelled` term is dropped.
    #[test]
    fn a_cancelled_stream_logs_nothing_even_after_sending_hits() {
        assert!(
            !should_log(true, 25),
            "a superseded stream must not log the page it had already sent"
        );
        assert!(
            should_log(false, 25),
            "an ordinary completed page is logged"
        );
    }

    /// A query that showed nothing has nothing to learn from, cancelled or
    /// not — a bare `search_log` row would only record what was searched for.
    #[test]
    fn a_page_that_showed_nothing_is_not_logged() {
        assert!(!should_log(false, 0));
        assert!(!should_log(true, 0));
    }

    /// The escalation guard behind `auth::methods`' claim that `Search` is
    /// safe at `mail.read`: a request can turn reranking *down* from any
    /// configuration, and can never turn it up to a provider call the
    /// operator did not already enable.
    #[test]
    fn a_request_can_only_reduce_the_configured_rerank() {
        for configured in [
            Rerank::Off,
            Rerank::CrossEncoder,
            Rerank::Claude,
            Rerank::Auto,
        ] {
            assert_eq!(clamp_rerank(Rerank::Off, configured), Rerank::Off);
            assert_eq!(
                clamp_rerank(Rerank::CrossEncoder, configured),
                Rerank::CrossEncoder
            );
        }

        // Escalation is refused and falls back to the configured backend.
        assert_eq!(clamp_rerank(Rerank::Claude, Rerank::Off), Rerank::Off);
        assert_eq!(
            clamp_rerank(Rerank::Claude, Rerank::CrossEncoder),
            Rerank::CrossEncoder
        );
        assert_eq!(clamp_rerank(Rerank::Auto, Rerank::Off), Rerank::Off);
        assert_eq!(
            clamp_rerank(Rerank::Auto, Rerank::CrossEncoder),
            Rerank::CrossEncoder
        );

        // ...and honored where the configuration already reaches Claude.
        assert_eq!(clamp_rerank(Rerank::Claude, Rerank::Claude), Rerank::Claude);
        assert_eq!(clamp_rerank(Rerank::Claude, Rerank::Auto), Rerank::Claude);
        assert_eq!(clamp_rerank(Rerank::Auto, Rerank::Auto), Rerank::Auto);
    }

    /// A golden-set sweep must never bill one provider call per judged query.
    #[test]
    fn eval_never_resolves_to_the_hosted_backend() {
        assert_eq!(eval_rerank(Rerank::Off), Rerank::Off);
        assert_eq!(eval_rerank(Rerank::CrossEncoder), Rerank::CrossEncoder);
        assert_eq!(eval_rerank(Rerank::Claude), Rerank::CrossEncoder);
        assert_eq!(eval_rerank(Rerank::Auto), Rerank::CrossEncoder);
    }

    #[test]
    fn deep_is_what_auto_resolves_against() {
        assert_eq!(search_kind(true), SearchKind::Deep);
        assert_eq!(search_kind(false), SearchKind::Interactive);
    }

    /// The mechanism the module docs' "Cancellation" section describes,
    /// proven in isolation (no database, no gRPC harness): a fresh
    /// generation cancels the previous one's token and leaves its own live.
    /// The integration-level proof that this actually halts an in-flight
    /// scan (not just discards its output) lives in
    /// `rmaild/tests/search_service.rs`, which needs the real pipeline this
    /// unit test deliberately does not.
    #[test]
    fn a_fresh_generation_cancels_the_previous_one() {
        let shutdown = CancellationToken::new();
        let generation = Generation::default();

        let first = generation.begin(&shutdown);
        assert!(!first.is_cancelled());

        let second = generation.begin(&shutdown);
        assert!(
            first.is_cancelled(),
            "the fresh call must cancel the prior stream's token"
        );
        assert!(
            !second.is_cancelled(),
            "the fresh call's own token must stay live"
        );

        let third = generation.begin(&shutdown);
        assert!(second.is_cancelled());
        assert!(!third.is_cancelled());
    }

    /// The feedback log has exactly one door, and it only opens inward.
    ///
    /// prd.md's learning loop is "local telemetry, never transmitted," and a
    /// serialized feature vector is a behavioural fingerprint of the user's
    /// mailbox — so the property worth defending is not "we do not currently
    /// upload it" (nothing does) but "there is no RPC that hands it out."
    /// That cannot be proven by inspecting this file, because the way it
    /// would be lost is somebody adding an `ExportFeedback`/`GetImpressions`
    /// RPC in a later task and nobody connecting it to this promise.
    ///
    /// So the check is against the compiled descriptor set: this is the
    /// complete list of `SearchService` RPCs, and adding one fails here by
    /// name. That makes this deliberately a changes-detector, which is the
    /// point — a new RPC on this service is a decision about egress, and this
    /// test is where that decision gets made rather than assumed. Extending
    /// the list is the correct fix, once the new RPC has been checked against
    /// the sentence above.
    #[test]
    fn no_search_rpc_reads_the_feedback_log_back_out() {
        use prost::Message as _;

        const EXPECTED: &[&str] = &[
            // Read-only over the index; none of them touch the feedback
            // tables at all.
            "Search",
            "Semantic",
            "Explain",
            "Evaluate",
            // Read-only over the attachment index. Its handler never reaches
            // `self.feedback`, and `attach::search` reads only
            // fts_attachments / vec_chunks / attachment_* / index_content /
            // mailboxes — no feedback table is in any of its queries.
            "SearchAttachments",
            // Write-only, into the log. Returns `google.protobuf.Empty` —
            // it reports nothing back about what is stored.
            "LogFeedback",
            // Compiles one sentence into a query string (task 58). It reads
            // `query_plan_cache` and writes `ai_ledger`; no feedback table
            // appears in either, and its response carries only the caller's
            // own question and the plan derived from it. The one thing worth
            // stating explicitly, because this RPC *does* egress: what leaves
            // the machine is the sentence the caller just typed, never
            // anything the daemon observed about past searches.
            "CompileQuery",
            // Task 73. Reads `entities`/`entity_mentions` joined to
            // `messages`; it names none of `search_log`, `search_impression`
            // or `search_action`, so the feedback log stays unreadable over
            // the wire. It reaches no provider either.
            "SearchEntities",
        ];

        let set = prost_types::FileDescriptorSet::decode(rmail_proto::FILE_DESCRIPTOR_SET)
            .expect("the compiled descriptor set must decode");
        let mut found: Vec<String> = Vec::new();
        for file in &set.file {
            for service in &file.service {
                if file.package() == "rmail.v1" && service.name() == "SearchService" {
                    found.extend(service.method.iter().map(|m| m.name().to_owned()));
                }
            }
        }
        found.sort();
        let mut expected: Vec<String> = EXPECTED.iter().map(|m| (*m).to_owned()).collect();
        expected.sort();

        assert_eq!(
            found, expected,
            "SearchService's RPC list changed. If the new RPC can return \
             anything from search_log/search_impression/search_action, it \
             breaks prd.md's 'never transmitted' promise for the feedback \
             log; if it cannot, add it to EXPECTED."
        );
    }

    #[test]
    fn daemon_shutdown_cancels_every_generation_token() {
        let shutdown = CancellationToken::new();
        let generation = Generation::default();
        let token = generation.begin(&shutdown);
        assert!(!token.is_cancelled());
        shutdown.cancel();
        assert!(token.is_cancelled());
    }
}
