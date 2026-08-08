//! The scheduler end to end, against a real SMTP server counting deliveries.
//!
//! The most important test in this file — and in the task — is
//! [`a_crash_between_the_fence_and_the_completion_does_not_deliver_twice`]. It
//! is the only place where "exactly one message reached the server" is a claim
//! about an actual server rather than about bookkeeping, and a duplicate-mail
//! bug is exactly the kind that every layer above the socket agrees was fine.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::config::{HumanDuration, SendConfig, SmtpSecurity};
use crate::events::{EventLog, Retention};
use crate::outbox::mock::{MockSmtp, MockSmtpConfig};
use crate::outbox::smtp::{LettreSender, SendEnvelope};
use crate::outbox::tests::Fixture;
use crate::outbox::{NewSend, Origin, OutboxState, SendPolicy};

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

fn events(fixture: &Fixture) -> EventLog {
    EventLog::new(fixture.db.clone(), Retention::default())
}

fn policy() -> SendPolicy {
    SendPolicy::from_config(&SendConfig {
        // Long enough that a test which asserts "the Notify woke it" cannot
        // pass by accident on the poll timer.
        poll_interval: HumanDuration::new(Duration::from_secs(3_600)),
        ..SendConfig::default()
    })
}

fn build_scheduler(
    fixture: &Fixture,
    store: OutboxStore,
    sender: Arc<dyn SmtpSender>,
    policy: SendPolicy,
    worker: &str,
) -> SendScheduler {
    SendScheduler::new(
        store,
        FollowupStore::new(fixture.db.clone()),
        sender,
        events(fixture),
        policy,
        worker,
    )
}

// ---------------------------------------------------------------------------
// Senders
// ---------------------------------------------------------------------------

/// Delivers for real, then never returns — the shape of a process that dies
/// between `DATA` and the write that records it.
#[derive(Debug)]
struct CrashingSender {
    inner: LettreSender,
    delivered: mpsc::UnboundedSender<()>,
}

#[async_trait::async_trait]
impl SmtpSender for CrashingSender {
    async fn send(
        &self,
        account_id: i64,
        envelope: &SendEnvelope,
        raw_mime: &[u8],
    ) -> Result<(), SendFailure> {
        self.inner.send(account_id, envelope, raw_mime).await?;
        let _ = self.delivered.send(());
        // The task driving this is aborted by the test the moment the signal
        // above arrives. Returning `Ok` instead would let `mark_sent` run,
        // which is the state the crash is defined by *not* reaching.
        std::future::pending::<()>().await;
        Ok(())
    }
}

/// Answers with a fixed failure, and counts how often it was asked.
#[derive(Debug)]
struct FailingSender {
    failure: SendFailure,
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl SmtpSender for FailingSender {
    async fn send(
        &self,
        _account_id: i64,
        _envelope: &SendEnvelope,
        _raw_mime: &[u8],
    ) -> Result<(), SendFailure> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        Err(self.failure.clone())
    }
}

/// Records what it was handed, and succeeds.
#[derive(Debug, Default)]
struct RecordingSender {
    sent: std::sync::Mutex<Vec<(SendEnvelope, Vec<u8>)>>,
}

#[async_trait::async_trait]
impl SmtpSender for RecordingSender {
    async fn send(
        &self,
        _account_id: i64,
        envelope: &SendEnvelope,
        raw_mime: &[u8],
    ) -> Result<(), SendFailure> {
        self.sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push((envelope.clone(), raw_mime.to_vec()));
        Ok(())
    }
}

/// Records what it was asked to file in `Sent`.
#[derive(Debug, Default)]
struct RecordingAppender {
    filed: std::sync::Mutex<Vec<Vec<u8>>>,
}

#[async_trait::async_trait]
impl SentAppender for RecordingAppender {
    async fn append_to_sent(&self, _account_id: i64, raw_mime: &[u8]) -> Result<(), crate::Error> {
        self.filed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(raw_mime.to_vec());
        Ok(())
    }
}

/// Always refuses, so the "a failed append does not fail the send" case has
/// something to fail with.
#[derive(Debug)]
struct BrokenAppender;

#[async_trait::async_trait]
impl SentAppender for BrokenAppender {
    async fn append_to_sent(&self, _account_id: i64, _raw_mime: &[u8]) -> Result<(), crate::Error> {
        Err(crate::Error::unavailable("IMAP is down"))
    }
}

