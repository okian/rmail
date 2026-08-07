//! Snippet extraction + query-term highlighting (prd.md, Stage 6: "best-
//! matching span extracted (FTS5 `snippet()` for lexical, best chunk for
//! semantic), query terms highlighted using match positions").
//!
//! # Why not FTS5's own `snippet()`
//!
//! `fts_messages` is a **contentless** FTS5 table (`content=''` — see
//! `index::fts`'s own "Contentless, deliberately" doc section): the inverted
//! index is stored, the original text is not, because keeping a second copy
//! of every body would double the largest table in the database. SQLite's
//! `snippet()`/`highlight()` auxiliary functions need the original column
//! text to reconstruct a span from, which a contentless table structurally
//! cannot supply — SQLite raises an error the moment either is called against
//! one. This is a schema property, not a choice this task gets to make:
//! prd.md's own parenthetical ("FTS5 `snippet()` ... **or** the best chunk")
//! already anticipates a case where `snippet()` cannot apply, and for this
//! build's contentless index that is *every* case, lexical hits included —
//! not only the semantic ones prd.md's own wording expects it for. This
//! module reads straight from `index_content` instead (the same table
//! `fuse::collapse_near_duplicates` and `features::extract`'s local text
//! scan already read for the identical reason) and reimplements the "best
//! matching span, best-effort" idea that `snippet()` would otherwise give,
//! uniformly for both the lexical and the semantic path.
//!
//! # Offsets, never embedded markup — the safety design
//!
//! [`Snippet`] carries `text` and a list of byte ranges *into* that text
//! (`highlights`) rather than text with delimiter characters (`<mark>...
//! </mark>`, `**...**`, ...) spliced in. This is not a style preference; it
//! is what makes this module's output safe against both injection directions
//! the task's own acceptance criteria name:
//!
//! - **A message body that happens to contain the delimiter text itself**
//!   (a newsletter whose own copy reads literally "click **here**", a body
//!   quoting HTML source with a literal `<mark>` in it) cannot be confused
//!   with a highlight this module inserted, because this module never
//!   inserts any — there is nothing in `text` for a body's own content to
//!   collide with. A downstream renderer applies `highlights` to `text`
//!   itself, so "does the body already contain my markup" is a question that
//!   never has to be asked.
//! - **A query term containing FTS5 metacharacters** (`"`, `*`, `NEAR(`, a
//!   bare `OR`) cannot restructure anything, because this module never
//!   builds a `MATCH` expression or any other query-language string from
//!   query text at all — matching here is a plain, byte-level substring/
//!   token comparison ([`eq_ignore_ascii_case`]) against the already-fetched
//!   text. A term is data to compare, never syntax to parse, so there is no
//!   parser for it to confuse.
//!
//! # Known limitation: ASCII-only case folding
//!
//! Matching is case-insensitive on ASCII bytes only
//! ([`eq_ignore_ascii_case`]); a non-ASCII byte compares exactly. Full
//! Unicode case folding (`str::to_lowercase`) can change a string's byte
//! length (German `ß` → `ss`, some Turkish/Greek/Cyrillic forms), which would
//! break the one invariant this module depends on for safety: every match
//! position is a substring of the *original* text's own byte offsets, with
//! no separately-lowered copy to realign against. Folding only the ASCII
//! subset costs a case-insensitive match on the (rare, for mail search)
//! length-changing non-ASCII case pairs, in exchange for every offset this
//! module ever returns being provably a valid slice of its input — the same
//! trade [`crate::fuse::simhash`]'s own "Known limitation: CJK and other
//! unspaced scripts" section makes for a different reason, documented rather
//! than silently wrong.
//!
//! # Known limitation: CJK and other unspaced scripts merge adjacent tokens
//!
//! [`tokenize`]'s boundary is `char::is_alphanumeric`, the same rule
//! [`crate::fuse::simhash::fingerprint`] uses for the identical reason (see
//! that module's own "unspaced scripts" section) — which means a Latin term
//! embedded directly in unspaced CJK text with no separating punctuation or
//! whitespace becomes part of *one* token together with its neighbors, not
//! its own token: `extract("你好invoice世界", ["invoice"])` returns `None`,
//! not a match, because the whole string tokenizes as a single run. A
//! genuinely space-separated body (the common case even for mixed-script
//! mail, and what `index::extract`'s own whitespace-normalization produces
//! from real message text) is unaffected.

