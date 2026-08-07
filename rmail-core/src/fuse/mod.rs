//! Stage 2 — Fusion & Dedup: combining task 28's per-source candidate lists
//! into one ranked, deduplicated candidate set (prd.md, "Stage 2 — Fusion &
//! Dedup").
//!
//! # Three steps, two of them pure
//!
//! prd.md's Stage 2 is really three separate pieces of work, and this module
//! keeps them separate on purpose:
//!
//! 1. **[`fuse_scores`]** — weighted RRF (or, with `fusion = "linear"`, a
//!    normalized weighted linear blend) over every source's rank/score.
//!    Pure and DB-free: every input is already in memory (the
//!    [`Candidate`]s [`crate::retrieve::Fanout::generate`] returned), so this
//!    is directly unit-testable against hand-computed values without a
//!    database — see `tests.rs`'s "hand-computed" tests, and the module
//!    brief's own warning that a subtly wrong fusion formula still produces
//!    a *plausible* ranking, which is exactly the failure an ordering-only
//!    test cannot catch.
//! 2. **[`collapse_threads`]** — optional message→thread collapse. Also
//!    pure, given each message's `thread_id` (looked up once, batched, by
//!    [`Fuser::fuse`]).
//! 3. **[`collapse_near_duplicates`]** — unconditional SimHash near-dup
//!    collapse (see [`simhash`]). Also pure, given each message's body text.
//!
//! Only the metadata lookup feeding steps 2 and 3 touches the database
//! ([`Fuser::fetch_meta`]); [`Fuser::fuse`] is the thin orchestrator that
//! wires it to the three pure functions above. Splitting it this way is what
//! makes the RRF arithmetic testable without a `Database` fixture at all,
//! while still giving `collapse_threads`/`collapse_near_duplicates` their
//! own direct tests (constructed `BTreeMap` metadata, no I/O) separate from
//! the one or two DB-integration tests [`Fuser::fetch_meta`] actually needs.
//!
//! # Chunk→message collapse already happened
//!
//! prd.md's Stage 2 bullet reads "chunk hits → parent message." That
//! collapse is already done by the time a [`Candidate`] reaches this module:
//! [`crate::retrieve::dense::DenseRetriever`] — the only chunk-level
//! source — dedupes chunk hits to their parent message itself, keeping `max`
//! chunk similarity as [`Candidate::score`] and `mean` as
//! [`Candidate::mean_score`] (see that module's and [`Candidate`]'s own
//! docs). Every [`Candidate`] this module ever sees is already
//! message-granular. What this module *does* still have to do is the
//! cross-source half of "collapse": [`fuse_scores`] groups by `message_id`
//! across sources — the same message found by lexical rank 3 and dense rank
//! 1 must become one fused row carrying both hits, not two competing rows —
//! which is the actual mechanism by which "a document found by several
//! sources outranks one found by a single strong source" (this module's own
//! required test) becomes true.
//!
//! # `thread_collapse` is a parameter, not a config knob
//!
//! prd.md calls thread collapse "optional" and describes it as switching
//! between a flat list and "thread-mode" — a per-search presentation choice
//! (list view vs. conversation view), not a deployment-wide default the way
//! `search.fusion`/`search.rrf_k` are. [`crate::config::SearchConfig`] has no
//! field for it, and this module does not add one: [`Fuser::fuse`] takes
//! `thread_collapse: bool` as a call argument, left for task 33's
//! `SearchRequest` (or an equivalent CLI flag) to set per request. SimHash
//! collapse has no such qualifier in the task's acceptance bullet and runs
//! unconditionally — a bulk newsletter resent into two different threads is
//! exactly the duplicate thread collapse cannot catch.
//!
//! # No output truncation here
//!
//! prd.md describes Stage 2's output as "~200–500 fused candidates," which
//! reads as a size bound at first glance. It isn't one this module enforces:
//! that range is what naturally falls out of `search.candidates_per_source`
//! (200 by default) times however many of the seven sources are enabled and
//! actually matched, deduplicated by [`fuse_scores`]'s own `message_id`
//! grouping — a description of the typical shape, not a `top_k` this stage
//! is responsible for cutting to. [`Fuser::fuse`] returns every surviving
//! candidate. Keeping only the top-K for the next stage is prd.md's Stage 4
//! job ("L1 Ranker... keeps top-K (default 50)"), which needs the *ranked*
//! (feature-scored) list to decide K sensibly — cutting earlier, on the raw
//! fused score alone, would be a second, cruder ranking decision made with
//! less information than the one Stage 4 already owns.
//!
//! # Two sources prd.md's fusion-weight table does not tune
//!
//! [`crate::config::FusionSourceWeights`] has exactly the five fields
//! prd.md's Stage 2 weight table names (lexical/dense/fuzzy/entity/recency).
//! [`Source::Structured`] and [`Source::Prefix`] are real sources in the
//! fan-out (task 28) but absent from that table — `Structured`'s own docs
//! call it "a hard gate rather than a ranking source in its own right," and
//! `Prefix` is autocomplete-shaped recall the table simply never mentions
//! tuning. The acceptance bullet for this task still asks for weights "over
//! all sources," so both need *some* weight.
//!
//! That weight cannot be a neutral `1.0`, though — `1.0` is the *highest*
//! weight in every intent row except navigational lexical, which makes it
//! actively wrong rather than merely unspecified, and both sources are
//! *correlated* with a tuned source rather than independent of it, so their
//! weight adds to that source's rather than competing fairly against it:
//!
//! - `retrieve::structured` and `retrieve::recency` gate on the identical
//!   hard-filter mask and order by the identical `COALESCE(date,
//!   internaldate) DESC` (see `retrieve::structured`'s own "Score is uniform;
//!   order comes from recency" doc section) — so on *any* query with a hard
//!   filter (`from:`, `in:`, `has:`, ...), [`Source::Structured`] and
//!   [`Source::Recency`] return the same ids at the same ranks. A `Structured`
//!   weight does not add a fair fifth vote; it adds directly onto the
//!   recency prior's real weight (`w_recency + w_structured` instead of
//!   `w_recency`).
//! - `retrieve::prefix` builds a `"term"*` prefix query from the same
//!   original, non-negated free-text terms `retrieve::lexical` ANDs together
//!   verbatim, so for the common single/few-term query prefix's result set
//!   is a superset of lexical's, matched at essentially the same rank. A
//!   `Prefix` weight adds directly onto lexical's real weight the same way.
//!
//! Both distortions land hardest on exploratory and lookup intent, where
//! `recency`/`lexical` are deliberately weighted *down* (`0.3`/`0.7` and
//! `0.4`/`0.8`) so dense/entity recall can dominate instead — a correlated
//! source's weight stacking on top silently un-suppresses exactly what the
//! intent table was tuned to suppress.
//!
//! [`STRUCTURED_SOURCE_WEIGHT`] and [`PREFIX_SOURCE_WEIGHT`] are fixed, low,
//! intent-invariant constants instead — deliberately *not* added to
//! [`crate::config::FusionSourceWeights`] as tunable fields, even though
//! that struct's `#[serde(deny_unknown_fields, default)]` would make doing
//! so backward-compatible: prd.md's table gives no basis for what a *tuned*
//! per-intent value should be for either source, so a config knob here would
//! invite tuning two numbers against no spec rather than fixing the actual
//! bug (weights that were too high by construction, not merely
//! unconfigurable). Both are `0.1`, chosen so `w_structured + w_recency` and
//! `w_prefix + w_lexical` stay comfortably (never by less than 10%, usually
//! far more) below the source each is correlated with, across all three
//! intents:
//!
//! | intent | structured + recency | vs. lexical | prefix + lexical | vs. dense/entity |
//! |---|---|---|---|---|
//! | navigational | 0.1+0.8=0.9 | 1.0 | 1.0+0.1=1.1 | n/a — lexical already the max weight here |
//! | exploratory | 0.1+0.3=0.4 | 0.7 | 0.7+0.1=0.8 | dense 1.0 |
//! | lookup | 0.1+0.4=0.5 | 0.8 | 0.8+0.1=0.9 | entity 1.0 |
//!
//! An identical value for both is deliberate, not a coincidence worth
//! collapsing into one constant: they bound two different correlations
//! (structured-with-recency, prefix-with-lexical) that happen to need the
//! same-sized margin against prd.md's actual table, and keeping them as
//! separate named constants leaves room for either to move independently
//! if a future measurement (task 37's evaluation harness) says they should.

