//! Integration test: drive `AnalyticsService.GetResponseTimes` end-to-end
//! against an in-process tonic server over a Unix domain socket — the real
//! auth layer, the real codec, the real database — covering the happy path,
//! the `--by mailbox` grouping, the bottleneck flag, and the `Status` a
//! malformed request comes back with.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use rmail_core::repo;
use rmail_proto::v1::analytics_service_client::AnalyticsServiceClient;
use rmail_proto::v1::{
    AskAnalyticsRequest, GetContactInsightRequest, GetResponseTimesRequest,
    ListSubscriptionsRequest, ResponseTimeGroup, ResponseTimeGroupBy, SubscriptionClass,
    SubscriptionSource,
};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tonic::transport::Channel;
use tonic::Code;

static COUNTER: AtomicU32 = AtomicU32::new(0);

const T0: i64 = 1_700_000_000;
const HOUR: i64 = 3_600;
const DAY: i64 = 86_400;
const ME: &str = "me@example.com";

struct TestServer {
    socket: PathBuf,
    db_path: PathBuf,
    db: rmail_core::Database,
    account_id: i64,
    inbox: i64,
    sent: i64,
    next_uid: i64,
    shutdown: oneshot::Sender<()>,
    handle: JoinHandle<Result<(), rmaild::ServeError>>,
}

impl TestServer {
    async fn start() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let socket = PathBuf::from("/tmp").join(format!("rmail-analytics-{pid}-{n}.sock"));
        let db_path = std::env::temp_dir().join(format!("rmail-analytics-{pid}-{n}.db"));
        let db = rmail_core::Database::open(&db_path).unwrap();

        let (account_id, inbox, sent) = db
            .with_write(|c| {
                let account_id = repo::insert_account(
                    c,
                    &repo::NewAccount {
                        name: "Personal".to_owned(),
                        username: Some(ME.to_owned()),
                        ..Default::default()
                    },
                )?;
                let inbox = repo::insert_mailbox(
                    c,
                    &repo::NewMailbox {
                        account_id,
                        name: "INBOX".to_owned(),
                        ..Default::default()
                    },
                )?;
                let sent = repo::insert_mailbox(
                    c,
                    &repo::NewMailbox {
                        account_id,
                        name: "Sent".to_owned(),
                        ..Default::default()
                    },
                )?;
                Ok((account_id, inbox, sent))
            })
            .unwrap();

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let server_socket = socket.clone();
        let server_db = db.clone();
        let handle = tokio::spawn(async move {
            let mut config = rmail_core::Config::default();
            config.index.semantic.enabled = false;
            // No provider for this suite, said explicitly rather than assumed.
            // `ai.enabled` defaults *on* and building a Claude client does not
            // validate its key, so a daemon left at the default constructs a
            // provider that only fails when it is used — and the model-backed
            // RPCs then decline with `UNAUTHENTICATED` (an accurate statement
            // about that daemon) instead of the `FAILED_PRECONDITION` these
            // tests are about (an accurate statement about a daemon with no
            // AI subsystem at all).
            config.ai.enabled = false;
            rmaild::serve_uds_with_config(&server_socket, server_db, config, async move {
                let _ = shutdown_rx.await;
            })
            .await
        });

