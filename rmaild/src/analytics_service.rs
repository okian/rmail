//! The `AnalyticsService` gRPC implementation (task 71, prd.md feature 58).
//!
//! A thin boundary over [`rmail_core::analytics::response_time`]: decode the
//! request, apply the defaults a zero field stands for, run the report, and
//! project it back. Every rule about what a pair *is* — direction, the
//! bottleneck test, the percentile method — lives in the core module, because
//! the CLI, MCP and any future surface must all get the same numbers.
//!
//! # Zero means "default", and the resolved values come back
//!
//! proto3 has no field presence for scalars, so a client that leaves `since`
//! or `bucket_seconds` alone sends zero. Treating zero as a literal here
//! would make "the last 90 days" unspellable: `since = 0` is the unix epoch,
//! and a report from 1970 is not what an empty field means. So zero selects
//! the documented default, and the response echoes the window that was
//! actually used — a client rendering "response times since …" must not have
//! to re-derive it and risk disagreeing with the numbers underneath.
//!
//! # The daemon's shutdown token reaches the scan
//!
//! A report walks the message table. Handing the handler a child of the
//! daemon's shutdown token means `sqlite3_interrupt` actually stops that walk
//! when the process is going down, rather than leaving a blocking-pool thread
//! to finish a report nobody will read. A cancelled scan surfaces as
//! `CANCELLED`, never as an empty report — see the core module's `scan`.
#![allow(clippy::result_large_err)] // see mail_service.rs's note on `Result<_, Status>`

use rmail_core::analytics::response_time::{
    self, GroupBy, ResponseGroup, ResponseTimeQuery, ResponseTimes, Stats, TrendPoint,
};
use rmail_core::{Database, Error};
use rmail_proto::v1::analytics_service_server::AnalyticsService;
use rmail_proto::v1::{
    GetResponseTimesRequest, GetResponseTimesResponse, ResponseStats,
    ResponseTimeGroup as ProtoGroup, ResponseTimeGroupBy as ProtoGroupBy,
    ResponseTrendPoint as ProtoTrendPoint,
};
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};

/// The `AnalyticsService` handler, backed by the local database.
#[derive(Clone)]
pub struct AnalyticsApi {
    db: Database,
    /// Cancelled when the daemon shuts down, so an in-flight report stops
    /// with it rather than holding shutdown open.
    shutdown: CancellationToken,
}

impl AnalyticsApi {
    /// Create a handler over the given database.
    #[must_use]
    pub fn new(db: Database, shutdown: CancellationToken) -> Self {
        Self { db, shutdown }
    }
}

#[tonic::async_trait]
impl AnalyticsService for AnalyticsApi {
    async fn get_response_times(
        &self,
        request: Request<GetResponseTimesRequest>,
    ) -> Result<Response<GetResponseTimesResponse>, Status> {
        let req = request.into_inner();
        if req.account_id != 0 {
            tracing::Span::current().record(rmail_core::telemetry::FIELD_ACCOUNT, req.account_id);
        }
        let query = query_from_proto(&req, now()?)?;
        let cancel = self.shutdown.child_token();
        let report = response_time::response_times(&self.db, &cancel, query).await?;
        Ok(Response::new(to_proto(&report)))
    }
}

/// Wall-clock seconds, for the `until = 0` default.
///
/// An error rather than a fallback. Substituting 0 would *not* be caught
/// downstream: `since` would resolve to `0 - 90 days`, which is a perfectly
/// well-ordered window, and the caller would get a silent, empty report about
/// 1969 with no indication that the machine's clock is the reason. A daemon
/// whose clock predates the epoch cannot answer "the last 90 days" and should
/// say so.
fn now() -> Result<i64, Status> {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| Error::internal("the system clock is before the unix epoch"))?;
    i64::try_from(elapsed.as_secs())
        .map_err(|_| Error::internal("the system clock is beyond the representable range"))
        .map_err(Status::from)
}

