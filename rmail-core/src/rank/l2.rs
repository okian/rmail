//! Stage 5 — the L2 reranker (prd.md, "Stage 5 — L2 Reranker (expensive,
//! top-K only)"): "Re-order only the top-K (≈50) with a heavier model that
//! reads actual text, not just features."
//!
//! # What this stage is, in one sentence
//!
//! Everything before it ranks by *features* — BM25 numbers, cosine numbers,
//! recency, sender affinity — and never reads a message. This stage reads the
//! message. That is why it is last, why it is capped at the top-K, and why it
//! is the only optional stage in the cascade.
//!
//! # Two backends, one trait
//!
//! [`cross_encoder::CrossEncoderReranker`] runs a local ONNX cross-encoder
//! over `(query, document)` pairs — offline, zero egress, ~80 ms for 50
//! pairs. [`claude::ClaudeReranker`] sends the top ~30 to Claude for a
//! listwise ordering plus a one-line "why this matched," cached by
//! `(query_hash, candidate_id_set)`. They share [`Reranker`] and nothing
//! else: the local one produces a logit per pair with no prose, the hosted
//! one produces an ordering with prose and no meaningful magnitude. The trait
//! is deliberately narrow enough that both fit it honestly rather than one of
//! them pretending to produce what the other does.
//!
//! `search.rerank` picks between them: `off | cross_encoder | claude | auto`,
//! where `auto` means the cross-encoder for interactive search and Claude for
//! an explicit deep search ([`SearchKind`]).
//!
//! # Degradation is the contract, not the error path
//!
//! [`L2Stage::rerank`] returns [`Reranked`], never a `Result`. There is no
//! way for this stage to fail a search: an unprovisioned ONNX model, a
//! provider outage, an exhausted spend budget, a superseded query, a
//! malformed model answer, a database read that lost half its rows — every
//! one of them logs a reason and returns the L1 order unchanged. prd.md is
//! explicit ("Degrades to the L1 order on error/budget") and it is the whole
//! reason Stage 4 is a complete ranking on its own rather than a pre-filter.
//!
//! # Why the reordering is a permutation of the L1 *scores*
//!
//! Downstream, [`crate::present::Presenter`] re-sorts by
//! [`RankedCandidate::score`] — both on the strict-order path and inside MMR
//! — because [`crate::rank::Ranker::rank`]'s contract says its output is
//! score-ordered. A stage that reordered the list while leaving the scores
//! alone would therefore be silently undone by the next stage.
//!
//! So the new order is expressed *in the scores*: the window's own L1 scores,
//! taken as a multiset and sorted descending, are re-assigned to the reranked
//! order. Nothing else changes — not the scale, not the distribution, not the
//! relationship between the window and the tail below it (the window's
//! smallest score is still the window's smallest score, so a reranked
//! candidate can never fall below an un-reranked one). Writing raw
//! cross-encoder logits or synthetic listwise ranks into that field would do
//! all three, and would make the tail — which no backend scored —
//! incomparable with the head.
//!
//! The one artifact of this choice is that a hit's `score` after a rerank is
//! *a* score from the window rather than *its own* L1 score. That matters in
//! exactly one place, and it is handled there: `rmaild`'s
//! `RankExplanation.score` is recomputed from the candidate's own feature
//! vector, so the wire invariant "summing every contribution reproduces
//! `score`" survives a rerank.

pub mod cache;
pub mod claude;
pub mod cross_encoder;
mod document;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::RankedCandidate;
use crate::config::{Rerank, RerankerConfig, SearchConfig};
use crate::error::Error;
use crate::storage::Database;

pub use cache::{CacheKey, RerankCache};
pub use claude::ClaudeReranker;
pub use cross_encoder::CrossEncoderReranker;

/// One candidate as a backend sees it: an identity and the bounded text to
/// judge, and nothing else.
///
/// Deliberately carries no L1 score. A reranker that could see the incoming
/// ranking would be free to anchor on it — which is the one thing this stage
/// exists *not* to do, since anchoring reproduces the L1 order and buys
/// nothing for the latency it costs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RerankCandidate {
    /// The matched message (`messages.id`).
    pub message_id: i64,
    /// The rendered `(subject, sender, date, body excerpt)` blob, already
    /// bounded — see [`document`]'s own docs for the shape and the cut.
    pub document: String,
}

