//! Offline training and the model hot-swap (task 65; prd.md, "Training").
//!
//! > *A nightly (or on-demand) local job trains the L1 GBDT / updates linear
//! > weights on the accumulated pairs, evaluates on a held-out slice, and
//! > **hot-swaps** the model only if offline NDCG improves (guardrail against
//! > regressions). Old model kept for rollback.*
//!
//! Search is this product's first feature, which makes this module the one
//! that can quietly ruin it. A trainer that ships a model scoring well on its
//! own training signal and worse for the person using the mailbox is the
//! failure mode, and it is invisible without a guardrail that actually
//! refuses things. So the shape of this module is: the training is the easy
//! half, and everything else here exists to make the refusal trustworthy.
//!
//! # The four things that make the verdict mean something
//!
//! 1. **The held-out slice is split by query group, not by logged query.**
//!    A keystroke-driven search box logs the same search several times a
//!    minute and a user re-runs "acme invoice" every month; splitting by
//!    `query_id` would put near-identical impressions of one search on both
//!    sides of the line and the guardrail would be measuring memorization.
//!    [`labels::is_holdout`] keys on `search_log.norm_hash`, so every
//!    impression of a given search text lands wholly on one side.
//! 2. **The evaluator is the one that already existed.** [`crate::eval`] owns
//!    NDCG@10 and [`crate::eval::replay::shadow`] owns "how would this ranker
//!    have ordered what the user was actually shown". This module builds
//!    impressions and calls them. A second implementation of NDCG living next
//!    to the thing it is supposed to gate would be a metric nobody else's
//!    tests cover.
//! 3. **Both sides are measured the same way.** The baseline is not the order
//!    the user saw — that came from whatever ranker was live at log time,
//!    which may be neither of the two models being compared. It is the
//!    *currently live* model, shadow-scored over the same held-out
//!    impressions the candidate is scored over — and over only the ones that
//!    carry an engagement, since a search nobody clicked scores `0.0` for
//!    every model and including it would quietly rescale the guardrail's
//!    threshold (see [`HoldoutSlice`]). Anything else compares a model
//!    against a moving target.
//! 4. **The comparison is conservative by construction.** The held-out
//!    judgments are clicks, and those clicks happened under the incumbent's
//!    ordering, so the incumbent has a structural advantage:
//!    [`crate::eval::replay`] deliberately does not correct position bias
//!    (its own module docs say so). That tilts the guardrail toward refusing,
//!    which for a guardrail is the right direction to be wrong in.
//!
//! # Why linear weights rather than the GBDT prd.md also names
//!
//! prd.md offers both — "trains the L1 GBDT / updates linear weights" — and
//! this build does the second. Three reasons, in order of weight:
//!
//! - A local mailbox's feedback log is small. The corpus a nightly run has to
//!   work with is on the order of a few thousand impressions and a few
//!   hundred clicks, bounded by `[search.feedback]` retention. Thirty-four
//!   features and a few hundred effective observations is squarely in the
//!   regime where a linear model regularized toward a hand-tuned prior beats
//!   a tree ensemble that has enough capacity to memorize the log.
//! - The artifact stays a [`l1::Weights`] table, so the learned model *is* an
//!   [`l1::L1Ranker`] — which means `Explain` keeps producing a per-feature
//!   contribution breakdown after personalization turns on, prd.md's
//!   cold-start fallback is the same code path rather than a second one, and
//!   the hot-swap seam is a table swap rather than a new inference engine on
//!   the hot path.
//! - It is honest about what the guardrail can prove. With this little data,
//!   a held-out NDCG comparison can distinguish "this weight table is better"
//!   from "this one is worse". It cannot referee a model class with the
//!   capacity to fit the holdout by accident.
//!
//! The seam for the GBDT is still open and is where it always was:
//! [`crate::rank::Ranker`] is a trait, `ranker_model.kind` is a column, and
//! [`model::MODEL_KIND_LINEAR`] is one value of it.
//!
//! # One model per daemon, not one per account
//!
//! prd.md says "personalization is per-user and per-mailbox-profile", and
//! this build reads the first half literally and the second half as a
//! deliberate simplification. A daemon serves one person's mailboxes, so one
//! model *is* per-user; `ranker_model` has a single `active` row and
//! `SearchApi` a single live ranker, which is also what task 65's own
//! acceptance bullet describes (`ranker_model.active`, singular).
//!
//! Per-account models were considered and rejected on data, not on effort:
//! the log is bounded by `[search.feedback]` retention, splitting it N ways
//! puts every account below `min_queries`, and the first thing an operator
//! with two accounts would see is that personalization stopped working
//! entirely. The columns to change that later exist — `search_log.account_id`
//! is recorded per query — so this is a filter and a keyed `active` flag away
//! if a real multi-account mailbox ever shows the models should differ.
//!
//! # What runs where
//!
//! Training is CPU-bound, not I/O-bound. Reading the log is one
//! [`Database::read`] (itself `spawn_blocking`-backed); decoding, labelling,
//! fitting and scoring are one [`tokio::task::spawn_blocking`], so a nightly
//! run never occupies a runtime worker. `spawn_blocking` tasks cannot be
//! aborted, so cancellation is cooperative: the token is checked per logged
//! query while decoding and per epoch while fitting, which bounds a
//! shutdown's wait by one page of JSON or one gradient pass.