        let mut ready = false;
        for _ in 0..200 {
            if rmail_core::connect_uds(&socket).await.is_ok() {
                ready = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(ready, "server never became ready");

        Self {
            socket,
            db_path,
            db,
            account_id,
            inbox,
            sent,
            next_uid: 1,
            shutdown: shutdown_tx,
            handle,
        }
    }

    async fn client(&self) -> AnalyticsServiceClient<Channel> {
        let channel = rmail_core::connect_uds(&self.socket).await.unwrap();
        AnalyticsServiceClient::new(channel)
    }

    /// Seed a message directly. `AnalyticsService` is read-only; putting mail
    /// in the mirror is `rmail_core`'s job, and this suite only needs rows to
    /// exist so the report has something real to summarize.
    fn seed(
        &mut self,
        message_id: &str,
        from: &str,
        at: i64,
        parent: Option<&str>,
        mailbox_id: Option<i64>,
    ) {
        let uid = self.next_uid;
        self.next_uid += 1;
        let mailbox_id = mailbox_id.unwrap_or(if from == ME { self.sent } else { self.inbox });
        let new = repo::NewMessage {
            account_id: self.account_id,
            mailbox_id,
            uid,
            uidvalidity: 1,
            message_id: Some(message_id.to_owned()),
            in_reply_to: parent.map(str::to_owned),
            subject: Some(message_id.to_owned()),
            from_addr: Some(from.to_owned()),
            date: Some(at),
            ..Default::default()
        };
        self.db
            .with_write(|c| {
                let id = repo::insert_message(c, &new)?;
                rmail_core::thread::assign_thread(c, id)?;
                Ok(())
            })
            .unwrap();
    }

    /// Seed a message *with* its raw header block, which is what subscription
    /// detection reads. Separate from [`Self::seed`] because the response-time
    /// suite deliberately stores no `raw` — that module never reads one, and a
    /// fixture carrying octets nobody looks at would suggest it did.
    fn seed_raw(&mut self, message_id: &str, from: &str, at: i64, headers: &str) {
        let uid = self.next_uid;
        self.next_uid += 1;
        let raw = format!("From: {from}\r\n{headers}\r\n\r\nBody.\r\n").into_bytes();
        let new = repo::NewMessage {
            account_id: self.account_id,
            mailbox_id: self.inbox,
            uid,
            uidvalidity: 1,
            message_id: Some(message_id.to_owned()),
            subject: Some("This week".to_owned()),
            from_addr: Some(from.to_owned()),
            date: Some(at),
            raw: Some(raw),
            ..Default::default()
        };
        self.db
            .with_write(|c| {
                let id = repo::insert_message(c, &new)?;
                rmail_core::thread::assign_thread(c, id)?;
                Ok(())
            })
            .unwrap();
    }

    /// A thread they open and we answer `after` seconds later.
    fn exchange(&mut self, tag: &str, them: &str, at: i64, after: i64) {
        let inbound = format!("{tag}-in@x");
        self.seed(&inbound, them, at, None, None);
        // The reply carries `To:`. Response-time analysis pairs on
        // `In-Reply-To` and never reads a recipient, so this fixture used to
        // leave `to_addrs` null — but contact insight counts an outbound
        // message *to a contact*, which is a claim about recipients, and a
        // reply with no `To:` is not a message anyone could have sent.
        self.seed_to(
            &format!("{tag}-out@x"),
            ME,
            Some(them),
            at + after,
            Some(&inbound),
        );
    }

    /// [`Self::seed`] with a `To:` recipient list.
    fn seed_to(
        &mut self,
        message_id: &str,
        from: &str,
        to: Option<&str>,
        at: i64,
        parent: Option<&str>,
    ) {
        let uid = self.next_uid;
        self.next_uid += 1;
        let mailbox_id = if from == ME { self.sent } else { self.inbox };
        let new = repo::NewMessage {
            account_id: self.account_id,
            mailbox_id,
            uid,
            uidvalidity: 1,
            message_id: Some(message_id.to_owned()),
            in_reply_to: parent.map(str::to_owned),
            subject: Some(message_id.to_owned()),
            from_addr: Some(from.to_owned()),
            to_addrs: to.map(str::to_owned),
            date: Some(at),
            ..Default::default()
        };
        self.db
            .with_write(|c| {
                let id = repo::insert_message(c, &new)?;
                rmail_core::thread::assign_thread(c, id)?;
                Ok(())
            })
            .unwrap();
    }

    fn request(&self) -> GetResponseTimesRequest {
        GetResponseTimesRequest {
            account_id: self.account_id,
            group_by: ResponseTimeGroupBy::Contact as i32,
            since: T0,
            until: T0 + 30 * DAY,
            bucket_seconds: 7 * DAY,
            window_seconds: 28 * DAY,
            limit: 0,
            min_samples: 0,
            bottleneck_ratio: 0.0,
        }
    }

    async fn stop(self) {
        let _ = self.shutdown.send(());
        let _ = tokio::time::timeout(Duration::from_secs(5), self.handle).await;
        for suffix in ["", "-wal", "-shm"] {
            let _ =
                std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.db_path.display())));
        }
        let _ = std::fs::remove_file(&self.socket);
    }
}