/// One backend's judgment of one candidate.
#[derive(Debug, Clone, PartialEq)]
pub struct RerankVerdict {
    /// The candidate this verdict is about.
    pub message_id: i64,
    /// Higher is better. Comparable only against other verdicts from the
    /// *same* [`Reranker::rerank`] call: a cross-encoder logit and a
    /// listwise position are not the same kind of number, and neither is
    /// comparable with a [`RankedCandidate::score`].
    pub score: f64,
    /// prd.md's "one-line why this matched," when the backend produces one.
    /// [`claude::ClaudeReranker`] does; a cross-encoder emits a logit and no
    /// prose, and inventing a sentence from one would be a fabricated
    /// explanation.
    pub why: Option<String>,
}

/// A backend that re-orders a top-K window by reading the candidates' text.
#[async_trait]
pub trait Reranker: Send + Sync + std::fmt::Debug {
    /// A stable short name for logs and for `--explain` output.
    fn name(&self) -> &'static str;

    /// Whether judging a candidate sends its text off the machine.
    ///
    /// This is what [`L2Stage`] resolves [`crate::ai::PolicyEngine`] against:
    /// a network backend needs `permits_network`, a local one needs only
    /// `is_visible`. It is a property of the *backend*, not of the policy, so
    /// it lives here — a future on-device listwise reranker would answer
    /// `false` and correctly become usable on `local_only` mail without any
    /// change to the gate.
    fn needs_network(&self) -> bool;

    /// Judge every candidate against `query`.
    ///
    /// Implementations may return verdicts in any order — [`L2Stage`] sorts
    /// by score itself — but must return one verdict per candidate. A short
    /// list is treated as a failed rerank, not as a partial one, because a
    /// candidate with no verdict has no defensible position in the new order.
    ///
    /// # Errors
    ///
    /// Whatever the backend could not do: an unloadable model, a provider
    /// failure, an exhausted budget, a superseded query. Every one of them is
    /// degraded by [`L2Stage::rerank`] to the incoming L1 order.
    async fn rerank(
        &self,
        query: &str,
        candidates: &[RerankCandidate],
        cancel: &CancellationToken,
    ) -> Result<Vec<RerankVerdict>, Error>;

    /// Whether this backend can run at all right now, checked *before*
    /// [`L2Stage`] reads any message text.
    ///
    /// This exists because the default configuration is `search.rerank =
    /// "auto"` and the cross-encoder's model is not provisioned by default:
    /// without a readiness check, every search on an ordinary daemon would
    /// fetch and render up to `top_k_rerank` documents purely to hand them to
    /// a backend that was always going to refuse. The document fetch is the
    /// expensive half of this stage when the model is absent, and this is what
    /// skips it.
    ///
    /// The default implementation says yes — a backend with nothing to
    /// provision has nothing to check.
    ///
    /// # Errors
    ///
    /// The same class of error [`Reranker::rerank`] would have returned, and
    /// with the same consequence: the L1 order stands.
    async fn ready(&self, _cancel: &CancellationToken) -> Result<(), Error> {
        Ok(())
    }
}

/// Whether this search is the kind a user is typing into, or the kind that
/// is allowed to be slow and expensive.
///
/// This is the input `search.rerank = "auto"` resolves against — prd.md:
/// "`auto` uses the cross-encoder for interactive typing and Claude for
/// explicit 'deep search' / `mail ask`." It is a *request* property, not a
/// config one: the same daemon serves both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchKind {
    /// A search box. Latency-bound; local backends only under `auto`.
    #[default]
    Interactive,
    /// An explicit deep search or a RAG retrieval (`mail ask`). Quality-bound.
    Deep,
}

/// What [`L2Stage::rerank`] produced.
#[derive(Debug, Clone, PartialEq)]
pub struct Reranked {
    /// The ranking to hand to Stage 6 — reordered when a backend ran, and
    /// the caller's own input unchanged when none did.
    pub ranked: Vec<RankedCandidate>,
    /// prd.md's per-result "why this matched", by message. Empty unless a
    /// backend that produces prose actually ran.
    pub reasons: BTreeMap<i64, String>,
    /// Which backend reordered the list, or `None` when the L1 order stands.
    /// `None` is the answer for "off", "no backend configured", and every
    /// degradation path alike — a caller cannot tell them apart, and does not
    /// need to.
    pub backend: Option<&'static str>,
}