pub mod simhash;

use std::collections::BTreeMap;

use tokio_util::sync::CancellationToken;

use crate::config::{Fusion, FusionSourceWeights, FusionWeights, SearchConfig};
use crate::query::{Intent, QueryPlan};
use crate::retrieve::cancel::interruptible_read;
use crate::retrieve::{Candidate, Source};
use crate::storage::Database;

/// One source's contribution to a [`FusedCandidate`] — its own rank, score,
/// and (dense-only) mean chunk score, carried through unchanged so task 30's
/// feature extraction can read "ranked #3 in lexical" and "barely beat the
/// BM25 floor" as the distinct signals prd.md's Stage 3 feature table treats
/// them as (`Candidate`'s own doc comment makes the same point about why
/// neither field is derivable from the other).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SourceHit {
    /// Which retriever produced this hit.
    pub source: Source,
    /// 1-based rank within that source's own result list.
    pub rank: u32,
    /// That source's own relevance score (the max, for dense).
    pub score: f64,
    /// The mean chunk cosine similarity, dense-only — see
    /// [`Candidate::mean_score`].
    pub mean_score: Option<f64>,
}

/// One message after Stage 2: a fused score plus every source's rank/score
/// that contributed to it, and what got collapsed into it.
#[derive(Debug, Clone, PartialEq)]
pub struct FusedCandidate {
    /// The matched message (`messages.id`).
    pub message_id: i64,
    /// The combined score — weighted RRF sum, or the linear blend, depending
    /// on `SearchConfig::fusion`. Comparable across candidates *within this
    /// fusion run only*; never across two different queries or fusion modes.
    pub fused_score: f64,
    /// Every source that returned this message, each with its own
    /// rank/score/mean_score intact. prd.md's Stage 3 `rrf_score`,
    /// `num_sources_hit`, `best_source`, `cos_max_chunk`/`cos_mean_chunk`,
    /// and every per-field BM25 feature all read from here.
    pub hits: Vec<SourceHit>,
    /// `hits.len()` — how many sources agreed on this candidate. Carried as
    /// its own field (rather than making every caller re-derive it) because
    /// it is a named Stage 3 feature in its own right.
    pub num_sources_hit: usize,
    /// The source contributing this candidate's single largest weighted
    /// term — its strongest individual signal. Ties (two sources landing on
    /// the exact same weighted term) break toward the source earlier in
    /// [`Source`]'s prd.md table order (lexical, dense, fuzzy, entity,
    /// structured, prefix, recency), so `best_source` is a pure function of
    /// the input candidates rather than of `BTreeMap`/iteration order.
    pub best_source: Source,
    /// This message's thread, when thread collapse ran and the message has
    /// one. `None` when thread collapse did not run, the metadata lookup
    /// degraded (see [`Fuser::fetch_meta`]), or the message has no thread.
    pub thread_id: Option<i64>,
    /// Sibling message ids from the same thread that were collapsed into
    /// this one (only ever non-empty on the thread's canonical/best-scoring
    /// candidate — see [`collapse_threads`]). Empty when thread collapse did
    /// not run or this thread had only one candidate in the fused set.
    pub thread_collapsed: Vec<i64>,
    /// Sibling message ids whose SimHash fingerprint was within
    /// [`simhash::NEAR_DUP_HAMMING_THRESHOLD`] of this one's, collapsed into
    /// it (only ever non-empty on the cluster's canonical/best-scoring
    /// candidate — see [`collapse_near_duplicates`]). Includes any message
    /// ids a merged candidate had itself already absorbed via thread
    /// collapse, so a message never disappears without a trace no matter
    /// how many collapse steps it passed through.
    pub near_duplicates: Vec<i64>,
}

