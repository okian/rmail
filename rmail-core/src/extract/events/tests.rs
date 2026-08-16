//! Calendar extraction: the parser's bounds, the emitter's escaping, and the
//! idempotency that makes delivery safe to retry.
//!
//! Two groups carry most of the weight. The **injection** tests check that a
//! sender's `SUMMARY` cannot become an `ATTENDEE` in the file a calendar
//! application opens — the emitted side is where an extractor's output becomes
//! somebody else's input. The **delivery** tests check the claim-before-effect
//! ordering the module docs argue for, including the case that ordering costs
//! something: a sink that fails must give its claim back.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use super::*;
use crate::repo;
use crate::ErrorReason;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// A database with one account, one mailbox and one message, so the
/// `extraction_deliveries` foreign key has something real to point at.
struct Fixture {
    db: Database,
    message_id: i64,
    path: PathBuf,
}

impl Fixture {
    async fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-extract-events-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", path.display())));
        }
        let db = Database::open(&path).expect("open");
        let message_id = db
            .write(|c| {
                let account_id = repo::insert_account(
                    c,
                    &repo::NewAccount {
                        name: "Personal".to_owned(),
                        ..Default::default()
                    },
                )?;
                let mailbox_id = repo::insert_mailbox(
                    c,
                    &repo::NewMailbox {
                        account_id,
                        name: "INBOX".to_owned(),
                        ..Default::default()
                    },
                )?;
                repo::insert_message(
                    c,
                    &repo::NewMessage {
                        account_id,
                        mailbox_id,
                        uid: 1,
                        uidvalidity: 1,
                        subject: Some("Invite".to_owned()),
                        ..Default::default()
                    },
                )
            })
            .await
            .expect("seed");
        Self {
            db,
            message_id,
            path,
        }
    }

    fn delivery(&self) -> Delivery<'_> {
        Delivery {
            db: &self.db,
            message_id: self.message_id,
        }
    }

    async fn rows(&self) -> Vec<(String, String, String)> {
        self.db
            .read(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT kind, uid, sink FROM extraction_deliveries ORDER BY uid, sink",
                )?;
                let rows = stmt
                    .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await
            .expect("read")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.path.display())));
        }
    }
}

fn calendar(body: &str) -> String {
    format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//test//EN\r\n{body}END:VCALENDAR\r\n")
}

const INVITE: &str = "BEGIN:VEVENT\r\n\
UID:abc-123@example.com\r\n\
SUMMARY:Quarterly review\r\n\
LOCATION:Room 4\r\n\
DTSTART:20240115T140000Z\r\n\
DTEND:20240115T150000Z\r\n\
ORGANIZER:mailto:Ada@Example.COM\r\n\
ATTENDEE:mailto:grace@example.com\r\n\
END:VEVENT\r\n";

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

#[test]
fn an_invite_becomes_a_normalized_event() {
    let report = parse_ics(&calendar(INVITE)).expect("parses");
    let event = report.events.first().expect("one event");
    assert_eq!(event.uid, "abc-123@example.com");
    assert_eq!(event.summary, "Quarterly review");
    assert_eq!(event.location, "Room 4");
    assert_eq!(event.starts_at, 1_705_327_200);
    assert_eq!(event.ends_at, Some(1_705_330_800));
    assert!(!event.all_day);
    assert_eq!(
        event.organizer, "ada@example.com",
        "normalized out of the mailto"
    );
    assert_eq!(event.attendees, vec!["grace@example.com".to_owned()]);
    assert_eq!(event.source, Source::Ics);
    assert_eq!(event.confidence, 1.0);
}

#[test]
fn folded_lines_are_unfolded_before_anything_reads_them() {
    let folded = "BEGIN:VEVENT\r\nUID:u1\r\nSUMMARY:A very long titl\r\n e that was folded\r\nDTSTART:20240115T140000Z\r\nEND:VEVENT\r\n";
    let report = parse_ics(&calendar(folded)).expect("parses");
    assert_eq!(
        report.events[0].summary, "A very long title that was folded",
        "the continuation's leading space is the fold marker, not content"
    );
}

