//! The panes task 85 layers over the message list: search, the fuzzy finder,
//! the command palette, the ask pane, the outbox and the AI quick-action menu
//! — plus the collapsible AI panel, which is a column rather than an overlay.
//!
//! # What lives here and what does not
//!
//! Each overlay's *state* and the pure functions over it. No `Model`, no
//! `Msg`, no command dispatch: `tui::model` still owns the state machine, and
//! an overlay is a field in it. Splitting the panes out keeps `model.rs`
//! readable, and — because everything here is data plus total functions over
//! it — lets the highlighting, truncation and completion logic be tested
//! without building a `Model` at all.
//!
//! The behaviour tests for the overlays *do* live here (`tests`), because
//! task 85's acceptance names `tui::overlays` as where they are found. They
//! drive `tui::model::update` end to end, exactly as `tui::model::tests`
//! does; this module is where they are indexed, not a second harness.
//!
//! # Streams, and why the panes hold a generation
//!
//! Search, the finder and the ask pane are all fed by server streams that the
//! next keystroke supersedes. `update` is pure and cannot cancel anything, so
//! every pane carries a `generation` that the command it issued was stamped
//! with, and a frame whose generation is not the current one is dropped. The
//! executor also aborts the superseded task (see `tui::grpc`), but that race
//! is not winnable from the model's side and does not need to be: a late
//! frame from an abandoned query is *data about a query nobody is running*,
//! and the only safe thing to do with it is ignore it.
//!
//! The finder's stream needs one more rule. `FinderService.Find` sends
//! **snapshots**, not deltas — each `FindBatch` is the complete current top-K
//! — because a bounded heap can evict an entry it already sent. So a batch
//! *replaces* [`FinderPane::items`]; appending would keep showing results the
//! server has since rejected. `SearchService.Search` is the opposite (hits
//! stream once, in rank order) and appends.
//!
//! # Untrusted text
//!
//! Everything these panes draw is written by someone else: a subject and a
//! snippet by whoever sent the mail, the ask pane's prose by a model that a
//! hostile message can steer. A bidi override reorders a line, a raw `ESC`
//! run repaints the screen — and a TUI is a screen an attacker would very
//! much like to repaint. [`safe_line`] and [`safe_prose`] both go through
//! `crate::terminal_safe`, the one definition of "safe to show" this crate
//! has (bidi and invisibles via `injection::sanitize_model_text`, then C0/C1
//! controls); the only difference is that a row folds newlines to spaces and
//! prose keeps them.
//!
//! # Highlights land on characters, never bytes
//!
//! Two different highlight coordinate systems arrive from the daemon, and
//! both are easy to get wrong in the same way:
//!
//! - `FindResult.positions` are **char** offsets into `primary_text`
//!   ([`runs_from_char_positions`]).
//! - `Snippet.highlights` are **byte** ranges into `snippet.text`
//!   ([`runs_from_byte_ranges`]).
//!
//! Both renderers decide "is this character highlighted" from the character's
//! position in the *original* string and only then push its sanitized form,
//! so dropping a control character cannot shift a highlight off the character
//! it belongs to. A byte range that does not land on a char boundary is
//! discarded rather than rounded — `search_cli` has the same rule and the
//! test that pins it, and rounding would highlight a different substring than
//! the one that matched.

#[cfg(test)]
mod tests;

use rmail_core::command::{self, Verb};
use rmail_core::keymap::{Action, Keymap, Mode};
use rmail_core::query::parse::OPERATORS;

use crate::{terminal_safe, terminal_safe_char};

/// The most rows any overlay keeps. The daemon caps its own result sets well
/// below this; the cap is here so a misbehaving stream cannot grow the model
/// without bound while the user watches.
pub const MAX_ROWS: usize = 500;

/// The most characters of streamed answer the ask pane retains.
///
/// A model that never emits a stop token would otherwise grow a `String` for
/// as long as the daemon keeps the stream open. Truncation is marked, and it
/// happens on a char boundary because `String::truncate` panics otherwise.
pub const MAX_ANSWER_CHARS: usize = 64 * 1024;

// ---------------------------------------------------------------------------
// terminal-safe text
// ---------------------------------------------------------------------------