pub(crate) mod data;
pub mod fit;
pub mod labels;
pub mod model;
pub mod store;

#[cfg(test)]
mod tests;

use tokio_util::sync::CancellationToken;

use crate::cache::ResultCache;
use crate::config::TrainingConfig;
use crate::error::Error;
use crate::eval::replay::{shadow, Engagement, EngagementAction, Impression};
use crate::features::CandidateFeatures;
use crate::feedback::ActionKind;
use crate::query::Intent;
use crate::rank::l1::{L1Ranker, Weights};
use crate::rank::Ranker as _;
use crate::storage::Database;

use fit::FitParams;
use labels::{is_holdout, pairs_for, LoggedQuery, PreferencePair};
use model::MODEL_KIND_LINEAR;
use store::{ModelStatus, NewModel};

pub use model::{
    decode, encode, ActiveRanker, EncodedModel, ModelError, MODEL_FORMAT_VERSION,
    MODEL_KIND_LINEAR as LINEAR_MODEL_KIND,
};
pub use store::{ModelRecord, ModelStatus as StoredModelStatus};

/// Everything that can stop a training run.
#[derive(Debug, thiserror::Error)]
pub enum TrainError {
    /// `[search.training]` holds a value that cannot describe a training run.
    /// Raised at construction, so a daemon fails to start rather than
    /// discovering it at 3 a.m.
    #[error("invalid [search.training] configuration: {0}")]
    InvalidConfig(String),

    /// Too few usable logged queries. Not an error the operator can fix by
    /// retrying — it is the normal state of a new mailbox.
    #[error(
        "not enough logged feedback to train: {found} usable queries, \
         search.training.min_queries is {needed}"
    )]
    InsufficientQueries {
        /// Usable queries in the log.
        found: usize,
        /// What the configuration requires.
        needed: u32,
    },

    /// Enough queries, but too few of them produced a preference pair — a log
    /// full of searches nobody clicked on.
    #[error(
        "not enough preference pairs to train: {found}, \
         search.training.min_pairs is {needed}"
    )]
    InsufficientPairs {
        /// Pairs derived from the training slice.
        found: usize,
        /// What the configuration requires.
        needed: u32,
    },

    /// The held-out slice cannot referee anything: too few of its queries
    /// carry an engagement, so NDCG over it is `0.0` for every model and the
    /// comparison would be a tie rather than a measurement.
    #[error(
        "the held-out slice has only {engaged} distinct query groups with any \
         engagement (across {total} logged searches), \
         search.training.min_eval_queries is {needed}; refusing to judge a \
         model on it"
    )]
    DegenerateHoldout {
        /// Distinct held-out query *groups* with at least one positive
        /// engagement — repeats of one search text count once, since they are
        /// not independent evidence.
        engaged: usize,
        /// Held-out logged searches in total.
        total: usize,
        /// What the configuration requires.
        needed: u32,
    },

    /// Gradient descent left the finite range — a `learning_rate` too large
    /// for this corpus. Refused rather than persisted: a table with a
    /// non-finite weight scores every candidate `NaN`, which
    /// `L1Ranker::rank`'s sort answers with silent `message_id` order.
    #[error(
        "training diverged; no model was written (try a smaller search.training.learning_rate)"
    )]
    Diverged,

    /// The run was cancelled — daemon shutdown, or a caller that went away.
    #[error("training was cancelled")]
    Cancelled,

    /// A rollback named a model id that does not exist.
    #[error("no ranker model with id {0}")]
    UnknownModel(i64),

    /// A rollback named a model the guardrail refused. See [`store`]'s module
    /// docs for why this is not merely discouraged.
    #[error(
        "ranker model {0} was refused by the regression guardrail and can never be \
         activated; train again on more feedback instead"
    )]
    RefusedModel(i64),

    /// A stored model could not be turned back into a runnable weight table.
    #[error(transparent)]
    Model(#[from] ModelError),

    /// A storage failure.
    #[error(transparent)]
    Storage(#[from] crate::StorageError),

    /// The blocking task carrying the fit failed to join.
    #[error("training task failed: {0}")]
    Task(String),
}

impl From<TrainError> for Error {
    fn from(err: TrainError) -> Self {
        match err {
            TrainError::InvalidConfig(_) => Error::invalid_argument(err.to_string()),
            TrainError::InsufficientQueries { .. }
            | TrainError::InsufficientPairs { .. }
            | TrainError::DegenerateHoldout { .. }
            | TrainError::RefusedModel(_) => Error::FailedPrecondition(err.to_string()),
            TrainError::UnknownModel(_) => Error::not_found(err.to_string()),
            TrainError::Cancelled => Error::Cancelled(err.to_string()),
            TrainError::Diverged | TrainError::Task(_) => Error::internal(err.to_string()),
            TrainError::Model(inner) => Error::from(inner),
            TrainError::Storage(inner) => Error::from(inner),
        }
    }
}

/// [`TrainingConfig`], validated into the shape a run actually reads.
///
/// Validation rather than clamping: a `holdout_percent` of `0` clamped to
/// something sane would train against a guardrail measuring nothing, and the
/// operator who typed it would never find out. Every field below is rejected
/// at daemon startup instead.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TrainingParams {
    /// Fewest usable logged queries before a run will train.
    pub min_queries: u32,
    /// Fewest preference pairs before a run will train.
    pub min_pairs: u32,
    /// Percentage of query groups held out, `1..=90`.
    pub holdout_percent: u32,
    /// Fewest engaged held-out queries before the verdict is trusted.
    pub min_eval_queries: u32,
    /// Newest logged queries one run reads.
    pub max_training_queries: u32,
    /// Examination-propensity exponent — see [`labels`].
    pub position_bias_eta: f64,
    /// Inverse-propensity clipping ceiling.
    pub max_propensity_weight: f64,
    /// Held-out NDCG@10 gain the candidate must clear to go live.
    pub min_ndcg_gain: f64,
    /// Optimizer knobs.
    pub fit: FitParams,
    /// Model rows retained (excluding the live one).
    pub max_models: u32,
}

