use super::*;
use crate::tui::model::MessageRow;

fn row(id: i64, seen: bool) -> MessageRow {
    MessageRow {
        id,
        subject: format!("subject {id}"),
        from: "Alice".to_owned(),
        from_addr: Some("alice@example.com".to_owned()),
        date: Some(1_700_000_000 + id),
        flags: if seen {
            vec![SEEN.to_owned()]
        } else {
            Vec::new()
        },
        has_attachments: false,
        has_note: false,
        to: None,
        tags: Vec::new(),
        ai: None,
    }
}

// ---- Unknown vs Estimated ----

#[test]
fn a_folder_never_seeded_is_unknown() {
    let ledger = Ledger::default();
    assert_eq!(ledger.get(1), Estimate::Unknown);
}

#[test]
fn seeding_counts_the_unread_rows() {
    let mut ledger = Ledger::default();
    ledger.seed(1, &[row(1, false), row(2, true), row(3, false)]);
    assert_eq!(ledger.get(1), Estimate::Estimated(2));
}

#[test]
fn seeding_an_all_read_folder_is_estimated_zero_not_unknown() {
    // Distinct from `a_folder_never_seeded_is_unknown`: this folder has
    // been visited, it just happens to have nothing unread. Collapsing
    // this into `Unknown` would render `•` for a folder the user has
    // actually looked at, which is exactly the honesty law this type
    // exists to enforce.
    let mut ledger = Ledger::default();
    ledger.seed(1, &[row(1, true), row(2, true)]);
    assert_eq!(ledger.get(1), Estimate::Estimated(0));
}

#[test]
fn seeding_an_empty_page_is_estimated_zero() {
    let mut ledger = Ledger::default();
    ledger.seed(1, &[]);
    assert_eq!(ledger.get(1), Estimate::Estimated(0));
}

#[test]
fn each_folder_is_tracked_independently() {
    let mut ledger = Ledger::default();
    ledger.seed(1, &[row(1, false)]);
    ledger.seed(2, &[row(2, false), row(3, false)]);
    assert_eq!(ledger.get(1), Estimate::Estimated(1));
    assert_eq!(ledger.get(2), Estimate::Estimated(2));
    assert_eq!(ledger.get(3), Estimate::Unknown);
}

#[test]
fn reseeding_replaces_rather_than_merges() {
    let mut ledger = Ledger::default();
    ledger.seed(1, &[row(1, false), row(2, false)]);
    ledger.apply(
        1,
        &Delta::Arrived {
            mailbox_id: 1,
            message_id: 3,
        },
    );
    assert_eq!(ledger.get(1), Estimate::Estimated(3));

    // A fresh page for the same folder replaces the whole entry, deltas
    // applied since the last seed included — the entry it produces is
    // exactly what that page says, nothing carried over.
    ledger.seed(1, &[row(1, true)]);
    assert_eq!(ledger.get(1), Estimate::Estimated(0));
}

// ---- Estimate::label ----

#[test]
fn unknowns_label_is_the_bullet_glyph() {
    assert_eq!(Estimate::Unknown.label(), "•");
}

#[test]
fn an_estimates_label_is_tilde_prefixed() {
    assert_eq!(Estimate::Estimated(0).label(), "~0");
    assert_eq!(Estimate::Estimated(42).label(), "~42");
}

// ---- Arrived ----

#[test]
fn arrived_increments_a_seeded_folder() {
    let mut ledger = Ledger::default();
    ledger.seed(1, &[row(1, true)]);
    ledger.apply(
        1,
        &Delta::Arrived {
            mailbox_id: 1,
            message_id: 2,
        },
    );
    assert_eq!(ledger.get(1), Estimate::Estimated(1));
}

#[test]
fn arrived_for_an_unseeded_folder_is_a_no_op() {
    let mut ledger = Ledger::default();
    ledger.apply(
        1,
        &Delta::Arrived {
            mailbox_id: 1,
            message_id: 2,
        },
    );
    assert_eq!(ledger.get(1), Estimate::Unknown);
}