/// Fixed weight for [`Source::Structured`] — see the module docs' "Two
/// sources prd.md's fusion-weight table does not tune" section for why this
/// is low and intent-invariant rather than `1.0` or config-tunable.
const STRUCTURED_SOURCE_WEIGHT: f64 = 0.1;

/// Fixed weight for [`Source::Prefix`] — see the same module doc section.
const PREFIX_SOURCE_WEIGHT: f64 = 0.1;

/// A fixed, arbitrary total order over [`Source`] used only to make
/// [`fuse_scores`]'s `best_source` tie-break and `hits` ordering
/// deterministic. [`Source`] itself has no [`Ord`] (task 28 never needed
/// one), and adding one would be a change to a shared type outside this
/// task's module — a private ordinal here gets the same determinism without
/// touching `retrieve::Source`. The order matches prd.md's own Stage 1
/// retriever table row order.
const fn source_ordinal(source: Source) -> u8 {
    match source {
        Source::Lexical => 0,
        Source::Dense => 1,
        Source::Fuzzy => 2,
        Source::Entity => 3,
        Source::Structured => 4,
        Source::Prefix => 5,
        Source::Recency => 6,
    }
}

/// This source's configured weight for `intent`. [`Source::Structured`] and
/// [`Source::Prefix`] get their fixed constants regardless of `intent` — see
/// the module docs.
fn source_weight(weights: &FusionSourceWeights, source: Source) -> f64 {
    match source {
        Source::Lexical => weights.lexical,
        Source::Dense => weights.dense,
        Source::Fuzzy => weights.fuzzy,
        Source::Entity => weights.entity,
        Source::Recency => weights.recency,
        Source::Structured => STRUCTURED_SOURCE_WEIGHT,
        Source::Prefix => PREFIX_SOURCE_WEIGHT,
    }
}

/// The per-source weight table for `intent` (prd.md's Stage 2 weight table,
/// one row per [`Intent`]).
fn weights_for_intent(fusion_weights: &FusionWeights, intent: Intent) -> &FusionSourceWeights {
    match intent {
        Intent::Navigational => &fusion_weights.navigational,
        Intent::Exploratory => &fusion_weights.exploratory,
        Intent::Lookup => &fusion_weights.lookup,
    }
}