use std::collections::BTreeSet;
use std::ops::Range;

use crate::query::parse::{self, Mode};
use crate::retrieve::lexical::has_indexable_content;

/// Characters of source text this module scans for a window — matches the
/// `MAX_BODY_CHARS_FOR_*` convention already established by
/// `fuse::MAX_BODY_CHARS_FOR_SIMHASH` and
/// `features::extract::MAX_BODY_CHARS_FOR_SCAN` (both `4_000`): generous
/// relative to what a snippet needs (the best-matching span is essentially
/// always near the start of a realistic message), capped so one very long
/// body cannot dominate a batch's memory/CPU.
pub const MAX_SOURCE_CHARS: usize = 4_000;

/// Target width, in bytes, of an extracted snippet window. Not a hard limit —
/// the window is snapped outward to the nearest token boundary, so the final
/// snippet is usually a little wider than this.
const WINDOW_BYTES: usize = 220;

/// How much of [`WINDOW_BYTES`] sits *before* a match anchor versus after it.
/// Weighted toward "after" because a query term is more often the subject of
/// a sentence than its conclusion, and English readers scan left to right —
/// showing more of what *follows* a hit reads more like a real excerpt than
/// a window centered mechanically on the match.
const WINDOW_BEFORE_FRACTION: usize = 2; // 2/5 before, 3/5 after
const WINDOW_FRACTION_DENOM: usize = 5;

/// Ellipsis inserted where a snippet's window does not reach the source
/// text's own start/end. Not a delimiter a consumer could confuse with a
/// highlight boundary — it carries no [`Snippet::highlights`] range and is
/// this module's own inserted text, never derived from `text`.
const ELLIPSIS: &str = "…";

/// A displayable excerpt plus where the query matched within it.
///
/// `highlights` are byte ranges into `text`, sorted ascending and
/// non-overlapping (touching matches are merged — see [`merge_ranges`]).
/// Empty `highlights` is a normal, common outcome: a semantic-only hit whose
/// best chunk happens to share no literal words with the query, or any
/// fallback excerpt ([`plain_excerpt`]) taken with no query to match against
/// at all.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Snippet {
    /// The excerpt itself — a substring of the source text, possibly
    /// prefixed/suffixed with [`ELLIPSIS`] when the window does not reach
    /// the source's own edges.
    pub text: String,
    /// Byte ranges into `text` where a query term or phrase matched.
    pub highlights: Vec<Range<usize>>,
}

/// The query's terms and phrases, extracted once per query (not once per
/// candidate) and reused across every candidate's [`extract`] call.
///
/// Re-parses `raw` fresh via [`parse::parse`] rather than reading
/// [`crate::query::QueryPlan::lexical_terms`]/`expansions` — the same choice
/// `features::extract` makes for its own local text scan, and for the same
/// reason (see that module's "Re-parsing `plan.raw`" doc section): a
/// snippet should highlight what the user actually typed, not a spell-fixed
/// or PMI-expanded sibling the retriever that actually found this candidate
/// may never have matched on.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct QueryTerms {
    /// Non-negated, non-`~`-forced-semantic free-text terms, original case,
    /// deduplicated case-insensitive-ASCII (first occurrence kept).
    pub terms: Vec<String>,
    /// Non-negated quoted phrases, original case.
    pub phrases: Vec<String>,
}