fn group<'a>(groups: &'a [ResponseTimeGroup], key: &str) -> &'a ResponseTimeGroup {
    groups
        .iter()
        .find(|g| g.key == key)
        .unwrap_or_else(|| panic!("no group {key:?} in {:?}", groups))
}

#[tokio::test]
async fn reports_per_contact_percentiles_over_the_wire() {
    let mut server = TestServer::start().await;
    for (i, after) in [HOUR, 2 * HOUR, 3 * HOUR].into_iter().enumerate() {
        server.exchange(
            &format!("a{i}"),
            "alice@example.com",
            T0 + DAY + i as i64 * DAY,
            after,
        );
    }
    server.exchange("b0", "bob@example.com", T0 + DAY, 5 * DAY);

    let report = server
        .client()
        .await
        .get_response_times(server.request())
        .await
        .unwrap()
        .into_inner();

    assert_eq!(report.since, T0);
    assert_eq!(report.until, T0 + 30 * DAY);
    assert_eq!(report.group_by, ResponseTimeGroupBy::Contact as i32);
    assert_eq!(report.pairs, 4);
    assert_eq!(report.self_addresses, vec![ME.to_owned()]);
    assert_eq!(report.ours.unwrap().samples, 4);
    assert_eq!(report.total_groups, 2);

    let alice = group(&report.groups, "alice@example.com");
    assert_eq!(alice.ours.unwrap().samples, 3);
    assert_eq!(alice.ours.unwrap().p50_seconds, 2 * HOUR);
    assert_eq!(alice.ours.unwrap().p90_seconds, 3 * HOUR);
    assert_eq!(
        group(&report.groups, "bob@example.com")
            .ours
            .unwrap()
            .p50_seconds,
        5 * DAY
    );

    // 30 days at a 7-day bucket, newest point ending exactly at `until`.
    assert_eq!(report.trend.len(), 5);
    assert_eq!(report.trend.last().unwrap().window_end, T0 + 30 * DAY);

    server.stop().await;
}

#[tokio::test]
async fn groups_by_mailbox_when_asked_to() {
    let mut server = TestServer::start().await;
    let project = server
        .db
        .with_write(|c| {
            repo::insert_mailbox(
                c,
                &repo::NewMailbox {
                    account_id: server.account_id,
                    name: "Projects".to_owned(),
                    ..Default::default()
                },
            )
        })
        .unwrap();
    server.seed("p-in@x", "alice@example.com", T0 + DAY, None, Some(project));
    server.seed("p-out@x", ME, T0 + DAY + 4 * DAY, Some("p-in@x"), None);
    server.exchange("i0", "bob@example.com", T0 + DAY, HOUR);

    let mut request = server.request();
    request.group_by = ResponseTimeGroupBy::Mailbox as i32;
    let report = server
        .client()
        .await
        .get_response_times(request)
        .await
        .unwrap()
        .into_inner();

    assert_eq!(report.group_by, ResponseTimeGroupBy::Mailbox as i32);
    let projects = group(&report.groups, "Projects");
    assert_eq!(projects.mailbox_id, project);
    assert_eq!(projects.ours.unwrap().p50_seconds, 4 * DAY);
    assert_eq!(
        group(&report.groups, "INBOX").ours.unwrap().p50_seconds,
        HOUR
    );
    assert!(
        report.groups.iter().all(|g| g.key != "Sent"),
        "a pair is keyed on the side that waited, never on Sent"
    );

    server.stop().await;
}

