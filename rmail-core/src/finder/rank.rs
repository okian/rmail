//! Blended ranking: the subsequence score plus the signals that decide
//! between two candidates that match equally well.
//!
//! prd.md's formula, verbatim:
//!
//! ```text
//! final = fuzzy
//!       + w_recency   * recency_decay(last_activity)
//!       + w_unread    * is_unread
//!       + w_important * importance
//!       + w_frequency * interaction_count
//!       + w_kind      * kind_priority(scope)
//! ```
//!
//! Every weight is `[finder.ranking]`-tunable
//! ([`crate::config::FinderRanking`]); this module owns only the shape of the
//! terms each weight multiplies, and each of the three that prd.md leaves
//! implicit is pinned here rather than left to the call site.
//!
//! # The terms are normalized to 0..1, except recency
//!
//! `w_recency = 40`, `w_unread = 25` and so on are only comparable to each
//! other — and to a fuzzy score in the low hundreds — if what they multiply
//! is on a comparable scale. `is_unread` and `importance` already are.
//! `interaction_count` is not: prd.md multiplies a raw count by `w_frequency
//! = 10`, and a contact with four thousand messages would then score forty
//! thousand points of "frequency" and win every query regardless of what was
//! typed. [`frequency_signal`] therefore compresses it logarithmically and
//! saturates, so frequency breaks ties between plausible candidates instead
//! of deciding the ranking on its own.
//!
//! Recency is the exception, and deliberately: prd.md states its scale
//! explicitly ("`exp(-age/half_life)` scaled 0..64"), so [`recency_signal`]
//! multiplies the decay by 64 and `w_recency` is read as a per-unit weight
//! against that. Left as written rather than renormalized, because the
//! default weights were chosen against that scale.
//!
//! # Kind priority is a function of query length, not just of scope
//!
//! prd.md asks for `kind_priority(scope)` and explains what it is for:
//! "command/mailbox outrank message for short queries". Scope alone cannot
//! deliver that — under the default `all` scope, scope is a constant, so a
//! prior that depends only on it is a constant too and reorders nothing.
//! What actually varies, and what the sentence is about, is the query: two
//! characters is someone reaching for a folder or a command, and a whole
//! remembered subject line is someone reaching for a message. So
//! [`kind_signal`] decays the navigational kinds' prior as the query gets
//! longer, reaching zero at [`KIND_PRIOR_FADE_CHARS`]. Under a single-kind
//! scope every candidate shares the term and it cancels out — which is the
//! right behavior, and is why making it depend on scope alone would have
//! been the wrong one.
//!
//! # Ties
//!
//! prd.md: "Ties: higher fuzzy → newer → shorter candidate → id." [`Ranked`]
//! implements exactly that as its `Ord`, so the top-K heap and the final
//! sort cannot disagree about what "best" means. The full key is
//! deterministic down to the id, which is what keeps a picker's list from
//! reshuffling between two keystrokes that produced the same candidates.

use std::cmp::Ordering;

use crate::config::FinderRanking;

use super::ItemKind;

/// The scale prd.md gives the recency term: `exp(-age/half_life)` mapped onto
/// `0..64` before `w_recency` multiplies it.
const RECENCY_SCALE: f64 = 64.0;

/// The interaction count at which [`frequency_signal`] reaches 1.0.
///
/// Above it the term saturates rather than continuing to grow: the
/// difference between a contact you have exchanged 900 messages with and one
/// you have exchanged 4000 with is not information a picker should act on,
/// whereas the difference between 0 and 20 very much is.
const FREQUENCY_SATURATION: f64 = 1_000.0;

/// The query length at which every kind's navigational prior has faded to
/// zero. See the module docs.
const KIND_PRIOR_FADE_CHARS: f64 = 8.0;

/// The signals a candidate carries into the blend, independent of the query.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Signals {
    /// Unix seconds of the last thing that happened to this item, or `None`
    /// for an item that has no meaningful time (a command).
    pub last_activity: Option<i64>,
    /// Whether the item is unread. Only messages are ever unread.
    pub unread: bool,
    /// 0..1. Flagged mail, today; a wider notion of importance is task 81's.
    pub importance: f64,
    /// Interaction count: messages in a mailbox, messages from a contact.
    pub frequency: i64,
}