/// Combine every source's candidate list into one fused, best-first-sorted
/// list — prd.md's Stage 2 core arithmetic:
///
/// ```text
/// fused_score(m) = Σ_over_sources s  w_s · 1 / (k_rrf + rank_s(m))          (RRF)
/// fused_score(m) = Σ_over_sources s  w_s · minmax(score_s(m))               (linear)
/// ```
///
/// `w_s` is `intent`'s configured weight for source `s` ([`source_weight`]).
/// For RRF, `rank_s(m)` is `m`'s 1-based rank in source `s`'s own list —
/// absent sources contribute no term, matching prd.md's "absent → term
/// omitted." For linear, `minmax(score_s(m))` normalizes `m`'s score against
/// *that source's own* min/max over every candidate it returned (not just
/// the ones that also matched elsewhere) — a source that returned exactly
/// one candidate, or several with an identical score, has no range to
/// normalize against, so every one of that source's candidates gets `1.0`
/// (full weight) rather than a division by zero or an arbitrary `0.0`: a
/// source that could not distinguish its own candidates still found them,
/// which is evidence, not its absence.
///
/// Pure and synchronous — no I/O, no cancellation to honor, which is what
/// makes this function directly testable against hand-computed values (see
/// `tests.rs`).
#[must_use]
pub fn fuse_scores(
    candidates: &[Candidate],
    intent: Intent,
    fusion: Fusion,
    rrf_k: u32,
    fusion_weights: &FusionWeights,
) -> Vec<FusedCandidate> {
    let weights = weights_for_intent(fusion_weights, intent);

    // Defensive de-dup: keep one row per (source, message_id), the
    // lowest-ranked (== highest-scoring — every retriever's `rank` is
    // derived from a best-first score sort) occurrence. Every shipped
    // retriever already returns at most one row per message (task 28's own
    // `rank_by_score`, and each source's `GROUP BY`/dedup), so this never
    // fires today; it exists so a future retriever bug degrades to "ignore
    // the duplicate" instead of double-counting one source's vote for the
    // same message. Keyed by `(source_ordinal, message_id)` rather than
    // `(Source, message_id)` so this can be a `BTreeMap` (deterministic
    // iteration) without giving `Source` an `Ord` it does not otherwise
    // need.
    let mut best_per_source: BTreeMap<(u8, i64), &Candidate> = BTreeMap::new();
    for candidate in candidates {
        let key = (source_ordinal(candidate.source), candidate.message_id);
        best_per_source
            .entry(key)
            .and_modify(|existing| {
                if candidate.rank < existing.rank {
                    *existing = candidate;
                }
            })
            .or_insert(candidate);
    }

    // Linear fusion's per-source [min, max] range, computed over that
    // source's own deduped rows above — see this function's doc comment for
    // why a degenerate (single-candidate or all-tied) range maps to `1.0`
    // rather than `0.0`.
    let mut source_range: BTreeMap<u8, (f64, f64)> = BTreeMap::new();
    if matches!(fusion, Fusion::Linear) {
        for (&(source_ord, _), candidate) in &best_per_source {
            let entry = source_range
                .entry(source_ord)
                .or_insert((candidate.score, candidate.score));
            entry.0 = entry.0.min(candidate.score);
            entry.1 = entry.1.max(candidate.score);
        }
    }

    struct Acc {
        hits: Vec<SourceHit>,
        sum: f64,
        best_term: f64,
        best_source: Source,
    }

    let mut by_message: BTreeMap<i64, Acc> = BTreeMap::new();
    for (&(source_ord, message_id), candidate) in &best_per_source {
        let source = candidate.source;
        let weight = source_weight(weights, source);
        let term = match fusion {
            // `f64::from` on each operand *before* adding, not
            // `f64::from(rrf_k + candidate.rank)`: `rrf_k` is a `u32` read
            // straight from TOML/env with no upper-bound validation
            // anywhere in `config`, so a pathological config value summed
            // in `u32` first would panic (debug) or silently wrap (release)
            // instead of just producing a very small, harmless term.
            Fusion::Rrf => weight / (f64::from(rrf_k) + f64::from(candidate.rank)),
            Fusion::Linear => {
                let (min, max) = source_range
                    .get(&source_ord)
                    .copied()
                    .unwrap_or((candidate.score, candidate.score));
                let normalized = if max > min {
                    (candidate.score - min) / (max - min)
                } else {
                    1.0
                };
                weight * normalized
            }
        };

        let entry = by_message.entry(message_id).or_insert_with(|| Acc {
            hits: Vec::new(),
            sum: 0.0,
            best_term: f64::MIN,
            best_source: source,
        });
        entry.hits.push(SourceHit {
            source,
            rank: candidate.rank,
            score: candidate.score,
            mean_score: candidate.mean_score,
        });
        entry.sum += term;
        // Exact equality is deliberate, not a float-comparison bug: two
        // sources landing on the literal same term (e.g. equal weight, equal
        // rank under RRF) is common enough — every `FusionSourceWeights`
        // default is uniform — that this tie-break needs to be exercised,
        // not just theoretically reachable. See `source_ordinal`'s docs.
        #[allow(clippy::float_cmp)]
        let is_new_best = term > entry.best_term
            || (term == entry.best_term
                && source_ordinal(source) < source_ordinal(entry.best_source));
        if is_new_best {
            entry.best_term = term;
            entry.best_source = source;
        }
    }

    let mut out: Vec<FusedCandidate> = by_message
        .into_iter()
        .map(|(message_id, mut acc)| {
            acc.hits
                .sort_by_key(|hit| (source_ordinal(hit.source), hit.rank));
            FusedCandidate {
                message_id,
                fused_score: acc.sum,
                num_sources_hit: acc.hits.len(),
                hits: acc.hits,
                best_source: acc.best_source,
                thread_id: None,
                thread_collapsed: Vec::new(),
                near_duplicates: Vec::new(),
            }
        })
        .collect();

    out.sort_by(|a, b| {
        b.fused_score
            .partial_cmp(&a.fused_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.message_id.cmp(&b.message_id))
    });
    out
}

/// Whether `plan` expresses real free-text search intent: a non-empty
/// [`QueryPlan::lexical_terms`]/[`QueryPlan::phrases`], or a
/// [`QueryPlan::query_vector`] to search against.
///
/// This is the guard [`drop_prior_only_candidates`] checks before removing
/// anything: a filters-only query (`is:flagged`, `from:alice`, or a
/// genuinely empty query) has no free text for lexical/dense/fuzzy/entity to
/// have matched in the first place, so a structured/recency-only result
/// *is* the intended answer, not noise — prd.md's own "Empty query ->
/// recency-ranked recent mail" edge case, generalized to "filters-only
/// query" the same way.
fn has_free_text_intent(plan: &QueryPlan) -> bool {
    !plan.lexical_terms.is_empty() || !plan.phrases.is_empty() || plan.query_vector.is_some()
}

