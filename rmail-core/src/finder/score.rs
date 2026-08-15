//! The subsequence scorer: `(score, positions)` for one query against one
//! candidate.
//!
//! # This is nucleo's DP, not a second one
//!
//! prd.md asks for "skim/fzf-style subsequence scoring ... implemented
//! in-crate (nucleo-style, no FFI)" and lists the bonus/penalty table it
//! wants. [`nucleo_matcher`] is already a workspace dependency, is already
//! what `retrieve::fuzzy` scores Part I's fuzzy retriever with, is pure Rust
//! (so "no FFI" is satisfied), and implements exactly that algorithm — the
//! bounded `O(query × candidate)` alignment DP with per-character bonuses
//! over a pre-classified character table. Writing a second one here would
//! mean two implementations of the same scorer in one binary, drifting apart
//! from the first typo fix onward, so this module *configures* nucleo rather
//! than reimplementing it.
//!
//! The constants line up almost exactly with prd.md's table — base 16, word
//! boundary 8, gap start −3, gap extension −1, first character doubled —
//! with two deliberate differences that come from nucleo and are worth
//! keeping. nucleo scores a consecutive match at 4 rather than prd.md's 8,
//! and camelCase at 5 rather than 7, and its own source explains why: fzf and
//! skim score consecutive runs off the *maximum* bonus in the run, which
//! double-counts, and nucleo's lower constants are what balance camelCase,
//! snake_case and plain consecutive matches against each other once that
//! double-counting is removed. Adopting prd.md's two numbers would mean
//! reintroducing the bug they were calibrated against.
//!
//! # Exact substring short-circuits the DP
//!
//! prd.md: "Exact substring → flat `+40`, short-circuits DP." Both halves
//! matter and they are separate mechanisms:
//!
//! - The **short-circuit** is `Matcher::substring_match`, nucleo's own
//!   scan-based substring path. It walks the candidate looking for the query
//!   run and never fills the alignment matrix, so the common case — a user
//!   who typed a real run of a real subject — costs `O(candidate)` instead
//!   of `O(query × candidate)`.
//! - The **flat bonus** is [`EXACT_SUBSTRING_BONUS`], added on top. Its job
//!   is ordering, not speed: without it a contiguous match and a scattered
//!   one can score within a few points of each other, and "the thing whose
//!   name literally contains what I typed" must not lose to an acronym
//!   coincidence.
//!
//! # Smart-case
//!
//! prd.md: "case-insensitive with smart-case (any uppercase → case-
//! sensitive)". [`Scorer::new`] decides that once per query, from the query,
//! and hands it to nucleo as `Config::ignore_case`. It is a per-query
//! decision rather than a per-candidate one because it is a property of what
//! was typed — which is also why `finder_index.match_blob` keeps its case
//! (see [`super::fold`]).
//!
//! nucleo case-folds the *haystack* only and compares the needle raw, so
//! whenever `ignore_case` is on the needle must arrive already lowercased.
//! That is not the same as "the user typed no uppercase": compatibility
//! decomposition can introduce it (`ǅ` → `Dž`, `㎐` → `Hz`). [`Scorer::new`]
//! therefore decides smart-case from the raw query and lowercases the
//! *folded* needle, so the two can never disagree.
//!
//! # Positions are computed for the winners, not for every candidate
//!
//! Scoring and highlighting are split into [`Scorer::score`] and
//! [`Scorer::positions`] rather than returned together, because a scan looks
//! at every entry in the store and at most `limit` of them are ever
//! rendered. Asking nucleo for indices costs an extra matrix traceback per
//! candidate and mapping them back costs a fold of the candidate's original
//! text; doing that for 100k rows to render 200 would dominate the keystroke
//! budget. The scan calls `score`; the top-K pass calls `positions`.
//!
//! # ...and they are char offsets, into a named string
//!
//! nucleo reports match positions as indices into the `Utf32Str` it was
//! given — never byte offsets. They are indices into the *folded* blob,
//! though, and folding is not length-preserving, so [`Scorer::positions`]
//! maps them back through [`super::fold::fold_with_map`] into char indices
//! in the original `primary_text`. Positions that land in the `secondary`
//! half of the blob are dropped rather than reported against the wrong
//! string: a picker row renders one highlightable line, and an index into a
//! string the caller did not ask about is worse than no highlight at all.
//!
//! One nucleo detail is load-bearing for that mapping and is the reason
//! [`utf32`] exists instead of a plain `Utf32Str::new` call.
//! `Utf32Str::new`, with nucleo's default features, segments a non-ASCII
//! string into **grapheme clusters** and keeps only each cluster's first
//! codepoint — so its indices count clusters, not characters. Folding does
//! not eliminate multi-codepoint clusters (NFKD explodes a Hangul syllable
//! into three jamo that remain one cluster), so a fold map counted in
//! characters and an index counted in clusters would silently disagree for
//! exactly the scripts least likely to be tested. [`utf32`] therefore hands
//! nucleo the codepoints directly — `Utf32Str::Ascii` when the folded text
//! is ASCII, `Utf32Str::Unicode` over the character array otherwise — so
//! nucleo's index space and [`super::fold`]'s are the same space by
//! construction.