#[tokio::test]
async fn flags_the_bottleneck_and_explains_which_arm_fired() {
    let mut server = TestServer::start().await;
    for i in 0..4 {
        let base = T0 + (i + 1) * DAY;
        // She opens, we take ten hours.
        server.seed(&format!("t{i}-in@x"), "alice@example.com", base, None, None);
        server.seed(
            &format!("t{i}-out@x"),
            ME,
            base + 10 * HOUR,
            Some(&format!("t{i}-in@x")),
            None,
        );
        // We open, she takes an hour, and we close the thread so `overdue`
        // stays out of it — this test is about the ratio arm alone.
        server.seed(&format!("u{i}-out@x"), ME, base, None, None);
        server.seed(
            &format!("u{i}-in@x"),
            "alice@example.com",
            base + HOUR,
            Some(&format!("u{i}-out@x")),
            None,
        );
        server.seed(
            &format!("u{i}-ack@x"),
            ME,
            base + 11 * HOUR,
            Some(&format!("u{i}-in@x")),
            None,
        );
    }

    let report = server
        .client()
        .await
        .get_response_times(server.request())
        .await
        .unwrap()
        .into_inner();

    let alice = group(&report.groups, "alice@example.com");
    assert_eq!(alice.ours.unwrap().p50_seconds, 10 * HOUR);
    assert_eq!(alice.theirs.unwrap().p50_seconds, HOUR);
    assert!(alice.bottleneck);
    assert!(alice.slower_than_counterpart);
    assert!(!alice.stalled);
    assert_eq!(alice.overdue, 0);

    server.stop().await;
}

#[tokio::test]
async fn an_all_zero_request_defaults_to_the_last_ninety_days() {
    let server = TestServer::start().await;
    let report = server
        .client()
        .await
        .get_response_times(GetResponseTimesRequest {
            account_id: 0,
            group_by: 0,
            since: 0,
            until: 0,
            bucket_seconds: 0,
            window_seconds: 0,
            limit: 0,
            min_samples: 0,
            bottleneck_ratio: 0.0,
        })
        .await
        .unwrap()
        .into_inner();

    assert_eq!(
        report.until - report.since,
        90 * DAY,
        "an unset window is 90 days, not the epoch"
    );
    assert!(report.until > T0, "an unset `until` is the daemon's clock");
    assert_eq!(report.group_by, ResponseTimeGroupBy::Contact as i32);
    assert!(!report.trend.is_empty());

    server.stop().await;
}

#[tokio::test]
async fn an_inverted_window_comes_back_as_invalid_argument() {
    let server = TestServer::start().await;
    let mut request = server.request();
    request.since = request.until;
    let status = server
        .client()
        .await
        .get_response_times(request)
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(
        status.message().contains("since"),
        "unhelpful message: {}",
        status.message()
    );
    server.stop().await;
}

#[tokio::test]
async fn a_bucket_too_fine_for_the_range_comes_back_as_invalid_argument() {
    let server = TestServer::start().await;
    let mut request = server.request();
    request.until = request.since + 5 * 365 * DAY;
    request.bucket_seconds = HOUR;
    let status = server
        .client()
        .await
        .get_response_times(request)
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(status.message().contains("bucket_seconds"));
    server.stop().await;
}

#[tokio::test]
async fn a_negative_bound_comes_back_as_invalid_argument() {
    let server = TestServer::start().await;
    let mut request = server.request();
    request.since = -1;
    let status = server
        .client()
        .await
        .get_response_times(request)
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(status.message().contains("must not be negative"));
    server.stop().await;
}

#[tokio::test]
async fn a_ratio_below_one_comes_back_as_invalid_argument() {
    let server = TestServer::start().await;
    let mut request = server.request();
    request.bottleneck_ratio = 0.25;
    let status = server
        .client()
        .await
        .get_response_times(request)
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(status.message().contains("bottleneck_ratio"));
    server.stop().await;
}

// ---------------------------------------------------------------------------
// Task 72: contact insight, subscriptions, NL analytics
// ---------------------------------------------------------------------------
//
// This harness runs the daemon with `ai.enabled = false` — a daemon with no
// AI subsystem at all — so every one of these exercises the *model-free* half of the
// three new RPCs end to end, plus the `FAILED_PRECONDITION` the model-backed
// halves decline with. That split is the point: two of the three are usable
// on a daemon with no provider at all, and a suite that could only test them
// with one would not have proven it.

