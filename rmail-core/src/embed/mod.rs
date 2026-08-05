//! Embeddings: text in, a unit vector out.
//!
//! # One trait, three backends, one degradation story
//!
//! Semantic retrieval is the half of search that finds the message whose words
//! you did not remember. It is also the half with a heavyweight dependency —
//! a model — and the PRD is explicit that the absence of one degrades search
//! rather than breaking it. So the backends form a ladder:
//!
//! - [`local`] runs `bge-small-en-v1.5` through ONNX Runtime on this machine.
//!   Nothing leaves the host, which is what makes it the default.
//! - [`voyage`] calls a hosted API, for accounts where quality matters more
//!   than egress. The key comes from a command, never from the config file.
//! - [`hash`] is deterministic, dependency-free and always available. It is not
//!   semantic and does not pretend to be; it exists so that a daemon with no
//!   model still produces vectors of the right shape, so the retrieval pipeline
//!   below it has one code path instead of two.
//!
//! # Vectors are unit-normalized at the boundary
//!
//! [`Embedding`]'s only public constructor is [`Embedding::new`], which
//! normalizes. Cosine similarity is then a dot product, and no consumer outside
//! this module has to remember to normalize — a rule enforced at the boundary
//! cannot be forgotten in the ranker, the kNN index or the cache. The backends
//! in this module's own submodules could bypass it, so each of them goes
//! through `new` deliberately and the tests check the result is unit length.
//!
//! # Loading is lazy, warming is explicit
//!
//! Constructing a backend does no I/O. A model is loaded on first use, or by
//! [`Embedder::warm`] at daemon start, so that the first user query does not
//! pay for a several-hundred-megabyte load and so that constructing a daemon in
//! a test does not need a model at all.

use std::sync::Arc;

use crate::config::{IndexSemanticConfig, SemanticProvider};
use crate::error::Error;

pub mod hash;
#[cfg(feature = "onnx")]
pub mod local;
pub mod voyage;

/// Largest batch handed to a backend in one call.
///
/// Batching is the difference between one model invocation and a thousand, but
/// an unbounded batch is an unbounded allocation: a backfill over a
/// hundred-thousand-message mailbox would otherwise try to embed all of it at
/// once. Backends chunk anything larger.
pub const MAX_BATCH: usize = 64;

/// Longest input one embedding covers, in bytes.
///
/// Every model has a token limit and silently truncates past it. Truncating
/// here instead means the limit is visible, uniform across backends, and
/// applied to the *same* prefix every time — so re-embedding unchanged content
/// produces an unchanged vector, which is what the content-hash cache depends
/// on.
pub const MAX_INPUT_BYTES: usize = 8 * 1024;

/// A unit-normalized embedding.
///
/// The invariant is the point: cosine similarity between two of these is their
/// dot product, so nothing downstream carries a normalization step that could
/// be skipped in one place and not another.
#[derive(Clone, PartialEq)]
pub struct Embedding(Vec<f32>);

impl std::fmt::Debug for Embedding {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // An embedding is partially invertible back to the text it came from,
        // so printing one puts message content wherever the log goes. The shape
        // is what a reader of a trace actually wants anyway.
        write!(f, "Embedding({} dims)", self.0.len())
    }
}

impl Embedding {
    /// Normalize a raw vector.
    ///
    /// A zero vector — which an empty or entirely out-of-vocabulary input can
    /// produce — has no direction to normalize to and is kept as zeros. Its
    /// similarity to everything is then zero, which is the honest answer;
    /// dividing by a norm of zero would fill it with `NaN` and poison every
    /// comparison it ever took part in, including the ones that decide ranking.
    ///
    /// # The norm is accumulated in `f64`
    ///
    /// Not fastidiousness: in `f32` the sum of squares overflows above about
    /// `1.8e19` per component and underflows below about `1e-22`, and both
    /// failures are silent. A vector of `1e-25`s came back *unnormalized* —
    /// the invariant this whole type exists to hold, quietly broken — and
    /// `[1e30, 1e-30]`, which has a perfectly good direction, came back as all
    /// zeros. Neither is hypothetical, because [`Embedding::from_bytes`] is a
    /// deserialization path fed by the cache: a corrupt or exotic row is
    /// exactly the input that reaches it.
    #[must_use]
    pub fn new(mut values: Vec<f32>) -> Self {
        // Non-finite components come from a model that has gone wrong, and one
        // `NaN` makes every downstream comparison false rather than merely
        // wrong. Neutralize before measuring, not after.
        for value in &mut values {
            if !value.is_finite() {
                *value = 0.0;
            }
        }
        let norm = values
            .iter()
            .map(|v| f64::from(*v) * f64::from(*v))
            .sum::<f64>()
            .sqrt();
        // Already unit length within a rounding error — which is the common
        // case, because most of these arrive from a model that normalized or
        // from a round trip through storage. Skipping the divide keeps a stored
        // vector bit-identical to the one that was written, so "has this
        // cached vector changed?" is answerable by comparison.
        if norm > 0.0 && norm.is_finite() && (norm - 1.0).abs() > 1e-6 {
            for value in &mut values {
                *value = (f64::from(*value) / norm) as f32;
            }
        }
        Self(values)
    }

    /// How many dimensions.
    #[must_use]
    pub fn dim(&self) -> usize {
        self.0.len()
    }

    /// The components.
    #[must_use]
    pub fn as_slice(&self) -> &[f32] {
        &self.0
    }

