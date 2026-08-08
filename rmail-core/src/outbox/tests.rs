//! What the outbox owes at the storage layer.
//!
//! The happy path here is two lines; nearly everything below is one of the two
//! irreversible failures. In order of how much they would cost a user:
//!
//! - a crash between committing the fence and `DATA` must not deliver twice,
//! - a lease that lapses must return the row to somebody,
//! - a cancel racing the sender must resolve one way, not both,
//! - a message that came due while rmail was off must still go out.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use super::*;
use crate::compose::mime;
use crate::repo;
use crate::ErrorReason;

static COUNTER: AtomicU32 = AtomicU32::new(0);

pub(super) struct Fixture {
    pub(super) db: Database,
    pub(super) account_id: i64,
    pub(super) mailbox_id: i64,
    next_uid: std::sync::atomic::AtomicI64,
    path: PathBuf,
}

impl Fixture {
    pub(super) fn open() -> Self {
        Self::open_named("outbox")
    }

    pub(super) fn open_named(tag: &str) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-{tag}-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", path.display())));
        }
        let db = Database::open(&path).unwrap();
        let (account_id, mailbox_id) = db
            .with_write(|c| {
                let account_id = repo::insert_account(
                    c,
                    &repo::NewAccount {
                        name: "Personal".to_owned(),
                        smtp_server: Some("127.0.0.1".to_owned()),
                        smtp_port: Some(2525),
                        username: Some("alice@example.com".to_owned()),
                        ..Default::default()
                    },
                )?;
                let mailbox_id = repo::insert_mailbox(
                    c,
                    &repo::NewMailbox {
                        account_id,
                        name: "Sent".to_owned(),
                        ..Default::default()
                    },
                )?;
                Ok((account_id, mailbox_id))
            })
            .unwrap();
        Self {
            db,
            account_id,
            mailbox_id,
            next_uid: std::sync::atomic::AtomicI64::new(1),
            path,
        }
    }

    pub(super) fn store(&self) -> OutboxStore {
        OutboxStore::new(self.db.clone())
    }

    /// Point the account's SMTP config at a mock server's ephemeral port.
    pub(super) fn set_smtp_port(&self, port: u16) {
        let account_id = self.account_id;
        self.db
            .with_write(move |c| {
                c.execute(
                    "UPDATE accounts SET smtp_port = ?2 WHERE id = ?1",
                    rusqlite::params![account_id, i64::from(port)],
                )
            })
            .unwrap();
    }

    /// Insert a locally-synced message with the given threading headers.
    pub(super) fn message(&self, message_id: &str, in_reply_to: Option<&str>) {
        let uid = self.next_uid.fetch_add(1, Ordering::Relaxed);
        let (account_id, mailbox_id) = (self.account_id, self.mailbox_id);
        let (message_id, in_reply_to) = (message_id.to_owned(), in_reply_to.map(str::to_owned));
        self.db
            .with_write(move |c| {
                c.execute(
                    "INSERT INTO messages
                         (account_id, mailbox_id, uid, uidvalidity, message_id, in_reply_to,
                          subject)
                     VALUES (?1, ?2, ?3, 1, ?4, ?5, 'Re: something')",
                    rusqlite::params![account_id, mailbox_id, uid, message_id, in_reply_to],
                )
            })
            .unwrap();
    }

    /// Render a real message through `compose::mime`, so every test operates
    /// on the same octets production would.
    pub(super) fn rendered(&self, subject: &str) -> (Vec<u8>, String) {
        let draft = inline_draft(InlineMessage {
            account_id: self.account_id,
            from: Mailbox::new("alice@example.com", Some("Alice")).unwrap(),
            to: vec![Mailbox::new("bob@example.com", None).unwrap()],
            cc: Vec::new(),
            bcc: vec![Mailbox::new("blind@example.com", None).unwrap()],
            subject: subject.to_owned(),
            body_text: "hello there".to_owned(),
            in_reply_to: None,
            references: Vec::new(),
        })
        .unwrap();
        let envelope = mime::Envelope::now(&draft);
        let mime = mime::build(&draft, &envelope).unwrap();
        (mime, envelope.message_id().to_owned())
    }

    pub(super) fn new_send(&self, subject: &str, send_at: i64) -> NewSend {
        let (raw_mime, _) = self.rendered(subject);
        NewSend {
            account_id: self.account_id,
            draft_id: None,
            from_addr: "alice@example.com".to_owned(),
            to: vec!["bob@example.com".to_owned()],
            cc: Vec::new(),
            bcc: vec!["blind@example.com".to_owned()],
            subject: subject.to_owned(),
            raw_mime,
            body_preview: "hello there".to_owned(),
            in_reply_to: None,
            thread_id: None,
            send_at,
            tz: "UTC".to_owned(),
            origin: Origin::User,
            undo_deadline: None,
            max_retries: 3,
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.path.display())));
        }
    }
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

