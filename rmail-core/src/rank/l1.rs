//! The cold-start implementation of [`super::Ranker`]: prd.md's Stage 4
//! "hand-tuned linear scorer," used "until enough feedback is collected, and
//! as the always-available fallback" once task 65's learned model exists.
//!
//! # Why a `HashMap<FeatureName, f64>`, not a struct or a `Vec<f64>`
//!
//! This task's own acceptance bullet is explicit: "Weights must be keyed by
//! `FeatureName`, never by positional index." A fixed struct (one field per
//! weighted feature, à la [`crate::config::Bm25Weights`]) would satisfy that
//! for exactly the seventeen features prd.md's formula names today, but
//! rejects — at the type level, before any override even runs — the
//! seventeen-plus-one problem task 65 exists to solve: a learned model tunes
//! weights for features the cold-start formula does not use at all (an
//! `Explain` report should still be able to show *some* weight for
//! `bm25_body` even before task 65 ships, once an operator decides to tune
//! it). [`Weights`] stores its table as a plain `HashMap<FeatureName, f64>`
//! — not because a fixed array indexed by a hand-assigned ordinal could not
//! *also* be exposed behind a name-keyed [`Weights::get`]/[`Weights::set`]
//! API (it could, and would even be faster on the hot path: a `HashMap`
//! lookup costs a hash, an array lookup costs an index), but because a
//! `HashMap` needs no *second* place, alongside [`FeatureName`] itself, that
//! assigns each variant a numeric slot and has to be kept in sync with it by
//! hand — the same kind of manually-maintained ordinal table this crate
//! already carries a couple of out of necessity (`features::vector`'s
//! private `source_serde::ordinal`, `match_field_ordinal`) and does not need
//! a third of here, since [`FeatureName`] is already `Copy + Eq + Hash`. The
//! map's own (unspecified, run-to-run-varying) iteration order is never
//! observed: [`Weights::score`] always iterates
//! [`crate::features::FeatureVector::as_pairs`]'s fixed, documented order and
//! *looks up* each name's weight, one point query at a time, rather than
//! iterating the map itself. That is also what keeps scoring itself
//! deterministic (see this module's `tests::score_is_a_pure_function_of_the_feature_vector`)
//! despite `HashMap`'s own iteration order being unspecified — a property
//! that would not hold if this module ever summed by walking the map.
//!
//! # The two-layer split with `config::RankWeights`
//!
//! [`crate::config::RankWeights`] is an untyped `BTreeMap<String, f64>` —
//! `config` cannot import [`FeatureName`] (the reverse dependency direction
//! already holds: [`crate::features::extract`] reads
//! [`crate::config::Bm25Weights`] from `config`), so it cannot validate a key
//! against the real feature set itself. [`Weights::from_config`] is where
//! that validation actually happens, string by string, against
//! [`FeatureName::ALL`] — an override key that matches no real feature name
//! is [`RankError::UnknownFeature`], and a syntactically-parsed-but-`NaN`/
//! `±inf` value (TOML accepts `nan`/`inf` literals) is
//! [`RankError::NonFiniteWeight`]; neither is a silently-dropped or
//! silently-corrupting entry. Silently ignoring either would be strictly
//! worse than an error: an operator who mistypes `bm25_subect` in
//! `rmail.toml`, or fat-fingers a value into `nan`, gets a config file that
//! parses clean and a ranking that either never changes or collapses to
//! `message_id` order with no error and no log line — see
//! [`L1Ranker::rank`]'s doc comment for why a `NaN` score cannot even be
//! detected downstream, only prevented here.
//!
//! Nothing in this crate calls [`Weights::from_config`] automatically yet —
//! no gRPC service builds a [`super::Ranker`] from a loaded
//! [`crate::config::Config`] today; that wiring is `SearchService`'s job
//! (task 33), not this task's. `Config::load` succeeding is therefore *not*
//! proof that `[search.rank_weights]` is well-formed; that proof only exists
//! once whatever builds the live `Ranker` actually calls
//! [`Weights::from_config`] and handles its `Result`.