#[test]
fn arrived_is_idempotent_against_a_redelivered_event() {
    let mut ledger = Ledger::default();
    ledger.seed(1, &[]);
    let delta = Delta::Arrived {
        mailbox_id: 1,
        message_id: 5,
    };
    ledger.apply(1, &delta);
    ledger.apply(2, &delta);
    assert_eq!(ledger.get(1), Estimate::Estimated(1));
}

#[test]
fn a_message_that_arrives_and_is_then_marked_read_nets_to_zero() {
    // Proves the per-message map bridges across delta *kinds*, not just
    // within one: `Arrived` records message 5 as unread without a seed
    // ever having seen it, and a later `Flags` for that same id must find
    // that record to know which way to adjust.
    let mut ledger = Ledger::default();
    ledger.seed(1, &[]);
    ledger.apply(
        1,
        &Delta::Arrived {
            mailbox_id: 1,
            message_id: 5,
        },
    );
    assert_eq!(ledger.get(1), Estimate::Estimated(1));
    ledger.apply(
        2,
        &Delta::Flags {
            mailbox_id: 1,
            message_id: 5,
            flags: vec![SEEN.to_owned()],
        },
    );
    assert_eq!(ledger.get(1), Estimate::Estimated(0));
}

// ---- Flags ----

#[test]
fn flags_decrements_when_a_known_unread_message_gains_seen() {
    let mut ledger = Ledger::default();
    ledger.seed(1, &[row(1, false)]);
    ledger.apply(
        1,
        &Delta::Flags {
            mailbox_id: 1,
            message_id: 1,
            flags: vec![SEEN.to_owned()],
        },
    );
    assert_eq!(ledger.get(1), Estimate::Estimated(0));
}

#[test]
fn flags_increments_when_a_known_read_message_loses_seen() {
    // The "mark as unread" direction — just as real as read, and the
    // reason this cannot be a one-way ratchet.
    let mut ledger = Ledger::default();
    ledger.seed(1, &[row(1, true)]);
    ledger.apply(
        1,
        &Delta::Flags {
            mailbox_id: 1,
            message_id: 1,
            flags: Vec::new(),
        },
    );
    assert_eq!(ledger.get(1), Estimate::Estimated(1));
}

#[test]
fn flags_matches_seen_exactly_not_case_insensitively() {
    // Mirrors `MessageRow::has_flag`'s own exact-match semantics — see the
    // comment at the call site in `apply`. A wrongly-cased flag from a
    // server that (against convention) sent `\seen` must not be read as
    // SEEN here any more than `has_flag` reads it as SEEN elsewhere.
    let mut ledger = Ledger::default();
    ledger.seed(1, &[row(1, false)]);
    ledger.apply(
        1,
        &Delta::Flags {
            mailbox_id: 1,
            message_id: 1,
            flags: vec!["\\seen".to_owned()],
        },
    );
    assert_eq!(
        ledger.get(1),
        Estimate::Estimated(1),
        "a differently-cased flag is not \\Seen"
    );
}

#[test]
fn flags_is_a_no_op_when_the_seen_state_does_not_change() {
    let mut ledger = Ledger::default();
    ledger.seed(1, &[row(1, false)]);
    ledger.apply(
        1,
        &Delta::Flags {
            mailbox_id: 1,
            message_id: 1,
            flags: vec!["\\Flagged".to_owned()],
        },
    );
    assert_eq!(ledger.get(1), Estimate::Estimated(1));
}

#[test]
fn flags_for_an_unknown_message_in_a_known_folder_is_a_no_op() {
    let mut ledger = Ledger::default();
    ledger.seed(1, &[row(1, false)]);
    ledger.apply(
        1,
        &Delta::Flags {
            mailbox_id: 1,
            message_id: 999,
            flags: vec![SEEN.to_owned()],
        },
    );
    assert_eq!(ledger.get(1), Estimate::Estimated(1));
}

