//! 64-bit SimHash fingerprinting over word-bigram shingles, and the Hamming
//! distance test used to call two fingerprints "near-duplicate" (prd.md's
//! Stage 2: "near-duplicate bodies (bulk newsletters, quoted replies)
//! collapse via SimHash so one query doesn't return ten copies").
//!
//! # Why bigram shingles, not a bag of words
//!
//! A unigram (bag-of-words) SimHash treats "the roadmap for Q3" and "Q3 for
//! the roadmap" identically — same multiset, same fingerprint — which is far
//! looser than "near-duplicate" should mean for mail search. Two messages
//! that merely share vocabulary (a reply that mentions the same three nouns
//! as the original, in different sentences making a different point) would
//! collapse together under unigrams. Word bigrams ("the roadmap", "roadmap
//! for", "for q3") encode local order, so genuinely different text produces
//! a different shingle set even when the word multiset overlaps heavily —
//! the false-positive direction the task cares about most: collapsing two
//! real, distinct results hides mail the user asked for, which is worse than
//! failing to collapse an actual duplicate.
//!
//! # Why weighted by occurrence count, not just presence
//!
//! A shingle repeated many times (a quoted signature block, a repeated
//! disclaimer, the bulk of a forwarded copy) is stronger evidence that "this
//! text recurs verbatim" than a shingle seen once. Voting per *occurrence*
//! (not per distinct shingle) into the per-bit sum means a long block of
//! literally-repeated text dominates the fingerprint the way it should for
//! detecting "this is the same block of text again" — exactly the
//! quoted-reply/forward case this module exists for.
//!
//! # Why a minimum of 12 tokens, not "at least one bigram"
//!
//! A short body fingerprints from very little evidence: "ok thanks", "lgtm
//! ship it", and "sounds good to me" each produce exactly one or two
//! shingles, and two *unrelated* messages that happen to share one of those
//! stock phrases fingerprint identically (distance 0) purely because there
//! was nothing else to disambiguate them on. That is exactly the
//! false-positive direction this module is supposed to avoid — a search for
//! "lgtm" that returns forty approvals from a dozen different reviewers on a
//! dozen different threads must not collapse to one result. Twelve tokens
//! (roughly two short sentences) is enough shingles that two genuinely
//! different short replies are very unlikely to coincide, while a real
//! duplicate (even a short automated notification resent verbatim) still
//! clears it easily. See `simhash/tests.rs` for the specific short-reply
//! phrases this bar exists to keep apart.
//!
//! # Known limitation: CJK and other unspaced scripts
//!
//! Tokenizing on `char::is_alphanumeric` treats an entire unpunctuated CJK
//! sentence as a single "word" (Han/Kana/Hangul are alphanumeric), so a
//! sentence-level, not word-level, unit gets shingled. A short CJK body
//! (fewer than [`MIN_TOKENS_FOR_FINGERPRINT`] "words" under this scheme) may
//! never fingerprint at all, and a longer one shingles far more coarsely
//! than the bigram scheme intends — near-dup collapse degrades toward
//! exact-duplicate-only for these scripts rather than failing outright, but
//! it is not doing what this module's docs claim for a Latin-script body.
//! Proper CJK segmentation needs a dictionary/statistical tokenizer this
//! module does not have; [`crate::index`] already tracks `index_content.lang`
//! if a future task wants to route non-Latin bodies to a different scheme.

/// Hamming-distance ceiling for "near-duplicate", out of the 64 fingerprint
/// bits. `3` mirrors the threshold large-scale SimHash near-duplicate
/// detection has settled on elsewhere (Google's own published web near-dup
/// work uses the same value at the same fingerprint width): loose enough to
/// survive a differing tracking id or footer, tight enough that two
/// messages sharing a topic but not the same underlying text sit far
/// outside it. See `simhash/tests.rs` for the false-positive-direction check
/// this constant has to satisfy — a merely-similar pair must land well past
/// this threshold, not just past it by one bit.
pub const NEAR_DUP_HAMMING_THRESHOLD: u32 = 3;