use std::collections::HashMap;

use crate::config::RankWeights;
use crate::features::{CandidateFeatures, FeatureName, FeatureVector};
use crate::query::Intent;

use super::{RankedCandidate, Ranker};

/// prd.md's default for `search.top_k_rerank` — how many of Stage 4's
/// best-scoring candidates survive to Stage 5. Not read automatically by
/// [`L1Ranker::rank`] (see [`Ranker::rank`]'s doc comment for why `top_k` is
/// always a caller-supplied argument, never an internal config read); this
/// constant exists so tests and callers that just want "the PRD default"
/// have one canonical place to name it, cross-checked against
/// [`crate::config::SearchConfig::default`]'s own value by
/// `tests::default_top_k_matches_the_search_config_default` so the two can
/// never silently drift apart.
pub const DEFAULT_TOP_K: usize = 50;

/// [`Weights::from_config`]'s failure modes.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum RankError {
    /// An override in `[search.rank_weights]` (or any other source of
    /// [`crate::config::RankWeights`]) named a key that is not one of
    /// [`FeatureName::ALL`]'s stable strings.
    #[error(
        "unknown feature name {0:?} in rank-weight override — not one of FeatureName::ALL's stable strings"
    )]
    UnknownFeature(String),

    /// An override named a real feature but gave it a `NaN`/`±inf` value.
    /// TOML's float grammar accepts `nan`/`inf`/`-inf` literals, so this is
    /// a real, reachable input, not a defensive-only case: an unrejected
    /// non-finite weight would poison [`Weights::score`]'s sum, turning
    /// every candidate's score to `NaN` and silently collapsing
    /// [`L1Ranker::rank`]'s ordering to plain `message_id` order (its sort's
    /// `partial_cmp` fallback) with no error anywhere on the path.
    #[error("non-finite weight {value} for feature {name:?} in rank-weight override")]
    NonFiniteWeight {
        /// The feature name the override targeted.
        name: String,
        /// The rejected value.
        value: f64,
    },
}

impl From<RankError> for crate::error::Error {
    /// A bad rank-weight override is a malformed configuration value — the
    /// same [`crate::error::ErrorReason::InvalidArgument`] mapping
    /// `crate::error::Error`'s own `From<ConfigError>` impl
    /// (see [`crate::config::ConfigError`]) gives `ConfigError::Invalid` for
    /// the identical reason: the detail is safe to show a caller (a future
    /// `SetConfig` RPC wants to know *which* key was wrong), and the fix
    /// belongs on the caller's side, not the server's.
    fn from(err: RankError) -> Self {
        crate::error::Error::invalid_argument(err.to_string())
    }
}

/// prd.md's Stage 4 cold-start weight table (see the module docs for why
/// this is a name-keyed map rather than a struct or a positional list).
///
/// A feature with no entry contributes `0.0` to [`Weights::score`] — the
/// same "absent weight, absent contribution" convention
/// [`FeatureVector::as_pairs`]'s own docs describe for the two categorical
/// features ([`crate::features::MatchField`]/[`crate::retrieve::Source`]'s
/// ordinals, `best_match_field`/`best_source`) this cold-start formula does
/// not weight at all — a category has no sign a linear model could sensibly
/// assign without a per-category expansion this task does not build.
#[derive(Debug, Clone, PartialEq)]
pub struct Weights(HashMap<FeatureName, f64>);