/// Decode the request, resolving every "0 means default" field.
fn query_from_proto(req: &GetResponseTimesRequest, now: i64) -> Result<ResponseTimeQuery, Status> {
    for (name, value) in [
        ("account_id", req.account_id),
        ("since", req.since),
        ("until", req.until),
        ("bucket_seconds", req.bucket_seconds),
        ("window_seconds", req.window_seconds),
    ] {
        if value < 0 {
            return Err(Status::from(Error::invalid_argument(format!(
                "{name} must not be negative"
            ))));
        }
    }
    let until = if req.until == 0 { now } else { req.until };
    let since = if req.since == 0 {
        until.saturating_sub(response_time::DEFAULT_RANGE_SECONDS)
    } else {
        req.since
    };
    Ok(ResponseTimeQuery {
        account_id: (req.account_id != 0).then_some(req.account_id),
        group_by: group_by_from_proto(req.group_by)?,
        since,
        until,
        bucket_seconds: zero_default(req.bucket_seconds, response_time::DEFAULT_BUCKET_SECONDS),
        window_seconds: zero_default(req.window_seconds, response_time::DEFAULT_WINDOW_SECONDS),
        limit: if req.limit == 0 {
            response_time::DEFAULT_LIMIT
        } else {
            usize::try_from(req.limit).unwrap_or(response_time::MAX_LIMIT)
        },
        min_samples: if req.min_samples == 0 {
            response_time::DEFAULT_MIN_SAMPLES
        } else {
            req.min_samples
        },
        // A ratio of exactly 0 is indistinguishable from an unset field, and
        // the core rejects everything below 1 anyway — so zero is the
        // default, not a value that would flag every correspondence.
        bottleneck_ratio: if req.bottleneck_ratio == 0.0 {
            response_time::DEFAULT_BOTTLENECK_RATIO
        } else {
            req.bottleneck_ratio
        },
    })
}

/// `0` selects `fallback`; anything else is taken literally.
const fn zero_default(value: i64, fallback: i64) -> i64 {
    if value == 0 {
        fallback
    } else {
        value
    }
}

/// Decode the grouping enum, rejecting a value no version of this proto
/// defines rather than silently grouping by something the caller did not ask
/// for.
fn group_by_from_proto(raw: i32) -> Result<GroupBy, Status> {
    match ProtoGroupBy::try_from(raw) {
        Ok(ProtoGroupBy::Unspecified | ProtoGroupBy::Contact) => Ok(GroupBy::Contact),
        Ok(ProtoGroupBy::Mailbox) => Ok(GroupBy::Mailbox),
        Err(_) => Err(Status::from(Error::invalid_argument(format!(
            "unknown group_by value {raw}"
        )))),
    }
}

/// Project a finished report onto the wire.
fn to_proto(report: &ResponseTimes) -> GetResponseTimesResponse {
    GetResponseTimesResponse {
        since: report.since,
        until: report.until,
        group_by: group_by_to_proto(report.group_by) as i32,
        ours: Some(stats_to_proto(report.ours)),
        theirs: Some(stats_to_proto(report.theirs)),
        groups: report.groups.iter().map(group_to_proto).collect(),
        total_groups: u32::try_from(report.total_groups).unwrap_or(u32::MAX),
        trend: report.trend.iter().map(trend_to_proto).collect(),
        self_addresses: report.self_addresses.clone(),
        pairs: report.pairs,
        skipped_out_of_order: report.skipped_out_of_order,
    }
}

fn group_by_to_proto(group_by: GroupBy) -> ProtoGroupBy {
    match group_by {
        GroupBy::Contact => ProtoGroupBy::Contact,
        GroupBy::Mailbox => ProtoGroupBy::Mailbox,
    }
}

fn stats_to_proto(stats: Stats) -> ResponseStats {
    ResponseStats {
        samples: stats.samples,
        p50_seconds: stats.p50_seconds,
        p90_seconds: stats.p90_seconds,
        mean_seconds: stats.mean_seconds,
        min_seconds: stats.min_seconds,
        max_seconds: stats.max_seconds,
    }
}

fn group_to_proto(group: &ResponseGroup) -> ProtoGroup {
    ProtoGroup {
        key: group.key.clone(),
        label: group.label.clone(),
        mailbox_id: group.mailbox_id.unwrap_or(0),
        ours: Some(stats_to_proto(group.ours)),
        theirs: Some(stats_to_proto(group.theirs)),
        inbound: group.inbound,
        awaiting_reply: group.awaiting_reply,
        overdue: group.overdue,
        bottleneck: group.bottleneck,
        slower_than_counterpart: group.slower_than_counterpart,
        stalled: group.stalled,
    }
}

fn trend_to_proto(point: &TrendPoint) -> ProtoTrendPoint {
    ProtoTrendPoint {
        window_start: point.window_start,
        window_end: point.window_end,
        stats: Some(stats_to_proto(point.stats)),
    }
}

#[cfg(test)]
mod tests;
