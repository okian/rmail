//! The loop that drains the outbox.
//!
//! # It sleeps; it does not poll
//!
//! prd.md is explicit: *no busy-polling — sleeps until `min(next_due,
//! poll_interval)`, woken by `Notify` on insert*. So each pass asks the
//! database one indexed question ("what is the earliest thing still
//! outstanding?"), sleeps exactly that long, and is interrupted the moment
//! anything is scheduled, rescheduled, sent now, or retried. A daemon with an
//! empty outbox costs one query per `poll_interval`, and a message scheduled
//! for two seconds from now goes out in two seconds rather than at the next
//! tick.
//!
//! The `poll_interval` ceiling is what makes the loop safe against the two
//! things a `Notify` cannot cover: a wall clock that moves (an NTP step, a
//! timezone-less clock correction) and a machine that suspends. `tokio::time`
//! is monotonic and does not advance while a laptop is closed, so a lid opened
//! after three hours resumes a sleep that still believes it has twenty seconds
//! left — the interval bounds that lateness, and [`SchedulerHandle::wake`]
//! collapses it to zero for any caller that can observe a resume or a
//! network-up event directly. This build ships no OS power/network observer,
//! so the ceiling is what is actually load-bearing today, and
//! `send.late_tolerance` is what tells the user when it mattered.
//!
//! # One message at a time, twice
//!
//! Concurrency is bounded by [`super::SendPolicy::workers`] (prd.md's default
//! is 2). Not for throughput — an outbox is not a mail server — but because
//! each in-flight send holds an SMTP connection and a lease, and a backlog
//! drained fifty-at-once is indistinguishable from an outbound spam run to
//! every submission relay that rate-limits.
//!
//! # The order of operations inside a send is the whole contract
//!
//! ```text
//! claim (scheduled -> sending, lease)          <- transactional; beats cancel
//!   fence already set? -> mark_recovered, stop <- at-most-once
//!   begin_transmit (commit Message-ID)         <- BEFORE any octet moves
//!   sender.send(...)
//!     Ok        -> mark_sent, append to Sent
//!     Transient -> clear fence, back off, stay scheduled
//!     Permanent -> clear fence, fail
//! ```
//!
//! Every line of it is load-bearing; see [`super`]'s module docs for why.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Notify;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::events::{EventKind, EventLog, NewEvent};

use super::followup::FollowupStore;
use super::policy::SendPolicy;
use super::sent::SentAppender;
use super::smtp::{SendFailure, SmtpSender};
use super::{ClaimedSend, OutboxStore};

/// Wakes a sleeping scheduler.
///
/// Every write path on [`OutboxStore`] already wakes it; this is for the
/// callers that are not writes — a resume-from-sleep or network-up observer,
/// or a test that wants the next pass now.
#[derive(Debug, Clone)]
pub struct SchedulerHandle {
    pub(super) notify: Arc<Notify>,
}

impl SchedulerHandle {
    pub(super) fn new(notify: Arc<Notify>) -> Self {
        Self { notify }
    }

    /// Interrupt the current sleep and run a pass immediately.
    ///
    /// `notify_one` rather than `notify_waiters`: it stores a permit when the
    /// loop is mid-pass rather than mid-sleep, so a wake that arrives in that
    /// window is honoured on the next sleep instead of being dropped — which
    /// for an insert is the difference between "sent on time" and "sent at
    /// the next poll".
    pub fn wake(&self) {
        self.notify.notify_one();
    }
}

/// What one pass of the loop did. Returned so tests can drive the scheduler
/// deterministically instead of sleeping and hoping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PassOutcome {
    /// Rows reclaimed from expired leases.
    pub reclaimed: u64,
    /// Messages delivered.
    pub sent: usize,
    /// Rows closed out by the at-most-once recovery path, without
    /// transmitting.
    pub recovered: usize,
    /// Sends that failed (transiently or permanently) this pass.
    pub failed: usize,
    /// Reminders raised.
    pub followups_fired: usize,
}

/// Drains the outbox and the follow-up queue.
#[derive(Clone)]
pub struct SendScheduler {
    store: OutboxStore,
    followups: FollowupStore,
    sender: Arc<dyn SmtpSender>,
    appender: Option<Arc<dyn SentAppender>>,
    events: EventLog,
    policy: SendPolicy,
    worker: String,
    wake: Arc<Notify>,
}

impl std::fmt::Debug for SendScheduler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SendScheduler")
            .field("worker", &self.worker)
            .field("policy", &self.policy)
            .finish_non_exhaustive()
    }
}

