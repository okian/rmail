//! Quiet hours: the daily do-not-disturb window a delivery is held across.
//!
//! # Held, never dropped
//!
//! Everything in this file computes one of two answers — "is this instant
//! inside the window" and "when does the window this instant is inside end" —
//! and the second is the reason the first exists. A notification that arrives
//! at 03:00 is not discarded; it is deferred to the moment the window closes.
//! A message important enough to interrupt someone for is still important at
//! breakfast, and a do-not-disturb feature that silently ate the one alert the
//! user actually wanted would be worse than having no quiet hours at all.
//!
//! # The window may wrap midnight; a zero-length window is empty, not full
//!
//! `22:00`–`07:00` is the shape almost every real quiet-hours setting has, so
//! `start > end` means "wraps", not "invalid". The genuinely ambiguous case is
//! `start == end`, which could as easily mean "never quiet" as "always quiet".
//! [`QuietHours::is_quiet`] reads it as **never**, because the failure modes
//! are not symmetric: reading it as "always" would silence every notification
//! forever from a single typo, with no error and nothing in the logs to
//! explain it.
//!
//! # Failure is bounded, not silent
//!
//! [`QuietHours::ends_after`] resolves a wall-clock instant back to UTC, which
//! is where DST lives: the hour that repeats in autumn is ambiguous, and the
//! hour that vanishes in spring does not exist at all. A gap is stepped past,
//! exactly as [`crate::outbox::schedule::resolve_local`] does for scheduled
//! send; a repeated hour resolves to the *later* of the pair, which is the
//! opposite of that function's choice and is explained at length on
//! [`Zone::window_end`] — in one word, because a window *end* is the last time
//! the clock reads it, while a scheduled *send* is the first.
//!
//! That difference is also why `resolve_local` is not simply called here. The
//! other reason is typing: it is fixed to [`chrono_tz::Tz`], and this module
//! must also answer for the host's own local zone
//! (`notify.quiet_hours.timezone = ""`), which is [`chrono::Local`] — a
//! different type implementing the same trait.
//!
//! And if resolution somehow fails anyway, this returns `now + 1 hour` rather
//! than erroring. The caller is a delivery loop deciding when to look at a row
//! again; a bounded "check back soon" costs one wasted wakeup, while an error
//! would have to be turned into *some* decision by the caller anyway, and the
//! only two available are "deliver during quiet hours" and "never deliver".

use chrono::{DateTime, Local, NaiveDateTime, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;

use crate::config::QuietHoursConfig;

/// How far past a nonexistent local time (a DST spring-forward gap) to step
/// looking for one that exists. No zone in the IANA database skips more than
/// two hours; three is the same bound
/// [`crate::outbox::schedule`] uses for the identical search.
const MAX_GAP_STEPS: i64 = 3;

/// What [`QuietHours::ends_after`] returns when a window end cannot be
/// resolved to a real instant at all — see the module docs on why this is a
/// bounded retry rather than an error.
const UNRESOLVED_RECHECK: chrono::TimeDelta = chrono::TimeDelta::hours(1);

/// The zone a quiet-hours window's wall-clock times are read in.
///
/// Two variants rather than one, because `notify.quiet_hours.timezone = ""`
/// must mean *the machine the user is sitting at*. Defaulting an unset zone to
/// UTC — which is what reusing [`crate::outbox::schedule::parse_timezone`]
/// would do — is right for scheduled send (an absolute instant frozen at
/// schedule time) and wrong here: a European user who writes `start = "22:00"`
/// and sets nothing else would get a window that opened at midnight in summer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zone {
    /// The host's own local timezone.
    Host,
    /// A named IANA zone.
    Named(Tz),
}

impl Zone {
    /// Resolve a configured zone name. An empty name is [`Zone::Host`].
    ///
    /// # Errors
    /// [`crate::Error::InvalidArgument`] if `name` is not in the IANA
    /// database.
    pub fn parse(name: &str) -> Result<Self, crate::Error> {
        let name = name.trim();
        if name.is_empty() {
            return Ok(Self::Host);
        }
        name.parse::<Tz>()
            .map(Self::Named)
            .map_err(|_| crate::Error::invalid_argument(format!("unknown timezone {name:?}")))
    }

    /// `at`, as a wall clock in this zone.
    fn wall_clock(self, at: DateTime<Utc>) -> NaiveDateTime {
        match self {
            Self::Host => at.with_timezone(&Local).naive_local(),
            Self::Named(tz) => at.with_timezone(&tz).naive_local(),
        }
    }

    /// The UTC instant a wall clock in this zone names, stepping past a DST
    /// gap — see the module docs.
    ///
    /// An *ambiguous* wall time (the hour that repeats when clocks go back)
    /// resolves to the **later** of the two, which is the opposite of the
    /// choice [`crate::outbox::schedule::resolve_local`] makes — deliberately,
    /// because this function only ever resolves a window *end*. A window that
    /// ends at 01:45 on a fall-back night ends at the *last* 01:45; taking the
    /// first would put the end an hour behind the instant being evaluated,
    /// [`QuietHours::ends_after`] would reject it as not in the future, and
    /// the notification would be held until 01:45 *tomorrow* — a
    /// twenty-four-hour silence in place of a thirty-minute one. Scheduled
    /// send takes the earlier for the mirror-image reason: a message must
    /// never go out later than the user asked.
    fn window_end(self, naive: NaiveDateTime) -> Option<DateTime<Utc>> {
        for step in 0..=MAX_GAP_STEPS {
            let candidate = naive + chrono::TimeDelta::hours(step);
            let resolved = match self {
                Self::Host => single_or_latest(Local.from_local_datetime(&candidate)),
                Self::Named(tz) => single_or_latest(tz.from_local_datetime(&candidate)),
            };
            if let Some(instant) = resolved {
                return Some(instant);
            }
        }
        None
    }
}

