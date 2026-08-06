//! Stage 3 — Feature Extraction (prd.md, "Stage 3 — Feature Extraction"):
//! turning one of task 29's [`FusedCandidate`](crate::fuse::FusedCandidate)s
//! into a named, deterministic, serializable [`FeatureVector`] that task
//! 31's L1 ranker scores, task 33's `Explain` reports on, task 64 logs
//! verbatim as an impression, and task 65 trains a learned ranker against.
//!
//! # Three pieces, one per file
//!
//! - [`name`] — [`FeatureName`]/[`FeatureGroup`]: every feature's stable
//!   identity, named once so it survives a struct-field reorder or a new
//!   feature landing between two existing ones.
//! - [`vector`] — [`FeatureVector`]: the typed, `serde`-serializable output
//!   shape, plus [`FeatureVector::as_pairs`] for a name-keyed numeric view.
//! - [`extract`] — [`FeatureExtractor`]: the batched-SQL-plus-pure-Rust
//!   computation that turns fused candidates into vectors.
//!
//! Each module's own docs cover its design decisions in depth; this module
//! only wires them together and re-exports the public surface a caller
//! (task 31's ranker, task 33's `Explain`, task 64's impression logger)
//! actually needs.
//!
//! # Why this exists as its own crate module rather than living in `fuse`
//! # or `rank`
//!
//! [`FusedCandidate`](crate::fuse::FusedCandidate) is Stage 2's *output*
//! shape — every source's rank/score, thread/near-dup collapse — and stays
//! that way regardless of what Stage 3 does with it. A ranker (task 31) does
//! not want to know how a feature was computed, only its name and value.
//! Keeping extraction in its own module, consuming `fuse`'s public type and
//! producing a type `rank`/`explain`/the offline trainer can consume without
//! any of them depending on each other's internals, is the same
//! transport/domain/wiring separation `CLAUDE.md`'s "Workspace shape"
//! section asks the crate boundaries themselves to keep — just one level
//! down, between pipeline stages within `rmail-core`.

pub mod extract;
pub mod name;
pub mod vector;

pub use extract::{CandidateFeatures, FeatureExtractor};
pub use name::{FeatureGroup, FeatureName};
pub use vector::{FeatureVector, MatchField};