/// Make untrusted text safe to draw on **one line**.
///
/// [`terminal_safe`](crate::terminal_safe) neutralizes bidi overrides,
/// invisibles and control characters but deliberately keeps `\n`, which is
/// right for prose and wrong for a table row — a subject containing a newline
/// would otherwise shear the row it is drawn in. ratatui would render the
/// `\n` as a blank cell rather than a break, which is not a security problem
/// but is a rendering one; folding to a space keeps the row one line.
#[must_use]
pub fn safe_line(text: &str) -> String {
    terminal_safe(text).replace('\n', " ")
}

/// Make untrusted text safe to draw as prose, keeping paragraph breaks.
#[must_use]
pub fn safe_prose(text: &str) -> String {
    terminal_safe(text)
}

/// The first `max` characters of `text`, with an ellipsis when it was cut.
///
/// Characters, never bytes: a byte truncation of "café" can land inside the
/// `é` and produce something that is not a `str` at all.
#[must_use]
pub fn truncate_chars(text: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let mut out: String = text.chars().take(max).collect();
    if text.chars().nth(max).is_some() {
        out.push('…');
    }
    out
}

/// Split `text` into alternating (run, highlighted) pieces from **char**
/// offsets — `FindResult.positions`' coordinate system.
///
/// Positions outside the string are ignored rather than clamped: a highlight
/// on a character that is not there is not a highlight, and clamping it onto
/// the last character would mark the wrong one.
#[must_use]
pub fn runs_from_char_positions(text: &str, positions: &[usize]) -> Vec<(String, bool)> {
    runs(text, |index, _byte| positions.contains(&index))
}

/// Split `text` into alternating (run, highlighted) pieces from **byte**
/// ranges — `Snippet.highlights`' coordinate system, already validated by
/// [`valid_byte_ranges`].
#[must_use]
pub fn runs_from_byte_ranges(text: &str, ranges: &[(usize, usize)]) -> Vec<(String, bool)> {
    runs(text, |_index, byte| {
        ranges
            .iter()
            .any(|(start, end)| byte >= *start && byte < *end)
    })
}

/// Walk `text` deciding highlighting from each character's position in the
/// *original* string, then push its sanitized form.
///
/// That order is the whole trick: sanitizing first and then applying offsets
/// would desync every highlight after the first dropped control character.
fn runs(text: &str, highlighted: impl Fn(usize, usize) -> bool) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = Vec::new();
    for (index, (byte, ch)) in text.char_indices().enumerate() {
        let on = highlighted(index, byte);
        // One character at a time through the shared rule: it can drop a
        // character entirely (a bare `ESC`) or substitute it (`\t`), and both
        // outcomes have to be attributed to *this* character's highlight
        // state rather than to a run boundary computed afterwards.
        //
        // `terminal_safe_char` rather than `safe_line` on a one-character
        // string: this runs per glyph, per hit, per frame, and a streaming
        // search repaints on every hit that lands.
        let Some(safe) = terminal_safe_char(ch) else {
            continue;
        };
        // `safe_line`'s newline fold, applied here for the same reason: a run
        // is drawn on one line.
        let safe = if safe == '\n' { ' ' } else { safe };
        match out.last_mut() {
            Some((run, run_on)) if *run_on == on => run.push(safe),
            _ => out.push((String::from(safe), on)),
        }
    }
    out
}

