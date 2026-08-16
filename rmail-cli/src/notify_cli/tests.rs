//! What `mail notify` owes at the formatting layer: the enum names it prints
//! come from the generated proto (never a second hand-written table that
//! could drift), and the summary line degrades sensibly when the daemon
//! withheld a field.

use super::*;

#[test]
fn tier_names_come_from_the_generated_enum() {
    assert_eq!(tier_name(NotificationTier::Low as i32), "low");
    assert_eq!(tier_name(NotificationTier::Normal as i32), "normal");
    assert_eq!(tier_name(NotificationTier::High as i32), "high");
    assert_eq!(tier_name(NotificationTier::Critical as i32), "critical");
}

/// A tier this build does not know is reported as unknown, not silently
/// rendered as the zero value — a newer daemon must not be able to make an
/// older client print a confident wrong answer.
#[test]
fn an_unknown_tier_is_reported_as_unknown() {
    assert_eq!(tier_name(9999), "unknown(9999)");
}

#[test]
fn state_names_come_from_the_generated_enum() {
    assert_eq!(state_name(NotificationState::Pending as i32), "pending");
    assert_eq!(state_name(NotificationState::Delivered as i32), "delivered");
    assert_eq!(
        state_name(NotificationState::Suppressed as i32),
        "suppressed"
    );
    assert_eq!(state_name(NotificationState::Failed as i32), "failed");
    assert_eq!(state_name(NotificationState::Queued as i32), "queued");
    assert_eq!(state_name(-3), "unknown(-3)");
}

#[test]
fn the_summary_line_degrades_when_a_field_was_withheld() {
    assert_eq!(
        summary_line(
            Some("Invoice due"),
            Some("Ada <ada@example.com>"),
            "past due"
        ),
        "Ada <ada@example.com>: Invoice due — past due"
    );
    assert_eq!(
        summary_line(None, Some("Ada <ada@example.com>"), "past due"),
        "Ada <ada@example.com> — past due"
    );
    assert_eq!(
        summary_line(Some("Invoice due"), None, "past due"),
        "Invoice due — past due"
    );
    assert_eq!(summary_line(None, None, "past due"), "past due");
}