/// Build [`QueryTerms`] from a raw query string.
///
/// Filters mirror [`crate::features::extract`]'s own `scan_terms`/
/// `scan_phrases` exactly (non-negated, non-[`Mode::Semantic`],
/// [`has_indexable_content`]) — deliberately duplicated rather than imported,
/// since those helpers are private to `features::extract`; the same "a
/// three-line duplication beats a cross-module private dependency" call this
/// crate already makes more than once (see `fuse::source_ordinal`'s doc
/// comment for the precedent).
#[must_use]
pub fn query_terms(raw: &str) -> QueryTerms {
    let parsed = parse::parse(raw);
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let terms = parsed
        .terms
        .into_iter()
        .filter(|t| !t.negated && t.mode != Mode::Semantic && has_indexable_content(&t.text))
        .filter(|t| seen.insert(t.text.to_ascii_lowercase()))
        .map(|t| t.text)
        .collect();
    let phrases = parsed
        .phrases
        .into_iter()
        .filter(|p| !p.negated && has_indexable_content(&p.text))
        .map(|p| p.text)
        .collect();
    QueryTerms { terms, phrases }
}

/// Extract the best-matching window of `text` against `terms`/`phrases`,
/// with highlight ranges for every match the chosen window contains.
///
/// Returns `None` when `text` is empty/whitespace-only, or when neither a
/// term nor a phrase occurs anywhere in it — the caller's cue to fall
/// through to another text source (a semantic best chunk) or finally to
/// [`plain_excerpt`], per prd.md's "FTS5 `snippet()` ... or the best chunk"
/// fallback chain.
#[must_use]
pub fn extract(text: &str, terms: &[String], phrases: &[String]) -> Option<Snippet> {
    if text.trim().is_empty() {
        return None;
    }
    let source = cap_chars(text, MAX_SOURCE_CHARS);

    let mut matches: Vec<Range<usize>> = Vec::new();
    for token in tokenize(source) {
        if terms
            .iter()
            .any(|term| eq_ignore_ascii_case(token.text, term))
        {
            matches.push(token.start..token.end);
        }
    }
    for phrase in phrases {
        matches.extend(find_phrase(source, phrase));
    }
    if matches.is_empty() {
        return None;
    }
    matches.sort_by_key(|r| r.start);

    let window = best_window(source, &matches);
    Some(build_snippet(source, window, &matches))
}

/// A snippet with no highlights: the source text's own opening, snapped to a
/// word boundary and capped to roughly [`WINDOW_BYTES`]. The last resort in
/// the fallback chain — used when nothing in this candidate's available text
/// matched the query at all (a semantic hit with no literal word overlap, or
/// a query with no free-text terms to match in the first place).
#[must_use]
pub fn plain_excerpt(text: &str) -> Snippet {
    let source = cap_chars(text.trim(), MAX_SOURCE_CHARS);
    if source.is_empty() {
        return Snippet::default();
    }
    // Short enough already: return it whole, with no truncation and
    // therefore no ellipsis. Skipping straight to this rather than always
    // calling `word_boundary_at_or_before(source, source.len())` matters
    // when `source` ends with a real word and no trailing separator — that
    // call would walk backward through the *entire* final word looking for
    // a non-alphanumeric character to stop at, finding none until the word
    // itself starts, and incorrectly drop it from a text that needed no
    // truncation in the first place.
    if source.len() <= WINDOW_BYTES {
        return Snippet {
            text: source.to_owned(),
            highlights: Vec::new(),
        };
    }
    let end = word_boundary_at_or_before(source, WINDOW_BYTES);
    let end = if end == 0 {
        next_char_boundary(source, 0)
    } else {
        end
    };
    let mut out = source[..end].to_owned();
    if !out.ends_with(char::is_whitespace) {
        out.push(' ');
    }
    out.push_str(ELLIPSIS);
    Snippet {
        text: out,
        highlights: Vec::new(),
    }
}

