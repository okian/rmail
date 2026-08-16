//! Turning the model's markdown into a briefing whose every line is linked to
//! a message this daemon actually retrieved.
//!
//! # The document is engine-authored; only the sentences are not
//!
//! [`parse`] does not trust the model's document structure at all. It reads
//! the model's markdown for *bullets under a recognized heading*, throws away
//! everything else (preamble, invented sections, tables, closing remarks), and
//! then [`Briefing::render`] emits a fresh document with the five sections in
//! the fixed order [`Section::ALL`] declares, every one of them present even
//! when empty. So the shape of a briefing is a property of this module rather
//! than of a particular response, and "the sections are the five prd.md names"
//! cannot regress on a model's whim.
//!
//! # A line with no resolvable citation does not appear
//!
//! The acceptance criterion is that every line is linked to source
//! message-ids. That is enforced here by construction rather than asked for in
//! the prompt: [`parse`] resolves each bullet's `[n]` markers against the
//! sources that were actually packed, and a bullet whose markers all resolve
//! to nothing is dropped and counted in [`Briefing::dropped_uncited`]. What
//! survives is rewritten — the positional `[n]` the model wrote becomes
//! `[msg:<message_id>]`, the local identity — so the rendered markdown carries
//! message ids inline rather than requiring a reader to consult a legend.
//!
//! Dropping rather than failing: a model that forgets one citation in eight
//! should cost the reader that line, not the whole week's briefing. A briefing
//! where *every* line was dropped is a different matter and the caller treats
//! it as a refusal — see [`super::DigestEngine::generate`].
//!
//! # Why the model never sees a message id
//!
//! Sources are labelled positionally (`[1]`, `[2]`, ...) exactly as
//! [`crate::ai::rag::cite`] labels them, for the two reasons that module gives
//! at length: a `messages.id` is an unbounded digit run
//! [`crate::ai::redact`] may tokenize mid-prompt, and nothing a model can say
//! about a row id is more useful than "the fourth one". The mapping from label
//! to id is a lookup this module performs, so there is no text a model could
//! emit that produces a citation naming a message the digest never packed.

use std::collections::BTreeSet;

use crate::ai::rag::context::Source;

/// One of the five sections prd.md names, in the order a briefing presents
/// them.
///
/// A closed vocabulary on purpose. The model is told these five and nothing
/// else; a heading it invents is not mapped to one of these and its bullets
/// are discarded, which is what stops a briefing from quietly growing a
/// sixth section that no client renders.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Section {
    /// Mail a reasonable recipient is expected to answer.
    NeedsReply,
    /// Worth knowing, nothing to do.
    Fyi,
    /// The user is waiting on somebody else.
    WaitingOn,
    /// A rule, filter or automation already dealt with it.
    AutoHandled,
    /// Deliberately not worth the reader's attention.
    Skipped,
}

impl Section {
    /// Every section, in presentation order.
    pub const ALL: [Self; 5] = [
        Self::NeedsReply,
        Self::Fyi,
        Self::WaitingOn,
        Self::AutoHandled,
        Self::Skipped,
    ];

    /// The stable identifier a client keys on — also what the wire carries.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::NeedsReply => "needs_reply",
            Self::Fyi => "fyi",
            Self::WaitingOn => "waiting_on",
            Self::AutoHandled => "auto_handled",
            Self::Skipped => "skipped",
        }
    }

    /// The heading this section is written with, in the prompt and in the
    /// rendered briefing alike. Byte-identical in both directions so the
    /// parser recognizes exactly what the prompt asked for.
    #[must_use]
    pub const fn heading(self) -> &'static str {
        match self {
            Self::NeedsReply => "Needs reply",
            Self::Fyi => "FYI",
            Self::WaitingOn => "Waiting on",
            Self::AutoHandled => "Auto-handled",
            Self::Skipped => "Skipped",
        }
    }

    /// Match a heading the model wrote against this vocabulary.
    ///
    /// Lenient about case, surrounding punctuation and the hyphen/space/
    /// underscore a model might choose between (`Waiting-on`, `waiting on`,
    /// `WAITING_ON:`), strict about the words themselves. Leniency here is not
    /// trust: an unrecognized heading still discards its bullets, so the worst
    /// a mismatch costs is a section rendered empty.
    #[must_use]
    pub fn from_heading(text: &str) -> Option<Self> {
        let normalized: String = text
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .map(|c| c.to_ascii_lowercase())
            .collect();
        Self::ALL
            .into_iter()
            .find(|section| section.normalized_heading() == normalized)
    }

    fn normalized_heading(self) -> String {
        self.heading()
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .map(|c| c.to_ascii_lowercase())
            .collect()
    }
}

/// One bullet of a briefing, with the messages it points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    /// The model's sentence, with its citation markers already rewritten to
    /// `[msg:<id>]` — see [`Briefing::render`].
    pub text: String,
    /// `messages.id` for every source this line cited, in the order the line
    /// named them, deduplicated. Never empty: a line with no resolvable
    /// citation is not a [`Line`].
    pub message_ids: Vec<i64>,
}

