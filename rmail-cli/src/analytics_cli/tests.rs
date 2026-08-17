//! Rendering and flag-translation tests for `mail contact`/`subs`/`ask`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use rmail_proto::v1::{
    AnalyticsCell, AnalyticsRow, ContactCadence, ContactDecay, ContactTopic, ContactVolume,
    ResponseStats, Unsubscribe,
};

#[test]
fn durations_render_at_the_coarsest_honest_unit() {
    assert_eq!(duration(0), "-");
    assert_eq!(duration(-5), "-");
    assert_eq!(duration(45), "45s");
    assert_eq!(duration(600), "10m");
    assert_eq!(duration(7_200), "2h");
    assert_eq!(duration(3 * 86_400), "3d");
}

#[test]
fn a_null_cell_renders_as_nothing_and_not_as_the_word_null() {
    let cell = AnalyticsCell {
        value: Some(CellValue::NullValue(true)),
    };
    assert_eq!(render_cell(&cell), "");
    assert_eq!(cell_json(&cell), serde_json::Value::Null);
}

/// A cell with no `value` at all is what a client from a future proto
/// revision could send. It must not panic and must not be mistaken for text.
#[test]
fn an_absent_cell_value_is_empty_rather_than_a_panic() {
    let cell = AnalyticsCell { value: None };
    assert_eq!(render_cell(&cell), "");
    assert_eq!(cell_json(&cell), serde_json::Value::Null);
}

#[test]
fn a_result_table_is_column_aligned() {
    let answer = AskAnalyticsResponse {
        columns: vec!["sender".to_owned(), "messages".to_owned()],
        rows: vec![
            AnalyticsRow {
                cells: vec![
                    AnalyticsCell {
                        value: Some(CellValue::TextValue("a-very-long@example.com".to_owned())),
                    },
                    AnalyticsCell {
                        value: Some(CellValue::IntegerValue(12)),
                    },
                ],
            },
            AnalyticsRow {
                cells: vec![
                    AnalyticsCell {
                        value: Some(CellValue::TextValue("b@example.com".to_owned())),
                    },
                    AnalyticsCell {
                        value: Some(CellValue::IntegerValue(3)),
                    },
                ],
            },
        ],
        ..AskAnalyticsResponse::default()
    };
    let mut out: Vec<u8> = Vec::new();
    print_answer(&mut out, &answer, false).unwrap();
    let text = String::from_utf8(out).unwrap();
    let lines: Vec<&str> = text.lines().filter(|l| !l.is_empty()).collect();
    // Every rendered row starts its second column at the same offset.
    //
    // Measured as "end of the first field, then skip the padding" rather than
    // as `find("  ") + 2`, which was the original and was measuring the wrong
    // thing: the first double-space in `sender<19 spaces>messages` is at index
    // 6, inside the padding, so a correctly aligned table reported offsets of
    // 8, 25 and 15 and the test failed on output that was in fact aligned.
    fn second_column_start(line: &str) -> usize {
        let first_field_end = line.find(' ').unwrap_or(line.len());
        line[first_field_end..]
            .find(|c: char| c != ' ')
            .map_or(line.len(), |offset| first_field_end + offset)
    }
    let offsets: Vec<usize> = lines.iter().map(|line| second_column_start(line)).collect();
    assert!(
        offsets.windows(2).all(|w| w[0] == w[1]),
        "columns are not aligned: {lines:?}"
    );
}

#[test]
fn explain_prints_the_sql_and_its_parameters() {
    let answer = AskAnalyticsResponse {
        sql: "SELECT count(*) AS n FROM analytics_messages WHERE sent_at >= ?".to_owned(),
        params: vec!["integer:1700000000".to_owned()],
        ..AskAnalyticsResponse::default()
    };
    let mut out: Vec<u8> = Vec::new();
    print_answer(&mut out, &answer, true).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("analytics_messages"), "{text}");
    assert!(text.contains("integer:1700000000"), "{text}");

    let mut quiet: Vec<u8> = Vec::new();
    print_answer(&mut quiet, &answer, false).unwrap();
    assert!(!String::from_utf8(quiet)
        .unwrap()
        .contains("analytics_messages"));
}

