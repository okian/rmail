//! Indexing: turning stored mail into something searchable.
//!
//! Indexing a message is a sequence of independent stages — extract text,
//! feed the lexical index, pull out entities, chunk and embed — and they fail
//! independently. [`queue`] is what makes that survivable: a durable work queue
//! where each stage is its own job, so an embeddings provider being down does
//! not stop lexical search from being built, and a laptop closed halfway
//! through a first index resumes rather than restarts.
//!
//! The stages themselves land in the tasks after this one; this module ships
//! the queue they will be driven from.

pub mod extract;
pub mod fts;
pub mod queue;

pub use extract::{extract_message, ExtractReport, ExtractedPart, Part};
pub use fts::{FtsIndex, Hit};
pub use queue::{
    DeadLetter, Failure, IndexKind, IndexQueue, JobState, Lease, NewJob, QueueOptions, QueueStats,
    PRIORITY_BACKFILL, PRIORITY_NORMAL, PRIORITY_RECENT,
};