/// A parsed, validated briefing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Briefing {
    /// `(section, lines)` for each of [`Section::ALL`], in that order, always
    /// all five present.
    pub sections: Vec<(Section, Vec<Line>)>,
    /// Bullets discarded because nothing they cited was a source this digest
    /// packed (including bullets that cited nothing at all).
    pub dropped_uncited: usize,
    /// Labels named by surviving bullets that resolved to no source. Counted
    /// separately from [`Self::dropped_uncited`] because a line citing `[2]`
    /// and `[99]` keeps its real citation and loses only the fabricated one.
    pub dangling: usize,
    /// Positions in `sources` that at least one surviving line cited.
    pub cited: BTreeSet<usize>,
}

impl Briefing {
    /// Whether any section holds a line. An empty briefing over a non-empty
    /// window is a refusal, not a result — see [`super::DigestEngine`].
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.sections.iter().all(|(_, lines)| lines.is_empty())
    }

    /// How many lines survived, across every section.
    #[must_use]
    pub fn line_count(&self) -> usize {
        self.sections.iter().map(|(_, lines)| lines.len()).sum()
    }

    /// The briefing as markdown: a fixed `## ` heading per section in
    /// [`Section::ALL`] order, its surviving bullets beneath it, and `_none_`
    /// where a section has none.
    ///
    /// Written here rather than passed through from the model so the document
    /// a client stores and a human reads is one this codebase authored. The
    /// prose inside a bullet is the model's; the heading, the ordering, the
    /// bullet marker and the citation are not.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::with_capacity(512 + self.line_count() * 128);
        for (section, lines) in &self.sections {
            out.push_str("## ");
            out.push_str(section.heading());
            out.push('\n');
            if lines.is_empty() {
                out.push_str("_none_\n\n");
                continue;
            }
            for line in lines {
                out.push_str("- ");
                out.push_str(&line.text);
                out.push('\n');
            }
            out.push('\n');
        }
        // One trailing newline, not two: the loop above separates sections
        // with a blank line and the last one does not need a separator.
        while out.ends_with("\n\n") {
            out.pop();
        }
        out
    }
}

/// The briefing a window with no mail in it gets, without a model call.
///
/// Rendered through the same [`Briefing::render`] every other briefing goes
/// through, so an empty week is the same document shape as a busy one —
/// exactly five headings, all `_none_` — rather than a special-cased string a
/// client has to detect. See [`super::DigestEngine::generate`] on why an empty
/// window must not become an empty prompt.
#[must_use]
pub fn empty_briefing() -> Briefing {
    Briefing {
        sections: Section::ALL
            .into_iter()
            .map(|section| (section, Vec::new()))
            .collect(),
        ..Briefing::default()
    }
}

/// Parse one model response into a [`Briefing`] over `sources`.
///
/// Never fails: a response this cannot make sense of yields an empty briefing,
/// which the caller turns into a refusal. There is no partially-trusted middle
/// state — every line in the result cites at least one real source.
#[must_use]
pub fn parse(markdown: &str, sources: &[Source]) -> Briefing {
    let mut sections: Vec<(Section, Vec<Line>)> = Section::ALL
        .into_iter()
        .map(|section| (section, Vec::new()))
        .collect();
    let mut dropped = 0usize;
    let mut dangling = 0usize;
    let mut cited: BTreeSet<usize> = BTreeSet::new();
    // `None` until the first recognized heading: text before any heading is
    // preamble, and a bullet in it belongs to no section.
    let mut current: Option<Section> = None;

    for raw in markdown.lines() {
        let line = raw.trim();
        if let Some(rest) = heading_text(line) {
            // An unrecognized heading closes the current section rather than
            // leaving it open — otherwise a `## Notes` block's bullets would
            // silently join whichever real section preceded it.
            current = Section::from_heading(rest);
            continue;
        }
        let Some(body) = bullet_text(line) else {
            continue;
        };
        let Some(section) = current else {
            // A bullet outside any recognized section. Counted, because
            // silently ignoring it would make a model that emitted its whole
            // briefing under one invented heading look like a quiet week.
            dropped += 1;
            continue;
        };
        if body.is_empty() || body == "_none_" || body.eq_ignore_ascii_case("none") {
            // The prompt's own "write `_none_`" instruction, echoed as a
            // bullet instead of as a bare line. Not a dropped claim.
            continue;
        }
        let (text, ids, missing) = rewrite(body, sources, &mut cited);
        dangling += missing;
        if ids.is_empty() {
            dropped += 1;
            continue;
        }
        if let Some((_, lines)) = sections.iter_mut().find(|(s, _)| *s == section) {
            lines.push(Line {
                text,
                message_ids: ids,
            });
        }
    }

    Briefing {
        sections,
        dropped_uncited: dropped,
        dangling,
        cited,
    }
}