/// The unambiguous instant, or the later of a repeated hour. `None` for a
/// local time that does not exist — see [`Zone::window_end`].
fn single_or_latest<T: TimeZone>(result: chrono::LocalResult<DateTime<T>>) -> Option<DateTime<Utc>>
where
    T::Offset: Copy,
{
    match result {
        chrono::LocalResult::Single(dt) => Some(dt.with_timezone(&Utc)),
        chrono::LocalResult::Ambiguous(_earlier, later) => Some(later.with_timezone(&Utc)),
        chrono::LocalResult::None => None,
    }
}

/// A resolved daily do-not-disturb window.
///
/// Construct once at engine startup ([`QuietHours::from_config`]) and share:
/// this is [`Copy`], holds no allocation, and every method on it is pure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuietHours {
    enabled: bool,
    start: NaiveTime,
    end: NaiveTime,
    zone: Zone,
}

impl QuietHours {
    /// A window that is never quiet — what a disabled or unparseable config
    /// resolves to.
    #[must_use]
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            start: NaiveTime::MIN,
            end: NaiveTime::MIN,
            zone: Zone::Host,
        }
    }

    /// Resolve `[notify.quiet_hours]`.
    ///
    /// # Errors
    /// [`crate::Error::InvalidArgument`] if the window is enabled and its
    /// `start`/`end` are not `HH:MM`, or its `timezone` is not an IANA zone.
    /// A *disabled* window is never validated — an operator who turned quiet
    /// hours off should not have to keep the times they turned off parseable
    /// in order for the daemon to boot.
    pub fn from_config(config: &QuietHoursConfig) -> Result<Self, crate::Error> {
        if !config.enabled {
            return Ok(Self::disabled());
        }
        Ok(Self {
            enabled: true,
            start: parse_clock(&config.start, "start")?,
            end: parse_clock(&config.end, "end")?,
            zone: Zone::parse(&config.timezone)?,
        })
    }

    /// A window between two wall-clock times in `zone` — for tests, which
    /// need to pin a zone rather than inherit the build machine's.
    #[must_use]
    pub fn new(start: NaiveTime, end: NaiveTime, zone: Zone) -> Self {
        Self {
            enabled: true,
            start,
            end,
            zone,
        }
    }

    /// Whether `at` falls inside the window.
    ///
    /// Half-open: `start` is inside, `end` is not. That is what makes the
    /// window's two boundaries composable —
    /// [`Self::ends_after`] returns the instant at `end`, and this must
    /// report that instant as *not* quiet or the delivery it unblocks would
    /// be deferred again to the same instant, forever.
    #[must_use]
    pub fn is_quiet(&self, at: DateTime<Utc>) -> bool {
        if !self.enabled || self.start == self.end {
            return false;
        }
        let now = self.zone.wall_clock(at).time();
        if self.start < self.end {
            now >= self.start && now < self.end
        } else {
            // Wraps midnight: quiet from `start` to the end of the day, and
            // from the start of the day to `end`.
            now >= self.start || now < self.end
        }
    }

    /// The instant the window containing `at` ends, or `None` if `at` is not
    /// inside a window.
    ///
    /// Never returns an instant at or before `at`: a caller that used one as
    /// a "try again at" would spin. When the window's own end cannot be
    /// resolved to a real instant (see the module docs) this returns
    /// `at + 1 hour` — a bounded recheck, not a claim about the window.
    #[must_use]
    pub fn ends_after(&self, at: DateTime<Utc>) -> Option<DateTime<Utc>> {
        if !self.is_quiet(at) {
            return None;
        }
        let local = self.zone.wall_clock(at);
        // Today's `end` if it is still ahead of us, otherwise tomorrow's. Two
        // candidates cover both the plain and the wrapping window: a wrapping
        // window entered before midnight ends tomorrow, one entered after
        // midnight ends today.
        for day in 0..=1 {
            let Some(date) = local.date().checked_add_days(chrono::Days::new(day)) else {
                break;
            };
            let Some(instant) = self.zone.window_end(date.and_time(self.end)) else {
                continue;
            };
            if instant > at {
                return Some(instant);
            }
        }
        Some(at + UNRESOLVED_RECHECK)
    }

    /// Whether this window is switched on at all — for logging and tests.
    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.enabled
    }
}

/// Parse an `HH:MM` wall-clock time.
fn parse_clock(value: &str, field: &str) -> Result<NaiveTime, crate::Error> {
    NaiveTime::parse_from_str(value.trim(), "%H:%M").map_err(|_| {
        crate::Error::invalid_argument(format!(
            "notify.quiet_hours.{field} must be HH:MM, got {value:?}"
        ))
    })
}