/// `highlights` from the wire, kept only when they are a non-empty,
/// in-bounds, char-boundary-respecting slice of `text`.
///
/// The daemon's own contract already guarantees this; the value still crossed
/// a wire, and a bad range must degrade to "not highlighted" rather than
/// panic a slice or mark a partial code point.
#[must_use]
pub fn valid_byte_ranges(text: &str, highlights: &[(u32, u32)]) -> Vec<(usize, usize)> {
    highlights
        .iter()
        .filter_map(|(start, end)| {
            let start = usize::try_from(*start).ok()?;
            let end = usize::try_from(*end).ok()?;
            (start < end
                && end <= text.len()
                && text.is_char_boundary(start)
                && text.is_char_boundary(end))
            .then_some((start, end))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// search
// ---------------------------------------------------------------------------

/// Which half of the search overlay the keyboard is talking to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SearchFocus {
    /// Typing the query. Results stream in underneath as they arrive.
    #[default]
    Query,
    /// Walking the results. This is where `x` can mean "why did this match"
    /// rather than typing an `x`.
    Results,
}

/// One ranked hit, as the overlay draws it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    /// `messages.id`.
    pub message_id: i64,
    /// Decoded subject, or a placeholder.
    pub subject: String,
    /// Display name or bare address.
    pub from: String,
    /// `Date` header, unix seconds.
    pub date: Option<i64>,
    /// The snippet's text, verbatim from the daemon (sanitized at draw time,
    /// so the byte offsets below still address it).
    pub snippet: String,
    /// Validated byte ranges into [`Hit::snippet`].
    pub highlights: Vec<(usize, usize)>,
    /// Which retrievers surfaced it.
    pub sources: Vec<String>,
}

/// A rank explanation, pre-formatted.
///
/// The numbers are rendered to strings at the wire seam rather than carried
/// as `f64`: `Model` is compared with `assert_eq!` all over its tests, and a
/// float in it would cost `Eq` on every enum that reaches it for no gain a
/// renderer can use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Explanation {
    /// Which hit this explains.
    pub message_id: i64,
    /// The L1 score.
    pub score: String,
    /// `(name, "value=… weight=… -> …")`, in the daemon's order.
    pub features: Vec<(String, String)>,
    /// Which retrievers contributed.
    pub sources: Vec<String>,
    /// The matched span, when the daemon reported one.
    pub matched: Option<String>,
    /// The L2 reranker's one-line "why", when one ran.
    pub claude_reason: String,
}

/// `/` — streaming ranked search over the mailbox.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SearchPane {
    /// What has been typed, passed to the daemon verbatim.
    ///
    /// Never inspected here for `~`/`=` sigils. That grammar has exactly one
    /// implementation (`rmail_core::query::parse`), and `search_cli`'s module
    /// docs spell out at length why a client re-implementing even the
    /// "strip one leading character" part of it is how two parsers for one
    /// syntax start to drift.
    pub query: String,
    /// The generation the outstanding `Search` was issued under.
    pub generation: u64,
    /// Hits, in the order the daemon streamed them (which is rank order).
    pub hits: Vec<Hit>,
    /// Cursor within [`SearchPane::hits`].
    pub cursor: usize,
    /// Which half has the keyboard.
    pub focus: SearchFocus,
    /// Whether the stream for [`SearchPane::generation`] has ended.
    pub complete: bool,
    /// Why the last search failed, if it did.
    pub error: Option<String>,
    /// Whether the why-panel is open (`x`).
    pub explain: bool,
    /// The hit an `Explain` is outstanding for. What makes a late one
    /// recognisable as stale once the cursor has moved on.
    pub explaining: Option<i64>,
    /// The hit whose explanation could not be produced. A latch, not a
    /// nicety: `Explain` re-runs the whole retrieval pipeline, and without
    /// somewhere to remember the failure the why-panel would ask again on
    /// every message that arrives.
    pub explain_failed: Option<i64>,
    /// The explanation for the highlighted hit, once it has arrived.
    pub explanation: Option<Explanation>,
}

impl SearchPane {
    /// Whether keys are text right now.
    #[must_use]
    pub fn typing(&self) -> bool {
        self.focus == SearchFocus::Query
    }

    /// The highlighted hit.
    #[must_use]
    pub fn hit(&self) -> Option<&Hit> {
        self.hits.get(self.cursor)
    }

    /// Append a hit from the stream, if it belongs to the current query.
    pub fn push_hit(&mut self, generation: u64, hit: Hit) {
        if generation != self.generation || self.hits.len() >= MAX_ROWS {
            return;
        }
        self.hits.push(hit);
    }

    /// Start a fresh query: new generation, no results, nothing explained.
    ///
    /// `complete` is left for the caller to set, because whether a query is
    /// running is the caller's fact: an empty box issues nothing and is
    /// finished the moment it is cleared.
    pub fn restart(&mut self, generation: u64) {
        self.generation = generation;
        self.hits.clear();
        self.cursor = 0;
        self.complete = false;
        self.error = None;
        self.explaining = None;
        self.explain_failed = None;
        self.explanation = None;
    }
}

