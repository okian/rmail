//! Task 91's WhichKey band, on the `rmail-core` side: what one more key press
//! after a half-typed prefix can do, and which bindings a chain has made
//! impossible to type at all.
//!
//! # Why it belongs to the engine rather than to the band
//!
//! Both answers are functions of the bindings in force and of nothing else —
//! no terminal, no model, no frame. Deriving them here means the band renders
//! a query rather than a table, which is the property that matters: a
//! hand-maintained group table starts lying the moment a `keys.toml` rebinds
//! anything, and this crate's own history has that failure in it (task 83
//! hand-maintained the key reference).
//!
//! It also means both are testable against a constructed [`Keymap`] rather
//! than against a rendered frame, and reusable by the two surfaces that are
//! not the band: task 105's startup lint and `:keys check` read
//! [`Keymap::shadowed_across_layers`], and the manual's generated key
//! reference is the same shape of query.
//!
//! # Why no timer
//!
//! vim waits `timeoutlen` before showing anything, because in vim an exact
//! match that is also a prefix is ambiguous. It is not ambiguous here: rule 1
//! of this crate's key engine (see the parent module) fires an exact match
//! immediately, so a prefix is *pending* only after [`Keymap::resolve`] has
//! already looked it up and found nothing. Nothing pending could have fired on
//! its own, so there is nothing for a delay to disambiguate and the band draws
//! at once. `tests::a_pending_prefix_is_always_one_that_resolved_to_nothing`
//! is the proof rather than the claim.

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};

use super::{Action, Chord, Key, Keymap, Mode};

/// What one more key press after a prefix can do (task 91's WhichKey band).
///
/// Derived from the bindings in force, never from a table: a band built from a
/// hand-maintained list would start lying the moment a `keys.toml` rebound
/// anything, which is the failure the generated key reference exists to rule
/// out.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Continuation {
    /// The key that extends the prefix.
    pub key: Key,
    /// What the prefix plus this key means.
    pub leads: Leads,
    /// Bindings under the prefix plus this key that can never be typed, each
    /// with the action it would have run.
    ///
    /// Non-empty only for [`Leads::Run`], and that is the whole point of the
    /// field: [`Keymap::resolve`] runs a chord the moment `lookup` finds it
    /// and never waits for a longer one, so a longer binding sitting under a
    /// complete chord is dead. [`Keymap::bind`] refuses that *within* a layer;
    /// across a chain it cannot see the other layers, so `keys.toml` can
    /// produce it and a band is where somebody finds out.
    ///
    /// The action travels with the chord because a band has to be able to name
    /// what is broken: `lookup` on a dead chord answers the *killer's* action,
    /// not its own, so the only place the dead binding's own meaning is still
    /// known is here.
    pub buried: Vec<(Chord, Action)>,
}

/// What a [`Continuation`] leads to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Leads {
    /// It completes a binding: this action runs.
    Run(Action),
    /// It is a prefix of longer bindings and completes none of its own.
    Group {
        /// What to call the group, derived from its members rather than
        /// declared: the longest common dot-prefix of their [`Action::id`]s
        /// (`ai` for `ai.panel` and `ai.quick`). Empty when the members share
        /// no leading segment at all, which a renderer shows as a count —
        /// inventing a name for an arbitrary collection would be the
        /// hand-maintained group table this derivation exists to avoid.
        label: String,
        /// How many bindings are under it.
        members: usize,
    },
}