// ---------------------------------------------------------------------------
// Matching
// ---------------------------------------------------------------------------

/// One alphanumeric run in the source text, with its original byte span —
/// the same tokenization rule `features::extract::tokenize_lower` and
/// `fuse::simhash::fingerprint` both use (`char::is_alphanumeric` as the
/// boundary), but keeping the original-case slice and byte offsets instead
/// of discarding them: a highlight range has to point back into the *exact*
/// source text this function was given, not a lowered copy of it (see the
/// module docs' "Known limitation" section for why that distinction is load-
/// bearing for safety, not just precision).
struct Token<'a> {
    text: &'a str,
    start: usize,
    end: usize,
}

fn tokenize(text: &str) -> Vec<Token<'_>> {
    let mut tokens = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in text.char_indices() {
        if c.is_alphanumeric() {
            start.get_or_insert(i);
        } else if let Some(s) = start.take() {
            tokens.push(Token {
                text: &text[s..i],
                start: s,
                end: i,
            });
        }
    }
    if let Some(s) = start {
        tokens.push(Token {
            text: &text[s..],
            start: s,
            end: text.len(),
        });
    }
    tokens
}

/// Case-insensitive-on-ASCII string equality — see the module docs' "Known
/// limitation" section. Two strings of different byte length can never be
/// equal under this rule (a length-changing Unicode case fold would be the
/// only way they could be, and this function does not perform one).
fn eq_ignore_ascii_case(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.as_bytes().eq_ignore_ascii_case(b.as_bytes())
}

/// Every non-overlapping occurrence of `phrase` in `haystack`, case-
/// insensitive-on-ASCII, tested at every valid UTF-8 boundary rather than
/// assuming `phrase`'s byte length lines up with a haystack char boundary at
/// every offset it might start scanning from — a phrase and a haystack can
/// disagree in which of their bytes begin a character, so both ends of every
/// candidate window are checked before slicing either.
fn find_phrase(haystack: &str, phrase: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    if phrase.is_empty() || phrase.len() > haystack.len() {
        return out;
    }
    let hb = haystack.len();
    let pb = phrase.len();
    let mut start = 0usize;
    while start + pb <= hb {
        if haystack.is_char_boundary(start) {
            let end = start + pb;
            if haystack.is_char_boundary(end) && eq_ignore_ascii_case(&haystack[start..end], phrase)
            {
                out.push(start..end);
                start = end;
                continue;
            }
        }
        start += 1;
    }
    out
}

// ---------------------------------------------------------------------------
// Window selection
// ---------------------------------------------------------------------------

/// Choose the [`WINDOW_BYTES`]-ish window of `text` that contains the most
/// matches, snapped outward to token boundaries so a window never opens or
/// closes mid-word.
///
/// Tries each match as an anchor (its window is built around that match's
/// own position, split `WINDOW_BEFORE_FRACTION`/`WINDOW_FRACTION_DENOM`
/// before it and the rest after), counts how many *other* matches the
/// resulting window also fully contains, and keeps the anchor with the
/// highest count — ties broken by the earliest anchor start, so the result
/// is a deterministic function of `matches`' order (itself already sorted by
/// [`extract`]) rather than of iteration order.
fn best_window(text: &str, matches: &[Range<usize>]) -> Range<usize> {
    let mut best_window = raw_window(text, &matches[0]);
    let mut best_count = count_contained(&best_window, matches);
    for anchor in &matches[1..] {
        let candidate = raw_window(text, anchor);
        let count = count_contained(&candidate, matches);
        if count > best_count {
            best_count = count;
            best_window = candidate;
        }
    }
    snap_to_tokens(text, best_window)
}