#[test]
fn flags_for_an_unseeded_folder_is_a_no_op() {
    let mut ledger = Ledger::default();
    ledger.apply(
        1,
        &Delta::Flags {
            mailbox_id: 1,
            message_id: 1,
            flags: vec![SEEN.to_owned()],
        },
    );
    assert_eq!(ledger.get(1), Estimate::Unknown);
}

// ---- Moved ----
//
// `Moved` never credits a destination — see `ledger.rs`'s own module doc
// ("`Moved` only ever touches the folder a message left") for why: the
// destination discovers the message fresh, under a different id, and
// credits it there via its own `Arrived`. These tests pin exactly that
// division of labor.

#[test]
fn moved_decrements_the_source_and_never_touches_the_destination() {
    let mut ledger = Ledger::default();
    ledger.seed(1, &[row(1, false)]);
    ledger.seed(2, &[]);
    ledger.apply(
        1,
        &Delta::Moved {
            mailbox_id: 1,
            message_id: 1,
        },
    );
    assert_eq!(ledger.get(1), Estimate::Estimated(0), "the source lost it");
    assert_eq!(
        ledger.get(2),
        Estimate::Estimated(0),
        "the destination was never credited under the old id"
    );
}

#[test]
fn a_moved_messages_own_new_arrival_at_the_destination_is_the_only_credit() {
    // The realistic full sequence: a message moves (decrementing the
    // source), and only later does the destination's own sync discover it
    // — under a new id — and credit it there via `Arrived`. An earlier
    // draft of `Moved` credited the destination directly under the *old*
    // id, which both double-counted against this sequence and left a
    // phantom `known` entry no future event could ever reference again.
    let mut ledger = Ledger::default();
    ledger.seed(1, &[row(1, false)]);
    ledger.seed(2, &[]);

    ledger.apply(
        1,
        &Delta::Moved {
            mailbox_id: 1,
            message_id: 1,
        },
    );
    assert_eq!(ledger.get(1), Estimate::Estimated(0));
    assert_eq!(ledger.get(2), Estimate::Estimated(0), "not yet discovered");

    // The destination's sync finds it, under a brand new id.
    ledger.apply(
        2,
        &Delta::Arrived {
            mailbox_id: 2,
            message_id: 101,
        },
    );
    assert_eq!(
        ledger.get(2),
        Estimate::Estimated(1),
        "credited exactly once, under the id that actually persists"
    );
}

#[test]
fn moved_of_a_read_message_adjusts_nothing() {
    let mut ledger = Ledger::default();
    ledger.seed(1, &[row(1, true)]);
    ledger.apply(
        1,
        &Delta::Moved {
            mailbox_id: 1,
            message_id: 1,
        },
    );
    assert_eq!(ledger.get(1), Estimate::Estimated(0));
}

#[test]
fn moved_of_an_unknown_message_is_a_no_op() {
    let mut ledger = Ledger::default();
    ledger.seed(1, &[]);
    ledger.apply(
        1,
        &Delta::Moved {
            mailbox_id: 1,
            message_id: 999,
        },
    );
    assert_eq!(ledger.get(1), Estimate::Estimated(0));
}

#[test]
fn moved_from_an_unseeded_folder_is_a_no_op() {
    let mut ledger = Ledger::default();
    ledger.apply(
        1,
        &Delta::Moved {
            mailbox_id: 1,
            message_id: 1,
        },
    );
    assert_eq!(ledger.get(1), Estimate::Unknown);
}

// ---- Deleted ----

#[test]
fn deleted_decrements_a_known_unread_message() {
    let mut ledger = Ledger::default();
    ledger.seed(1, &[row(1, false)]);
    ledger.apply(
        1,
        &Delta::Deleted {
            mailbox_id: 1,
            message_id: 1,
        },
    );
    assert_eq!(ledger.get(1), Estimate::Estimated(0));
}