    /// Cosine similarity, in `-1.0..=1.0`.
    ///
    /// Both operands are unit vectors, so this is a dot product. Vectors of
    /// different dimension are not comparable and score zero rather than
    /// silently comparing a prefix — mixing two models' vectors in one index is
    /// a configuration mistake, and a plausible-looking score would hide it.
    #[must_use]
    pub fn cosine(&self, other: &Self) -> f32 {
        if self.dim() != other.dim() {
            return 0.0;
        }
        self.0
            .iter()
            .zip(&other.0)
            .map(|(a, b)| a * b)
            .sum::<f32>()
            .clamp(-1.0, 1.0)
    }

    /// Little-endian `f32` bytes, for the vector index and the cache.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        self.0.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    /// Read back what [`Embedding::to_bytes`] wrote.
    ///
    /// `expect_dim` is the dimensionality the caller believes the row has —
    /// the model's, from the column stored beside it.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] if the length is not a whole number of `f32`s
    /// or does not match `expect_dim`. A truncated blob is corruption, and
    /// reading it as a shorter vector would turn corruption into a quietly
    /// wrong ranking: a dimension mismatch scores zero against everything, and
    /// since real cosines from these models are all comfortably positive, a
    /// zero sorts last instead of being reported.
    pub fn from_bytes(bytes: &[u8], expect_dim: usize) -> Result<Self, Error> {
        if bytes.len() % 4 != 0 {
            return Err(Error::invalid_argument(format!(
                "embedding blob of {} bytes is not a whole number of f32s",
                bytes.len()
            )));
        }
        if bytes.len() / 4 != expect_dim {
            return Err(Error::invalid_argument(format!(
                "embedding blob holds {} dimensions, expected {expect_dim}",
                bytes.len() / 4
            )));
        }
        let values = bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        // Already normalized when it was written; `new` re-normalizes rather
        // than trusting the blob, which costs one pass and removes a way for a
        // corrupt row to skew every comparison it appears in.
        Ok(Self::new(values))
    }
}

/// Turns text into vectors.
///
/// Implementations must be cheap to clone or share behind an [`Arc`]: one
/// embedder serves the whole process, because a model loaded twice is a model
/// that costs twice as much memory.
#[async_trait::async_trait]
pub trait Embedder: Send + Sync + std::fmt::Debug {
    /// The model id these vectors came from.
    ///
    /// Stored alongside every vector. Vectors from different models are not
    /// comparable, so a model change has to be detectable after the fact.
    fn model(&self) -> &str;

    /// How many dimensions this model produces.
    fn dim(&self) -> usize;

    /// Embed a batch, in order.
    ///
    /// The result has exactly one embedding per input, at the same index, so a
    /// caller can zip it back against whatever it was embedding. Inputs longer
    /// than [`MAX_INPUT_BYTES`] are truncated at a character boundary.
    ///
    /// # Errors
    ///
    /// Backend-specific: a model that will not load, an API that refuses, a
    /// network that is not there.
    async fn embed(&self, texts: &[String]) -> Result<Vec<Embedding>, Error>;

    /// Load whatever this backend needs before the first query does.
    ///
    /// Called at daemon start. The default asks for one trivial embedding,
    /// which is enough to force a lazy model load without every backend
    /// needing its own warm path.
    ///
    /// # Errors
    ///
    /// Whatever [`Embedder::embed`] would have returned.
    async fn warm(&self) -> Result<(), Error> {
        self.embed(std::slice::from_ref(&String::from("warm")))
            .await
            .map(|_| ())
    }
}

/// Build the configured embedder.
///
/// # Errors
///
/// [`Error::FailedPrecondition`] if the configured backend cannot be built at
/// all — a Voyage key command that does not exist, say. A backend that merely
/// has not loaded its model yet is not an error here; that is what
/// [`Embedder::warm`] is for.
pub fn build(config: &IndexSemanticConfig) -> Result<Arc<dyn Embedder>, Error> {
    let embedder: Arc<dyn Embedder> = match config.provider {
        SemanticProvider::Local => {
            #[cfg(feature = "onnx")]
            {
                Arc::new(local::LocalEmbedder::new(&config.local))
            }
            // Search that works offline and without a model is a stated
            // requirement, so a build without the ONNX backend degrades to
            // deterministic vectors rather than refusing to start. It says so
            // loudly, because the difference in result quality is large.
            #[cfg(not(feature = "onnx"))]
            {
                tracing::warn!(
                    "built without the `onnx` feature; semantic search will use \
                     deterministic hashed vectors, which are not semantic"
                );
                Arc::new(hash::HashEmbedder::new(config.local.dim as usize))
            }
        }
        SemanticProvider::Voyage => Arc::new(voyage::VoyageEmbedder::new(&config.voyage)?),
        SemanticProvider::None => Arc::new(hash::HashEmbedder::new(config.local.dim as usize)),
    };
    tracing::info!(
        model = embedder.model(),
        dim = embedder.dim(),
        "embedder ready"
    );
    Ok(embedder)
}

/// Cut `text` to [`MAX_INPUT_BYTES`] on a character boundary.
///
/// Byte-slicing a UTF-8 string at an arbitrary offset panics, and the offset
/// here is a fixed limit meeting arbitrary mail — so the boundary walk is not
/// defensive padding, it is the normal case for any message that happens to be
/// long and not in English.
#[must_use]
pub fn truncate(text: &str) -> &str {
    if text.len() <= MAX_INPUT_BYTES {
        return text;
    }
    let mut end = MAX_INPUT_BYTES;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.get(..end).unwrap_or("")
}

#[cfg(test)]
mod tests;