/// The raw (not yet token-snapped) byte window around `anchor`, clamped to
/// `text`'s bounds and re-expanded on the side that did *not* get clamped so
/// the total width stays close to [`WINDOW_BYTES`] even near the start/end
/// of `text` — a match one byte into a long body still gets a window mostly
/// showing what follows it, not a window truncated to almost nothing because
/// there was no room to its left.
///
/// The window always covers `anchor` in full, even when `anchor` itself is
/// longer than [`WINDOW_BYTES`] (a long quoted phrase): `end` is clamped to
/// *at least* `anchor.end`, not derived from `anchor.start` alone. Without
/// this, a match wider than the nominal "after" budget could sit partly
/// outside its own anchor window, get filtered out of `count_contained` and
/// then out of [`build_snippet`]'s highlights entirely — `extract` would
/// return `Some` for a message with a genuine match and still highlight
/// nothing.
fn raw_window(text: &str, anchor: &Range<usize>) -> Range<usize> {
    let before = WINDOW_BYTES * WINDOW_BEFORE_FRACTION / WINDOW_FRACTION_DENOM;
    let after = WINDOW_BYTES - before;
    let mut start = anchor.start.saturating_sub(before);
    let mut end = (anchor.start + after).max(anchor.end).min(text.len());
    let width = end - start;
    if width < WINDOW_BYTES {
        let deficit = WINDOW_BYTES - width;
        if start == 0 {
            end = (end + deficit).min(text.len());
        } else {
            start = start.saturating_sub(deficit);
        }
    }
    start..end
}

/// How many of `matches` fall entirely inside `window`.
fn count_contained(window: &Range<usize>, matches: &[Range<usize>]) -> usize {
    matches
        .iter()
        .filter(|m| m.start >= window.start && m.end <= window.end)
        .count()
}

/// Widen `window` outward to the nearest token (or text) boundary on each
/// side, so the final snippet never opens or closes mid-word. Widening,
/// never narrowing: a window that already sits mid-word only grows to
/// include the whole word it cut into, rather than dropping a partial match
/// at the edge.
fn snap_to_tokens(text: &str, window: Range<usize>) -> Range<usize> {
    let start = word_boundary_at_or_before(text, window.start);
    let end = word_boundary_at_or_after(text, window.end);
    start..end
}

/// How far [`word_boundary_at_or_before`]/[`word_boundary_at_or_after`] will
/// walk past their starting point looking for a non-alphanumeric character,
/// before giving up and cutting mid-word anyway.
///
/// Text with no word boundary at all for a long stretch is real, not
/// theoretical: CJK scripts have no inter-word whitespace (`char::
/// is_alphanumeric` is true for essentially every character in a Han/Kana/
/// Hangul sentence, the same "unspaced scripts" gap
/// `crate::fuse::simhash`'s own docs name), and a long unbroken token
/// (base64, a hash, a tracking id) is exactly the shape a real body
/// produces. Without a cap, [`snap_to_tokens`] can walk to the edges of the
/// whole (already-capped-at-[`MAX_SOURCE_CHARS`]) source looking for a
/// boundary that never comes — turning a nominal `WINDOW_BYTES`-wide
/// snippet into the entire source, and [`plain_excerpt`]'s
/// `word_boundary_at_or_before(source, WINDOW_BYTES)` call into a walk all
/// the way back to byte `0`, yielding a one-character excerpt for a CJK
/// message instead of a merely-imperfect mid-character-run cut. A bounded
/// walk trades "never cuts a word" for "never blows the budget by more than
/// this much," which is the safer failure mode for a value task 33 streams
/// over gRPC and a client renders inline.
const MAX_SNAP_BYTES: usize = 96;