#[test]
fn escaped_text_is_unescaped() {
    let body = "BEGIN:VEVENT\r\nUID:u1\r\nSUMMARY:Review\\, part 1\\; then lunch\r\nDESCRIPTION:line one\\nline two\r\nDTSTART:20240115T140000Z\r\nEND:VEVENT\r\n";
    let report = parse_ics(&calendar(body)).expect("parses");
    assert_eq!(report.events[0].summary, "Review, part 1; then lunch");
    assert_eq!(report.events[0].description, "line one\nline two");
}

#[test]
fn a_date_valued_start_is_a_whole_day() {
    let body = "BEGIN:VEVENT\r\nUID:u1\r\nSUMMARY:Holiday\r\nDTSTART;VALUE=DATE:20240115\r\nEND:VEVENT\r\n";
    let report = parse_ics(&calendar(body)).expect("parses");
    assert!(report.events[0].all_day);
    assert_eq!(report.events[0].starts_at, 1_705_276_800);
}

#[test]
fn a_zoned_start_resolves_through_the_iana_database() {
    let body = "BEGIN:VEVENT\r\nUID:u1\r\nSUMMARY:Call\r\nDTSTART;TZID=America/New_York:20240115T090000\r\nEND:VEVENT\r\n";
    let report = parse_ics(&calendar(body)).expect("parses");
    assert_eq!(
        report.events[0].starts_at, 1_705_327_200,
        "09:00 EST is 14:00 UTC in January"
    );
}

#[test]
fn a_floating_time_is_read_as_utc_so_every_machine_agrees() {
    let body =
        "BEGIN:VEVENT\r\nUID:u1\r\nSUMMARY:Call\r\nDTSTART:20240115T140000\r\nEND:VEVENT\r\n";
    let report = parse_ics(&calendar(body)).expect("parses");
    assert_eq!(report.events[0].starts_at, 1_705_327_200);
}

#[test]
fn a_local_time_that_does_not_exist_is_declined_rather_than_shifted() {
    // 02:30 on the US spring-forward night never happens. Guessing puts a
    // meeting in someone's week at a time nobody agreed to.
    let body = "BEGIN:VEVENT\r\nUID:u1\r\nSUMMARY:Call\r\nDTSTART;TZID=America/New_York:20240310T023000\r\nEND:VEVENT\r\n";
    let report = parse_ics(&calendar(body)).expect("parses");
    assert!(report.events.is_empty());
    assert_eq!(report.skipped, 1, "and the skip is reported");
}

#[test]
fn a_duration_supplies_the_end_when_there_is_no_dtend() {
    let body = "BEGIN:VEVENT\r\nUID:u1\r\nSUMMARY:Call\r\nDTSTART:20240115T140000Z\r\nDURATION:PT1H30M\r\nEND:VEVENT\r\n";
    let report = parse_ics(&calendar(body)).expect("parses");
    assert_eq!(report.events[0].ends_at, Some(1_705_327_200 + 5_400));
}

#[test]
fn a_vtodo_becomes_a_task() {
    let body = "BEGIN:VTODO\r\nUID:t1\r\nSUMMARY:File the return\r\nDUE:20240131T170000Z\r\nPRIORITY:1\r\nSTATUS:NEEDS-ACTION\r\nEND:VTODO\r\n";
    let report = parse_ics(&calendar(body)).expect("parses");
    let task = report.tasks.first().expect("one task");
    assert_eq!(task.summary, "File the return");
    assert_eq!(task.due_at, Some(1_706_720_400));
    assert_eq!(task.priority, 1);
    assert!(!task.completed);
}

