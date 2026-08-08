//! Turning "tomorrow 9am" into an absolute instant.
//!
//! prd.md's rule is "deterministic `chrono` grammar first; Claude only for
//! ambiguous input, always echoing the resolved absolute time for
//! confirmation." This module is the first half — a small, closed,
//! side-effect-free grammar that either produces exactly one instant or says
//! it does not understand. Nothing here calls a model, and nothing here
//! guesses: an input the grammar does not cover is an
//! [`Error::InvalidArgument`] naming what it does cover, because a scheduler
//! that silently picks the wrong Tuesday is worse than one that asks again.
//!
//! # Why the instant is frozen here and never recomputed
//!
//! Every path returns a unix instant, and the caller stores that. The IANA
//! zone travels alongside it for display only. The alternative — storing
//! `"09:00"` plus `"America/Los_Angeles"` and resolving at send time — is the
//! single most common way scheduled send goes wrong: a message scheduled on
//! the Friday before a DST change for "9am Monday" resolves to a different UTC
//! instant depending on *when you ask*, so it goes out at 08:00 or 10:00 local
//! and every intermediate value looks correct.
//!
//! # DST edge cases have one answer each
//!
//! A wall-clock time can be **ambiguous** (the hour that repeats when clocks
//! go back) or **nonexistent** (the hour that is skipped when they go
//! forward). [`resolve_local`] takes the *earlier* of an ambiguous pair — "9am"
//! means the first 9am — and steps forward through a gap to the first instant
//! that does exist, which is what every calendar application does and what a
//! user means by "the 9am that happens".
//!
//! # What this grammar deliberately does not cover
//!
//! `"optimal"` and `"recipient 9am"` (prd.md's CLI examples) are not times,
//! they are *questions* — the first needs the send-time suggester
//! ([`suggest_send_time`]), the second needs recipient-timezone inference from
//! `Date` header offsets and domain heuristics, which belongs with the
//! optimal-time AI work rather than here. Both are rejected by name so the
//! caller learns which RPC to ask instead of receiving a plausible wrong
//! answer.

use chrono::{
    DateTime, Datelike, Days, LocalResult, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Timelike,
    Utc, Weekday,
};
use chrono_tz::Tz;

use crate::config::SendOptimal;
use crate::error::Error;

/// How many hourly steps [`resolve_local`] takes looking for the far side of a
/// DST gap.
///
/// Every spring-forward transition in the IANA database is at most two hours,
/// and nearly all are one. Three bounds the loop without excluding any real
/// zone.
const MAX_GAP_STEPS: u32 = 3;

/// A resolved schedule time: the instant, plus everything a caller needs to
/// echo it back for confirmation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTime {
    /// The absolute instant, unix seconds. This is what gets stored.
    pub at: i64,
    /// The IANA zone it was resolved in. Display only — see the module docs.
    pub tz: String,
    /// The instant rendered in `tz`, RFC 3339. prd.md asks the time picker to
    /// "echo the resolved absolute time live"; this is that string, produced
    /// by the same code that did the resolving so the two cannot disagree.
    pub display: String,
}

/// Resolve an IANA zone name.
///
/// # Errors
///
/// [`Error::InvalidArgument`] if `name` is not in the IANA database. An empty
/// name is UTC rather than an error, so a caller that simply has no preference
/// need not special-case it.
pub fn parse_timezone(name: &str) -> Result<Tz, Error> {
    let name = name.trim();
    if name.is_empty() {
        return Ok(Tz::UTC);
    }
    name.parse::<Tz>()
        .map_err(|_| Error::invalid_argument(format!("unknown timezone {name:?}")))
}

