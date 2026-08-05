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

pub mod lexical;

pub use lexical::LexicalRetriever;

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
    /// This source's own relevance score, oriented higher-is-better.
    pub score: f64,
    /// 1-based rank within this source's result list (1 = best).
    pub rank: u32,
}

/// Assign 1-based ranks to a list already sorted best-first, pairing each
/// `(message_id, score)` into a [`Candidate`].
///
/// Shared because every retriever ends its own scoring with exactly this
/// step: sort by source score, then number the result 1, 2, 3, .... Fusion
/// (task 29) needs the number, not just the order, so it is computed once
/// here rather than re-derived (`position + 1`) at every call site.
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
        })
        .collect()
}

#[cfg(test)]
mod tests;