#[test]
fn an_alarm_inside_an_event_does_not_overwrite_the_events_properties() {
    let body = "BEGIN:VEVENT\r\nUID:u1\r\nSUMMARY:Real title\r\nDTSTART:20240115T140000Z\r\nBEGIN:VALARM\r\nACTION:DISPLAY\r\nSUMMARY:Alarm text\r\nTRIGGER:-PT15M\r\nEND:VALARM\r\nEND:VEVENT\r\n";
    let report = parse_ics(&calendar(body)).expect("parses");
    assert_eq!(report.events[0].summary, "Real title");
}

#[test]
fn one_broken_component_does_not_cost_the_others() {
    let broken = "BEGIN:VEVENT\r\nUID:bad\r\nSUMMARY:No start at all\r\nEND:VEVENT\r\n";
    let report = parse_ics(&calendar(&format!("{broken}{INVITE}"))).expect("parses");
    assert_eq!(report.events.len(), 1, "the good one survived");
    assert_eq!(report.skipped, 1);
}

#[test]
fn the_method_is_surfaced_so_a_cancel_is_not_read_as_an_invitation() {
    let text =
        format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:CANCEL\r\n{INVITE}END:VCALENDAR\r\n");
    let report = parse_ics(&text).expect("parses");
    assert_eq!(report.method, "CANCEL");
}

#[test]
fn a_cancellation_survives_the_round_trip_rather_than_becoming_an_invitation() {
    // The failure this guards: a METHOD:CANCEL invite piped to Reminders as a
    // brand new appointment. The calendar-level field is not enough on its
    // own, because a consumer handling events one at a time has already lost
    // it — so it lands on the event and is emitted again.
    let text =
        format!("BEGIN:VCALENDAR\r\nVERSION:2.0\r\nMETHOD:CANCEL\r\n{INVITE}END:VCALENDAR\r\n");
    let report = parse_ics(&text).expect("parses");
    assert_eq!(report.method, "CANCEL");
    assert!(
        report.events[0].cancelled,
        "the calendar's METHOD reaches the event"
    );

    let ics = events_to_ics(&report.events);
    assert!(property_names(&ics).contains(&"METHOD".to_owned()), "{ics}");
    assert!(ics.contains("METHOD:CANCEL"), "{ics}");
    assert!(ics.contains("STATUS:CANCELLED"), "{ics}");

    let back = parse_ics(&ics).expect("round trip");
    assert_eq!(back.method, "CANCEL");
    assert!(back.events[0].cancelled);
}

#[test]
fn a_component_level_status_is_read_without_a_calendar_method() {
    let body = "BEGIN:VEVENT\r\nUID:u1\r\nSUMMARY:Call\r\nDTSTART:20240115T140000Z\r\nSTATUS:CANCELLED\r\nEND:VEVENT\r\n";
    let report = parse_ics(&calendar(body)).expect("parses");
    assert!(report.events[0].cancelled);
    assert!(report.method.is_empty(), "the file declared none");
}

#[test]
fn an_ordinary_invite_is_not_emitted_as_a_cancellation() {
    let report = parse_ics(&calendar(INVITE)).expect("parses");
    assert!(!report.events[0].cancelled);
    let ics = events_to_ics(&report.events);
    assert!(!ics.contains("METHOD:CANCEL"), "{ics}");
    assert!(!ics.contains("STATUS:CANCELLED"), "{ics}");
}

#[test]
fn something_that_is_not_a_calendar_is_invalid_argument() {
    let error = parse_ics("just some text").expect_err("declined");
    assert_eq!(error.reason(), ErrorReason::InvalidArgument);
}

#[test]
fn an_oversized_calendar_is_declined_rather_than_read() {
    let error = parse_ics(&"x".repeat(MAX_ICS_BYTES + 1)).expect_err("declined");
    assert_eq!(error.reason(), ErrorReason::InvalidArgument);
}

