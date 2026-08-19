//! Task 91's WhichKey band: the strip along the bottom that says what the
//! *next* key can do, whenever something is half-typed.
//!
//! # Why there is no delay
//!
//! nvim's which-key waits, because in vim an exact match that is also a prefix
//! is genuinely ambiguous — pressing `g` might mean `g` or might be the start
//! of `gg`, and only a timer can tell. That ambiguity does not exist here:
//! rule 1 of `rmail_core::keymap` fires an exact match immediately, so a
//! prefix is *pending* only after `Keymap::resolve` already looked it up and
//! found nothing bound. Nothing half-typed could have fired on its own, so
//! there is nothing for a delay to disambiguate and the band draws at once.
//! `rmail_core::keymap::continuations::tests::a_pending_prefix_is_always_one_that_resolved_to_nothing`
//! is that argument as a test rather than as a paragraph.
//!
//! A delay would also cost the thing the band is for. The reason to show it is
//! that somebody has forgotten what comes next; a band that appears only after
//! they have already waited long enough to feel lost is help arriving after
//! the fact.
//!
//! # Data, not drawing
//!
//! [`band`] is a pure function of `&Model` returning [`Band`], and `tui::view`
//! maps that onto styles — the same split `tui::manual`'s `Ink` keeps, and for
//! the same reason: every claim the band makes is then testable without a
//! terminal, including the ones about a keymap no default install has.
//!
//! # Two sources, one strip
//!
//! A half-typed chord and a half-typed `:` line are the same question asked of
//! different vocabularies, so they share one renderer. The chord side reads
//! `Keymap::continuations`; the command side reads `rmail_core::command::complete`,
//! which task 88 wrote for exactly this. Group labels on the chord side come
//! from the longest common dot-prefix of the member action ids — derived, never
//! a table, because a table would start lying the first time somebody rebound
//! something.

#[cfg(test)]
mod tests;

use rmail_core::command;
use rmail_core::keymap::{Chord, Continuation, Key, Keymap, Leads, Mode};

use super::model::{Model, Overlay};

/// The most entries the band carries.
///
/// The renderer truncates to the terminal anyway; this bounds the *data*, so a
/// `keys.toml` with three hundred bindings under one prefix costs a bounded
/// `Vec` per frame rather than three hundred allocations nobody can read.
pub const MAX_ENTRIES: usize = 32;

/// The reserved ways out, pinned into every band.
///
/// Spelled here as the chords they are rather than looked up by action,
/// because that is the direction the guarantee runs: `Chord::is_reserved`
/// refuses to bind anything starting with either, so these two keys always
/// mean what they mean, and the band promising them is a promise the engine
/// keeps. Their *labels* are still derived — whatever the map has them bound
/// to names itself.
const PINNED: [Key; 2] = [Key::Esc, Key::CTRL_C];

/// One entry the band draws.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// What to press, as it would be written in `keys.toml`.
    pub keys: String,
    /// What pressing it does. Empty for a command-line candidate, where the
    /// text *is* the answer and the overlay's own ranked list already carries
    /// descriptions.
    pub label: String,
    /// How it reads.
    pub kind: Kind,
}

/// What an [`Entry`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Pressing it runs something.
    Run,
    /// Pressing it opens more bindings — or, on the command line, settles a
    /// segment that has children of its own.
    Group,
    /// A binding this layering has made impossible to type. Drawn struck
    /// through: it exists in the map, and the keyboard can never deliver it.
    Dead,
    /// The way out, present in every band.
    Pinned,
}

/// What the band should draw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Band {
    /// What the band is about — the keys pending, or the `:` line so far.
    pub title: String,
    /// The entries, in order: the live ones, then any dead ones, then the
    /// pinned ways out.
    pub entries: Vec<Entry>,
    /// How many entries were dropped at [`MAX_ENTRIES`].
    pub dropped: usize,
    /// Present when some binding under the pending prefix can never be typed.
    pub warning: Option<String>,
}

/// What the band should draw right now, or `None` when it should not draw.
///
/// Two states put it on screen, and nothing else does:
///
/// - Keys are pending. **Keys**, not a count: `3` alone is a repeat waiting
///   for a command, and every binding in the mode is still available, so a
///   band listing all of them would say only "the keyboard works". `Pending`
///   keeps the two separate for exactly this.
/// - The command overlay is open, where the same question is asked of the verb
///   registry instead.
#[must_use]
pub fn band(model: &Model) -> Option<Band> {
    if let Some(Overlay::Command(pane)) = model.overlay.as_ref() {
        return Some(command_band(&model.keymap, model.mode(), &pane.input));
    }
    let pending = model.pending.keys();
    if pending.is_empty() {
        return None;
    }
    Some(chord_band(&model.keymap, model.mode(), pending))
}

