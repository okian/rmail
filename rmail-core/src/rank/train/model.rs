//! The learned model's on-disk form, and the live handle every search reads
//! its ranker through.
//!
//! # The hot-swap seam
//!
//! [`ActiveRanker`] is the one place in the process that answers "which Stage
//! 4 model is live right now". A search takes an [`Arc`] out of it per request
//! and scores against that snapshot, so a swap landing mid-page cannot make
//! one page's hits come from two different models — the page finishes under
//! whichever model it started with, and the next one picks up the new table.
//!
//! It holds an `Arc<L1Ranker>` rather than an `Arc<dyn Ranker>` because
//! task 65's learned model *is* an [`L1Ranker`]: the trained artifact is a
//! [`Weights`] table over the same 34 features the cold-start formula uses
//! (see [`super`]'s module docs for why a linear model rather than the GBDT
//! prd.md also names). Keeping the concrete type is what lets `Explain` keep
//! showing a per-feature contribution breakdown for the *learned* model —
//! [`L1Ranker::contributions`] is an inherent method, not part of the
//! [`crate::rank::Ranker`] trait, and behind a trait object an explanation of
//! the live ranking would simply stop being available the day personalization
//! turned on. The trait-object seam is still there and still the thing a
//! future tree model would arrive through; it is just not what this task
//! needs.
//!
//! # A live learned model supersedes `[search.rank_weights]`
//!
//! Worth stating because it surprises people. [`encode`] writes all 34
//! feature names and [`decode`] overwrites every key of its `base`, so once a
//! model is accepted, editing `[search.rank_weights]` and restarting changes
//! nothing about ranking: `base` only supplies weights for features a
//! *newer* build has added and the stored model has never heard of. That is
//! the correct precedence — a hand-tuned prior is what personalization starts
//! from, not something that keeps overriding it afterwards — but it means the
//! way to get the configured table back is `mail search rollback`, not a
//! config edit. `ActiveRanker::fallback` is what a rollback lands on, so the
//! override is never lost, only inactive.
//!
//! # The fallback is a value, not a `None`
//!
//! prd.md: "Cold users fall back to the deterministic scorer." [`ActiveRanker`]
//! is therefore always holding a usable ranker — the config-derived cold-start
//! table until a model is accepted, and back to it if a rollback runs out of
//! accepted models. There is no state in which a search has to ask whether a
//! ranker exists, which is what keeps the fallback path the *same* code path
//! rather than a branch nobody exercises.
//!
//! # Why the stored form is name-keyed JSON
//!
//! Same argument [`crate::feedback::EncodedFeatures`] makes for impressions,
//! and it matters more here: a positional array of 34 floats would silently
//! reinterpret every stored model the day a feature is inserted in the middle
//! of [`crate::features::FeatureName::ALL`], and the symptom would be a
//! ranking that is subtly wrong with nothing logged. A name-keyed map makes
//! an added feature a missing key (which keeps its cold-start weight) and an
//! unknown name a hard [`ModelError::UnknownFeature`] — a model this build
//! cannot faithfully run is refused, not approximated.

use std::collections::BTreeMap;
use std::sync::{Arc, PoisonError, RwLock};

use serde::{Deserialize, Serialize};

use crate::features::FeatureName;
use crate::rank::l1::{L1Ranker, Weights};

/// The version stamped into every [`encode`] envelope.
///
/// Bump when the *encoding* changes in a way a decoder must branch on — not
/// when a feature is added or removed, which the name keying already absorbs.
pub const MODEL_FORMAT_VERSION: u32 = 1;

/// `ranker_model.kind` for the only model family this build trains or runs.
pub const MODEL_KIND_LINEAR: &str = "linear";

