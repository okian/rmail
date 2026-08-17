//! The `AnalyticsService` gRPC implementation (tasks 71 and 70, prd.md
//! features 58 and 57).
//!
//! A thin boundary over [`rmail_core::analytics::response_time`] and
//! [`rmail_core::digest`]: decode the request, apply the defaults a zero field
//! stands for, run the report, and project it back. Every rule about what a
//! pair *is* — direction, the bottleneck test, the percentile method — and
//! every rule about what a briefing is — the window grid, clustering, the
//! policy gate, the fence, the citation discipline — lives in the core
//! modules, because the CLI, MCP and any future surface must all get the same
//! answers.
//!
//! # Why the digest lives on this service and not on `AiService`
//!
//! It is prd.md's own placement (`AnalyticsService.GenerateDigest`), and the
//! reason survives scrutiny: a digest answers a question *about* a window of
//! mail rather than about a message, which is the line this service is drawn
//! on. What it does *not* share with its neighbour is the risk profile —
//! `GetResponseTimes` is arithmetic over headers, while `GenerateDigest` reads
//! bodies, calls a provider, spends, and writes a row. That difference is
//! carried entirely by the scope table (`mail.read` versus `mail.read` +
//! `ai.invoke`), not by which service the RPC sits on.
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

use rmail_core::analytics::contacts::{
    self, Briefing, Cadence, ContactBriefer, ContactInsight, ContactInsightQuery, Decay, Topic,
    Volume,
};
use rmail_core::analytics::nl::{AnalyticsAnswer, AnalyticsAsker, AnalyticsQuestion};
use rmail_core::analytics::response_time::{
    self, GroupBy, ResponseGroup, ResponseTimeQuery, ResponseTimes, Stats, TrendPoint,
};
use rmail_core::analytics::sql::Cell;
use rmail_core::analytics::subscriptions::{
    self, Class, Source, Subscription, SubscriptionClassifier, SubscriptionQuery,
    SubscriptionReport, Unsubscribe,
};
use rmail_core::digest::{
    schedule, DigestEngine, DigestReport, DigestRequest, Period, StoredSource,
};
use rmail_core::{Database, Error};
use rmail_proto::v1::analytics_cell::Value as ProtoCellValue;
use rmail_proto::v1::analytics_service_server::AnalyticsService;
use rmail_proto::v1::{
    AnalyticsCell, AnalyticsRow, AskAnalyticsRequest, AskAnalyticsResponse,
    ContactCadence as ProtoCadence, ContactDecay as ProtoDecay, ContactTopic as ProtoTopic,
    ContactVolume as ProtoVolume, DigestLine as ProtoLine, DigestSection as ProtoSection,
    DigestSource as ProtoSource, GenerateDigestRequest, GenerateDigestResponse,
    GetContactInsightRequest, GetContactInsightResponse, GetResponseTimesRequest,
    GetResponseTimesResponse, ListSubscriptionsRequest, ListSubscriptionsResponse, ResponseStats,
    ResponseTimeGroup as ProtoGroup, ResponseTimeGroupBy as ProtoGroupBy,
    ResponseTrendPoint as ProtoTrendPoint, SubscriptionClass as ProtoClass,
    SubscriptionSender as ProtoSender, SubscriptionSource as ProtoSource2,
    Unsubscribe as ProtoUnsubscribe,
};
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};

/// The `AnalyticsService` handler, backed by the local database.
#[derive(Clone)]
pub struct AnalyticsApi {
    db: Database,
    /// The digest engine, `None` on a daemon whose AI subsystem is off.
    ///
    /// `None` rather than an engine over a `NullProvider`: `GenerateDigest`
    /// then declines with `FAILED_PRECONDITION` *before* selecting a window
    /// and scanning it, instead of doing all the work and failing at the
    /// provider. The RPC stays registered either way — reflection and the
    /// fail-closed scope table must see every RPC regardless of runtime
    /// config, the convention `AiService`/`HookService` established.
    digest: Option<DigestEngine>,
    /// The contact briefer, `None` on a daemon whose AI subsystem is off.
    ///
    /// Unlike the digest, half of `GetContactInsight` still works without one:
    /// `metrics_only` is pure arithmetic and is served straight off the
    /// database. Only the briefing declines.
    briefer: Option<ContactBriefer>,
    /// The subscription classifier's model fallback, `None` when AI is off.
    /// Detection itself never needs it.
    classifier: Option<SubscriptionClassifier>,
    /// The natural-language analytics asker, `None` when AI is off. There is
    /// no model-free half of this one: without a model there is no SQL.
    asker: Option<AnalyticsAsker>,
    /// Cancelled when the daemon shuts down, so an in-flight report stops
    /// with it rather than holding shutdown open.
    shutdown: CancellationToken,
}

