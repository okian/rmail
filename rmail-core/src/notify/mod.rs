//! The priority notification engine (task 81, prd.md #62 "AI Priority
//! Notification Engine"): every newly synced message is scored by Haiku into
//! an importance tier plus a one-line reason, and a desktop notification fires
//! only at or above the account's threshold — "so newsletters never ping".
//!
//! # Two halves, and the seam between them is durable
//!
//! Scoring ([`score::NotifyPassHandler`]) is an [`crate::ai::queue`] pass like
//! triage and the deep pass; it produces a verdict and writes it to the
//! `notifications` table (V40). Delivery ([`NotifyEngine::tick`]) reads that
//! table and does the rest. Nothing ever hands a verdict straight to a
//! notifier.
//!
//! The seam is durable because the two halves have different failure
//! semantics and only one of them is repeatable. Re-running a model call is
//! ordinary: it costs money and produces the same kind of answer. Re-running a
//! delivery is not — it interrupts a human a second time about a message they
//! already dismissed, and no later correctness argument can undo it. So the
//! fact of a decision lands in SQLite under `UNIQUE (message_id)` before any
//! notifier is touched, and every transition out of `pending` is conditional
//! on still being `pending` (see [`repo`]).
//!
//! # What that does and does not guarantee
//!
//! **Does:** a message is decided about exactly once. Scoring it again — a
//! reaped AI lease, a re-enqueued pass, a restarted daemon — cannot produce a
//! second decision, and a decision that has been *recorded* as delivered,
//! suppressed or failed is never revisited. A restart re-delivers nothing.
//!
//! **Does not:** make delivery atomic with the record of it. The order is
//! claim → deliver → mark, so a crash (or a failing write) in the window
//! between `osascript` accepting the notification and the row committing
//! leaves the row `pending`, and it will be delivered again once its claim
//! lease lapses. That window is deliberately on this side of the trade: the
//! alternative — mark first, then deliver — turns the same crash into a
//! notification the user *never* sees, and for a priority alert a duplicate is
//! a smaller failure than a silent loss. What is bounded is how often it can
//! repeat: [`NotifyEngine::tick`] records a row `failed` once its claims
//! exceed `notify.max_attempts`, so a notification that reliably kills the
//! delivery loop stops rather than looping forever.
//!
//! # What the gate actually is
//!
//! Three things, checked in this order, and the order matters:
//!
//! 1. **Is this account allowed to notify at all** (`notify.enabled`, or the
//!    account's own `notify.enabled` override). A `no` here is terminal —
//!    the row is `suppressed`, not held.
//! 2. **Does the tier clear the threshold** (`notify.threshold`, or the
//!    account's own). Also terminal: a newsletter scored `low` is not going to
//!    become urgent by waiting.
//! 3. **Are we inside quiet hours.** This one is *not* terminal — the row
//!    stays `pending` with `next_attempt_at` at the end of the window, and is
//!    delivered when it closes. See [`quiet`] on why a held notification is
//!    the only defensible reading of a do-not-disturb window.
//!
//! Putting the threshold before quiet hours is deliberate: a message that
//! would never have notified must not sit `pending` until morning and then be
//! evaluated again. Suppress it once, cheaply, at 03:00.
//!
//! # What leaves the machine: nothing
//!
//! The only delivering channel is [`channel::DesktopChannel`], which spawns
//! `osascript` locally. There is no webhook or push arm on
//! [`channel::NotifyChannel`], and adding one would be a data-egress feature
//! needing its own opt-in — see that module's docs. What the local
//! notification may *say* is minimized by default: sender and tier always,
//! subject only under `notify.include_subject`, the model's reason only under
//! `notify.include_reason` (off by default). No setting puts a message body in
//! a notification.
//!
//! # Why the delivery loop polls rather than subscribing
//!
//! The same reasoning [`crate::hooks`] gives, plus one more that is specific
//! to this engine: quiet hours mean a row can become deliverable with no event
//! happening at all. A subscription to "a notification was scored" would sleep
//! straight through 07:00. A timer is not an optimization here, it is the only
//! thing that can wake the deferred set.

pub mod channel;
pub mod quiet;
pub mod repo;
pub mod score;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::config::{AccountConfig, NotifyConfig};
use crate::error::Error;
use crate::storage::Database;

pub use channel::{DeliveryError, Notification, NotifyChannel};
pub use quiet::QuietHours;
pub use repo::Alert;
pub use score::{NotifyPassHandler, NotifyScore, Threshold, Tier, PASS};

