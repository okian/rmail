//! Task 102's key reference: `?`, redesigned to be mode-aware, scrollable,
//! grouped and filterable, with a row that does something instead of a dead
//! end.
//!
//! # Data, not drawing
//!
//! [`rows`] is a pure function of a [`Mode`], a filter and a `&Keymap`,
//! returning [`Row`]s — the same split `tui::whichkey` and `tui::report`
//! keep, and for the same reason: the grouping and the filtering are then
//! testable without a terminal.
//!
//! # Why the rows are cached on the pane rather than computed per frame
//!
//! Every other overlay with a list cursor answers `Overlay::list_cursor`
//! from a field already on the pane (`pane.hits.len()`, `pane.rows.len()`),
//! because that method has only `&self` — no `&Keymap` to derive an answer
//! from. [`HelpPane::rows`] is this overlay's version of that: recomputed by
//! `refresh_help` whenever [`HelpPane::mode`] or [`HelpPane::filter`]
//! changes (mode cycled, a character typed), not on every frame.
//!
//! # Grouping
//!
//! Bucketed by the *first* dot-segment of each bound action's id — `cursor`
//! for `cursor.down`/`cursor.up`, `ai` for `ai.panel`/`ai.quick` — which is
//! coarser than [`rmail_core::keymap::common_id_prefix`] alone would derive
//! (that answers the longest *shared* prefix of a bag already known to
//! belong together; nothing here hands it one). The bucket key decides
//! membership; `common_id_prefix`, called once per bucket, decides the
//! label — reusing the exact function WhichKey's own grouping calls, over a
//! different input shape (every bound action in a mode, not one chord's
//! continuations). A bucket of one gets no header at all: naming a group of
//! one is the hand-written table this derivation exists to avoid.

#[cfg(test)]
mod tests;

use std::collections::BTreeMap;

use rmail_core::keymap::{common_id_prefix, Action, Keymap, Mode};

use super::overlays::is_subsequence;

/// The `?` overlay's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelpPane {
    /// Whose chain is on screen — captured from [`crate::tui::model::Model::mode`]
    /// when `?` is pressed, cycled by `<tab>`/`<c-i>` (forward) and `<c-o>`
    /// (back) through [`Mode::CONFIGURABLE`].
    pub mode: Mode,
    /// The typed filter. Empty shows every binding in [`HelpPane::mode`]'s
    /// chain.
    pub filter: String,
    /// Whether `/` is capturing keystrokes into [`HelpPane::filter`] right
    /// now — what makes this overlay's mode [`Mode::Prompt`] rather than
    /// [`Mode::Help`], the same way a typing search or ask pane does.
    pub editing: bool,
    /// The grouped, filtered rows to draw. See the module docs for why this
    /// is stored rather than derived per frame.
    pub rows: Vec<Row>,
    /// Index into [`HelpPane::rows`]' [`Row::Binding`] entries only — group
    /// headers are not selectable, so the cursor counts past them rather
    /// than needing every caller to know to skip them.
    pub cursor: usize,
}

impl HelpPane {
    /// Fresh state for `?` pressed while `mode` was current: every binding
    /// `mode`'s chain reaches, unfiltered, cursor at the top.
    #[must_use]
    pub fn new(mode: Mode, keymap: &Keymap) -> Self {
        Self {
            mode,
            filter: String::new(),
            editing: false,
            rows: rows(mode, "", keymap),
            cursor: 0,
        }
    }
}

/// One row the key reference draws.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Row {
    /// A derived group heading. Not selectable.
    Group(String),
    /// One bound action.
    Binding {
        /// What pressing it does.
        action: Action,
        /// How it is bound in this mode, joined with `" / "` when more than
        /// one chord reaches it.
        chords: String,
        /// One line of help.
        describe: &'static str,
    },
}

/// The grouped, filtered rows for `mode`'s chain, as `keymap` binds it.
///
/// An action absent from `mode`'s chain entirely is skipped rather than
/// listed as unbound: this overlay answers "what can I press right now",
/// and nothing can be pressed for it. The generated key reference in
/// `tui::manual` (task 103) is the comprehensive surface, bound and unbound
/// together, across every mode — a different question with a different
/// answer, on a different screen.
#[must_use]
pub fn rows(mode: Mode, filter: &str, keymap: &Keymap) -> Vec<Row> {
    let needle = filter.trim().to_lowercase();
    let mut buckets: BTreeMap<&'static str, Vec<(Action, String)>> = BTreeMap::new();
    for &action in Action::ALL {
        let chords = keymap.chords_for(mode, action);
        if chords.is_empty() {
            continue;
        }
        let joined = chords
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" / ");
        if !matches_filter(&needle, &joined, action.id(), action.describe()) {
            continue;
        }
        let key = action.id().split('.').next().unwrap_or(action.id());
        buckets.entry(key).or_default().push((action, joined));
    }

    let mut out = Vec::new();
    for (_, members) in buckets {
        if members.len() > 1 {
            let label = common_id_prefix(members.iter().map(|(action, _)| *action));
            if !label.is_empty() {
                out.push(Row::Group(label));
            }
        }
        for (action, chords) in members {
            out.push(Row::Binding {
                action,
                chords,
                describe: action.describe(),
            });
        }
    }
    out
}

/// Whether `chord`, `id` or `describe` match `needle`, empty always
/// matching.
///
/// A match test, not a rank. `overlays::command_matches` scores the command
/// line by which of four conditions a candidate meets — a prefix of the
/// primary text, then a substring, then a subsequence, then a substring of
/// the description — and *reorders* its results by the tier that admitted
/// them. This answers only "does it match", over the same four conditions
/// widened from that function's one primary field (a verb's path) to two
/// (what the row is bound to *and* what it is named, joined so a needle can
/// span both) — but as a yes/no test the first three collapse: a prefix is
/// a substring is a subsequence, so the `starts_with`/`contains` checks
/// would never admit a row `is_subsequence` alone would not. What is left
/// is `is_subsequence(needle, primary) || describe.contains(needle)`; see
/// task 102's tasks.md note for why real tiering — reordering a bucket's
/// rows by match quality — was left out rather than shipped
/// half-considered.
fn matches_filter(needle: &str, chord: &str, id: &str, describe: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let primary = format!("{} {chord}", id.replace('.', " ")).to_lowercase();
    is_subsequence(needle, &primary) || describe.to_lowercase().contains(needle)
}

/// How many selectable rows there are — [`Overlay::list_cursor`]'s answer
/// for this overlay.
///
/// [`Overlay::list_cursor`]: super::model::Overlay::list_cursor
#[must_use]
pub fn binding_count(pane: &HelpPane) -> usize {
    pane.rows
        .iter()
        .filter(|row| matches!(row, Row::Binding { .. }))
        .count()
}

/// The action the cursor is on, if [`HelpPane::rows`] has one at
/// [`HelpPane::cursor`].
#[must_use]
pub fn selected(pane: &HelpPane) -> Option<Action> {
    pane.rows
        .iter()
        .filter_map(|row| match row {
            Row::Binding { action, .. } => Some(*action),
            Row::Group(_) => None,
        })
        .nth(pane.cursor)
}
