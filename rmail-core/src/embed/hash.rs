//! The embedder that is always available.
//!
//! # What this is for
//!
//! It is not a semantic model and must not be mistaken for one. It is the
//! bottom of the ladder in [`super`]: a daemon built without the ONNX feature,
//! or running before a model has been provisioned, still needs to produce
//! vectors of the right shape so that the vector index, the cache and the
//! ranker have one code path rather than a populated one and an empty one.
//!
//! # What it actually captures
//!
//! Hashed features — words and character 3-grams — projected into `dim`
//! buckets with a sign drawn from the same hash, then normalized. That is a
//! random projection of a bag of features, so it recovers *lexical* overlap and
//! survives light misspelling through the 3-grams. It recovers no synonymy
//! whatsoever: "invoice" and "bill" are as unrelated here as "invoice" and
//! "trombone". A retrieval pipeline leaning on this alone would be a worse FTS5
//! index, which is why it is a fallback and not a choice.
//!
//! # Why signed hashing
//!
//! Two features colliding in a bucket cancel half the time instead of always
//! reinforcing, so collision noise stays zero-mean rather than accumulating
//! into a bias every vector shares. It is the standard fix for the hashing
//! trick and it costs one extra bit from a hash we already computed.

use crate::embed::{truncate, Embedder, Embedding};
use crate::error::Error;

/// Deterministic hashed-feature embeddings.
#[derive(Debug, Clone)]
pub struct HashEmbedder {
    dim: usize,
}

impl HashEmbedder {
    /// Smallest dimensionality worth producing.
    ///
    /// Below this, collisions dominate: with 64 buckets, a few dozen features
    /// collide so often the vector says more about the hash than the text.
    const MIN_DIM: usize = 64;

    /// An embedder producing `dim`-dimensional vectors.
    ///
    /// `dim` is clamped rather than rejected: it comes from configuration, and
    /// a daemon that will not start because somebody wrote `dim = 0` is a worse
    /// outcome than one that starts with a usable default.
    #[must_use]
    pub fn new(dim: usize) -> Self {
        Self {
            dim: dim.max(Self::MIN_DIM),
        }
    }

    /// One text's raw, unnormalized feature vector.
    fn features(&self, text: &str) -> Vec<f32> {
        let mut values = vec![0.0f32; self.dim];
        let lowered = truncate(text).to_lowercase();

        for word in lowered.split(|c: char| !c.is_alphanumeric()) {
            if word.is_empty() {
                continue;
            }
            self.add(&mut values, word.as_bytes(), 1.0);
            // Character 3-grams as well as whole words, so a typo costs
            // similarity rather than all of it. Over the word, not the whole
            // string, so a 3-gram never straddles a word boundary and turns two
            // unrelated words into a shared feature.
            let bytes = word.as_bytes();
            for gram in bytes.windows(3) {
                self.add(&mut values, gram, 0.5);
            }
        }
        values
    }

    /// Add one feature to its bucket, with the sign the hash chose.
    fn add(&self, values: &mut [f32], feature: &[u8], weight: f32) {
        let h = fnv1a(feature);
        // The high word picks the sign and the low word picks the bucket, so
        // the two really are independent. Taking the sign from bit 63 while the
        // bucket came from `h % dim` over the *whole* hash correlated them for
        // any `dim` that is not a power of two — 384, the default, among them —
        // which is exactly the case signed hashing exists to avoid.
        let sign = if (h >> 32) & 1 == 0 { 1.0 } else { -1.0 };
        let bucket = usize::try_from((h & 0xffff_ffff) % self.dim as u64).unwrap_or(0);
        if let Some(slot) = values.get_mut(bucket) {
            *slot += sign * weight;
        }
    }
}

#[async_trait::async_trait]
impl Embedder for HashEmbedder {
    fn model(&self) -> &str {
        // Named so it is unmistakable in a stored vector's `model` column: a
        // row embedded by the fallback must never be silently compared against
        // one from a real model.
        "hash-fallback"
    }

    fn dim(&self) -> usize {
        self.dim
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Embedding>, Error> {
        Ok(texts
            .iter()
            .map(|text| Embedding::new(self.features(text)))
            .collect())
    }

    async fn warm(&self) -> Result<(), Error> {
        // Nothing to load; overridden so daemon start does not do pointless
        // work and, more usefully, so a warm-up failure can only ever mean a
        // real backend failed.
        Ok(())
    }
}

/// FNV-1a, 64-bit.
///
/// Chosen for being short, dependency-free and deterministic across builds and
/// platforms — the last of which matters most: the vector for a message is
/// cached against its content hash, so a hash that varied by build would make
/// every cached vector wrong after an upgrade rather than merely stale.
fn fnv1a(bytes: &[u8]) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x1000_0000_01b3;
    let mut hash = OFFSET;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}
