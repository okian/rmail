//! Candidate generation: retrievers that turn a query into a ranked,
//! source-scored candidate list (prd.md, "Stage 1 — Candidate Generation").
//!
//! # Why a shared result type
//!
//! Every retriever here disagrees about almost everything — a BM25 score and
//! a cosine similarity are not the same unit, let alone a comparable
//! magnitude — so fusion (task 29) cannot combine them by score. What it
//! *can* combine is each source's own **rank**: weighted reciprocal rank
//! fusion sums `w_s / (k + rank_s(m))` across sources and never looks at
//! `score` at all. [`Candidate`] carries both fields anyway, because the raw
//! score does not stop mattering once fusion has run — it survives into
//! feature extraction (task 30) as a ranking feature in its own right
//! ("ranked #3 in lexical" and "barely beat the BM25 floor" are different
//! signals at the same rank). Neither field is derivable from the other, so
//! both are computed once, here, and carried through unchanged.
//!
//! # What lives here vs. in each retriever
//!
//! This module is deliberately thin: the [`Source`] enum and [`Candidate`]
//! type are the contract every retriever — this task's [`lexical`], and task
//! 28's dense/fuzzy/entity/structured/prefix/recency siblings — returns
//! against, plus the one piece of arithmetic every one of them needs
//! ([`rank_by_score`]): turning a scored, best-first list into ranked
//! candidates is identical work for every source and not worth
//! reimplementing seven times. Everything specific to *how* a source finds
//! its candidates (FTS5 `MATCH` construction, kNN, trigram, ...) stays in
//! that source's own module.
//!
//! # `hard_filters`, not `scope` — the integration decision task 26 left open
//!
//! [`QueryPlan::scope`](crate::query::QueryPlan::scope) and
//! [`QueryPlan::hard_filters`](crate::query::QueryPlan::hard_filters) both
//! describe `account:`/`in:` constraints, and task 26's own docs flag the
//! overlap: `hard_filters` is "authoritative for matching", `scope` is "a
//! routing convenience" for a caller that wants to pick a shard/connection
//! without scanning filters for two specific operators. This build has no
//! such caller — [`crate::storage::Database`] is one SQLite file with no
//! per-account or per-mailbox routing — so every retriever in this task
//! ([`filtermask`], and through it [`dense`], [`entity`], [`fuzzy`],
//! [`prefix`], [`recency`], [`structured`]) reads **only** `hard_filters`.
//! `scope` is not read anywhere in `retrieve::`. This is the one place that
//! matters: had two retrievers each applied `account:`/`in:` their own way —
//! one from `scope`, another from `hard_filters` — a negated `-in:Spam` would
//! silently disagree between them (`scope` only ever holds the *positive*
//! `in:`/`account:` filters — see its doc comment — so a retriever reading it
//! for exclusion would get the wrong answer, or none at all). Reading
//! `hard_filters` uniformly means negation, and every other operator, is
//! handled in exactly one place per retriever family: [`filtermask::compile`].
//!
//! # One filter compiler, six retrievers
//!
//! [`dense`], [`entity`], [`fuzzy`], [`prefix`], [`recency`], and
//! [`structured`] all gate on the same hard-filter mask, so
//! [`filtermask::compile`] exists to compute it once per retriever call
//! rather than seven times differently. It duplicates a fair amount of
//! [`lexical::classify`](lexical) — see [`filtermask`]'s own docs for why
//! that duplication is the smaller risk than the alternatives.
//!
//! # Cancellation
//!
//! Every retriever's database work runs through [`cancel::interruptible_read`],
//! which turns a superseded query's [`CancellationToken`](tokio_util::sync::CancellationToken)
//! into a real SQLite `interrupt()` call rather than merely walking away from
//! an unread future — see that module's docs for why a plain `spawn_blocking`
//! future is not enough. [`fanout::Fanout`] is what threads one token through
//! every source concurrently and degrades a disabled or failing source to no
//! candidates instead of failing the whole query.

pub mod cancel;
pub mod dense;
pub mod entity;
pub mod fanout;
pub mod filtermask;
pub mod fuzzy;
pub mod lexical;
pub mod prefix;
pub mod recency;
pub mod structured;