/// Drop fused candidates that only a "prior" source found —
/// [`Source::Recency`] and/or [`Source::Structured`] — when the query
/// expressed real free-text search intent ([`has_free_text_intent`]) that no
/// free-text-matching retriever ([`Source::Lexical`], [`Source::Dense`],
/// [`Source::Fuzzy`], [`Source::Entity`]) actually satisfied for that
/// candidate. A no-op (`fused` returned unchanged) when the query has no
/// free text at all.
///
/// # Why this exists
///
/// [`crate::retrieve::recency::RecencyRetriever`] is deliberately
/// unconditional: prd.md's own retriever table describes it as "recent mail
/// with **weak** textual match," and its implementation returns up to
/// `candidates_per_source` of the mailbox — subject only to hard filters,
/// with no notion of whether the query's free text appears anywhere in a
/// candidate at all — ordered by date (see that module's own docs: it
/// exists so fusion "can credit a candidate that... is unusually recent,"
/// evidence the free-text retrievers never look at). That is the right
/// shape for what it is meant to *augment* — a real but weak match earns a
/// recency boost on top of it — but composed with every other stage
/// unmodified, it also means a query whose free text matches nothing still
/// returns a full page of "recent mail": on a real mailbox, *every* query
/// would present up to `candidates_per_source` results, almost all of which
/// never matched a single word the user typed, with the Stage 4 top-K/
/// `SearchRequest.limit` cut the only thing hiding that from view. That
/// directly contradicts this system's own stated bar ("the right message is
/// in the top 3, always" — prd.md, Part 0) and the retriever table's own
/// description of recency as evidence that *augments* a match, not an
/// unconditional recall path in its own right. (Task 33's `SearchService`
/// integration tests are what actually surfaced this — a two-message
/// mailbox with one obviously-irrelevant recent message made an otherwise
/// negligible low-score artifact visible; see that crate's `search_service`
/// module docs.)
///
/// [`Source::Structured`] gets the identical treatment, not a carve-out:
/// this module's own docs already call it "a hard gate rather than a
/// ranking source in its own right" — every retriever, `Source::Structured`
/// included, is already gated by the same hard filters (see
/// `retrieve::mod`'s "`hard_filters`, not `scope`" section), so a
/// structured-only candidate is not new *filter* evidence, only a message
/// that happens to satisfy the filter with no free-text support — the exact
/// same shape of noise recency-only contributes, for the exact same reason.
/// A genuinely filters-only query (no free text at all) is unaffected:
/// [`has_free_text_intent`] returns `false`, and `is:flagged`/`from:alice`
/// alone still returns every message that satisfies it, which *is* the
/// correct answer to a query with nothing else to match against.
#[must_use]
fn drop_prior_only_candidates(fused: Vec<FusedCandidate>, plan: &QueryPlan) -> Vec<FusedCandidate> {
    if !has_free_text_intent(plan) {
        return fused;
    }
    fused
        .into_iter()
        .filter(|candidate| {
            candidate.hits.iter().any(|hit| {
                matches!(
                    hit.source,
                    Source::Lexical | Source::Dense | Source::Fuzzy | Source::Entity
                )
            })
        })
        .collect()
}

/// Per-message metadata the collapse steps need beyond what [`fuse_scores`]
/// already computed: `thread_id` for [`collapse_threads`]; `body` for
/// [`collapse_near_duplicates`] (what gets SimHash-fingerprinted). `date` is
/// fetched in the same round trip because it comes free with the join this
/// module already runs, and is left on the type for a future consumer (a
/// presentation layer applying prd.md's Stage 6 "usually newest" convention
/// over `near_duplicates` — see [`collapse_near_duplicates`]'s doc comment)
/// even though nothing in this module reads it back out today.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MessageMeta {
    /// This message's thread, if any.
    pub thread_id: Option<i64>,
    /// `COALESCE(date, internaldate)`, unix seconds.
    pub date: Option<i64>,
    /// Normalized body text (`index_content` where `part = 'body'`, capped
    /// to [`MAX_BODY_CHARS_FOR_SIMHASH`] characters), if the message has
    /// been indexed. `None` skips SimHash fingerprinting for this message
    /// rather than fingerprinting an empty string.
    pub body: Option<String>,
}

