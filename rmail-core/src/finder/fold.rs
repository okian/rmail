//! Unicode folding, the char-index map that keeps highlights honest, and the
//! O(1) presence mask that keeps the scan bounded.
//!
//! # NFKD-then-drop-marks, not NFKC
//!
//! prd.md asks for "NFKC + ASCII-fold" so that `cafe` matches `café`. Taken
//! literally those two steps fight each other: NFKC's whole job is to
//! *recompose*, so `e` + U+0301 comes back out as `é` and there is nothing
//! left for an ASCII fold to strip. [`fold`] therefore does the compatibility
//! decomposition (NFKD — which is where the useful half of "NFK" lives: `ﬁ`
//! becomes `fi`, `①` becomes `1`, a full-width `Ａ` becomes `A`) and then
//! drops combining marks, which is the ASCII fold. The result is identical to
//! NFKC for text that has no marks to begin with, and is what prd.md's own
//! worked example (`café` matching `cafe`) actually requires.
//!
//! Decomposition is done **per source character** rather than by running the
//! whole string through `nfkd()`, because [`fold_with_map`] has to be able to
//! say which source character each folded character came from. Canonical
//! reordering — the one thing a per-character decomposition does not do — is
//! irrelevant here: every character it would reorder is a combining mark, and
//! every combining mark is dropped.
//!
//! # Case survives folding, deliberately
//!
//! prd.md describes `match_blob` as "lowercased", and also specifies
//! smart-case ("any uppercase → case-sensitive"). Both cannot be true: a
//! lowercased blob has destroyed the only evidence smart-case reads. So the
//! blob keeps its case and case-folding happens per query, inside the
//! matcher, where the query is available to decide. See
//! [`super::score::Scorer`].
//!
//! # Why highlights need a map, and why byte offsets are not an option
//!
//! Folding is not length-preserving in *either* unit. `ﬁle` is 3 chars and 5
//! bytes; folded it is `file`, 4 chars and 4 bytes. A matcher position of 3
//! in the folded string is the `e`; interpreted against the original it is
//! past the end. Byte offsets are worse still — `rmail-cli`'s `search_cli`
//! already has a test pinning that a highlight range ending mid-`é` must be
//! rejected, and a fuzzy matcher hands back one position per matched
//! character, so *every* position would land mid-character on the first
//! multi-byte match.
//!
//! [`fold_with_map`] returns the folded text alongside `map[i] = the source
//! char index that produced folded char i`, so a position list can be
//! translated back into char offsets into the string the UI actually renders.
//! It is computed on demand for the handful of entries that reach the top-K,
//! never stored: at 4 bytes per folded character it would cost more than the
//! whole rest of an entry, against prd.md's < 25 MB budget for 100k messages.

use unicode_normalization::char::{decompose_compatible, is_combining_mark};

/// Bit 36 of a [`char_mask`] — set by any character that is neither an ASCII
/// letter nor an ASCII digit, and therefore by every non-Latin script.
///
/// Collapsing all of them into one bit is what keeps the mask a single
/// `u64` (26 letters + 10 digits + 1 catch-all). The cost is precision, in
/// the safe direction: a query containing any such character matches the bit
/// on every candidate that contains *any* such character, so those
/// candidates fall through to the real matcher instead of being rejected.
/// A prefilter that is too permissive costs time; one that is too strict
/// loses results, which is why the catch-all is a set bit rather than an
/// attempt to hash the character.
const OTHER_BIT: u32 = 36;

/// Fold `text` for matching: compatibility-decomposed, combining marks
/// dropped, case preserved.
///
/// This is what is stored in `finder_index.match_blob`.
#[must_use]
pub fn fold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for source in text.chars() {
        fold_char(source, |folded| out.push(folded));
    }
    out
}

/// Fold `text`, and record where each folded character came from.
///
/// `map[i]` is the **char** index into `text` of the character that produced
/// folded character `i`. See the module docs for why this is recomputed on
/// demand rather than stored, and why char indices rather than byte offsets.
#[must_use]
pub fn fold_with_map(text: &str) -> (String, Vec<u32>) {
    let mut out = String::with_capacity(text.len());
    let mut map = Vec::with_capacity(text.len());
    for (index, source) in text.chars().enumerate() {
        // Saturating rather than wrapping: a string long enough to overflow a
        // u32 char index is one no picker will ever render, and pinning every
        // position past that to the last representable index keeps the map
        // monotonic (so a caller's dedup/sort still behaves) instead of
        // wrapping to 0 and pointing a highlight at the wrong end.
        let index = u32::try_from(index).unwrap_or(u32::MAX);
        fold_char(source, |folded| {
            out.push(folded);
            map.push(index);
        });
    }
    (out, map)
}

/// Fold one character, emitting zero or more replacements.
///
/// Zero happens for a combining mark, which is the ASCII fold itself: the
/// acute in `e` + U+0301 contributes no folded character, which is exactly
/// what makes `cafe` match `café`.
fn fold_char(source: char, mut emit: impl FnMut(char)) {
    decompose_compatible(source, |part| {
        if !is_combining_mark(part) {
            emit(part);
        }
    });
}

/// The set of characters present in `folded`, as a bitmask.
///
/// The finder's cheap prefilter: a candidate whose mask does not contain
/// every bit of the query's mask cannot possibly contain every query
/// character, so it cannot possibly be a subsequence match, so the
/// `O(query × candidate)` aligner never runs on it. One `u64` AND per entry
/// instead of a dynamic-programming table is what makes a full scan of the
/// store affordable on every keystroke.
///
/// Case-insensitive on both sides (ASCII letters are lowercased into the
/// mask). That is required for correctness under smart-case, not just
/// convenient: with smart-case active the matcher is *stricter* than the
/// mask, and a prefilter is only ever allowed to be looser than the matcher
/// it guards.
#[must_use]
pub fn char_mask(folded: &str) -> u64 {
    let mut mask = 0u64;
    for c in folded.chars() {
        mask |= 1u64 << char_bit(c);
    }
    mask
}

/// The bit one character claims. See [`OTHER_BIT`] for the catch-all.
fn char_bit(c: char) -> u32 {
    let c = c.to_ascii_lowercase();
    if c.is_ascii_lowercase() {
        // 'a' => 0 ..= 'z' => 25.
        u32::from(c) - u32::from(b'a')
    } else if c.is_ascii_digit() {
        // '0' => 26 ..= '9' => 35.
        26 + (u32::from(c) - u32::from(b'0'))
    } else {
        OTHER_BIT
    }
}

/// Whether `candidate` could possibly contain every character of `query`.
///
/// False is a definite "no match"; true is "maybe" and means the matcher has
/// to run.
#[must_use]
pub fn mask_admits(candidate: u64, query: u64) -> bool {
    candidate & query == query
}

#[cfg(test)]
mod tests;
