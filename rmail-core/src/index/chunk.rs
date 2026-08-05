//! Splitting extracted text into the units a vector can actually mean.
//!
//! # Why chunk at all
//!
//! An embedding is one point for whatever it was given. Embed a two-thousand
//! word thread and the point sits at the average of everything in it, which is
//! near nothing in particular — the "topic drift" that makes long-document
//! retrieval worse than no retrieval. Chunking trades one vague point for
//! several sharp ones, and lets a citation quote the paragraph that matched
//! rather than the message that contained it.
//!
//! # Boundaries are chosen, not counted
//!
//! A fixed-width window cuts sentences in half, and half a sentence embeds to
//! something neither half means. Splits are taken at the strongest separator
//! available within the size budget: a blank line, then a line break, then a
//! sentence end, and only then a word boundary. The last of those is the floor
//! — a chunk never splits mid-word, because a fragment of a word is a token the
//! model has never seen.
//!
//! # Overlap, and what it costs
//!
//! Consecutive chunks share a tail so that a passage straddling a boundary is
//! whole in one of them. It is not free: overlapped text is embedded twice and
//! can return two hits for one passage, which the retriever deduplicates by
//! message. Sixty-four tokens against five hundred and twelve is the usual
//! ratio and buys most of the benefit for an eighth of the cost.
//!
//! # Input is not assumed to be normalized
//!
//! Everything reaching this today comes through `extract::normalize`, which
//! collapses whitespace runs to a single ASCII space — so in production the
//! paragraph and line tiers below rarely fire and multi-byte whitespace never
//! arrives. That is a property of the current caller, not of this module, and
//! `split` is public with a documented span contract. Every offset here is
//! therefore snapped to a character boundary rather than assumed to be on one.
//!
//! # Tokens are estimated
//!
//! The real count depends on the model's tokenizer, which is inside the
//! embedder and not worth loading to chunk. Four bytes per token is the
//! standard approximation for English and errs toward *over*-counting for
//! non-Latin scripts, so the estimate stays on the safe side of a context
//! limit. What matters here is that the estimate is stable: it decides where
//! boundaries fall, and boundaries decide chunk identity.

use crate::config::IndexSemanticConfig;

/// Bytes per token, for estimating a count without a tokenizer.
///
/// Conservative on purpose. English averages closer to four and a half; CJK and
/// other multi-byte scripts are denser in bytes per token, so this
/// over-estimates for them and the resulting chunk is smaller than the budget
/// rather than larger. Under a context limit, smaller is the safe direction.
const BYTES_PER_TOKEN: usize = 4;

/// Shortest chunk worth embedding, in bytes.
///
/// A trailing "Thanks," is a point in vector space that matches every polite
/// message ever sent. Dropping it costs nothing and removes a small flood of
/// meaningless near-duplicate hits.
const MIN_CHUNK_BYTES: usize = 48;

/// One piece of a part's text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// Position within the part, from zero.
    pub ordinal: usize,
    /// Byte offset of the chunk in the part's normalized text.
    pub span_start: usize,
    /// Byte offset just past it.
    pub span_end: usize,
    /// Estimated tokens.
    pub tokens: usize,
    /// The text itself, for embedding. Borrowed from the part.
    pub text: String,
}

/// How big chunks are and how much they overlap.
#[derive(Debug, Clone, Copy)]
pub struct ChunkSpec {
    /// Target size in tokens.
    pub tokens: usize,
    /// Tokens shared with the previous chunk.
    pub overlap: usize,
}

