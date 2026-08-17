//! Ranking: Stage 4 (fast, over features) and Stage 5 (expensive, over
//! text).
//!
//! [`Ranker`]/[`l1`] are prd.md's "Stage 4 — L1 Ranker (fast, learned)":
//! turning task 30's per-candidate [`crate::features::FeatureVector`]s into
//! one best-first, top-K list. [`l2`] is Stage 5, the optional reranker that
//! re-orders that top-K by *reading the messages* — a local ONNX
//! cross-encoder or a Claude listwise pass. The two are deliberately
//! asymmetric: Stage 4 is a pure, synchronous, always-available function of
//! the feature vectors (see below), while Stage 5 is asynchronous, does I/O,
//! and is allowed to be absent — every one of its failure modes returns
//! Stage 4's order unchanged. That is why Stage 4's contract is a complete
//! ranking rather than a pre-filter.
//!
//! Everything below is about Stage 4; [`l2`]'s own module docs cover Stage 5.
//!
//! # One trait, two eras of implementation
//!
//! prd.md is explicit that Stage 4 has two lives: "gradient-boosted decision
//! trees... when a trained model exists; otherwise a hand-tuned **linear
//! scorer** (cold-start, below)." [`Ranker`] is the seam between them —
//! anything that can turn a batch of feature vectors plus the query's
//! classified intent into a scored, truncated list implements it. [`l1`]'s
//! [`l1::L1Ranker`] is the cold-start implementation this task ships: a
//! deterministic, TOML-overridable linear scorer with no training step and
//! no state beyond its weight table. Task 65's learned model is the other
//! implementation — same trait, same call sites in `rmaild`, a different
//! `struct` behind `Box<dyn Ranker>` (or an `Arc`, however the caller wants
//! to share it across requests). Nothing downstream of [`Ranker::rank`]
//! needs to know or care which one is live.
//!
//! # Why the trait lives here and not in `l1`
//!
//! [`l1`] is one *implementation* of Stage 4; [`Ranker`] and
//! [`RankedCandidate`] are Stage 4's *contract*, shared by every
//! implementation task 65 will ever add. Defining the trait inside `l1`
//! would make a future `rank::gbdt` module depend on `rank::l1` just to
//! implement the same interface — exactly backwards, since the two
//! implementations should know nothing about each other. This mirrors
//! `crate::embed`'s own split: the `Embedder` trait lives in `embed::mod`,
//! and `embed::local`/`embed::hosted`/`embed::fallback` are interchangeable
//! implementations of it.
//!
//! # Why the trait is synchronous, unlike `Embedder`
//!
//! [`crate::embed::Embedder::embed`] is `async` because it may cross a
//! process boundary (a hosted provider's HTTP call) or load a model file.
//! [`Ranker::rank`] never does either: prd.md's whole point for this stage
//! is "pure Rust, no FFI on the hot path," and [`l1::L1Ranker`] backs that
//! up literally — no I/O, no lock, no `.await` anywhere in its call graph.
//! A future GBDT implementation (task 65) still fits this contract: model
//! inference over an already-loaded tree ensemble is CPU work, not I/O, the
//! same category `l1::L1Ranker`'s dot product is. Keeping the trait
//! synchronous is what lets every acceptance test call it directly, with no
//! runtime, and is itself part of what "pure function of the feature
//! vector" (this task's own acceptance bullet) means in practice.
//!
//! # What crosses the trait boundary, and what does not
//!
//! [`Ranker::rank`] takes `&[CandidateFeatures]` — task 30's own output
//! type, straight from [`crate::features::FeatureExtractor::extract_at`] —
//! rather than a bespoke ranker-local shape. [`RankedCandidate`], the
//! output, is deliberately thin: a `message_id` and a `score`, nothing
//! else. A caller that needs a chosen result's full feature vector (task
//! 32's diversification, task 33's `Explain`) already has the
//! `Vec<CandidateFeatures>` this function was given — the same list it can
//! index by `message_id` — so `RankedCandidate` does not have to carry a
//! second copy of it forward. This is the same single-responsibility split
//! [`crate::fuse::FusedCandidate`]/[`crate::features::CandidateFeatures`]
//! already draw between "what stage N computed" and "what stage N+1 needs
//! restated."

pub mod l1;
pub mod l2;
pub mod train;

use crate::features::CandidateFeatures;
use crate::query::Intent;

/// One candidate after Stage 4 scoring: its identity plus the scalar score
/// whichever [`Ranker`] produced it — comparable only against other
/// [`RankedCandidate`]s from the *same* `rank` call (a fresh `Ranker`
/// implementation, a reweighted [`l1::Weights`], or even a different
/// [`Intent`] gate, all change what the number means).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RankedCandidate {
    /// The matched message (`messages.id`), unchanged from the
    /// [`CandidateFeatures::message_id`] this score was computed from.
    pub message_id: i64,
    /// This candidate's Stage 4 score. Higher is better; the scale is
    /// specific to whichever [`Ranker`] produced it (an [`l1::L1Ranker`]'s
    /// linear-combination scale is not the same as a future GBDT model's
    /// leaf-sum scale) — never compared across two different rankers or
    /// persisted as if it meant something on its own.
    pub score: f64,
}

/// prd.md's Stage 4 seam: "scores all candidates... keeps the top-K" is
/// every implementation's contract, whichever model produces the score.
/// [`l1::L1Ranker`] is the cold-start implementation this task ships; task
/// 65's learned model hot-swaps behind the identical trait — see the module
/// docs for why the trait lives here rather than inside [`l1`].
pub trait Ranker: Send + Sync + std::fmt::Debug {
    /// Score every candidate in `candidates` and return the best `top_k`,
    /// best-first (descending by [`RankedCandidate::score`], ties broken by
    /// `message_id` ascending so the result is a deterministic function of
    /// its inputs even when two candidates score identically).
    ///
    /// `candidates` is exactly task 30's Stage 3 output — every fused
    /// candidate that survived to feature extraction, not a pre-truncated
    /// list; deciding what to keep is this call's own job, per prd.md's
    /// Stage 4 description ("[the L1 ranker] scores **all** fused
    /// candidates... and keeps the top-K"). `top_k` is a caller-supplied cut
    /// (`search.top_k_rerank`, prd.md's default `50`) rather than a config
    /// value read internally, so a `Ranker` implementation stays a pure
    /// function of its arguments — no hidden dependency on
    /// [`crate::config::SearchConfig`] that a test would have to construct
    /// just to call this method.
    fn rank(
        &self,
        candidates: &[CandidateFeatures],
        intent: Intent,
        top_k: usize,
    ) -> Vec<RankedCandidate>;
}