// ---------------------------------------------------------------------------
// Round trip
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_scheduled_message_round_trips_with_its_blind_recipients() {
    let fixture = Fixture::open();
    let store = fixture.store();
    let entry = store
        .schedule(fixture.new_send("Lunch", now() + 3600))
        .await
        .unwrap();

    assert_eq!(entry.state, OutboxState::Scheduled);
    assert_eq!(entry.to, ["bob@example.com"]);
    assert_eq!(entry.bcc, ["blind@example.com"]);
    // The envelope carries the blind recipient; the message must not.
    assert_eq!(
        entry.envelope_recipients(),
        ["bob@example.com", "blind@example.com"]
    );
    let raw = String::from_utf8(store.raw_mime(entry.id).await.unwrap()).unwrap();
    assert!(
        !raw.to_ascii_lowercase().contains("\nbcc:"),
        "the stored octets must carry no Bcc header:\n{raw}"
    );
    assert_eq!(store.get(entry.id).await.unwrap(), entry);
}

#[tokio::test]
async fn a_send_with_no_recipient_or_no_message_id_is_refused() {
    let fixture = Fixture::open();
    let store = fixture.store();

    let mut empty = fixture.new_send("Nobody", now());
    empty.to.clear();
    empty.bcc.clear();
    assert_eq!(
        store.schedule(empty).await.unwrap_err().reason(),
        ErrorReason::InvalidArgument
    );

    // Without a Message-ID the at-most-once fence has nothing to write, so a
    // crash mid-send could deliver twice. Caught at the only moment a caller
    // can still fix it.
    let mut headerless = fixture.new_send("Headerless", now());
    headerless.raw_mime = b"Subject: no id\r\n\r\nbody\r\n".to_vec();
    assert_eq!(
        store.schedule(headerless).await.unwrap_err().reason(),
        ErrorReason::InvalidArgument
    );
}

#[tokio::test]
async fn scheduling_for_an_unknown_account_is_not_found() {
    let fixture = Fixture::open();
    let store = fixture.store();
    let mut send = fixture.new_send("Ghost", now());
    send.account_id = 9_999;
    assert_eq!(
        store.schedule(send).await.unwrap_err().reason(),
        ErrorReason::NotFound
    );
}