/// The full deterministic report, over the wire, with the zero-means-default
/// fields resolved by the handler rather than by the caller.
#[tokio::test]
async fn contact_insight_reports_the_numbers_without_a_model() {
    let mut server = TestServer::start().await;
    // Three exchanges: they open, we answer an hour later each time.
    for (i, at) in [T0 + DAY, T0 + 3 * DAY, T0 + 5 * DAY]
        .into_iter()
        .enumerate()
    {
        server.exchange(&format!("c{i}"), "ada@example.com", at, HOUR);
    }
    // ... and one of theirs we never answered.
    server.seed("orphan@x", "ada@example.com", T0 + 6 * DAY, None, None);

    let insight = server
        .client()
        .await
        .get_contact_insight(GetContactInsightRequest {
            account_id: server.account_id,
            address: "Ada@Example.COM".to_owned(),
            since: T0,
            until: T0 + 30 * DAY,
            topic_limit: 0,
            metrics_only: true,
        })
        .await
        .unwrap()
        .into_inner();

    assert_eq!(insight.address, "ada@example.com", "normalized by the core");
    assert_eq!(insight.since, T0);
    assert_eq!(insight.until, T0 + 30 * DAY);
    let volume = insight.volume.unwrap();
    assert_eq!(volume.inbound, 4);
    assert_eq!(volume.outbound, 3);
    let ours = insight.ours.unwrap();
    assert_eq!(ours.samples, 3);
    assert_eq!(ours.p50_seconds, HOUR);
    assert_eq!(insight.awaiting_reply, 1);
    assert_eq!(insight.accounts, vec![server.account_id]);
    assert!(
        insight.briefing.is_empty() && insight.model.is_empty(),
        "metrics_only must not have called a model"
    );
    assert!(insight.cadence.is_some() && insight.decay.is_some());
    server.stop().await;
}