/// The recency term: `exp(-age / half_life)`, scaled to `0..RECENCY_SCALE`.
///
/// **`half_life_days` is not a half-life.** `exp(-1)` is 0.368, so the term
/// falls to 37% of the scale over `half_life_days`, not to 50%. prd.md's
/// formula and its parameter name disagree; this follows the formula, because
/// `features::extract::recency_decay` — the search pipeline's own recency
/// feature — is the identical expression under the identical name, and two
/// subsystems answering "how old is old" differently from the same configured
/// number would be worse than one misleading name.
///
/// An item with no `last_activity` scores 0 rather than "infinitely old" or
/// "now" — both of which would be a claim this index cannot support. An item
/// stamped in the future (clock skew, a server date header from next week)
/// is clamped to "now" rather than allowed to score above the scale.
#[must_use]
pub fn recency_signal(last_activity: Option<i64>, now: i64, half_life_days: u32) -> f64 {
    let Some(last_activity) = last_activity else {
        return 0.0;
    };
    let half_life = f64::from(half_life_days.max(1));
    let age_days = f64::from(i32::try_from((now - last_activity).max(0)).unwrap_or(i32::MAX))
        / (24.0 * 60.0 * 60.0);
    RECENCY_SCALE * (-age_days / half_life).exp()
}

/// The frequency term, compressed to `0..1`. See the module docs.
#[must_use]
pub fn frequency_signal(count: i64) -> f64 {
    if count <= 0 {
        return 0.0;
    }
    #[allow(clippy::cast_precision_loss)]
    let count = count as f64;
    (count.min(FREQUENCY_SATURATION).ln_1p() / FREQUENCY_SATURATION.ln_1p()).clamp(0.0, 1.0)
}

/// The kind term: how much this kind should be favored for a query of
/// `query_chars` characters, in `0..1`.
///
/// The per-kind base is an ordering, not a measurement: a command is the
/// most "jump-like" thing in the index (it has no other way to be reached),
/// a mailbox next, then the named things (saved searches, tags), then
/// contacts, and a message last — a message is what full search is for, and
/// the finder's message rows exist for the known-item case where the query
/// is long and specific anyway.
#[must_use]
pub fn kind_signal(kind: ItemKind, query_chars: usize) -> f64 {
    let base = match kind {
        ItemKind::Command => 1.0,
        ItemKind::Mailbox => 0.8,
        ItemKind::SavedSearch => 0.6,
        ItemKind::Tag => 0.5,
        ItemKind::Contact => 0.4,
        ItemKind::Message => 0.0,
    };
    #[allow(clippy::cast_precision_loss)]
    let typed = query_chars as f64;
    let fade = ((KIND_PRIOR_FADE_CHARS - typed) / KIND_PRIOR_FADE_CHARS).clamp(0.0, 1.0);
    base * fade
}

/// Blend a subsequence score with a candidate's signals.
#[must_use]
pub fn blend(
    fuzzy: u32,
    kind: ItemKind,
    signals: &Signals,
    query_chars: usize,
    now: i64,
    weights: &FinderRanking,
) -> f64 {
    f64::from(fuzzy)
        + weights.w_recency * recency_signal(signals.last_activity, now, weights.half_life_days)
        + weights.w_unread * f64::from(u8::from(signals.unread))
        + weights.w_important * signals.importance.clamp(0.0, 1.0)
        + weights.w_frequency * frequency_signal(signals.frequency)
        + weights.w_kind * kind_signal(kind, query_chars)
}

/// One scored candidate, ordered by prd.md's full ranking key.
///
/// `Ord` is "better first" reversed into `Ordering::Less` — i.e. the *worst*
/// candidate is the `min`, which is what lets a plain `BinaryHeap` of these
/// act as a bounded top-K by popping its max... except that a `BinaryHeap`
/// pops its max, so the scan wraps these in `Reverse`. Keeping the natural
/// order "best is greatest" is what makes the final `sort` read correctly
/// without a comparator.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ranked {
    /// The blended score. Not `Ord` on its own — see [`Ranked::cmp`].
    pub score: f64,
    /// The raw subsequence score, prd.md's first tie-break.
    pub fuzzy: u32,
    /// Unix seconds, prd.md's second tie-break ("newer").
    pub last_activity: i64,
    /// Characters in `primary_text`, prd.md's third ("shorter candidate").
    pub length: u32,
    /// The index row id, prd.md's last resort and the reason this order is
    /// total.
    pub item_id: i64,
}

impl Eq for Ranked {}

impl Ord for Ranked {
    /// Best is greatest.
    ///
    /// `total_cmp` rather than `partial_cmp`: the blended score is an `f64`
    /// built from an `exp()` and a `ln_1p()`, and a `BinaryHeap` whose
    /// comparator can return `None` is a heap whose invariant can silently
    /// break. `total_cmp` orders every `f64` including NaN, so a weight
    /// configured to something pathological degrades the *ranking* rather
    /// than the data structure.
    fn cmp(&self, other: &Self) -> Ordering {
        self.score
            .total_cmp(&other.score)
            .then_with(|| self.fuzzy.cmp(&other.fuzzy))
            .then_with(|| self.last_activity.cmp(&other.last_activity))
            // Shorter is better, so the comparison is inverted here only.
            .then_with(|| other.length.cmp(&self.length))
            // Lower id is better, likewise inverted.
            .then_with(|| other.item_id.cmp(&self.item_id))
    }
}

impl PartialOrd for Ranked {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests;
