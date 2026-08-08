//! Follow-up reminders: arming, firing, and the reply that cancels one.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::outbox::tests::Fixture;
use crate::ErrorReason;

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn new(fixture: &Fixture, message_id: &str, remind_at: i64) -> NewFollowup {
    NewFollowup {
        account_id: fixture.account_id,
        thread_id: None,
        message_id: message_id.to_owned(),
        remind_at,
        tz: "UTC".to_owned(),
        cancel_on_reply: true,
        note: Some("chase the quote".to_owned()),
    }
}

#[tokio::test]
async fn a_reminder_round_trips_and_its_id_is_stored_bare() {
    let fixture = Fixture::open_named("followup");
    let store = FollowupStore::new(fixture.db.clone());
    // Angle brackets are what a user copies out of a header; storing them
    // would mean the reply that should dismiss this never matches, because
    // `messages.message_id` holds the bare form.
    let followup = store
        .create(new(&fixture, "<abc@example.com>", now() + 3_600))
        .await
        .unwrap();
    assert_eq!(followup.message_id, "abc@example.com");
    assert_eq!(followup.state, FollowupState::Armed);
    assert_eq!(store.get(followup.id).await.unwrap(), followup);
}

#[tokio::test]
async fn an_empty_message_id_or_an_unknown_account_is_refused() {
    let fixture = Fixture::open_named("followup-bad");
    let store = FollowupStore::new(fixture.db.clone());
    assert_eq!(
        store
            .create(new(&fixture, "  ", now()))
            .await
            .unwrap_err()
            .reason(),
        ErrorReason::InvalidArgument
    );
    let mut ghost = new(&fixture, "abc@example.com", now());
    ghost.account_id = 9_999;
    assert_eq!(
        store.create(ghost).await.unwrap_err().reason(),
        ErrorReason::NotFound
    );
    let mut wordy = new(&fixture, "abc@example.com", now());
    wordy.note = Some("x".repeat(MAX_NOTE + 1));
    assert_eq!(
        store.create(wordy).await.unwrap_err().reason(),
        ErrorReason::InvalidArgument
    );
}

#[tokio::test]
async fn a_due_reminder_fires_once() {
    let fixture = Fixture::open_named("followup-fire");
    let store = FollowupStore::new(fixture.db.clone());
    let due = store
        .create(new(&fixture, "due@example.com", now() - 1))
        .await
        .unwrap();
    store
        .create(new(&fixture, "later@example.com", now() + 86_400))
        .await
        .unwrap();

    let fired = store.sweep(now()).await.unwrap();
    assert_eq!(fired.len(), 1);
    assert_eq!(fired[0].id, due.id);
    assert_eq!(store.get(due.id).await.unwrap().state, FollowupState::Fired);
    // A fired reminder is not raised again on the next tick.
    assert!(store.sweep(now()).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_reply_dismisses_the_reminder_instead_of_nudging() {
    let fixture = Fixture::open_named("followup-reply");
    let store = FollowupStore::new(fixture.db.clone());
    let followup = store
        .create(new(&fixture, "asked@example.com", now() - 1))
        .await
        .unwrap();
    // Somebody answered.
    fixture.message("reply@example.com", Some("asked@example.com"));

    assert!(store.sweep(now()).await.unwrap().is_empty());
    assert_eq!(
        store.get(followup.id).await.unwrap().state,
        FollowupState::Dismissed
    );
}

#[tokio::test]
async fn a_reply_naming_a_different_message_does_not_dismiss_it() {
    // The join is on the whole id, not a substring of one: `instr` over a
    // space-padded copy, because `LIKE` would treat a `_` inside a real
    // Message-ID as a wildcard and match the wrong thread.
    let fixture = Fixture::open_named("followup-nomatch");
    let store = FollowupStore::new(fixture.db.clone());
    let followup = store
        .create(new(&fixture, "a_b@example.com", now() - 1))
        .await
        .unwrap();
    fixture.message("reply@example.com", Some("axb@example.com"));
    fixture.message("reply2@example.com", Some("prefix-a_b@example.com"));

    let fired = store.sweep(now()).await.unwrap();
    assert_eq!(fired.len(), 1);
    assert_eq!(fired[0].id, followup.id);
}

#[tokio::test]
async fn cancel_on_reply_off_fires_anyway() {
    let fixture = Fixture::open_named("followup-nocancel");
    let store = FollowupStore::new(fixture.db.clone());
    let mut insistent = new(&fixture, "asked@example.com", now() - 1);
    insistent.cancel_on_reply = false;
    let followup = store.create(insistent).await.unwrap();
    fixture.message("reply@example.com", Some("asked@example.com"));

    let fired = store.sweep(now()).await.unwrap();
    assert_eq!(fired.len(), 1);
    assert_eq!(fired[0].id, followup.id);
}

#[tokio::test]
async fn dismissing_is_idempotent_and_beats_a_sweep() {
    let fixture = Fixture::open_named("followup-dismiss");
    let store = FollowupStore::new(fixture.db.clone());
    let followup = store
        .create(new(&fixture, "asked@example.com", now() - 1))
        .await
        .unwrap();

    assert_eq!(
        store.dismiss(followup.id).await.unwrap().state,
        FollowupState::Dismissed
    );
    assert_eq!(
        store.dismiss(followup.id).await.unwrap().state,
        FollowupState::Dismissed
    );
    assert!(
        store.sweep(now()).await.unwrap().is_empty(),
        "a dismissed reminder must not nudge"
    );
    assert_eq!(
        store.dismiss(9_999).await.unwrap_err().reason(),
        ErrorReason::NotFound
    );
}

#[tokio::test]
async fn listing_filters_by_state_and_account() {
    let fixture = Fixture::open_named("followup-list");
    let store = FollowupStore::new(fixture.db.clone());
    let armed = store
        .create(new(&fixture, "a@example.com", now() + 3_600))
        .await
        .unwrap();
    let dismissed = store
        .create(new(&fixture, "b@example.com", now() + 3_600))
        .await
        .unwrap();
    store.dismiss(dismissed.id).await.unwrap();

    assert_eq!(store.list(None, None, 0).await.unwrap().len(), 2);
    let only_armed = store
        .list(Some(fixture.account_id), Some(FollowupState::Armed), 0)
        .await
        .unwrap();
    assert_eq!(only_armed.len(), 1);
    assert_eq!(only_armed[0].id, armed.id);
    assert!(store
        .list(Some(fixture.account_id + 1), None, 0)
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        store
            .list(None, None, MAX_LIST_LIMIT + 100)
            .await
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn every_state_round_trips_through_its_wire_string() {
    for state in FollowupState::ALL {
        assert_eq!(FollowupState::parse(state.as_str()).unwrap(), state);
    }
    assert_eq!(
        FollowupState::parse("wat").unwrap_err().reason(),
        ErrorReason::Internal
    );
}
