//! The rules, without a database.
//!
//! The AI-window cases are the point of this file: they are a safety property,
//! and a safety property with no test is a comment.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::time::Duration;

use super::*;
use crate::config::{HumanDuration, SendConfig};

const NOW: i64 = 1_700_000_000;

fn policy(undo: Duration, ai_confirm: bool) -> SendPolicy {
    SendPolicy::from_config(&SendConfig {
        undo_window: HumanDuration::new(undo),
        ai_requires_confirmation: ai_confirm,
        ..SendConfig::default()
    })
}

#[test]
fn an_immediate_send_is_really_a_schedule_at_now_plus_the_undo_window() {
    let policy = policy(Duration::from_secs(10), true);
    let resolved = policy.resolve(Origin::User, None, None, NOW);
    assert_eq!(resolved.send_at, NOW + 10);
    assert_eq!(resolved.undo_deadline, Some(NOW + 10));
}

#[test]
fn a_zero_undo_window_makes_a_user_send_truly_immediate() {
    // prd.md: "window 0 = true immediate". For a human, that is a choice they
    // are entitled to make.
    let policy = policy(Duration::ZERO, true);
    let resolved = policy.resolve(Origin::User, None, None, NOW);
    assert_eq!(resolved.send_at, NOW);
    assert_eq!(resolved.undo_deadline, None);
}

#[test]
fn a_future_schedule_gets_no_countdown() {
    let policy = policy(Duration::from_secs(10), true);
    let resolved = policy.resolve(Origin::User, Some(NOW + 3600), None, NOW);
    assert_eq!(resolved.send_at, NOW + 3600);
    assert_eq!(
        resolved.undo_deadline, None,
        "a message scheduled for later is cancelable until it fires; a countdown \
         would be a second, shorter deadline that means nothing"
    );
}

#[test]
fn an_explicit_undo_window_can_lengthen_but_the_caller_chooses() {
    let policy = policy(Duration::from_secs(10), true);
    let resolved = policy.resolve(Origin::User, None, Some(Duration::from_secs(60)), NOW);
    assert_eq!(resolved.send_at, NOW + 60);
}

// ---------------------------------------------------------------------------
// The safety property
// ---------------------------------------------------------------------------

#[test]
fn an_ai_send_cannot_be_configured_out_of_its_undo_window() {
    // Every way there is to ask for "no window", against every configuration
    // that might be thought to grant it.
    for ai_confirm in [true, false] {
        let policy = policy(Duration::ZERO, ai_confirm);
        let floor = MIN_AI_UNDO_WINDOW.as_secs() as i64;

        // ... by asking for a zero window explicitly.
        let explicit = policy.resolve(Origin::Ai, None, Some(Duration::ZERO), NOW);
        assert_eq!(
            explicit.send_at,
            NOW + floor,
            "ai_requires_confirmation = {ai_confirm}: an explicit zero window"
        );
        assert_eq!(explicit.undo_deadline, Some(NOW + floor));

        // ... by letting a zero-configured window apply.
        let implicit = policy.resolve(Origin::Ai, None, None, NOW);
        assert_eq!(implicit.send_at, NOW + floor);

        // ... and by naming `send_at = now`, which is the same bypass wearing
        // a different hat.
        let scheduled_now = policy.resolve(Origin::Ai, Some(NOW), None, NOW);
        assert_eq!(
            scheduled_now.send_at,
            NOW + floor,
            "ai_requires_confirmation = {ai_confirm}: send_at = now"
        );
        // A send_at in the past is the same trick again.
        let backdated = policy.resolve(Origin::Ai, Some(NOW - 86_400), None, NOW);
        assert_eq!(backdated.send_at, NOW + floor);
    }
}

#[test]
fn ai_confirmation_lengthens_the_floor_to_the_configured_window() {
    // With confirmation on, the floor is the *configured* window, not the
    // bare minimum: an operator who set a 60-second undo window meant it to
    // apply to the sends they are least able to supervise.
    let strict = policy(Duration::from_secs(60), true);
    assert_eq!(
        strict.mandatory_undo_window(Origin::Ai),
        Duration::from_secs(60)
    );
    let relaxed = policy(Duration::from_secs(60), false);
    assert_eq!(
        relaxed.mandatory_undo_window(Origin::Ai),
        MIN_AI_UNDO_WINDOW
    );
}

#[test]
fn a_genuine_future_ai_schedule_is_left_where_the_caller_put_it() {
    // The floor is a floor, not an offset — "send this at 9am tomorrow" from
    // an AI must not become "9am tomorrow plus ten seconds".
    let policy = policy(Duration::from_secs(10), true);
    let resolved = policy.resolve(Origin::Ai, Some(NOW + 86_400), None, NOW);
    assert_eq!(resolved.send_at, NOW + 86_400);
    assert_eq!(resolved.undo_deadline, None);
}

#[test]
fn only_ai_carries_a_mandatory_window() {
    let policy = policy(Duration::from_secs(10), true);
    for origin in [Origin::User, Origin::Followup, Origin::Undo] {
        assert_eq!(policy.mandatory_undo_window(origin), Duration::ZERO);
    }
    assert!(policy.mandatory_undo_window(Origin::Ai) > Duration::ZERO);
}

// ---------------------------------------------------------------------------
// Backoff and lateness
// ---------------------------------------------------------------------------

#[test]
fn backoff_doubles_and_is_capped() {
    let policy = SendPolicy::from_config(&SendConfig {
        backoff_base: HumanDuration::new(Duration::from_secs(30)),
        backoff_max: HumanDuration::new(Duration::from_secs(300)),
        ..SendConfig::default()
    });
    assert_eq!(policy.backoff_for(1), Duration::from_secs(30));
    assert_eq!(policy.backoff_for(2), Duration::from_secs(60));
    assert_eq!(policy.backoff_for(3), Duration::from_secs(120));
    assert_eq!(policy.backoff_for(4), Duration::from_secs(240));
    assert_eq!(policy.backoff_for(5), Duration::from_secs(300));
    // Saturating rather than overflowing: a corrupt attempt count must not
    // panic the sender.
    assert_eq!(policy.backoff_for(i64::MAX), Duration::from_secs(300));
    assert_eq!(policy.backoff_for(0), Duration::from_secs(30));
}

#[test]
fn lateness_is_measured_against_the_tolerance() {
    let policy = SendPolicy::from_config(&SendConfig {
        late_tolerance: HumanDuration::new(Duration::from_secs(600)),
        ..SendConfig::default()
    });
    assert!(!policy.is_late(NOW, NOW));
    assert!(!policy.is_late(NOW - 600, NOW));
    assert!(policy.is_late(NOW - 601, NOW));
    // A send_at in the future (a clock that moved backwards) is not late.
    assert!(!policy.is_late(NOW + 3600, NOW));
}

#[test]
fn a_zero_worker_pool_is_coerced_up_rather_than_stopping_the_daemon() {
    let policy = SendPolicy::from_config(&SendConfig {
        workers: 0,
        ..SendConfig::default()
    });
    assert_eq!(policy.workers(), 1);
    assert_eq!(SendPolicy::default().workers(), 2, "prd.md's default");
}