#[test]
fn deep_nesting_terminates_rather_than_recursing() {
    let mut body = String::new();
    for _ in 0..5_000 {
        body.push_str("BEGIN:VNEST\r\n");
    }
    body.push_str(INVITE);
    for _ in 0..5_000 {
        body.push_str("END:VNEST\r\n");
    }
    let report = parse_ics(&calendar(&body)).expect("parses");
    assert!(
        report.events.is_empty(),
        "nothing at that depth is a component"
    );
}

#[test]
fn a_property_folded_across_thousands_of_lines_is_bounded() {
    // The classic amplification: one property, unbounded memory. Deliberately
    // kept under MAX_ICS_BYTES so it is the *per-property* cap being tested and
    // not the document cap, which would pass this test without the fold bound
    // existing at all.
    let mut body =
        String::from("BEGIN:VEVENT\r\nUID:u1\r\nDTSTART:20240115T140000Z\r\nSUMMARY:a\r\n");
    for _ in 0..25_000 {
        body.push_str(" bbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\r\n");
    }
    body.push_str("END:VEVENT\r\n");
    let report = parse_ics(&calendar(&body)).expect("parses");
    let summary = &report.events[0].summary;
    assert!(
        summary.len() <= MAX_PROPERTY_BYTES,
        "the unfolded value is capped, not accumulated: {} bytes",
        summary.len()
    );
}

#[test]
fn an_over_long_line_cut_through_a_multi_byte_character_does_not_abort_the_parse() {
    // The property cap is a *byte* cap, and `&raw[..8192]` panics when byte
    // 8192 lands inside a character. The padding is sized so an `é` straddles
    // exactly that boundary — the parse must survive and the value must still
    // be valid UTF-8.
    // `SUMMARY:` is 8 bytes, so the `é` has to start at raw byte 8191 for
    // byte 8192 — the cut — to land inside it. One byte either way and the cut
    // is on a boundary and the probe does not bite.
    let padding = "a".repeat(MAX_PROPERTY_BYTES - "SUMMARY:".len() - 1);
    let long = format!("{padding}\u{e9}{}", "b".repeat(200));
    let body = format!(
        "BEGIN:VEVENT\r\nUID:u1\r\nDTSTART:20240115T140000Z\r\nSUMMARY:{long}\r\nEND:VEVENT\r\n"
    );
    assert!(
        !format!("SUMMARY:{long}").is_char_boundary(MAX_PROPERTY_BYTES),
        "the probe must straddle the cut, or it proves nothing"
    );
    let report = parse_ics(&calendar(&body)).expect("parses rather than panicking");
    let summary = &report.events[0].summary;
    assert!(summary.len() <= MAX_PROPERTY_BYTES);
    assert!(
        summary.starts_with("aaaa"),
        "and it is the value that was cut"
    );
}

#[test]
fn the_component_cap_bounds_a_calendar_of_a_million_events() {
    let mut body = String::new();
    for index in 0..(MAX_COMPONENTS + 200) {
        body.push_str(&format!(
            "BEGIN:VEVENT\r\nUID:u{index}\r\nSUMMARY:E{index}\r\nDTSTART:20240115T140000Z\r\nEND:VEVENT\r\n"
        ));
    }
    let report = parse_ics(&calendar(&body)).expect("parses");
    assert!(report.events.len() <= MAX_COMPONENTS);
    assert!(report.skipped > 0);
}

#[test]
fn the_attendee_cap_bounds_one_event() {
    let mut body =
        String::from("BEGIN:VEVENT\r\nUID:u1\r\nSUMMARY:Big\r\nDTSTART:20240115T140000Z\r\n");
    for index in 0..(MAX_ATTENDEES + 100) {
        body.push_str(&format!("ATTENDEE:mailto:a{index}@example.com\r\n"));
    }
    body.push_str("END:VEVENT\r\n");
    let report = parse_ics(&calendar(&body)).expect("parses");
    assert!(report.events[0].attendees.len() <= MAX_ATTENDEES);
}

