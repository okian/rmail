//! The in-memory index a keystroke is answered from, and the explicit bounds
//! on how big it may get.
//!
//! # The type that answers a keystroke owns no `Database`
//!
//! This is the design's load-bearing property, and it is enforced by the type
//! system rather than by discipline: [`FinderStore`] has no connection, no
//! pool, and no handle to one, so [`super::Finder::find`] *cannot* issue a
//! query per character typed even by accident. A finder that re-queries the
//! mailbox on every keystroke is the failure mode this whole subsystem
//! exists to avoid, and "we remembered not to" is not a guarantee. SQLite is
//! touched by exactly one place — [`super::index::FinderIndex`]'s drain,
//! on its own timer, on its own task.
//!
//! # Two explicit caps, because "in memory" is not a size
//!
//! prd.md budgets "< 25 MB for 100k messages" and, separately, "instant on
//! 100k+ messages". Neither is self-enforcing: a mailbox is however big it
//! is, and the store is loaded from it. So the store enforces both directly.
//!
//! - [`Limits::max_entries`] caps how many entries exist at all, which caps
//!   the scan.
//! - [`Limits::max_bytes`] caps the heap they occupy, measured
//!   ([`Entry::footprint`]) rather than estimated, and checked on every
//!   admission.
//!
//! Whichever binds first wins, and both degrade the same way: entries are
//! loaded newest-first (`idx_finder_kind_activity`), so what a full store
//! turns away is the oldest mail — the least likely thing a "jump to the
//! message I am thinking of" prompt is reaching for. [`FinderStore::rejected`]
//! counts what was turned away so `IndexStatus` can say so out loud rather
//! than quietly serving a partial index.
//!
//! # Per-kind vectors, and why a `Vec` rather than a map
//!
//! A scan is a linear walk with a `u64` mask test per element; nothing about
//! it wants hashing or ordering. But a *drain* has to find one entry by
//! `(kind, ref_id)` and replace or remove it, which a `Vec` cannot do in less
//! than a scan. So each kind carries a `Vec` for the scan and a `HashMap`
//! from `ref_id` to its slot for the drain, kept in step by
//! [`Kinded::upsert`]/[`Kinded::remove`] — removal is a `swap_remove` plus a
//! single index fix-up for whichever entry moved, so a delete is `O(1)` too.
//! Scan order is therefore not stable across drains, which is fine: nothing
//! downstream depends on it, because [`super::rank::Ranked`]'s ordering is
//! total down to the item id.

use std::collections::HashMap;
use std::mem;

use super::rank::Signals;
use super::{fold, ItemKind};

/// What separates the primary text from the secondary in [`Entry::text`] —
/// and, identically, in the folded blob. It has to be the same character in
/// both, because that is what lets a folded blob be *byte-identical* to the
/// display text for ASCII rows; see [`Entry::new`].
const SEPARATOR: char = ' ';