/// What can go wrong turning a stored blob back into a runnable model.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ModelError {
    /// The blob is not the documented envelope at all.
    #[error("malformed ranker model: {0}")]
    Malformed(String),

    /// The envelope carries a version this build does not read.
    #[error("ranker model is format version {found}, this build reads {expected}")]
    Version {
        /// The version the row carried.
        found: u32,
        /// The version this build understands.
        expected: u32,
    },

    /// A weight is keyed by a name that is not one of
    /// [`FeatureName::ALL`]'s stable strings — a model written by a build
    /// that knows a feature this one does not.
    #[error("ranker model weights an unknown feature {0:?}; refusing to run a model this build cannot reproduce")]
    UnknownFeature(String),

    /// A weight is `NaN`/`±inf` — a non-finite weight collapses every
    /// candidate's score to `NaN` and silently reduces the ranking to
    /// `message_id` order, precisely the failure [`Weights::from_config`]
    /// rejects for configured overrides.
    ///
    /// Reachable from [`encode`] (a `Weights` gets there from Rust, so a
    /// diverged optimizer could hand it one) and, today, not from [`decode`]:
    /// JSON has no `NaN`/`inf` literal and `serde_json`'s parser rejects a
    /// number outside `f64`'s range outright, so a corrupt row surfaces as
    /// [`ModelError::Malformed`] instead. The check on the decode side is
    /// kept anyway — it costs one comparison per feature and it is the only
    /// thing that would still hold if the JSON backend ever changed — and
    /// `rank::train::tests::a_weight_outside_the_float_range_cannot_be_decoded`
    /// records which of the two errors that path actually produces.
    #[error("ranker model has a non-finite weight {value} for feature {name:?}")]
    NonFiniteWeight {
        /// Which feature.
        name: String,
        /// The rejected value.
        value: f64,
    },

    /// `ranker_model.kind` names a model family this build cannot run.
    #[error("ranker model kind {0:?} is not one this build can run")]
    UnknownKind(String),
}

impl From<ModelError> for crate::error::Error {
    /// Every variant is "this stored model is not one I can run", which is a
    /// state the system is in rather than a caller mistake — hence
    /// `FailedPrecondition` rather than `InvalidArgument`. A client that asked
    /// to activate such a model cannot fix its bytes; it can roll back to
    /// another one, which is exactly what `FAILED_PRECONDITION` tells it.
    fn from(err: ModelError) -> Self {
        crate::error::Error::FailedPrecondition(err.to_string())
    }
}

/// The wire format of `ranker_model.weights`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EncodedModel {
    /// [`MODEL_FORMAT_VERSION`] at the time of writing.
    pub version: u32,
    /// Every feature's weight, keyed by [`FeatureName::as_str`]. A `BTreeMap`
    /// so the serialization is byte-stable for a given table — two identical
    /// models hash the same, which makes "did tonight's run actually change
    /// anything" answerable by comparing blobs.
    pub weights: BTreeMap<String, f64>,
}

/// Serialize a weight table for `ranker_model.weights`.
///
/// Every one of [`FeatureName::ALL`] is written, including the ones whose
/// weight is `0.0`: a trained model has an opinion about every feature, and
/// omitting the zeros would make a decoded model silently inherit cold-start
/// values for exactly the features training decided should not contribute.
///
/// # Errors
///
/// [`ModelError::NonFiniteWeight`] if any weight is `NaN`/`±inf` — refused on
/// the way *in* as well as on the way out, so a diverged optimizer cannot
/// persist a model that would later fail to load. [`ModelError::Malformed`]
/// if serialization itself fails, which for this shape can only mean the
/// non-finite case the check above already covers.
pub fn encode(weights: &Weights) -> Result<Vec<u8>, ModelError> {
    let mut table = BTreeMap::new();
    for name in FeatureName::ALL {
        let value = weights.get(name);
        if !value.is_finite() {
            return Err(ModelError::NonFiniteWeight {
                name: name.as_str().to_owned(),
                value,
            });
        }
        table.insert(name.as_str().to_owned(), value);
    }
    let envelope = EncodedModel {
        version: MODEL_FORMAT_VERSION,
        weights: table,
    };
    serde_json::to_vec(&envelope).map_err(|error| ModelError::Malformed(error.to_string()))
}

/// Decode a `ranker_model.weights` blob into a runnable weight table.
///
/// `base` is what an absent key falls back to — the deterministic
/// configuration-derived table, so a model written before a feature existed
/// keeps that feature's cold-start weight rather than silently zeroing it.
///
/// # Errors
///
/// [`ModelError::Malformed`] for a blob that is not the envelope,
/// [`ModelError::Version`] for one from a build with a different encoding,
/// [`ModelError::UnknownFeature`] for a weight this build has no feature for,
/// and [`ModelError::NonFiniteWeight`] for a corrupt value. All four refuse
/// rather than best-effort: a model that cannot be reproduced faithfully is
/// worse than no model, because the deterministic fallback is known-good and
/// an approximation of a learned one is not.
pub fn decode(blob: &[u8], base: &Weights) -> Result<Weights, ModelError> {
    let envelope: EncodedModel =
        serde_json::from_slice(blob).map_err(|error| ModelError::Malformed(error.to_string()))?;
    if envelope.version != MODEL_FORMAT_VERSION {
        return Err(ModelError::Version {
            found: envelope.version,
            expected: MODEL_FORMAT_VERSION,
        });
    }
    let mut weights = base.clone();
    for (key, value) in envelope.weights {
        let name = FeatureName::ALL
            .into_iter()
            .find(|candidate| candidate.as_str() == key)
            .ok_or_else(|| ModelError::UnknownFeature(key.clone()))?;
        if !value.is_finite() {
            return Err(ModelError::NonFiniteWeight { name: key, value });
        }
        weights.set(name, value);
    }
    Ok(weights)
}