// ---------------------------------------------------------------------------
// finder
// ---------------------------------------------------------------------------

/// What sort of thing a finder row is. The wire's `ItemKind`, minus the
/// proto's `UNSPECIFIED`, which is a wire artefact rather than a kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FinderKind {
    /// A message. `ref_id` is a `messages.id`.
    Message,
    /// A folder. `ref_id` is a `mailboxes.id`.
    Mailbox,
    /// A correspondent. `secondary` is their address.
    Contact,
    /// A saved search. `secondary` is its query text.
    SavedSearch,
    /// A tag. `primary_text` is its name.
    Tag,
    /// A command. `secondary` is an [`Action`] id.
    Command,
    /// A kind this build does not know. Rendered, never acted on — a newer
    /// daemon adding a kind must not make an old client do something
    /// arbitrary with it.
    Unknown,
}

impl FinderKind {
    /// The one-word label a row is tagged with.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Message => "msg",
            Self::Mailbox => "folder",
            Self::Contact => "person",
            Self::SavedSearch => "search",
            Self::Tag => "tag",
            Self::Command => "cmd",
            Self::Unknown => "?",
        }
    }
}

/// One finder row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinderItem {
    /// What it is.
    pub kind: FinderKind,
    /// The row id in whichever source table [`FinderItem::kind`] names.
    pub ref_id: i64,
    /// The line a row renders. Original text, never folded.
    pub primary: String,
    /// The dimmer second line.
    pub secondary: String,
    /// **Char** offsets into [`FinderItem::primary`] — never bytes. See the
    /// module docs.
    pub positions: Vec<usize>,
    /// The mailbox, for a message; 0 otherwise.
    pub mailbox_id: i64,
}

/// `Ctrl-P` — jump to anything.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FinderPane {
    /// The prompt text, sigil included, passed to the daemon verbatim so
    /// `rmail_core::finder::Query::parse` stays the only implementation of
    /// the `>#@/:` grammar.
    pub query: String,
    /// The generation the outstanding `Find` was issued under.
    pub generation: u64,
    /// The latest snapshot. Replaced per batch, never appended to.
    pub items: Vec<FinderItem>,
    /// Cursor within [`FinderPane::items`].
    pub cursor: usize,
    /// Whether the scan has ended.
    pub complete: bool,
    /// Whether it ended because a newer `Find` superseded it.
    pub superseded: bool,
    /// Index entries walked so far.
    pub scanned: u64,
    /// Why the last find failed, if it did.
    pub error: Option<String>,
}

impl FinderPane {
    /// The highlighted item.
    #[must_use]
    pub fn item(&self) -> Option<&FinderItem> {
        self.items.get(self.cursor)
    }

    /// Replace the visible list with a snapshot, if it belongs to the current
    /// query.
    ///
    /// Replace, not extend: see the module docs on why a bounded top-K heap
    /// cannot send deltas. The cursor is kept where it was and clamped, so
    /// progressive fill-in does not move the selection under the user's
    /// finger — but a cursor past the new end has to come back inside it.
    pub fn apply_batch(&mut self, generation: u64, mut items: Vec<FinderItem>, complete: bool) {
        if generation != self.generation {
            return;
        }
        items.truncate(MAX_ROWS);
        self.items = items;
        self.complete = complete;
        self.cursor = self.cursor.min(self.items.len().saturating_sub(1));
    }

    /// Start a fresh query.
    pub fn restart(&mut self, generation: u64) {
        self.generation = generation;
        self.complete = false;
        self.superseded = false;
        self.scanned = 0;
        self.error = None;
        // Deliberately *not* clearing `items`: the previous query's rows stay
        // on screen until the first batch of the new one lands, which is what
        // makes typing feel continuous instead of strobing to empty on every
        // keystroke. The generation check above is what keeps them from being
        // mistaken for the new query's answer.
    }
}

// ---------------------------------------------------------------------------
// the command line
// ---------------------------------------------------------------------------

