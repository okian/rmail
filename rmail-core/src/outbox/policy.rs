//! The rules that turn a send *request* into a scheduled row: undo windows,
//! retry budgets, backoff, and leases.
//!
//! Split out from [`super::OutboxStore`] because these are decisions, not
//! storage — every one of them is a pure function of `[send]` configuration
//! and the request, and every one is worth testing without a database.
//!
//! # The one rule that is not configurable
//!
//! An AI-originated send always gets an interception window. prd.md states it
//! twice — as a behavior ("MCP-originated sends store `origin="ai"` and are
//! always subject to the undo window so a human can intercept") and as a
//! config key (`ai_requires_confirmation`) — and it is the only thing standing
//! between a model that can call `schedule_send` and a model that can put mail
//! in front of a stranger with nobody able to stop it.
//!
//! So [`MIN_AI_UNDO_WINDOW`] is a floor rather than a default. Turning
//! `ai_requires_confirmation` off shortens the window to that floor; it does
//! not remove it. Neither does a request asking for `undo_window_secs = 0`,
//! and neither does asking for `send_at = now`, which is the same bypass
//! wearing a different hat — [`SendPolicy::resolve`] pushes an AI send's
//! instant out to the floor however it was expressed.

use std::time::Duration;

use crate::config::SendConfig;

use super::Origin;

/// The shortest interception window an AI-originated send can have.
///
/// A floor, not a default: see the module docs. Ten seconds is prd.md's
/// `undo_window` default, which is the interval the product already claims is
/// enough for a human to react to a send they did not want.
pub const MIN_AI_UNDO_WINDOW: Duration = Duration::from_secs(10);

/// How long a worker's lease on a `sending` row is good for.
///
/// Long enough that a slow SMTP conversation over a bad link finishes inside
/// it — a large attachment to a distant server is minutes, not seconds — and
/// short enough that a crashed worker's row is not stranded for an hour. The
/// cost of getting this wrong in the short direction is real: a lease that
/// expires while the transmission is still in flight hands the row to a second
/// worker, and only the `smtp_message_id` fence then stops a duplicate.
pub const DEFAULT_LEASE: Duration = Duration::from_secs(5 * 60);

/// A resolved schedule: the absolute instant, and the undo deadline (if any).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedSchedule {
    /// The absolute instant the message goes out (unix seconds).
    pub send_at: i64,
    /// Until when an undo is offered (unix seconds), or `None` for a genuine
    /// future schedule — which is cancelable right up to its lease anyway,
    /// and so needs no countdown.
    pub undo_deadline: Option<i64>,
}

/// The `[send]` decisions the outbox and its scheduler consult.
///
/// Built from [`SendConfig`] rather than holding one, so the scheduler's
/// arithmetic does not have to keep unwrapping `HumanDuration`s and the
/// invariants (a floor on the AI window, a non-zero worker count) are
/// established once at construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SendPolicy {
    undo_window: Duration,
    ai_requires_confirmation: bool,
    poll_interval: Duration,
    late_tolerance: Duration,
    max_retries: i64,
    backoff_base: Duration,
    backoff_max: Duration,
    workers: usize,
    lease: Duration,
    append_to_sent: bool,
}

impl Default for SendPolicy {
    fn default() -> Self {
        Self::from_config(&SendConfig::default())
    }
}

impl SendPolicy {
    /// Derive the policy from `[send]`.
    #[must_use]
    pub fn from_config(config: &SendConfig) -> Self {
        Self {
            undo_window: config.undo_window.as_duration(),
            ai_requires_confirmation: config.ai_requires_confirmation,
            poll_interval: config.poll_interval.as_duration(),
            late_tolerance: config.late_tolerance.as_duration(),
            max_retries: i64::from(config.max_retries),
            backoff_base: config.backoff_base.as_duration(),
            backoff_max: config.backoff_max.as_duration(),
            // Coerced up rather than rejected: a `workers = 0` typo should
            // make the daemon slow, not silently stop sending mail forever.
            workers: (config.workers as usize).max(1),
            lease: DEFAULT_LEASE,
            append_to_sent: config.append_to_sent,
        }
    }

    /// The configured undo window for a human-initiated send.
    #[must_use]
    pub fn undo_window(&self) -> Duration {
        self.undo_window
    }

    /// How long the scheduler sleeps when nothing is due sooner.
    #[must_use]
    pub fn poll_interval(&self) -> Duration {
        self.poll_interval
    }