use nucleo_matcher::{Config, Matcher, Utf32Str};

use super::fold;

/// Added to an exact-substring match. prd.md's number; see the module docs
/// for why it is a bonus on top of the substring score rather than a
/// replacement for it.
pub const EXACT_SUBSTRING_BONUS: u32 = 40;

/// The longest query the DP is ever run with.
///
/// A picker's query is something a human types into a one-line prompt; past
/// this length it is a paste, and the alignment matrix is `O(query ×
/// candidate)` in both time and space. Truncating (rather than refusing)
/// keeps a pasted subject line working as a query — it just stops getting
/// more selective after 64 characters, by which point it has already matched
/// at most a handful of entries.
pub const MAX_QUERY_CHARS: usize = 64;

/// The longest candidate blob the DP is ever run over.
///
/// The other half of the same bound. A 40 KB subject line is not something a
/// user is trying to jump to by name, and letting one into the matrix would
/// make a single pathological row cost more than the entire rest of the
/// scan. Enforced when the blob is *built* (see [`super::index`]), so the
/// truncation is paid once per message rather than once per keystroke.
pub const MAX_MATCH_CHARS: usize = 256;

/// A query, compiled once and then run against many candidates.
///
/// Holds nucleo's `Matcher` — which owns the reusable DP scratch buffers, so
/// constructing one per candidate would allocate the matrix per row — plus
/// the folded needle and this module's own scratch space. One per scan,
/// never shared: `Matcher` is `Send` but not `Sync`, which is the type
/// system correctly reporting that its scratch space is single-threaded.
pub struct Scorer {
    matcher: Matcher,
    /// The folded, length-capped needle.
    needle: String,
    /// `needle` as chars, so a non-ASCII query is converted once rather than
    /// per candidate.
    needle_chars: Vec<char>,
    /// Reused across candidates: `Utf32Str::new` only fills this for a
    /// non-ASCII haystack, and the finder's blobs are ASCII far more often
    /// than not once folding has run.
    haystack_chars: Vec<char>,
    /// Reused across the top-K position pass.
    indices: Vec<u32>,
    /// Every character the needle contains — the prefilter's query side.
    mask: u64,
    /// Whether the caller typed any uppercase, per prd.md's smart-case rule.
    case_sensitive: bool,
}