/// The text of a `#`-style heading, at any level.
fn heading_text(line: &str) -> Option<&str> {
    let rest = line.strip_prefix('#')?;
    Some(rest.trim_start_matches('#').trim())
}

/// The text of a markdown bullet (`-`, `*`, `+` or `1.`), or `None` when the
/// line is not one.
fn bullet_text(line: &str) -> Option<&str> {
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = line.strip_prefix(marker) {
            return Some(rest.trim());
        }
    }
    // `1.` / `12)` ordered bullets.
    let digits = line.bytes().take_while(u8::is_ascii_digit).count();
    if digits > 0 {
        let rest = line.get(digits..)?;
        for marker in [". ", ") "] {
            if let Some(rest) = rest.strip_prefix(marker) {
                return Some(rest.trim());
            }
        }
    }
    None
}

/// Rewrite one bullet's `[n]` markers to `[msg:<id>]`, returning the rewritten
/// text, the message ids it resolved to (in order, deduplicated) and how many
/// markers named no source.
///
/// The scan is the same shape [`crate::ai::rag::cite`]'s is — a bracketed run
/// of digits, commas and spaces — because it has to recognize exactly what
/// that module's prompt convention produces. A marker naming a label outside
/// `1..=sources.len()` is deleted from the text as well as counted, so a
/// fabricated `[99]` cannot survive into a rendered briefing looking like a
/// citation.
///
/// # The output form cannot be written by the model
///
/// A resolved marker is rendered `[msg:<id>]`, and a model that has seen that
/// convention (or guessed it) can write one *itself*. Nothing else here would
/// stop it: `[msg:41]` is not a digit run, so it is not a marker, so it would
/// be copied through verbatim — and the rendered briefing a human reads would
/// then carry a citation to a message the line never actually cited, with no
/// corresponding entry in [`Line::message_ids`]. Worse, the read-back path
/// ([`super::cached_report`]) recovers a stored briefing's citations from that
/// very syntax, so the fabrication would survive a round trip through the
/// database and reach a client as structured data.
///
/// So any bracketed group that is *not* a marker but does contain `msg:` is
/// rewritten `(...)` — the same rewrite-don't-strip treatment
/// [`crate::ai::rag::cite::neutralize_markers`] gives sender-authored text.
/// The reader still sees the number; only this function can mint a citation.
/// The whole group is rewritten, not just its opening bracket, so the result
/// is balanced (`(msg:41)`, never `(msg:41]`), and the test is `contains`
/// rather than `starts_with` so a mixed group like `[1, msg:41]` — which is
/// not a marker either, since `msg:` is not a digit run — cannot smuggle one
/// through beside a real label.
fn rewrite(
    body: &str,
    sources: &[Source],
    cited: &mut BTreeSet<usize>,
) -> (String, Vec<i64>, usize) {
    let mut out = String::with_capacity(body.len() + 16);
    let mut ids: Vec<i64> = Vec::new();
    let mut missing = 0usize;
    let bytes = body.as_bytes();
    let mut i = 0usize;
    while i < body.len() {
        if bytes[i] != b'[' {
            // `[` is ASCII, so anything else starts a char we copy whole.
            let ch = body[i..].chars().next().unwrap_or('\u{fffd}');
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        let Some(close) = bytes[i + 1..].iter().position(|b| *b == b']') else {
            out.push('[');
            i += 1;
            continue;
        };
        let inner = body.get(i + 1..i + 1 + close).unwrap_or_default();
        let is_marker = !inner.is_empty()
            && inner
                .bytes()
                .all(|b| b.is_ascii_digit() || b == b',' || b == b' ');
        if !is_marker {
            if inner.contains("msg:") {
                // Not a marker, but shaped like this function's own output —
                // see the docs above. Rewritten whole, so nothing downstream
                // can read it back as a citation.
                out.push('(');
                out.push_str(inner);
                out.push(')');
                i += close + 2;
                continue;
            }
            out.push('[');
            i += 1;
            continue;
        }
        let mut rendered: Vec<String> = Vec::new();
        for part in inner.split(',') {
            let Ok(label) = part.trim().parse::<usize>() else {
                continue;
            };
            let Some(index) = label.checked_sub(1) else {
                missing += 1;
                continue;
            };
            let Some(source) = sources.get(index) else {
                missing += 1;
                continue;
            };
            cited.insert(index);
            if !ids.contains(&source.message_id) {
                ids.push(source.message_id);
            }
            rendered.push(format!("msg:{}", source.message_id));
        }
        if !rendered.is_empty() {
            out.push('[');
            out.push_str(&rendered.join(", "));
            out.push(']');
        }
        i += close + 2;
    }
    // A marker that resolved to nothing leaves a hole where it stood; collapse
    // the double space rather than shipping `handled by ops  .`.
    let text = out.split_whitespace().collect::<Vec<_>>().join(" ");
    (text.trim().to_owned(), ids, missing)
}