// ---------------------------------------------------------------------------
// At-most-once
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_claim_that_finds_a_committed_message_id_does_not_transmit_again() {
    // The single most important property in this module. Simulated at the
    // storage layer here (the end-to-end version, with a real SMTP server
    // counting deliveries, lives in `scheduler::tests`).
    let fixture = Fixture::open();
    let store = fixture.store();
    let entry = store
        .schedule(fixture.new_send("Once", now() - 1))
        .await
        .unwrap();

    // Attempt one: claim, commit the fence, then vanish.
    let claim = store
        .claim_due("worker-a", 10, now(), Duration::from_secs(1))
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert!(claim.committed_message_id.is_none());
    assert!(store.begin_transmit(&claim).await.unwrap());

    // The lease lapses and the reaper returns the row — with the fence.
    assert_eq!(store.reap_expired(now() + 5).await.unwrap(), 1);
    let after_reap = store.get(entry.id).await.unwrap();
    assert_eq!(after_reap.state, OutboxState::Scheduled);
    assert_eq!(
        after_reap.smtp_message_id.as_deref(),
        Some(claim.message_id.as_str()),
        "the fence must survive the reap, or the retry has no way to know"
    );

    // Attempt two sees the fence and closes the row out without sending.
    let retry = store
        .claim_due("worker-b", 10, now() + 5, Duration::from_secs(60))
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(
        retry.committed_message_id.as_deref(),
        Some(claim.message_id.as_str())
    );
    assert!(store.mark_recovered(&retry).await.unwrap());

    let final_entry = store.get(entry.id).await.unwrap();
    assert_eq!(final_entry.state, OutboxState::Sent);
    assert_eq!(final_entry.last_error.as_deref(), Some(RECOVERED_NOTE));
}

#[tokio::test]
async fn a_returned_failure_clears_the_fence_so_the_retry_really_retries() {
    // The other half of the fence's contract. A returned SMTP error proves
    // the peer queued nothing, so keeping the fence would silently convert
    // every transient failure into a lost message.
    let fixture = Fixture::open();
    let store = fixture.store();
    let entry = store
        .schedule(fixture.new_send("Retry me", now() - 1))
        .await
        .unwrap();

    let claim = store
        .claim_due("worker", 10, now(), Duration::from_secs(60))
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert!(store.begin_transmit(&claim).await.unwrap());
    let outcome = store
        .mark_transient_failure(&claim, "451 try later", Duration::ZERO, now())
        .await
        .unwrap();
    assert!(matches!(outcome, Some(RetryOutcome::Retrying { .. })));

    let after = store.get(entry.id).await.unwrap();
    assert_eq!(after.state, OutboxState::Scheduled);
    assert_eq!(after.smtp_message_id, None);

    let again = store
        .claim_due("worker", 10, now(), Duration::from_secs(60))
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(again.committed_message_id, None, "the retry must transmit");
}

#[tokio::test]
async fn two_rows_cannot_claim_the_same_message_id() {
    // The fence is only a fence if the database enforces uniqueness — two
    // rows sharing a Message-ID would protect neither.
    let fixture = Fixture::open();
    let store = fixture.store();
    let first = store
        .schedule(fixture.new_send("A", now() - 1))
        .await
        .unwrap();
    let second_send = NewSend {
        raw_mime: store.raw_mime(first.id).await.unwrap(),
        ..fixture.new_send("B", now() - 1)
    };
    let second = store.schedule(second_send).await.unwrap();

    let claims = store
        .claim_due("worker", 10, now(), Duration::from_secs(60))
        .await
        .unwrap();
    let first_claim = claims.iter().find(|c| c.id == first.id).unwrap();
    let second_claim = claims.iter().find(|c| c.id == second.id).unwrap();
    assert!(store.begin_transmit(first_claim).await.unwrap());
    assert!(
        store.begin_transmit(second_claim).await.is_err(),
        "a second row must not be able to commit the same Message-ID"
    );
}

// ---------------------------------------------------------------------------
// Leases
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_lapsed_lease_returns_the_row_and_a_stale_worker_cannot_complete_it() {
    let fixture = Fixture::open();
    let store = fixture.store();
    let entry = store
        .schedule(fixture.new_send("Stranded", now() - 1))
        .await
        .unwrap();

    let stale = store
        .claim_due("worker-a", 10, now(), Duration::from_secs(1))
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(store.reap_expired(now() + 5).await.unwrap(), 1);
    let fresh = store
        .claim_due("worker-b", 10, now() + 5, Duration::from_secs(60))
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(fresh.id, entry.id);

    // The old worker waking up late must not write over the new owner's row.
    assert!(!store.mark_sent(&stale, false).await.unwrap());
    assert_eq!(
        store.get(entry.id).await.unwrap().state,
        OutboxState::Sending
    );
    assert!(store.mark_sent(&fresh, false).await.unwrap());
    assert_eq!(store.get(entry.id).await.unwrap().state, OutboxState::Sent);
}