// ---------------------------------------------------------------------------
// At-most-once, end to end
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_crash_between_the_fence_and_the_completion_does_not_deliver_twice() {
    let fixture = Fixture::open_named("sched-crash");
    let mock = MockSmtp::start(MockSmtpConfig::default()).await.unwrap();
    fixture.set_smtp_port(mock.port());
    let store = fixture.store();
    let entry = store
        .schedule(fixture.new_send("Exactly once", now() - 1))
        .await
        .unwrap();

    // A zero lease so the reap in the *second* pass finds this one expired
    // without the test waiting five minutes for it.
    let crash_policy = policy().with_lease(Duration::ZERO);
    let (tx, mut rx) = mpsc::unbounded_channel();
    let crashing = Arc::new(CrashingSender {
        inner: LettreSender::new(fixture.db.clone(), SmtpSecurity::Plaintext),
        delivered: tx,
    });
    let first = build_scheduler(&fixture, store.clone(), crashing, crash_policy, "worker-a");

    let pass = tokio::spawn(async move {
        let _ = first.pass().await;
    });
    // Wait until the message has genuinely reached the server, then kill the
    // task before it can record the outcome. This is the exact window a
    // duplicate would be delivered in.
    rx.recv().await.unwrap();
    pass.abort();
    let _ = pass.await;

    assert_eq!(mock.accepted_count(), 1, "the crash delivered one copy");
    let mid = store.get(entry.id).await.unwrap();
    assert_eq!(mid.state, OutboxState::Sending);
    assert!(
        mid.smtp_message_id.is_some(),
        "the fence must have been committed before DATA, or the recovery below \
         has nothing to go on"
    );

    // Restart: a fresh scheduler, a working sender, the same outbox.
    let recovered = build_scheduler(
        &fixture,
        store.clone(),
        Arc::new(LettreSender::new(
            fixture.db.clone(),
            SmtpSecurity::Plaintext,
        )),
        policy(),
        "worker-b",
    );
    let outcome = recovered.pass().await.unwrap();

    assert_eq!(outcome.reclaimed, 1);
    assert_eq!(outcome.recovered, 1);
    assert_eq!(outcome.sent, 0);
    assert_eq!(
        mock.accepted_count(),
        1,
        "the retry must not have delivered a second copy"
    );
    let final_entry = store.get(entry.id).await.unwrap();
    assert_eq!(final_entry.state, OutboxState::Sent);

    // And a third pass changes nothing — a `sent` row is not work.
    let again = recovered.pass().await.unwrap();
    assert_eq!(again, PassOutcome::default());
    assert_eq!(mock.accepted_count(), 1);
}

#[tokio::test]
async fn the_happy_path_delivers_the_frozen_octets_to_the_envelope() {
    let fixture = Fixture::open_named("sched-happy");
    let mock = MockSmtp::start(MockSmtpConfig::default()).await.unwrap();
    fixture.set_smtp_port(mock.port());
    let store = fixture.store();
    let entry = store
        .schedule(fixture.new_send("Hello", now() - 1))
        .await
        .unwrap();
    let expected = store.raw_mime(entry.id).await.unwrap();

    let scheduler = build_scheduler(
        &fixture,
        store.clone(),
        Arc::new(LettreSender::new(
            fixture.db.clone(),
            SmtpSecurity::Plaintext,
        )),
        policy(),
        "worker",
    );
    let outcome = scheduler.pass().await.unwrap();
    assert_eq!(outcome.sent, 1);

    let accepted = mock.accepted();
    assert_eq!(accepted.len(), 1);
    // The stored octets, unmodified — plus the one CRLF SMTP's own `DATA`
    // terminator contributes. lettre writes `\r\n.\r\n` unconditionally
    // (`transport::smtp::client::async_connection`), so a message that already
    // ends on a line boundary picks up a trailing blank line on the wire.
    // Asserting a prefix rather than equality is what keeps this test about
    // *our* bytes: anything rmail rewrote — a re-encoded body, a header
    // reordered, a `Bcc` reintroduced — still fails it.
    assert!(
        accepted[0].starts_with(&expected),
        "SMTP must transmit the stored octets verbatim"
    );
    assert_eq!(&accepted[0][expected.len()..], b"\r\n");
    let sent = store.get(entry.id).await.unwrap();
    assert_eq!(sent.state, OutboxState::Sent);
    assert!(sent.sent_at.is_some());
    assert!(!sent.sent_late);
}