#[test]
fn a_quoted_parameter_containing_a_colon_does_not_truncate_the_value() {
    let body = "BEGIN:VEVENT\r\nUID:u1\r\nSUMMARY:Call\r\nDTSTART;TZID=\"America/New_York\":20240115T090000\r\nEND:VEVENT\r\n";
    let report = parse_ics(&calendar(body)).expect("parses");
    assert_eq!(report.events[0].starts_at, 1_705_327_200);
}

// ---------------------------------------------------------------------------
// Emission: the injection surface
// ---------------------------------------------------------------------------

fn event(summary: &str) -> Event {
    Event {
        cancelled: false,
        uid: "u1".to_owned(),
        summary: summary.to_owned(),
        description: String::new(),
        location: String::new(),
        starts_at: 1_705_327_200,
        ends_at: Some(1_705_330_800),
        all_day: false,
        organizer: String::new(),
        attendees: Vec::new(),
        rrule: String::new(),
        source: Source::Ics,
        confidence: 1.0,
    }
}

#[test]
fn a_summary_carrying_a_newline_cannot_become_a_property() {
    // The whole point: a calendar application parses what this daemon writes.
    let hostile = "Lunch\r\nATTENDEE:mailto:attacker@evil.example\r\nX-EVIL:1";
    let ics = events_to_ics(&[event(hostile)]);
    assert!(
        !ics.contains("\r\nATTENDEE:mailto:attacker@evil.example"),
        "the injected property survived: {ics}"
    );
    assert!(ics.contains("\\nATTENDEE"), "it is escaped, not dropped");

    // And it round-trips back to exactly the text that was written.
    let back = parse_ics(&ics).expect("our own output parses");
    assert_eq!(back.events.len(), 1);
    assert!(
        back.events[0].attendees.is_empty(),
        "no attendee was created"
    );
    assert_eq!(back.events[0].summary, hostile.replace("\r\n", "\n"));
}

#[test]
fn a_semicolon_or_comma_in_text_cannot_open_a_parameter_or_a_list() {
    let ics = events_to_ics(&[event("Review; part 1, then lunch")]);
    assert!(ics.contains("SUMMARY:Review\\; part 1\\, then lunch"));
    let back = parse_ics(&ics).expect("round trip");
    assert_eq!(back.events[0].summary, "Review; part 1, then lunch");
}

/// Every property name a line in `ics` begins with. The assertion surface for
/// "no new property was created": what matters is not whether the injected
/// *characters* survive somewhere inside a value, but whether any of them
/// became a property in their own right.
fn property_names(ics: &str) -> Vec<String> {
    ics.split("\r\n")
        .filter(|line| !line.starts_with(' '))
        .filter_map(|line| line.split(&[':', ';'][..]).next())
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .collect()
}

#[test]
fn an_attendee_address_cannot_inject_a_property_either() {
    let mut hostile = event("Lunch");
    hostile.organizer = "ada@example.com\r\nX-EVIL:1".to_owned();
    hostile.attendees =
        vec!["grace@example.com\r\nATTENDEE:mailto:attacker@evil.example".to_owned()];
    let ics = events_to_ics(&[hostile]);
    assert!(
        !property_names(&ics).contains(&"X-EVIL".to_owned()),
        "an address created a property: {ics}"
    );
    assert_eq!(
        property_names(&ics)
            .iter()
            .filter(|name| *name == "ATTENDEE")
            .count(),
        1,
        "and only the one attendee that was set: {ics}"
    );
    let back = parse_ics(&ics).expect("round trip");
    assert_eq!(back.events[0].attendees.len(), 1);
    assert!(
        !back.events[0]
            .attendees
            .iter()
            .any(|address| address == "attacker@evil.example"),
        "the injected address is inert text run together with the one \
         legitimate value, never an addressable attendee of its own: {:?}",
        back.events[0].attendees
    );
}