impl TrainingParams {
    /// Validate `[search.training]`.
    ///
    /// # Errors
    ///
    /// [`TrainError::InvalidConfig`] naming the offending field.
    pub fn from_config(config: &TrainingConfig) -> Result<Self, TrainError> {
        fn require(condition: bool, message: &str) -> Result<(), TrainError> {
            if condition {
                Ok(())
            } else {
                Err(TrainError::InvalidConfig(message.to_owned()))
            }
        }

        require(
            (1..=90).contains(&config.holdout_percent),
            "holdout_percent must be between 1 and 90: 0 leaves the guardrail nothing to \
             measure on, and more than 90 leaves the trainer nothing to learn from",
        )?;
        require(
            config.position_bias_eta.is_finite() && config.position_bias_eta >= 0.0,
            "position_bias_eta must be finite and non-negative; a negative exponent would \
             weight a click at the top of the page *more* than one at the bottom, which \
             inverts the correction",
        )?;
        require(
            config.max_propensity_weight.is_finite() && config.max_propensity_weight >= 1.0,
            "max_propensity_weight must be finite and at least 1.0 (rank 1's own weight)",
        )?;
        require(
            config.min_ndcg_gain.is_finite() && (0.0..=1.0).contains(&config.min_ndcg_gain),
            "min_ndcg_gain must be finite and between 0.0 and 1.0; NDCG@10 is a fraction, \
             so a larger threshold can never be met and would disable the swap entirely",
        )?;
        require(config.epochs >= 1, "epochs must be at least 1")?;
        require(
            config.learning_rate.is_finite() && config.learning_rate > 0.0,
            "learning_rate must be finite and positive",
        )?;
        require(
            config.l2.is_finite() && config.l2 >= 0.0,
            "l2 must be finite and non-negative",
        )?;
        require(
            config.max_training_queries >= 1,
            "max_training_queries must be at least 1",
        )?;
        require(
            config.min_eval_queries >= 1,
            "min_eval_queries must be at least 1; a guardrail measured on nothing is not a \
             guardrail",
        )?;
        require(config.max_models >= 1, "max_models must be at least 1")?;

        Ok(Self {
            min_queries: config.min_queries,
            min_pairs: config.min_pairs,
            holdout_percent: config.holdout_percent,
            min_eval_queries: config.min_eval_queries,
            max_training_queries: config.max_training_queries,
            position_bias_eta: config.position_bias_eta,
            max_propensity_weight: config.max_propensity_weight,
            min_ndcg_gain: config.min_ndcg_gain,
            fit: FitParams {
                epochs: config.epochs,
                learning_rate: config.learning_rate,
                l2: config.l2,
            },
            max_models: config.max_models,
        })
    }
}

impl Default for TrainingParams {
    /// The validated form of [`TrainingConfig::default`], which is by
    /// construction valid — the fallback only exists so this impl is total.
    fn default() -> Self {
        let config = TrainingConfig::default();
        Self::from_config(&config).unwrap_or(Self {
            min_queries: config.min_queries,
            min_pairs: config.min_pairs,
            holdout_percent: 25,
            min_eval_queries: 10,
            max_training_queries: config.max_training_queries,
            position_bias_eta: 1.0,
            max_propensity_weight: 10.0,
            min_ndcg_gain: 0.005,
            fit: FitParams {
                epochs: 60,
                learning_rate: 0.1,
                l2: 0.01,
            },
            max_models: 20,
        })
    }
}

