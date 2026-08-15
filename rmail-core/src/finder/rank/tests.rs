use super::{blend, frequency_signal, kind_signal, recency_signal, Ranked, Signals};
use crate::config::FinderRanking;
use crate::finder::ItemKind;

const DAY: i64 = 24 * 60 * 60;

fn weights() -> FinderRanking {
    FinderRanking::default()
}

// ---------------------------------------------------------------------------
// the individual terms
// ---------------------------------------------------------------------------

/// prd.md: "exp(-age/half_life) scaled 0..64".
///
/// Note what is deliberately *not* asserted: that the term halves after
/// `half_life_days`. It does not — `exp(-1)` is 0.368, not 0.5 — and making
/// it do so would be the wrong fix. prd.md's formula and its parameter name
/// disagree, and this build follows the formula everywhere:
/// `features::extract::recency_decay` is the identical expression under the
/// identical misleading name. Giving the finder a true half-life alone would
/// mean two subsystems answering "how old is old" differently from the same
/// configured number.
#[test]
fn recency_decays_exponentially_from_the_scale() {
    let now = 1_800_000_000;
    let fresh = recency_signal(Some(now), now, 30);
    let one_life = recency_signal(Some(now - 30 * DAY), now, 30);
    let two_lives = recency_signal(Some(now - 60 * DAY), now, 30);
    assert!(
        (fresh - 64.0).abs() < 1e-6,
        "a brand new item scores the cap"
    );
    assert!(
        (one_life - 64.0 * (-1.0f64).exp()).abs() < 0.01,
        "got {one_life}"
    );
    assert!(
        two_lives < one_life && one_life < fresh,
        "the term must decay monotonically: {fresh} {one_life} {two_lives}"
    );
    assert!(two_lives > 0.0);
}

/// An item with no timestamp must not be treated as either brand new or
/// infinitely old — both are claims the index cannot support.
#[test]
fn an_item_with_no_activity_scores_no_recency() {
    assert_eq!(recency_signal(None, 1_800_000_000, 30), 0.0);
}

/// Clock skew is real (a `Date:` header from next week), and a future
/// timestamp must not be able to score above the scale.
#[test]
fn a_future_timestamp_is_clamped_to_the_cap() {
    let now = 1_800_000_000;
    let future = recency_signal(Some(now + 90 * DAY), now, 30);
    assert!(
        (future - 64.0).abs() < 1e-6,
        "a future item must cap at the scale, got {future}"
    );
}

#[test]
fn a_zero_half_life_does_not_divide_by_zero() {
    let now = 1_800_000_000;
    let value = recency_signal(Some(now - DAY), now, 0);
    assert!(value.is_finite(), "got {value}");
}

/// The term prd.md leaves as a raw count. Compressed, or a heavily-used
/// contact would outrank every textual match in the index.
#[test]
fn frequency_is_compressed_and_saturates() {
    assert_eq!(frequency_signal(0), 0.0);
    assert_eq!(frequency_signal(-5), 0.0);
    let twenty = frequency_signal(20);
    let thousand = frequency_signal(1_000);
    let huge = frequency_signal(1_000_000);
    assert!(twenty > 0.0 && twenty < 1.0, "got {twenty}");
    assert!((thousand - 1.0).abs() < 1e-9, "got {thousand}");
    assert!(
        (huge - thousand).abs() < 1e-9,
        "past saturation the term must stop growing: {huge} vs {thousand}"
    );
    // The compression is what keeps the term bounded: a raw count times
    // `w_frequency = 10` would be 10_000_000 here.
    assert!(weights().w_frequency * huge < 100.0);
}

/// prd.md: "command/mailbox outrank message for short queries".
#[test]
fn navigational_kinds_lead_on_a_short_query() {
    let short = 2;
    assert!(kind_signal(ItemKind::Command, short) > kind_signal(ItemKind::Mailbox, short));
    assert!(kind_signal(ItemKind::Mailbox, short) > kind_signal(ItemKind::Message, short));
    assert_eq!(kind_signal(ItemKind::Message, short), 0.0);
}

/// ...and the prior has to fade, or a remembered subject line would never be
/// able to outrank a folder.
#[test]
fn the_kind_prior_fades_as_the_query_lengthens() {
    let short = kind_signal(ItemKind::Command, 1);
    let medium = kind_signal(ItemKind::Command, 4);
    let long = kind_signal(ItemKind::Command, 40);
    assert!(short > medium, "{short} vs {medium}");
    assert!(medium > 0.0);
    assert_eq!(long, 0.0, "a long query is not a navigation jump");
}

// ---------------------------------------------------------------------------
// the blend
// ---------------------------------------------------------------------------