#[test]
fn an_rrule_is_reduced_to_the_characters_its_grammar_allows() {
    let mut recurring = event("Standup");
    recurring.rrule = "FREQ=WEEKLY;BYDAY=MO\r\nX-EVIL:1".to_owned();
    let ics = events_to_ics(&[recurring]);
    assert!(
        !property_names(&ics).contains(&"X-EVIL".to_owned()),
        "an rrule created a property: {ics}"
    );
    assert!(
        ics.contains("RRULE:FREQ=WEEKLY;BYDAY=MOX-EVIL1"),
        "the rule itself survives, minus the characters that could end the line: {ics}"
    );
}

#[test]
fn long_lines_are_folded_on_character_boundaries() {
    let long = "é".repeat(300);
    let ics = events_to_ics(&[event(&long)]);
    assert!(ics.is_char_boundary(ics.len()), "still valid utf-8");
    for line in ics.split("\r\n") {
        assert!(
            line.len() <= 75,
            "a content line is at most 75 octets: {} bytes",
            line.len()
        );
    }
    let back = parse_ics(&ics).expect("round trip");
    assert_eq!(back.events[0].summary, long, "and folding is lossless");
}

#[test]
fn a_task_round_trips_through_its_own_ics() {
    let task = Task {
        uid: "t1".to_owned(),
        summary: "File the return".to_owned(),
        description: "Before the deadline".to_owned(),
        due_at: Some(1_706_720_400),
        priority: 2,
        completed: false,
        source: Source::Model,
        confidence: 0.7,
    };
    let ics = tasks_to_ics(std::slice::from_ref(&task));
    let back = parse_ics(&ics).expect("round trip");
    let parsed = back.tasks.first().expect("one task");
    assert_eq!(parsed.summary, task.summary);
    assert_eq!(parsed.due_at, task.due_at);
    assert_eq!(parsed.priority, task.priority);
    assert!(!parsed.completed);
}

#[test]
fn a_synthesized_uid_is_stable_and_addresses_its_content() {
    let a = synthesize_uid(7, "event", "Quarterly review", Some(1_705_327_200));
    assert_eq!(
        a,
        synthesize_uid(7, "event", "Quarterly review", Some(1_705_327_200)),
        "extracting twice must not deliver twice"
    );
    assert_ne!(
        a,
        synthesize_uid(8, "event", "Quarterly review", Some(1_705_327_200))
    );
    assert_ne!(
        a,
        synthesize_uid(7, "task", "Quarterly review", Some(1_705_327_200))
    );
    assert_ne!(
        a,
        synthesize_uid(7, "event", "Quarterly review", Some(1_705_330_800)),
        "a corrected time is a different item"
    );
}

// ---------------------------------------------------------------------------
// The model route
// ---------------------------------------------------------------------------

#[test]
fn a_model_answer_becomes_events_and_tasks_marked_as_inferred() {
    let json = serde_json::json!({
        "events": [{
            "summary": "Coffee",
            "description": "",
            "location": "Cafe",
            "starts_at": "2024-01-15T09:00:00-05:00",
            "ends_at": "2024-01-15T10:00:00-05:00",
            "all_day": false,
            "confidence": 0.8,
        }],
        "tasks": [{
            "summary": "Send the deck",
            "description": "",
            "due_at": "2024-01-16T17:00:00Z",
            "priority": 3,
            "confidence": 0.6,
        }],
    });
    let report = from_model_answer(42, &json.to_string()).expect("parses");
    let event = report.events.first().expect("one event");
    assert_eq!(event.source, Source::Model);
    assert_eq!(event.starts_at, 1_705_327_200);
    assert_eq!(event.confidence, 0.8);
    assert!(
        !event.uid.is_empty(),
        "an inferred item still gets an identity"
    );
    assert_eq!(report.tasks[0].priority, 3);
    assert_eq!(report.tasks[0].source, Source::Model);
}