/// What one training run did, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct TrainingReport {
    /// Logged queries in the training slice.
    pub train_queries: usize,
    /// Preference pairs derived from them.
    pub train_pairs: usize,
    /// Logged searches in the held-out slice.
    pub holdout_queries: usize,
    /// Distinct held-out query *groups* that carried a positive engagement —
    /// the effective sample size of the comparison below, and the number
    /// `search.training.min_eval_queries` is checked against. Repeats of one
    /// search text count once; see [`HoldoutSlice::engaged_groups`].
    pub holdout_engaged: usize,
    /// Logged queries dropped as unreplayable while decoding.
    pub skipped_queries: usize,
    /// Held-out NDCG@10 of the model that was live.
    pub baseline_ndcg_at_10: f64,
    /// Held-out NDCG@10 of the candidate.
    pub candidate_ndcg_at_10: f64,
    /// The margin it had to clear.
    pub min_gain: f64,
    /// Whether the guardrail let it through.
    pub accepted: bool,
    /// Whether this run was forbidden from changing anything.
    pub dry_run: bool,
    /// Propensity-weighted pairwise loss before and after fitting.
    pub initial_loss: f64,
    /// See [`TrainingReport::initial_loss`].
    pub final_loss: f64,
    /// The `ranker_model` row this run wrote, if it wrote one.
    pub model_id: Option<i64>,
    /// Which model is live now.
    pub active_model_id: Option<i64>,
    /// One line an operator can read without the numbers.
    pub verdict: String,
}

/// The model history plus what is actually running.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelHistory {
    /// Newest first.
    pub models: Vec<ModelRecord>,
    /// The model the *live* ranker is running, `None` for the deterministic
    /// scorer.
    ///
    /// Read from the in-process handle rather than from the `active` column,
    /// and the two can disagree in exactly one case: a stored model this
    /// build cannot decode (see [`ModelError`]) leaves the flag on disk and
    /// the deterministic scorer running. That discrepancy is the honest
    /// report of the situation and the thing that tells an operator to roll
    /// back, which is also the fix.
    pub active_model_id: Option<i64>,
}

/// What a rollback did.
#[derive(Debug, Clone, PartialEq)]
pub struct RollbackOutcome {
    /// The model now live, `None` for the deterministic scorer.
    pub active_model_id: Option<i64>,
    /// One line for the operator.
    pub detail: String,
}

/// The offline trainer, the model store, and the hot-swap.
///
/// One type owns all three deliberately. Installing a model into
/// [`ActiveRanker`] and purging the result cache have to happen together —
/// a cached page is a ranking, and one produced by the previous model is
/// stale the instant the swap lands. Splitting "persist" from "install" from
/// "invalidate" across three callers is how one of them eventually gets
/// forgotten.
///
/// Cheap to clone: every field is a handle, and every clone shares the same
/// live ranker and the same swap lock.
#[derive(Debug, Clone)]
pub struct Trainer {
    db: Database,
    params: TrainingParams,
    active: ActiveRanker,
    cache: ResultCache,
    /// Held for the whole of a train or a rollback.
    ///
    /// Not for the database's benefit — SQLite's single writer already
    /// serializes the `active`-flag transaction, so the *stored* state cannot
    /// tear. It is the in-process half that can: two runs that both promote a
    /// model do their `store::insert` under the writer lock in one order and
    /// their `ActiveRanker::install` in whatever order the runtime happens to
    /// schedule, so the daemon can end up serving model 1 while the database
    /// says model 2 is live — a discrepancy nothing reports and a restart
    /// silently "fixes" by changing the user's ranking.
    ///
    /// Serializing also makes the guardrail's comparison mean what it says:
    /// two concurrent runs would each measure against the model that was live
    /// when they *started*, so the second one to finish would promote a
    /// candidate judged against a baseline that no longer exists.
    ///
    /// `tokio::sync::Mutex` rather than `std`'s: this is held across `.await`
    /// points by construction (the read, the fit, the write).
    swap: std::sync::Arc<tokio::sync::Mutex<()>>,
}