// ---------------------------------------------------------------------------
// Failure handling
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_transient_failure_stays_scheduled_and_a_permanent_one_fails() {
    let fixture = Fixture::open_named("sched-4xx");
    let store = fixture.store();
    let entry = store
        .schedule(fixture.new_send("Flaky", now() - 1))
        .await
        .unwrap();
    let sender = Arc::new(FailingSender {
        failure: SendFailure::Transient("451 later".to_owned()),
        calls: AtomicUsize::new(0),
    });
    let scheduler = build_scheduler(&fixture, store.clone(), sender.clone(), policy(), "worker");
    assert_eq!(scheduler.pass().await.unwrap().failed, 1);
    let backed_off = store.get(entry.id).await.unwrap();
    assert_eq!(backed_off.state, OutboxState::Scheduled);
    assert_eq!(backed_off.smtp_message_id, None);
    assert!(backed_off.next_attempt_at.is_some());
    // The backoff is real: an immediate second pass must not claim it again.
    assert_eq!(scheduler.pass().await.unwrap(), PassOutcome::default());
    assert_eq!(sender.calls.load(Ordering::Relaxed), 1);

    let fixture = Fixture::open_named("sched-5xx");
    let store = fixture.store();
    let entry = store
        .schedule(fixture.new_send("Rejected", now() - 1))
        .await
        .unwrap();
    let sender = Arc::new(FailingSender {
        failure: SendFailure::Permanent("550 no such user".to_owned()),
        calls: AtomicUsize::new(0),
    });
    let scheduler = build_scheduler(&fixture, store.clone(), sender.clone(), policy(), "worker");
    assert_eq!(scheduler.pass().await.unwrap().failed, 1);
    let failed = store.get(entry.id).await.unwrap();
    assert_eq!(failed.state, OutboxState::Failed);
    assert_eq!(failed.last_error.as_deref(), Some("550 no such user"));

    // A failed row is not retried, ever, by any number of passes.
    for _ in 0..5 {
        assert_eq!(scheduler.pass().await.unwrap(), PassOutcome::default());
    }
    assert_eq!(
        sender.calls.load(Ordering::Relaxed),
        1,
        "a permanently-rejected message must be asked about exactly once"
    );
}

#[tokio::test]
async fn an_offline_send_is_retried_until_its_budget_is_spent_and_then_stops() {
    let fixture = Fixture::open_named("sched-offline");
    let store = fixture.store();
    // `max_retries` is 3 in the fixture's `new_send`.
    let entry = store
        .schedule(fixture.new_send("Offline", now() - 1))
        .await
        .unwrap();
    let sender = Arc::new(FailingSender {
        failure: SendFailure::Transient("connection refused".to_owned()),
        calls: AtomicUsize::new(0),
    });
    // Zero backoff so the passes below are not waiting on a wall clock.
    let no_backoff = SendPolicy::from_config(&SendConfig {
        backoff_base: HumanDuration::new(Duration::ZERO),
        backoff_max: HumanDuration::new(Duration::ZERO),
        ..SendConfig::default()
    });
    let scheduler = build_scheduler(
        &fixture,
        store.clone(),
        sender.clone(),
        no_backoff,
        "worker",
    );

    for _ in 0..3 {
        scheduler.pass().await.unwrap();
    }
    assert_eq!(sender.calls.load(Ordering::Relaxed), 3);
    assert_eq!(
        store.get(entry.id).await.unwrap().state,
        OutboxState::Failed
    );
    // And it stays stopped.
    scheduler.pass().await.unwrap();
    assert_eq!(sender.calls.load(Ordering::Relaxed), 3);
}