    /// How overdue a send may be before it is flagged "sent late".
    #[must_use]
    pub fn late_tolerance(&self) -> Duration {
        self.late_tolerance
    }

    /// Attempts a send gets before a transient failure becomes permanent.
    #[must_use]
    pub fn max_retries(&self) -> i64 {
        self.max_retries
    }

    /// How many messages may be in flight at once.
    #[must_use]
    pub fn workers(&self) -> usize {
        self.workers
    }

    /// How long a worker's lease on a `sending` row is good for.
    #[must_use]
    pub fn lease(&self) -> Duration {
        self.lease
    }

    /// Whether a delivered message is appended to the IMAP `Sent` folder.
    #[must_use]
    pub fn append_to_sent(&self) -> bool {
        self.append_to_sent
    }

    /// Override the lease. Tests use it to make expiry observable without
    /// waiting five minutes.
    #[must_use]
    pub fn with_lease(mut self, lease: Duration) -> Self {
        self.lease = lease;
        self
    }

    /// Override the poll interval.
    #[must_use]
    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    /// The shortest window this origin may have, whatever anyone asks for.
    ///
    /// Non-zero only for [`Origin::Ai`] — see the module docs.
    #[must_use]
    pub fn mandatory_undo_window(&self, origin: Origin) -> Duration {
        match origin {
            Origin::Ai if self.ai_requires_confirmation => self.undo_window.max(MIN_AI_UNDO_WINDOW),
            Origin::Ai => MIN_AI_UNDO_WINDOW,
            Origin::User | Origin::Followup | Origin::Undo => Duration::ZERO,
        }
    }

    /// The undo window an immediate send from `origin` gets.
    ///
    /// `requested` is the caller's `undo_window_secs`; it can lengthen the
    /// window but never shorten it below [`Self::mandatory_undo_window`].
    #[must_use]
    pub fn undo_window_for(&self, origin: Origin, requested: Option<Duration>) -> Duration {
        requested
            .unwrap_or(self.undo_window)
            .max(self.mandatory_undo_window(origin))
    }

    /// Resolve a send request into an absolute instant and an undo deadline.
    ///
    /// `requested_send_at` is `None` for "send now" — which is really
    /// "schedule at `now + undo_window`", the trick that makes undo a cancel
    /// rather than a recall.
    #[must_use]
    pub fn resolve(
        &self,
        origin: Origin,
        requested_send_at: Option<i64>,
        requested_undo: Option<Duration>,
        now: i64,
    ) -> ResolvedSchedule {
        let window = secs(self.undo_window_for(origin, requested_undo));
        let send_at = match requested_send_at {
            None => now.saturating_add(window),
            // A named instant is honoured, except that it cannot dip below
            // the mandatory floor: "schedule at now" is the same bypass as
            // "undo_window_secs = 0" expressed differently, and a floor that
            // only guards one of the two guards neither.
            Some(at) => at.max(now.saturating_add(secs(self.mandatory_undo_window(origin)))),
        };
        // A deadline is a countdown, and a countdown only makes sense while
        // the send is inside the window. A message scheduled for Friday is
        // cancelable until Friday without one.
        let undo_deadline =
            (window > 0 && send_at <= now.saturating_add(window)).then_some(send_at);
        ResolvedSchedule {
            send_at,
            undo_deadline,
        }
    }

    /// The delay before attempt `attempts`, doubling and capped.
    ///
    /// The same shape [`crate::index::queue`] uses, for the same reason: a
    /// server that is down stays down for a while, and hammering it every
    /// thirty seconds for an hour helps nobody.
    #[must_use]
    pub fn backoff_for(&self, attempts: i64) -> Duration {
        let shift = u32::try_from(attempts.max(1) - 1)
            .unwrap_or(u32::MAX)
            .min(32);
        self.backoff_base
            .saturating_mul(1u32.checked_shl(shift).unwrap_or(u32::MAX))
            .min(self.backoff_max)
    }

    /// Whether a send that came due at `send_at` and is being transmitted at
    /// `now` counts as prd.md's "sent late (was offline)".
    ///
    /// Never a reason not to send — only a reason to say so.
    #[must_use]
    pub fn is_late(&self, send_at: i64, now: i64) -> bool {
        now.saturating_sub(send_at) > secs(self.late_tolerance)
    }
}

/// Whole seconds of a duration, saturating.
fn secs(duration: Duration) -> i64 {
    i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests;