/// The band for a half-typed chord.
fn chord_band(keymap: &Keymap, mode: Mode, pending: &[Key]) -> Band {
    let found = keymap.continuations(mode, pending);
    let mut live = Vec::new();
    let mut dead = Vec::new();
    for continuation in &found {
        live.push(entry_for(continuation));
        for (chord, action) in &continuation.buried {
            dead.push(Entry {
                keys: chord.to_string(),
                label: action.id().to_owned(),
                kind: Kind::Dead,
            });
        }
    }
    let warning = (!dead.is_empty()).then(|| {
        format!(
            "{} binding(s) here cannot be typed: a shorter chord in a nearer layer runs first",
            dead.len()
        )
    });
    let title: String = pending.iter().map(ToString::to_string).collect();
    let mut entries = live;
    entries.extend(dead);
    finish(title, entries, pinned(keymap, mode), warning)
}

/// One live entry for one continuation.
fn entry_for(continuation: &Continuation) -> Entry {
    match &continuation.leads {
        Leads::Run(action) => Entry {
            keys: continuation.key.to_string(),
            label: action.id().to_owned(),
            kind: Kind::Run,
        },
        Leads::Group { label, members } => Entry {
            keys: continuation.key.to_string(),
            // A count when the members share no leading segment: naming an
            // arbitrary collection is the hand-written group table this whole
            // derivation exists to avoid, and "4 commands" is at least true.
            label: if label.is_empty() {
                format!("{members} commands")
            } else {
                format!("{label}…")
            },
            kind: Kind::Group,
        },
    }
}

/// The band for a half-typed `:` line.
///
/// The same strip over a different vocabulary, and the ways out are pinned here
/// too — the command line's own hint row happens to say `Esc closes` as well,
/// but "the way out is in every band" is a promise worth being able to make
/// without exceptions, and the two are not the same row.
///
/// No dead entries and no warning: the verb registry is one namespace with no
/// layers, so nothing in it can shadow anything else — `command::tests`'
/// `no_two_real_verbs_share_the_same_path` and
/// `no_real_verb_that_takes_positionals_is_shadowed_by_a_longer_one` are what
/// keep that true.
fn command_band(keymap: &Keymap, mode: Mode, input: &str) -> Band {
    let entries = command::complete(input)
        .into_iter()
        .map(|candidate| Entry {
            keys: candidate.text,
            label: String::new(),
            kind: if candidate.has_more {
                Kind::Group
            } else {
                Kind::Run
            },
        })
        .collect();
    // The typed line, not a rendered prompt: `tui::view` draws the `:` itself,
    // and the band's title is what has been settled so far.
    finish(format!(":{input}"), entries, pinned(keymap, mode), None)
}

/// The pinned ways out, labelled by whatever they are bound to.
///
/// Skipped rather than invented when a map has neither — `Keymap::empty` is a
/// real state in the tests, and a band promising a key that does nothing is
/// worse than a band with one fewer entry.
fn pinned(keymap: &Keymap, mode: Mode) -> Vec<Entry> {
    PINNED
        .iter()
        .filter_map(|key| {
            let chord = Chord::new(vec![*key]).ok()?;
            let action = keymap.lookup(mode, &chord)?;
            Some(Entry {
                keys: chord.to_string(),
                label: action.id().to_owned(),
                kind: Kind::Pinned,
            })
        })
        .collect()
}

/// Assemble a band, capping the entries and never dropping a pinned one.
///
/// Pinned last and reserved: the whole point of pinning is that the way out is
/// there whatever else is, so the cap has to come out of the middle rather
/// than off the end.
fn finish(
    title: String,
    mut entries: Vec<Entry>,
    pinned: Vec<Entry>,
    warning: Option<String>,
) -> Band {
    let room = MAX_ENTRIES.saturating_sub(pinned.len());
    let dropped = entries.len().saturating_sub(room);
    entries.truncate(room);
    entries.extend(pinned);
    Band {
        title,
        entries,
        dropped,
        warning,
    }
}