/// How many alerts the `StreamAlerts` fan-out buffers per subscriber.
///
/// Small next to [`crate::events::DEFAULT_CHANNEL_CAPACITY`] on purpose: an
/// alert is, by construction, something rare enough to have interrupted a
/// human, and a subscriber that has fallen a hundred alerts behind has a
/// bigger problem than the buffer. Lag is recoverable in any case — the
/// durable rows are still in `notifications`, readable through
/// [`repo::alerts_since`] with the last id the subscriber actually saw.
pub const ALERT_CHANNEL_CAPACITY: usize = 128;

/// Largest page [`repo::alerts_since`] will return through this engine.
pub const MAX_ALERT_PAGE: i64 = 500;

/// Floor on `notify.tick_interval`, for the reason
/// [`crate::hooks::MIN_TICK_INTERVAL`] gives: a `"0s"` typo must degrade to
/// "as fast as is sane", never to a busy loop against the database.
pub const MIN_TICK_INTERVAL: Duration = Duration::from_millis(10);

/// What one [`NotifyEngine::tick`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct NotifyTickReport {
    /// Rows claimed this tick.
    pub claimed: u64,
    /// Notifications actually delivered.
    pub delivered: u64,
    /// Suppressed below threshold, or because the account has notifications
    /// off.
    pub suppressed: u64,
    /// Held for quiet hours.
    pub deferred: u64,
    /// Delivery failed and will be retried.
    pub retried: u64,
    /// Delivery failed for the last time; recorded `failed`.
    pub failed: u64,
    /// Released untouched because the daemon is shutting down.
    pub released: u64,
}

/// The per-account notification policy, resolved once from config.
///
/// Keyed by account *name* because that is what `[[accounts]]` is keyed by;
/// [`repo::claim_due`] joins `accounts` so a claimed row already carries the
/// name and no lookup is needed per delivery.
///
/// Shared by both halves of the engine, and that sharing is the point. The
/// delivery loop needs it to decide whether to ping; [`NotifyPassHandler`]
/// needs the *same* answer one step earlier, to decline scoring a message for
/// an account that will never be pinged — otherwise `notify.enabled = false`
/// on one noisy account would silence its notifications while still paying for
/// every one of its model calls, forever. Two independently derived copies of
/// this decision would be exactly the kind of drift that hides such a bill.
#[derive(Debug, Clone)]
pub struct NotifyPolicy {
    enabled: bool,
    threshold: Threshold,
    accounts: HashMap<String, AccountPolicy>,
}

#[derive(Debug, Clone, Copy)]
struct AccountPolicy {
    enabled: Option<bool>,
    threshold: Option<Threshold>,
}

impl NotifyPolicy {
    /// Resolve `[notify]` plus every `[[accounts]] notify` override.
    ///
    /// An unrecognized threshold is warned about here — once, at
    /// construction — rather than on every message, and resolves to
    /// [`Threshold::Unrecognized`], which admits nothing. See [`Threshold`].
    #[must_use]
    pub fn from_config(config: &NotifyConfig, accounts: &[AccountConfig]) -> Self {
        let threshold = Threshold::parse(&config.threshold);
        if threshold == Threshold::Unrecognized {
            tracing::warn!(
                threshold = %config.threshold,
                recognized = ?Tier::ALL.map(Tier::as_str),
                "notify.threshold is not a recognized tier; no notification will be delivered \
                 under it until this is fixed"
            );
        }
        let mut resolved = HashMap::with_capacity(accounts.len());
        for account in accounts {
            let per_account = account.notify.threshold.as_deref().map(Threshold::parse);
            if per_account == Some(Threshold::Unrecognized) {
                tracing::warn!(
                    account = %account.name,
                    threshold = ?account.notify.threshold,
                    "this account's notify.threshold is not a recognized tier; it will deliver \
                     nothing until this is fixed"
                );
            }
            resolved.insert(
                account.name.clone(),
                AccountPolicy {
                    enabled: account.notify.enabled,
                    threshold: per_account,
                },
            );
        }
        Self {
            enabled: config.enabled,
            threshold,
            accounts: resolved,
        }
    }

    /// Whether `account` notifies at all, and from which tier up. An account
    /// with no `[[accounts]]` block of its own inherits the `[notify]` table.
    #[must_use]
    pub fn resolve(&self, account: &str) -> (bool, Threshold) {
        let over = self.accounts.get(account);
        (
            over.and_then(|a| a.enabled).unwrap_or(self.enabled),
            over.and_then(|a| a.threshold).unwrap_or(self.threshold),
        )
    }