/// One verb the command line offers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandEntry {
    /// The verb's path as it is typed, space separated.
    pub verb: String,
    /// The action it runs with no arguments, if it has one. `None` for a verb
    /// the grammar is the only way to reach.
    pub action: Option<Action>,
    /// How it is bound in the message list, for the right-hand column.
    pub chords: String,
    /// One line of help.
    pub describe: String,
}

/// Where an `<up>`/`<down>` walk through the history currently stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Browse {
    /// What was typed before the walk began, and the prefix it filters on.
    /// Restored when the walk comes back past its start.
    pub seed: String,
    /// Index into `History::matching(seed)` — 0 is the most recent match.
    pub at: usize,
}

/// `:` — the command line.
///
/// It replaces task 85's `PalettePane`, and `Action::PaletteOpen` still opens
/// it: the palette's job — "run a command by name, ranked, without knowing
/// its key" — is this pane's ranked match list, and the id stays because
/// renaming one breaks a `keys.toml` somebody has already written.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandPane {
    /// What has been typed, without the leading `:`.
    pub input: String,
    /// The ranked verbs matching it, recomputed on every keystroke.
    pub matches: Vec<CommandEntry>,
    /// Why the last `<enter>` did not run anything. Shown in the line itself
    /// and cleared by the next edit, so a parse error leaves the overlay open
    /// with the offending text still there to fix.
    pub error: Option<String>,
    /// The history walk in progress, if any.
    pub browse: Option<Browse>,
}

impl CommandPane {
    /// The verb `<enter>` falls back to when the typed line names none — the
    /// best-ranked match, which is the row the list draws first.
    #[must_use]
    pub fn best(&self) -> Option<&CommandEntry> {
        self.matches.first()
    }

    /// Whether the typed line names no verb of its own, so `<enter>` would
    /// run [`CommandPane::best`] instead.
    ///
    /// Asked of the registry rather than tracked as a field, because the
    /// answer is a function of the input and a field would be a second copy
    /// of it — free to be stale for exactly one frame after a keystroke,
    /// which is the frame somebody presses Enter in.
    ///
    /// An *empty* line is not a live fallback even though it also names no
    /// verb: the list is then every verb there is, in path order, and
    /// pointing at whichever sorts first would be pointing at something
    /// `<enter>` does not do — a bare `<enter>` asks for a verb.
    #[must_use]
    pub fn fallback_is_live(&self) -> bool {
        !self.matches.is_empty()
            && matches!(
                command::parse(self.input.trim()),
                Err(command::CommandError::UnknownVerb { .. })
            )
    }
}