/// The report must say, in the rendered output, that rmail did not and will
/// not act on the link. A URL printed with no such line reads as an offer.
#[test]
fn the_unsubscribe_line_says_rmail_does_not_act_on_it() {
    let report = ListSubscriptionsResponse {
        senders: vec![SubscriptionSender {
            address: "news@example.com".to_owned(),
            messages: 20,
            read_messages: 1,
            read_rate: 0.05,
            sender_class: SubscriptionClass::Newsletter as i32,
            source: SubscriptionSource::Header as i32,
            signals: vec!["list-unsubscribe".to_owned()],
            unsubscribe: Some(Unsubscribe {
                http_url: "https://example.com/u/abc".to_owned(),
                mailto: String::new(),
                one_click: true,
            }),
            headers_read: true,
            candidate: true,
            ..SubscriptionSender::default()
        }],
        total_senders: 1,
        ..ListSubscriptionsResponse::default()
    };
    let mut out: Vec<u8> = Vec::new();
    print_subs(&mut out, &report).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("https://example.com/u/abc"), "{text}");
    assert!(text.contains("one-click"), "{text}");
    assert!(
        text.contains("rmail does not act on these"),
        "the URL is shown without saying rmail will not follow it: {text}"
    );
}

/// A report with no unsubscribe method must not print the disclaimer, or it
/// becomes noise nobody reads.
#[test]
fn the_disclaimer_only_appears_when_a_link_does() {
    let report = ListSubscriptionsResponse {
        senders: vec![SubscriptionSender {
            address: "person@example.com".to_owned(),
            messages: 3,
            sender_class: SubscriptionClass::Personal as i32,
            ..SubscriptionSender::default()
        }],
        total_senders: 1,
        ..ListSubscriptionsResponse::default()
    };
    let mut out: Vec<u8> = Vec::new();
    print_subs(&mut out, &report).unwrap();
    assert!(!String::from_utf8(out).unwrap().contains("does not act"));
}

#[test]
fn a_multi_account_correspondence_says_how_to_brief_one() {
    let insight = GetContactInsightResponse {
        address: "ada@example.com".to_owned(),
        volume: Some(ContactVolume::default()),
        ours: Some(ResponseStats::default()),
        theirs: Some(ResponseStats::default()),
        cadence: Some(ContactCadence::default()),
        decay: Some(ContactDecay::default()),
        accounts: vec![1, 2],
        ..GetContactInsightResponse::default()
    };
    let mut out: Vec<u8> = Vec::new();
    print_contact(&mut out, &insight).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("--account"), "{text}");
}

#[test]
fn topics_and_symmetry_render_when_present() {
    let insight = GetContactInsightResponse {
        address: "ada@example.com".to_owned(),
        name: "Ada".to_owned(),
        volume: Some(ContactVolume {
            inbound: 10,
            outbound: 5,
            threads: 4,
            direction_ratio: 0.333,
            ..ContactVolume::default()
        }),
        ours: Some(ResponseStats {
            samples: 3,
            p50_seconds: 7_200,
            p90_seconds: 86_400,
            ..ResponseStats::default()
        }),
        theirs: Some(ResponseStats {
            samples: 2,
            p50_seconds: 3_600,
            ..ResponseStats::default()
        }),
        symmetry: 0.5,
        cadence: Some(ContactCadence::default()),
        decay: Some(ContactDecay::default()),
        topics: vec![ContactTopic {
            term: "lease".to_owned(),
            messages: 4,
        }],
        ..GetContactInsightResponse::default()
    };
    let mut out: Vec<u8> = Vec::new();
    print_contact(&mut out, &insight).unwrap();
    let text = String::from_utf8(out).unwrap();
    assert!(text.contains("Ada <ada@example.com>"), "{text}");
    assert!(text.contains("lease (4)"), "{text}");
    assert!(text.contains("they are the faster side"), "{text}");
}