impl Weights {
    /// prd.md's Stage 4 cold-start formula, verbatim:
    ///
    /// ```text
    /// score = 1.00 * rrf_score
    ///       + 0.90 * bm25_subject      + 0.35 * bm25_body
    ///       + 0.80 * cos_max_chunk     + 0.30 * cos_mean_chunk
    ///       + 0.60 * exact_phrase_hit  + 0.40 * term_coverage
    ///       + 0.50 * sender_affinity   + 0.30 * user_replied_thread
    ///       + 0.45 * recency_decay     + 0.25 * ai_priority
    ///       + 0.20 * is_flagged        + 0.15 * is_unread
    ///       + 0.15 * has_tag_match     + 0.20 * has_attachment_match
    ///       - 0.40 * is_newsletter     - 0.25 * is_automated (gated — see
    ///                                                          `bulk_downweight_suppressed`)
    /// ```
    ///
    /// Every other [`FeatureName`] (the fusion/temporal/structural/status
    /// features the formula does not mention — `bm25_from`, `age_days`,
    /// `thread_size`, ...) is absent here and contributes `0.0`, exactly as
    /// prd.md's own worked formula omits them.
    #[must_use]
    pub fn cold_start() -> Self {
        Self(
            [
                (FeatureName::RrfScore, 1.00),
                (FeatureName::Bm25Subject, 0.90),
                (FeatureName::Bm25Body, 0.35),
                (FeatureName::CosMaxChunk, 0.80),
                (FeatureName::CosMeanChunk, 0.30),
                (FeatureName::ExactPhraseHit, 0.60),
                (FeatureName::TermCoverage, 0.40),
                (FeatureName::SenderAffinity, 0.50),
                (FeatureName::UserRepliedThread, 0.30),
                (FeatureName::RecencyDecay, 0.45),
                (FeatureName::AiPriority, 0.25),
                (FeatureName::IsFlagged, 0.20),
                (FeatureName::IsUnread, 0.15),
                (FeatureName::HasTagMatch, 0.15),
                (FeatureName::HasAttachmentMatch, 0.20),
                (FeatureName::IsNewsletter, -0.40),
                (FeatureName::IsAutomated, -0.25),
            ]
            .into_iter()
            .collect(),
        )
    }

    /// [`Weights::cold_start`] with `overrides` (`[search.rank_weights]`)
    /// applied on top — a *sparse* patch, per [`crate::config::RankWeights`]'s
    /// own doc comment: an omitted key keeps its cold-start value rather
    /// than resetting to `0.0`.
    ///
    /// # Errors
    ///
    /// [`RankError::UnknownFeature`] if any override key is not one of
    /// [`FeatureName::ALL`]'s stable strings, or [`RankError::NonFiniteWeight`]
    /// if a key names a real feature but the value is `NaN`/`±inf` — see the
    /// module docs for why both reject rather than silently drop/poison the
    /// entry.
    pub fn from_config(overrides: &RankWeights) -> Result<Self, RankError> {
        let mut weights = Self::cold_start();
        for (key, value) in &overrides.0 {
            let name = FeatureName::ALL
                .into_iter()
                .find(|candidate| candidate.as_str() == key.as_str())
                .ok_or_else(|| RankError::UnknownFeature(key.clone()))?;
            if !value.is_finite() {
                return Err(RankError::NonFiniteWeight {
                    name: key.clone(),
                    value: *value,
                });
            }
            weights.set(name, *value);
        }
        Ok(weights)
    }

    /// `name`'s configured weight, or `0.0` if unconfigured (the struct
    /// docs' "absent weight, absent contribution" convention).
    #[must_use]
    pub fn get(&self, name: FeatureName) -> f64 {
        self.0.get(&name).copied().unwrap_or(0.0)
    }

    /// Set (or replace) one feature's weight.
    pub fn set(&mut self, name: FeatureName, weight: f64) {
        self.0.insert(name, weight);
    }

    /// prd.md's Stage 4 score for one candidate: `Σ weight(name) * value`
    /// over every [`FeatureVector::as_pairs`] pair, with the
    /// `is_newsletter`/`is_automated` terms zeroed under
    /// [`Intent::Navigational`] (see [`bulk_downweight_suppressed`]).
    ///
    /// Pure: reads only `self` and `features`, touches neither the clock nor
    /// the database, and returns the identical `f64` for the identical
    /// `(self, features, intent)` triple on every call — this task's
    /// acceptance bullet ("the score is a pure function of the feature
    /// vector") as code, pinned by
    /// `tests::score_is_a_pure_function_of_the_feature_vector`.
    #[must_use]
    pub fn score(&self, features: &FeatureVector, intent: Intent) -> f64 {
        features
            .as_pairs()
            .into_iter()
            .map(|(name, value)| self.effective_weight(name, intent) * value)
            .sum()
    }