impl AnalyticsApi {
    /// Create a handler over the given database, with no AI engines — the
    /// briefing, the model fallback and `AskAnalytics` all decline. Use the
    /// `with_*` builders to wire them.
    #[must_use]
    pub fn new(db: Database, shutdown: CancellationToken) -> Self {
        Self {
            db,
            digest: None,
            briefer: None,
            classifier: None,
            asker: None,
            shutdown,
        }
    }

    /// Serve `GenerateDigest` from `engine`.
    #[must_use]
    pub fn with_digest(mut self, engine: DigestEngine) -> Self {
        self.digest = Some(engine);
        self
    }

    /// Serve `GetContactInsight`'s briefing from `briefer`.
    #[must_use]
    pub fn with_contact_briefer(mut self, briefer: ContactBriefer) -> Self {
        self.briefer = Some(briefer);
        self
    }

    /// Serve `ListSubscriptions`' model fallback from `classifier`.
    #[must_use]
    pub fn with_subscription_classifier(mut self, classifier: SubscriptionClassifier) -> Self {
        self.classifier = Some(classifier);
        self
    }

    /// Serve `AskAnalytics` from `asker`.
    #[must_use]
    pub fn with_analytics_asker(mut self, asker: AnalyticsAsker) -> Self {
        self.asker = Some(asker);
        self
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

    async fn generate_digest(
        &self,
        request: Request<GenerateDigestRequest>,
    ) -> Result<Response<GenerateDigestResponse>, Status> {
        let req = request.into_inner();
        if req.account_id != 0 {
            tracing::Span::current().record(rmail_core::telemetry::FIELD_ACCOUNT, req.account_id);
        }
        let Some(engine) = self.digest.as_ref() else {
            return Err(Status::from(Error::failed_precondition(
                "the AI subsystem is not available on this daemon, so no digest can be \
                 generated (check `ai.enabled` and the configured provider)",
            )));
        };
        let period = window_from_proto(&req, engine.interval_seconds(), now()?)?;
        let cancel = self.shutdown.child_token();
        let digest = engine
            .generate(
                DigestRequest {
                    account_id: req.account_id,
                    period,
                    // 0, not the configured cadence: an RPC-selected window is
                    // ad hoc even when it happens to coincide with a period,
                    // and `digests.interval_seconds` records which cadence
                    // *produced* a row rather than which one it resembles.
                    interval_seconds: 0,
                    force: req.force,
                    // A caller is on the other end of this RPC, so it is
                    // charged as interactive rather than against the bulk
                    // sub-budget the scheduled job uses.
                    interactive: true,
                },
                &cancel,
            )
            .await?;
        Ok(Response::new(digest_to_proto(&digest)))
    }

    async fn get_contact_insight(
        &self,
        request: Request<GetContactInsightRequest>,
    ) -> Result<Response<GetContactInsightResponse>, Status> {
        let req = request.into_inner();
        if req.account_id != 0 {
            tracing::Span::current().record(rmail_core::telemetry::FIELD_ACCOUNT, req.account_id);
        }
        let query = contact_query_from_proto(&req, now()?)?;
        let cancel = self.shutdown.child_token();
        // Two paths, and the split is not just an optimization: the numbers
        // are reachable on a daemon with no provider at all, so declining the
        // whole RPC because the briefing half cannot run would take a working
        // report away over a feature the caller did not ask for.
        let insight = if query.metrics_only {
            contacts::metrics(&self.db, &cancel, query).await?
        } else {
            let Some(briefer) = self.briefer.as_ref() else {
                return Err(Status::from(Error::failed_precondition(
                    "the AI subsystem is not available on this daemon, so no contact briefing \
                     can be written (check `ai.enabled` and the configured provider); ask with \
                     metrics_only for the numbers alone",
                )));
            };
            briefer.insight(&cancel, query).await?
        };
        Ok(Response::new(contact_to_proto(&insight)))
    }

    async fn list_subscriptions(
        &self,
        request: Request<ListSubscriptionsRequest>,
    ) -> Result<Response<ListSubscriptionsResponse>, Status> {
        let req = request.into_inner();
        if req.account_id != 0 {
            tracing::Span::current().record(rmail_core::telemetry::FIELD_ACCOUNT, req.account_id);
        }
        let query = subscription_query_from_proto(&req, now()?)?;
        let cancel = self.shutdown.child_token();
        let report = if query.classify_unknown {
            let Some(classifier) = self.classifier.as_ref() else {
                return Err(Status::from(Error::failed_precondition(
                    "the AI subsystem is not available on this daemon, so unclassified senders \
                     cannot be classified (check `ai.enabled` and the configured provider); ask \
                     without classify_unknown for the header and behaviour verdicts alone",
                )));
            };
            classifier.list(&cancel, query).await?
        } else {
            subscriptions::detect(&self.db, &cancel, query).await?
        };
        Ok(Response::new(subscriptions_to_proto(&report)))
    }

    async fn ask_analytics(
        &self,
        request: Request<AskAnalyticsRequest>,
    ) -> Result<Response<AskAnalyticsResponse>, Status> {
        let req = request.into_inner();
        if req.account_id != 0 {
            tracing::Span::current().record(rmail_core::telemetry::FIELD_ACCOUNT, req.account_id);
        }
        if req.account_id < 0 {
            return Err(Status::from(Error::invalid_argument(
                "account_id must not be negative",
            )));
        }
        let Some(asker) = self.asker.as_ref() else {
            return Err(Status::from(Error::failed_precondition(
                "the AI subsystem is not available on this daemon, so a plain-English \
                 analytics question cannot be compiled (check `ai.enabled` and the configured \
                 provider)",
            )));
        };
        let cancel = self.shutdown.child_token();
        let answer = asker
            .ask(
                &cancel,
                AnalyticsQuestion {
                    account_id: (req.account_id != 0).then_some(req.account_id),
                    question: req.question,
                    narrate: req.narrate,
                },
                now()?,
            )
            .await?;
        Ok(Response::new(answer_to_proto(&answer)))
    }
}

/// Decode a contact-insight request, resolving every "0 means default" field.
fn contact_query_from_proto(
    req: &GetContactInsightRequest,
    now: i64,
) -> Result<ContactInsightQuery, Status> {
    for (name, value) in [
        ("account_id", req.account_id),
        ("since", req.since),
        ("until", req.until),
    ] {
        if value < 0 {
            return Err(Status::from(Error::invalid_argument(format!(
                "{name} must not be negative"
            ))));
        }
    }
    let until = if req.until == 0 { now } else { req.until };
    let since = if req.since == 0 {
        until.saturating_sub(contacts::DEFAULT_RANGE_SECONDS)
    } else {
        req.since
    };
    Ok(ContactInsightQuery {
        account_id: (req.account_id != 0).then_some(req.account_id),
        address: req.address.clone(),
        since,
        until,
        topic_limit: if req.topic_limit == 0 {
            contacts::DEFAULT_TOPIC_LIMIT
        } else {
            usize::try_from(req.topic_limit).unwrap_or(contacts::MAX_TOPIC_LIMIT)
        },
        metrics_only: req.metrics_only,
    })
}

/// Decode a subscription request, resolving every "0 means default" field.
fn subscription_query_from_proto(
    req: &ListSubscriptionsRequest,
    now: i64,
) -> Result<SubscriptionQuery, Status> {
    for (name, value) in [
        ("account_id", req.account_id),
        ("since", req.since),
        ("until", req.until),
    ] {
        if value < 0 {
            return Err(Status::from(Error::invalid_argument(format!(
                "{name} must not be negative"
            ))));
        }
    }
    let until = if req.until == 0 { now } else { req.until };
    let since = if req.since == 0 {
        until.saturating_sub(subscriptions::DEFAULT_RANGE_SECONDS)
    } else {
        req.since
    };
    Ok(SubscriptionQuery {
        account_id: (req.account_id != 0).then_some(req.account_id),
        since,
        until,
        limit: if req.limit == 0 {
            subscriptions::DEFAULT_LIMIT
        } else {
            usize::try_from(req.limit).unwrap_or(subscriptions::MAX_LIMIT)
        },
        candidates_only: req.candidates_only,
        classify_unknown: req.classify_unknown,
    })
}

/// Project a contact insight onto the wire.
fn contact_to_proto(insight: &ContactInsight) -> GetContactInsightResponse {
    let Briefing {
        summary,
        next_actions,
        model,
    } = &insight.briefing;
    GetContactInsightResponse {
        address: insight.address.clone(),
        name: insight.name.clone().unwrap_or_default(),
        since: insight.since,
        until: insight.until,
        volume: Some(volume_to_proto(&insight.volume)),
        ours: Some(stats_to_proto(insight.ours)),
        theirs: Some(stats_to_proto(insight.theirs)),
        symmetry: insight.symmetry.unwrap_or(0.0),
        awaiting_reply: insight.awaiting_reply,
        overdue: insight.overdue,
        cadence: Some(cadence_to_proto(insight.cadence)),
        decay: Some(decay_to_proto(insight.decay)),
        topics: insight.topics.iter().map(topic_to_proto).collect(),
        briefing: summary.clone(),
        next_actions: next_actions.clone(),
        model: model.clone(),
        accounts: insight.accounts.clone(),
    }
}

fn volume_to_proto(volume: &Volume) -> ProtoVolume {
    ProtoVolume {
        inbound: volume.inbound,
        outbound: volume.outbound,
        threads: volume.threads,
        direction_ratio: volume.direction_ratio().unwrap_or(0.0),
        first_seen: volume.first_seen.unwrap_or(0),
        last_inbound: volume.last_inbound.unwrap_or(0),
        last_outbound: volume.last_outbound.unwrap_or(0),
    }
}

fn cadence_to_proto(cadence: Cadence) -> ProtoCadence {
    ProtoCadence {
        median_gap_seconds: cadence.median_gap_seconds.unwrap_or(0),
        longest_gap_seconds: cadence.longest_gap_seconds.unwrap_or(0),
        messages_per_week: cadence.messages_per_week,
    }
}

fn decay_to_proto(decay: Decay) -> ProtoDecay {
    ProtoDecay {
        silence_seconds: decay.silence_seconds.unwrap_or(0),
        dormant_after_seconds: decay.dormant_after_seconds,
        recent_messages: decay.recent_messages,
        prior_messages: decay.prior_messages,
        change_ratio: decay.change_ratio.unwrap_or(0.0),
        dormant: decay.dormant,
        declining: decay.declining,
    }
}

fn topic_to_proto(topic: &Topic) -> ProtoTopic {
    ProtoTopic {
        term: topic.term.clone(),
        messages: topic.messages,
    }
}

/// Project a subscription report onto the wire.
fn subscriptions_to_proto(report: &SubscriptionReport) -> ListSubscriptionsResponse {
    ListSubscriptionsResponse {
        since: report.since,
        until: report.until,
        senders: report.senders.iter().map(sender_to_proto).collect(),
        total_senders: u32::try_from(report.total_senders).unwrap_or(u32::MAX),
        headers_read: u32::try_from(report.headers_read).unwrap_or(u32::MAX),
        model_classified: u32::try_from(report.model_classified).unwrap_or(u32::MAX),
        model: report.model.clone(),
    }
}

fn sender_to_proto(sender: &Subscription) -> ProtoSender {
    ProtoSender {
        account_id: sender.account_id,
        address: sender.address.clone(),
        name: sender.name.clone().unwrap_or_default(),
        messages: sender.messages,
        read_messages: sender.read_messages,
        read_rate: sender.read_rate,
        first_seen: sender.first_seen.unwrap_or(0),
        last_seen: sender.last_seen.unwrap_or(0),
        median_gap_seconds: sender.median_gap_seconds.unwrap_or(0),
        your_replies: sender.your_replies,
        sender_class: class_to_proto(sender.class) as i32,
        source: source_to_proto_enum(sender.source) as i32,
        signals: sender.signals.clone(),
        unsubscribe: sender.unsubscribe.as_ref().map(unsubscribe_to_proto),
        headers_read: sender.headers_read,
        candidate: sender.candidate,
    }
}

fn class_to_proto(class: Class) -> ProtoClass {
    match class {
        Class::Newsletter => ProtoClass::Newsletter,
        Class::Transactional => ProtoClass::Transactional,
        Class::Automated => ProtoClass::Automated,
        Class::Personal => ProtoClass::Personal,
        Class::Unknown => ProtoClass::Unknown,
    }
}

fn source_to_proto_enum(source: Source) -> ProtoSource2 {
    match source {
        Source::Header => ProtoSource2::Header,
        Source::Heuristic => ProtoSource2::Heuristic,
        Source::Model => ProtoSource2::Model,
    }
}

fn unsubscribe_to_proto(unsubscribe: &Unsubscribe) -> ProtoUnsubscribe {
    ProtoUnsubscribe {
        http_url: unsubscribe.http_url.clone().unwrap_or_default(),
        mailto: unsubscribe.mailto.clone().unwrap_or_default(),
        one_click: unsubscribe.one_click,
    }
}

/// Project an analytics answer onto the wire.
fn answer_to_proto(answer: &AnalyticsAnswer) -> AskAnalyticsResponse {
    AskAnalyticsResponse {
        question: answer.question.clone(),
        sql: answer.sql.clone(),
        params: answer.params.clone(),
        notes: answer.notes.clone(),
        columns: answer.columns.clone(),
        rows: answer
            .rows
            .iter()
            .map(|row| AnalyticsRow {
                cells: row.iter().map(cell_to_proto).collect(),
            })
            .collect(),
        truncated: answer.truncated,
        narrative: answer.narrative.clone(),
        narrative_rows: u32::try_from(answer.narrative_rows).unwrap_or(u32::MAX),
        model: answer.model.clone(),
    }
}

/// One cell.
///
/// `null_value`/`unsupported` are set to `true` rather than to `false`: proto3
/// treats a `false` inside a `oneof` as present, so either spelling works on
/// the wire, and `true` is the one that does not read as "null is false".
fn cell_to_proto(cell: &Cell) -> AnalyticsCell {
    let value = match cell {
        Cell::Null => ProtoCellValue::NullValue(true),
        Cell::Integer(v) => ProtoCellValue::IntegerValue(*v),
        Cell::Real(v) => ProtoCellValue::RealValue(*v),
        Cell::Text(v) => ProtoCellValue::TextValue(v.clone()),
        Cell::Unsupported => ProtoCellValue::Unsupported(true),
    };
    AnalyticsCell { value: Some(value) }
}

/// Resolve the requested window, applying the "0 means the last completed
/// period" default.
///
/// The three shapes are deliberate and each has a use: no bounds is "what the
/// timer would have produced" (`mail digest`), a `since` alone is "since then,
/// up to now" (`mail digest --since 7d`), and an `until` alone is one cadence
/// ending there, which is how a caller re-asks for a specific past period.
fn window_from_proto(
    req: &GenerateDigestRequest,
    interval: i64,
    now: i64,
) -> Result<Period, Status> {
    for (name, value) in [
        ("account_id", req.account_id),
        ("since", req.since),
        ("until", req.until),
    ] {
        if value < 0 {
            return Err(Status::from(Error::invalid_argument(format!(
                "{name} must not be negative"
            ))));
        }
    }
    let period = match (req.since, req.until) {
        (0, 0) => schedule::last_completed(now, interval),
        (since, 0) => Period {
            start: since,
            end: now,
        },
        (0, until) => Period {
            start: until.saturating_sub(interval),
            end: until,
        },
        (since, until) => Period {
            start: since,
            end: until,
        },
    };
    if period.end <= period.start {
        return Err(Status::from(Error::invalid_argument(
            "a digest window must end after it starts",
        )));
    }
    Ok(period)
}

/// Project a finished briefing onto the wire.
fn digest_to_proto(report: &DigestReport) -> GenerateDigestResponse {
    GenerateDigestResponse {
        digest_id: report.id,
        since: report.period.start,
        until: report.period.end,
        account_id: report.account_id,
        generated_at: report.generated_at,
        markdown: report.markdown.clone(),
        sections: report
            .briefing
            .sections
            .iter()
            .map(|(section, lines)| ProtoSection {
                id: section.id().to_owned(),
                heading: section.heading().to_owned(),
                lines: lines
                    .iter()
                    .map(|line| ProtoLine {
                        text: line.text.clone(),
                        message_ids: line.message_ids.clone(),
                    })
                    .collect(),
            })
            .collect(),
        sources: report.sources.iter().map(source_to_proto).collect(),
        model: report.model.clone(),
        considered: report.considered,
        packed: report.packed,
        withheld_by_policy: report.withheld,
        clusters: report.clusters,
        cached: report.cached,
        empty: report.empty,
    }
}

fn source_to_proto(source: &StoredSource) -> ProtoSource {
    ProtoSource {
        label: source.label,
        message_id: source.message_id,
        message_uid: source.message_uid,
        account_id: source.account_id,
        mailbox: source.mailbox.clone(),
        subject: source.subject.clone(),
        from_addr: source.from_addr.clone(),
        date: source.date.unwrap_or(0),
        cited: source.cited,
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