#[test]
fn an_inferred_item_with_no_usable_time_or_title_is_dropped_not_repaired() {
    let json = serde_json::json!({
        "events": [
            {"summary": "No time", "description": "", "location": "", "starts_at": "", "ends_at": "", "all_day": false, "confidence": 0.9},
            {"summary": "", "description": "", "location": "", "starts_at": "2024-01-15T09:00:00Z", "ends_at": "", "all_day": false, "confidence": 0.9},
        ],
        "tasks": [],
    });
    let report = from_model_answer(1, &json.to_string()).expect("parses");
    assert!(
        report.events.is_empty(),
        "an entry called \"\" at the epoch is worse than no entry"
    );
    assert_eq!(report.skipped, 2);
}

#[test]
fn a_model_confidence_and_priority_are_clamped_to_their_ranges() {
    let json = serde_json::json!({
        "events": [{"summary": "X", "description": "", "location": "", "starts_at": "2024-01-15T09:00:00Z", "ends_at": "", "all_day": false, "confidence": 42.0}],
        "tasks": [{"summary": "Y", "description": "", "due_at": "", "priority": 900, "confidence": -3.0}],
    });
    let report = from_model_answer(1, &json.to_string()).expect("parses");
    assert_eq!(report.events[0].confidence, 1.0);
    assert_eq!(report.tasks[0].confidence, 0.0);
    assert_eq!(report.tasks[0].priority, 9);
}

#[test]
fn a_model_answer_that_is_not_the_requested_schema_is_an_internal_error() {
    let error = from_model_answer(1, "{nope").expect_err("declined");
    assert_eq!(error.reason(), ErrorReason::Internal);
}

// ---------------------------------------------------------------------------
// Delivery
// ---------------------------------------------------------------------------

/// A renderer over named events: the `.ics` for whichever uids it is handed.
/// This is the shape `Delivery::deliver` takes, and it is what lets a test see
/// *which* events reached the sink rather than only how many.
fn renderer<'a>(items: &'a [(&'a str, &'a str)]) -> impl Fn(&[String]) -> String + Sync + 'a {
    move |wanted: &[String]| {
        let selected: Vec<Event> = items
            .iter()
            .filter(|(uid, _)| wanted.iter().any(|w| w == uid))
            .map(|(uid, summary)| Event {
                uid: (*uid).to_owned(),
                ..event(summary)
            })
            .collect();
        events_to_ics(&selected)
    }
}

#[tokio::test]
async fn delivery_is_idempotent_per_message() {
    let fx = Fixture::open().await;
    let render = renderer(&[("u1", "Quarterly review")]);
    let cancel = CancellationToken::new();

    let first = fx
        .delivery()
        .deliver("event", &["u1".to_owned()], &render, &Sink::Ics, &cancel)
        .await
        .expect("first delivery");
    assert_eq!(first.delivered, 1);
    assert_eq!(first.skipped, 0);

    let second = fx
        .delivery()
        .deliver("event", &["u1".to_owned()], &render, &Sink::Ics, &cancel)
        .await
        .expect("second delivery");
    assert_eq!(second.delivered, 0, "the same item is not delivered twice");
    assert_eq!(second.skipped, 1);
    assert!(
        !second.ics.is_empty(),
        "but asking for the file again still returns the file"
    );
    assert_eq!(fx.rows().await.len(), 1, "and exactly one claim exists");
}

#[tokio::test]
async fn two_sinks_do_not_suppress_each_other() {
    let fx = Fixture::open().await;
    let cancel = CancellationToken::new();
    let uids = ["u1".to_owned()];
    let render = renderer(&[("u1", "Quarterly review")]);
    fx.delivery()
        .deliver("event", &uids, &render, &Sink::Ics, &cancel)
        .await
        .expect("ics");
    let claimed = fx
        .delivery()
        .claim("event", &uids, "webhook")
        .await
        .expect("claim");
    assert_eq!(
        claimed.len(),
        1,
        "the same event legitimately goes to two places"
    );
    assert_eq!(fx.rows().await.len(), 2);
}

