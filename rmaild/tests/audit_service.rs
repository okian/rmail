//! Integration test: drive `AuditService` end-to-end against an in-process
//! tonic server over a Unix domain socket, covering `QueryAiCalls` (including
//! pagination and filtering) and `ExportLedger`, plus the error/`Status` path
//! for a malformed filter.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use rmail_core::ai::{CallOutcome, CallRecord, Usage};
use rmail_proto::v1::audit_service_client::AuditServiceClient;
use rmail_proto::v1::{AuditFilter, CallStatus, ExportLedgerRequest, QueryAiCallsRequest};
use sha2::Digest;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_stream::StreamExt;
use tonic::transport::Channel;
use tonic::Code;

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct TestServer {
    socket: PathBuf,
    db_path: PathBuf,
    // Kept so tests can seed the ledger directly — `AuditService` is
    // read-only, and recording a call is `rmail_core::ai`'s job, not a gRPC
    // one. `Database` clones share the same handle as the server's.
    db: rmail_core::Database,
    shutdown: oneshot::Sender<()>,
    handle: JoinHandle<Result<(), rmaild::ServeError>>,
}

impl TestServer {
    async fn start() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let socket = PathBuf::from("/tmp").join(format!("rmail-audit-{pid}-{n}.sock"));
        let db_path = std::env::temp_dir().join(format!("rmail-audit-{pid}-{n}.db"));
        let db = rmail_core::Database::open(&db_path).unwrap();

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
            shutdown: shutdown_tx,
            handle,
        }
    }

    async fn client(&self) -> AuditServiceClient<Channel> {
        let channel = rmail_core::connect_uds(&self.socket).await.unwrap();
        AuditServiceClient::new(channel)
    }

    /// Seed the ledger directly, bypassing gRPC entirely — the ledger's write
    /// path is `rmail_core::ai::record_call`, exercised by rmail-core's own
    /// tests; this integration suite only needs rows to exist so the read
    /// RPCs have something real to return.
    async fn seed(&self, model: &str) -> i64 {
        rmail_core::ai::record_call(
            &self.db,
            CallRecord {
                account_id: None,
                message_id: None,
                request_id: Some("msg_test".to_owned()),
                model: model.to_owned(),
                pass: Some("triage".to_owned()),
                usage: Usage {
                    input_tokens: 100,
                    output_tokens: 20,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 0,
                },
                redaction_level: "none".to_owned(),
                latency: Duration::from_millis(15),
                payload: b"a redacted request body",
                outcome: CallOutcome::Ok,
            },
        )
        .await
        .unwrap()
    }

    /// As [`Self::seed`], but recorded with an error outcome — for the
    /// `status` filter test.
    async fn seed_error(&self, model: &str) -> i64 {
        rmail_core::ai::record_call(
            &self.db,
            CallRecord {
                account_id: None,
                message_id: None,
                request_id: None,
                model: model.to_owned(),
                pass: None,
                usage: Usage {
                    input_tokens: 100,
                    output_tokens: 0,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 0,
                },
                redaction_level: "none".to_owned(),
                latency: Duration::from_millis(5),
                payload: b"a redacted request body",
                outcome: CallOutcome::Error("upstream 529: overloaded".to_owned()),
            },
        )
        .await
        .unwrap()
    }

    /// Seed `count` calls sharing one model — for the `ExportLedger`
    /// multi-page test, which needs enough rows to force the internal paging
    /// loop across a batch boundary. Returns the ids in insertion order.
    async fn seed_many(&self, count: usize, model: &str) -> Vec<i64> {
        let mut ids = Vec::with_capacity(count);
        for _ in 0..count {
            ids.push(self.seed(model).await);
        }
        ids
    }

    async fn shutdown(self) {
        self.shutdown.send(()).unwrap();
        self.handle.await.unwrap().unwrap();
        for suffix in ["", "-wal", "-shm"] {
            let _ =
                std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.db_path.display())));
        }
    }
}

#[tokio::test]
async fn audit_service_query_ai_calls_returns_recorded_entries() {
    let server = TestServer::start().await;
    server.seed("claude-haiku-4-5").await;
    server.seed("claude-opus-4-8").await;
    let mut client = server.client().await;

    let response = client
        .query_ai_calls(QueryAiCallsRequest {
            filter: None,
            limit: 10,
            before_id: None,
        })
        .await
        .expect("query_ai_calls")
        .into_inner();

    assert_eq!(response.entries.len(), 2);
    assert!(!response.has_more);
    // Newest first.
    assert_eq!(response.entries[0].model, "claude-opus-4-8");
    assert_eq!(response.entries[1].model, "claude-haiku-4-5");
    let entry = &response.entries[0];
    assert_eq!(entry.status, CallStatus::Ok as i32);
    assert_eq!(entry.redaction_level, "none");
    assert_eq!(entry.pass.as_deref(), Some("triage"));
    assert_eq!(entry.input_tokens, 100);
    assert_eq!(entry.output_tokens, 20);
    assert!(entry.cost_usd > 0.0);
    assert_eq!(
        entry.payload_sha256,
        sha2::Sha256::digest(b"a redacted request body").to_vec()
    );

    server.shutdown().await;
}

#[tokio::test]
async fn audit_service_query_ai_calls_filters_by_model() {
    let server = TestServer::start().await;
    server.seed("claude-haiku-4-5").await;
    server.seed("claude-opus-4-8").await;
    let mut client = server.client().await;

    let response = client
        .query_ai_calls(QueryAiCallsRequest {
            filter: Some(AuditFilter {
                model: Some("claude-opus-4-8".to_owned()),
                ..Default::default()
            }),
            limit: 10,
            before_id: None,
        })
        .await
        .expect("query_ai_calls")
        .into_inner();

    assert_eq!(response.entries.len(), 1);
    assert_eq!(response.entries[0].model, "claude-opus-4-8");

    server.shutdown().await;
}

