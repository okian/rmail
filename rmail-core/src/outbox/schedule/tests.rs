//! The grammar, and the DST cases it exists to get right.
//!
//! Every expectation below is a literal UTC string rather than a value
//! re-derived with the same `chrono_tz` call the code under test makes — the
//! second form would agree with any bug that lives in the conversion itself,
//! which is exactly where the bug would be.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use crate::ErrorReason;

/// The zone every DST case below uses. Its 2026 transitions are 08 March
/// (02:00 PST -> 03:00 PDT) and 01 November (02:00 PDT -> 01:00 PST).
fn la() -> Tz {
    chrono_tz::America::Los_Angeles
}

fn at(rfc3339: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(rfc3339)
        .unwrap()
        .with_timezone(&Utc)
}

/// The resolved instant, rendered in UTC — the form the assertions compare.
fn utc(resolved: &ResolvedTime) -> String {
    DateTime::from_timestamp(resolved.at, 0)
        .unwrap()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn resolve(input: &str, now: &str) -> ResolvedTime {
    resolve_send_at(input, la(), at(now)).unwrap()
}

// ---------------------------------------------------------------------------
// DST
// ---------------------------------------------------------------------------

#[test]
fn a_schedule_across_a_spring_forward_lands_on_the_wall_clock_time_asked_for() {
    // 09:00 the day before the transition is PST (UTC-8) -> 17:00Z.
    assert_eq!(
        utc(&resolve("2026-03-07 09:00", "2026-03-06T12:00:00-08:00")),
        "2026-03-07T17:00:00Z"
    );
    // 09:00 the day *of* the transition is PDT (UTC-7) -> 16:00Z. Twenty-three
    // hours after the previous case, not twenty-four: this is the single
    // number that naive local-time arithmetic gets wrong, and it gets it wrong
    // silently.
    assert_eq!(
        utc(&resolve("2026-03-08 09:00", "2026-03-06T12:00:00-08:00")),
        "2026-03-08T16:00:00Z"
    );
}

#[test]
fn tomorrow_9am_across_a_spring_forward_is_twenty_hours_away_not_twenty_one() {
    // Asked on the Saturday at noon PST (20:00Z) for "tomorrow 9am", which is
    // Sunday 09:00 PDT = 16:00Z — twenty hours later. Anything that adds a
    // fixed 86 400 seconds and then sets the clock, or that resolves the zone
    // offset once and reuses it, produces 17:00Z and sends an hour late.
    let resolved = resolve("tomorrow 9am", "2026-03-07T12:00:00-08:00");
    assert_eq!(utc(&resolved), "2026-03-08T16:00:00Z");
    assert_eq!(
        resolved.at - at("2026-03-07T12:00:00-08:00").timestamp(),
        20 * 3600
    );
}

#[test]
fn a_wall_clock_time_that_does_not_exist_moves_forward_past_the_gap() {
    // 02:30 on 08 March never happens in Los Angeles. Rejecting would be
    // defensible; sending an hour *early* would not, so the resolution steps
    // to the first instant on the far side.
    assert_eq!(
        utc(&resolve("2026-03-08 02:30", "2026-03-01T00:00:00-08:00")),
        "2026-03-08T10:30:00Z"
    );
}

#[test]
fn an_ambiguous_wall_clock_time_takes_the_earlier_of_the_two() {
    // 01:30 on 01 November happens twice: 08:30Z (PDT) and 09:30Z (PST).
    // "01:30" means the first one — never later than the user expected.
    assert_eq!(
        utc(&resolve("2026-11-01 01:30", "2026-10-01T00:00:00-07:00")),
        "2026-11-01T08:30:00Z"
    );
}

#[test]
fn the_zone_is_carried_for_display_and_the_instant_is_what_is_stored() {
    let resolved = resolve("2026-03-08 09:00", "2026-03-06T12:00:00-08:00");
    assert_eq!(resolved.tz, "America/Los_Angeles");
    assert_eq!(resolved.display, "2026-03-08T09:00:00-07:00");
}

// ---------------------------------------------------------------------------
// Absolute forms
// ---------------------------------------------------------------------------

#[test]
fn an_expression_that_names_its_own_offset_ignores_the_configured_zone() {
    let resolved = resolve_send_at(
        "2026-07-26T09:00:00-07:00",
        chrono_tz::Europe::Berlin,
        at("2026-01-01T00:00:00Z"),
    )
    .unwrap();
    assert_eq!(utc(&resolved), "2026-07-26T16:00:00Z");
    // The zone still travels, because that is what the display echo uses.
    assert_eq!(resolved.tz, "Europe/Berlin");
}

#[test]
fn naive_date_and_datetime_forms_are_read_in_the_configured_zone() {
    assert_eq!(
        utc(&resolve("2026-07-26T09:00:00", "2026-01-01T00:00:00Z")),
        "2026-07-26T16:00:00Z"
    );
    assert_eq!(
        utc(&resolve("2026-07-26 09:00", "2026-01-01T00:00:00Z")),
        "2026-07-26T16:00:00Z"
    );
    // A bare date is midnight, not the start of the working day: a date with
    // no time is an instruction about the day, and 00:00 is where a day
    // begins.
    assert_eq!(
        utc(&resolve("2026-07-26", "2026-01-01T00:00:00Z")),
        "2026-07-26T07:00:00Z"
    );
}

// ---------------------------------------------------------------------------
// The relative grammar
// ---------------------------------------------------------------------------

#[test]
fn now_and_offsets() {
    let now = "2026-07-26T12:00:00-07:00";
    assert_eq!(utc(&resolve("now", now)), "2026-07-26T19:00:00Z");
    assert_eq!(utc(&resolve("in 30m", now)), "2026-07-26T19:30:00Z");
    assert_eq!(utc(&resolve("in 90 minutes", now)), "2026-07-26T20:30:00Z");
    assert_eq!(utc(&resolve("in 2h", now)), "2026-07-26T21:00:00Z");
    assert_eq!(utc(&resolve("in 3 days", now)), "2026-07-29T19:00:00Z");
    assert_eq!(utc(&resolve("in 1w", now)), "2026-08-02T19:00:00Z");
}

#[test]
fn today_and_tomorrow() {
    let now = "2026-07-26T12:00:00-07:00"; // a Sunday
    assert_eq!(utc(&resolve("today 5pm", now)), "2026-07-27T00:00:00Z");
    assert_eq!(utc(&resolve("tomorrow 9am", now)), "2026-07-27T16:00:00Z");
    assert_eq!(
        utc(&resolve("tomorrow at 9:30am", now)),
        "2026-07-27T16:30:00Z"
    );
    // A bare day means the start of the working day, not midnight.
    assert_eq!(utc(&resolve("tomorrow", now)), "2026-07-27T16:00:00Z");
    assert_eq!(utc(&resolve("tonight", now)), "2026-07-27T03:00:00Z");
}

#[test]
fn weekdays_and_the_meaning_of_next() {
    // 2026-07-26 is a Sunday.
    let now = "2026-07-26T12:00:00-07:00";
    assert_eq!(utc(&resolve("monday 9am", now)), "2026-07-27T16:00:00Z");
    // 17:00 PDT on Friday the 31st is midnight UTC on the 1st.
    assert_eq!(utc(&resolve("friday 5pm", now)), "2026-08-01T00:00:00Z");
    assert_eq!(
        utc(&resolve("next monday 8:30am", now)),
        "2026-07-27T15:30:00Z"
    );
    // A weekday naming *today* at a time that has already gone rolls to the
    // coming week: "sunday 9am" said at Sunday noon cannot mean three hours
    // ago, and scheduling into the past would send it immediately.
    assert_eq!(utc(&resolve("sunday 9am", now)), "2026-08-02T16:00:00Z");
    // ... but a time still ahead today is today.
    assert_eq!(utc(&resolve("sunday 5pm", now)), "2026-07-27T00:00:00Z");
    // "next sunday" is always a week away, however the clock stands.
    assert_eq!(
        utc(&resolve("next sunday 9am", now)),
        "2026-08-02T16:00:00Z"
    );
    assert_eq!(
        utc(&resolve("next sunday 5pm", now)),
        "2026-08-03T00:00:00Z"
    );
    assert_eq!(utc(&resolve("next week", now)), "2026-08-02T16:00:00Z");
}

#[test]
fn a_bare_clock_time_means_the_next_time_it_comes_round() {
    let now = "2026-07-26T12:00:00-07:00";
    // Still ahead today.
    assert_eq!(utc(&resolve("5pm", now)), "2026-07-27T00:00:00Z");
    // Already past: tomorrow.
    assert_eq!(utc(&resolve("9am", now)), "2026-07-27T16:00:00Z");
    assert_eq!(utc(&resolve("17:00", now)), "2026-07-27T00:00:00Z");
    assert_eq!(utc(&resolve("noon", now)), "2026-07-27T19:00:00Z");
    assert_eq!(utc(&resolve("midnight", now)), "2026-07-27T07:00:00Z");
}

#[test]
fn the_twelve_hour_clock_handles_its_two_special_hours() {
    let now = "2026-07-26T01:00:00-07:00";
    assert_eq!(utc(&resolve("12am", now)), "2026-07-27T07:00:00Z");
    assert_eq!(utc(&resolve("12pm", now)), "2026-07-26T19:00:00Z");
    // "13pm" is not a time; normalizing it into one would be inventing an
    // instant a user never named.
    assert!(resolve_send_at("13pm", la(), at(now)).is_err());
}

// ---------------------------------------------------------------------------
// Refusals
// ---------------------------------------------------------------------------

#[test]
fn an_expression_the_grammar_does_not_cover_is_refused_rather_than_guessed() {
    for input in [
        "",
        "   ",
        "sometime next quarter",
        "the second tuesday after the retro",
        "in a bit",
        "in -5m",
        "yesterday 9am",
    ] {
        let error = resolve_send_at(input, la(), at("2026-07-26T12:00:00-07:00")).unwrap_err();
        assert_eq!(
            error.reason(),
            ErrorReason::InvalidArgument,
            "input {input:?} should be refused"
        );
    }
}

#[test]
fn optimal_and_recipient_are_questions_for_other_rpcs_not_times() {
    let now = at("2026-07-26T12:00:00-07:00");
    let optimal = resolve_send_at("optimal", la(), now).unwrap_err();
    assert!(
        optimal.to_string().contains("SuggestSendTime"),
        "the refusal must name what to call instead: {optimal}"
    );
    let recipient = resolve_send_at("recipient 9am", la(), now).unwrap_err();
    assert_eq!(recipient.reason(), ErrorReason::InvalidArgument);
}

#[test]
fn an_unknown_timezone_is_refused_and_an_empty_one_is_utc() {
    assert_eq!(parse_timezone("").unwrap(), Tz::UTC);
    assert_eq!(
        parse_timezone("America/Los_Angeles").unwrap(),
        chrono_tz::America::Los_Angeles
    );
    assert_eq!(
        parse_timezone("Middle/Earth").unwrap_err().reason(),
        ErrorReason::InvalidArgument
    );
}

// ---------------------------------------------------------------------------
// Suggestions
// ---------------------------------------------------------------------------

#[test]
fn a_suggestion_is_clamped_into_the_configured_window() {
    let optimal = SendOptimal::default(); // 08:00-18:00
                                          // Before the window opens: its start, today.
    assert_eq!(
        utc(&suggest_send_time(&optimal, la(), at("2026-07-26T06:00:00-07:00")).unwrap()),
        "2026-07-26T15:00:00Z"
    );
    // Inside it: now.
    assert_eq!(
        utc(&suggest_send_time(&optimal, la(), at("2026-07-26T10:00:00-07:00")).unwrap()),
        "2026-07-26T17:00:00Z"
    );
    // After it closes: tomorrow's opening.
    assert_eq!(
        utc(&suggest_send_time(&optimal, la(), at("2026-07-26T20:00:00-07:00")).unwrap()),
        "2026-07-27T15:00:00Z"
    );
}

#[test]
fn a_window_that_is_empty_or_malformed_is_reported_rather_than_repaired() {
    let base = SendOptimal::default();
    for (earliest, latest) in [
        ("18:00", "08:00"),
        ("09:00", "09:00"),
        ("not a time", "18:00"),
        ("08:00", "25:00"),
    ] {
        let optimal = SendOptimal {
            earliest: earliest.to_owned(),
            latest: latest.to_owned(),
            ..base.clone()
        };
        assert_eq!(
            suggest_send_time(&optimal, la(), at("2026-07-26T10:00:00-07:00"))
                .unwrap_err()
                .reason(),
            ErrorReason::InvalidArgument,
            "window {earliest}..{latest}"
        );
    }
}