/// Rank every verb in the registry against `input`.
///
/// This is task 85's `palette_matches` generalized: the vocabulary widened
/// from [`Action::ALL`] to `rmail_core::command`'s registry — which is every
/// action *plus* the verbs no chord reaches — and the ranking is unchanged.
/// Resolution stays local and total, so "which command did they mean" is
/// still answered with the daemon unreachable, and an empty input still lists
/// everything, which is what makes this a discovery surface rather than only
/// a shortcut.
///
/// Four tiers, so an exact-ish path beats a coincidental word in a help
/// string: a prefix of the path, then a substring of it, then a subsequence
/// of it, then a substring of the description. Ties break on the path so the
/// order is stable across frames — a list that reshuffles between keystrokes
/// is unusable.
#[must_use]
pub fn command_matches(input: &str, keymap: &Keymap) -> Vec<CommandEntry> {
    let needle = verb_words(input);
    let mut scored: Vec<(u8, String, &'static Verb)> = Vec::new();
    for verb in command::children_of(&[]) {
        let path = verb.canonical();
        let describe = verb.describe().to_lowercase();
        let tier = if needle.is_empty() {
            3
        } else if path.starts_with(&needle) {
            0
        } else if path.contains(&needle) {
            1
        } else if is_subsequence(&needle, &path) {
            2
        } else if describe.contains(&needle) {
            3
        } else {
            continue;
        };
        scored.push((tier, path, verb));
    }
    scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    scored
        .into_iter()
        .map(|(_, path, verb)| CommandEntry {
            chords: verb
                .action
                .map(|action| {
                    keymap
                        .chords_for(Mode::Normal, action)
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(" / ")
                })
                .unwrap_or_default(),
            action: verb.action,
            describe: verb.describe(),
            verb: path,
        })
        .collect()
}

/// The part of a typed line that names a verb, lowercased and space
/// separated: no range, no trailing bang, no flags, and dots read as spaces.
///
/// Ranking happens on every keystroke, including on lines the parser would
/// reject outright, so this is deliberately forgiving where
/// `rmail_core::command::parse` is strict — its job is to decide what the
/// typist is reaching for, not whether they have typed it correctly yet.
#[must_use]
pub fn verb_words(input: &str) -> String {
    let rest = input.trim().trim_start_matches(':').trim_start();
    let rest = rest
        .strip_prefix("'<,'>")
        .or_else(|| rest.strip_prefix('%'))
        .unwrap_or(rest);
    let rest = rest
        .trim_start_matches(|c: char| c.is_ascii_digit())
        .trim_start();
    rest.split_whitespace()
        .take_while(|word| !word.starts_with('-'))
        .flat_map(|word| word.trim_end_matches('!').split('.'))
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Whether every character of `needle` appears in `haystack`, in order.
fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut chars = haystack.chars();
    needle
        .chars()
        .all(|wanted| chars.any(|candidate| candidate == wanted))
}

// ---------------------------------------------------------------------------
// operator autocomplete
// ---------------------------------------------------------------------------

/// The registered operators whose names start with the word being typed at
/// the end of `query`, or nothing when that word is already complete.
///
/// The names come from `rmail_core::query::parse::OPERATORS`, which the
/// parser's own test walks — so the menu can never offer an operator the
/// grammar does not have.
#[must_use]
pub fn operator_candidates(query: &str) -> Vec<(&'static str, &'static str)> {
    let Some(word) = trailing_word(query) else {
        return Vec::new();
    };
    // A word that already carries its colon has chosen its operator; what
    // comes after it is a value, and completing there would append an
    // operator name into the middle of one.
    if word.is_empty() || word.contains(':') {
        return Vec::new();
    }
    let lower = word.to_lowercase();
    OPERATORS
        .iter()
        .filter(|(name, _)| name.starts_with(&lower))
        .copied()
        .collect()
}

/// `query` with the operator being typed completed, or `None` when there is
/// nothing unambiguous to add.
///
/// Completes to the candidates' longest common prefix, and appends the `:`
/// only when exactly one operator remains — `t` is `to:` *or* `tag:`, and
/// silently choosing one of them would be a keystroke that did the wrong
/// thing rather than one that did nothing.
#[must_use]
pub fn complete_operator(query: &str) -> Option<String> {
    let candidates = operator_candidates(query);
    let (first, _) = *candidates.first()?;
    let word = trailing_word(query)?;
    let completed = if candidates.len() == 1 {
        format!("{first}:")
    } else {
        common_prefix(&candidates)
    };
    if completed.len() <= word.len() {
        return None;
    }
    let head = query.get(..query.len() - word.len())?;
    Some(format!("{head}{completed}"))
}

/// The whitespace-delimited word at the end of `query`.
fn trailing_word(query: &str) -> Option<&str> {
    let word = query.rsplit(char::is_whitespace).next()?;
    // A leading `-` is the grammar's negation and belongs to the word, not to
    // the operator name, so it is stripped for matching and restored by
    // `complete_operator`'s slice arithmetic... which it cannot do if the
    // hyphen is inside `word`. Simplest correct thing: refuse to complete a
    // negated term rather than complete it wrongly.
    (!word.starts_with('-')).then_some(word)
}

fn common_prefix(candidates: &[(&str, &str)]) -> String {
    let Some((first, _)) = candidates.first() else {
        return String::new();
    };
    let mut length = first.len();
    for (name, _) in candidates {
        length = length.min(
            name.chars()
                .zip(first.chars())
                .take_while(|(a, b)| a == b)
                .count(),
        );
    }
    first.chars().take(length).collect()
}

// ---------------------------------------------------------------------------
// ask pane
// ---------------------------------------------------------------------------

/// Where an ask is in its life.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum AskPhase {
    /// Typing the question.
    #[default]
    Asking,
    /// The answer is streaming.
    Streaming,
    /// The stream ended.
    Done,
}