/// Resolve a natural-language or absolute time expression into an instant.
///
/// `tz` is the zone bare wall-clock expressions ("tomorrow 9am") are
/// interpreted in — the account's `send.default_timezone`. An expression that
/// carries its own offset (RFC 3339) ignores it, because it already names an
/// instant.
///
/// # Errors
///
/// [`Error::InvalidArgument`] for an expression the grammar does not cover,
/// including `"optimal"` and `"recipient …"`, which are questions for other
/// RPCs rather than times (see the module docs).
pub fn resolve_send_at(input: &str, tz: Tz, now: DateTime<Utc>) -> Result<ResolvedTime, Error> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err(Error::invalid_argument("empty time expression"));
    }

    // An expression carrying its own offset already names an instant; the
    // configured zone has no say over it.
    if let Ok(absolute) = DateTime::parse_from_rfc3339(raw) {
        return Ok(present(absolute.with_timezone(&Utc), tz));
    }

    let lower = raw.to_ascii_lowercase();
    if lower == "optimal" {
        return Err(Error::invalid_argument(
            "\"optimal\" is not a time; call SuggestSendTime to get one, then schedule it",
        ));
    }
    if lower.starts_with("recipient") {
        return Err(Error::invalid_argument(
            "\"recipient <time>\" needs recipient-timezone inference, which this build does \
             not do; name an explicit timezone or an absolute time instead",
        ));
    }

    for format in [
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
        "%Y-%m-%dT%H:%M",
        "%Y-%m-%d %H:%M",
    ] {
        if let Ok(naive) = NaiveDateTime::parse_from_str(raw, format) {
            return Ok(present(resolve_local(tz, naive)?, tz));
        }
    }
    if let Ok(date) = NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
        let naive = date.and_time(NaiveTime::MIN);
        return Ok(present(resolve_local(tz, naive)?, tz));
    }

    let local_now = now.with_timezone(&tz);
    let instant = parse_relative(&lower, tz, local_now)?;
    Ok(present(instant, tz))
}

/// Interpret a wall-clock time in `tz`, resolving the two DST edge cases.
///
/// # Errors
///
/// [`Error::InvalidArgument`] if the local time does not exist and no instant
/// within [`MAX_GAP_STEPS`] hours of it does either — which no zone in the
/// IANA database produces, but which is reported rather than assumed away.
pub fn resolve_local(tz: Tz, naive: NaiveDateTime) -> Result<DateTime<Utc>, Error> {
    for step in 0..=MAX_GAP_STEPS {
        let candidate = naive + chrono::Duration::hours(i64::from(step));
        match tz.from_local_datetime(&candidate) {
            LocalResult::Single(dt) => return Ok(dt.with_timezone(&Utc)),
            // The hour that repeats when clocks go back. "9am" means the
            // first one — the same choice a calendar makes, and the one that
            // never sends a message later than the user expected.
            LocalResult::Ambiguous(earlier, _later) => return Ok(earlier.with_timezone(&Utc)),
            // The hour that is skipped when clocks go forward: step past it.
            LocalResult::None => continue,
        }
    }
    Err(Error::invalid_argument(format!(
        "{naive} does not exist in {tz}"
    )))
}

/// A side-effect-free send-time suggestion: the next instant inside the
/// configured business-hours guardrails.
///
/// This is the deterministic half of prd.md's "Optimal time" feature. It
/// applies `[send.optimal] earliest`/`latest` in `tz` and returns the next
/// moment that satisfies them — never earlier than `not_before`, so a caller
/// can insist on "after the undo window" or "after this meeting". Model-chosen
/// times (recipient history, reply-rate learning, a rationale and
/// alternatives) layer on top of this by *narrowing* the same window, so a
/// suggestion is never outside the guardrails whether or not a model produced
/// it — prd.md's "clamped to guardrails".
///
/// # Errors
///
/// [`Error::InvalidArgument`] if `earliest`/`latest` are not `HH:MM`, or if
/// `earliest` is not before `latest` (a window that is empty or wraps midnight
/// has no next moment inside it, and silently picking one would be inventing
/// policy the operator did not write).
pub fn suggest_send_time(
    optimal: &SendOptimal,
    tz: Tz,
    not_before: DateTime<Utc>,
) -> Result<ResolvedTime, Error> {
    let earliest = parse_guardrail(&optimal.earliest, "earliest")?;
    let latest = parse_guardrail(&optimal.latest, "latest")?;
    if earliest >= latest {
        return Err(Error::invalid_argument(format!(
            "send.optimal.earliest ({}) must be before latest ({})",
            optimal.earliest, optimal.latest
        )));
    }

    let local = not_before.with_timezone(&tz);
    // Today if the window is still open, otherwise the first moment of
    // tomorrow's. Bounded by construction: at most one day is skipped.
    let (date, time) = if local.time() < earliest {
        (local.date_naive(), earliest)
    } else if local.time() < latest {
        (
            local.date_naive(),
            local.time().with_nanosecond(0).unwrap_or(local.time()),
        )
    } else {
        (
            local
                .date_naive()
                .checked_add_days(Days::new(1))
                .ok_or_else(|| Error::internal("date overflow computing a send-time suggestion"))?,
            earliest,
        )
    };
    let instant = resolve_local(tz, date.and_time(time))?;
    Ok(present(instant.max(not_before), tz))
}