/// The briefing declines on a daemon with no provider — and says which
/// switch, and that the numbers are still reachable.
#[tokio::test]
async fn a_contact_briefing_declines_without_an_ai_subsystem() {
    let server = TestServer::start().await;
    let status = server
        .client()
        .await
        .get_contact_insight(GetContactInsightRequest {
            account_id: server.account_id,
            address: "ada@example.com".to_owned(),
            metrics_only: false,
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::FailedPrecondition);
    assert!(status.message().contains("ai.enabled"), "{status:?}");
    assert!(status.message().contains("metrics_only"), "{status:?}");
    server.stop().await;
}

#[tokio::test]
async fn a_blank_contact_address_comes_back_as_invalid_argument() {
    let server = TestServer::start().await;
    let status = server
        .client()
        .await
        .get_contact_insight(GetContactInsightRequest {
            account_id: server.account_id,
            address: "   ".to_owned(),
            metrics_only: true,
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);
    server.stop().await;
}

/// Header detection end to end, and the assertion that matters most in this
/// file: the unsubscribe method comes back as a *proposal*, with nothing
/// having been fetched or sent.
#[tokio::test]
async fn subscriptions_detect_a_newsletter_and_only_propose_leaving_it() {
    let mut server = TestServer::start().await;
    for week in 0..8 {
        server.seed_raw(
            &format!("n{week}@x"),
            "news@example.com",
            T0 + week * 7 * DAY,
            "List-Id: Weekly <weekly.example.com>\r\n\
             List-Unsubscribe: <https://example.com/u/1>\r\n\
             List-Unsubscribe-Post: List-Unsubscribe=One-Click\r\n\
             Precedence: bulk",
        );
    }

    let report = server
        .client()
        .await
        .list_subscriptions(ListSubscriptionsRequest {
            account_id: server.account_id,
            since: T0,
            until: T0 + 180 * DAY,
            limit: 0,
            candidates_only: true,
            classify_unknown: false,
        })
        .await
        .unwrap()
        .into_inner();

    assert_eq!(report.senders.len(), 1, "{:?}", report.senders);
    let sender = &report.senders[0];
    assert_eq!(sender.address, "news@example.com");
    assert_eq!(sender.sender_class, SubscriptionClass::Newsletter as i32);
    assert_eq!(sender.source, SubscriptionSource::Header as i32);
    assert_eq!(sender.messages, 8);
    assert_eq!(sender.read_messages, 0);
    assert!(sender.candidate);
    assert!(sender.headers_read);
    assert_eq!(report.headers_read, 1);
    assert_eq!(report.model_classified, 0);
    assert!(report.model.is_empty(), "no model was called");

    let unsubscribe = sender.unsubscribe.clone().unwrap();
    assert_eq!(unsubscribe.http_url, "https://example.com/u/1");
    assert!(unsubscribe.one_click);
    assert!(unsubscribe.mailto.is_empty());
    server.stop().await;
}

#[tokio::test]
async fn classifying_unknown_senders_declines_without_an_ai_subsystem() {
    let server = TestServer::start().await;
    let status = server
        .client()
        .await
        .list_subscriptions(ListSubscriptionsRequest {
            account_id: server.account_id,
            classify_unknown: true,
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::FailedPrecondition);
    assert!(status.message().contains("classify_unknown"), "{status:?}");
    server.stop().await;
}

#[tokio::test]
async fn an_inverted_subscription_window_comes_back_as_invalid_argument() {
    let server = TestServer::start().await;
    let status = server
        .client()
        .await
        .list_subscriptions(ListSubscriptionsRequest {
            account_id: server.account_id,
            since: T0 + DAY,
            until: T0,
            ..Default::default()
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);
    server.stop().await;
}

/// `AskAnalytics` has no model-free half: without a provider there is no SQL
/// to run, and `UNIMPLEMENTED`-shaped silence would be less useful than saying
/// which switch is off.
#[tokio::test]
async fn ask_analytics_declines_without_an_ai_subsystem() {
    let server = TestServer::start().await;
    let status = server
        .client()
        .await
        .ask_analytics(AskAnalyticsRequest {
            account_id: server.account_id,
            question: "who writes to me the most?".to_owned(),
            narrate: true,
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::FailedPrecondition);
    assert!(status.message().contains("ai.enabled"), "{status:?}");
    server.stop().await;
}

#[tokio::test]
async fn a_negative_account_on_ask_analytics_comes_back_as_invalid_argument() {
    let server = TestServer::start().await;
    let status = server
        .client()
        .await
        .ask_analytics(AskAnalyticsRequest {
            account_id: -1,
            question: "anything".to_owned(),
            narrate: false,
        })
        .await
        .unwrap_err();
    assert_eq!(status.code(), Code::InvalidArgument);
    assert!(status.message().contains("must not be negative"));
    server.stop().await;
}

/// The views the sandbox reads must exist on a freshly migrated database, and
/// must be readable through the ordinary read pool. A migration that created
/// a view referencing a column that had been renamed would otherwise only fail
/// the first time somebody asked a question.
#[tokio::test]
async fn the_analytics_views_exist_and_are_queryable_after_migration() {
    let mut server = TestServer::start().await;
    server.seed("v1@x", "ada@example.com", T0 + DAY, None, None);
    server.seed("v2@x", ME, T0 + 2 * DAY, Some("v1@x"), None);

    for view in [
        "analytics_messages",
        "analytics_senders",
        "analytics_daily",
        "analytics_threads",
        "analytics_mailboxes",
        "analytics_contacts",
    ] {
        let sql = format!("SELECT count(*) FROM {view}");
        let count: i64 = server
            .db
            .with_read(move |conn| conn.query_row(&sql, [], |row| row.get(0)))
            .unwrap_or_else(|error| panic!("{view} is not queryable: {error}"));
        assert!(count >= 0);
    }

    // The `direction` heuristic really does split the two folders.
    let outbound: i64 = server
        .db
        .with_read(|conn| {
            conn.query_row(
                "SELECT count(*) FROM analytics_messages WHERE direction = 'outbound'",
                [],
                |row| row.get(0),
            )
        })
        .unwrap();
    assert_eq!(outbound, 1, "the Sent folder message was not outbound");
    server.stop().await;
}