/// One findable thing, in the form a scan reads it.
///
/// # Layout is the budget
///
/// prd.md's memory model for this index is "~100 bytes + blob per entry →
/// 100k messages ≈ 15–25 MB", and the naive layout misses it by nearly a
/// factor of two: `primary_text`, `secondary` and the folded blob as three
/// separate `String`s costs three allocations, three 24-byte headers, and —
/// for the ASCII text that is the overwhelming majority of mail — two copies
/// of the same bytes, because folding ASCII is the identity.
///
/// So an entry holds **one** buffer, `primary_text` and `secondary` joined by
/// a single [`SEPARATOR`], with [`Entry::primary_text`] and
/// [`Entry::secondary`] as views into it, and keeps a folded copy *only when
/// folding actually changed something*. `café` and `会議` still get their own
/// blob; `Re: Q3 planning` does not. That is what makes the measured cost of
/// a realistic 100k-message index match prd.md's estimate rather than
/// exceed it, and `a_hundred_thousand_messages_fit_the_default_budget`
/// pins it.
///
/// # What is deliberately absent
///
/// No `Database`, no lazily-fetched field, no `Option` a renderer has to go
/// and fill in — everything a result row carries is resident, because the
/// alternative is a round trip inside the keystroke budget.
///
/// And no snippet. `finder_index.snippet` exists on disk (prd.md's schema),
/// but body text is the single most expensive thing that could be resident
/// and the least useful: a picker row renders a name and a subtitle, not a
/// paragraph, and a preview pane is a lookup against the item's own service.
/// 160 bytes of body per message is 16 MB of the 25 MB budget spent on
/// something nothing renders.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    /// `finder_index.item_id`.
    pub item_id: i64,
    /// The row id in the source table.
    pub ref_id: i64,
    /// 0 for kinds that are not per-account (contacts, commands). Not an
    /// `Option`, which would cost another 8 bytes per entry for a niche the
    /// value 0 already provides — no row in this schema has id 0.
    pub account_id: i64,
    /// 0 for everything but messages.
    pub mailbox_id: i64,
    /// Every character the blob contains, as a bitmask; the scan's prefilter.
    pub mask: u64,
    /// Unix seconds, or 0 for an item with no meaningful time.
    pub last_activity: i64,
    /// `primary_text`, then [`SEPARATOR`], then `secondary`. Never sliced by
    /// anything but the two accessors.
    text: Box<str>,
    /// The folded blob, or `None` when folding `text` changes nothing — the
    /// ASCII case, which is most of them.
    folded: Option<Box<str>>,
    /// Byte length of the primary part within `text`. A char boundary by
    /// construction.
    primary_bytes: u32,
    /// How many leading characters of the blob came from the primary text —
    /// the boundary [`super::score::Scorer::positions`] drops positions past.
    pub primary_folded_len: u32,
    /// 0..1.
    pub importance: f32,
    /// Interaction count, saturating.
    pub frequency: u32,
    pub kind: ItemKind,
    pub unread: bool,
}

impl Entry {
    /// Assemble an entry from its parts, folding once.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        item_id: i64,
        kind: ItemKind,
        ref_id: i64,
        account_id: i64,
        mailbox_id: i64,
        primary_text: &str,
        secondary: &str,
        signals: &Signals,
    ) -> Self {
        let mut text = String::with_capacity(primary_text.len() + 1 + secondary.len());
        text.push_str(primary_text);
        if !secondary.is_empty() {
            text.push(SEPARATOR);
            text.push_str(secondary);
        }
        let folded_primary = fold::fold(primary_text);
        let primary_folded_len = folded_primary.chars().count();
        let mut folded = folded_primary;
        if !secondary.is_empty() {
            folded.push(SEPARATOR);
            folded.push_str(&fold::fold(secondary));
        }
        // The blob is capped at write time so the aligner's per-candidate
        // cost has a ceiling no mailbox's contents can raise.
        if folded.chars().count() > super::score::MAX_MATCH_CHARS {
            folded = folded.chars().take(super::score::MAX_MATCH_CHARS).collect();
        }
        let mask = fold::char_mask(&folded);
        let folded = (folded != text).then(|| folded.into_boxed_str());

        Self {
            item_id,
            ref_id,
            account_id,
            mailbox_id,
            mask,
            last_activity: signals.last_activity.unwrap_or(0),
            primary_bytes: u32::try_from(primary_text.len()).unwrap_or(u32::MAX),
            primary_folded_len: u32::try_from(primary_folded_len).unwrap_or(u32::MAX),
            #[allow(clippy::cast_possible_truncation)]
            importance: signals.importance.clamp(0.0, 1.0) as f32,
            frequency: u32::try_from(signals.frequency.max(0)).unwrap_or(u32::MAX),
            kind,
            unread: signals.unread,
            text: text.into_boxed_str(),
            folded,
        }
    }

    /// The text a row renders. Highlight positions are char offsets into
    /// **this** string.
    #[must_use]
    pub fn primary_text(&self) -> &str {
        self.text
            .get(..self.primary_bytes as usize)
            .unwrap_or(&self.text)
    }

    /// The dimmer second line, empty when there is none.
    #[must_use]
    pub fn secondary(&self) -> &str {
        // `+ 1` skips the separator. `get` rather than indexing: the field is
        // private and maintained by `new`, but a slice panic here would take
        // the daemon down over a bookkeeping error.
        self.text
            .get(self.primary_bytes as usize + SEPARATOR.len_utf8()..)
            .unwrap_or_default()
    }

    /// The folded text the matcher runs against.
    #[must_use]
    pub fn blob(&self) -> &str {
        self.folded.as_deref().unwrap_or(&self.text)
    }

    /// The blended-ranking inputs, rebuilt from the packed fields.
    #[must_use]
    pub fn signals(&self) -> Signals {
        Signals {
            last_activity: (self.last_activity != 0).then_some(self.last_activity),
            unread: self.unread,
            importance: f64::from(self.importance),
            frequency: i64::from(self.frequency),
        }
    }

    /// This entry's approximate heap footprint in bytes.
    ///
    /// Approximate in one direction only — a `Box<str>` allocation is exactly
    /// its length, and the struct itself is counted in full, so this never
    /// *under*-reports. Per-allocation allocator overhead is not modelled;
    /// the caps this feeds are budgets, and a budget that is slightly
    /// conservative is the right kind of wrong.
    #[must_use]
    pub fn footprint(&self) -> usize {
        mem::size_of::<Self>()
            + self.text.len()
            + self.folded.as_ref().map_or(0, |folded| folded.len())
    }
}