impl Keymap {
    /// What one more key press after `prefix` can do in `mode`, in key order.
    ///
    /// Answers the band's question and only that question: "given these keys
    /// are pending, what does the next one do". Whether `prefix` itself can be
    /// typed is a different question with a different answer
    /// ([`Keymap::shadowed_across_layers`]) — and in the band it never arises,
    /// because a prefix is only pending when [`Keymap::resolve`] has already
    /// found nothing bound along it.
    ///
    /// An empty `prefix` lists every first key in the mode, which is what a
    /// full key reference wants; the band never asks for it, since it draws
    /// only while something is half-typed.
    #[must_use]
    pub fn continuations(&self, mode: Mode, prefix: &[Key]) -> Vec<Continuation> {
        // Keyed by the next key, in key order, so the band's entries are
        // stable across frames — a list that reshuffles between keystrokes is
        // unusable, the same reason the command completer sorts.
        let mut by_key: BTreeMap<Key, Vec<(Chord, Action)>> = BTreeMap::new();
        for layer in mode.chain() {
            for (chord, action) in self.layer(*layer) {
                let keys = chord.keys();
                if keys.len() <= prefix.len() || !keys.starts_with(prefix) {
                    continue;
                }
                let Some(next) = keys.get(prefix.len()).copied() else {
                    continue;
                };
                let entry = by_key.entry(next).or_default();
                // A chord bound in two layers of one chain is one binding as
                // far as the keyboard is concerned; the nearest layer's action
                // is what `lookup` answers, so the farther one is not a second
                // member of a group.
                if entry.iter().any(|(seen, _)| seen == chord) {
                    continue;
                }
                entry.push((chord.clone(), action));
            }
        }

        by_key
            .into_iter()
            .filter_map(|(key, chords)| {
                let mut keys = prefix.to_vec();
                keys.push(key);
                // Only fails past `MAX_CHORD_KEYS`, and every chord here came
                // out of the map, so one of them is at least this long.
                let here = Chord::new(keys).ok()?;
                Some(match self.lookup(mode, &here) {
                    Some(action) => Continuation {
                        key,
                        leads: Leads::Run(action),
                        buried: chords
                            .iter()
                            .filter(|(chord, _)| chord.keys().len() > here.keys().len())
                            .cloned()
                            .collect(),
                    },
                    // Nothing in the chain binds `here` exactly, so every
                    // chord collected under this key is strictly longer than
                    // it — all of them are members, and none is buried.
                    None => Continuation {
                        key,
                        leads: Leads::Group {
                            label: common_id_prefix(chords.iter().map(|(_, action)| *action)),
                            members: chords.len(),
                        },
                        buried: Vec::new(),
                    },
                })
            })
            .collect()
    }

    /// Every binding this map makes unreachable by cross-layer shadowing, as
    /// `(mode, dead, killer)`.
    ///
    /// In `mode`'s chain, `killer` is bound and `dead` is strictly longer and
    /// starts with it — so [`Keymap::resolve`] runs `killer` the moment it is
    /// typed and `dead` can never be delivered.
    ///
    /// [`Keymap::bind`] already refuses this *within* one layer, and cannot
    /// refuse it across a chain without refusing legitimate edits: `viewer`
    /// binding `g` is a perfectly reasonable thing to want, and the fact that
    /// it silently kills `Normal`'s `gg` for the viewer is a consequence of
    /// the chain rather than of that one edit. So it is reported rather than
    /// prevented — a lint, which is what task 105 wires it up as.
    ///
    /// [`Mode::Global`] is not reported on its own: it is never the active
    /// mode, so a shadow inside it shows up under every mode that inherits it,
    /// which is where somebody would actually meet it.
    ///
    /// One entry per dead binding, naming the killer that *actually fires* —
    /// the shortest bound prefix, since [`Keymap::resolve`] runs the first one
    /// it reaches. With `a`, `ab` and `abc` all bound, `abc` is reported once
    /// against `a` rather than twice against both; the second pairing is true
    /// and tells nobody anything they can act on.
    ///
    /// Same-layer conflicts are caught here too, not only by [`Keymap::bind`]:
    /// [`Keymap::defaults`] installs through `insert` rather than `bind`, so
    /// the built-in table is not covered by that check and is covered by this
    /// one.
    #[must_use]
    pub fn shadowed_across_layers(&self) -> Vec<(Mode, Chord, Chord)> {
        let mut found = Vec::new();
        for mode in Mode::CONFIGURABLE {
            let mut bound: BTreeSet<Chord> = BTreeSet::new();
            for layer in mode.chain() {
                for (chord, _) in self.layer(*layer) {
                    bound.insert(chord.clone());
                }
            }
            for dead in &bound {
                // Ascending order, so the first hit is the shortest bound
                // prefix — the one the engine fires on.
                let killer = bound
                    .iter()
                    .find(|killer| *killer != dead && dead.starts_with(killer));
                if let Some(killer) = killer {
                    found.push((*mode, dead.clone(), killer.clone()));
                }
            }
        }
        found
    }
}

/// The longest common dot-prefix of `actions`' ids, joined with `.`.
///
/// `ai` for `ai.panel` and `ai.quick`; `message` for `message.archive` and
/// `message.delete`; empty for `help` and `search`, which share no leading
/// segment. Segments rather than characters, so `search` and `search.explain`
/// answer `search` while `manual` and `menu.accept` answer nothing at all
/// instead of the meaningless `m`.
fn common_id_prefix(actions: impl Iterator<Item = Action>) -> String {
    let mut common: Option<Vec<&str>> = None;
    for action in actions {
        let segments: Vec<&str> = action.id().split('.').collect();
        common = Some(match common {
            None => segments,
            Some(shared) => shared
                .iter()
                .zip(segments.iter())
                .take_while(|(a, b)| a == b)
                .map(|(a, _)| *a)
                .collect(),
        });
    }
    common.unwrap_or_default().join(".")
}
