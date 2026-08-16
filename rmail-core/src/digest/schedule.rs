//! Which periods are owed a briefing — pure arithmetic over a cursor, a
//! cadence and a clock.
//!
//! Deliberately free of a database, a provider and a clock of its own, because
//! the two behaviours a periodic job most often gets wrong are exactly the two
//! this file decides:
//!
//! - **A run was missed.** The daemon was off, asleep, or the operator only
//!   just enabled the feature. [`due_periods`] walks the grid forward from the
//!   stored cursor, so every completed period since the last briefing comes
//!   back rather than only the newest one.
//! - **A run happened twice.** Two ticks inside one period, a restart in the
//!   middle of one, a manual `mail digest` covering the same days. The
//!   in-progress period is never returned, and a period already covered by the
//!   cursor is never returned.
//!
//! # The grid, and why periods are absolute rather than relative
//!
//! A period is `[k * interval, (k + 1) * interval)` for integer `k` — an
//! absolute grid anchored at the unix epoch, not "the last 24 hours from
//! whenever the daemon happened to start". That is what makes the period a
//! stable *identity*: two daemons, a restart, and a manual request all resolve
//! the same instant to the same window, so `V41__digests.sql`'s `UNIQUE
//! (account_id, period_start, period_end)` can mean "this window has been
//! briefed" rather than "some window overlapping it has". A relative grid
//! would produce a slightly different window on every boot, and the uniqueness
//! constraint would never fire.
//!
//! The consequence worth stating: with the default 24-hour cadence a period is
//! a UTC day, and with a 7-day one it is a week that begins on a Thursday (the
//! epoch was a Thursday). Anchoring to a local midnight or to a Monday would
//! need a timezone this module does not have and a rule for what happens when
//! it changes; the boundary of a briefing window matters much less than its
//! stability.

/// A closed digest window, half-open in unix seconds: `start <= t < end`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Period {
    /// Inclusive start, unix seconds.
    pub start: i64,
    /// Exclusive end, unix seconds.
    pub end: i64,
}

impl Period {
    /// The window's length in seconds.
    #[must_use]
    pub const fn seconds(&self) -> i64 {
        self.end.saturating_sub(self.start)
    }
}

/// Floor on a configured cadence. A `"0s"` interval would make every instant
/// its own period and the grid arithmetic a division by zero; a one-minute
/// floor keeps a typo expensive-but-bounded rather than fatal, the same shape
/// `notify`'s tick floor takes.
pub const MIN_INTERVAL_SECONDS: i64 = 60;

/// A configured cadence, clamped into the range this module can work in.
#[must_use]
pub const fn clamp_interval(seconds: i64) -> i64 {
    if seconds < MIN_INTERVAL_SECONDS {
        MIN_INTERVAL_SECONDS
    } else {
        seconds
    }
}

/// The grid period containing `at`.
///
/// Uses Euclidean division so a pre-epoch instant still lands in the period
/// *containing* it rather than the one after: `-1 / 86_400` truncates toward
/// zero and would put yesterday in today's window.
#[must_use]
pub fn period_containing(at: i64, interval: i64) -> Period {
    let interval = clamp_interval(interval);
    let start = at.div_euclid(interval).saturating_mul(interval);
    Period {
        start,
        end: start.saturating_add(interval),
    }
}

/// The most recent period that has already finished at `now`.
#[must_use]
pub fn last_completed(now: i64, interval: i64) -> Period {
    let interval = clamp_interval(interval);
    let current = period_containing(now, interval);
    Period {
        start: current.start.saturating_sub(interval),
        end: current.start,
    }
}

/// Every completed period that has not been briefed yet, oldest first.
///
/// `cursor` is the latest `period_end` already stored for this scope, or
/// `None` when nothing has ever been briefed. `max` bounds how many periods
/// one tick will take on: a machine that was off for a month produces the most
/// recent `max` briefings and skips the rest, because a thirty-call catch-up
/// in one tick is a bill, not a feature. The skipped periods are gone for
/// good, which is the honest trade — the alternative is either an unbounded
/// spend spike or a backlog that never drains.
///
/// # A first run brief one period, not all of history
///
/// With no cursor the answer is the single most recently completed period.
/// `digest.enabled` is off by default precisely because this costs money, and
/// an operator who has just switched it on wants yesterday's briefing, not
/// every day since the mailbox was first synced.
#[must_use]
pub fn due_periods(cursor: Option<i64>, now: i64, interval: i64, max: usize) -> Vec<Period> {
    let interval = clamp_interval(interval);
    let max = max.max(1);
    let boundary = period_containing(now, interval).start;

    let Some(cursor) = cursor else {
        let period = last_completed(now, interval);
        return if period.end <= period.start {
            Vec::new()
        } else {
            vec![period]
        };
    };

    // The cursor may be off the grid — an ad-hoc `mail digest --since 7d`
    // stores an arbitrary window, and an operator may have changed the
    // cadence since the last briefing. Rounding *up* to the next boundary is
    // what keeps this from re-briefing days the cursor already covers.
    let mut start = ceil_to_grid(cursor, interval);
    if start >= boundary {
        return Vec::new();
    }

    // Skip forward when the backlog is longer than `max`, so what comes back
    // is the most recent `max` periods rather than the oldest ones.
    //
    // Expressed as "start `max` periods back from the boundary" rather than as
    // "advance `start` by the excess". The two agree for any sane cursor, but
    // only this form is total: `start` comes from a stored row, and a corrupt
    // or hand-edited one far enough in the past makes the excess overflow —
    // `saturating_add` would then push `start` past the boundary and this
    // function would return *nothing*, silently briefing no period at all on a
    // daemon whose cursor is broken. Counting back from the boundary always
    // yields exactly `max` periods, whatever the cursor says, and
    // `max_catchup_periods` bounds `allowed * interval` well inside range.
    //
    // `saturating_sub` for `pending` for the same reason: the subtraction
    // itself can overflow before any of this is reached.
    let pending = boundary.saturating_sub(start) / interval;
    let allowed = i64::try_from(max).unwrap_or(i64::MAX);
    if pending > allowed {
        tracing::warn!(
            skipped = pending - allowed,
            cursor,
            interval,
            "the digest is further behind than digest.max_catchup_periods allows; the oldest \
             periods will never be briefed"
        );
        start = boundary.saturating_sub(allowed.saturating_mul(interval));
    }

    let mut out = Vec::new();
    while start < boundary {
        let end = start.saturating_add(interval);
        out.push(Period { start, end });
        start = end;
    }
    out
}

/// `at`, rounded up to the next multiple of `interval` (or left alone when it
/// is already one).
fn ceil_to_grid(at: i64, interval: i64) -> i64 {
    let floor = at.div_euclid(interval).saturating_mul(interval);
    if floor == at {
        at
    } else {
        floor.saturating_add(interval)
    }
}