#[test]
fn every_signal_moves_the_blended_score() {
    let now = 1_800_000_000;
    let weights = weights();
    let base = Signals::default();
    let plain = blend(100, ItemKind::Message, &base, 6, now, &weights);

    let unread = blend(
        100,
        ItemKind::Message,
        &Signals {
            unread: true,
            ..base
        },
        6,
        now,
        &weights,
    );
    assert!(unread > plain, "unread must help: {unread} vs {plain}");

    let recent = blend(
        100,
        ItemKind::Message,
        &Signals {
            last_activity: Some(now),
            ..base
        },
        6,
        now,
        &weights,
    );
    assert!(recent > plain, "recency must help: {recent} vs {plain}");

    let important = blend(
        100,
        ItemKind::Message,
        &Signals {
            importance: 1.0,
            ..base
        },
        6,
        now,
        &weights,
    );
    assert!(important > plain, "importance must help");

    let frequent = blend(
        100,
        ItemKind::Contact,
        &Signals {
            frequency: 500,
            ..base
        },
        6,
        now,
        &weights,
    );
    let infrequent = blend(100, ItemKind::Contact, &base, 6, now, &weights);
    assert!(frequent > infrequent, "frequency must help");
}

/// An importance value outside 0..1 (a corrupt row, a future writer) must not
/// be able to dominate the blend.
#[test]
fn importance_is_clamped() {
    let now = 1_800_000_000;
    let weights = weights();
    let sane = blend(
        0,
        ItemKind::Message,
        &Signals {
            importance: 1.0,
            ..Signals::default()
        },
        6,
        now,
        &weights,
    );
    let absurd = blend(
        0,
        ItemKind::Message,
        &Signals {
            importance: 1_000.0,
            ..Signals::default()
        },
        6,
        now,
        &weights,
    );
    assert_eq!(sane, absurd);
}

/// Zeroed weights must reduce the blend to the raw subsequence score — the
/// property that makes `[finder.ranking]` genuinely a set of weights rather
/// than a set of hints.
#[test]
fn zero_weights_leave_only_the_fuzzy_score() {
    let weights = FinderRanking {
        half_life_days: 30,
        w_recency: 0.0,
        w_unread: 0.0,
        w_important: 0.0,
        w_frequency: 0.0,
        w_kind: 0.0,
    };
    let signals = Signals {
        last_activity: Some(1_800_000_000),
        unread: true,
        importance: 1.0,
        frequency: 900,
    };
    let score = blend(137, ItemKind::Command, &signals, 1, 1_800_000_000, &weights);
    assert!((score - 137.0).abs() < 1e-9, "got {score}");
}

// ---------------------------------------------------------------------------
// tie-breaking
// ---------------------------------------------------------------------------

fn ranked(score: f64, fuzzy: u32, last_activity: i64, length: u32, item_id: i64) -> Ranked {
    Ranked {
        score,
        fuzzy,
        last_activity,
        length,
        item_id,
    }
}

/// prd.md: "Ties: higher fuzzy -> newer -> shorter candidate -> id."
#[test]
fn ties_break_by_fuzzy_then_newer_then_shorter_then_id() {
    let base = ranked(10.0, 5, 100, 20, 7);

    let higher_fuzzy = ranked(10.0, 6, 100, 20, 7);
    assert!(higher_fuzzy > base, "higher fuzzy wins a score tie");

    let newer = ranked(10.0, 5, 200, 20, 7);
    assert!(newer > base, "newer wins a fuzzy tie");

    let shorter = ranked(10.0, 5, 100, 10, 7);
    assert!(shorter > base, "shorter wins a recency tie");

    let lower_id = ranked(10.0, 5, 100, 20, 3);
    assert!(lower_id > base, "the lower id wins everything else");
}

/// The order has to be *total*, or a picker's list reshuffles between two
/// keystrokes that produced the same candidates.
#[test]
fn identical_keys_compare_equal() {
    let a = ranked(10.0, 5, 100, 20, 7);
    let b = ranked(10.0, 5, 100, 20, 7);
    assert_eq!(a.cmp(&b), std::cmp::Ordering::Equal);
}

/// A NaN score (a pathological weight, an overflowing `exp`) must degrade the
/// ranking, never the heap that the ordering is an invariant of.
#[test]
fn a_nan_score_still_orders_totally() {
    let nan = ranked(f64::NAN, 5, 100, 20, 7);
    let real = ranked(10.0, 5, 100, 20, 7);
    // Whichever way it falls, it must be consistent and it must not be
    // `None` — which is what `partial_cmp` would give with a naive impl.
    assert!(nan.partial_cmp(&real).is_some());
    assert_eq!(nan.cmp(&real), nan.cmp(&real));
    let mut items = [nan, real, ranked(20.0, 1, 1, 1, 1)];
    items.sort_unstable();
    assert_eq!(items.len(), 3);
}
