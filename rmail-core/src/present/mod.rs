//! Stage 6 — Diversify & Present (prd.md, "Stage 6 — Diversify + Present"):
//! turning task 31's best-first [`RankedCandidate`] list into what a client
//! actually renders — a diversified, thread-grouped, near-dup-annotated,
//! snippet-and-highlight-carrying list, cut into streaming-ready batches.
//!
//! # Four responsibilities, three of them thin
//!
//! - [`mmr`] — the one genuinely algorithmic piece: greedy Maximal Marginal
//!   Relevance, gated by intent.
//! - Thread grouping and near-duplicate chips are **not** recomputed here.
//!   [`crate::fuse::FusedCandidate`] already carries `thread_id`,
//!   `thread_collapsed`, and `near_duplicates` from Stage 2 — this module's
//!   whole job for those two acceptance bullets is to join a
//!   [`RankedCandidate`] back to the [`FusedCandidate`] Stage 2 computed for
//!   the same `message_id` and carry the three fields through.
//!   `thread_collapsed.len()` *is* the "+N in thread" affordance;
//!   `near_duplicates.len()` *is* the "N similar" chip count. Recomputing
//!   either here would be a second, competing definition of "duplicate" —
//!   Stage 2 already decided, this stage only has to remember what it said.
//! - [`snippet`] — best-matching span extraction with highlight offsets.
//! - [`batching`] — cutting the final list into streaming-ready pages.
//!
//! # `PresentedResult::score` is Stage 4's relevance, unchanged by MMR — and
//! # a diversified list is not guaranteed monotonic in it
//!
//! [`RankedCandidate::score`](crate::rank::RankedCandidate::score) carries a
//! specific warning in its own doc comment: it means something only within
//! the run that produced it, and is never persisted or shown as if it meant
//! something on its own. [`PresentedResult::score`] is that same number,
//! carried through unmodified — MMR's own per-step objective
//! (`λ·relevance − (1−λ)·max_similarity_to_already_picked`) is a *selection*
//! criterion, not a replacement relevance score a user or task 33's `Explain`
//! should ever see, exactly the way `fuse::fuse_scores`'s RRF sum is a
//! ranking mechanism and not something `--explain` shows on its own either.
//!
//! This has a real consequence: **diversification means a diversified list
//! is not required to be non-increasing in `score`.** A lower-scored but
//! topically distinct result outranking a higher-scored but redundant one is
//! not a bug — it is the entire point of running MMR at all (prd.md: "so the
//! top-10 isn't ten near-identical newsletters"). The batching/streaming
//! order guarantee this module *does* make (see [`batching`]'s own docs) is
//! narrower and unconditional: whatever order [`Presenter::present`] decided
//! is preserved intact across batch boundaries, with nothing dropped,
//! reordered, or repeated. For [`crate::query::Intent::Navigational`]/[`crate::query::Intent::Lookup`]
//! (MMR disabled), that order *is* strict score order, and this module's own
//! tests pin exactly that case — see `tests::navigational_batches_are_strict_score_order`.
//! For [`crate::query::Intent::Exploratory`], "best-first" means "in the order MMR decided
//! is the best trade-off of relevance against diversity," not "sorted by
//! `score`."
//!
//! # Why this module needs a database at all
//!
//! Everything upstream of this stage — [`RankedCandidate`],
//! [`crate::fuse::FusedCandidate`] — is pure, in-memory data. This module is
//! the first one back at Stage 6 that has to read message bodies again: MMR
//! needs a similarity signal between candidates ([`mmr`]'s own docs explain
//! why that is a [`crate::fuse::simhash`] fingerprint), and snippet
//! extraction needs the actual text to excerpt from. [`Presenter`] batches
//! both reads into the same query (see [`Presenter::fetch_meta`]) so
//! computing one does not cost a second round trip for the other.

pub mod batching;
pub mod mmr;
pub mod snippet;

use std::collections::BTreeMap;

use tokio_util::sync::CancellationToken;