/// What is live, as one value under one lock.
#[derive(Debug, Clone)]
struct Live {
    /// `ranker_model.id` of the model in `ranker`, or `None` when the
    /// deterministic scorer is what is running.
    model_id: Option<i64>,
    ranker: Arc<L1Ranker>,
}

/// The live Stage 4 ranker, swappable in place.
///
/// Cheap to clone — every clone shares one slot, which is the point: the
/// handle `SearchApi` reads per request and the handle the trainer installs
/// into have to be the same slot or the "hot" in hot-swap means "after a
/// restart".
#[derive(Debug, Clone)]
pub struct ActiveRanker {
    /// prd.md's "always-available fallback": the configuration-derived
    /// cold-start table. Kept separately from `live` so a rollback past the
    /// oldest accepted model has somewhere to land, and so a decoded model
    /// can inherit a feature's cold-start weight (see [`decode`]).
    fallback: Weights,
    live: Arc<RwLock<Live>>,
}

impl ActiveRanker {
    /// A handle running the deterministic scorer over `weights` — what every
    /// cold mailbox runs, and what this is before
    /// [`crate::rank::train::Trainer::restore`] has had a chance to load
    /// anything.
    #[must_use]
    pub fn deterministic(weights: Weights) -> Self {
        let ranker = Arc::new(L1Ranker::new(weights.clone()));
        Self {
            fallback: weights,
            live: Arc::new(RwLock::new(Live {
                model_id: None,
                ranker,
            })),
        }
    }

    /// The deterministic table this falls back to. Also the base a stored
    /// model's absent keys inherit from.
    #[must_use]
    pub fn fallback(&self) -> &Weights {
        &self.fallback
    }

    /// The ranker to score this request with.
    ///
    /// Returns an owned [`Arc`] rather than a guard so the read lock is held
    /// for a pointer clone and nothing else — no caller can hold it across an
    /// `.await`, which is what keeps a swap from ever waiting on a search.
    /// A poisoned lock reads through [`PoisonError::into_inner`] rather than
    /// failing the search: the data behind it is a single `Arc` swap that
    /// cannot be observed half-written, and refusing to rank because an
    /// unrelated thread panicked would turn a cosmetic fault into an outage.
    #[must_use]
    pub fn current(&self) -> Arc<L1Ranker> {
        Arc::clone(
            &self
                .live
                .read()
                .unwrap_or_else(PoisonError::into_inner)
                .ranker,
        )
    }

    /// `ranker_model.id` of the live model, or `None` when the deterministic
    /// scorer is running.
    #[must_use]
    pub fn active_model_id(&self) -> Option<i64> {
        self.live
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .model_id
    }

    /// Make `weights` (stored as `model_id`) the live ranker.
    ///
    /// Deliberately not `pub`: installing a model is something
    /// [`crate::rank::train::Trainer`] does *after* the guardrail has passed
    /// and as part of the same operation that purges the result cache. A
    /// public setter here would be a way to put a model live without either.
    pub(crate) fn install(&self, model_id: i64, weights: Weights) {
        let mut live = self.live.write().unwrap_or_else(PoisonError::into_inner);
        live.model_id = Some(model_id);
        live.ranker = Arc::new(L1Ranker::new(weights));
    }

    /// Fall back to the deterministic scorer — a rollback with no earlier
    /// accepted model to land on.
    pub(crate) fn reset(&self) {
        let mut live = self.live.write().unwrap_or_else(PoisonError::into_inner);
        live.model_id = None;
        live.ranker = Arc::new(L1Ranker::new(self.fallback.clone()));
    }
}