/// One source the answer pointed at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Citation {
    /// The bracketed marker the prose used.
    pub label: u32,
    /// `messages.id`.
    pub message_id: i64,
    /// Subject, as stored.
    pub subject: String,
    /// The sender's addr-spec.
    pub from_addr: String,
    /// The folder it is in.
    pub mailbox: String,
    /// A verbatim excerpt of what was actually in the prompt for this source.
    pub quote: String,
}

/// `A` — ask the mailbox a question.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AskPane {
    /// The question.
    pub question: String,
    /// The generation the outstanding `AskMailbox` was issued under.
    pub generation: u64,
    /// Where it is.
    pub phase: AskPhase,
    /// The retrieval trace, pre-formatted, shown while the answer streams.
    pub trace: Option<String>,
    /// The prose so far.
    pub answer: String,
    /// The sources, which arrive *after* the prose — an inline `[n]` marker
    /// is only resolvable once the whole answer has been seen.
    pub citations: Vec<Citation>,
    /// Cursor within [`AskPane::citations`].
    pub cursor: usize,
    /// The daemon's verdict on whether the answer cited anything real. Not
    /// the model's claim about itself, and rendered as the daemon's.
    pub grounded: bool,
    /// Why not, when it is not.
    pub refusal: String,
    /// Why the stream failed, if it did.
    pub error: Option<String>,
}

impl AskPane {
    /// Whether keys are text right now.
    #[must_use]
    pub fn typing(&self) -> bool {
        self.phase == AskPhase::Asking
    }

    /// The highlighted citation.
    #[must_use]
    pub fn citation(&self) -> Option<&Citation> {
        self.citations.get(self.cursor)
    }

    /// Append a token, bounded.
    pub fn push_token(&mut self, generation: u64, token: &str) {
        if generation != self.generation {
            return;
        }
        let room = MAX_ANSWER_CHARS.saturating_sub(self.answer.chars().count());
        if room == 0 {
            return;
        }
        self.answer.extend(token.chars().take(room));
        if self.answer.chars().count() >= MAX_ANSWER_CHARS {
            self.answer.push_str("\n[answer truncated]");
        }
    }
}

// ---------------------------------------------------------------------------
// outbox
// ---------------------------------------------------------------------------

/// One outbox entry, as the pseudo-folder draws it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxRow {
    /// `outbox.id`.
    pub id: i64,
    /// Recipients, joined.
    pub to: String,
    /// Subject.
    pub subject: String,
    /// `scheduled`, `sending`, `sent`, `failed`, `canceled`.
    pub state: String,
    /// When it goes out, unix seconds.
    pub send_at: i64,
    /// Until when an undo is offered, unix seconds.
    pub undo_deadline: Option<i64>,
    /// The last failure, verbatim.
    pub last_error: Option<String>,
}

/// `O` — the outbox pseudo-folder.
///
/// A pseudo-folder rather than a mailbox: nothing in `mailboxes` holds these
/// rows, they live in `outbox` and are reached through
/// `SendSchedulerService.ListOutbox`. Presenting them as a folder is a
/// presentation choice this pane makes, not a claim that the server has one.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutboxPane {
    /// The entries, as listed.
    pub rows: Vec<OutboxRow>,
    /// Cursor within [`OutboxPane::rows`].
    pub cursor: usize,
    /// Whether a listing is outstanding.
    pub loading: bool,
    /// Why the listing failed, if it did.
    pub error: Option<String>,
}

impl OutboxPane {
    /// The highlighted entry.
    #[must_use]
    pub fn row(&self) -> Option<&OutboxRow> {
        self.rows.get(self.cursor)
    }
}

/// The undo-send countdown.
///
/// prd.md's undo window is a *server* fact — `outbox.undo_deadline`, frozen
/// when the message was scheduled — so this holds the absolute deadline and
/// derives the number on screen from a clock reading that arrives as a
/// message. `update` has no clock (it is pure, by type), which is exactly why
/// the countdown cannot be computed inside it from `Instant::now`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoToast {
    /// Which entry `u` would cancel.
    pub outbox_id: i64,
    /// Who it is going to.
    pub to: String,
    /// The absolute deadline, unix seconds.
    pub deadline: i64,
    /// Seconds left as of the last tick. Zero means the window has closed and
    /// the toast is about to go.
    pub remaining: i64,
}