pub use dense::DenseRetriever;
pub use entity::EntityRetriever;
pub use fanout::Fanout;
pub use fuzzy::FuzzyRetriever;
pub use lexical::LexicalRetriever;
pub use prefix::PrefixRetriever;
pub use recency::RecencyRetriever;
pub use structured::StructuredRetriever;

/// Which retriever produced a [`Candidate`].
///
/// Mirrors prd.md's Stage 1 retriever table exactly (`Lexical BM25`, `Dense
/// vector kNN`, `Fuzzy`, `Entity match`, `Structured filter`, `Prefix /
/// autocomplete`, `Recency prior`) so a fused result's source list (task 29)
/// and a `RankExplanation` (task 33) can name a source without a separate
/// translation table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Source {
    /// Field-weighted BM25 over `fts_messages` (this task).
    Lexical,
    /// Cosine kNN over chunk/message embeddings (task 28).
    Dense,
    /// nucleo subsequence + trigram matching (task 28).
    Fuzzy,
    /// Exact/normalized entity match (task 28).
    Entity,
    /// Structured SQL predicate. A hard gate rather than a ranking source in
    /// its own right, but still worth naming so `--explain` can show that a
    /// filter matched (task 28).
    Structured,
    /// FTS5 prefix / `finder_index` autocomplete (task 28).
    Prefix,
    /// Recency-decay prior (task 28).
    Recency,
}

/// One candidate as scored by a single retrieval source.
///
/// `rank` is 1-based and assigned within this source's own result list
/// only — it says nothing about a candidate's standing against another
/// source's candidates, which is exactly why fusion needs it labeled
/// per-source rather than as one global number.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// The matched message (`messages.id`).
    pub message_id: i64,
    /// Which retriever produced this candidate.
    pub source: Source,
    /// This source's own relevance score, oriented higher-is-better. For
    /// [`Source::Dense`] this is the **max** chunk cosine similarity — the
    /// value the candidate is ranked by — mirroring prd.md's Stage 3 feature
    /// table, where `cos_max_chunk` (not the mean) is the primary semantic
    /// signal.
    pub score: f64,
    /// 1-based rank within this source's result list (1 = best).
    pub rank: u32,
    /// The **mean** chunk cosine similarity, alongside `score`'s max —
    /// prd.md's dense retriever keeps both ("chunk-level dense retrieval...
    /// kNN returns chunks, deduped to their parent message keeping `max` and
    /// `mean` chunk similarity as separate features"). `None` for every
    /// source except [`Source::Dense`]: no other retriever produces more
    /// than one number per candidate, and threading an always-`None` field
    /// through six retrievers that have nothing to put in it would be a
    /// worse shape than one retriever-specific field on the shared type.
    /// Task 30's feature extraction is what reads this back out as
    /// `cos_mean_chunk`.
    pub mean_score: Option<f64>,
}

/// Assign 1-based ranks to a list already sorted best-first, pairing each
/// `(message_id, score)` into a [`Candidate`].
///
/// Shared because every retriever ends its own scoring with exactly this
/// step: sort by source score, then number the result 1, 2, 3, .... Fusion
/// (task 29) needs the number, not just the order, so it is computed once
/// here rather than re-derived (`position + 1`) at every call site.
/// `mean_score` is always `None` — the only source that has one
/// ([`Source::Dense`]) computes candidates itself rather than through this
/// helper, since it needs to attach a second number this function's
/// `(id, score)` pairs have no room for.
#[must_use]
pub fn rank_by_score(source: Source, scored: Vec<(i64, f64)>) -> Vec<Candidate> {
    scored
        .into_iter()
        .enumerate()
        .map(|(i, (message_id, score))| Candidate {
            message_id,
            source,
            score,
            // A result page is bounded well under u32::MAX (see
            // `index::fts::MAX_LIMIT`); saturating rather than panicking
            // keeps a future, larger page from becoming a panic instead of a
            // merely-suspicious rank number.
            rank: u32::try_from(i + 1).unwrap_or(u32::MAX),
            mean_score: None,
        })
        .collect()
}

#[cfg(test)]
mod tests;