use crate::embed::Embedding;
use crate::fuse::simhash;
use crate::fuse::FusedCandidate;
use crate::index::semantic::VECTOR_DIM;
use crate::query::QueryPlan;
use crate::rank::RankedCandidate;
use crate::retrieve::cancel::interruptible_read;
use crate::storage::Database;

pub use snippet::Snippet;

/// One Stage 6 result: a ranked candidate plus everything a client renders
/// alongside it.
///
/// Deliberately flat rather than nesting a `FusedCandidate`/`RankedCandidate`
/// inside — task 33's `SearchHit` and task 34's `--json` output both
/// serialize this shape close to verbatim, and neither wants to reach
/// through two levels of struct to find `thread_id`.
#[derive(Debug, Clone, PartialEq)]
pub struct PresentedResult {
    /// The matched message (`messages.id`).
    pub message_id: i64,
    /// Stage 4's relevance score, unchanged by MMR — see the module docs.
    pub score: f64,
    /// This message's thread, when Stage 2 ran thread collapse and the
    /// message has one. `None` under the identical conditions
    /// [`FusedCandidate::thread_id`] is `None`.
    pub thread_id: Option<i64>,
    /// Sibling messages from the same thread, collapsed into this result by
    /// Stage 2 — `thread_collapsed.len()` is the "+N in thread" affordance.
    /// Empty when thread collapse did not run or this thread had only one
    /// surviving candidate.
    pub thread_collapsed: Vec<i64>,
    /// Sibling messages whose SimHash fingerprint Stage 2 found within
    /// [`simhash::NEAR_DUP_HAMMING_THRESHOLD`] of this one's —
    /// `near_duplicates.len()` is the "N similar" chip count.
    pub near_duplicates: Vec<i64>,
    /// The best-matching excerpt, with highlight offsets — see [`snippet`].
    pub snippet: Snippet,
}

/// Characters of a message's body fetched for both MMR fingerprinting and
/// lexical snippet extraction — one shared cap serves both, since both read
/// [`Presenter::fetch_meta`]'s same batched fetch. Matches the
/// `snippet::MAX_SOURCE_CHARS`/`fuse::MAX_BODY_CHARS_FOR_SIMHASH` convention
/// (`4_000`) exactly, so the text this module fingerprints is the same text
/// it would extract a snippet from.
const MAX_BODY_CHARS: i64 = snippet::MAX_SOURCE_CHARS as i64;

/// Largest number of candidates one [`Presenter::present`] call fetches
/// metadata for. Mirrors `fuse::MAX_META_FETCH`/
/// `features::extract::MAX_FEATURE_BATCH`'s identical defensive ceiling —
/// `ranked` is already Stage 4's own top-K (`search.top_k_rerank`, default
/// `50`), so this is a backstop against a pathological config, not a limit
/// this stage expects to ever hit.
const MAX_META_FETCH: usize = 2_000;

/// One message's body, as much as [`Presenter::fetch_meta`] fetched.
#[derive(Debug, Clone, Default)]
struct PresentMeta {
    body: Option<String>,
}

/// Stage 6: diversify, group, annotate, and snippet a ranked candidate list.
///
/// Cheap to clone: shares a database handle, the same pattern
/// [`crate::fuse::Fuser`]/[`crate::features::FeatureExtractor`] both use.
#[derive(Debug, Clone)]
pub struct Presenter {
    db: Database,
}

