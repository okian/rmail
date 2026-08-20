//! The extraction verbs, and the three that read the index.
#![allow(clippy::panic)]

use rmail_core::command::{self, Resolution};
use rmail_proto::v1::{
    EvalMetrics, EvalReport, ExtractEventsResponse, ExtractLinksResponse, ExtractedEvent,
    ExtractedLink, QueryEval, QueryPlan,
};

use super::super::tests::{no_account, no_message, screen};
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

fn refusal(line: &str, target: &Target) -> String {
    match asked(line, target) {
        Answer::Refused(why) => why,
        other => panic!("{line:?} was not refused: {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// events and tasks
// ---------------------------------------------------------------------------

#[test]
fn events_and_tasks_are_one_command_over_two_item_kinds() {
    // The request fields, the sink, the idempotency claim and the report shape are
    // identical; two commands would be two copies of one wire seam.
    let Cmd::Extract { tasks, .. } = request("extract events").cmd else {
        panic!("expected an extraction");
    };
    assert!(!tasks);
    let Cmd::Extract { tasks, .. } = request("extract tasks").cmd else {
        panic!("expected an extraction");
    };
    assert!(tasks);
    // And the two draw different columns, because a task has a due date and an
    // event has a place.
    assert_ne!(
        request("extract events").columns,
        request("extract tasks").columns
    );
}

#[test]
fn the_sink_defaults_to_delivering_nothing() {
    // The reading that changes nothing outside this daemon: return the document.
    let Cmd::Extract { sink, .. } = request("extract events").cmd else {
        panic!("expected an extraction");
    };
    assert_eq!(sink, Sink::Ics);
    for (name, expected) in [
        ("ics", Sink::Ics),
        ("command", Sink::Command),
        ("webhook", Sink::Webhook),
    ] {
        let Cmd::Extract { sink, .. } = request(&format!("extract tasks --sink={name}")).cmd else {
            panic!("expected an extraction");
        };
        assert_eq!(sink, expected, "{name}");
    }
    assert_eq!(SINKS.len(), 3);
    let why = refusal("extract events --sink=calendar", &screen());
    assert!(why.contains("ics, command, webhook"), "{why}");
}

#[test]
fn an_item_a_model_inferred_is_drawn_differently_from_one_that_was_parsed() {
    // A meeting time read out of a sentence can be wrong in a way one read out of
    // an invitation cannot, and a cancellation is the row nobody must skim past.
    let response = ExtractEventsResponse {
        events: vec![
            ExtractedEvent {
                summary: "standup".to_owned(),
                starts_at: 1_700_000_000,
                source: 1,
                ..Default::default()
            },
            ExtractedEvent {
                summary: "maybe lunch".to_owned(),
                starts_at: 1_700_003_600,
                source: 2,
                ..Default::default()
            },
            ExtractedEvent {
                summary: "review".to_owned(),
                starts_at: 1_700_007_200,
                source: 1,
                cancelled: true,
                ..Default::default()
            },
        ],
        method: String::new(),
        skipped: 1,
        ics: String::new(),
        delivered: 2,
        already_delivered: 3,
        sink_output: "queued\n".to_owned(),
    };
    let rows = wire::event_rows(&response);
    assert_eq!(rows[0].tone, ReportTone::Plain);
    assert_eq!(rows[0].cells[3], "ics");
    assert_eq!(rows[1].tone, ReportTone::Warn, "inferred from prose");
    assert_eq!(rows[2].tone, ReportTone::Bad, "cancelled");
    // The idempotency claim working is said, not left to read as a failure.
    let delivered = rows
        .iter()
        .find(|row| row.cells[0] == "delivered")
        .expect("a delivered row");
    assert!(
        delivered.cells[1].contains("3 already claimed"),
        "{:?}",
        delivered.cells
    );
    assert!(rows.iter().any(|row| row.cells[0] == "skipped"));
    assert!(rows.iter().any(|row| row.cells[0] == "sink"));
}

#[test]
fn an_ordinary_extraction_has_no_delivery_noise_under_it() {
    let response = ExtractEventsResponse {
        events: vec![ExtractedEvent {
            summary: "standup".to_owned(),
            source: 1,
            ..Default::default()
        }],
        ..Default::default()
    };
    let rows = wire::event_rows(&response);
    assert_eq!(rows.len(), 1);
}

#[test]
fn structured_extraction_needs_a_named_schema() {
    assert!(refusal("extract data", &screen()).contains("--schema"));
    let Cmd::ExtractData {
        schema, refresh, ..
    } = request("extract data --schema=invoice --refresh").cmd
    else {
        panic!("expected an extraction");
    };
    assert_eq!(schema, "invoice");
    assert!(refresh);
}

// ---------------------------------------------------------------------------
// links
// ---------------------------------------------------------------------------

#[test]
fn a_deceptive_link_is_the_row_this_verb_exists_for() {
    let response = ExtractLinksResponse {
        links: vec![
            ExtractedLink {
                url: "https://evil.example/x".to_owned(),
                host: "evil.example".to_owned(),
                scheme: "https".to_owned(),
                display_text: "bank.example".to_owned(),
                display_host: "bank.example".to_owned(),
                deceptive: true,
                kind: 5,
                classifier: 1,
                score: 0.9,
                reason: "text names another host".to_owned(),
                occurrences: 1,
                source: None,
            },
            ExtractedLink {
                url: "https://x.example/p.gif".to_owned(),
                host: "x.example".to_owned(),
                kind: 2,
                ..Default::default()
            },
        ],
        truncated: 4,
        skipped_parts: 0,
        tracking_pixels: 3,
    };
    let rows = wire::link_rows(&response);
    assert_eq!(rows[0].tone, ReportTone::Bad);
    assert_eq!(rows[0].cells[0], "call to action");
    assert!(
        rows[0].cells[3].contains("names another host"),
        "{:?}",
        rows[0].cells
    );
    assert_eq!(
        rows[1].tone,
        ReportTone::Muted,
        "a tracker is not interesting"
    );
    let pixels = rows
        .iter()
        .find(|row| row.cells[0] == "pixels")
        .expect("a pixel count");
    assert_eq!(pixels.tone, ReportTone::Warn);
    assert!(rows.iter().any(|row| row.cells[0] == "truncated"));
}

// ---------------------------------------------------------------------------
// the index verbs
// ---------------------------------------------------------------------------

#[test]
fn compiling_a_query_shows_the_plan_before_it_runs() {
    let plan = QueryPlan {
        raw: "invoices from stripe".to_owned(),
        compiled: "from:stripe invoice".to_owned(),
        filters: vec!["from:stripe".to_owned(), "after:2026-01-01".to_owned()],
        semantic_query: "invoices".to_owned(),
        intent: 0,
        notes: "read as a sender filter".to_owned(),
        cached: true,
        model: "haiku".to_owned(),
        compiled_at: 0,
    };
    let rows = wire::query_plan_rows(&plan);
    assert_eq!(rows[0].cells[1], "invoices from stripe");
    assert_eq!(rows[1].cells[1], "from:stripe invoice");
    assert_eq!(rows[1].tone, ReportTone::Ok);
    // Every filter is its own row: a plan whose filters were folded into one cell
    // would be a plan nobody could check.
    let filters: Vec<&str> = rows
        .iter()
        .filter(|row| row.cells[0] == "filter")
        .map(|row| row.cells[1].as_str())
        .collect();
    assert_eq!(filters, vec!["from:stripe", "after:2026-01-01"]);
    let from = rows.last().expect("a provenance row");
    assert!(from.cells[1].contains("cached"), "{:?}", from.cells);
}

#[test]
fn entity_kinds_are_read_from_both_spellings() {
    let Cmd::SearchEntities { kinds, query, .. } =
        request("search entities acme --kinds=org,person --kinds=email").cmd
    else {
        panic!("expected a search");
    };
    assert_eq!(query, "acme");
    assert_eq!(
        kinds,
        vec!["org".to_owned(), "person".to_owned(), "email".to_owned()]
    );
}

#[test]
fn an_unresolved_judgment_makes_a_metric_a_lower_bound_and_says_so() {
    // A judgment naming a message the index does not have means every metric for
    // that query is a floor rather than a measurement.
    let report = EvalReport {
        corpus: "fixture-v1".to_owned(),
        per_query: vec![
            QueryEval {
                name: "invoices".to_owned(),
                query: "from:stripe".to_owned(),
                metrics: Some(EvalMetrics {
                    ndcg_at_10: 0.812,
                    mrr: 1.0,
                    recall_at_50: 0.9,
                    p_at_3: 0.667,
                }),
                returned: 10,
                relevant: 6,
                unresolved: Vec::new(),
            },
            QueryEval {
                name: "missing".to_owned(),
                query: "x".to_owned(),
                metrics: Some(EvalMetrics::default()),
                returned: 0,
                relevant: 0,
                unresolved: vec!["<a@b>".to_owned()],
            },
        ],
        aggregate: Some(EvalMetrics {
            ndcg_at_10: 0.4,
            mrr: 0.5,
            recall_at_50: 0.45,
            p_at_3: 0.33,
        }),
    };
    let rows = wire::eval_rows(&report);
    assert_eq!(rows[0].cells[0], "— all queries —");
    assert_eq!(rows[0].cells[1], "0.400");
    assert_eq!(rows[1].cells[1], "0.812");
    assert_eq!(rows[1].cells[5], "6/10 relevant");
    assert_eq!(rows[2].tone, ReportTone::Warn);
    assert!(
        rows[2].cells[5].contains("unresolved"),
        "{:?}",
        rows[2].cells
    );
}

#[test]
fn eval_needs_a_path_and_a_known_mode() {
    assert!(refusal("search eval", &screen()).contains("which golden set"));
    let Cmd::SearchEval { path, mode, .. } = request("search eval eval/golden.toml").cmd else {
        panic!("expected an evaluation");
    };
    assert_eq!(path, "eval/golden.toml");
    assert_eq!(mode, None, "absent means the daemon's own default");
    for (name, expected) in [
        ("lexical", Mode::Lexical),
        ("semantic", Mode::Semantic),
        ("hybrid", Mode::Hybrid),
    ] {
        let Cmd::SearchEval { mode, .. } =
            request(&format!("search eval g.toml --mode={name}")).cmd
        else {
            panic!("expected an evaluation");
        };
        assert_eq!(mode, Some(expected), "{name}");
    }
    assert_eq!(MODES.len(), 3);
    let why = refusal("search eval g.toml --mode=vector", &screen());
    assert!(why.contains("lexical, semantic, hybrid"), "{why}");
}

#[test]
fn the_index_verbs_need_an_account_and_a_query() {
    assert!(refusal("search compile", &screen()).contains("compile what"));
    assert!(refusal("search entities", &screen()).contains("search for what"));
    for line in ["search compile x", "search entities x"] {
        match asked(line, &no_account()) {
            Answer::Refused(why) => assert!(why.contains("no account"), "{line}: {why}"),
            other => panic!("{line}: {other:?}"),
        }
    }
    // The extraction verbs need a message instead.
    for line in ["extract events", "extract tasks", "links"] {
        assert!(
            refusal(line, &no_message()).contains("no message selected"),
            "{line}"
        );
    }
}
