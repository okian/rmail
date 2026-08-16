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
use rmail_proto::v1::{GetResponseTimesRequest, ResponseTimeGroup, ResponseTimeGroupBy};
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
            rmaild::serve_uds(&server_socket, server_db, async move {
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

    /// A thread they open and we answer `after` seconds later.
    fn exchange(&mut self, tag: &str, them: &str, at: i64, after: i64) {
        let inbound = format!("{tag}-in@x");
        self.seed(&inbound, them, at, None, None);
        self.seed(
            &format!("{tag}-out@x"),
            ME,
            at + after,
            Some(&inbound),
            None,
        );
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