impl SendScheduler {
    /// Build a scheduler over `store`.
    ///
    /// `worker` names this instance in the lease it takes; it exists so a
    /// completion can be fenced against a lease that was reaped and handed to
    /// someone else. One daemon has one scheduler, so a constant is enough —
    /// but it must be *stable across a restart*, which is why it is passed in
    /// rather than randomized here.
    #[must_use]
    pub fn new(
        store: OutboxStore,
        followups: FollowupStore,
        sender: Arc<dyn SmtpSender>,
        events: EventLog,
        policy: SendPolicy,
        worker: impl Into<String>,
    ) -> Self {
        let wake = store.wake_handle();
        Self {
            store,
            followups,
            sender,
            appender: None,
            events,
            policy,
            worker: worker.into(),
            wake: wake.notify,
        }
    }

    /// File delivered messages in the account's IMAP `Sent` folder.
    ///
    /// Without one, `send.append_to_sent` has nothing to append with and is
    /// simply not honoured — which is the right behavior for a daemon whose
    /// IMAP side is unavailable, since the alternative is failing sends that
    /// already succeeded.
    #[must_use]
    pub fn with_sent_appender(mut self, appender: Arc<dyn SentAppender>) -> Self {
        self.appender = Some(appender);
        self
    }

    /// A handle that wakes this scheduler.
    #[must_use]
    pub fn handle(&self) -> SchedulerHandle {
        SchedulerHandle::new(Arc::clone(&self.wake))
    }

    /// Run until `cancel` fires.
    ///
    /// Detached onto its own task; the returned handle resolves when the loop
    /// has stopped, so a caller can wait for an in-flight send to finish
    /// rather than dropping it mid-`DATA`.
    #[must_use]
    pub fn spawn(self, cancel: CancellationToken) -> JoinHandle<()> {
        let span = tracing::info_span!("send_scheduler", worker = %self.worker);
        tokio::spawn(
            async move {
                self.run(cancel).await;
            }
            .instrument(span),
        )
    }

    /// The loop itself.
    async fn run(self, cancel: CancellationToken) {
        tracing::info!(
            workers = self.policy.workers(),
            poll_interval_secs = self.policy.poll_interval().as_secs(),
            "send scheduler started"
        );
        loop {
            if cancel.is_cancelled() {
                break;
            }
            // A pass runs first, before any sleep: a daemon restarted more
            // often than its poll interval would otherwise never drain a
            // missed window at all — and that machine is exactly the one
            // prd.md's "missed window (rmail off)" case describes.
            if let Err(error) = self.pass().await {
                tracing::warn!(%error, "a send scheduler pass failed; retrying after a sleep");
            }

            let delay = self.sleep_for().await;
            let before_wall = chrono::Utc::now().timestamp();
            let before = Instant::now();
            tokio::select! {
                () = cancel.cancelled() => break,
                () = self.wake.notified() => {}
                () = tokio::time::sleep(delay) => {}
            }
            // Monotonic time does not advance while a machine is suspended,
            // so a wall clock that ran much further than the sleep did is the
            // one observable signal that the lid was closed. Logged rather
            // than acted on: the next pass — which is about to run anyway —
            // is the whole correction.
            let wall_delta = chrono::Utc::now().timestamp() - before_wall;
            let monotonic = i64::try_from(before.elapsed().as_secs()).unwrap_or(i64::MAX);
            if wall_delta.saturating_sub(monotonic) > RESUME_SKEW_SECS {
                tracing::info!(
                    skew_secs = wall_delta - monotonic,
                    "the wall clock jumped past the sleep (a suspend, or a clock step); \
                     draining the outbox now"
                );
            }
        }
        tracing::info!("send scheduler stopped");
    }

    /// How long to sleep: `min(next_due - now, poll_interval)`, never
    /// negative.
    async fn sleep_for(&self) -> Duration {
        let poll = self.policy.poll_interval();
        match self.store.next_due_at().await {
            Ok(Some(next)) => {
                let now = chrono::Utc::now().timestamp();
                let delta = u64::try_from(next.saturating_sub(now)).unwrap_or(0);
                Duration::from_secs(delta).min(poll)
            }
            Ok(None) => poll,
            // A failed query must not turn into a hot loop; sleep the normal
            // interval and let the next pass report it properly.
            Err(error) => {
                tracing::warn!(%error, "could not compute the next due time");
                poll
            }
        }
    }

