//! Unit tests for the `AnalyticsService` boundary: the "0 means default"
//! decoding, the rejected inputs, and the enum round trip. The report's own
//! arithmetic is `rmail_core::analytics::response_time`'s to test; what is
//! checked here is only that a request arrives at it unmangled.
#![allow(clippy::unwrap_used)]

use super::*;

const NOW: i64 = 1_700_000_000;

fn request() -> GetResponseTimesRequest {
    GetResponseTimesRequest {
        account_id: 0,
        group_by: 0,
        since: 0,
        until: 0,
        bucket_seconds: 0,
        window_seconds: 0,
        limit: 0,
        min_samples: 0,
        bottleneck_ratio: 0.0,
    }
}

#[test]
fn an_all_zero_request_resolves_to_the_documented_defaults() {
    let query = query_from_proto(&request(), NOW).unwrap();
    assert_eq!(query.account_id, None, "0 means every account");
    assert_eq!(query.group_by, GroupBy::Contact);
    assert_eq!(query.until, NOW);
    assert_eq!(query.since, NOW - response_time::DEFAULT_RANGE_SECONDS);
    assert_eq!(query.bucket_seconds, response_time::DEFAULT_BUCKET_SECONDS);
    assert_eq!(query.window_seconds, response_time::DEFAULT_WINDOW_SECONDS);
    assert_eq!(query.limit, response_time::DEFAULT_LIMIT);
    assert_eq!(query.min_samples, response_time::DEFAULT_MIN_SAMPLES);
    assert!(
        (query.bottleneck_ratio - response_time::DEFAULT_BOTTLENECK_RATIO).abs() < f64::EPSILON,
        "a zero ratio is an unset field, not a ratio that flags everything"
    );
}

#[test]
fn an_explicit_since_with_a_defaulted_until_still_ends_at_now() {
    let mut req = request();
    req.since = NOW - 10;
    let query = query_from_proto(&req, NOW).unwrap();
    assert_eq!(query.since, NOW - 10);
    assert_eq!(query.until, NOW);
}

#[test]
fn an_explicit_until_moves_the_defaulted_since_with_it() {
    let mut req = request();
    req.until = NOW - 1000;
    let query = query_from_proto(&req, NOW).unwrap();
    assert_eq!(query.until, NOW - 1000);
    assert_eq!(
        query.since,
        NOW - 1000 - response_time::DEFAULT_RANGE_SECONDS,
        "the default range hangs off `until`, not off the clock"
    );
}

#[test]
fn every_field_a_caller_sets_survives_decoding() {
    let req = GetResponseTimesRequest {
        account_id: 7,
        group_by: ProtoGroupBy::Mailbox as i32,
        since: 100,
        until: 900,
        bucket_seconds: 60,
        window_seconds: 120,
        limit: 9,
        min_samples: 4,
        bottleneck_ratio: 3.5,
    };
    let query = query_from_proto(&req, NOW).unwrap();
    assert_eq!(query.account_id, Some(7));
    assert_eq!(query.group_by, GroupBy::Mailbox);
    assert_eq!(query.since, 100);
    assert_eq!(query.until, 900);
    assert_eq!(query.bucket_seconds, 60);
    assert_eq!(query.window_seconds, 120);
    assert_eq!(query.limit, 9);
    assert_eq!(query.min_samples, 4);
    assert!((query.bottleneck_ratio - 3.5).abs() < f64::EPSILON);
}

#[test]
fn a_negative_field_is_invalid_argument() {
    for mutate in [
        (|req: &mut GetResponseTimesRequest| req.account_id = -1) as fn(&mut _),
        |req| req.since = -1,
        |req| req.until = -1,
        |req| req.bucket_seconds = -1,
        |req| req.window_seconds = -1,
    ] {
        let mut req = request();
        mutate(&mut req);
        let status = query_from_proto(&req, NOW).unwrap_err();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(
            status.message().contains("must not be negative"),
            "unhelpful message: {}",
            status.message()
        );
    }
}

#[test]
fn an_unknown_group_by_is_rejected_rather_than_silently_defaulted() {
    let mut req = request();
    req.group_by = 99;
    let status = query_from_proto(&req, NOW).unwrap_err();
    assert_eq!(status.code(), tonic::Code::InvalidArgument);
    assert!(status.message().contains("99"));
}

#[test]
fn an_over_large_limit_lands_at_the_ceiling_rather_than_wrapping() {
    let mut req = request();
    req.limit = u32::MAX;
    let query = query_from_proto(&req, NOW).unwrap();
    // The core clamps to MAX_LIMIT; what matters here is that the conversion
    // never produced a small number by truncation.
    assert!(query.limit >= response_time::MAX_LIMIT);
}

#[test]
fn the_grouping_enum_round_trips() {
    for group_by in [GroupBy::Contact, GroupBy::Mailbox] {
        let raw = group_by_to_proto(group_by) as i32;
        assert_eq!(group_by_from_proto(raw).unwrap(), group_by);
        assert_ne!(raw, ProtoGroupBy::Unspecified as i32);
    }
}

#[test]
fn an_empty_report_projects_to_zeroed_stats_not_missing_ones() {
    let report = ResponseTimes {
        since: 1,
        until: 2,
        group_by: GroupBy::Mailbox,
        ours: Stats::default(),
        theirs: Stats::default(),
        groups: Vec::new(),
        total_groups: 0,
        trend: Vec::new(),
        self_addresses: Vec::new(),
        pairs: 0,
        skipped_out_of_order: 0,
    };
    let wire = to_proto(&report);
    assert_eq!(wire.group_by, ProtoGroupBy::Mailbox as i32);
    assert_eq!(wire.ours.map(|s| s.samples), Some(0));
    assert_eq!(wire.theirs.map(|s| s.samples), Some(0));
    assert_eq!(wire.total_groups, 0);
}

#[test]
fn a_group_projects_every_flag_and_its_mailbox_id() {
    let report = ResponseTimes {
        since: 1,
        until: 2,
        group_by: GroupBy::Contact,
        ours: Stats::default(),
        theirs: Stats::default(),
        groups: vec![ResponseGroup {
            key: "alice@x".to_owned(),
            label: "Alice".to_owned(),
            mailbox_id: None,
            ours: Stats::from_sorted(&[10, 20]),
            theirs: Stats::from_sorted(&[1]),
            inbound: 5,
            awaiting_reply: 4,
            overdue: 3,
            bottleneck: true,
            slower_than_counterpart: true,
            stalled: false,
        }],
        total_groups: 1,
        trend: vec![TrendPoint {
            window_start: 1,
            window_end: 2,
            stats: Stats::from_sorted(&[7]),
        }],
        self_addresses: vec!["me@x".to_owned()],
        pairs: 3,
        skipped_out_of_order: 1,
    };
    let wire = to_proto(&report);
    let group = wire.groups.first().unwrap();
    assert_eq!(group.key, "alice@x");
    assert_eq!(group.label, "Alice");
    assert_eq!(group.mailbox_id, 0, "no mailbox is 0 on the wire");
    assert_eq!(group.awaiting_reply, 4);
    assert_eq!(group.overdue, 3);
    assert!(group.bottleneck && group.slower_than_counterpart && !group.stalled);
    assert_eq!(group.ours.as_ref().map(|s| s.samples), Some(2));
    assert_eq!(
        wire.trend
            .first()
            .and_then(|p| p.stats)
            .map(|s| s.p50_seconds),
        Some(7)
    );
    assert_eq!(wire.self_addresses, vec!["me@x".to_owned()]);
    assert_eq!(wire.skipped_out_of_order, 1);
}