// ---------------------------------------------------------------------------
// The relative grammar
// ---------------------------------------------------------------------------

/// Parse the natural-language half: `now`, `in 30m`, `tomorrow 9am`,
/// `next monday 8:30am`, `friday 5pm`, `9am`.
fn parse_relative(lower: &str, tz: Tz, now: DateTime<Tz>) -> Result<DateTime<Utc>, Error> {
    let tokens: Vec<&str> = lower
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|t| !t.is_empty())
        // "at" is noise in every position it can appear ("tomorrow at 9am").
        .filter(|t| *t != "at")
        .collect();
    let Some((&head, rest)) = tokens.split_first() else {
        return Err(unparsed(lower));
    };

    match head {
        "now" if rest.is_empty() => return Ok(now.with_timezone(&Utc)),
        "in" => return parse_offset(rest, now).ok_or_else(|| unparsed(lower)),
        _ => {}
    }

    // `next monday`, `next week`. "next" before a weekday means the one in
    // the coming week even if that weekday is today — "next monday" said on a
    // Monday is seven days away, not zero, which is the reading every human
    // uses and the one a calendar would give.
    let (explicit_next, rest_tokens) = match head {
        "next" => (true, rest),
        _ => (false, tokens.as_slice()),
    };
    let Some((&day_token, time_tokens)) = rest_tokens.split_first() else {
        return Err(unparsed(lower));
    };

    let time = match time_tokens {
        [] => None,
        [token] => Some(parse_clock(token).ok_or_else(|| unparsed(lower))?),
        _ => return Err(unparsed(lower)),
    };

    // Set when the day came from a weekday name that resolved to *today*
    // without an explicit "next": "friday 5pm" said at 6pm on a Friday means
    // the coming Friday, not one that has already gone.
    let mut roll_if_past = false;
    let date = match day_token {
        "today" => now.date_naive(),
        "tonight" => now.date_naive(),
        "tomorrow" => now
            .date_naive()
            .checked_add_days(Days::new(1))
            .ok_or_else(|| unparsed(lower))?,
        "week" if explicit_next => now
            .date_naive()
            .checked_add_days(Days::new(7))
            .ok_or_else(|| unparsed(lower))?,
        other => match other.parse::<Weekday>() {
            Ok(weekday) => {
                roll_if_past = !explicit_next && weekday == now.weekday();
                next_weekday(now.date_naive(), weekday, explicit_next)
                    .ok_or_else(|| unparsed(lower))?
            }
            // Not a day at all: the whole expression may be a bare time
            // ("9am", "17:00"), which means the next time it comes round.
            Err(_) => {
                if !time_tokens.is_empty() || explicit_next {
                    return Err(unparsed(lower));
                }
                let clock = parse_clock(other).ok_or_else(|| unparsed(lower))?;
                return next_occurrence(tz, now, clock);
            }
        },
    };

    let time = match (time, day_token) {
        (Some(time), _) => time,
        // A bare day means the start of the working day rather than midnight:
        // "send it monday" does not mean 00:00, and prd.md's own guardrails
        // put the day's earliest send at 08:00.
        (None, "tonight") => NaiveTime::from_hms_opt(20, 0, 0).unwrap_or(NaiveTime::MIN),
        (None, _) => NaiveTime::from_hms_opt(9, 0, 0).unwrap_or(NaiveTime::MIN),
    };
    let instant = resolve_local(tz, date.and_time(time))?;
    if roll_if_past && instant <= now.with_timezone(&Utc) {
        let next_week = date
            .checked_add_days(Days::new(7))
            .ok_or_else(|| unparsed(lower))?;
        return resolve_local(tz, next_week.and_time(time));
    }
    Ok(instant)
}