    /// Whether `account` may notify at all — the question
    /// [`NotifyPassHandler`] asks before it spends anything.
    #[must_use]
    pub fn notifies(&self, account: &str) -> bool {
        self.resolve(account).0
    }
}

/// What a delivered notification is allowed to say.
#[derive(Debug, Clone, Copy)]
struct Presentation {
    include_subject: bool,
    include_reason: bool,
}

/// The delivery half of the notification engine.
///
/// Cheap to clone: every field is a handle or a small owned value. Build one,
/// hand clones to whatever needs [`Self::subscribe`], and drive the single
/// [`Self::spawn`] loop from the daemon.
#[derive(Clone)]
pub struct NotifyEngine {
    db: Database,
    channel: Arc<dyn NotifyChannel>,
    policy: NotifyPolicy,
    quiet: QuietHours,
    presentation: Presentation,
    tick_interval: Duration,
    max_attempts: i64,
    retry_backoff: Duration,
    delivery_timeout: Duration,
    max_per_tick: i64,
    alerts: broadcast::Sender<Alert>,
}

impl std::fmt::Debug for NotifyEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NotifyEngine")
            .field("channel", &self.channel.name())
            .field("enabled", &self.policy.enabled)
            .field("threshold", &self.policy.threshold)
            .field("quiet_hours", &self.quiet.is_enabled())
            .field("tick_interval", &self.tick_interval)
            .finish_non_exhaustive()
    }
}

impl NotifyEngine {
    /// Build an engine from `[notify]` and the configured accounts, resolving
    /// the delivery channel for this host.
    ///
    /// # Errors
    /// [`Error::InvalidArgument`] if `notify.quiet_hours` is enabled and its
    /// times or timezone do not parse. A bad *threshold* is deliberately not
    /// an error — see [`Threshold`] on why it resolves to "notify nothing"
    /// and is warned about instead: a daemon that refuses to boot over a
    /// notification setting is worse than one that boots quiet and says so.
    pub fn from_config(
        db: Database,
        config: &NotifyConfig,
        accounts: &[AccountConfig],
    ) -> Result<Self, Error> {
        let channel = channel::resolve(config.channel, config.delivery_timeout.as_duration());
        Self::new(db, config, accounts, channel)
    }

    /// Build an engine over an explicit channel — what tests and any future
    /// non-desktop delivery use.
    ///
    /// # Errors
    /// As [`Self::from_config`].
    pub fn new(
        db: Database,
        config: &NotifyConfig,
        accounts: &[AccountConfig],
        channel: Arc<dyn NotifyChannel>,
    ) -> Result<Self, Error> {
        let (alerts, _) = broadcast::channel(ALERT_CHANNEL_CAPACITY);
        Ok(Self {
            db,
            channel,
            policy: NotifyPolicy::from_config(config, accounts),
            quiet: QuietHours::from_config(&config.quiet_hours)?,
            presentation: Presentation {
                include_subject: config.include_subject,
                include_reason: config.include_reason,
            },
            tick_interval: config.tick_interval.as_duration().max(MIN_TICK_INTERVAL),
            max_attempts: i64::from(config.max_attempts.max(1)),
            retry_backoff: config.retry_backoff.as_duration(),
            delivery_timeout: config.delivery_timeout.as_duration(),
            max_per_tick: i64::from(config.max_per_tick.max(1)),
            alerts,
        })
    }

    /// Subscribe to alerts as they are delivered.
    ///
    /// Lossy under lag by design, exactly like [`crate::events::EventLog::subscribe`]:
    /// a subscriber that falls behind loses its *place*, not the data, and
    /// recovers it through [`Self::alerts_since`] with the last id it saw.
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Alert> {
        self.alerts.subscribe()
    }

    /// Delivered alerts after `since_id`, oldest first.
    ///
    /// # Errors
    /// A mapped storage error.
    pub async fn alerts_since(&self, since_id: i64, limit: i64) -> Result<Vec<Alert>, Error> {
        repo::alerts_since(&self.db, since_id, limit.clamp(1, MAX_ALERT_PAGE)).await
    }

    /// The newest notification id, for a subscriber that wants "from now on".
    ///
    /// # Errors
    /// A mapped storage error.
    pub async fn latest_alert_id(&self) -> Result<i64, Error> {
        repo::latest_id(&self.db).await
    }