/// The store's two caps. See the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// The most entries the store will hold, across every kind.
    pub max_entries: usize,
    /// The most heap the entries may occupy, in bytes.
    pub max_bytes: usize,
}

impl Limits {
    /// The caps a `[finder]` block asks for.
    ///
    /// A zero falls back to the default and says so. Zero is not a usable
    /// cap — it admits nothing, so the finder would answer every query with
    /// an empty list while `IndexStatus` cheerfully reported an index with no
    /// entries and no backlog — and a subsystem that silently does nothing is
    /// the hardest kind of misconfiguration to find. Warning and continuing
    /// is what `features::extract` already does for a non-positive
    /// `recency_half_life_days`.
    #[must_use]
    pub fn from_config(config: &crate::config::FinderConfig) -> Self {
        let defaults = crate::config::FinderConfig::default();
        let max_entries = if config.max_entries == 0 {
            tracing::warn!(
                default = defaults.max_entries,
                "finder.max_entries = 0 would admit no entries at all; using the default"
            );
            defaults.max_entries
        } else {
            config.max_entries
        };
        let max_memory_mb = if config.max_memory_mb == 0 {
            tracing::warn!(
                default = defaults.max_memory_mb,
                "finder.max_memory_mb = 0 would admit no entries at all; using the default"
            );
            defaults.max_memory_mb
        } else {
            config.max_memory_mb
        };
        Self {
            max_entries: max_entries as usize,
            max_bytes: (max_memory_mb as usize).saturating_mul(1024 * 1024),
        }
    }
}

/// One kind's entries, plus the `ref_id` index the drain needs.
#[derive(Debug, Default)]
struct Kinded {
    entries: Vec<Entry>,
    slots: HashMap<i64, usize>,
}

impl Kinded {
    /// Insert or replace by `ref_id`. Returns the byte delta the caller
    /// should apply to the store's running total.
    fn upsert(&mut self, entry: Entry) -> isize {
        let added = entry.footprint();
        match self.slots.get(&entry.ref_id).copied() {
            Some(slot) => {
                // `get_mut` rather than indexing: the map and the vector are
                // maintained together, but a panic here would take the whole
                // daemon down over a bookkeeping bug, and returning 0 leaves
                // a self-correcting inconsistency instead.
                let Some(existing) = self.entries.get_mut(slot) else {
                    return 0;
                };
                let removed = existing.footprint();
                *existing = entry;
                added as isize - removed as isize
            }
            None => {
                self.slots.insert(entry.ref_id, self.entries.len());
                self.entries.push(entry);
                added as isize
            }
        }
    }

    /// Remove by `ref_id`, returning the bytes freed.
    fn remove(&mut self, ref_id: i64) -> usize {
        let Some(slot) = self.slots.remove(&ref_id) else {
            return 0;
        };
        if slot >= self.entries.len() {
            return 0;
        }
        let removed = self.entries.swap_remove(slot);
        // `swap_remove` moved the last entry into `slot` — unless `slot` *was*
        // the last, in which case nothing moved and there is nothing to fix.
        if let Some(moved) = self.entries.get(slot) {
            self.slots.insert(moved.ref_id, slot);
        }
        removed.footprint()
    }
}