/// Collapse fused candidates that share a thread to their single
/// best-scoring representative (prd.md: "thread-mode shows the best
/// representative message, with a '+N in thread' affordance").
///
/// `fused` must already be sorted best-first, as [`fuse_scores`] returns
/// it — that invariant is what makes a single forward pass sufficient: the
/// first member of a thread encountered in score order is, by construction,
/// that thread's highest-scoring member, so it becomes canonical without a
/// second pass to compare scores within each group.
///
/// A message with no `thread_id` (not in `meta`, or `meta` has `None`) is
/// never collapsed — it stands alone, same as a thread with only one
/// candidate in the fused set.
///
/// An absorbed candidate's own `thread_collapsed`/`near_duplicates` (empty
/// in the pipeline [`Fuser::fuse`] runs, where this always executes before
/// [`collapse_near_duplicates`] ever populates either list — but not
/// guaranteed for a caller invoking this function directly, since both are
/// `pub`) are merged into the canonical's matching list rather than dropped,
/// so calling this function on an already-collapsed input can never lose a
/// message id.
#[must_use]
pub fn collapse_threads(
    fused: Vec<FusedCandidate>,
    meta: &BTreeMap<i64, MessageMeta>,
) -> Vec<FusedCandidate> {
    let mut out: Vec<FusedCandidate> = Vec::with_capacity(fused.len());
    // thread_id -> index into `out` of that thread's canonical candidate.
    let mut canonical_index: BTreeMap<i64, usize> = BTreeMap::new();
    for mut candidate in fused {
        let thread_id = meta.get(&candidate.message_id).and_then(|m| m.thread_id);
        let existing_canonical = thread_id.and_then(|t| canonical_index.get(&t).copied());
        match existing_canonical.and_then(|idx| out.get_mut(idx)) {
            // `idx` is always in bounds by construction (inserted as
            // `out.len()` at the moment a candidate was pushed, so it can
            // only ever name an earlier, already-pushed slot) — `get_mut`
            // rather than indexing anyway, so a future refactor that broke
            // that invariant would silently drop one duplicate rather than
            // panic the whole search.
            Some(canonical) => {
                canonical.thread_collapsed.push(candidate.message_id);
                canonical
                    .thread_collapsed
                    .append(&mut candidate.thread_collapsed);
                canonical
                    .near_duplicates
                    .append(&mut candidate.near_duplicates);
            }
            None => {
                candidate.thread_id = thread_id;
                if let Some(thread_id) = thread_id {
                    canonical_index.insert(thread_id, out.len());
                }
                out.push(candidate);
            }
        }
    }
    out
}

/// Collapse fused candidates whose bodies SimHash-fingerprint within
/// [`simhash::NEAR_DUP_HAMMING_THRESHOLD`] of each other (prd.md: "near-
/// duplicate bodies (bulk newsletters, quoted replies) collapse via SimHash
/// so one query doesn't return ten copies").
///
/// Unconditional, unlike [`collapse_threads`]: the task's acceptance bullet
/// lists it with no "optional" qualifier, and a near-duplicate is a
/// presentation problem independent of thread-mode — a newsletter resent
/// into two different threads is exactly the duplicate thread collapse
/// cannot catch.
///
/// # Canonical selection: highest-scoring first, by construction, not "newest"
///
/// `fused` must already be sorted best-first, as [`fuse_scores`] returns
/// it. Cluster leaders are assigned with a single forward pass over that
/// order: the first candidate to establish a cluster becomes its leader,
/// and every later candidate that matches an existing leader's fingerprint
/// joins it — the identical single-pass structure [`collapse_threads`] uses
/// above, and for the identical reason: because the scan order is
/// fused-score-descending, a cluster's leader is *always* its
/// highest-scoring member, by construction, with no separate "best score
/// seen so far" bookkeeping needed per cluster.
///
/// This is deliberately not prd.md's Stage 6 "canonical (usually newest)"
/// rule. That line describes *presentation* (task 32, which still has every
/// message id in `near_duplicates` to apply it from, independently, using
/// each member's own date) — collapsing a cluster to anything other than
/// its strongest evidence would silently demote the query's own best result
/// behind a weaker duplicate of it, corrupting both the ranking a user sees
/// and the `rrf_score` feature task 30 reads off `fused_score`. Picking the
/// highest-scoring member costs nothing here: `fused` is already in that
/// order, so "first in the scan" and "best in the cluster" are the same
/// candidate without extra bookkeeping, exactly mirroring
/// [`collapse_threads`]'s own canonical rule.
///
/// An absorbed candidate's own `thread_collapsed` (if thread collapse ran
/// before this and already folded siblings into it) and `near_duplicates`
/// (empty in the pipeline [`Fuser::fuse`] runs, where this is the only step
/// that ever populates it — but not guaranteed for a caller invoking this
/// function directly) are both merged into the survivor's `near_duplicates`
/// rather than dropped — otherwise a message collapsed by thread, whose
/// thread-canonical is *itself* later absorbed into a near-dup cluster,
/// would vanish from every list with no trace.
///
/// A message with no fingerprint (fewer than
/// [`simhash::MIN_TOKENS_FOR_FINGERPRINT`] body words, or no
/// `index_content` row — [`simhash::fingerprint`]) can never join or start a
/// cluster; it passes through unchanged.
#[must_use]
pub fn collapse_near_duplicates(
    fused: Vec<FusedCandidate>,
    meta: &BTreeMap<i64, MessageMeta>,
) -> Vec<FusedCandidate> {
    let fingerprints: BTreeMap<i64, u64> = fused
        .iter()
        .filter_map(|candidate| {
            let body = meta.get(&candidate.message_id)?.body.as_deref()?;
            simhash::fingerprint(body).map(|fp| (candidate.message_id, fp))
        })
        .collect();
    if fingerprints.len() < 2 {
        return fused;
    }

    let mut out: Vec<FusedCandidate> = Vec::with_capacity(fused.len());
    // Each existing leader's index into `out` and fingerprint, in creation
    // order, so a later candidate can be tested against every leader seen
    // so far.
    let mut leaders: Vec<(usize, u64)> = Vec::new();
    for mut candidate in fused {
        let Some(&fp) = fingerprints.get(&candidate.message_id) else {
            out.push(candidate);
            continue;
        };
        let matched = leaders
            .iter()
            .find(|(_, leader_fp)| simhash::is_near_duplicate(*leader_fp, fp))
            .map(|&(idx, _)| idx);
        match matched.and_then(|idx| out.get_mut(idx)) {
            Some(canonical) => {
                canonical.near_duplicates.push(candidate.message_id);
                canonical
                    .near_duplicates
                    .append(&mut candidate.thread_collapsed);
                canonical
                    .near_duplicates
                    .append(&mut candidate.near_duplicates);
            }
            None => {
                leaders.push((out.len(), fp));
                out.push(candidate);
            }
        }
    }
    out
}