#[test]
fn deleted_of_a_read_message_does_not_decrement() {
    let mut ledger = Ledger::default();
    ledger.seed(1, &[row(1, true)]);
    ledger.apply(
        1,
        &Delta::Deleted {
            mailbox_id: 1,
            message_id: 1,
        },
    );
    assert_eq!(ledger.get(1), Estimate::Estimated(0));
}

#[test]
fn deleted_of_an_unknown_message_is_a_no_op() {
    let mut ledger = Ledger::default();
    ledger.seed(1, &[row(1, false)]);
    ledger.apply(
        1,
        &Delta::Deleted {
            mailbox_id: 1,
            message_id: 999,
        },
    );
    assert_eq!(ledger.get(1), Estimate::Estimated(1));
}

#[test]
fn deleting_the_same_message_twice_only_decrements_once() {
    // The `known` entry is removed on the first delete, so a redelivered
    // or duplicate `Deleted` event finds nothing left to subtract.
    let mut ledger = Ledger::default();
    ledger.seed(1, &[row(1, false)]);
    let delta = Delta::Deleted {
        mailbox_id: 1,
        message_id: 1,
    };
    ledger.apply(1, &delta);
    ledger.apply(2, &delta);
    assert_eq!(ledger.get(1), Estimate::Estimated(0));
}

// ---- The seq floor: a stale replay cannot double up on a fresh seed ----

#[test]
fn seeding_stamps_the_folder_with_the_ledgers_current_high_water_seq() {
    let mut ledger = Ledger::default();
    // Advance the ledger's own notion of "how far it has seen" via an
    // unrelated folder, then seed a fresh one — its floor must reflect
    // that high-water mark, not zero.
    ledger.seed(9, &[]);
    ledger.apply(
        5,
        &Delta::Arrived {
            mailbox_id: 9,
            message_id: 1,
        },
    );
    ledger.seed(1, &[row(1, false)]);

    // A delta at exactly the floor is stale — already reflected in the
    // fresh seed above — and must not apply.
    ledger.apply(
        5,
        &Delta::Arrived {
            mailbox_id: 1,
            message_id: 2,
        },
    );
    assert_eq!(
        ledger.get(1),
        Estimate::Estimated(1),
        "the seq-5 arrival predates the seed and must be ignored"
    );

    // A delta genuinely newer than the floor still applies normally.
    ledger.apply(
        6,
        &Delta::Arrived {
            mailbox_id: 1,
            message_id: 3,
        },
    );
    assert_eq!(ledger.get(1), Estimate::Estimated(2));
}

#[test]
fn a_stale_flags_event_below_the_floor_cannot_corrupt_a_fresh_seed() {
    // The scenario `Ledger::seed`'s own doc names directly: a replayed
    // `WatchEvents` backlog landing on top of an already-accurate reseed.
    // The fresh seed already reflects message 1's *current* read state;
    // replaying an old `Flags` event from before that state was reached
    // must not flip it back.
    let mut ledger = Ledger::default();
    ledger.seed(9, &[]);
    ledger.apply(
        10,
        &Delta::Flags {
            mailbox_id: 9,
            message_id: 1,
            flags: Vec::new(),
        },
    );
    // Message 1 is freshly re-seeded as read (its true current state).
    ledger.seed(1, &[row(1, true)]);
    assert_eq!(ledger.get(1), Estimate::Estimated(0));

    // A stale, already-superseded `Flags` event (message 1 was unread,
    // seq 10, replayed from before the fresh seed) must not reapply.
    ledger.apply(
        10,
        &Delta::Flags {
            mailbox_id: 1,
            message_id: 1,
            flags: Vec::new(),
        },
    );
    assert_eq!(
        ledger.get(1),
        Estimate::Estimated(0),
        "the stale replay must not flip an already-fresh seed back to unread"
    );
}

