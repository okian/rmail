//! Indexing: turning stored mail into something searchable.
//!
//! Indexing a message is a sequence of independent stages — extract text,
//! feed the lexical index, pull out entities, chunk and embed — and they fail
//! independently. [`queue`] is what makes that survivable: a durable work queue
//! where each stage is its own job, so an embeddings provider being down does
//! not stop lexical search from being built, and a laptop closed halfway
//! through a first index resumes rather than restarts.
//!
//! [`pipeline`] is what actually drives the stages from that queue, and
//! [`admin`] is the operator surface over the whole thing — coverage, drift,
//! garbage collection, and the reindex/rebuild verbs `mail index` exposes.

pub mod admin;
pub mod chunk;
pub mod entities;
pub mod extract;
pub mod fts;
pub mod pipeline;
pub mod queue;
pub mod semantic;

pub use admin::{
    EntityRow, GcReport, IndexAdmin, IndexDrift, IndexStatus, KindStatus, RebuildReport, Selection,
};
pub use entities::{collect_orphans, extract_entities, EntityKind, EntityReport, Mention};
pub use extract::{extract_message, ExtractReport, ExtractedPart, Part};
pub use fts::{FtsIndex, Hit};
pub use pipeline::{
    DrainReport, IndexLoop, IndexPauseFlag, IndexPipeline, StageSwitches, TickReport,
};
pub use queue::{
    DeadLetter, Failure, IndexKind, IndexQueue, JobState, Lease, NewJob, QueueOptions, QueueStats,
    PRIORITY_BACKFILL, PRIORITY_NORMAL, PRIORITY_RECENT,
};