/// Largest number of message ids [`Fuser::fetch_meta`] will look up in one
/// query, however many `fuse_scores` produced. `search.candidates_per_source`
/// defaults to 200 but prd.md sanctions up to `500` per source
/// ([`crate::index::fts::MAX_LIMIT`]) across all seven sources — `3,500` in
/// the worst case a *supported* config allows, comfortably past this cap.
/// SQLite's own bound-variable limit is far above `MAX_META_FETCH` in
/// practice, so this is a deliberate defensive ceiling, not a workaround for
/// SQLite's own limit. `fused` stays sorted best-first, so truncating keeps
/// the top-ranked candidates and simply leaves the tail uncollapsed rather
/// than failing the query.
const MAX_META_FETCH: usize = 2_000;

/// How much of each candidate's body text [`Fuser::fetch_meta`] pulls for
/// SimHash fingerprinting, in characters.
///
/// Two reasons this is capped rather than fetching the full
/// `index_content.text` row: memory (`MAX_META_FETCH` candidates × an
/// uncapped body is unbounded — a handful of large attachment-extraction or
/// export emails at tens of KB each, times a couple thousand candidates, is
/// real memory for one in-flight query) and CPU
/// ([`simhash::fingerprint`] is `O(text length)`, and near-dup collapse
/// already runs on a blocking thread precisely because that cost is real —
/// see [`Fuser::fuse`]'s doc comment). 4,000 characters is generous relative
/// to what near-dup detection actually needs: the shared content that makes
/// two messages near-duplicates (a quoted original, a forwarded body, a
/// newsletter template) is present from the start of the text in every
/// realistic case this module targets, so truncating the tail costs
/// fingerprint quality only for bodies long enough that the cap could never
/// have been the bottleneck anyway.
const MAX_BODY_CHARS_FOR_SIMHASH: i64 = 4_000;

/// Runs Stage 2 (prd.md, "Fusion & Dedup") end to end: [`fuse_scores`], then
/// optional [`collapse_threads`], then unconditional
/// [`collapse_near_duplicates`]. Holds a [`Database`] purely for the two
/// collapse steps' metadata lookup — `fuse_scores` itself needs no I/O and
/// is exposed as a free function precisely so it can be tested (and called)
/// without a database at all.
#[derive(Debug, Clone)]
pub struct Fuser {
    db: Database,
}