#[tokio::test]
async fn a_failed_sink_gives_its_claim_back_so_the_item_can_be_retried() {
    let fx = Fixture::open().await;
    let cancel = CancellationToken::new();
    let sink = Sink::Command {
        // A command that exits non-zero without reading stdin.
        command: "false".to_owned(),
        args: Vec::new(),
    };
    let render = renderer(&[("u1", "Quarterly review")]);
    let error = fx
        .delivery()
        .deliver("event", &["u1".to_owned()], &render, &sink, &cancel)
        .await
        .expect_err("the sink failed");
    assert_eq!(error.reason(), ErrorReason::Unavailable);
    assert!(
        fx.rows().await.is_empty(),
        "a claim for something that never happened would strand the item for ever"
    );
}

#[tokio::test]
async fn a_command_sink_receives_the_ics_on_its_stdin() {
    let fx = Fixture::open().await;
    let cancel = CancellationToken::new();
    let sink = Sink::Command {
        command: "cat".to_owned(),
        args: Vec::new(),
    };
    let render = renderer(&[("u1", "Quarterly review")]);
    let report = fx
        .delivery()
        .deliver("event", &["u1".to_owned()], &render, &sink, &cancel)
        .await
        .expect("delivered");
    assert!(
        report.output.contains("SUMMARY:Quarterly review"),
        "the calendar reached the command's stdin: {:?}",
        report.output
    );
}

#[tokio::test]
async fn more_items_than_the_cap_is_declined_before_anything_is_claimed() {
    let fx = Fixture::open().await;
    let uids: Vec<String> = (0..MAX_DELIVERY_ITEMS + 5)
        .map(|index| format!("u{index}"))
        .collect();
    let render = renderer(&[]);
    let error = fx
        .delivery()
        .deliver(
            "event",
            &uids,
            &render,
            &Sink::Command {
                command: "cat".to_owned(),
                args: Vec::new(),
            },
            &CancellationToken::new(),
        )
        .await
        .expect_err("declined");
    assert_eq!(error.reason(), ErrorReason::InvalidArgument);
    assert!(fx.rows().await.is_empty(), "and nothing was claimed");
}

#[tokio::test]
async fn a_partial_overlap_claims_only_what_is_new() {
    let fx = Fixture::open().await;
    let cancel = CancellationToken::new();
    let render = renderer(&[("u1", "First meeting"), ("u2", "Second meeting")]);
    let sink = Sink::Command {
        // `cat` echoes its stdin, so the test can see exactly what was pushed
        // rather than only how many items the counters claim.
        command: "cat".to_owned(),
        args: Vec::new(),
    };
    let first = fx
        .delivery()
        .deliver("event", &["u1".to_owned()], &render, &sink, &cancel)
        .await
        .expect("first");
    assert!(first.output.contains("First meeting"));

    let second = fx
        .delivery()
        .deliver(
            "event",
            &["u1".to_owned(), "u2".to_owned()],
            &render,
            &sink,
            &cancel,
        )
        .await
        .expect("second");
    assert_eq!(second.delivered, 1);
    assert_eq!(second.skipped, 1);
    assert!(
        second.output.contains("Second meeting"),
        "the new item was pushed: {:?}",
        second.output
    );
    assert!(
        !second.output.contains("First meeting"),
        "and the already-delivered one was not pushed again — a sink payload \
         rendered from everything hands back the idempotency the claim table \
         just bought: {:?}",
        second.output
    );
    assert!(
        second.ics.contains("First meeting") && second.ics.contains("Second meeting"),
        "while the caller's own file still describes the whole message"
    );
}

// ---------------------------------------------------------------------------
// The wire vocabulary
// ---------------------------------------------------------------------------

#[test]
fn every_source_round_trips_through_its_string_form() {
    for source in Source::ALL {
        assert_eq!(Source::parse(source.as_str()), Some(source));
    }
    assert_eq!(Source::parse("vision"), None);
}