impl Reranked {
    /// The untouched L1 order — every degradation path's answer.
    fn passthrough(ranked: &[RankedCandidate]) -> Self {
        Self {
            ranked: ranked.to_vec(),
            reasons: BTreeMap::new(),
            backend: None,
        }
    }
}

/// Stage 5, wired: the configured policy, whichever backends this daemon
/// could build, and the document reads they need.
///
/// Cheap to clone (every field is an `Arc` or a `Database` handle), because
/// `SearchApi` clones itself into every streaming request.
#[derive(Debug, Clone)]
pub struct L2Stage {
    /// The configured `search.rerank`, or a per-request override.
    rerank: Rerank,
    cross_encoder: Option<Arc<dyn Reranker>>,
    claude: Option<Arc<dyn Reranker>>,
    documents: Option<document::DocumentSource>,
    /// The operator's `ai.policy` / `accounts.ai.enabled` rules, resolved
    /// per candidate before any text is handed to a backend. `None` only in
    /// [`L2Stage::disabled`], which never reaches a backend at all.
    policy: Option<Arc<crate::ai::PolicyEngine>>,
    claude_max_candidates: usize,
    timeout: Duration,
    /// Whether the *previous* attempt degraded, shared across every clone of
    /// this stage (`SearchApi` clones one per request).
    ///
    /// Reranking is best-effort and the default configuration ships with no
    /// cross-encoder model provisioned, so the common degradation is not an
    /// incident — it is the steady state, once. Logging it at `warn` on every
    /// keystroke would bury real failures in a daemon's log. This makes the
    /// warning edge-triggered: the first degradation after a working state
    /// says so loudly, the rest are `debug`, and a recovery re-arms it.
    degraded: Arc<AtomicBool>,
}

impl L2Stage {
    /// The stage as `search.rerank` and `search.reranker` configure it.
    ///
    /// `claude` is the provider-backed backend, or `None` when the daemon's
    /// AI subsystem is off or could not be built — in which case
    /// `search.rerank = "claude"` degrades to the L1 order rather than
    /// failing every search, which is the same posture every other AI-gated
    /// feature in this codebase takes.
    ///
    /// The cross-encoder is always constructed: it does no I/O until its
    /// first inference, and building it unconditionally means an
    /// unprovisioned model reports itself once, at search time, with a
    /// message naming the fix — rather than being indistinguishable from
    /// "reranking is off."
    #[must_use]
    pub fn new(
        db: Database,
        search: &SearchConfig,
        ai_policy: Arc<crate::ai::PolicyEngine>,
        claude: Option<Arc<dyn Reranker>>,
    ) -> Self {
        let cross_encoder: Arc<dyn Reranker> =
            Arc::new(CrossEncoderReranker::new(&search.reranker));
        Self {
            rerank: search.rerank,
            cross_encoder: Some(cross_encoder),
            claude,
            documents: Some(document::DocumentSource::new(db)),
            policy: Some(ai_policy),
            claude_max_candidates: search.reranker.claude_max_candidates as usize,
            timeout: search.reranker.timeout.as_duration(),
            degraded: Arc::new(AtomicBool::new(false)),
        }
    }

    /// A stage that never reranks — for callers with no database and for
    /// `search.rerank = "off"` deployments that would rather not construct a
    /// backend at all.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            rerank: Rerank::Off,
            cross_encoder: None,
            claude: None,
            documents: None,
            policy: None,
            claude_max_candidates: RerankerConfig::default().claude_max_candidates as usize,
            timeout: RerankerConfig::default().timeout.as_duration(),
            degraded: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Report a degradation: `warn` the first time, `debug` while it
    /// persists. See [`L2Stage::degraded`]'s own comment for why.
    fn degrade(&self, backend: &str, reason: &dyn std::fmt::Display) {
        if self.degraded.swap(true, Ordering::Relaxed) {
            tracing::debug!(backend, %reason, "the L2 rerank is still degraded");
        } else {
            tracing::warn!(
                backend,
                %reason,
                "the L2 rerank is unavailable; search results keep their L1 order"
            );
        }
    }

    /// Replace the backends with caller-supplied ones. Test seam: it is what
    /// lets the reorder, degrade, and cache behaviours be proven against a
    /// deterministic stub, with no ONNX model file and no network — see this
    /// module's tests.
    #[must_use]
    pub fn with_backends(
        mut self,
        cross_encoder: Option<Arc<dyn Reranker>>,
        claude: Option<Arc<dyn Reranker>>,
    ) -> Self {
        self.cross_encoder = cross_encoder;
        self.claude = claude;
        self
    }