impl Scorer {
    /// Compile `query` into a reusable scorer.
    ///
    /// Returns `None` for a query with nothing to match: an empty string, or
    /// one that folds away to nothing (a lone combining mark). An empty
    /// query is not an error; it means "rank by signals alone", which the
    /// caller handles by not scoring at all rather than by asking this type
    /// for a score of zero.
    #[must_use]
    pub fn new(query: &str) -> Option<Self> {
        // Smart-case reads the *original* query, so the rule stays stated in
        // terms of what the user typed rather than in terms of what folding
        // did to it.
        let case_sensitive = query.chars().any(char::is_uppercase);
        let folded: String = fold::fold(query).chars().take(MAX_QUERY_CHARS).collect();
        // ...but the needle handed to nucleo must be case-folded whenever
        // `ignore_case` is on, and folding can *introduce* uppercase that the
        // check above never saw. Every one of these is reachable: U+01C5 `ǅ`
        // decomposes to `Dž` and is not `is_uppercase`; U+1D2C `ᴬ` becomes
        // `A`; U+3390 `㎐` becomes `Hz`. nucleo case-folds the haystack only
        // and compares the needle raw ("the needle argument must always be
        // normalized by the caller ... otherwise the matcher may fail to
        // produce a match"), so an uppercase needle character under
        // `ignore_case` can never match anything — the query would silently
        // return nothing, *including* against text containing the identical
        // character. Lowercasing here keeps the two halves consistent without
        // making smart-case depend on a decomposition the user never saw.
        let needle = if case_sensitive {
            folded
        } else {
            folded.to_lowercase()
        };
        if needle.is_empty() {
            return None;
        }
        let mut config = Config::DEFAULT;
        config.ignore_case = !case_sensitive;
        // The blob is already compatibility-decomposed and mark-stripped, so
        // nucleo's own latin normalization has nothing left to do; leaving it
        // on would cost a per-character table lookup on every DP cell for a
        // transformation already applied to both sides.
        config.normalize = false;
        Some(Self {
            matcher: Matcher::new(config),
            needle_chars: needle.chars().collect(),
            haystack_chars: Vec::new(),
            indices: Vec::new(),
            mask: fold::char_mask(&needle),
            needle,
            case_sensitive,
        })
    }

    /// The query's character mask, for the caller's prefilter.
    #[must_use]
    pub fn mask(&self) -> u64 {
        self.mask
    }

    /// Whether smart-case put this query in case-sensitive mode.
    #[must_use]
    pub fn case_sensitive(&self) -> bool {
        self.case_sensitive
    }

    /// The folded needle, as the prefilter and tests see it.
    #[must_use]
    pub fn needle(&self) -> &str {
        &self.needle
    }

    /// Score `blob` (a candidate's folded `match_blob`), or `None` when the
    /// query is not a subsequence of it.
    pub fn score(&mut self, blob: &str) -> Option<u32> {
        let Self {
            matcher,
            needle,
            needle_chars,
            haystack_chars,
            case_sensitive,
            ..
        } = self;
        haystack_chars.clear();
        if !blob.is_ascii() {
            haystack_chars.extend(blob.chars());
        }
        let haystack = utf32(blob, haystack_chars);
        let needle_str = utf32(needle, needle_chars);
        if is_substring(blob, needle, *case_sensitive) {
            return matcher
                .substring_match(haystack, needle_str)
                .map(|score| u32::from(score) + EXACT_SUBSTRING_BONUS);
        }
        matcher.fuzzy_match(haystack, needle_str).map(u32::from)
    }