/// Every findable entry, grouped by kind.
#[derive(Debug, Default)]
pub struct FinderStore {
    kinds: [Kinded; ItemKind::COUNT],
    bytes: usize,
    /// Admissions refused because a cap was already reached, since the last
    /// full load.
    rejected: u64,
    /// Unix seconds of the most recent successful drain or load, or 0 if the
    /// store has never been populated.
    refreshed_at: i64,
}

impl FinderStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Entries currently held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.kinds.iter().map(|k| k.entries.len()).sum()
    }

    /// Whether the store holds nothing — a cold daemon, before the first
    /// load.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The measured heap footprint of everything held.
    #[must_use]
    pub fn footprint(&self) -> usize {
        self.bytes
    }

    /// How many admissions a cap has refused since the last full load.
    #[must_use]
    pub fn rejected(&self) -> u64 {
        self.rejected
    }

    /// A cheap fingerprint of *which* entries are resident: the wrapping sum
    /// of their `item_id`s.
    ///
    /// [`super::index::FinderIndex`]'s reconcile pass compares this against
    /// the same sum taken over the rows a fresh load would select. A count
    /// alone cannot do that job on a capped store — the count is pinned at
    /// the cap whatever the table holds, so a store full of entries for
    /// deleted mail has exactly the same length as a correct one.
    ///
    /// Wrapping rather than saturating, and matched by SQLite's own `SUM`
    /// over the same column: a collision would cost one missed reconcile,
    /// which the next real change repairs anyway.
    #[must_use]
    pub fn checksum(&self) -> i64 {
        self.kinds
            .iter()
            .flat_map(|kind| kind.entries.iter())
            .fold(0i64, |acc, entry| acc.wrapping_add(entry.item_id))
    }

    /// Unix seconds of the last successful refresh, or 0 if never.
    #[must_use]
    pub fn refreshed_at(&self) -> i64 {
        self.refreshed_at
    }

    /// Record that a drain or load completed at `now`.
    pub fn mark_refreshed(&mut self, now: i64) {
        self.refreshed_at = now;
    }

    /// Entries of one kind, in scan order.
    #[must_use]
    pub fn entries(&self, kind: ItemKind) -> &[Entry] {
        &self.kinds[kind.slot()].entries
    }

    /// Insert or replace an entry, honoring `limits`.
    ///
    /// Returns whether the entry is now in the store. A *replacement* is
    /// always accepted even when a cap is already at its limit: refusing it
    /// would leave the stale copy resident, which is strictly worse than
    /// being marginally over budget for one entry, and an update of an
    /// existing entry does not grow the index by a row.
    pub fn upsert(&mut self, entry: Entry, limits: &Limits) -> bool {
        let slot = entry.kind.slot();
        let replacing = self.kinds[slot].slots.contains_key(&entry.ref_id);
        if !replacing
            && (self.len() >= limits.max_entries
                || self.bytes.saturating_add(entry.footprint()) > limits.max_bytes)
        {
            self.rejected = self.rejected.saturating_add(1);
            return false;
        }
        let delta = self.kinds[slot].upsert(entry);
        self.apply_delta(delta);
        true
    }

    /// Remove an entry. A `ref_id` that is not present is not an error — the
    /// change feed is allowed to be redundant (see [`super::index`]).
    pub fn remove(&mut self, kind: ItemKind, ref_id: i64) {
        let freed = self.kinds[kind.slot()].remove(ref_id);
        self.bytes = self.bytes.saturating_sub(freed);
    }

    /// Drop everything, for a full reload.
    pub fn clear(&mut self) {
        for kind in &mut self.kinds {
            kind.entries.clear();
            kind.slots.clear();
        }
        self.bytes = 0;
        self.rejected = 0;
    }

    fn apply_delta(&mut self, delta: isize) {
        if delta >= 0 {
            self.bytes = self.bytes.saturating_add(delta.unsigned_abs());
        } else {
            self.bytes = self.bytes.saturating_sub(delta.unsigned_abs());
        }
    }
}

#[cfg(test)]
mod tests;