#[tokio::test]
async fn a_claim_takes_each_row_exactly_once() {
    let fixture = Fixture::open();
    let store = fixture.store();
    for i in 0..3 {
        store
            .schedule(fixture.new_send(&format!("m{i}"), now() - 1))
            .await
            .unwrap();
    }
    let first = store
        .claim_due("worker", 10, now(), Duration::from_secs(60))
        .await
        .unwrap();
    let second = store
        .claim_due("worker", 10, now(), Duration::from_secs(60))
        .await
        .unwrap();
    assert_eq!(first.len(), 3);
    assert!(second.is_empty(), "a claimed row must not be claimed again");
}

// ---------------------------------------------------------------------------
// Failure classification
// ---------------------------------------------------------------------------

#[tokio::test]
async fn transient_failures_back_off_then_give_up_and_stay_given_up() {
    let fixture = Fixture::open();
    let store = fixture.store();
    // `max_retries` is 3 in the fixture.
    let entry = store
        .schedule(fixture.new_send("Flaky", now() - 1))
        .await
        .unwrap();

    for attempt in 1..=3 {
        let claim = store
            .claim_due("worker", 10, now(), Duration::from_secs(60))
            .await
            .unwrap()
            .pop()
            .unwrap_or_else(|| panic!("attempt {attempt} should have claimed the row"));
        assert_eq!(claim.attempts, attempt);
        let outcome = store
            .mark_transient_failure(&claim, "451 later", Duration::ZERO, now())
            .await
            .unwrap();
        match attempt {
            3 => assert_eq!(outcome, Some(RetryOutcome::Exhausted { attempts: 3 })),
            _ => assert!(matches!(outcome, Some(RetryOutcome::Retrying { .. }))),
        }
    }

    let failed = store.get(entry.id).await.unwrap();
    assert_eq!(failed.state, OutboxState::Failed);
    assert_eq!(failed.last_error.as_deref(), Some("451 later"));
    // And a failed row is never picked up again on its own.
    assert!(store
        .claim_due("worker", 10, now() + 10_000, Duration::from_secs(60))
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn a_backoff_is_respected_before_the_row_is_eligible_again() {
    let fixture = Fixture::open();
    let store = fixture.store();
    store
        .schedule(fixture.new_send("Backoff", now() - 1))
        .await
        .unwrap();
    let claim = store
        .claim_due("worker", 10, now(), Duration::from_secs(60))
        .await
        .unwrap()
        .pop()
        .unwrap();
    store
        .mark_transient_failure(&claim, "451", Duration::from_secs(300), now())
        .await
        .unwrap();

    assert!(store
        .claim_due("worker", 10, now(), Duration::from_secs(60))
        .await
        .unwrap()
        .is_empty());
    assert!(!store
        .claim_due("worker", 10, now() + 301, Duration::from_secs(60))
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn a_permanent_failure_fails_immediately_and_only_an_explicit_retry_revives_it() {
    let fixture = Fixture::open();
    let store = fixture.store();
    let entry = store
        .schedule(fixture.new_send("Rejected", now() - 1))
        .await
        .unwrap();
    let claim = store
        .claim_due("worker", 10, now(), Duration::from_secs(60))
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert!(store
        .mark_permanent_failure(&claim, "550 no such user")
        .await
        .unwrap());

    let failed = store.get(entry.id).await.unwrap();
    assert_eq!(failed.state, OutboxState::Failed);
    assert_eq!(failed.attempts, 1, "a 5xx must not burn the retry budget");
    assert!(store
        .claim_due("worker", 10, now() + 86_400, Duration::from_secs(60))
        .await
        .unwrap()
        .is_empty());

    let revived = store.retry(entry.id).await.unwrap();
    assert_eq!(revived.state, OutboxState::Scheduled);
    assert_eq!(revived.attempts, 0);
    assert_eq!(revived.last_error, None);
    assert!(!store
        .claim_due("worker", 10, now(), Duration::from_secs(60))
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn retrying_something_that_did_not_fail_is_refused() {
    let fixture = Fixture::open();
    let store = fixture.store();
    let entry = store
        .schedule(fixture.new_send("Fine", now() + 600))
        .await
        .unwrap();
    assert_eq!(
        store.retry(entry.id).await.unwrap_err().reason(),
        ErrorReason::FailedPrecondition
    );
}

// ---------------------------------------------------------------------------
// Cancel / undo
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cancel_and_the_sender_cannot_both_win() {
    let fixture = Fixture::open();
    let store = fixture.store();

    // Sender first: the cancel must lose, and say so with ALREADY_EXISTS
    // rather than silently succeeding on a message already in flight.
    let claimed_first = store
        .schedule(fixture.new_send("Racing A", now() - 1))
        .await
        .unwrap();
    let claim = store
        .claim_due("worker", 10, now(), Duration::from_secs(60))
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(claim.id, claimed_first.id);
    let error = store.cancel(claimed_first.id).await.unwrap_err();
    assert_eq!(error.reason(), ErrorReason::AlreadyExists);

    // Cancel first: the claim must find nothing.
    let cancelled_first = store
        .schedule(fixture.new_send("Racing B", now() - 1))
        .await
        .unwrap();
    assert_eq!(
        store.cancel(cancelled_first.id).await.unwrap().state,
        OutboxState::Canceled
    );
    assert!(store
        .claim_due("worker", 10, now(), Duration::from_secs(60))
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn cancelling_twice_is_not_an_error() {
    let fixture = Fixture::open();
    let store = fixture.store();
    let entry = store
        .schedule(fixture.new_send("Undo", now() + 10))
        .await
        .unwrap();
    store.cancel(entry.id).await.unwrap();
    assert_eq!(
        store.cancel(entry.id).await.unwrap().state,
        OutboxState::Canceled
    );
}

#[tokio::test]
async fn cancelling_a_sent_message_reports_already_sent() {
    let fixture = Fixture::open();
    let store = fixture.store();
    let entry = store
        .schedule(fixture.new_send("Gone", now() - 1))
        .await
        .unwrap();
    let claim = store
        .claim_due("worker", 10, now(), Duration::from_secs(60))
        .await
        .unwrap()
        .pop()
        .unwrap();
    store.mark_sent(&claim, false).await.unwrap();
    assert_eq!(
        store.cancel(entry.id).await.unwrap_err().reason(),
        ErrorReason::AlreadyExists
    );
}

#[tokio::test]
async fn a_bare_undo_picks_the_send_whose_countdown_is_running() {
    let fixture = Fixture::open();
    let store = fixture.store();
    store
        .schedule(fixture.new_send("Next week", now() + 604_800))
        .await
        .unwrap();
    let undoable = store
        .schedule(NewSend {
            undo_deadline: Some(now() + 10),
            ..fixture.new_send("Just sent", now() + 10)
        })
        .await
        .unwrap();

    let picked = store.newest_cancelable(None).await.unwrap();
    assert_eq!(picked.id, undoable.id);

    store.cancel(picked.id).await.unwrap();
    // With no window open it falls back to the newest scheduled row rather
    // than reporting nothing to undo.
    assert_eq!(
        store.newest_cancelable(None).await.unwrap().subject,
        "Next week"
    );
}

#[tokio::test]
async fn nothing_to_undo_is_not_found() {
    let fixture = Fixture::open();
    let store = fixture.store();
    assert_eq!(
        store.newest_cancelable(None).await.unwrap_err().reason(),
        ErrorReason::NotFound
    );
}

// ---------------------------------------------------------------------------
// Mutations
// ---------------------------------------------------------------------------

#[tokio::test]
async fn rescheduling_moves_the_instant_and_send_now_makes_it_due() {
    let fixture = Fixture::open();
    let store = fixture.store();
    let entry = store
        .schedule(fixture.new_send("Later", now() + 86_400))
        .await
        .unwrap();

    let moved = store
        .reschedule(entry.id, now() + 3600, "Europe/Berlin", 0)
        .await
        .unwrap();
    assert_eq!(moved.tz, "Europe/Berlin");
    assert!(store
        .claim_due("worker", 10, now(), Duration::from_secs(60))
        .await
        .unwrap()
        .is_empty());

    store.send_now(entry.id, 0).await.unwrap();
    assert!(!store
        .claim_due("worker", 10, now(), Duration::from_secs(60))
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn a_send_already_in_flight_can_no_longer_be_rescheduled_or_edited() {
    let fixture = Fixture::open();
    let store = fixture.store();
    let entry = store
        .schedule(fixture.new_send("Flying", now() - 1))
        .await
        .unwrap();
    store
        .claim_due("worker", 10, now(), Duration::from_secs(60))
        .await
        .unwrap();

    assert_eq!(
        store
            .reschedule(entry.id, now() + 60, "UTC", 0)
            .await
            .unwrap_err()
            .reason(),
        ErrorReason::AlreadyExists
    );
    assert_eq!(
        store
            .update_body(entry.id, "too late".to_owned())
            .await
            .unwrap_err()
            .reason(),
        ErrorReason::AlreadyExists
    );
}

#[tokio::test]
async fn editing_the_body_re_renders_from_the_draft_and_refuses_without_one() {
    let fixture = Fixture::open();
    let store = fixture.store();

    // No draft: refused rather than silently rebuilt from the outbox columns,
    // which would drop attachments and the HTML alternative.
    let inline = store
        .schedule(fixture.new_send("Inline", now() + 600))
        .await
        .unwrap();
    assert_eq!(
        store
            .update_body(inline.id, "new body".to_owned())
            .await
            .unwrap_err()
            .reason(),
        ErrorReason::FailedPrecondition
    );

    let draft = store
        .drafts()
        .create(crate::compose::NewDraft {
            account_id: fixture.account_id,
            from: Mailbox::new("alice@example.com", None).unwrap(),
            to: vec![Mailbox::new("bob@example.com", None).unwrap()],
            cc: Vec::new(),
            bcc: Vec::new(),
            subject: "From a draft".to_owned(),
            body_text: "first".to_owned(),
            body_html: None,
            attachments: Vec::new(),
            in_reply_to_message_id: None,
        })
        .await
        .unwrap();
    let rendered = store.drafts().render(draft.id).await.unwrap();
    let entry = store
        .schedule(NewSend {
            draft_id: Some(draft.id),
            raw_mime: rendered.mime,
            ..fixture.new_send("From a draft", now() + 600)
        })
        .await
        .unwrap();

    let edited = store
        .update_body(entry.id, "second".to_owned())
        .await
        .unwrap();
    assert_eq!(edited.body_preview, "second");
    let raw = String::from_utf8(store.raw_mime(entry.id).await.unwrap()).unwrap();
    assert!(raw.contains("second"), "the frozen octets must be replaced");
    assert!(!raw.contains("first"));
}

// ---------------------------------------------------------------------------
// Missed windows
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_message_that_came_due_while_rmail_was_off_still_goes_out() {
    // prd.md's three branches: on time, late-but-tolerated, and late enough
    // to say so. All three send; only the third is flagged.
    let fixture = Fixture::open();
    let store = fixture.store();
    let policy = SendPolicy::default(); // late_tolerance = 10m

    for (label, overdue_secs, expect_late) in [
        ("on time", 0i64, false),
        ("within tolerance", 300, false),
        ("was offline", 86_400, true),
    ] {
        let entry = store
            .schedule(fixture.new_send(label, now() - overdue_secs))
            .await
            .unwrap();
        let claim = store
            .claim_due("worker", 10, now(), Duration::from_secs(60))
            .await
            .unwrap()
            .pop()
            .unwrap_or_else(|| panic!("{label} should be claimable"));
        let late = policy.is_late(claim.send_at, now());
        assert_eq!(late, expect_late, "lateness for {label}");
        store.mark_sent(&claim, late).await.unwrap();

        let sent = store.get(entry.id).await.unwrap();
        assert_eq!(sent.state, OutboxState::Sent, "{label} must still be sent");
        assert_eq!(sent.sent_late, expect_late, "sent_late for {label}");
    }
}

// ---------------------------------------------------------------------------
// Sleeping, not polling
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_next_due_time_is_the_earliest_outstanding_row() {
    let fixture = Fixture::open();
    let store = fixture.store();
    assert_eq!(store.next_due_at().await.unwrap(), None);

    let far = now() + 86_400;
    store.schedule(fixture.new_send("Far", far)).await.unwrap();
    assert_eq!(store.next_due_at().await.unwrap(), Some(far));

    let near = now() + 5;
    store
        .schedule(fixture.new_send("Near", near))
        .await
        .unwrap();
    assert_eq!(
        store.next_due_at().await.unwrap(),
        Some(near),
        "an insert that is due sooner must move the wake-up"
    );

    // A backed-off row's next attempt is what counts, not its original time.
    let claim = store
        .claim_due("worker", 10, now() + 10, Duration::from_secs(60))
        .await
        .unwrap()
        .pop()
        .unwrap();
    store
        .mark_transient_failure(&claim, "451", Duration::from_secs(600), now())
        .await
        .unwrap();
    assert!(store.next_due_at().await.unwrap().unwrap() > near);
}

// ---------------------------------------------------------------------------
// Listing and fan-out
// ---------------------------------------------------------------------------

#[tokio::test]
async fn listing_filters_by_state_and_caps_its_page() {
    let fixture = Fixture::open();
    let store = fixture.store();
    let failed = store
        .schedule(fixture.new_send("Bad", now() - 1))
        .await
        .unwrap();
    store
        .schedule(fixture.new_send("Good", now() + 600))
        .await
        .unwrap();
    let claim = store
        .claim_due("worker", 10, now(), Duration::from_secs(60))
        .await
        .unwrap()
        .pop()
        .unwrap();
    store.mark_permanent_failure(&claim, "550").await.unwrap();

    let all = store.list(Some(fixture.account_id), None, 0).await.unwrap();
    assert_eq!(all.len(), 2);
    let only_failed = store
        .list(None, Some(OutboxState::Failed), 0)
        .await
        .unwrap();
    assert_eq!(only_failed.len(), 1);
    assert_eq!(only_failed[0].id, failed.id);
    assert!(store
        .list(Some(fixture.account_id + 1), None, 0)
        .await
        .unwrap()
        .is_empty());
    // Over-large pages are clamped, not rejected.
    assert_eq!(
        store
            .list(None, None, MAX_LIST_LIMIT + 1_000)
            .await
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn every_transition_reaches_a_watcher() {
    let fixture = Fixture::open();
    let store = fixture.store();
    let mut watch = store.watch();

    let entry = store
        .schedule(fixture.new_send("Watched", now() - 1))
        .await
        .unwrap();
    assert_eq!(
        watch.recv().await.unwrap().entry.state,
        OutboxState::Scheduled
    );

    let claim = store
        .claim_due("worker", 10, now(), Duration::from_secs(60))
        .await
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(
        watch.recv().await.unwrap().entry.state,
        OutboxState::Sending
    );

    store.mark_sent(&claim, false).await.unwrap();
    let sent = watch.recv().await.unwrap().entry;
    assert_eq!(sent.state, OutboxState::Sent);
    assert_eq!(sent.id, entry.id);
}

// ---------------------------------------------------------------------------
// Header scanning
// ---------------------------------------------------------------------------

#[test]
fn the_message_id_scanner_reads_only_the_header_block() {
    assert_eq!(
        message_id_of(b"Subject: hi\r\nMessage-ID: <abc@x>\r\n\r\nbody\r\n"),
        Some("abc@x".to_owned())
    );
    // Case-insensitive field name, and folded continuation lines join.
    assert_eq!(
        message_id_of(b"message-id:\r\n <folded@x>\r\n\r\nbody"),
        Some("folded@x".to_owned())
    );
    // A body line that looks like a header is body text.
    assert_eq!(
        message_id_of(b"Subject: hi\r\n\r\nMessage-ID: <fake@x>\r\n"),
        None
    );
    assert_eq!(message_id_of(b"Message-ID: <>\r\n\r\n"), None);
    assert_eq!(message_id_of(b""), None);
}

#[test]
fn every_state_and_origin_round_trips_through_its_wire_string() {
    for state in OutboxState::ALL {
        assert_eq!(OutboxState::parse(state.as_str()).unwrap(), state);
    }
    for origin in Origin::ALL {
        assert_eq!(Origin::parse(origin.as_str()).unwrap(), origin);
    }
    // Both are corrupt data. `Origin::parse` used to report
    // `InvalidArgument` on the premise that it also parsed
    // `ScheduleSendRequest.origin` -- but the shipped proto uses an enum and
    // the request boundary is `origin_from_proto`, so this only ever reads a
    // value some earlier version of this code wrote. Telling a caller their
    // request was invalid, when the request never carried this string, sends
    // them looking in the wrong place.
    assert_eq!(
        OutboxState::parse("wat").unwrap_err().reason(),
        ErrorReason::Internal
    );
    assert_eq!(
        Origin::parse("wat").unwrap_err().reason(),
        ErrorReason::Internal
    );
}

#[tokio::test]
async fn send_now_and_reschedule_cannot_strip_an_ai_undo_window() {
    // `ScheduleSend` already refused "send_at = now" and "undo_window = 0".
    // These two RPCs move the instant *after* the row exists, and did so with
    // no reference to `origin` at all -- so schedule-then-SendNow was the same
    // bypass in two calls instead of one. The floor is enforced in SQL from
    // the row's own origin, so this asserts against the stored row.
    let fixture = Fixture::open_named("ai-floor");
    let store = fixture.store();
    const FLOOR: i64 = 30;

    let ai = |subject: &str| {
        let mut send = fixture.new_send(subject, now() + 3600);
        send.origin = Origin::Ai;
        send
    };

    let entry = store.schedule(ai("AI: send now")).await.unwrap();
    let after = store.send_now(entry.id, FLOOR).await.unwrap();
    assert!(
        after.send_at >= now() + FLOOR - 2,
        "SendNow moved an AI send inside its mandatory undo window: send_at={} now={}",
        after.send_at,
        now()
    );
    assert!(
        after.undo_deadline.is_some(),
        "the window has to be visible, not merely implied by send_at"
    );

    // The same bypass through the other door: name an instant in the past.
    let entry = store
        .schedule(ai("AI: reschedule to the past"))
        .await
        .unwrap();
    let after = store
        .reschedule(entry.id, now() - 86_400, "UTC", FLOOR)
        .await
        .unwrap();
    assert!(
        after.send_at >= now() + FLOOR - 2,
        "RescheduleSend backdated an AI send past its undo window: send_at={} now={}",
        after.send_at,
        now()
    );

    // A human-originated send is untouched by any of this.
    let mut human = fixture.new_send("Mine, send it", now() + 3600);
    human.origin = Origin::User;
    let entry = store.schedule(human).await.unwrap();
    let after = store.send_now(entry.id, FLOOR).await.unwrap();
    assert!(
        after.send_at <= now() + 1,
        "a user's own SendNow must still be immediate"
    );
}