/// `in 30m`, `in 2 hours`, `in 3d`.
fn parse_offset(tokens: &[&str], now: DateTime<Tz>) -> Option<DateTime<Utc>> {
    let (value, unit) = match tokens {
        // "in 90m"
        [single] => {
            let split = single.find(|c: char| !c.is_ascii_digit())?;
            let (value, unit) = single.split_at(split);
            (value.parse::<i64>().ok()?, unit)
        }
        // "in 90 minutes"
        [value, unit] => (value.parse::<i64>().ok()?, *unit),
        _ => return None,
    };
    let seconds = match unit.trim_end_matches('s') {
        "s" | "sec" | "second" => 1,
        "m" | "min" | "minute" => 60,
        "h" | "hr" | "hour" => 3_600,
        "d" | "day" => 86_400,
        "w" | "week" => 604_800,
        _ => return None,
    };
    let delta = chrono::Duration::try_seconds(value.checked_mul(seconds)?)?;
    now.checked_add_signed(delta)
        .map(|dt| dt.with_timezone(&Utc))
}

/// The next date that falls on `weekday`.
///
/// `explicit_next` forces at least one full week when today already is that
/// weekday — see the caller's comment.
fn next_weekday(today: NaiveDate, weekday: Weekday, explicit_next: bool) -> Option<NaiveDate> {
    let ahead = (weekday.num_days_from_monday() + 7 - today.weekday().num_days_from_monday()) % 7;
    let ahead = if ahead == 0 && explicit_next {
        7
    } else {
        ahead
    };
    today.checked_add_days(Days::new(u64::from(ahead)))
}

/// A bare clock time means the next time it comes round: today if it is still
/// ahead, tomorrow otherwise.
fn next_occurrence(tz: Tz, now: DateTime<Tz>, clock: NaiveTime) -> Result<DateTime<Utc>, Error> {
    let today = resolve_local(tz, now.date_naive().and_time(clock))?;
    if today > now.with_timezone(&Utc) {
        return Ok(today);
    }
    let tomorrow = now
        .date_naive()
        .checked_add_days(Days::new(1))
        .ok_or_else(|| Error::invalid_argument("date overflow"))?;
    resolve_local(tz, tomorrow.and_time(clock))
}

/// `9am`, `9:30pm`, `17:00`, `noon`, `midnight`.
fn parse_clock(token: &str) -> Option<NaiveTime> {
    match token {
        "noon" | "midday" => return NaiveTime::from_hms_opt(12, 0, 0),
        "midnight" => return NaiveTime::from_hms_opt(0, 0, 0),
        _ => {}
    }
    let (body, meridiem) = if let Some(body) = token.strip_suffix("am") {
        (body, Some(false))
    } else if let Some(body) = token.strip_suffix("pm") {
        (body, Some(true))
    } else {
        (token, None)
    };
    let body = body.trim();

    let (hour, minute) = match body.split_once(':') {
        Some((h, m)) => (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?),
        None => (body.parse::<u32>().ok()?, 0),
    };
    let hour = match meridiem {
        // 12am is 00:00 and 12pm is 12:00; every other hour shifts by twelve.
        Some(true) if hour == 12 => 12,
        Some(true) if hour < 12 => hour + 12,
        Some(false) if hour == 12 => 0,
        Some(false) if hour < 12 => hour,
        // "13pm" is not a time. Rejecting beats normalizing it into one.
        Some(_) => return None,
        None => hour,
    };
    NaiveTime::from_hms_opt(hour, minute, 0)
}

/// `HH:MM`, for the `[send.optimal]` guardrails.
fn parse_guardrail(value: &str, field: &str) -> Result<NaiveTime, Error> {
    NaiveTime::parse_from_str(value.trim(), "%H:%M").map_err(|_| {
        Error::invalid_argument(format!("send.optimal.{field} must be HH:MM, got {value:?}"))
    })
}

fn present(instant: DateTime<Utc>, tz: Tz) -> ResolvedTime {
    let local = instant.with_timezone(&tz);
    ResolvedTime {
        at: instant.timestamp(),
        tz: tz.name().to_owned(),
        display: local.to_rfc3339(),
    }
}

fn unparsed(input: &str) -> Error {
    Error::invalid_argument(format!(
        "could not read {input:?} as a time; try an RFC 3339 instant \
         (2026-07-26T09:00:00-07:00), \"tomorrow 9am\", \"next monday 8:30am\", \
         \"friday 5pm\", or \"in 30m\""
    ))
}

#[cfg(test)]
mod tests;