/// Minimum token count before a body is eligible to fingerprint at all — see
/// this module's "Why a minimum of 12 tokens" doc section. Below this, a
/// message can never join or start a near-duplicate cluster
/// ([`super::collapse_near_duplicates`]).
pub const MIN_TOKENS_FOR_FINGERPRINT: usize = 12;

/// A small, dependency-free, **stable-forever** 64-bit hash (FNV-1a) for
/// shingle hashing.
///
/// Deliberately not `std::collections::hash_map::DefaultHasher`: its output
/// is explicitly documented as unspecified and free to change between Rust
/// releases. Fingerprints here are computed fresh per query and never
/// persisted, so a toolchain bump changing them would not corrupt stored
/// data — but it would silently change which messages collapse in
/// production *and* could break `simhash/tests.rs` for a reason with
/// nothing to do with this module's own code. FNV-1a is a well-known,
/// ~10-line algorithm; owning it here removes that whole class of drift for
/// the cost of a few lines instead of a new workspace dependency.
const fn fnv1a64(bytes: &[u8]) -> u64 {
    const FNV_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET_BASIS;
    let mut i = 0;
    while i < bytes.len() {
        hash ^= bytes[i] as u64;
        hash = hash.wrapping_mul(FNV_PRIME);
        i += 1;
    }
    hash
}

/// Hash one bigram (a pair of adjacent tokens) to a 64-bit shingle hash.
///
/// The tokens are hashed with a `\0` separator between them: alphanumeric
/// tokens can never themselves contain a NUL byte, so `("a", "bc")` and
/// `("ab", "c")` cannot collide into the same byte string the way naive
/// concatenation would.
fn hash_bigram(a: &str, b: &str) -> u64 {
    let mut bytes = Vec::with_capacity(a.len() + b.len() + 1);
    bytes.extend_from_slice(a.as_bytes());
    bytes.push(0);
    bytes.extend_from_slice(b.as_bytes());
    fnv1a64(&bytes)
}

/// Compute a 64-bit SimHash fingerprint over `text`'s lowercase,
/// alphanumeric-run word-bigram shingles.
///
/// Returns `None` for text with fewer than [`MIN_TOKENS_FOR_FINGERPRINT`]
/// words — both because a short body has too few bigrams to shingle
/// meaningfully (a single word has none at all) and because a fingerprint
/// built from very little evidence is a false-positive risk in its own
/// right (see this module's "Why a minimum of 12 tokens" doc section). A
/// candidate with no fingerprint can never join or start a near-duplicate
/// cluster — see [`super::collapse_near_duplicates`].
#[must_use]
pub fn fingerprint(text: &str) -> Option<u64> {
    let tokens: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(str::to_lowercase)
        .collect();
    if tokens.len() < MIN_TOKENS_FOR_FINGERPRINT {
        return None;
    }

    // One signed vote accumulator per bit: positive means "more shingles had
    // this bit set than clear," which is what the final threshold-at-zero
    // step below turns back into a single fingerprint bit.
    let mut votes = [0i64; 64];
    for pair in tokens.windows(2) {
        let shingle_hash = hash_bigram(&pair[0], &pair[1]);
        for (bit, vote) in votes.iter_mut().enumerate() {
            if shingle_hash & (1u64 << bit) != 0 {
                *vote += 1;
            } else {
                *vote -= 1;
            }
        }
    }

    let mut fp: u64 = 0;
    for (bit, vote) in votes.iter().enumerate() {
        if *vote > 0 {
            fp |= 1u64 << bit;
        }
    }
    Some(fp)
}

/// Hamming distance between two fingerprints: the number of differing bits.
#[must_use]
pub fn hamming_distance(a: u64, b: u64) -> u32 {
    (a ^ b).count_ones()
}

/// Whether `a` and `b` are close enough to call near-duplicate.
#[must_use]
pub fn is_near_duplicate(a: u64, b: u64) -> bool {
    hamming_distance(a, b) <= NEAR_DUP_HAMMING_THRESHOLD
}

#[cfg(test)]
mod tests;