impl Presenter {
    /// Build a presenter over `db`.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Run Stage 6 end to end.
    ///
    /// `ranked` is Stage 4's output — already scored and cut to its own
    /// top-K, best-first (see [`crate::rank::Ranker::rank`]'s contract).
    /// `fused` is Stage 2's output for the *same* query, read for its
    /// `thread_id`/`thread_collapsed`/`near_duplicates` metadata only — a
    /// `message_id` present in `ranked` but absent from `fused` (should not
    /// happen in the real pipeline, since Stage 4 only ever scores Stage
    /// 2/3's own candidates, but is not assumed) simply gets no thread/near-
    /// dup annotation rather than being dropped.
    ///
    /// Intent is read from `plan.intent` rather than taken as its own
    /// parameter — unlike [`crate::rank::Ranker::rank`] (which has no
    /// `QueryPlan` of its own to read one from at all, only bare
    /// [`crate::features::CandidateFeatures`]), this method already requires
    /// `plan` for its `raw`/`query_vector` fields, so a second, separately-
    /// passed `intent` would just be a second copy of `plan.intent` a caller
    /// could hand over out of sync with the first. `lambda` *is* still its
    /// own parameter, not read from [`crate::config::SearchConfig`]
    /// internally — the same "pure function of its arguments" contract
    /// `Ranker::rank` itself keeps for `top_k`, for the identical reason: a
    /// test should not need to construct a `SearchConfig` fixture just to
    /// call this method. `limit` is the final number of results to present
    /// (task 33's `SearchRequest.limit`, or `search.default_limit` when the
    /// caller has none of its own) — MMR (when enabled) both diversifies and
    /// truncates to `limit` in one pass; [`crate::query::Intent::Navigational`]/
    /// [`crate::query::Intent::Lookup`] simply keep the top `limit` in score order.
    ///
    /// Never fails: a metadata lookup that errors or is cancelled degrades
    /// the step it feeds (MMR runs with no similarity signal at all — every
    /// candidate reads as maximally diverse — and every snippet falls back
    /// to a plain excerpt) rather than failing the whole search, the same
    /// graceful-degradation contract [`crate::fuse::Fuser::fuse`] and
    /// [`crate::features::FeatureExtractor::extract_at`] both already give
    /// their callers.
    // `limit` is deliberately *not* listed in `fields(...)`: it names a
    // parameter, and `tracing::instrument` only auto-records an argument
    // into the span when nothing else claims that field name — a bare
    // identifier in `fields(...)` instead declares an *empty* slot the
    // function must fill itself via `Span::record`, which this function
    // never does for `limit` (it never changes, so there is nothing to
    // record after the fact the way `presented` needs to be). Listing it
    // there anyway would silently leave it `Empty` on every request.
    #[tracing::instrument(
        skip(self, ranked, fused, plan, cancel),
        fields(
            candidates = ranked.len(),
            intent = ?plan.intent,
            mmr = mmr::enabled_for(plan.intent),
            presented
        )
    )]
    pub async fn present(
        &self,
        ranked: &[RankedCandidate],
        fused: &[FusedCandidate],
        plan: &QueryPlan,
        lambda: f64,
        limit: usize,
        cancel: &CancellationToken,
    ) -> Vec<PresentedResult> {
        if ranked.is_empty() || limit == 0 {
            return Vec::new();
        }

        let ids: Vec<i64> = ranked.iter().map(|c| c.message_id).collect();
        let meta = self.fetch_meta(&ids, cancel).await;

        // A query superseded while `fetch_meta` was in flight: skip the two
        // steps that exist purely to make the *ordering*/*fallback text*
        // better (MMR's fingerprinting pass, the semantic best-chunk
        // lookup) rather than spend CPU and a second round trip on a result
        // this caller has already moved on from. `meta` itself is still
        // used below — whatever landed before cancellation still gives a
        // real snippet instead of an unnecessary plain excerpt.
        let cancelled = cancel.is_cancelled();

        let selected: Vec<RankedCandidate> = if !cancelled && mmr::enabled_for(plan.intent) {
            let fingerprints = self.fingerprint_batch(ranked, &meta).await;
            mmr::diversify(ranked, &fingerprints, lambda, limit)
        } else {
            strict_score_order(ranked, limit)
        };

        let fused_by_id: BTreeMap<i64, &FusedCandidate> =
            fused.iter().map(|f| (f.message_id, f)).collect();

        let query = snippet::query_terms(&plan.raw);
        let lexical = self.lexical_snippets(&selected, &meta, &query).await;
        let needs_chunk: Vec<i64> = selected
            .iter()
            .filter(|c| lexical.get(&c.message_id).map_or(true, Option::is_none))
            .map(|c| c.message_id)
            .collect();
        let chunk_text = match (&plan.query_vector, needs_chunk.is_empty(), cancelled) {
            (Some(query_vector), false, false) => {
                self.fetch_best_chunks(&needs_chunk, query_vector, cancel)
                    .await
            }
            _ => BTreeMap::new(),
        };
        let snippets = self
            .finalize_snippets(&selected, &meta, lexical, chunk_text, &query)
            .await;

        let out: Vec<PresentedResult> = selected
            .into_iter()
            .map(|candidate| {
                let f = fused_by_id.get(&candidate.message_id).copied();
                PresentedResult {
                    message_id: candidate.message_id,
                    score: candidate.score,
                    thread_id: f.and_then(|f| f.thread_id),
                    thread_collapsed: f.map(|f| f.thread_collapsed.clone()).unwrap_or_default(),
                    near_duplicates: f.map(|f| f.near_duplicates.clone()).unwrap_or_default(),
                    snippet: snippets
                        .get(&candidate.message_id)
                        .cloned()
                        .unwrap_or_default(),
                }
            })
            .collect();

        tracing::Span::current().record("presented", out.len());
        out
    }

    /// The lexical (body-window) snippet for every candidate in `selected`
    /// — `None` where the body carried no literal query-term/phrase match
    /// at all, the caller's cue to try the semantic best-chunk fallback.
    ///
    /// Computed **once** per candidate rather than twice (once to decide
    /// [`needs_chunk`](Self::present)'s membership, again to build the
    /// final result) — [`snippet::extract`] tokenizes and scans up to
    /// [`snippet::MAX_SOURCE_CHARS`] of text per call, real CPU across up to
    /// `search.top_k_rerank` (default 50) candidates, so this runs on the
    /// blocking pool for the same "never block the runtime" reason
    /// [`fingerprint_batch`](Self::fingerprint_batch) does.
    async fn lexical_snippets(
        &self,
        selected: &[RankedCandidate],
        meta: &BTreeMap<i64, PresentMeta>,
        query: &snippet::QueryTerms,
    ) -> BTreeMap<i64, Option<Snippet>> {
        let bodies: Vec<(i64, String)> = selected
            .iter()
            .map(|c| {
                let body = meta
                    .get(&c.message_id)
                    .and_then(|m| m.body.clone())
                    .unwrap_or_default();
                (c.message_id, body)
            })
            .collect();
        let terms = query.terms.clone();
        let phrases = query.phrases.clone();
        match tokio::task::spawn_blocking(move || {
            bodies
                .into_iter()
                .map(|(id, body)| (id, snippet::extract(&body, &terms, &phrases)))
                .collect()
        })
        .await
        {
            Ok(snippets) => snippets,
            Err(join_error) => {
                // Degrades to "every candidate needs the chunk/plain-excerpt
                // fallback" — `needs_chunk` reads an absent entry as `None`
                // (see `needs_chunk`'s filter above), so this loses precision (a real
                // lexical hit is not reported as one) but not correctness.
                tracing::warn!(%join_error, "lexical snippet task failed; falling back to chunk/plain excerpts");
                BTreeMap::new()
            }
        }
    }

    /// Combine [`lexical_snippets`](Self::lexical_snippets)' output with the
    /// best-chunk text [`fetch_best_chunks`](Self::fetch_best_chunks) found
    /// into the final per-candidate [`Snippet`]: the lexical hit if there
    /// was one, else a chunk-text extraction (still checking for a literal
    /// match, in case the chunk happens to contain one) or a plain excerpt
    /// of the chunk, else a plain excerpt of the body itself.
    ///
    /// Runs on the blocking pool for the same reason
    /// [`lexical_snippets`](Self::lexical_snippets) does — the chunk-text
    /// extraction/excerpt calls here are the identical CPU-bound work, just
    /// over the smaller `needs_chunk` subset.
    async fn finalize_snippets(
        &self,
        selected: &[RankedCandidate],
        meta: &BTreeMap<i64, PresentMeta>,
        lexical: BTreeMap<i64, Option<Snippet>>,
        chunk_text: BTreeMap<i64, String>,
        query: &snippet::QueryTerms,
    ) -> BTreeMap<i64, Snippet> {
        let bodies: Vec<(i64, String)> = selected
            .iter()
            .map(|c| {
                let body = meta
                    .get(&c.message_id)
                    .and_then(|m| m.body.clone())
                    .unwrap_or_default();
                (c.message_id, body)
            })
            .collect();
        let terms = query.terms.clone();
        let phrases = query.phrases.clone();
        match tokio::task::spawn_blocking(move || {
            bodies
                .into_iter()
                .map(|(id, body)| {
                    let snip = lexical
                        .get(&id)
                        .cloned()
                        .flatten()
                        .or_else(|| {
                            chunk_text.get(&id).map(|chunk| {
                                snippet::extract(chunk, &terms, &phrases)
                                    .unwrap_or_else(|| snippet::plain_excerpt(chunk))
                            })
                        })
                        .unwrap_or_else(|| snippet::plain_excerpt(&body));
                    (id, snip)
                })
                .collect()
        })
        .await
        {
            Ok(snippets) => snippets,
            Err(join_error) => {
                // Should not happen -- neither `snippet::extract` nor
                // `snippet::plain_excerpt` panics -- but a join failure
                // still must degrade rather than lose the whole
                // presentation step: every candidate falls back to
                // `PresentedResult::default`-shaped empty snippets via
                // `present`'s own `.unwrap_or_default()` on this map.
                tracing::warn!(%join_error, "snippet finalization task failed; results carry empty snippets");
                BTreeMap::new()
            }
        }
    }

    /// Batched `body`-text fetch for `ids`, one round trip via a `messages`
    /// ⋈ `index_content` join — the same shape as [`crate::fuse::Fuser::fetch_meta`],
    /// minus the columns this stage does not need (`thread_id`/`date`, both
    /// already answered by the caller's own `FusedCandidate`s).
    async fn fetch_meta(
        &self,
        ids: &[i64],
        cancel: &CancellationToken,
    ) -> BTreeMap<i64, PresentMeta> {
        if ids.is_empty() {
            return BTreeMap::new();
        }
        let capped = if ids.len() > MAX_META_FETCH {
            tracing::warn!(
                len = ids.len(),
                cap = MAX_META_FETCH,
                "ranked candidate set exceeds the metadata fetch cap; tail left unsnippeted"
            );
            &ids[..MAX_META_FETCH]
        } else {
            ids
        };
        let placeholders = placeholder_list(capped.len());
        let sql = format!(
            "SELECT m.id, SUBSTR(ic.text, 1, {MAX_BODY_CHARS}) \
             FROM messages m \
             LEFT JOIN index_content ic ON ic.message_id = m.id AND ic.part = 'body' \
             WHERE m.id IN ({placeholders})"
        );
        let ids_owned = capped.to_vec();
        let result = interruptible_read(&self.db, cancel, move |conn| {
            let mut stmt = conn.prepare(&sql)?;
            let params: Vec<&dyn rusqlite::ToSql> = ids_owned
                .iter()
                .map(|id| id as &dyn rusqlite::ToSql)
                .collect();
            let rows = stmt.query_map(params.as_slice(), |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
        .await;

        match result {
            Ok(Some(rows)) => rows
                .into_iter()
                .map(|(id, body)| (id, PresentMeta { body }))
                .collect(),
            Ok(None) => {
                tracing::debug!("present metadata fetch cancelled; snippets degrade to excerpts");
                BTreeMap::new()
            }
            Err(error) => {
                tracing::warn!(%error, "present metadata fetch failed; snippets degrade to excerpts");
                BTreeMap::new()
            }
        }
    }

    /// SimHash-fingerprint every candidate in `ranked` whose body
    /// [`fetch_meta`](Self::fetch_meta) found — the CPU-bound step MMR's
    /// similarity signal depends on.
    ///
    /// Runs on the blocking pool: fingerprinting up to `search.top_k_rerank`
    /// (default 50) bodies of up to [`snippet::MAX_SOURCE_CHARS`] characters
    /// each is real CPU time, the same "never block the runtime" reasoning
    /// [`crate::fuse::Fuser::fuse`] gives for its own near-duplicate
    /// fingerprinting pass.
    async fn fingerprint_batch(
        &self,
        ranked: &[RankedCandidate],
        meta: &BTreeMap<i64, PresentMeta>,
    ) -> BTreeMap<i64, u64> {
        let bodies: Vec<(i64, String)> = ranked
            .iter()
            .filter_map(|c| {
                meta.get(&c.message_id)
                    .and_then(|m| m.body.clone())
                    .map(|body| (c.message_id, body))
            })
            .collect();
        if bodies.is_empty() {
            return BTreeMap::new();
        }
        match tokio::task::spawn_blocking(move || {
            bodies
                .into_iter()
                .filter_map(|(id, body)| simhash::fingerprint(&body).map(|fp| (id, fp)))
                .collect()
        })
        .await
        {
            Ok(fingerprints) => fingerprints,
            Err(join_error) => {
                // Should not happen — `simhash::fingerprint` has no panics —
                // but a join failure degrades like every other failure mode
                // here: MMR simply runs with no similarity signal (every
                // candidate reads as maximally diverse) rather than losing
                // the whole presentation step.
                tracing::warn!(%join_error, "mmr fingerprinting task failed; diversifying with no similarity signal");
                BTreeMap::new()
            }
        }
    }

    /// For each of `ids`, the text of its own single highest-cosine-
    /// similarity chunk against `query_vector` — prd.md's "best chunk"
    /// fallback for a candidate whose body carried no literal query-term
    /// match at all (a semantic-only hit).
    ///
    /// Deliberately a *per-message* comparison over that message's own
    /// (typically few) chunks, not a fresh global kNN: the dense retriever
    /// (Stage 1) already ran the expensive global search once, and
    /// [`RankedCandidate`] does not carry which chunk it matched forward
    /// (see `rank`'s own module docs on why it stays deliberately thin) — so
    /// this reconstructs "the best chunk for *this* message" from
    /// `plan.query_vector`, which every candidate here already has in
    /// common, rather than re-querying the whole index per result.
    ///
    /// Two round trips, not one: chunk metadata (span/embedding) and part
    /// text are fetched separately and joined in Rust, rather than one
    /// query joining `chunks` ⋈ `index_content`. A message with `n` chunks
    /// in the same part would otherwise repeat that part's *entire* text
    /// `n` times in the result set — `index_content.text` is not capped the
    /// way [`fetch_meta`](Self::fetch_meta)'s own body read is, so an
    /// uncapped body transferred once per chunk is real, avoidable I/O.
    /// Fetching text once per distinct `(message_id, part)` instead, capped
    /// at [`MAX_BODY_CHARS`] like every other body read in this module,
    /// bounds both.
    async fn fetch_best_chunks(
        &self,
        ids: &[i64],
        query_vector: &Embedding,
        cancel: &CancellationToken,
    ) -> BTreeMap<i64, String> {
        if ids.is_empty() {
            return BTreeMap::new();
        }
        let capped = if ids.len() > MAX_META_FETCH {
            tracing::warn!(
                len = ids.len(),
                cap = MAX_META_FETCH,
                "best-chunk candidate set exceeds the metadata fetch cap; tail left unsnippeted"
            );
            &ids[..MAX_META_FETCH]
        } else {
            ids
        };

        let chunk_rows = match self.fetch_chunk_meta(capped, cancel).await {
            Some(rows) if !rows.is_empty() => rows,
            _ => return BTreeMap::new(),
        };

        let part_ids: Vec<i64> = {
            let mut ids: Vec<i64> = chunk_rows.iter().map(|(id, ..)| *id).collect();
            ids.sort_unstable();
            ids.dedup();
            ids
        };
        let part_texts = self.fetch_part_texts(&part_ids, cancel).await;
        if part_texts.is_empty() {
            return BTreeMap::new();
        }

        // Decoding and cosine-comparing every candidate's chunk vectors,
        // and slicing each one's span out of its part's text, is real (if
        // small) CPU work — routed through the blocking pool for the same
        // reason `fingerprint_batch` is.
        let query_vector = query_vector.clone();
        match tokio::task::spawn_blocking(move || {
            best_chunk_per_message(chunk_rows, &part_texts, &query_vector)
        })
        .await
        {
            Ok(best) => best,
            Err(join_error) => {
                tracing::warn!(%join_error, "best-chunk selection task failed; falling back to a plain excerpt");
                BTreeMap::new()
            }
        }
    }

    /// `chunks` ⋈ `vec_chunks` metadata for `ids` — no `index_content` join,
    /// so no part text is fetched (let alone repeated) here; see
    /// [`fetch_best_chunks`](Self::fetch_best_chunks)'s doc comment for why
    /// that is a separate call.
    async fn fetch_chunk_meta(
        &self,
        ids: &[i64],
        cancel: &CancellationToken,
    ) -> Option<Vec<(i64, String, i64, i64, Vec<u8>)>> {
        let placeholders = placeholder_list(ids.len());
        let sql = format!(
            "SELECT c.message_id, c.part, c.span_start, c.span_end, v.embedding \
             FROM chunks c \
             JOIN vec_chunks v ON v.chunk_id = c.chunk_id \
             WHERE c.message_id IN ({placeholders})"
        );
        let ids_owned = ids.to_vec();
        let result = interruptible_read(&self.db, cancel, move |conn| {
            let mut stmt = conn.prepare(&sql)?;
            let params: Vec<&dyn rusqlite::ToSql> = ids_owned
                .iter()
                .map(|id| id as &dyn rusqlite::ToSql)
                .collect();
            let rows = stmt.query_map(params.as_slice(), |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
        .await;

        match result {
            Ok(Some(rows)) => Some(rows),
            Ok(None) => {
                tracing::debug!(
                    "best-chunk metadata lookup cancelled; falling back to a plain excerpt"
                );
                None
            }
            Err(error) => {
                tracing::warn!(%error, "best-chunk metadata lookup failed; falling back to a plain excerpt");
                None
            }
        }
    }

    /// Every part's text for `ids`, capped to [`MAX_BODY_CHARS`] like every
    /// other body read in this module, keyed by `(message_id, part)`. One
    /// row per distinct part regardless of how many chunks reference it —
    /// see [`fetch_best_chunks`](Self::fetch_best_chunks)'s doc comment.
    async fn fetch_part_texts(
        &self,
        ids: &[i64],
        cancel: &CancellationToken,
    ) -> BTreeMap<(i64, String), String> {
        if ids.is_empty() {
            return BTreeMap::new();
        }
        let placeholders = placeholder_list(ids.len());
        let sql = format!(
            "SELECT message_id, part, SUBSTR(text, 1, {MAX_BODY_CHARS}) \
             FROM index_content WHERE message_id IN ({placeholders})"
        );
        let ids_owned = ids.to_vec();
        let result = interruptible_read(&self.db, cancel, move |conn| {
            let mut stmt = conn.prepare(&sql)?;
            let params: Vec<&dyn rusqlite::ToSql> = ids_owned
                .iter()
                .map(|id| id as &dyn rusqlite::ToSql)
                .collect();
            let rows = stmt.query_map(params.as_slice(), |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
        .await;

        match result {
            Ok(Some(rows)) => rows
                .into_iter()
                .map(|(id, part, text)| ((id, part), text))
                .collect(),
            Ok(None) => {
                tracing::debug!(
                    "best-chunk part-text lookup cancelled; falling back to a plain excerpt"
                );
                BTreeMap::new()
            }
            Err(error) => {
                tracing::warn!(%error, "best-chunk part-text lookup failed; falling back to a plain excerpt");
                BTreeMap::new()
            }
        }
    }
}

/// `ranked`, kept in strict best-first score order and truncated to `limit`
/// — [`crate::query::Intent::Navigational`]/[`crate::query::Intent::Lookup`]'s path when MMR is
/// disabled. Re-sorts rather than trusting `ranked` is already sorted: it
/// always is, by [`crate::rank::Ranker::rank`]'s own contract, but a
/// defensive re-sort costs nothing at this size (`search.top_k_rerank`,
/// default 50) and is what makes "navigational returns strict score order"
/// true regardless of what produced `ranked`, not merely true of the one
/// `Ranker` implementation this build ships.
fn strict_score_order(ranked: &[RankedCandidate], limit: usize) -> Vec<RankedCandidate> {
    let mut sorted = ranked.to_vec();
    sorted.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.message_id.cmp(&b.message_id))
    });
    sorted.truncate(limit);
    sorted
}

/// Pick, per `message_id`, the chunk row with the highest cosine similarity
/// to `query_vector`, and return that chunk's own text (sliced from
/// `part_texts`' matching `(message_id, part)` entry by
/// `span_start..span_end`).
///
/// Pure CPU, no I/O — called from [`Presenter::fetch_best_chunks`] on the
/// blocking pool, and directly unit-testable without a database.
fn best_chunk_per_message(
    chunk_rows: Vec<(i64, String, i64, i64, Vec<u8>)>,
    part_texts: &BTreeMap<(i64, String), String>,
    query_vector: &Embedding,
) -> BTreeMap<i64, String> {
    let mut best: BTreeMap<i64, (f32, String)> = BTreeMap::new();
    for (message_id, part, span_start, span_end, embedding_bytes) in chunk_rows {
        let Ok(embedding) = Embedding::from_bytes(&embedding_bytes, VECTOR_DIM) else {
            continue;
        };
        let Some(part_text) = part_texts.get(&(message_id, part)) else {
            continue;
        };
        let score = embedding.cosine(query_vector);
        let start = usize::try_from(span_start).unwrap_or(0);
        let end = usize::try_from(span_end).unwrap_or(0);
        // `end > start` (not just a valid `get` range) and `get`, not
        // indexing: a stored span is trusted but not blindly — the same
        // caution `index::semantic`'s own witness/fingerprint re-check
        // takes toward another pass's prior output. Without the
        // non-empty check, a corrupt or stale zero-width span (`start ==
        // end`, or a negative value saturating both to `0`) would slice to
        // `""` and — being `Some("")`, not `None` — win a slot and shadow
        // the real body-excerpt fallback in `Presenter::finalize_snippets`
        // with an empty snippet instead of leaving the field for a
        // legitimate chunk (or the body fallback) to fill.
        let Some(chunk_text) = (end > start).then(|| part_text.get(start..end)).flatten() else {
            continue;
        };
        let better = match best.get(&message_id) {
            Some((existing, _)) => score > *existing,
            None => true,
        };
        if better {
            best.insert(message_id, (score, chunk_text.to_owned()));
        }
    }
    best.into_iter().map(|(id, (_, text))| (id, text)).collect()
}

/// `?, ?, ..., ?` for `n` placeholders — matches every other batched `IN
/// (...)` query in this crate's search stages
/// (`fuse::Fuser::fetch_meta`/`features::extract`'s own helper of the same
/// name and shape).
fn placeholder_list(n: usize) -> String {
    vec!["?"; n].join(", ")
}

#[cfg(test)]
mod tests;