/// One line at the bottom of the message list: an undo countdown, a
/// finished background job, or a priority alert.
///
/// [`super::model::Model::toasts`] is a queue rather than a single slot
/// because more than one can be true at once — a reindex can finish while a
/// send is still undoable — but the row itself does not grow: `view::render`
/// draws one [`super::model::Model::shown_toast`] plus a `+N` badge for
/// whatever else is queued, which is what keeps this a one-line reflow no
/// matter how many are waiting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Toast {
    /// A scheduled send still inside its undo window.
    Undo(UndoToast),
    /// A background job finished — a reindex, an export, a rebuild.
    ///
    /// `#[allow(dead_code)]`: no task wires a real completion event into the
    /// TUI yet (that needs `WatchEvents`, task 94's job) — this variant and
    /// [`Toast::Priority`] are exercised by `tui::model::tests` and
    /// `tui::view::tests` today, and by task 94's own production code once
    /// it lands. Declaring the shape now, against the queue and the render
    /// side both built in task 93, is what lets 94 add one line rather than
    /// a second toast type.
    #[allow(dead_code)]
    Completion {
        /// What to show on the row.
        text: String,
    },
    /// `NotificationService::StreamAlerts` surfaced something ranked to
    /// interrupt. Same `#[allow(dead_code)]` reasoning as
    /// [`Toast::Completion`]; task 98's `:notify` is the real source.
    #[allow(dead_code)]
    Priority {
        /// What to show on the row.
        text: String,
    },
}

// ---------------------------------------------------------------------------
// AI panel and quick actions
// ---------------------------------------------------------------------------

/// A message's cached AI analysis, as the collapsible panel draws it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AiSummary {
    /// Which message.
    pub message_id: i64,
    /// `ok`, `pending` or `not queued` — the panel says which, because "no
    /// summary yet" and "AI is off for this folder" look identical otherwise.
    pub status: String,
    /// The triage one-liner.
    pub tl_dr: Option<String>,
    /// The deep pass's summary.
    pub summary: Option<String>,
    /// Its key points.
    pub key_points: Vec<String>,
    /// Its to-dos, flattened to one line each.
    pub todos: Vec<String>,
    /// Tags the triage pass suggested.
    pub tags: Vec<String>,
    /// The triage priority.
    pub priority: Option<String>,
    /// Whether triage thought a reply was needed.
    pub needs_reply: Option<bool>,
    /// A suggested reply, when one was asked for.
    pub suggested_reply: Option<String>,
}

/// One entry of the `.` menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuickAction {
    /// Open the AI panel on this message's cached analysis. Never calls the
    /// model — `GetSummary` reads what the triage/deep passes already wrote.
    Summarize,
    /// Open the ask pane with this message's subject already typed.
    Ask,
    /// Ask the daemon for a reply suggestion. This one *does* cost a model
    /// call, which is why it is behind a menu rather than on a bare key.
    SuggestReply,
}

impl QuickAction {
    /// The menu, in the order it is drawn.
    pub const ALL: &'static [(QuickAction, &'static str)] = &[
        (QuickAction::Summarize, "summarize (cached — no model call)"),
        (QuickAction::Ask, "ask about this message"),
        (
            QuickAction::SuggestReply,
            "suggest a reply (calls the model)",
        ),
    ];
}

/// `.` — AI actions for the message under the cursor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QuickPane {
    /// The message the menu acts on, captured when it opened. Never
    /// re-derived on accept: the list is live, and a `Changed` reload can
    /// move the cursor while the menu is up — the folder-picker overlay
    /// captures its targets for the same reason.
    pub message_id: i64,
    /// That message's subject, for the title and for pre-filling the ask
    /// pane.
    pub subject: String,
    /// Cursor within [`QuickAction::ALL`].
    pub cursor: usize,
}

impl QuickPane {
    /// The highlighted action.
    #[must_use]
    pub fn action(&self) -> Option<QuickAction> {
        QuickAction::ALL.get(self.cursor).map(|(action, _)| *action)
    }
}