/// The nearest char boundary at or before `at` that does not sit inside an
/// alphanumeric run — i.e. a safe place to *start* a slice without cutting a
/// word in half — bounded to at most [`MAX_SNAP_BYTES`] past `at` (see that
/// constant's doc comment for why the walk cannot be unbounded).
///
/// Steps back one whole character at a time (via
/// `str::chars().next_back()`'s own byte length), never merely "snap to the
/// nearest boundary" — a position already sitting on a valid boundary must
/// still make progress backward when its preceding character is
/// alphanumeric, which a snap-only step (a no-op when already valid) would
/// loop on forever.
fn word_boundary_at_or_before(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    // First snap down to a valid boundary, in case `at` itself landed
    // mid-character (a raw, not-yet-snapped window edge).
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    let floor = at.saturating_sub(MAX_SNAP_BYTES);
    while at > floor {
        let Some(c) = text[..at].chars().next_back() else {
            break;
        };
        if !c.is_alphanumeric() {
            break;
        }
        at -= c.len_utf8();
    }
    at
}

/// The nearest char boundary at or after `at` that does not sit inside an
/// alphanumeric run — the end-side twin of
/// [`word_boundary_at_or_before`], with the identical [`MAX_SNAP_BYTES`]
/// bound.
fn word_boundary_at_or_after(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while at < text.len() && !text.is_char_boundary(at) {
        at += 1;
    }
    let ceiling = (at + MAX_SNAP_BYTES).min(text.len());
    while at < ceiling {
        let Some(c) = text[at..].chars().next() else {
            break;
        };
        if !c.is_alphanumeric() {
            break;
        }
        at += c.len_utf8();
    }
    at
}

fn next_char_boundary(text: &str, at: usize) -> usize {
    let mut at = (at + 1).min(text.len());
    while at < text.len() && !text.is_char_boundary(at) {
        at += 1;
    }
    at
}

/// Build the final [`Snippet`]: slice `text` to `window`, add ellipsis where
/// the window does not reach `text`'s own edges, and re-express every match
/// in `all_matches` that falls inside `window` as an offset into the
/// *output* string (accounting for a leading ellipsis prefix shifting every
/// later byte).
fn build_snippet(text: &str, window: Range<usize>, all_matches: &[Range<usize>]) -> Snippet {
    let body = &text[window.clone()];
    let prefix = if window.start > 0 {
        format!("{ELLIPSIS} ")
    } else {
        String::new()
    };
    let suffix = if window.end < text.len() {
        format!(" {ELLIPSIS}")
    } else {
        String::new()
    };
    let shift = prefix.len();

    let mut highlights: Vec<Range<usize>> = all_matches
        .iter()
        .filter(|m| m.start >= window.start && m.end <= window.end)
        .map(|m| (m.start - window.start + shift)..(m.end - window.start + shift))
        .collect();
    merge_ranges(&mut highlights);

    let mut out = String::with_capacity(prefix.len() + body.len() + suffix.len());
    out.push_str(&prefix);
    out.push_str(body);
    out.push_str(&suffix);

    Snippet {
        text: out,
        highlights,
    }
}

/// Sort-and-merge overlapping/touching ranges into a minimal, non-overlapping
/// set — a phrase match and one of its own words matched as a separate term
/// (`"quarterly report"` the phrase, `report` the term) can otherwise produce
/// two highlight ranges that share bytes, which is not wrong exactly but is
/// a worse contract for a consumer than the guarantee "ranges never overlap"
/// this function establishes unconditionally.
fn merge_ranges(ranges: &mut Vec<Range<usize>>) {
    ranges.sort_by_key(|r| r.start);
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for range in ranges.drain(..) {
        match merged.last_mut() {
            Some(last) if range.start <= last.end => {
                last.end = last.end.max(range.end);
            }
            _ => merged.push(range),
        }
    }
    *ranges = merged;
}

/// `text`, truncated to at most `max_chars` **characters** (not bytes) —
/// snapped to a char boundary so the cap can never split a multi-byte
/// character, matching `index::chunk`'s own boundary-snapping discipline.
fn cap_chars(text: &str, max_chars: usize) -> &str {
    match text.char_indices().nth(max_chars) {
        Some((byte_at, _)) => &text[..byte_at],
        None => text,
    }
}

#[cfg(test)]
mod tests;
