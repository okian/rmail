//! The `SearchService` gRPC implementation: the wiring that finally makes
//! the search pipeline (tasks 26-32) reachable — `query::QueryPlanner` ->
//! `retrieve::Fanout` -> `fuse::Fuser` -> `features::FeatureExtractor` ->
//! `rank::l1::L1Ranker` -> `present::Presenter`, streamed back as
//! [`SearchHit`](rmail_proto::v1::SearchHit)s.
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
use rmail_core::config::{IndexSemanticConfig, RetrieversConfig, SearchConfig, SearchMode};
use rmail_core::embed::Embedder;
use rmail_core::features::{CandidateFeatures, FeatureExtractor};
use rmail_core::fuse::{FusedCandidate, Fuser};
use rmail_core::index::fts::FtsIndex;
use rmail_core::index::semantic::SemanticIndex;
use rmail_core::present::{PresentedResult, Presenter};
use rmail_core::query::{Intent, QueryPlan, QueryPlanner};
use rmail_core::rank::l1::{L1Ranker, Weights};
use rmail_core::rank::Ranker;
use rmail_core::retrieve::{Candidate, DenseRetriever, Fanout, Source};
use rmail_core::{present, repo, Database, Error as RmailError};
use rmail_proto::v1::search_service_server::SearchService;
use rmail_proto::v1::{
    ByteRange as ProtoByteRange, ExplainRequest, FeatureContribution as ProtoFeatureContribution,
    Intent as ProtoIntent, Message as ProtoMessage, Mode as ProtoMode,
    RankExplanation as ProtoRankExplanation, SearchHit as ProtoSearchHit, SearchRequest,
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

fn to_proto_message(message: &repo::Message, flags: Vec<String>) -> ProtoMessage {
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
    planner: QueryPlanner,
    fuser: Fuser,
    feature_extractor: FeatureExtractor,
    ranker: L1Ranker,
    presenter: Presenter,
    search: SearchConfig,
    /// Cancelled when the daemon shuts down, so every generation's token —
    /// and therefore every open stream — stops with it. Same pattern as
    /// `MailApi`/`SyncApi`.
    shutdown: CancellationToken,
    generation: Generation,
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
    #[must_use]
    pub fn new(
        db: Database,
        embedder: Arc<dyn Embedder>,
        weights: Weights,
        search: SearchConfig,
        semantic_config: &IndexSemanticConfig,
        shutdown: CancellationToken,
    ) -> Self {
        let fts = FtsIndex::new(db.clone(), search.bm25_weights.clone());
        let semantic_index = SemanticIndex::new(db.clone(), Arc::clone(&embedder), semantic_config);
        let dense = search
            .retrievers
            .dense
            .then(|| DenseRetriever::new(db.clone(), &semantic_index));
        let planner = QueryPlanner::new(db.clone(), embedder, search.expansion.clone());
        let fuser = Fuser::new(db.clone());
        let feature_extractor = FeatureExtractor::new(
            db.clone(),
            search.bm25_weights.clone(),
            search.retrievers.recency_half_life_days,
        );
        let ranker = L1Ranker::new(weights);
        let presenter = Presenter::new(db.clone());

        Self {
            db,
            fts,
            semantic_index,
            dense,
            planner,
            fuser,
            feature_extractor,
            ranker,
            presenter,
            search,
            shutdown,
            generation: Generation::default(),
        }
    }

    /// Kick off a `Search`/`Semantic` stream: register it as the current
    /// generation (cancelling whichever stream held the slot before),
    /// spawn the pipeline as a background task, and return the response
    /// stream immediately — the client sees this call resolve before any
    /// planning/retrieval work has even started, which is what makes the
    /// generation-token race in the module docs' "Cancellation" section
    /// actually favor the newer request.
    async fn start_stream(
        &self,
        req: SearchRequest,
        dense_only: bool,
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
        let cancel = self.generation.begin(&self.shutdown);
        let (tx, rx) = tokio::sync::mpsc::channel(STREAM_BUFFER);
        let this = self.clone();
        tokio::spawn(
            async move {
                this.run_stream(req, dense_only, cancel, tx).await;
            }
            .instrument(tracing::Span::current()),
        );
        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    /// Run the pipeline end to end and stream every hit it produces — the
    /// body of both `Search` and `Semantic`. See the module docs' "Streaming
    /// the first hit" section for the two-phase `present` call.
    #[tracing::instrument(skip(self, req, cancel, tx), fields(dense_only, explain = req.explain))]
    async fn run_stream(
        &self,
        req: SearchRequest,
        dense_only: bool,
        cancel: CancellationToken,
        tx: tokio::sync::mpsc::Sender<Result<ProtoSearchHit, Status>>,
    ) {
        let now = Utc::now();

        let query = match self
            .effective_query(&req.query, &req.filter, req.account_id)
            .await
        {
            Ok(query) => query,
            Err(status) => {
                let _ = send(&tx, &cancel, Err(status)).await;
                return;
            }
        };

        let mut plan = match self.planner.plan_at(&query, now).await {
            Ok(plan) => plan,
            Err(error) => {
                let _ = send(&tx, &cancel, Err(Status::from(error))).await;
                return;
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
            return;
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
            return;
        }

        let features = self
            .feature_extractor
            .extract_at(&fused, &plan, now, &cancel)
            .await;
        let ranked = self
            .ranker
            .rank(&features, plan.intent, self.search.top_k_rerank as usize);
        if ranked.is_empty() || cancel.is_cancelled() {
            return;
        }

        let limit = if req.limit == 0 {
            self.search.default_limit as usize
        } else {
            req.limit as usize
        };
        let lambda = self.search.mmr_lambda;

        // Phase 1: the single best-scoring candidate, presented and flushed
        // alone — see the module docs for why this is provably the same
        // message the full-page call below would also pick first.
        let head = &ranked[..1];
        let first_presented = self
            .presenter
            .present(head, &fused, &plan, lambda, 1, &cancel)
            .await;
        let first_hits = self
            .build_hits(&first_presented, &fused, &features, &plan, req.explain)
            .await;
        let mut sent: BTreeSet<i64> = BTreeSet::new();
        for (id, hit) in first_hits {
            sent.insert(id);
            if send(&tx, &cancel, Ok(hit)).await.is_break() {
                return;
            }
        }
        if cancel.is_cancelled() {
            return;
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
        let rest_hits = self
            .build_hits(&rest, &fused, &features, &plan, req.explain)
            .await;
        for (_, hit) in rest_hits {
            if send(&tx, &cancel, Ok(hit)).await.is_break() {
                return;
            }
        }
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
    /// Returns `(message_id, hit)` pairs, in `presented`'s own order, so a
    /// caller can both stream them and track which ids it already sent.
    ///
    /// A `message_id` [`Self::fetch_messages`] found no row for (deleted
    /// between ranking and now) is silently dropped — a hit with no
    /// `Message` payload is not something any client can render.
    async fn build_hits(
        &self,
        presented: &[PresentedResult],
        fused: &[FusedCandidate],
        features: &[CandidateFeatures],
        plan: &QueryPlan,
        explain: bool,
    ) -> Vec<(i64, ProtoSearchHit)> {
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
            let why = if explain {
                features_by_id.get(&p.message_id).map(|cf| {
                    self.explanation(
                        cf,
                        plan.intent,
                        p.score,
                        sources.clone(),
                        Some(snippet.clone()),
                    )
                })
            } else {
                None
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
            // No L2 rerank stage exists yet (prd.md Stage 5 — task 51); this
            // stays empty rather than a placeholder string until it does.
            claude_reason: String::new(),
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

#[tonic::async_trait]
impl SearchService for SearchApi {
    type SearchStream =
        Pin<Box<dyn tokio_stream::Stream<Item = Result<ProtoSearchHit, Status>> + Send + 'static>>;

    async fn search(
        &self,
        request: Request<SearchRequest>,
    ) -> Result<Response<Self::SearchStream>, Status> {
        self.start_stream(request.into_inner(), false).await
    }

    type SemanticStream =
        Pin<Box<dyn tokio_stream::Stream<Item = Result<ProtoSearchHit, Status>> + Send + 'static>>;

    async fn semantic(
        &self,
        request: Request<SearchRequest>,
    ) -> Result<Response<Self::SemanticStream>, Status> {
        self.start_stream(request.into_inner(), true).await
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

        Ok(Response::new(self.explanation(
            cf,
            plan.intent,
            score,
            sources,
            matched,
        )))
    }
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
    use super::Generation;
    use tokio_util::sync::CancellationToken;

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