    /// One pass: reclaim, drain, sweep.
    ///
    /// Public so tests (and a future `AdminService` "drain now") can run the
    /// scheduler deterministically rather than racing a sleep.
    ///
    /// # Errors
    ///
    /// A mapped storage error from the reclaim or the claim. Individual send
    /// failures are recorded on their rows, not returned — one unreachable
    /// server must not stop the pass that would have delivered everything
    /// else.
    #[tracing::instrument(skip(self))]
    pub async fn pass(&self) -> Result<PassOutcome, crate::Error> {
        let now = chrono::Utc::now().timestamp();
        let mut outcome = PassOutcome {
            reclaimed: self.store.reap_expired(now).await?,
            ..PassOutcome::default()
        };

        // Claimed in one batch of at most `workers`, then run concurrently.
        // Claiming more than can be worked would lease rows nobody is
        // transmitting, and a lease is a promise to make progress.
        let limit = i64::try_from(self.policy.workers()).unwrap_or(1);
        let claimed = self
            .store
            .claim_due(&self.worker, limit, now, self.policy.lease())
            .await?;
        let results = futures::future::join_all(claimed.iter().map(|claim| {
            self.deliver(claim, now)
                .instrument(tracing::Span::current())
        }))
        .await;
        for result in results {
            match result {
                SendResult::Sent => outcome.sent += 1,
                SendResult::Recovered => outcome.recovered += 1,
                SendResult::Failed => outcome.failed += 1,
                SendResult::Lost => {}
            }
        }

        match self.followups.sweep(now).await {
            Ok(fired) => {
                outcome.followups_fired = fired.len();
                for followup in &fired {
                    // Logged rather than appended to the event log: the
                    // durable vocabulary (`events::EventKind`) has no
                    // follow-up kind, and borrowing `SEND_RESULT` for one
                    // would put a record on the bus that every existing
                    // consumer would misread as an outbound delivery. The
                    // reminder's own durable record is its `fired` row, which
                    // `ListFollowups` returns.
                    tracing::info!(
                        followup_id = followup.id,
                        account_id = followup.account_id,
                        message_id = %followup.message_id,
                        note = followup.note.as_deref().unwrap_or(""),
                        "follow-up due"
                    );
                }
            }
            // A follow-up is a reminder; failing to raise one must not fail
            // the pass that is delivering mail.
            Err(error) => tracing::warn!(%error, "the follow-up sweep failed"),
        }
        Ok(outcome)
    }

    /// Transmit one claimed row, honouring the at-most-once fence.
    async fn deliver(&self, claim: &ClaimedSend, now: i64) -> SendResult {
        // The at-most-once branch. A fence on a freshly-claimed row means a
        // previous attempt committed a `Message-ID` and then vanished, so a
        // copy may already be on the wire. Do not send a second one.
        if claim.committed_message_id.is_some() {
            return match self.store.mark_recovered(claim).await {
                Ok(true) => SendResult::Recovered,
                Ok(false) => SendResult::Lost,
                Err(error) => {
                    tracing::error!(outbox_id = claim.id, %error, "could not close out a recovered send");
                    SendResult::Failed
                }
            };
        }

        // The fence, committed before any octet moves.
        match self.store.begin_transmit(claim).await {
            Ok(true) => {}
            Ok(false) => return SendResult::Lost,
            Err(error) => {
                tracing::error!(
                    outbox_id = claim.id, %error,
                    "could not commit the Message-ID before DATA; not transmitting"
                );
                // Recorded as a transient failure rather than simply returned.
                // Left alone, the row would sit in `sending` until its lease
                // lapsed, be reclaimed, fail here again, and loop forever at
                // one round per lease — a database that cannot accept this
                // write is not going to start accepting it. Charging the
                // attempt budget is what turns that into a bounded number of
                // tries and then a `failed` row the user can see. The detail
                // stays in the log: `Error::Internal`'s message is deliberately
                // server-side, and `last_error` is client-readable.
                self.record_failure(
                    claim,
                    &SendFailure::Transient(
                        "could not record this message's identity before sending".to_owned(),
                    ),
                    now,
                )
                .await;
                return SendResult::Failed;
            }
        }

        let late = self.policy.is_late(claim.send_at, now);
        match self
            .sender
            .send(claim.account_id, &claim.envelope, &claim.raw_mime)
            .await
        {
            Ok(()) => {
                match self.store.mark_sent(claim, late).await {
                    Ok(true) => {}
                    // The lease was reaped mid-transmission. The message went
                    // out; the fence is what stops the new owner sending it
                    // again, and this is worth an error-level line because it
                    // means the lease is too short for this account's links.
                    Ok(false) => {
                        tracing::error!(
                            outbox_id = claim.id,
                            "delivered a message whose lease had already been reaped; the \
                             Message-ID fence is now the only thing preventing a duplicate"
                        );
                        return SendResult::Lost;
                    }
                    Err(error) => {
                        tracing::error!(
                            outbox_id = claim.id, %error,
                            "delivered a message but could not record it as sent"
                        );
                        return SendResult::Failed;
                    }
                }
                if late {
                    tracing::warn!(
                        outbox_id = claim.id,
                        overdue_secs = now.saturating_sub(claim.send_at),
                        "sent late: rmail was not running when this came due"
                    );
                }
                self.file_in_sent(claim).await;
                self.publish_send(claim, true, None).await;
                SendResult::Sent
            }
            Err(failure) => {
                self.record_failure(claim, &failure, now).await;
                SendResult::Failed
            }
        }
    }