/// Every string these three verbs print is either mail text or model prose
/// written from mail text. An `ESC [` run in any of it repaints the terminal;
/// a bidi override reorders the line it lands in. Neither may survive.
#[test]
fn no_printed_field_can_drive_the_terminal() {
    // A cursor-up-and-overwrite run plus a right-to-left override: the two
    // families `terminal_safe` exists for.
    const HOSTILE: &str = "Nothing\u{1b}[1A\u{1b}[2K urgent \u{202e}drawkcab\u{202c}";

    let answer = AskAnalyticsResponse {
        question: HOSTILE.to_owned(),
        sql: format!("SELECT '{HOSTILE}' AS s"),
        params: vec![format!("text:{HOSTILE}")],
        notes: HOSTILE.to_owned(),
        columns: vec![HOSTILE.to_owned()],
        rows: vec![AnalyticsRow {
            cells: vec![AnalyticsCell {
                value: Some(CellValue::TextValue(HOSTILE.to_owned())),
            }],
        }],
        narrative: HOSTILE.to_owned(),
        ..AskAnalyticsResponse::default()
    };
    let mut out: Vec<u8> = Vec::new();
    print_answer(&mut out, &answer, true).unwrap();
    assert_clean(&String::from_utf8(out).unwrap(), "mail stats ask");

    let report = ListSubscriptionsResponse {
        senders: vec![SubscriptionSender {
            address: HOSTILE.to_owned(),
            name: HOSTILE.to_owned(),
            signals: vec![HOSTILE.to_owned()],
            unsubscribe: Some(Unsubscribe {
                http_url: format!("https://example.com/{HOSTILE}"),
                mailto: HOSTILE.to_owned(),
                one_click: false,
            }),
            ..SubscriptionSender::default()
        }],
        total_senders: 1,
        ..ListSubscriptionsResponse::default()
    };
    let mut out: Vec<u8> = Vec::new();
    print_subs(&mut out, &report).unwrap();
    assert_clean(&String::from_utf8(out).unwrap(), "mail subs");

    let insight = GetContactInsightResponse {
        address: HOSTILE.to_owned(),
        name: HOSTILE.to_owned(),
        volume: Some(ContactVolume::default()),
        ours: Some(ResponseStats::default()),
        theirs: Some(ResponseStats::default()),
        cadence: Some(ContactCadence::default()),
        decay: Some(ContactDecay::default()),
        topics: vec![ContactTopic {
            term: HOSTILE.to_owned(),
            messages: 2,
        }],
        briefing: HOSTILE.to_owned(),
        next_actions: vec![HOSTILE.to_owned()],
        ..GetContactInsightResponse::default()
    };
    let mut out: Vec<u8> = Vec::new();
    print_contact(&mut out, &insight).unwrap();
    assert_clean(&String::from_utf8(out).unwrap(), "mail contact");
}

/// No ESC, no bidi override — and the words themselves still present, so a
/// renderer that simply dropped everything would not pass.
fn assert_clean(text: &str, what: &str) {
    assert!(
        !text.contains('\u{1b}'),
        "{what}: an ESC survived: {text:?}"
    );
    assert!(
        !text.contains('\u{202e}') && !text.contains('\u{202c}'),
        "{what}: a bidi override survived: {text:?}"
    );
    assert!(
        text.contains("urgent"),
        "{what}: the text itself was dropped rather than sanitized: {text:?}"
    );
}

#[test]
fn durations_reject_a_zero_or_negative_window() {
    assert!(parse_duration("0d").is_err());
    assert!(parse_duration("-1d").is_err());
    assert!(parse_duration("banana").is_err());
    assert_eq!(parse_duration("2w").unwrap(), 14 * 86_400);
}