    /// Where this query matched inside `primary`, as ascending char offsets
    /// into `primary` itself.
    ///
    /// `blob` is the same folded blob [`Scorer::score`] was given, and
    /// `primary_folded_len` is how many of its leading folded characters came
    /// from `primary` — the boundary past which a position belongs to the
    /// secondary text and is dropped. Returns an empty vector when the query
    /// does not match at all, which a caller that already scored the entry
    /// will never see.
    pub fn positions(&mut self, blob: &str, primary: &str, primary_folded_len: usize) -> Vec<u32> {
        let Self {
            matcher,
            needle,
            needle_chars,
            haystack_chars,
            indices,
            case_sensitive,
            ..
        } = self;
        indices.clear();
        haystack_chars.clear();
        if !blob.is_ascii() {
            haystack_chars.extend(blob.chars());
        }
        let haystack = utf32(blob, haystack_chars);
        let needle_str = utf32(needle, needle_chars);
        let matched = if is_substring(blob, needle, *case_sensitive) {
            matcher
                .substring_indices(haystack, needle_str, indices)
                .is_some()
        } else {
            matcher
                .fuzzy_indices(haystack, needle_str, indices)
                .is_some()
        };
        if !matched {
            return Vec::new();
        }
        map_positions(indices, primary, primary_folded_len)
    }
}

/// A `Utf32Str` over `text`'s **codepoints**, using `chars` (which the caller
/// has already filled with `text.chars()` when `text` is not ASCII).
///
/// Not `Utf32Str::new`: see the module docs — that constructor collapses
/// grapheme clusters, which would put nucleo's indices in a different space
/// from [`super::fold`]'s map and mis-place highlights in exactly the scripts
/// where a cluster spans several codepoints.
fn utf32<'a>(text: &'a str, chars: &'a [char]) -> Utf32Str<'a> {
    if text.is_ascii() {
        // The `Ascii` variant's documented invariant, checked immediately
        // above. For ASCII, byte index == char index, so this arm is in the
        // same index space as the other.
        Utf32Str::Ascii(text.as_bytes())
    } else {
        Utf32Str::Unicode(chars)
    }
}

/// Whether `needle` appears in `blob` as a contiguous run, honoring
/// smart-case.
///
/// Deciding here — rather than always calling nucleo's `substring_*` and
/// checking for `None` — keeps the negative case from touching the matcher's
/// state at all: `str::contains` is a memchr-backed scan and is cheap enough
/// to pay unconditionally.
fn is_substring(blob: &str, needle: &str, case_sensitive: bool) -> bool {
    if needle.is_empty() {
        return true;
    }
    if case_sensitive {
        blob.contains(needle)
    } else {
        contains_ignore_ascii_case(blob, needle)
    }
}

/// Translate folded-blob char indices into `primary`'s own char indices.
///
/// Positions at or past `primary_folded_len` matched inside the secondary
/// half of the blob and are dropped; see the module docs. The result is
/// sorted and deduped because folding is many-to-one — `ﬁ` folds to two
/// characters, both of which map back to the same source index, and a
/// renderer handed the same offset twice would either double-emit a marker
/// or mis-pair its open/close pair.
fn map_positions(indices: &[u32], primary: &str, primary_folded_len: usize) -> Vec<u32> {
    if indices.is_empty() {
        return Vec::new();
    }
    let (_, map) = fold::fold_with_map(primary);
    let mut out: Vec<u32> = indices
        .iter()
        .copied()
        .filter(|index| (*index as usize) < primary_folded_len)
        .filter_map(|index| map.get(index as usize).copied())
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// ASCII-case-insensitive substring test.
///
/// Only ASCII case is folded, which is deliberately *narrower* than the full
/// Unicode lowercasing `Config::ignore_case` applies inside nucleo. The
/// asymmetry is safe in exactly one direction, and this is that direction: a
/// substring this misses falls through to the fuzzy path, which finds the
/// same match and merely loses the flat bonus, whereas a false positive
/// would claim a substring nucleo then fails to locate — which would score
/// the candidate `None` and drop a genuine match.
///
/// Comparing raw bytes is sound over UTF-8: a multi-byte sequence contains
/// no ASCII byte, so a window that matches an ASCII-case-insensitive needle
/// cannot straddle a character boundary.
fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    let needle = needle.as_bytes();
    if needle.is_empty() || needle.len() > haystack.len() {
        return needle.is_empty();
    }
    haystack
        .as_bytes()
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}

#[cfg(test)]
mod tests;