// ---------------------------------------------------------------------------
// Missed windows
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_message_due_while_rmail_was_off_goes_out_and_is_marked_late() {
    let fixture = Fixture::open_named("sched-late");
    let store = fixture.store();
    let on_time = store
        .schedule(fixture.new_send("On time", now() - 60))
        .await
        .unwrap();
    let very_late = store
        .schedule(fixture.new_send("Was offline", now() - 86_400))
        .await
        .unwrap();

    let sender = Arc::new(RecordingSender::default());
    let scheduler = build_scheduler(
        &fixture,
        store.clone(),
        sender.clone(),
        // Two workers, so both go out in one pass.
        policy(),
        "worker",
    );
    let outcome = scheduler.pass().await.unwrap();
    assert_eq!(outcome.sent, 2, "neither may be dropped for being overdue");

    assert!(!store.get(on_time.id).await.unwrap().sent_late);
    let flagged = store.get(very_late.id).await.unwrap();
    assert_eq!(flagged.state, OutboxState::Sent);
    assert!(
        flagged.sent_late,
        "prd.md: send it, but say it was late — never drop it"
    );
}

// ---------------------------------------------------------------------------
// Sleeping, not polling
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_insert_wakes_the_scheduler_instead_of_waiting_out_the_poll_interval() {
    let fixture = Fixture::open_named("sched-wake");
    let store = fixture.store();
    let sender = Arc::new(RecordingSender::default());
    // A one-hour poll interval: if this test passes on the timer rather than
    // on the wake-up, it will not pass at all.
    let scheduler = build_scheduler(&fixture, store.clone(), sender.clone(), policy(), "worker");
    let cancel = CancellationToken::new();
    let running = scheduler.spawn(cancel.clone());

    // Let the first (empty) pass finish and the loop settle into its sleep.
    tokio::time::sleep(Duration::from_millis(50)).await;
    let entry = store
        .schedule(fixture.new_send("Wake up", now() - 1))
        .await
        .unwrap();

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if store.get(entry.id).await.unwrap().state == OutboxState::Sent {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the insert should have woken the scheduler; it is still asleep on a \
             one-hour poll interval"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    cancel.cancel();
    store.wake_handle().wake();
    let _ = tokio::time::timeout(Duration::from_secs(5), running).await;
}

#[tokio::test]
async fn a_future_message_is_not_sent_before_its_time() {
    let fixture = Fixture::open_named("sched-future");
    let store = fixture.store();
    store
        .schedule(fixture.new_send("Not yet", now() + 3_600))
        .await
        .unwrap();
    let sender = Arc::new(RecordingSender::default());
    let scheduler = build_scheduler(&fixture, store, sender.clone(), policy(), "worker");
    assert_eq!(scheduler.pass().await.unwrap(), PassOutcome::default());
    assert!(sender
        .sent
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_empty());
}

// ---------------------------------------------------------------------------
// Filing in Sent
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_copy_filed_in_sent_is_the_transmitted_octets_and_carries_no_bcc() {
    let fixture = Fixture::open_named("sched-sent");
    let store = fixture.store();
    let entry = store
        .schedule(fixture.new_send("Filed", now() - 1))
        .await
        .unwrap();
    assert_eq!(entry.bcc, ["blind@example.com"], "the fixture uses a Bcc");

    let sender = Arc::new(RecordingSender::default());
    let appender = Arc::new(RecordingAppender::default());
    let scheduler = build_scheduler(&fixture, store.clone(), sender.clone(), policy(), "worker")
        .with_sent_appender(appender.clone());
    assert_eq!(scheduler.pass().await.unwrap().sent, 1);

    let transmitted = sender
        .sent
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let filed = appender
        .filed
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    assert_eq!(filed.len(), 1);
    assert_eq!(filed[0], transmitted[0].1);

    // The blind recipient reached the envelope and nothing else.
    assert!(transmitted[0]
        .0
        .to
        .contains(&"blind@example.com".to_owned()));
    let body = String::from_utf8(filed[0].clone())
        .unwrap()
        .to_ascii_lowercase();
    assert!(
        !body.contains("bcc:") && !body.contains("blind@example.com"),
        "the filed copy must not name a blind recipient:\n{body}"
    );
}

#[tokio::test]
async fn a_failed_append_leaves_the_message_sent() {
    // The message is already delivered. Turning a filing failure into a
    // failed row would make the retry deliver it a second time.
    let fixture = Fixture::open_named("sched-append-fail");
    let store = fixture.store();
    let entry = store
        .schedule(fixture.new_send("Delivered", now() - 1))
        .await
        .unwrap();
    let scheduler = build_scheduler(
        &fixture,
        store.clone(),
        Arc::new(RecordingSender::default()),
        policy(),
        "worker",
    )
    .with_sent_appender(Arc::new(BrokenAppender));
    assert_eq!(scheduler.pass().await.unwrap().sent, 1);
    assert_eq!(store.get(entry.id).await.unwrap().state, OutboxState::Sent);
}

#[tokio::test]
async fn append_to_sent_off_files_nothing() {
    let fixture = Fixture::open_named("sched-no-append");
    let store = fixture.store();
    store
        .schedule(fixture.new_send("Unfiled", now() - 1))
        .await
        .unwrap();
    let appender = Arc::new(RecordingAppender::default());
    let policy = SendPolicy::from_config(&SendConfig {
        append_to_sent: false,
        ..SendConfig::default()
    });
    let scheduler = build_scheduler(
        &fixture,
        store,
        Arc::new(RecordingSender::default()),
        policy,
        "worker",
    )
    .with_sent_appender(appender.clone());
    assert_eq!(scheduler.pass().await.unwrap().sent, 1);
    assert!(appender
        .filed
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .is_empty());
}

// ---------------------------------------------------------------------------
// Bounded concurrency
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_worker_pool_bounds_how_many_messages_are_in_flight() {
    let fixture = Fixture::open_named("sched-pool");
    let store = fixture.store();
    for i in 0..5 {
        store
            .schedule(fixture.new_send(&format!("m{i}"), now() - 1))
            .await
            .unwrap();
    }
    let sender = Arc::new(RecordingSender::default());
    let policy = SendPolicy::from_config(&SendConfig {
        workers: 2,
        ..SendConfig::default()
    });
    let scheduler = build_scheduler(&fixture, store.clone(), sender.clone(), policy, "worker");

    assert_eq!(scheduler.pass().await.unwrap().sent, 2);
    assert_eq!(scheduler.pass().await.unwrap().sent, 2);
    assert_eq!(scheduler.pass().await.unwrap().sent, 1);
    assert_eq!(scheduler.pass().await.unwrap(), PassOutcome::default());
    assert_eq!(
        sender
            .sent
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len(),
        5
    );
}

// ---------------------------------------------------------------------------
// Cancel racing the sender
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_cancel_that_arrives_after_the_claim_reports_already_sent() {
    let fixture = Fixture::open_named("sched-race");
    let store = fixture.store();
    let entry = store
        .schedule(fixture.new_send("Racing", now() - 1))
        .await
        .unwrap();
    let scheduler = build_scheduler(
        &fixture,
        store.clone(),
        Arc::new(RecordingSender::default()),
        policy(),
        "worker",
    );
    scheduler.pass().await.unwrap();
    assert_eq!(
        store.cancel(entry.id).await.unwrap_err().reason(),
        crate::ErrorReason::AlreadyExists
    );
}

// ---------------------------------------------------------------------------
// AI origin
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_ai_send_is_not_transmitted_inside_its_interception_window() {
    // The end-to-end half of the policy test: a model asking for an immediate
    // send with no undo window still leaves a human time to cancel.
    let fixture = Fixture::open_named("sched-ai");
    let store = fixture.store();
    let policy = SendPolicy::from_config(&SendConfig {
        undo_window: HumanDuration::new(Duration::ZERO),
        ai_requires_confirmation: false,
        poll_interval: HumanDuration::new(Duration::from_secs(3_600)),
        ..SendConfig::default()
    });
    let resolved = policy.resolve(Origin::Ai, Some(now()), Some(Duration::ZERO), now());
    let entry = store
        .schedule(NewSend {
            origin: Origin::Ai,
            send_at: resolved.send_at,
            undo_deadline: resolved.undo_deadline,
            ..fixture.new_send("From Claude", resolved.send_at)
        })
        .await
        .unwrap();
    assert!(entry.undo_deadline.is_some());

    let sender = Arc::new(RecordingSender::default());
    let scheduler = build_scheduler(&fixture, store.clone(), sender.clone(), policy, "worker");
    assert_eq!(
        scheduler.pass().await.unwrap(),
        PassOutcome::default(),
        "an AI send must not be transmitted before its window closes"
    );
    // And the human can still stop it.
    assert_eq!(
        store.cancel(entry.id).await.unwrap().state,
        OutboxState::Canceled
    );
}