impl Fuser {
    /// Build a fuser over `db`.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Run the full Stage 2 pipeline.
    ///
    /// Never fails: a metadata lookup that errors or is cancelled degrades
    /// to skipping the collapse steps it feeds (every [`FusedCandidate`]
    /// keeps `thread_id: None`, `thread_collapsed`/`near_duplicates` empty)
    /// rather than failing the whole query — the same graceful-degradation
    /// contract [`crate::retrieve::Fanout::generate`] gives its own callers,
    /// and for the same reason: a superseded query's collapse step getting
    /// cancelled a few milliseconds in must never surface as a search
    /// failure when the fused ranking itself is already valid.
    ///
    /// # Near-dup collapse runs on the blocking pool
    ///
    /// [`collapse_near_duplicates`] fingerprints every candidate's body —
    /// `O(text length)` work, and at [`MAX_META_FETCH`] candidates with
    /// bodies near [`MAX_BODY_CHARS_FOR_SIMHASH`] that is real CPU time (not
    /// I/O), easily tens of milliseconds. Running it inline on this
    /// function's own task would hold a tokio worker thread for that whole
    /// span, exactly the "never block the runtime" rule the CPU-heavy work
    /// elsewhere in this codebase (`index::entities`, `index::extract`,
    /// `index::semantic`) already routes around via `spawn_blocking`. This
    /// does the same: the fingerprinting pass runs on the blocking pool, and
    /// is skipped entirely if `cancel` already fired before it would have
    /// started (checked, not threaded through — `collapse_near_duplicates`
    /// stays a plain synchronous function so it is still testable without a
    /// runtime at all; see the module docs).
    #[tracing::instrument(
        skip(self, candidates, plan, cfg, cancel),
        // `thread_collapse = thread_collapse`, not a bare `thread_collapse`:
        // see `retrieve::fanout::Fanout::generate`'s identical note — a bare
        // name here that also names an argument shadows the auto-recorded
        // argument with an empty placeholder rather than the real value.
        // The rest stay bare deliberately: they have no argument to shadow,
        // so each is an empty slot this function fills via `Span::record`
        // below — `thread_collapsed_n`/`near_dup_collapsed_n` exist because
        // "the result set changed size" is not enough to debug *why* a
        // message a user expected is missing; which step removed it is the
        // first question that gets asked.
        fields(
            candidates = candidates.len(),
            intent = ?plan.intent,
            fusion = ?cfg.fusion,
            thread_collapse = thread_collapse,
            prior_only_dropped,
            fused,
            thread_collapsed_n,
            near_dup_collapsed_n
        )
    )]
    pub async fn fuse(
        &self,
        candidates: Vec<Candidate>,
        plan: &QueryPlan,
        cfg: &SearchConfig,
        thread_collapse: bool,
        cancel: &CancellationToken,
    ) -> Vec<FusedCandidate> {
        let fused = fuse_scores(
            &candidates,
            plan.intent,
            cfg.fusion,
            cfg.rrf_k,
            &cfg.fusion_weights,
        );
        // Drop candidates only a "prior" source (recency/structured) found
        // when the query had real free text none of the free-text-matching
        // retrievers satisfied for them — see `drop_prior_only_candidates`'s
        // own docs for why this runs before the collapse steps below (a
        // candidate dropped here should not consume a metadata-fetch slot,
        // let alone occupy a thread's "+N" or a near-dup cluster's "N
        // similar" count for a match nobody asked for).
        let before_prior_drop = fused.len();
        let mut fused = drop_prior_only_candidates(fused, plan);
        tracing::Span::current().record("prior_only_dropped", before_prior_drop - fused.len());
        // `!fused.is_empty()`, not `fused.len() >= 2`: even a single
        // candidate benefits from the `thread_id` annotation below when
        // `thread_collapse` is off, and a lone candidate simply can't
        // collapse with anything — a caller should not see a different
        // `FusedCandidate` shape (`thread_id` populated or not) purely as a
        // function of how many other candidates happened to also match.
        if !fused.is_empty() {
            let ids: Vec<i64> = fused.iter().map(|c| c.message_id).collect();
            if let Some(meta) = self.fetch_meta(&ids, cancel).await {
                let before_threads = fused.len();
                if thread_collapse {
                    fused = collapse_threads(fused, &meta);
                } else {
                    // Not collapsing, but the lookup already has the answer
                    // for every candidate in hand — annotate it rather than
                    // leaving `thread_id: None` and forcing a second round
                    // trip on any consumer (task 30's thread-shape features)
                    // that wants it.
                    for candidate in &mut fused {
                        candidate.thread_id =
                            meta.get(&candidate.message_id).and_then(|m| m.thread_id);
                    }
                }
                tracing::Span::current().record("thread_collapsed_n", before_threads - fused.len());

                // `fused.len() >= 2`, re-checked here rather than trusted
                // from the outer guard: `collapse_threads` above can shrink
                // a 2+-candidate set down to 1, and `collapse_near_duplicates`
                // itself is a no-op below that size (see its own `< 2`
                // guard) — skipping the clone and the blocking-pool hop in
                // that case is free, not just tidy.
                if fused.len() >= 2 && !cancel.is_cancelled() {
                    let before_dedup = fused.len();
                    match tokio::task::spawn_blocking({
                        let fused = fused.clone();
                        move || collapse_near_duplicates(fused, &meta)
                    })
                    .await
                    {
                        Ok(collapsed) => fused = collapsed,
                        Err(join_error) => {
                            // Should not happen — `collapse_near_duplicates`
                            // has no panics — but a join failure must
                            // degrade like every other failure mode here,
                            // not lose the (still-valid) pre-collapse
                            // `fused` this branch already holds.
                            tracing::warn!(
                                %join_error,
                                "near-duplicate collapse task failed; skipping collapse"
                            );
                        }
                    }
                    tracing::Span::current()
                        .record("near_dup_collapsed_n", before_dedup - fused.len());
                }
            }
        }
        tracing::Span::current().record("fused", fused.len());
        fused
    }

    /// Batched `thread_id`/`date`/body-text lookup for `ids`, one round trip
    /// via a `messages` ⋈ `index_content` join. `None` on cancellation or
    /// any storage error — see [`Fuser::fuse`]'s doc comment for why that
    /// degrades rather than propagates.
    async fn fetch_meta(
        &self,
        ids: &[i64],
        cancel: &CancellationToken,
    ) -> Option<BTreeMap<i64, MessageMeta>> {
        if ids.is_empty() {
            return Some(BTreeMap::new());
        }
        let ids = if ids.len() > MAX_META_FETCH {
            tracing::warn!(
                len = ids.len(),
                cap = MAX_META_FETCH,
                "fused candidate set exceeds the metadata fetch cap; tail left uncollapsed"
            );
            &ids[..MAX_META_FETCH]
        } else {
            ids
        };

        let placeholders = (1..=ids.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT m.id, m.thread_id, COALESCE(m.date, m.internaldate), \
                    SUBSTR(ic.text, 1, {MAX_BODY_CHARS_FOR_SIMHASH}) \
             FROM messages m \
             LEFT JOIN index_content ic ON ic.message_id = m.id AND ic.part = 'body' \
             WHERE m.id IN ({placeholders})"
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
                    row.get::<_, Option<i64>>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
        .await;

        match result {
            Ok(Some(rows)) => Some(
                rows.into_iter()
                    .map(|(id, thread_id, date, body)| {
                        (
                            id,
                            MessageMeta {
                                thread_id,
                                date,
                                body,
                            },
                        )
                    })
                    .collect(),
            ),
            Ok(None) => {
                tracing::debug!(
                    "candidate metadata fetch cancelled; skipping thread/near-dup collapse"
                );
                None
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    "candidate metadata fetch failed; skipping thread/near-dup collapse"
                );
                None
            }
        }
    }
}

#[cfg(test)]
mod tests;
