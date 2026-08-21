//! The analytics and export verbs.
#![allow(clippy::panic)]

use rmail_core::command::{self, Resolution};
use rmail_proto::v1::{
    DigestLine, DigestSection, ExportDone, GenerateDigestResponse, GetResponseTimesResponse,
    ResponseStats, ResponseTimeGroup,
};

use super::super::tests::{loaded, no_account, run, screen};
use super::*;
use crate::tui::model::wire;
use crate::tui::report::ReportTone;

fn invocation(line: &str) -> Invocation {
    match command::parse(line) {
        Ok(Resolution::Invocation(invocation)) => *invocation,
        other => panic!("{line:?} does not parse to an invocation: {other:?}"),
    }
}

fn asked(line: &str, target: &Target) -> Answer {
    match answer(&invocation(line), target, 5) {
        Some(answer) => answer,
        None => panic!("{line:?} has no answer"),
    }
}

fn request(line: &str) -> Request {
    match asked(line, &screen()) {
        Answer::Rows(request) | Answer::Fact(request) => *request,
        other => panic!("{line:?} is not a request: {other:?}"),
    }
}

fn refusal(line: &str) -> String {
    match asked(line, &screen()) {
        Answer::Refused(why) => why,
        other => panic!("{line:?} was not refused: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// export
// ---------------------------------------------------------------------------

#[test]
fn export_reaches_every_framing_this_build_knows() {
    // The acceptance's "streams to each format". Each name maps to a distinct
    // framing, and a name outside the four is refused rather than defaulted —
    // writing an mbox for a line that said `--format mailbox` would be an
    // archive in a shape nobody asked for.
    for (name, expected) in [
        ("mbox", Format::Mbox),
        ("maildir", Format::Maildir),
        ("eml", Format::Eml),
        ("json", Format::Json),
    ] {
        let Cmd::Export { format, .. } =
            request(&format!("export from:x --to=/tmp/out --format={name}")).cmd
        else {
            panic!("expected an export");
        };
        assert_eq!(format, expected, "{name}");
    }
    assert_eq!(FORMATS.len(), 4);
    let why = refusal("export from:x --to=/tmp/out --format=mailbox");
    assert!(why.contains("mbox, maildir, eml, json"), "{why}");
    // The default is mbox, like `mail export`: one file is what somebody means
    // when they have not said otherwise.
    let Cmd::Export { format, .. } = request("export from:x --to=/tmp/out").cmd else {
        panic!("expected an export");
    };
    assert_eq!(format, Format::Mbox);
}

#[test]
fn export_needs_a_destination_and_exactly_one_selection() {
    assert!(refusal("export from:x").contains("--to"));
    assert!(refusal("export --to=/tmp/out").contains("a query, or --thread"));
    // The proto's selection is a oneof, so a request carrying both would have
    // one silently dropped — refused here, where the line is still on screen.
    let why = refusal("export from:x --to=/tmp/out --thread=4");
    assert!(why.contains("not both"), "{why}");
    let Cmd::Export {
        query, thread_id, ..
    } = request("export --to=/tmp/out --thread=4").cmd
    else {
        panic!("expected an export");
    };
    assert!(query.is_empty());
    assert_eq!(thread_id, Some(4));
}

#[test]
fn an_export_that_skipped_messages_says_so() {
    // An archive quietly short by forty messages is worse than one that admits
    // it: a message whose raw bytes this daemon never stored cannot be exported.
    let rows = wire::export_rows(
        "/tmp/out",
        &ExportDone {
            messages: 100,
            bytes: 2048,
            skipped_without_raw: 40,
        },
    );
    let skipped = rows
        .iter()
        .find(|row| row.cells[0] == "skipped")
        .expect("a skipped row");
    assert_eq!(skipped.tone, ReportTone::Warn);
    assert!(skipped.cells[1].contains("40"), "{:?}", skipped.cells);
    // And an export that skipped nothing has no such row to read past.
    let rows = wire::export_rows(
        "/tmp/out",
        &ExportDone {
            messages: 100,
            bytes: 2048,
            skipped_without_raw: 0,
        },
    );
    assert!(rows.iter().all(|row| row.cells[0] != "skipped"));
}

// ---------------------------------------------------------------------------
// the reports
// ---------------------------------------------------------------------------

#[test]
fn response_times_group_by_contact_unless_told_otherwise() {
    let Cmd::ResponseTimes { group_by, .. } = request("stats response-time").cmd else {
        panic!("expected a report");
    };
    assert_eq!(group_by, GroupBy::Contact);
    let Cmd::ResponseTimes { group_by, .. } = request("stats response-time --group-by=mailbox").cmd
    else {
        panic!("expected a report");
    };
    assert_eq!(group_by, GroupBy::Mailbox);
    let why = refusal("stats response-time --group-by=thread");
    assert!(why.contains("contact or mailbox"), "{why}");
}

#[test]
fn a_contact_with_no_samples_has_no_median_rather_than_a_median_of_zero() {
    // Printing `0s` for a contact who has never been replied to would read as
    // the fastest possible answer instead of no answer at all.
    let response = GetResponseTimesResponse {
        since: 0,
        until: 0,
        group_by: 1,
        ours: Some(ResponseStats {
            samples: 4,
            p50_seconds: 5_400,
            p90_seconds: 172_800,
            mean_seconds: 0.0,
            min_seconds: 0,
            max_seconds: 0,
        }),
        theirs: None,
        groups: vec![ResponseTimeGroup {
            key: "ada@example.com".to_owned(),
            label: "Ada".to_owned(),
            mailbox_id: 0,
            // Zero samples with a non-zero figure: `duration(0)` already reads
            // as `-`, so a stats block of all zeros would pass whether or not
            // the guard existed. This is the shape the guard is for.
            ours: Some(ResponseStats {
                samples: 0,
                p50_seconds: 5_400,
                p90_seconds: 5_400,
                mean_seconds: 0.0,
                min_seconds: 0,
                max_seconds: 0,
            }),
            theirs: None,
            inbound: 3,
            awaiting_reply: 2,
            overdue: 1,
            bottleneck: true,
            slower_than_counterpart: false,
            stalled: false,
        }],
        total_groups: 1,
        trend: Vec::new(),
        self_addresses: Vec::new(),
        pairs: 4,
        skipped_out_of_order: 0,
    };
    let rows = wire::response_time_rows(&response);
    // The overall figures first, so a group has something to be compared against.
    assert_eq!(rows[0].cells[0], "— everyone —");
    assert_eq!(rows[0].cells[1], "1h 30m");
    let group = &rows[1];
    assert_eq!(group.cells[0], "Ada");
    assert_eq!(group.cells[1], "-", "no samples is not a p50 of zero");
    assert_eq!(group.cells[4], "2 (1 late)");
    assert!(
        group.cells[5].contains("you are the delay"),
        "{:?}",
        group.cells
    );
    assert_eq!(group.tone, ReportTone::Warn);
}

#[test]
fn a_digest_line_opens_the_message_it_cites() {
    // The acceptance's own requirement, and the reason these rows carry an
    // invocation: a summary a reader cannot get behind is one they have to take
    // on trust.
    let response = GenerateDigestResponse {
        digest_id: 1,
        since: 0,
        until: 0,
        account_id: 7,
        generated_at: 0,
        markdown: String::new(),
        sections: vec![DigestSection {
            id: "waiting".to_owned(),
            heading: "Waiting on you".to_owned(),
            lines: vec![
                DigestLine {
                    text: "Ada asked about the contract".to_owned(),
                    message_ids: vec![42, 43],
                },
                DigestLine {
                    text: "Nothing cited here".to_owned(),
                    message_ids: Vec::new(),
                },
            ],
        }],
        sources: Vec::new(),
        model: "haiku".to_owned(),
        considered: 10,
        packed: 4,
        withheld_by_policy: 2,
        clusters: 1,
        cached: false,
        empty: false,
    };
    let rows = wire::digest_rows(&response);
    assert_eq!(rows[0].cells[0], "Waiting on you");
    let opens = rows[0].on_enter.clone().expect("a cited line opens it");
    assert_eq!(opens.verb, vec!["message", "open"]);
    assert_eq!(opens.positionals, vec!["42".to_owned()]);
    assert!(opens.bang, "reading mail is not a question");
    // The heading appears once per section, not on every line.
    assert_eq!(rows[1].cells[0], "");
    // A line citing nothing carries nothing rather than opening something else.
    assert!(rows[1].on_enter.is_none());
    // And mail kept out by policy is said, not silently absent.
    let withheld = rows
        .iter()
        .find(|row| row.cells[0] == "withheld")
        .expect("a withheld row");
    assert_eq!(withheld.tone, ReportTone::Warn);
}

#[test]
fn an_empty_digest_says_what_it_looked_at() {
    let response = GenerateDigestResponse {
        empty: true,
        considered: 91,
        ..GenerateDigestResponse::default()
    };
    let rows = wire::digest_rows(&response);
    assert_eq!(rows.len(), 1);
    assert!(rows[0].cells[1].contains("91"), "{:?}", rows[0].cells);
}

#[test]
fn the_model_calls_in_this_family_are_all_opt_in() {
    // Each of these costs money, and a report that spent it by default would be
    // a report somebody discovers on an invoice.
    let switches = [
        ("digest", "force"),
        ("stats ask who owes me", "narrate"),
        ("contact a@b.example", "metrics-only"),
        ("subs", "classify"),
    ];
    for (line, flag) in switches {
        let bare = request(line).cmd;
        let with = request(&format!("{line} --{flag}")).cmd;
        assert_ne!(bare, with, "--{flag} changed nothing on {line:?}");
    }
}

#[test]
fn every_analytics_verb_refuses_without_an_account() {
    for line in [
        "stats response-time",
        "stats ask who owes me",
        "digest",
        "contact a@b.example",
        "subs",
    ] {
        match asked(line, &no_account()) {
            Answer::Refused(why) => assert!(why.contains("no account"), "{line}: {why}"),
            other => panic!("{line}: {other:?}"),
        }
    }
}

#[test]
fn the_questions_and_the_address_are_required() {
    assert!(refusal("stats ask").contains("ask something"));
    assert!(refusal("contact").contains("whose"));
    // An unquoted question is one question, not its first word.
    let Cmd::AskAnalytics { question, .. } = request("stats ask who owes me a reply").cmd else {
        panic!("expected a question");
    };
    assert_eq!(question, "who owes me a reply");
}

#[test]
fn an_analytics_answer_shows_the_query_it_ran() {
    // A number nobody can see the query behind is a number nobody can check,
    // which is why the RPC returns the SQL at all.
    let response = rmail_proto::v1::AskAnalyticsResponse {
        question: "who".to_owned(),
        sql: "select 1\nfrom messages".to_owned(),
        params: Vec::new(),
        notes: "counted inbound only".to_owned(),
        columns: vec!["who".to_owned(), "n".to_owned()],
        rows: Vec::new(),
        truncated: true,
        narrative: String::new(),
        narrative_rows: 0,
        model: "haiku".to_owned(),
    };
    let rows = wire::ask_analytics_rows(&response);
    let sql: Vec<&str> = rows
        .iter()
        .filter(|row| row.cells[0] == "sql")
        .map(|row| row.cells[1].as_str())
        .collect();
    assert_eq!(sql, vec!["select 1", "from messages"]);
    assert!(rows.iter().any(|row| row.cells[0] == "truncated"));
}

#[test]
fn a_bulk_sender_nobody_reads_is_the_row_drawn_as_a_warning() {
    let response = rmail_proto::v1::ListSubscriptionsResponse {
        since: 0,
        until: 0,
        senders: vec![
            rmail_proto::v1::SubscriptionSender {
                account_id: 7,
                address: "news@example.com".to_owned(),
                name: "News".to_owned(),
                messages: 90,
                read_messages: 2,
                read_rate: 0.02,
                first_seen: 0,
                last_seen: 0,
                median_gap_seconds: 0,
                your_replies: 0,
                sender_class: 1,
                source: 1,
                signals: Vec::new(),
                unsubscribe: Some(rmail_proto::v1::Unsubscribe {
                    http_url: "https://x.example/u".to_owned(),
                    mailto: String::new(),
                    one_click: true,
                }),
                headers_read: true,
                candidate: true,
            },
            rmail_proto::v1::SubscriptionSender {
                address: "bank@example.com".to_owned(),
                sender_class: 2,
                read_rate: 0.9,
                candidate: false,
                ..Default::default()
            },
        ],
        total_senders: 2,
        headers_read: 1,
        model_classified: 0,
        model: String::new(),
    };
    let rows = wire::subscription_rows(&response);
    assert_eq!(rows[0].tone, ReportTone::Warn);
    assert_eq!(rows[0].cells[1], "newsletter");
    assert_eq!(rows[0].cells[3], "2%");
    // "There is a way out" and "there is a way out that works" are different.
    assert_eq!(rows[0].cells[4], "one click");
    assert_eq!(rows[1].tone, ReportTone::Plain);
    assert_eq!(rows[1].cells[4], "none offered");
}

// ---------------------------------------------------------------------------
// dispatch
// ---------------------------------------------------------------------------

#[test]
fn an_analytics_verb_opens_a_report() {
    let mut model = loaded();
    let cmds = run(&mut model, "digest --since=7d");
    assert!(matches!(cmds.first(), Some(Cmd::Digest { .. })), "{cmds:?}");
    assert!(matches!(
        model.overlay_top(),
        Some(crate::tui::model::Overlay::Report(_))
    ));
}