impl ChunkSpec {
    /// The spec a configuration asks for, with values that cannot produce a
    /// non-terminating or degenerate split.
    #[must_use]
    pub fn from_config(config: &IndexSemanticConfig) -> Self {
        // Clamped rather than rejected: these come from a config file, and a
        // daemon that refuses to start over `chunk_tokens = 0` is worse than
        // one that starts with something workable. An overlap at or above the
        // size would make every chunk start where the last one did and the
        // split would never terminate, so it is capped strictly below.
        // The ceiling is what the embedder will actually read, not an
        // arbitrary large number. Above it every backend silently truncates,
        // and the vector then answers for text it never saw — with no counter
        // anywhere that notices.
        const MAX_TOKENS: usize = crate::embed::MAX_INPUT_BYTES / BYTES_PER_TOKEN;
        let tokens = (config.chunk_tokens as usize).clamp(32, MAX_TOKENS);
        let overlap = (config.chunk_overlap as usize).min(tokens.saturating_sub(1) / 2);
        Self { tokens, overlap }
    }

    /// Target chunk size in bytes.
    fn window(self) -> usize {
        self.tokens * BYTES_PER_TOKEN
    }

    /// Overlap in bytes.
    fn stride_back(self) -> usize {
        self.overlap * BYTES_PER_TOKEN
    }
}

impl Default for ChunkSpec {
    fn default() -> Self {
        Self {
            tokens: 512,
            overlap: 64,
        }
    }
}

/// Estimated tokens in some text.
#[must_use]
pub fn estimate_tokens(text: &str) -> usize {
    text.len().div_ceil(BYTES_PER_TOKEN).max(1)
}

/// Split `text` into overlapping chunks.
///
/// Spans are byte offsets into `text` itself, so a citation can quote the
/// source rather than a copy.
#[must_use]
pub fn split(text: &str, spec: ChunkSpec) -> Vec<Chunk> {
    let trimmed_end = text.trim_end().len();
    if trimmed_end == 0 {
        return Vec::new();
    }
    let window = spec.window();
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut start = leading_space(text, 0);

    while start < trimmed_end {
        let end = if start + window >= trimmed_end {
            trimmed_end
        } else {
            boundary(text, start, start + window)
        };
        // `boundary` never returns a position at or before `start`, but a
        // caller-supplied window meeting pathological text is exactly where a
        // non-terminating loop would come from, so the invariant is enforced
        // rather than assumed.
        let end = end.max(next_boundary(text, start));
        let piece = text.get(start..end).unwrap_or("").trim();
        let offset = text
            .get(start..end)
            .map_or(0, |s| s.len() - s.trim_start().len());
        let short = piece.len() < MIN_CHUNK_BYTES;
        match chunks.last_mut() {
            // Absorbed, not dropped. A trailing "Thanks," on its own is a point
            // in vector space that matches every polite message ever sent — but
            // discarding it loses text from the index, and text that is missing
            // from a search index is not something anyone notices. Extending
            // the previous chunk avoids both.
            Some(previous) if short => {
                previous.span_end = start + offset + piece.len();
                previous.text = text
                    .get(previous.span_start..previous.span_end)
                    .unwrap_or(&previous.text)
                    .trim()
                    .to_owned();
                previous.tokens = estimate_tokens(&previous.text);
            }
            _ => chunks.push(Chunk {
                ordinal: chunks.len(),
                span_start: start + offset,
                span_end: start + offset + piece.len(),
                tokens: estimate_tokens(piece),
                text: piece.to_owned(),
            }),
        }
        if end >= trimmed_end {
            break;
        }
        // Step back by the overlap, but never to or before where this chunk
        // started: that would repeat the chunk for ever. The step-back lands
        // wherever the arithmetic puts it, which is usually mid-word, so it is
        // snapped to a word boundary for the same reason the forward boundary
        // is — a chunk beginning with "gilistic" starts on a token the model
        // has never seen.
        let back = end.saturating_sub(spec.stride_back());
        let floor = next_boundary(text, start);
        let snapped = word_start(text, char_boundary(text, back.max(floor)), floor);
        start = leading_space(text, snapped.max(floor));
    }
    chunks
}