    /// Record an SMTP failure on its row.
    ///
    /// The transient and permanent paths clear the fence: a *returned* error
    /// means the peer answered and queued nothing, so a retry is not a
    /// duplicate. The indeterminate path deliberately does not — see
    /// [`OutboxStore::mark_indeterminate`].
    async fn record_failure(&self, claim: &ClaimedSend, failure: &SendFailure, now: i64) {
        let outcome = match failure {
            SendFailure::Transient(_) => {
                let backoff = self.policy.backoff_for(claim.attempts);
                self.store
                    .mark_transient_failure(claim, failure.message(), backoff, now)
                    .await
                    .map(|outcome| outcome.is_some())
            }
            SendFailure::Permanent(_) => {
                self.store
                    .mark_permanent_failure(claim, failure.message())
                    .await
            }
            SendFailure::Indeterminate(_) => self
                .store
                .mark_indeterminate(claim, failure.message(), now)
                .await
                .map(|outcome| outcome.is_some()),
        };
        match outcome {
            Ok(true) => self.publish_send(claim, false, Some(failure)).await,
            Ok(false) => tracing::warn!(
                outbox_id = claim.id,
                "a send failed on a lease this worker no longer holds"
            ),
            Err(error) => tracing::error!(
                outbox_id = claim.id, %error,
                "could not record a send failure; the lease will expire and it will be retried"
            ),
        }
    }

    /// File a delivered message in IMAP `Sent`.
    ///
    /// Best effort by design — see [`super::sent`]'s module docs. The octets
    /// are the ones that were transmitted, unmodified: there is no `Bcc`
    /// header in them to strip, and adding a rewriting step here could only
    /// introduce one.
    async fn file_in_sent(&self, claim: &ClaimedSend) {
        if !self.policy.append_to_sent() {
            return;
        }
        let Some(appender) = &self.appender else {
            tracing::debug!(
                outbox_id = claim.id,
                "send.append_to_sent is on but no IMAP appender is wired; not filing"
            );
            return;
        };
        if let Err(error) = appender
            .append_to_sent(claim.account_id, &claim.raw_mime)
            .await
        {
            tracing::warn!(
                outbox_id = claim.id, %error,
                "the message was delivered but could not be filed in Sent"
            );
        }
    }

    /// Append a durable `SEND_RESULT` event.
    ///
    /// The broadcast fan-out `WatchOutbox` uses is in-process and lossy; this
    /// is the record that survives a restart and reaches `WatchEvents`.
    async fn publish_send(&self, claim: &ClaimedSend, ok: bool, failure: Option<&SendFailure>) {
        let event = NewEvent::new(EventKind::SendResult)
            .account(claim.account_id)
            .payload(serde_json::json!({
                "outbox_id": claim.id,
                "message_id": claim.message_id,
                "ok": ok,
                "origin": claim.origin.as_str(),
                "attempts": claim.attempts,
                "error": failure.map(SendFailure::message),
                "retryable": failure.map(SendFailure::is_transient),
            }));
        if let Err(error) = self.events.append(event).await {
            tracing::warn!(outbox_id = claim.id, %error, "could not record a send result");
        }
    }
}

/// How far the wall clock may run past a sleep before it is worth saying so.
///
/// Generous enough that ordinary scheduling jitter and NTP slew are silent,
/// small enough that a real suspend is not.
const RESUME_SKEW_SECS: i64 = 30;

/// What [`SendScheduler::deliver`] did with one row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SendResult {
    /// Transmitted and recorded.
    Sent,
    /// Closed out by the at-most-once path without transmitting.
    Recovered,
    /// Attempted and rejected.
    Failed,
    /// The lease was gone; whoever holds it now owns the outcome.
    Lost,
}

#[cfg(test)]
mod tests;