#[test]
fn moved_and_deleted_are_also_floor_gated() {
    let mut ledger = Ledger::default();
    ledger.seed(9, &[]);
    ledger.apply(
        3,
        &Delta::Arrived {
            mailbox_id: 9,
            message_id: 1,
        },
    );
    ledger.seed(1, &[row(1, false)]);

    ledger.apply(
        3,
        &Delta::Deleted {
            mailbox_id: 1,
            message_id: 1,
        },
    );
    assert_eq!(
        ledger.get(1),
        Estimate::Estimated(1),
        "a stale Deleted at the floor must not apply"
    );

    ledger.apply(
        3,
        &Delta::Moved {
            mailbox_id: 1,
            message_id: 1,
        },
    );
    assert_eq!(
        ledger.get(1),
        Estimate::Estimated(1),
        "a stale Moved at the floor must not apply either"
    );
}

#[test]
fn reseeding_raises_the_floor_again() {
    let mut ledger = Ledger::default();
    ledger.seed(1, &[row(1, false)]);
    ledger.apply(
        7,
        &Delta::Arrived {
            mailbox_id: 1,
            message_id: 2,
        },
    );
    assert_eq!(ledger.get(1), Estimate::Estimated(2));

    // Reseeding again picks up the ledger's now-higher last_seq (7), so a
    // delta at or below 7 is stale relative to *this* seed too.
    ledger.seed(1, &[row(1, false)]);
    ledger.apply(
        7,
        &Delta::Arrived {
            mailbox_id: 1,
            message_id: 3,
        },
    );
    assert_eq!(
        ledger.get(1),
        Estimate::Estimated(1),
        "seq 7 predates the reseed, which already happened after it was processed"
    );
}

// ---- unloaded/negative space: negation-style no-ops still touch nothing ----

#[test]
fn an_unrelated_folders_delta_never_touches_a_different_folder() {
    let mut ledger = Ledger::default();
    ledger.seed(1, &[row(1, false)]);
    ledger.seed(2, &[row(2, false)]);
    ledger.apply(
        1,
        &Delta::Deleted {
            mailbox_id: 1,
            message_id: 1,
        },
    );
    assert_eq!(ledger.get(1), Estimate::Estimated(0));
    assert_eq!(ledger.get(2), Estimate::Estimated(1), "untouched");
}

// ---- Convergence under a replayed event sequence ----
//
// The task's own acceptance: "proven by a test that replays an event
// sequence and checks the running count at each step, not just the final
// one." Both tests below assert after every single `apply`, not only at
// the end.

#[test]
fn a_folder_swept_clean_by_a_bulk_mark_all_read_converges_without_a_full_re_list() {
    let mut ledger = Ledger::default();
    ledger.seed(
        1,
        &[row(1, false), row(2, false), row(3, false), row(4, false)],
    );
    assert_eq!(ledger.get(1), Estimate::Estimated(4));

    // A bulk "mark all read" arrives as one `Flags` delta per message, in
    // whatever order the daemon happens to apply them — the running total
    // must be right after every single one, not just once the whole batch
    // has landed.
    for (step, message_id) in [1_i64, 2, 3, 4].into_iter().enumerate() {
        ledger.apply(
            u64_to_seq(step + 1),
            &Delta::Flags {
                mailbox_id: 1,
                message_id,
                flags: vec![SEEN.to_owned()],
            },
        );
        let expected = 4 - u64::try_from(step + 1).unwrap();
        assert_eq!(
            ledger.get(1),
            Estimate::Estimated(expected),
            "after marking message {message_id} read (step {step})"
        );
    }
    assert_eq!(ledger.get(1), Estimate::Estimated(0));
}