    /// The effective policy for `account`: whether it notifies, and from which
    /// tier up. What `ScoreMessage` reports so a caller can explain a
    /// suppression without re-deriving config.
    #[must_use]
    pub fn policy_for(&self, account: &str) -> (bool, Threshold) {
        self.policy.resolve(account)
    }

    /// Whether `at` is inside the configured quiet-hours window.
    #[must_use]
    pub fn is_quiet(&self, at: DateTime<Utc>) -> bool {
        self.quiet.is_quiet(at)
    }

    /// The channel's name, for diagnostics.
    #[must_use]
    pub fn channel_name(&self) -> &'static str {
        self.channel.name()
    }

    /// One delivery cycle at wall-clock `now`.
    ///
    /// Claims at most `notify.max_per_tick` due rows, decides each, and
    /// delivers those that clear the gate. Never propagates a single
    /// notification's failure — only a failure to read or write the table
    /// itself can fail a tick, for the reason [`crate::hooks::HookDispatcher::tick`]
    /// gives: one broken delivery must not stall every other one behind it.
    ///
    /// `now` is a parameter rather than read from the clock inside, so the
    /// quiet-hours boundary is testable at an exact instant rather than
    /// approximately, near midnight, on whichever machine ran the suite.
    ///
    /// # Errors
    /// A mapped storage error from claiming due rows.
    #[tracing::instrument(skip(self, cancel), fields(claimed, delivered, suppressed, deferred))]
    pub async fn tick(
        &self,
        now: DateTime<Utc>,
        cancel: &CancellationToken,
    ) -> Result<NotifyTickReport, Error> {
        let now_secs = now.timestamp();
        // The claim lease: how long a claimed row stays invisible. Sized for
        // one attempt (its timeout plus a margin) rather than for a whole
        // batch, so a process that dies mid-attempt leaves a row that becomes
        // claimable again within seconds instead of minutes.
        //
        // That sizing is only safe because this batch is delivered *serially*
        // by a *single* loop: `NotifyEngine::spawn` is called once per daemon
        // and awaits each `tick` before sleeping, so the twentieth row in a
        // batch cannot have its lease expire while the first is still being
        // delivered by some other worker — there is no other worker. Two
        // delivery loops over one database would break that, which is why
        // there is no API here for starting a second one.
        let lease = i64::try_from(self.delivery_timeout.as_secs()).unwrap_or(i64::MAX / 4) + 30;
        let claimed = repo::claim_due(&self.db, now_secs, lease, self.max_per_tick).await?;
        let mut report = NotifyTickReport {
            claimed: claimed.len() as u64,
            ..NotifyTickReport::default()
        };
        for pending in claimed {
            if cancel.is_cancelled() {
                // Released rather than left claimed: shutting down must not
                // cost this notification the delivery attempt it had not yet
                // made, nor delay it by the whole claim lease.
                self.release(&pending, now_secs, &mut report).await;
                continue;
            }
            // A row whose attempts are already spent when it is *claimed* was
            // never resolved by the attempts it was charged for — the delivery
            // loop died mid-attempt, repeatedly. Retrying it forever would let
            // one notification that reliably kills the process hold the queue
            // open indefinitely; `failed` says the true thing and stops.
            if pending.attempts > self.max_attempts {
                tracing::warn!(
                    notification_id = pending.id,
                    attempts = pending.attempts,
                    max_attempts = self.max_attempts,
                    "notification exhausted its attempts without ever resolving; recording it \
                     failed rather than claiming it again"
                );
                match repo::mark_failed(&self.db, pending.id).await {
                    Ok(true) => report.failed += 1,
                    Ok(false) => {}
                    Err(error) => tracing::warn!(
                        notification_id = pending.id,
                        %error,
                        "could not record an exhausted notification as failed"
                    ),
                }
                continue;
            }
            let (enabled, threshold) = self.policy.resolve(&pending.account);
            if !enabled {
                self.suppress(&pending, repo::SUPPRESSED_DISABLED, &mut report)
                    .await;
                continue;
            }
            if !threshold.admits(pending.tier) {
                tracing::debug!(
                    notification_id = pending.id,
                    message_id = pending.message_id,
                    tier = %pending.tier,
                    %threshold,
                    "notification suppressed below the account's threshold"
                );
                self.suppress(&pending, repo::SUPPRESSED_BELOW_THRESHOLD, &mut report)
                    .await;
                continue;
            }
            if let Some(until) = self.quiet.ends_after(now) {
                // Refunded: quiet hours never touched the channel, so this
                // must not burn one of `max_attempts`. Otherwise a long
                // enough window would exhaust a notification's retries
                // without a single delivery having been attempted.
                match repo::defer(&self.db, pending.id, until.timestamp(), true).await {
                    // `Ok(false)` means the row was no longer `pending` — it
                    // is not a deferral and must not be counted as one, or the
                    // tick report describes work it did not do.
                    Ok(true) => report.deferred += 1,
                    Ok(false) => tracing::debug!(
                        notification_id = pending.id,
                        "notification was already decided by another writer; not deferring it"
                    ),
                    Err(error) => tracing::warn!(
                        notification_id = pending.id,
                        %error,
                        "could not defer a notification across quiet hours; it stays claimed \
                         and will be retried after its lease"
                    ),
                }
                continue;
            }
            self.deliver(&pending, now_secs, cancel, &mut report).await;
        }

        let span = tracing::Span::current();
        span.record("claimed", report.claimed);
        span.record("delivered", report.delivered);
        span.record("suppressed", report.suppressed);
        span.record("deferred", report.deferred);
        Ok(report)
    }

    /// Hand a claimed row back untouched, refunding the attempt it was
    /// charged — what shutdown does, whether it lands before a delivery starts
    /// or while one is still in flight.
    async fn release(
        &self,
        pending: &repo::PendingNotification,
        now_secs: i64,
        report: &mut NotifyTickReport,
    ) {
        match repo::defer(&self.db, pending.id, now_secs, true).await {
            Ok(_) => {}
            // Not swallowed: a failed release leaves the row claimed for the
            // whole lease, which looks from the outside exactly like a
            // notification that silently went missing for a minute.
            Err(error) => tracing::warn!(
                notification_id = pending.id,
                %error,
                "could not release a notification on shutdown; it stays claimed and will be \
                 retried after its lease expires"
            ),
        }
        report.released += 1;
    }

    async fn suppress(
        &self,
        pending: &repo::PendingNotification,
        reason: &str,
        report: &mut NotifyTickReport,
    ) {
        match repo::mark_suppressed(&self.db, pending.id, reason).await {
            Ok(true) => report.suppressed += 1,
            Ok(false) => tracing::debug!(
                notification_id = pending.id,
                "notification was already decided by another writer; leaving it alone"
            ),
            Err(error) => tracing::warn!(
                notification_id = pending.id,
                %error,
                "could not record a notification as suppressed"
            ),
        }
    }

    async fn deliver(
        &self,
        pending: &repo::PendingNotification,
        now_secs: i64,
        cancel: &CancellationToken,
        report: &mut NotifyTickReport,
    ) {
        let notification = self.render(pending);
        // Raced against shutdown, not merely checked before it. A channel is
        // an external process (`osascript`) bounded only by
        // `notify.delivery_timeout`, and awaiting it unconditionally would let
        // one slow notifier hold the daemon's shutdown open for that whole
        // window. Losing the race releases the row untouched — the same
        // disposition the pre-delivery cancellation check above uses, and for
        // the same reason: a notification the daemon chose not to attempt must
        // not lose the attempt it was owed.
        //
        // The dropped `deliver` future takes its child process with it:
        // `DesktopChannel` sets `kill_on_drop`, so this is a real termination
        // rather than an orphan.
        //
        // `biased` is load-bearing, not style. Without it `select!` polls its
        // branches in random order, so a delivery that had *already* completed
        // could still lose to a cancellation that became ready in the same
        // poll — releasing a row whose notification the user has already seen,
        // and re-showing it after the next restart. Polling the delivery first
        // means cancellation can only win while the delivery is genuinely
        // still pending.
        //
        // What remains is the honest, irreducible case: a delivery abandoned
        // *mid-flight* may or may not have reached the screen, and this
        // retries it. That direction is deliberate — for a priority alert, a
        // duplicate is a smaller failure than a silent loss.
        let outcome = tokio::select! {
            biased;
            outcome = self.channel.deliver(&notification) => outcome,
            () = cancel.cancelled() => {
                tracing::debug!(
                    notification_id = pending.id,
                    "notification delivery abandoned for shutdown; the row is released, not spent"
                );
                self.release(pending, now_secs, report).await;
                return;
            }
        };
        match outcome {
            Ok(()) => {
                // The row is marked delivered *before* the alert is published.
                // A subscriber that saw an alert whose row had not yet
                // committed could reconnect with that id as its cursor and be
                // told the alert does not exist — the same commit-then-publish
                // ordering `EventLog::append` establishes, for the same
                // reason.
                match repo::mark_delivered(&self.db, pending.id).await {
                    Ok(true) => {
                        report.delivered += 1;
                        self.publish(pending, now_secs);
                        tracing::info!(
                            notification_id = pending.id,
                            message_id = pending.message_id,
                            tier = %pending.tier,
                            channel = self.channel.name(),
                            "notification delivered"
                        );
                    }
                    Ok(false) => tracing::warn!(
                        notification_id = pending.id,
                        "a notification was delivered but its row had already been decided; \
                         not publishing a duplicate alert"
                    ),
                    Err(error) => tracing::error!(
                        notification_id = pending.id,
                        %error,
                        "a notification was delivered but could not be recorded; it may be \
                         delivered again after its claim lease expires"
                    ),
                }
            }
            Err(error) => {
                let spent = pending.attempts >= self.max_attempts;
                if spent {
                    tracing::warn!(
                        notification_id = pending.id,
                        attempts = pending.attempts,
                        %error,
                        "notification delivery failed for the last time"
                    );
                    match repo::mark_failed(&self.db, pending.id).await {
                        Ok(true) => report.failed += 1,
                        Ok(false) => {}
                        Err(error) => tracing::warn!(
                            notification_id = pending.id,
                            %error,
                            "could not record a notification as failed"
                        ),
                    }
                } else {
                    let retry_at = now_secs
                        + i64::try_from(self.retry_backoff.as_secs()).unwrap_or(i64::MAX / 4);
                    tracing::debug!(
                        notification_id = pending.id,
                        attempts = pending.attempts,
                        %error,
                        "notification delivery failed; backing off"
                    );
                    match repo::defer(&self.db, pending.id, retry_at, false).await {
                        Ok(_) => report.retried += 1,
                        Err(error) => tracing::warn!(
                            notification_id = pending.id,
                            %error,
                            "could not back off a failed notification"
                        ),
                    }
                }
            }
        }
    }

    /// What the desktop is allowed to show for `pending`.
    ///
    /// Never the body, under any configuration — see the module docs.
    fn render(&self, pending: &repo::PendingNotification) -> Notification {
        let mut body = String::new();
        if self.presentation.include_subject {
            if let Some(subject) = pending.subject.as_deref().map(str::trim) {
                if !subject.is_empty() {
                    body.push_str(subject);
                }
            }
        }
        if self.presentation.include_reason {
            if !body.is_empty() {
                body.push_str(" — ");
            }
            body.push_str(&pending.reason);
        }
        Notification {
            title: pending
                .from
                .clone()
                .unwrap_or_else(|| pending.account.clone()),
            subtitle: format!("{} · {}", pending.account, pending.tier),
            body,
        }
    }

    /// Fan an alert out to live subscribers. Best-effort: no subscribers (or a
    /// full buffer) is not a failure — the durable row is the record, and
    /// [`Self::alerts_since`] is how a subscriber recovers.
    fn publish(&self, pending: &repo::PendingNotification, delivered_at: i64) {
        let _ = self.alerts.send(Alert {
            id: pending.id,
            message_id: pending.message_id,
            account: pending.account.clone(),
            tier: pending.tier,
            reason: pending.reason.clone(),
            subject: pending.subject.clone(),
            from: pending.from.clone(),
            delivered_at,
        });
    }

    /// Spawn the periodic delivery loop, running once immediately and then on
    /// `notify.tick_interval`, until `cancel` fires.
    ///
    /// Unlike [`crate::hooks::HookDispatcher::spawn`] there is no cursor to
    /// seed: this loop's queue is a durable table, not a position in the event
    /// log, so "what has already been handled" is a fact in the database
    /// rather than a number this process has to remember. That is also what
    /// makes a restart safe — see the module docs.
    #[must_use]
    pub fn spawn(self, cancel: CancellationToken) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            loop {
                match self.tick(Utc::now(), &cancel).await {
                    Ok(report) => tracing::debug!(?report, "notification delivery tick"),
                    Err(error) => {
                        tracing::warn!(%error, "notification delivery tick failed");
                    }
                }
                tokio::select! {
                    () = cancel.cancelled() => return,
                    () = tokio::time::sleep(self.tick_interval) => {}
                }
            }
        })
    }
}