    /// Force a policy, ignoring `search.rerank`. Used by the request-level
    /// override (`SearchRequest.rerank`, `mail search --rerank claude`).
    #[must_use]
    pub const fn with_policy(mut self, policy: Rerank) -> Self {
        self.rerank = policy;
        self
    }

    /// Which backend `policy` resolves to for a search of this `kind`, or
    /// `None` for "no rerank".
    ///
    /// The `auto` arm is prd.md's rule verbatim: cross-encoder while typing,
    /// Claude when the caller has declared the search deep.
    const fn backend_for(policy: Rerank, kind: SearchKind) -> Option<Which> {
        match policy {
            Rerank::Off => None,
            Rerank::CrossEncoder => Some(Which::CrossEncoder),
            Rerank::Claude => Some(Which::Claude),
            Rerank::Auto => match kind {
                SearchKind::Interactive => Some(Which::CrossEncoder),
                SearchKind::Deep => Some(Which::Claude),
            },
        }
    }

    /// Re-order `ranked`'s top-K against `query`.
    ///
    /// Never fails: see the module docs. `ranked` is Stage 4's output, best
    /// first and already cut to `search.top_k_rerank`.
    #[tracing::instrument(
        skip(self, query, ranked, cancel),
        fields(
            policy = ?self.rerank,
            kind = ?kind,
            candidates = ranked.len(),
            backend = tracing::field::Empty,
            window = tracing::field::Empty,
            reordered = tracing::field::Empty,
        )
    )]
    pub async fn rerank(
        &self,
        query: &str,
        ranked: &[RankedCandidate],
        kind: SearchKind,
        cancel: &CancellationToken,
    ) -> Reranked {
        let span = tracing::Span::current();
        let Some(which) = Self::backend_for(self.rerank, kind) else {
            return Reranked::passthrough(ranked);
        };
        let backend = match which {
            Which::CrossEncoder => self.cross_encoder.as_ref(),
            Which::Claude => self.claude.as_ref(),
        };
        let (Some(backend), Some(documents)) = (backend, self.documents.as_ref()) else {
            tracing::debug!(
                ?which,
                "the configured rerank backend is not available on this daemon; \
                 keeping the L1 order"
            );
            return Reranked::passthrough(ranked);
        };
        span.record("backend", backend.name());

        // The window: everything for the local backend (Stage 4 already cut
        // to `top_k_rerank`), the configured cap for the listwise one, whose
        // whole prompt has to hold every candidate's text at once.
        let window_len = match which {
            Which::CrossEncoder => ranked.len(),
            Which::Claude => ranked.len().min(self.claude_max_candidates),
        }
        // One document fetch has to be able to serve the whole window, so a
        // `search.top_k_rerank` past `document::MAX_FETCH` narrows the window
        // rather than degrading every rerank on a technicality.
        .min(document::MAX_FETCH);
        span.record("window", window_len);
        if window_len < 2 {
            // Nothing a permutation of one element could change.
            span.record("reordered", false);
            return Reranked::passthrough(ranked);
        }
        let window = &ranked[..window_len];

        // A child token so the stage's own deadline cancels whatever it
        // interrupted — a model load, an in-flight SQLite scan, a provider
        // call — rather than merely abandoning it, and so cancelling it
        // cannot cancel the caller's own token, which the rest of the request
        // still needs.
        //
        // The timeout wraps **everything** the stage does, not only
        // `Reranker::rerank`: `ready()` can be a cold ONNX session load (a
        // download, when `cross_encoder_allow_download` is on) and the
        // document fetch is a real read, so a budget that covered only the
        // last step would not be the "wall-clock ceiling for the whole L2
        // stage" `search.reranker.timeout` is documented to be.
        let deadline = cancel.child_token();
        let attempt = self.attempt(backend.as_ref(), documents, query, window, &deadline);
        let verdicts = match tokio::time::timeout(self.timeout, attempt).await {
            Ok(Ok(verdicts)) => verdicts,
            Ok(Err(Degraded::Quiet)) => {
                span.record("reordered", false);
                return Reranked::passthrough(ranked);
            }
            Ok(Err(Degraded::Reported(error))) => {
                self.degrade(backend.name(), &error);
                span.record("reordered", false);
                return Reranked::passthrough(ranked);
            }
            Err(_) => {
                deadline.cancel();
                self.degrade(
                    backend.name(),
                    &format_args!(
                        "the rerank exceeded its {}ms budget",
                        self.timeout.as_millis()
                    ),
                );
                span.record("reordered", false);
                return Reranked::passthrough(ranked);
            }
        };

        let Some(reranked) = apply(window, &verdicts) else {
            self.degrade(
                backend.name(),
                &"the backend judged a different candidate set than it was given",
            );
            span.record("reordered", false);
            return Reranked::passthrough(ranked);
        };
        span.record("reordered", true);
        // Re-arm the warning: the next degradation after a working rerank is
        // news again.
        self.degraded.store(false, Ordering::Relaxed);

        let reasons: BTreeMap<i64, String> = verdicts
            .into_iter()
            .filter_map(|verdict| verdict.why.map(|why| (verdict.message_id, why)))
            .collect();
        let mut out = reranked;
        out.extend_from_slice(&ranked[window_len..]);
        Reranked {
            ranked: out,
            reasons,
            backend: Some(backend.name()),
        }
    }

    /// Readiness → policy gate → document fetch → backend, as one future so
    /// [`Self::rerank`]'s timeout covers all of it.
    ///
    /// # Errors
    ///
    /// [`Degraded`] — the caller's cue to keep the L1 order, either quietly
    /// (an ordinary "nothing to rerank against" condition) or with a reported
    /// reason.
    async fn attempt(
        &self,
        backend: &dyn Reranker,
        documents: &document::DocumentSource,
        query: &str,
        window: &[RankedCandidate],
        cancel: &CancellationToken,
    ) -> Result<Vec<RerankVerdict>, Degraded> {
        // Readiness before any message text is read — see
        // [`Reranker::ready`]'s own docs for why the order matters on a
        // daemon with no cross-encoder model provisioned, which is the
        // default.
        backend
            .ready(cancel)
            .await
            .map_err(|error| Degraded::Reported(error.to_string()))?;

        let ids: Vec<i64> = window.iter().map(|c| c.message_id).collect();
        let docs = documents.fetch(&ids, cancel).await;
        let mut candidates = Vec::with_capacity(window.len());
        for id in &ids {
            let Some(doc) = docs.get(id) else {
                // A partial document set is not a partial rerank: the missing
                // candidates would be judged against nothing and sink, which
                // is a worse answer than the L1 order this stage started from.
                tracing::debug!(
                    message_id = id,
                    "no rerank document for a top-K candidate; keeping the L1 order"
                );
                return Err(Degraded::Quiet);
            };
            // The AI policy gate. `ai.policy` and `accounts.ai.enabled` are
            // the operator's statement about what may leave the host, and
            // they are resolved per (account, folder) — so this is the one
            // place a *search* can honor them, since the candidate set is not
            // known until now.
            //
            // A network backend needs `permits_network`; a local one needs
            // only `is_visible`, since running an on-device cross-encoder
            // over a `local_only` folder is exactly what that mode exists to
            // allow. The whole rerank is refused when any single candidate
            // fails, rather than the offending one being dropped: a shorter
            // window is still a window this policy said nothing about, and
            // silently omitting a message from a rerank would change the
            // ranking of the rest based on data the operator forbade using.
            if let Some(policy) = &self.policy {
                let target = crate::ai::PolicyTarget::account(doc.account.clone())
                    .mailbox(doc.mailbox.clone());
                let decision = policy.resolve(&target);
                let permitted = if backend.needs_network() {
                    decision.permits_network()
                } else {
                    decision.is_visible()
                };
                if !permitted {
                    tracing::debug!(
                        backend = backend.name(),
                        mode = ?decision.mode,
                        "ai policy forbids reranking this result set; keeping the L1 order"
                    );
                    return Err(Degraded::Quiet);
                }
            }
            candidates.push(RerankCandidate {
                message_id: *id,
                document: doc.render(),
            });
        }

        backend
            .rerank(query, &candidates, cancel)
            .await
            .map_err(|error| Degraded::Reported(error.to_string()))
    }
}