/// The best split point in `text[from..limit]`.
///
/// Preference order is paragraph, line, sentence, word. Each is looked for in
/// the *later* part of the window only, so a separator near the start does not
/// produce a chunk a tenth of the intended size.
fn boundary(text: &str, from: usize, limit: usize) -> usize {
    let limit = char_boundary(text, limit.min(text.len()));
    let Some(window) = text.get(from..limit) else {
        return limit;
    };
    // Two thirds: far enough in that the chunk is worth having, early enough
    // that a real separator is usually available.
    let floor = window.len() * 2 / 3;

    let at = window
        .rfind("\n\n")
        .filter(|at| *at >= floor)
        .map(|at| at + 2)
        .or_else(|| {
            window
                .rfind('\n')
                .filter(|at| *at >= floor)
                .map(|at| at + 1)
        })
        .or_else(|| sentence_end(window, floor))
        .or_else(|| {
            // As in `word_start`: past the whitespace *character*, not past its
            // first byte. `rfind` returns where the character starts, and
            // U+00A0, U+2009 and U+3000 are two and three bytes long.
            window
                .char_indices()
                .rev()
                .find(|(at, c)| c.is_whitespace() && *at >= floor)
                .map(|(at, c)| at + c.len_utf8())
        });

    match at {
        // A word boundary is the floor: splitting mid-word produces a token the
        // model has never seen on both sides of the cut.
        Some(at) => from + at,
        None => limit,
    }
}

/// The end of the last sentence in `window` at or after `floor`.
fn sentence_end(window: &str, floor: usize) -> Option<usize> {
    let bytes = window.as_bytes();
    let mut best = None;
    for (at, byte) in bytes.iter().enumerate() {
        if !matches!(byte, b'.' | b'!' | b'?') {
            continue;
        }
        // A full stop is only a sentence end when whitespace follows. Without
        // this, "v1.2" and "example.com" become sentence boundaries.
        let after = at + 1;
        if bytes.get(after).is_some_and(|b| b.is_ascii_whitespace()) && after >= floor {
            best = Some(after);
        }
    }
    best
}

/// The next character boundary at or after `at`.
fn next_boundary(text: &str, at: usize) -> usize {
    let mut at = (at + 1).min(text.len());
    while at < text.len() && !text.is_char_boundary(at) {
        at += 1;
    }
    at
}

/// The character boundary at or before `at`.
fn char_boundary(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while at > 0 && !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// Move `at` back to the start of the word it is inside, but not before
/// `floor`.
///
/// `floor` is what keeps the split terminating: without it a step-back into a
/// very long word could land at or before the previous chunk's start and repeat
/// it for ever.
fn word_start(text: &str, at: usize, floor: usize) -> usize {
    let Some(prefix) = text.get(floor..at) else {
        return at;
    };
    if prefix.ends_with(char::is_whitespace) {
        return at;
    }
    match prefix.char_indices().rev().find(|(_, c)| c.is_whitespace()) {
        // Past the whitespace *character*, not past its first byte. `rfind`
        // returns where the character starts, and U+00A0, U+2009 and U+3000 are
        // two and three bytes long — so `+ 1` lands inside one, and every span
        // derived from that offset is an invalid slice that panics the moment a
        // citation quotes it.
        Some((space, c)) => floor + space + c.len_utf8(),
        // No whitespace since the floor: the "word" spans the whole window, so
        // there is no boundary to snap to and cutting where the budget says is
        // the only option that makes progress.
        None => at,
    }
}

/// Skip whitespace from `at`, so a chunk's span starts at its first character.
fn leading_space(text: &str, at: usize) -> usize {
    // Snapped rather than trusted. Every caller computes `at` by arithmetic on
    // a byte budget, and a `get` that returned `None` here used to fall through
    // and hand the invalid offset straight back — which is how one mid-character
    // step became a chunk whose span nobody could slice.
    let at = char_boundary(text, at);
    let Some(rest) = text.get(at..) else {
        return at;
    };
    at + (rest.len() - rest.trim_start().len())
}

#[cfg(test)]
mod tests;