#[test]
fn a_burst_of_new_mail_while_unfocused_converges_without_a_full_re_list() {
    let mut ledger = Ledger::default();
    ledger.seed(1, &[row(1, true)]);
    assert_eq!(ledger.get(1), Estimate::Estimated(0));

    for (step, message_id) in [10_i64, 11, 12].into_iter().enumerate() {
        ledger.apply(
            u64_to_seq(step + 1),
            &Delta::Arrived {
                mailbox_id: 1,
                message_id,
            },
        );
        let expected = u64::try_from(step + 1).unwrap();
        assert_eq!(
            ledger.get(1),
            Estimate::Estimated(expected),
            "after message {message_id} arrived (step {step})"
        );
    }
    assert_eq!(ledger.get(1), Estimate::Estimated(3));
}

#[test]
fn a_mixed_burst_of_arrivals_reads_a_move_and_the_destinations_own_arrival_converges_step_by_step()
{
    // The realistic case none of the single-delta-kind tests above cover
    // on their own: new mail lands, some of it gets read, one message
    // gets moved away and only later discovered at its destination —
    // interleaved, exactly as a live session would deliver them, checked
    // after every step.
    let mut ledger = Ledger::default();
    ledger.seed(1, &[row(1, false), row(2, false)]);
    ledger.seed(2, &[]);
    assert_eq!(ledger.get(1), Estimate::Estimated(2));

    ledger.apply(
        1,
        &Delta::Arrived {
            mailbox_id: 1,
            message_id: 3,
        },
    );
    assert_eq!(ledger.get(1), Estimate::Estimated(3), "after arrival");

    ledger.apply(
        2,
        &Delta::Flags {
            mailbox_id: 1,
            message_id: 1,
            flags: vec![SEEN.to_owned()],
        },
    );
    assert_eq!(
        ledger.get(1),
        Estimate::Estimated(2),
        "after marking one read"
    );

    ledger.apply(
        3,
        &Delta::Moved {
            mailbox_id: 1,
            message_id: 3,
        },
    );
    assert_eq!(
        ledger.get(1),
        Estimate::Estimated(1),
        "after moving the still-unread arrival out"
    );
    assert_eq!(
        ledger.get(2),
        Estimate::Estimated(0),
        "not credited yet — the destination has not discovered it"
    );

    // The destination's own sync discovers it moments later, under a new id.
    ledger.apply(
        4,
        &Delta::Arrived {
            mailbox_id: 2,
            message_id: 201,
        },
    );
    assert_eq!(
        ledger.get(2),
        Estimate::Estimated(1),
        "credited exactly once, at the destination, under the new id"
    );

    ledger.apply(
        5,
        &Delta::Deleted {
            mailbox_id: 1,
            message_id: 2,
        },
    );
    assert_eq!(
        ledger.get(1),
        Estimate::Estimated(0),
        "after deleting the last unread one"
    );
}

/// Test-only convenience: every seq in these replay tests is small and
/// derived from a `usize` step counter.
fn u64_to_seq(step: usize) -> i64 {
    i64::try_from(step).unwrap()
}

// ---- `reset`: what an account switch keeps and what it throws away ----

#[test]
fn resetting_clears_every_folder_but_preserves_last_seq() {
    // `reset` is what `use_account` calls instead of `Ledger::default()` —
    // see `ledger.rs`'s own module doc for why throwing `last_seq` away on
    // every account switch would rearm the floor at zero for no reason.
    let mut ledger = Ledger::default();
    ledger.seed(1, &[row(1, false)]);
    ledger.apply(
        9,
        &Delta::Arrived {
            mailbox_id: 1,
            message_id: 2,
        },
    );
    assert_eq!(ledger.get(1), Estimate::Estimated(2));

    ledger.reset();
    assert_eq!(
        ledger.get(1),
        Estimate::Unknown,
        "every folder goes back to unvisited"
    );

    // A different folder, seeded fresh after the reset — standing in for a
    // different account's own folder, reached through the same shared
    // `Ledger`. Its floor comes from the seq `reset` preserved, not zero.
    ledger.seed(5, &[row(1, true)]);
    ledger.apply(
        9,
        &Delta::Arrived {
            mailbox_id: 5,
            message_id: 2,
        },
    );
    assert_eq!(
        ledger.get(5),
        Estimate::Estimated(0),
        "seq 9 predates the reset, which preserved it as this folder's own floor"
    );

    // Genuinely new activity still applies normally.
    ledger.apply(
        10,
        &Delta::Arrived {
            mailbox_id: 5,
            message_id: 3,
        },
    );
    assert_eq!(ledger.get(5), Estimate::Estimated(1));
}