#[tokio::test]
async fn audit_service_query_ai_calls_paginates_and_reports_has_more() {
    let server = TestServer::start().await;
    let mut ids = Vec::new();
    for _ in 0..3 {
        ids.push(server.seed("claude-haiku-4-5").await);
    }
    let mut client = server.client().await;

    let first_page = client
        .query_ai_calls(QueryAiCallsRequest {
            filter: None,
            limit: 2,
            before_id: None,
        })
        .await
        .expect("first page")
        .into_inner();
    assert_eq!(first_page.entries.len(), 2);
    assert!(first_page.has_more);
    assert_eq!(first_page.entries[0].id, ids[2]);
    assert_eq!(first_page.entries[1].id, ids[1]);

    let second_page = client
        .query_ai_calls(QueryAiCallsRequest {
            filter: None,
            limit: 2,
            before_id: Some(first_page.entries[1].id),
        })
        .await
        .expect("second page")
        .into_inner();
    assert_eq!(second_page.entries.len(), 1);
    assert!(!second_page.has_more);
    assert_eq!(second_page.entries[0].id, ids[0]);

    server.shutdown().await;
}

#[tokio::test]
async fn audit_service_query_ai_calls_rejects_unknown_status_filter() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let status = client
        .query_ai_calls(QueryAiCallsRequest {
            filter: Some(AuditFilter {
                // Not a value any `CallStatus` variant is assigned to.
                status: Some(999),
                ..Default::default()
            }),
            limit: 10,
            before_id: None,
        })
        .await
        .expect_err("an unrecognized status filter must be rejected");
    assert_eq!(status.code(), Code::InvalidArgument);

    server.shutdown().await;
}

#[tokio::test]
async fn audit_service_query_ai_calls_filters_by_status() {
    let server = TestServer::start().await;
    server.seed("claude-haiku-4-5").await;
    server.seed_error("claude-haiku-4-5").await;
    let mut client = server.client().await;

    let ok_only = client
        .query_ai_calls(QueryAiCallsRequest {
            filter: Some(AuditFilter {
                status: Some(CallStatus::Ok as i32),
                ..Default::default()
            }),
            limit: 10,
            before_id: None,
        })
        .await
        .expect("query_ai_calls")
        .into_inner();
    assert_eq!(ok_only.entries.len(), 1);
    assert_eq!(ok_only.entries[0].status, CallStatus::Ok as i32);
    assert_eq!(ok_only.entries[0].error, None);

    let errors_only = client
        .query_ai_calls(QueryAiCallsRequest {
            filter: Some(AuditFilter {
                status: Some(CallStatus::Error as i32),
                ..Default::default()
            }),
            limit: 10,
            before_id: None,
        })
        .await
        .expect("query_ai_calls")
        .into_inner();
    assert_eq!(errors_only.entries.len(), 1);
    assert_eq!(errors_only.entries[0].status, CallStatus::Error as i32);
    assert_eq!(
        errors_only.entries[0].error.as_deref(),
        Some("upstream 529: overloaded")
    );

    server.shutdown().await;
}

#[tokio::test]
async fn audit_service_export_ledger_streams_every_matching_entry() {
    let server = TestServer::start().await;
    for _ in 0..5 {
        server.seed("claude-haiku-4-5").await;
    }
    let mut client = server.client().await;

    let stream = client
        .export_ledger(ExportLedgerRequest { filter: None })
        .await
        .expect("export_ledger")
        .into_inner();

    let entries: Vec<_> = stream
        .map(|item| item.expect("stream item"))
        .collect()
        .await;
    // Export is not paginated by the caller — every seeded row comes back in
    // one stream, unlike QueryAiCalls's default page size.
    assert_eq!(entries.len(), 5);

    server.shutdown().await;
}

#[tokio::test]
async fn audit_service_export_ledger_pages_past_a_single_batch() {
    let server = TestServer::start().await;
    // `ExportLedger` pages internally in batches of 500
    // (`rmaild::audit_service::EXPORT_BATCH_SIZE`); seeding one row more than
    // that forces the id-cursor loop to advance across a batch boundary. A
    // test with fewer rows than the batch size — like the one above — would
    // pass identically whether or not the cursor advance were implemented at
    // all, since it would only ever exercise a single page.
    const ROWS: usize = 501;
    let mut ids = server.seed_many(ROWS, "claude-haiku-4-5").await;
    let mut client = server.client().await;

    let stream = client
        .export_ledger(ExportLedgerRequest { filter: None })
        .await
        .expect("export_ledger")
        .into_inner();
    let entries: Vec<_> = stream
        .map(|item| item.expect("stream item"))
        .collect()
        .await;
    let returned_ids: Vec<i64> = entries.iter().map(|entry| entry.id).collect();
    assert_eq!(returned_ids.len(), ROWS);

    // Strictly descending across the whole stream: a cursor bug at the batch
    // seam (repeating the boundary row, skipping past it, or resetting to
    // the start) would show up here even though the row *count* still came
    // out right.
    let mut sorted_desc = returned_ids.clone();
    sorted_desc.sort_unstable_by(|a, b| b.cmp(a));
    assert_eq!(
        returned_ids, sorted_desc,
        "entries must be strictly newest-first"
    );

    // Exactly the seeded set — no duplicate row and nothing dropped at the
    // boundary.
    ids.sort_unstable();
    let mut returned_sorted = returned_ids;
    returned_sorted.sort_unstable();
    assert_eq!(returned_sorted, ids);

    server.shutdown().await;
}
