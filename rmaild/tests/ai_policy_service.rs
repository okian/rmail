//! Integration test: drive `AiPolicyService` end-to-end against an in-process
//! tonic server over a Unix domain socket — `SetBudget` round-tripping
//! through storage, `GetSpend` reporting real ledger spend against the caps
//! actually in force, and the error/`Status` paths for the arguments a client
//! can get wrong.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use rmail_core::ai::{CallOutcome, CallRecord, Usage, WorkClass};
use rmail_proto::v1::ai_policy_service_client::AiPolicyServiceClient;
use rmail_proto::v1::{
    BudgetCaps, BudgetClass, BudgetWindowCaps, GetSpendRequest, SetBudgetRequest,
};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tonic::transport::Channel;
use tonic::Code;

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct TestServer {
    socket: PathBuf,
    db_path: PathBuf,
    /// Kept so tests can seed the ledger directly — spend is recorded by
    /// `rmail_core::ai::record_call*`, not by any RPC this service exposes.
    db: rmail_core::Database,
    shutdown: oneshot::Sender<()>,
    handle: JoinHandle<Result<(), rmaild::ServeError>>,
}

impl TestServer {
    async fn start() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let socket = PathBuf::from("/tmp").join(format!("rmail-aipol-{pid}-{n}.sock"));
        let db_path = std::env::temp_dir().join(format!("rmail-aipol-{pid}-{n}.db"));
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

    async fn client(&self) -> AiPolicyServiceClient<Channel> {
        let channel = rmail_core::connect_uds(&self.socket).await.unwrap();
        AiPolicyServiceClient::new(channel)
    }

    /// Record one call against `account_id`, charged to `work_class`, with
    /// enough tokens on a priced model to move the dollar figure.
    async fn seed(&self, account_id: Option<i64>, work_class: WorkClass) {
        rmail_core::ai::record_call_charged(
            &self.db,
            CallRecord {
                account_id,
                message_id: None,
                request_id: None,
                model: "claude-opus-4-8".to_owned(),
                pass: Some("deep".to_owned()),
                usage: Usage {
                    input_tokens: 100_000,
                    output_tokens: 0,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 0,
                },
                redaction_level: "none".to_owned(),
                latency: Duration::from_millis(15),
                payload: b"a redacted request body",
                outcome: CallOutcome::Ok,
            },
            1.0,
            work_class,
        )
        .await
        .unwrap();
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

/// Caps with only the daily dollar dimension set.
fn daily_usd(soft: f64, hard: f64) -> BudgetCaps {
    BudgetCaps {
        daily: Some(BudgetWindowCaps {
            soft_usd: Some(soft),
            hard_usd: Some(hard),
            soft_tokens: None,
            hard_tokens: None,
        }),
        monthly: None,
    }
}

#[tokio::test]
async fn set_budget_round_trips_and_get_spend_reports_it_as_stored() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let response = client
        .set_budget(SetBudgetRequest {
            account_id: 0,
            class: BudgetClass::All.into(),
            caps: Some(daily_usd(4.0, 9.0)),
        })
        .await
        .expect("set_budget")
        .into_inner();
    assert_eq!(response.account_id, 0);
    assert_eq!(response.class(), BudgetClass::All);
    let daily = response.caps.unwrap().daily.unwrap();
    assert_eq!(daily.soft_usd, Some(4.0));
    assert_eq!(daily.hard_usd, Some(9.0));
    assert_eq!(
        daily.hard_tokens, None,
        "a cap the client did not set must come back unset, not zero — zero would forbid all \
         spending"
    );

    let spend = client
        .get_spend(GetSpendRequest { account_id: 0 })
        .await
        .expect("get_spend")
        .into_inner();
    let all = spend.all.unwrap();
    assert!(all.stored, "the operator set this budget");
    assert_eq!(all.caps.unwrap().daily.unwrap().hard_usd, Some(9.0));

    let bulk = spend.bulk.unwrap();
    assert!(
        !bulk.stored,
        "the bulk sub-budget was never set and is derived from the `all` caps"
    );
    assert_eq!(
        bulk.caps.unwrap().daily.unwrap().hard_usd,
        Some(4.5),
        "the derived bulk cap is `bulk_share` (0.5) of the stored `all` hard cap"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn get_spend_reports_real_ledger_spend_split_by_class_and_scope() {
    let server = TestServer::start().await;
    // One interactive call and one bulk call for account 7, plus an
    // unrelated call for account 8 that must not show up in account 7's
    // report but must count toward the global one.
    server.seed(Some(7), WorkClass::Interactive).await;
    server.seed(Some(7), WorkClass::Bulk).await;
    server.seed(Some(8), WorkClass::Interactive).await;
    let mut client = server.client().await;

    let account = client
        .get_spend(GetSpendRequest { account_id: 7 })
        .await
        .expect("get_spend")
        .into_inner();
    assert_eq!(account.account_id, 7);
    assert_eq!(account.day.len(), "YYYY-MM-DD".len());
    assert_eq!(account.month.len(), "YYYY-MM".len());

    let all = account.all.unwrap();
    let all_daily = all.daily.unwrap();
    assert_eq!(all_daily.tokens, 200_000, "both of account 7's calls");
    assert!(
        all_daily.usd > 0.0,
        "the model is priced, so this is not free"
    );

    let bulk_daily = account.bulk.unwrap().daily.unwrap();
    assert_eq!(
        bulk_daily.tokens, 100_000,
        "only the bulk call counts against the bulk sub-budget"
    );

    let global = client
        .get_spend(GetSpendRequest { account_id: 0 })
        .await
        .expect("get_spend")
        .into_inner();
    assert_eq!(
        global.all.unwrap().daily.unwrap().tokens,
        300_000,
        "every account's spend counts toward the global budget"
    );

    server.shutdown().await;
}

#[tokio::test]
async fn set_budget_rejects_an_unspecified_class() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // Defaulting to ALL would silently rewrite the whole scope's budget when
    // the client meant to set only the bulk sub-budget.
    let status = client
        .set_budget(SetBudgetRequest {
            account_id: 0,
            class: BudgetClass::Unspecified.into(),
            caps: Some(daily_usd(1.0, 2.0)),
        })
        .await
        .expect_err("an unspecified class must be rejected");
    assert_eq!(status.code(), Code::InvalidArgument);

    server.shutdown().await;
}

#[tokio::test]
async fn set_budget_rejects_a_soft_cap_that_can_never_fire() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let status = client
        .set_budget(SetBudgetRequest {
            account_id: 0,
            class: BudgetClass::All.into(),
            caps: Some(daily_usd(9.0, 9.0)),
        })
        .await
        .expect_err("a soft cap at the hard cap must be rejected");
    assert_eq!(status.code(), Code::InvalidArgument);

    server.shutdown().await;
}

#[tokio::test]
async fn get_spend_rejects_a_negative_account_id() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let status = client
        .get_spend(GetSpendRequest { account_id: -1 })
        .await
        .expect_err("a negative account id must be rejected");
    assert_eq!(status.code(), Code::InvalidArgument);

    server.shutdown().await;
}