    /// [`Weights::get`], with the intent gate from
    /// [`bulk_downweight_suppressed`] applied.
    fn effective_weight(&self, name: FeatureName, intent: Intent) -> f64 {
        if bulk_downweight_suppressed(name, intent) {
            0.0
        } else {
            self.get(name)
        }
    }
}

impl Default for Weights {
    /// [`Weights::cold_start`] — the unmodified PRD table.
    fn default() -> Self {
        Self::cold_start()
    }
}

/// Whether `name`'s bulk/automated down-weight is suppressed for `intent`.
///
/// # A real tension in prd.md's text, resolved by this task's own spec
///
/// prd.md's Stage 4 formula marks the two terms "(unless query is
/// topical/bulk)," and Stage 0 spells "exploratory" and "topical" as the
/// *same* intent name (`` `exploratory / topical` — "everything about the
/// office move" ``). Read in isolation, that parenthetical could as easily
/// be misread as "suppress under Exploratory" — the opposite of what this
/// function does. This task's own acceptance bullet resolves the reading
/// explicitly and names the exact test that pins it: "a newsletter ranks
/// lower under exploratory intent but is not down-weighted when the query is
/// navigational and names it" (see
/// `tests::newsletter_ranks_lower_under_exploratory_but_not_navigational`).
/// That is the spec this function implements, not the isolated prd.md
/// sentence read on its own — the same kind of explicit, argued resolution
/// of a prd.md wording gap this codebase already makes elsewhere (see
/// `features::name`'s module docs on the "fusion" feature group for the
/// precedent of naming the discrepancy rather than silently picking a side).
///
/// prd.md settles the direction elsewhere, which is the citation worth having
/// when this comes up again: its Stage 3 feature table describes the whole
/// `sender_reputation`/`is_newsletter`/`is_automated` group as
/// "**down-weight bulk/automated unless asked**". The penalty is the default
/// and suppression is the exception, granted when the user *asked* — which is
/// what a named, known-item query is. A review pass read the Stage 4
/// parenthetical the other way round; that line is why it does not hold.
///
/// The product reasoning behind the chosen direction: [`Intent::Navigational`]
/// is prd.md's own "known item" intent — Stage 0's worked example is
/// literally "the invoice Acme sent last week," a *specific, named* target.
/// If that named target happens to be a bulk/automated sender (a particular
/// newsletter issue, a shipping notification the user is specifically
/// looking for), penalizing it for being what it structurally is would
/// contradict the intent that says the user already knows what they want and
/// named it — so the down-weight is suppressed entirely for navigational
/// queries, not merely reduced. [`Intent::Exploratory`] queries have no such
/// named target ("everything about the office move" names no specific
/// sender), so an unrequested newsletter/automated hit surfacing in that
/// broader net is exactly what the down-weight exists to suppress.
///
/// # `Intent::Lookup` also suppresses `is_automated` — but not `is_newsletter`
///
/// prd.md's own [`Intent::Lookup`] examples — "tracking number for my
/// order", "AWS bill" — name exactly the message class
/// [`crate::features::extract`]'s automated-sender heuristic flags
/// (`noreply@`, `notifications@`, `alerts@`, `no-reply`, ...): a shipping
/// notification and a billing email are automated *by construction*.
/// Applying the full `is_automated` penalty under Lookup would down-weight
/// the exact answer the intent exists to surface, so it is suppressed here
/// too. `is_newsletter` stays penalized under Lookup — a promotional
/// newsletter is not what "AWS bill" is asking for the way an automated
/// billing notice is, so there is no equivalent argument for suppressing it.
fn bulk_downweight_suppressed(name: FeatureName, intent: Intent) -> bool {
    match name {
        FeatureName::IsNewsletter => intent == Intent::Navigational,
        FeatureName::IsAutomated => matches!(intent, Intent::Navigational | Intent::Lookup),
        _ => false,
    }
}