// ---- The documented, bounded gap: a first subscription's own replay ----

#[test]
fn a_replayed_arrival_for_a_message_the_seed_already_knows_about_no_longer_corrupts_it() {
    // What closes the more damaging half of the "stale replay" gap: a
    // `since_seq: 0` backlog replay redelivering `Arrived` for a message
    // the seed already has an opinion about — here, one it correctly
    // recorded as *read* — no longer flips that record back to unread.
    // `apply`'s own doc explains the invariant this leans on: a second
    // `Arrived` for a `message_id` already in `known` can only be a
    // replay, never a distinct new message, so an existing record (read or
    // unread) is trusted over the arrival assumption. Before this fix this
    // exact sequence spiked the estimate on every first subscription of a
    // session, floor of `0` or not.
    let mut ledger = Ledger::default();
    ledger.seed(1, &[row(1, true)]);
    assert_eq!(ledger.get(1), Estimate::Estimated(0), "seeded as read");

    // A "replayed" `Arrived` for that same, already-read message. `seq` is
    // deliberately small (`1`) to show even the very next event after
    // construction — which a floor of `0` cannot reject — still does not
    // corrupt it.
    ledger.apply(
        1,
        &Delta::Arrived {
            mailbox_id: 1,
            message_id: 1,
        },
    );
    assert_eq!(
        ledger.get(1),
        Estimate::Estimated(0),
        "an existing record is trusted over a redelivered arrival"
    );
}

#[test]
fn a_replayed_arrival_for_a_message_outside_the_seeded_page_is_still_assumed_unread() {
    // Pins the residual gap that remains after the fix above: `apply`'s
    // "already known" guard only protects a `message_id` the seed (or an
    // earlier delta) actually recorded. A backlog replay redelivering
    // `Arrived` for a message this ledger has *no* record of at all —
    // because it sits outside the loaded page, exactly like the message
    // this test never includes in `seed`'s own rows — is indistinguishable
    // from a genuinely new arrival and is still credited. This test is not
    // asserting *desired* behavior; it pins a known, bounded,
    // self-correcting limitation (see `ledger.rs`'s own module doc, "what
    // this estimate does not cover") so a future change that closes it (or
    // makes it worse) is visible here rather than discovered in the field.
    let mut ledger = Ledger::default();
    ledger.seed(1, &[row(1, true)]);
    assert_eq!(ledger.get(1), Estimate::Estimated(0), "seeded as read");

    // A "replayed" `Arrived` for message 2 — never part of the seeded
    // page, so `known` has no record of it either way.
    ledger.apply(
        1,
        &Delta::Arrived {
            mailbox_id: 1,
            message_id: 2,
        },
    );
    assert_eq!(
        ledger.get(1),
        Estimate::Estimated(1),
        "no record to trust, so the arrival assumption still applies — the documented gap"
    );

    // Self-correction: the open folder's own next `Msg::Changed` reseed
    // (simulated here as a direct second `seed` call) overwrites the
    // guess with the real, current state.
    ledger.seed(1, &[row(1, true)]);
    assert_eq!(
        ledger.get(1),
        Estimate::Estimated(0),
        "a reseed heals it, same as the backlog-walk case"
    );
}