/// Why [`L2Stage::attempt`] gave up.
enum Degraded {
    /// An ordinary, expected condition (a missing document, a policy that
    /// forbids the call). Already logged at `debug` where it was detected;
    /// re-reporting it at `warn` would make the steady state look like an
    /// incident.
    Quiet,
    /// A backend failure worth an edge-triggered warning.
    Reported(String),
}

/// Which backend a policy resolved to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Which {
    CrossEncoder,
    Claude,
}

/// Turn `verdicts` into a reordered `window`, or `None` if they do not
/// describe exactly this window.
///
/// See the module docs for why the *scores* are permuted rather than
/// replaced. `None` is returned — rather than a best-effort partial order —
/// when a backend answered about a different candidate set than it was asked
/// about, because at that point nothing it said can be trusted to be about
/// these candidates.
fn apply(window: &[RankedCandidate], verdicts: &[RerankVerdict]) -> Option<Vec<RankedCandidate>> {
    if verdicts.len() != window.len() {
        tracing::warn!(
            verdicts = verdicts.len(),
            window = window.len(),
            "the rerank backend judged a different number of candidates than it was given"
        );
        return None;
    }
    let expected: BTreeSet<i64> = window.iter().map(|c| c.message_id).collect();
    let answered: BTreeSet<i64> = verdicts.iter().map(|v| v.message_id).collect();
    if expected != answered {
        tracing::warn!("the rerank backend judged candidates it was not given");
        return None;
    }

    // Position in the incoming (L1) order, so an exact tie between two
    // backend scores falls back to the ranking this stage started from —
    // deterministic, and never worse than where it began.
    let l1_position: BTreeMap<i64, usize> = window
        .iter()
        .enumerate()
        .map(|(index, candidate)| (candidate.message_id, index))
        .collect();
    let mut order: Vec<&RerankVerdict> = verdicts.iter().collect();
    order.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            // `Equal` for a NaN score: a backend that produced one has
            // already told us nothing useful about that candidate, and a
            // total order here is what keeps the sort deterministic instead
            // of implementation-defined.
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                l1_position
                    .get(&a.message_id)
                    .cmp(&l1_position.get(&b.message_id))
            })
    });

    // The window's own scores, taken as a multiset and sorted descending, to
    // be re-assigned to the new positions. Sorted here rather than assumed:
    // `Ranker::rank`'s contract does say its output is score-ordered, but a
    // permutation that quietly depended on it would produce a *differently*
    // ordered list — not an obviously wrong one — the day some caller passed
    // something else.
    let mut scores: Vec<f64> = window.iter().map(|candidate| candidate.score).collect();
    scores.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));

    // Assigned back-to-front, nudging *up* rather than down when two L1
    // scores were exactly equal (identical feature vectors — near-duplicate
    // mail). Left alone, `Presenter`'s own score sort would break such a tie
    // by `message_id` and silently undo the pair's reranked order; nudged
    // *downward*, the last window element could sink below the first
    // un-reranked one and break the window/tail invariant this module
    // documents. Going upward from the smallest score keeps both properties:
    // the lowest score assigned is exactly `scores[len - 1]`, and the highest
    // can only rise toward `scores[0]`, which is already the top of the whole
    // ranking. The nudge is at ulp scale, smaller than any difference a
    // ranker can express, so it reorders nothing else.
    let mut out: Vec<RankedCandidate> = Vec::with_capacity(window.len());
    let mut previous: Option<f64> = None;
    for (index, verdict) in order.iter().enumerate().rev() {
        let mut score = scores.get(index).copied().unwrap_or(0.0);
        if let Some(previous) = previous {
            if score <= previous {
                score = strictly_above(previous);
            }
        }
        previous = Some(score);
        out.push(RankedCandidate {
            message_id: verdict.message_id,
            score,
        });
    }
    out.reverse();
    Some(out)
}

/// The smallest value meaningfully above `value` at this magnitude.
fn strictly_above(value: f64) -> f64 {
    value + value.abs().max(1.0) * f64::EPSILON * 4.0
}

/// `?, ?, ?` for an `IN (...)` clause of `n` bound parameters.
///
/// Duplicated from [`crate::present`]'s identical private helper rather than
/// shared: it is three lines, and exporting it would make a formatting detail
/// of one module's SQL part of another's public surface.
fn placeholder_list(n: usize) -> String {
    let mut out = String::with_capacity(n * 3);
    for i in 0..n {
        if i > 0 {
            out.push_str(", ");
        }
        out.push('?');
    }
    out
}

#[cfg(test)]
mod tests;