impl Trainer {
    /// Build a trainer over `db` that swaps `active` and invalidates `cache`.
    #[must_use]
    pub fn new(
        db: Database,
        params: TrainingParams,
        active: ActiveRanker,
        cache: ResultCache,
    ) -> Self {
        Self {
            db,
            params,
            active,
            cache,
            swap: std::sync::Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// The live-ranker handle, so a caller can score with whatever is current
    /// without going through this type.
    #[must_use]
    pub fn active(&self) -> &ActiveRanker {
        &self.active
    }

    /// Install whichever model `ranker_model.active` names, at startup.
    ///
    /// Returns the id now live, or `None` when there is no accepted model —
    /// prd.md's cold user, running the deterministic scorer.
    ///
    /// A model that cannot be decoded is *not* an error here and does not
    /// clear the flag either: the daemon keeps running the deterministic
    /// scorer, says so, and leaves the row alone so that starting the correct
    /// build again picks it back up. Demoting it would make one accidental
    /// run of an older binary permanently discard a good model.
    ///
    /// # Errors
    ///
    /// A storage failure. A decode failure is reported through the returned
    /// `Option` (and a `warn`), not as an error.
    #[tracing::instrument(skip(self), err)]
    pub async fn restore(&self) -> Result<Option<i64>, Error> {
        // The third writer of the (stored `active` row, in-process handle)
        // pair, and it takes the same lock the other two do. Only startup
        // calls it today — but this is `pub`, and an invariant enforced on
        // two of three writers is not enforced.
        let _swap = self.swap.lock().await;

        let Some((id, blob, kind)) = self.db.read(store::active).await? else {
            self.active.reset();
            tracing::debug!("no active ranker model; using the deterministic scorer");
            return Ok(None);
        };
        match load_weights(&blob, &kind, self.active.fallback()) {
            Ok(weights) => {
                self.active.install(id, weights);
                tracing::info!(model_id = id, "restored the learned ranker model");
                Ok(Some(id))
            }
            Err(error) => {
                self.active.reset();
                tracing::warn!(
                    model_id = id,
                    %error,
                    "the stored ranker model cannot be run by this build; \
                     falling back to the deterministic scorer"
                );
                Ok(None)
            }
        }
    }

    /// The model history, newest first, and what is live.
    ///
    /// # Errors
    ///
    /// A storage failure.
    pub async fn models(&self, limit: usize) -> Result<ModelHistory, Error> {
        let limit = i64::try_from(limit.clamp(1, 500)).unwrap_or(50);
        let models = self.db.read(move |conn| store::list(conn, limit)).await?;
        Ok(ModelHistory {
            models,
            active_model_id: self.active.active_model_id(),
        })
    }

    /// Train on the accumulated feedback, evaluate on the held-out slice, and
    /// hot-swap **only** on a measured NDCG@10 win.
    ///
    /// With `dry_run`, everything runs and nothing is written or swapped —
    /// the answer to "what would tonight do".
    ///
    /// # Errors
    ///
    /// [`TrainError`]: too little data, a degenerate held-out slice, a
    /// diverged fit, cancellation, or a storage failure. A candidate the
    /// guardrail *refuses* is not an error — it is a successful run with
    /// `accepted: false`, and the refused candidate is still recorded so the
    /// refusal is auditable.
    #[tracing::instrument(skip(self, cancel))]
    pub async fn train(
        &self,
        dry_run: bool,
        cancel: &CancellationToken,
    ) -> Result<TrainingReport, TrainError> {
        // Held for the whole run, including the read: the baseline this
        // candidate is judged against is the model that is live *now*, and
        // that has to still be true when the verdict is applied. See
        // `Trainer::swap`.
        let _swap = self.swap.lock().await;

        let limit = i64::from(self.params.max_training_queries);
        let raw = self.db.read(move |conn| data::load(conn, limit)).await?;

        let params = self.params;
        let incumbent = self.active.current().weights().clone();
        let token = cancel.clone();
        // The calling span is carried onto the blocking thread, the same way
        // `Database::read`/`write` carry theirs: every event the CPU half
        // emits — the pair-ceiling warning, the per-query label cap, a
        // skipped undecodable page — belongs to *this* training run, and
        // rooting them in a fresh span makes them uncorrelatable with it.
        let span = tracing::Span::current();
        let judged = tokio::task::spawn_blocking(move || {
            let _entered = span.enter();
            judge(raw, &params, &incumbent, &token)
        })
        .await
        .map_err(|error| TrainError::Task(error.to_string()))??;

        let improvement = judged.candidate_ndcg - judged.baseline_ndcg;
        // Two conditions, not one. The margin is what stops nightly churn on
        // noise; the strict positivity is what stops a `min_ndcg_gain` of 0.0
        // — a value an operator can legitimately set — from promoting a model
        // that merely ties, which would rewrite the live model every night
        // for no measured benefit and bury the real rollback targets.
        let accepted = improvement >= self.params.min_ndcg_gain && improvement > 0.0;

        let verdict = if dry_run {
            format!(
                "dry run: candidate NDCG@10 {:.4} vs live {:.4} ({:+.4}); would {}",
                judged.candidate_ndcg,
                judged.baseline_ndcg,
                improvement,
                if accepted { "swap" } else { "not swap" }
            )
        } else if accepted {
            format!(
                "swapped: candidate NDCG@10 {:.4} beats live {:.4} by {:+.4} on {} held-out \
                 queries",
                judged.candidate_ndcg, judged.baseline_ndcg, improvement, judged.holdout_engaged
            )
        } else {
            format!(
                "kept the live model: candidate NDCG@10 {:.4} vs live {:.4} ({:+.4}) does not \
                 clear the {:.4} guardrail",
                judged.candidate_ndcg, judged.baseline_ndcg, improvement, self.params.min_ndcg_gain
            )
        };

        let mut report = TrainingReport {
            train_queries: judged.train_queries,
            train_pairs: judged.train_pairs,
            holdout_queries: judged.holdout_queries,
            holdout_engaged: judged.holdout_engaged,
            skipped_queries: judged.skipped_queries,
            baseline_ndcg_at_10: judged.baseline_ndcg,
            candidate_ndcg_at_10: judged.candidate_ndcg,
            min_gain: self.params.min_ndcg_gain,
            accepted: accepted && !dry_run,
            dry_run,
            initial_loss: judged.initial_loss,
            final_loss: judged.final_loss,
            model_id: None,
            active_model_id: self.active.active_model_id(),
            verdict,
        };

        if dry_run {
            tracing::info!(
                verdict = %report.verdict,
                "ranker training dry run finished"
            );
            return Ok(report);
        }

        let blob = encode(&judged.candidate)?;
        let candidate = NewModel {
            kind: MODEL_KIND_LINEAR,
            weights: blob,
            status: if accepted {
                ModelStatus::Accepted
            } else {
                ModelStatus::Rejected
            },
            train_queries: clamp_u32(judged.train_queries),
            train_pairs: clamp_u32(judged.train_pairs),
            eval_queries: clamp_u32(judged.holdout_queries),
            eval_engaged: clamp_u32(judged.holdout_engaged),
            baseline_ndcg: judged.baseline_ndcg,
            candidate_ndcg: judged.candidate_ndcg,
            note: report.verdict.clone(),
        };
        let model_id = self
            .db
            .write(move |conn| store::insert(conn, &candidate, accepted))
            .await?;
        report.model_id = Some(model_id);

        if accepted {
            // Install, *then* invalidate. See `ResultCache::purge`'s docs for
            // why that order is the one that cannot leave a stale page behind.
            self.active.install(model_id, judged.candidate);
            report.active_model_id = Some(model_id);
            self.invalidate_cached_rankings().await;
        }

        let keep = i64::from(self.params.max_models);
        if let Err(error) = self.db.write(move |conn| store::prune(conn, keep)).await {
            // Retention failing is a disk-growth problem, not a correctness
            // one, and the model this run produced is already live. Warning
            // and continuing is right; failing the run here would report a
            // swap that did happen as an error.
            tracing::warn!(%error, "pruning the ranker model history failed");
        }

        tracing::info!(
            model_id,
            accepted,
            baseline_ndcg_at_10 = judged.baseline_ndcg,
            candidate_ndcg_at_10 = judged.candidate_ndcg,
            train_pairs = judged.train_pairs,
            holdout_engaged = judged.holdout_engaged,
            verdict = %report.verdict,
            "ranker training finished"
        );
        Ok(report)
    }

    /// Roll the live ranker back.
    ///
    /// `target` of `None` steps to the newest accepted model strictly older
    /// than whatever is active, and to the deterministic scorer when there is
    /// none — so repeated rollbacks walk backwards through history rather
    /// than oscillating between the two newest models. A `Some(id)` goes
    /// straight there, provided the guardrail accepted it.
    ///
    /// # Errors
    ///
    /// [`TrainError::UnknownModel`] for an id with no row,
    /// [`TrainError::RefusedModel`] for one the guardrail rejected,
    /// [`TrainError::Model`] if the target cannot be decoded by this build,
    /// or a storage failure.
    #[tracing::instrument(skip(self), err)]
    pub async fn rollback(&self, target: Option<i64>) -> Result<RollbackOutcome, TrainError> {
        // The *stored* active row, not the in-process one: when a model
        // failed to decode at startup the deterministic scorer is running
        // while the flag still sits on that row, and "step back one" has to
        // mean "back from the row that is marked active" or it would offer
        // the broken model as the rollback target.
        // Same lock a train takes, for the same reason: "step back from
        // whatever is live" is a read-modify-write over the active flag and
        // the in-process handle together.
        let _swap = self.swap.lock().await;

        let stored_active = self.db.read(store::active).await?.map(|(id, _, _)| id);

        let chosen = match target {
            Some(id) => Some(id),
            None => match stored_active {
                Some(current) => {
                    self.db
                        .read(move |conn| store::rollback_target(conn, current))
                        .await?
                }
                // Nothing is live, so there is nothing to step back *from*.
                // Reconciles the in-process handle with the database and
                // stops. Notably it does *not* reach for "the newest accepted
                // model": that would re-activate the model the operator had
                // just rolled off, and repeated rollbacks would oscillate
                // instead of terminating.
                //
                // The cache is purged only if the handle was actually holding
                // a model — the usual case is a no-op on a mailbox that is
                // already cold, and a rollback that changed nothing must not
                // cost every cached page. But when the two disagreed (a
                // stored model this build could not decode, cleared out from
                // under a daemon that had one live), `reset` *did* change the
                // ranking, and cached pages from the model it just dropped
                // would outlive it.
                None => {
                    let was_live = self.active.active_model_id().is_some();
                    self.active.reset();
                    if was_live {
                        self.invalidate_cached_rankings().await;
                    }
                    let detail = "already running the deterministic scorer; \
                                  there is no model to roll back from"
                        .to_owned();
                    tracing::info!(detail, "ranker rollback finished");
                    return Ok(RollbackOutcome {
                        active_model_id: None,
                        detail,
                    });
                }
            },
        };

        let Some(id) = chosen else {
            self.db.write(|conn| store::deactivate(conn)).await?;
            self.active.reset();
            self.invalidate_cached_rankings().await;
            let detail =
                "rolled back to the deterministic scorer: no earlier accepted model".to_owned();
            tracing::info!(detail, "ranker rollback finished");
            return Ok(RollbackOutcome {
                active_model_id: None,
                detail,
            });
        };

        let Some((blob, kind, status)) = self.db.read(move |conn| store::by_id(conn, id)).await?
        else {
            return Err(TrainError::UnknownModel(id));
        };
        if ModelStatus::parse(&status) != Some(ModelStatus::Accepted) {
            return Err(TrainError::RefusedModel(id));
        }
        // Decoded *before* the flag moves. Activating a row this build cannot
        // run would leave the daemon on the deterministic scorer with the
        // database claiming otherwise — the exact discrepancy a rollback is
        // supposed to resolve.
        let weights = load_weights(&blob, &kind, self.active.fallback())?;

        match self.db.write(move |conn| store::activate(conn, id)).await? {
            store::Activation::Activated => {}
            store::Activation::Unknown => return Err(TrainError::UnknownModel(id)),
            store::Activation::Rejected => return Err(TrainError::RefusedModel(id)),
        }
        self.active.install(id, weights);
        self.invalidate_cached_rankings().await;

        let detail = format!("ranker model {id} is live");
        tracing::info!(model_id = id, "ranker rollback finished");
        Ok(RollbackOutcome {
            active_model_id: Some(id),
            detail,
        })
    }

    /// Drop cached result pages produced by the model that is no longer live.
    ///
    /// Best effort by design: the swap has already happened and is correct,
    /// and a failure here costs at most `search.cache.result_ttl_secs` of
    /// pages ordered by the previous model. Failing the whole operation would
    /// report a swap that did happen as one that did not.
    async fn invalidate_cached_rankings(&self) {
        if let Err(error) = self.cache.purge().await {
            tracing::warn!(
                %error,
                "could not purge the result cache after a ranker swap; cached pages may \
                 keep the previous model's ordering until they expire"
            );
        }
    }
}

/// Decode a stored blob, rejecting a `kind` this build cannot run.
fn load_weights(blob: &[u8], kind: &str, base: &Weights) -> Result<Weights, ModelError> {
    if kind != MODEL_KIND_LINEAR {
        return Err(ModelError::UnknownKind(kind.to_owned()));
    }
    model::decode(blob, base)
}

fn clamp_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

/// Hard ceiling on the preference pairs one run fits against.
///
/// A pair carries two flattened 34-feature arrays (~0.5 kB), so this bounds
/// the optimizer's working set at roughly 50 MB regardless of what the log
/// contains. `search.training.max_training_queries` bounds the *read*; this
/// bounds what pathological pages inside that read can expand into
/// (`labels::MAX_PAIRS_PER_QUERY` bounds any single one of them). A realistic
/// nightly run is a few thousand pairs and never approaches it — which is
/// exactly why exceeding it is a `warn` rather than a silent truncation.
///
/// Not a config knob: it is a memory ceiling for a background job, not a
/// modelling decision, and every value an operator could sensibly choose is
/// already expressible through `max_training_queries`.
const MAX_PAIRS: usize = 100_000;

/// Everything the blocking half of a run produces.
struct Judged {
    candidate: Weights,
    train_queries: usize,
    train_pairs: usize,
    holdout_queries: usize,
    holdout_engaged: usize,
    skipped_queries: usize,
    baseline_ndcg: f64,
    candidate_ndcg: f64,
    initial_loss: f64,
    final_loss: f64,
}

/// Decode, split, label, fit, and score both models on the held-out slice.
///
/// One function so the whole CPU-bound half of a run is one `spawn_blocking`
/// and the intermediate `Vec<LoggedQuery>` — the largest allocation a run
/// makes — never crosses a task boundary.
fn judge(
    raw: data::RawFeedback,
    params: &TrainingParams,
    incumbent: &Weights,
    cancel: &CancellationToken,
) -> Result<Judged, TrainError> {
    let decoded = data::decode(raw, cancel)?;
    let usable = decoded.queries.len();
    if usable < params.min_queries as usize {
        return Err(TrainError::InsufficientQueries {
            found: usable,
            needed: params.min_queries,
        });
    }

    let (holdout, training): (Vec<LoggedQuery>, Vec<LoggedQuery>) = decoded
        .queries
        .into_iter()
        .partition(|query| is_holdout(&query.group_key, params.holdout_percent));

    let mut pairs: Vec<PreferencePair> = Vec::new();
    for query in &training {
        if cancel.is_cancelled() {
            return Err(TrainError::Cancelled);
        }
        if pairs.len() >= MAX_PAIRS {
            tracing::warn!(
                cap = MAX_PAIRS,
                trained_on = pairs.len(),
                queries = training.len(),
                "hit the training pair ceiling; the rest of the log is not in this model"
            );
            break;
        }
        pairs.extend(pairs_for(
            query,
            params.position_bias_eta,
            params.max_propensity_weight,
        ));
    }
    if pairs.len() < params.min_pairs as usize {
        return Err(TrainError::InsufficientPairs {
            found: pairs.len(),
            needed: params.min_pairs,
        });
    }

    let slice = HoldoutSlice::build(&holdout);
    if slice.engaged_groups < params.min_eval_queries as usize {
        return Err(TrainError::DegenerateHoldout {
            engaged: slice.engaged_groups,
            total: holdout.len(),
            needed: params.min_eval_queries,
        });
    }

    let fitted = fit::fit(&pairs, incumbent, &params.fit, cancel)?;
    if cancel.is_cancelled() {
        return Err(TrainError::Cancelled);
    }

    Ok(Judged {
        baseline_ndcg: slice.ndcg_at_10(incumbent),
        candidate_ndcg: slice.ndcg_at_10(&fitted.weights),
        candidate: fitted.weights,
        train_queries: training.len(),
        train_pairs: pairs.len(),
        holdout_queries: holdout.len(),
        holdout_engaged: slice.engaged_groups,
        skipped_queries: decoded.skipped,
        initial_loss: fitted.initial_loss,
        final_loss: fitted.final_loss,
    })
}

/// The held-out slice, in the two shapes scoring it needs: what
/// [`crate::eval::replay`] wants (what was shown, what was engaged with) and
/// what a [`L1Ranker`] wants (the feature vectors and the intent to score
/// under).
///
/// # Only the engaged impressions are in it
///
/// A held-out search nobody engaged with has no judgments, so
/// [`crate::eval::metrics::ndcg_at`] returns `0.0` for it — for *every*
/// model. Leaving those in would not add information; it would multiply both
/// NDCG numbers, and therefore their difference, by
/// `engaged / total`. At a realistic 20% engagement rate a configured
/// `min_ndcg_gain` of `0.005` would silently demand a real gain of `0.025`
/// on the queries that actually carry evidence, and at 5% it would demand
/// `0.10` — personalization would simply never turn on, with nothing in the
/// report saying why. Both stored NDCG numbers are therefore means over the
/// queries that have a judgment, which is what "NDCG@10 on the held-out
/// slice" is normally taken to mean and what `search.training.min_ndcg_gain`
/// is documented against.
///
/// `TrainingReport` still carries the *total* held-out size alongside the
/// engaged count, so a slice that is mostly silent is visible rather than
/// hidden by the filter.
struct HoldoutSlice {
    impressions: Vec<Impression>,
    candidates: Vec<Vec<CandidateFeatures>>,
    intents: Vec<Intent>,
    /// Distinct query *groups* with engagement — not logged searches.
    ///
    /// The split is taken at the group grain precisely because a
    /// keystroke-driven search box logs the same search many times; counting
    /// those repeats as independent evidence would let ten impressions of one
    /// query text satisfy a `min_eval_queries` of ten while the comparison is
    /// really n = 1. This is the number the guardrail's degenerate-slice
    /// bound is checked against and the one the report shows.
    engaged_groups: usize,
}

impl HoldoutSlice {
    fn build(queries: &[LoggedQuery]) -> Self {
        let mut impressions = Vec::new();
        let mut candidates = Vec::new();
        let mut intents = Vec::new();
        let mut groups: std::collections::HashSet<Vec<u8>> = std::collections::HashSet::new();
        for query in queries {
            let impression = Impression {
                query: query.raw_query.clone(),
                shown: query.shown.iter().map(|s| s.message_id).collect(),
                engagements: query
                    .actions
                    .iter()
                    .map(|action| Engagement {
                        message_id: action.message_id,
                        action: engagement_of(action.kind),
                    })
                    .collect(),
            };
            if !impression.is_successful() {
                continue;
            }
            groups.insert(query.group_key.clone());
            impressions.push(impression);
            candidates.push(
                query
                    .shown
                    .iter()
                    .map(|shown| CandidateFeatures {
                        message_id: shown.message_id,
                        features: shown.features.clone(),
                    })
                    .collect(),
            );
            intents.push(query.intent);
        }
        Self {
            impressions,
            candidates,
            intents,
            engaged_groups: groups.len(),
        }
    }

    /// `weights`' NDCG@10 over this slice, through [`crate::eval`]'s own
    /// shadow scorer.
    ///
    /// The orderings are computed up front and handed to
    /// [`shadow`] through an iterator rather than recomputed inside its
    /// closure. `shadow` calls the closure exactly once per impression, in
    /// slice order (its own docs say so, and
    /// `tests::shadow_calls_reorder_once_per_impression_in_slice_order` pins
    /// it),
    /// so `next()` lines each order up with the impression it was computed
    /// for. `unwrap_or_default` rather than an index or an unwrap: if that
    /// contract ever changed, an empty ordering scores this model *worse*,
    /// which fails the guardrail closed rather than promoting a model on
    /// mismatched data.
    fn ndcg_at_10(&self, weights: &Weights) -> f64 {
        let ranker = L1Ranker::new(weights.clone());
        let mut orders = self
            .candidates
            .iter()
            .zip(&self.intents)
            .map(|(candidates, intent)| {
                ranker
                    .rank(candidates, *intent, candidates.len())
                    .into_iter()
                    .map(|ranked| ranked.message_id)
                    .collect::<Vec<i64>>()
            });
        shadow(&self.impressions, |_| orders.next().unwrap_or_default())
            .ranking
            .ndcg_at_10
    }
}

/// The feedback vocabulary in [`crate::eval::replay`]'s terms.
///
/// A total mapping between two closed enums that say the same thing, rather
/// than one type used in both places: `feedback::ActionKind` is what is on
/// disk and `eval::replay::EngagementAction` is what the evaluator was
/// written against, and neither task should have had to take a dependency on
/// the other's vocabulary to exist.
const fn engagement_of(kind: ActionKind) -> EngagementAction {
    match kind {
        ActionKind::Open => EngagementAction::Open,
        ActionKind::Reply => EngagementAction::Reply,
        ActionKind::Archive => EngagementAction::Archive,
        ActionKind::Dwell => EngagementAction::Dwell,
        ActionKind::ScrollPast => EngagementAction::ScrollPast,
    }
}