/// prd.md's Stage 4 cold-start scorer: a TOML-overridable linear model over
/// [`FeatureVector::as_pairs`], with intent-gated newsletter/automated
/// down-weighting. The [`Ranker`] implementation this task ships; task 65's
/// learned model is another, behind the identical trait (see the crate's
/// `rank` module docs).
#[derive(Debug, Clone, PartialEq)]
pub struct L1Ranker {
    weights: Weights,
}

impl L1Ranker {
    /// Build a ranker over `weights` — [`Weights::cold_start`] for the
    /// unmodified PRD table, or [`Weights::from_config`] to apply
    /// `[search.rank_weights]`'s overrides first.
    #[must_use]
    pub fn new(weights: Weights) -> Self {
        Self { weights }
    }

    /// The weight table this ranker scores with — read by task 33's
    /// `Explain` to show a per-feature contribution breakdown
    /// (`weight(name) * value` for each [`FeatureVector::as_pairs`] pair).
    #[must_use]
    pub fn weights(&self) -> &Weights {
        &self.weights
    }

    /// Score a single feature vector — the same pure function
    /// [`Ranker::rank`] applies to a whole batch before the top-K cut,
    /// exposed directly for a per-message re-score (task 33's `Explain`) and
    /// for tests that want one candidate's score without building a batch.
    #[must_use]
    pub fn score(&self, features: &FeatureVector, intent: Intent) -> f64 {
        self.weights.score(features, intent)
    }
}

impl Default for L1Ranker {
    /// A ranker over the unmodified PRD cold-start table
    /// ([`Weights::cold_start`]) — what a mailbox with no
    /// `[search.rank_weights]` overrides and no task-65 model actually runs.
    fn default() -> Self {
        Self::new(Weights::cold_start())
    }
}

impl Ranker for L1Ranker {
    /// `top_k` is auto-recorded (it names a parameter); `kept`/`top_score`
    /// start as empty slots this function fills via `Span::record` once the
    /// cut is known — the same pattern `fuse::Fuser::fuse`'s
    /// `thread_collapsed_n`/`near_dup_collapsed_n` fields use, for the
    /// identical reason: "the result set changed size" is not enough to
    /// debug a ranking a user is questioning; how many candidates Stage 4
    /// kept, and what the winning score was, are the first things worth
    /// logging on this path — task 33's `Explain` gives the per-candidate
    /// detail this span does not attempt to duplicate.
    #[tracing::instrument(
        skip(self, candidates),
        fields(candidates = candidates.len(), intent = ?intent, top_k, kept, top_score)
    )]
    fn rank(
        &self,
        candidates: &[CandidateFeatures],
        intent: Intent,
        top_k: usize,
    ) -> Vec<RankedCandidate> {
        let mut scored: Vec<RankedCandidate> = candidates
            .iter()
            .map(|candidate| RankedCandidate {
                message_id: candidate.message_id,
                score: self.score(&candidate.features, intent),
            })
            .collect();
        // Best-first, ties broken by `message_id` ascending. `partial_cmp`
        // can only return `None` for a `NaN` score, which
        // `FeatureVector`'s own `finite()` sanitization (see
        // `features::vector`'s module docs) guarantees never reaches this
        // sort for a feature *value*, and [`Weights::from_config`]'s
        // `NonFiniteWeight` rejection guarantees the same for a configured
        // *weight* — `Equal` is a defensive fallback that keeps this a total
        // order regardless, the same posture `fuse::fuse_scores`'s own sort
        // takes for the identical reason.
        scored.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.message_id.cmp(&b.message_id))
        });
        scored.truncate(top_k);
        tracing::Span::current().record("kept", scored.len());
        if let Some(top) = scored.first() {
            tracing::Span::current().record("top_score", top.score);
        }
        scored
    }
}

#[cfg(test)]
mod tests;
