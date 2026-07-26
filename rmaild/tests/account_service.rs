//! Integration test: drive `AccountService` end-to-end against an in-process
//! tonic server over a Unix domain socket, covering CRUD, the error/`Status`
//! paths, credential-reference round-tripping, and `TestConnection`'s
//! precondition/unreachable failure mappings.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use rmail_proto::v1::account_service_client::AccountServiceClient;
use rmail_proto::v1::{
    credential_ref, CreateAccountRequest, CredentialRef, DeleteAccountRequest, GetAccountRequest,
    ListAccountsRequest, TestConnectionRequest,
};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tonic::transport::Channel;
use tonic::Code;

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct TestServer {
    socket: PathBuf,
    db_path: PathBuf,
    shutdown: oneshot::Sender<()>,
    handle: JoinHandle<Result<(), rmaild::ServeError>>,
}

impl TestServer {
    async fn start() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let socket = PathBuf::from("/tmp").join(format!("rmail-acct-{pid}-{n}.sock"));
        let db_path = std::env::temp_dir().join(format!("rmail-acct-{pid}-{n}.db"));
        let db = rmail_core::Database::open(&db_path).unwrap();

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let server_socket = socket.clone();
        let handle = tokio::spawn(async move {
            rmaild::serve_uds(&server_socket, db, async move {
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
            shutdown: shutdown_tx,
            handle,
        }
    }

    async fn client(&self) -> AccountServiceClient<Channel> {
        let channel = rmail_core::connect_uds(&self.socket).await.unwrap();
        AccountServiceClient::new(channel)
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
async fn account_service_crud() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // Create.
    let created = client
        .create(CreateAccountRequest {
            name: "Personal".to_owned(),
            imap_server: Some("imap.fastmail.com".to_owned()),
            imap_port: Some(993),
            username: Some("me@example.com".to_owned()),
            credential: Some(CredentialRef {
                source: Some(credential_ref::Source::PasswordEnv(
                    "FASTMAIL_PW".to_owned(),
                )),
            }),
            ..Default::default()
        })
        .await
        .expect("create")
        .into_inner();
    assert_eq!(created.name, "Personal");
    assert_eq!(created.imap_port, Some(993));
    // Credential is stored as a reference, never the secret.
    assert_eq!(created.credential_kind, "env");
    assert_eq!(created.credential_ref.as_deref(), Some("FASTMAIL_PW"));

    // Get.
    let fetched = client
        .get(GetAccountRequest { id: created.id })
        .await
        .expect("get")
        .into_inner();
    assert_eq!(fetched.id, created.id);
    assert_eq!(fetched.username.as_deref(), Some("me@example.com"));

    // List.
    let listed = client
        .list(ListAccountsRequest {})
        .await
        .expect("list")
        .into_inner();
    assert_eq!(listed.accounts.len(), 1);

    // Delete.
    let deleted = client
        .delete(DeleteAccountRequest { id: created.id })
        .await
        .expect("delete")
        .into_inner();
    assert!(deleted.deleted);

    // Get after delete -> NOT_FOUND.
    let status = client
        .get(GetAccountRequest { id: created.id })
        .await
        .expect_err("get after delete");
    assert_eq!(status.code(), Code::NotFound);

    server.shutdown().await;
}

#[tokio::test]
async fn account_service_duplicate_name_already_exists() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let req = || CreateAccountRequest {
        name: "Work".to_owned(),
        ..Default::default()
    };
    client.create(req()).await.expect("first create");
    let status = client.create(req()).await.expect_err("duplicate create");
    assert_eq!(status.code(), Code::AlreadyExists);

    server.shutdown().await;
}

#[tokio::test]
async fn account_service_empty_name_invalid_argument() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let status = client
        .create(CreateAccountRequest {
            name: "  ".to_owned(),
            ..Default::default()
        })
        .await
        .expect_err("empty name");
    assert_eq!(status.code(), Code::InvalidArgument);

    server.shutdown().await;
}

#[tokio::test]
async fn account_service_out_of_range_port_is_invalid_argument() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let status = client
        .create(CreateAccountRequest {
            name: "BadPort".to_owned(),
            imap_port: Some(70_000), // > u16::MAX
            ..Default::default()
        })
        .await
        .expect_err("out-of-range port");
    assert_eq!(status.code(), Code::InvalidArgument);

    server.shutdown().await;
}

#[tokio::test]
async fn account_service_test_connection_requires_config() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // An account with no IMAP server can't be tested -> FAILED_PRECONDITION
    // (the RPC is wired; it just needs configuration).
    let created = client
        .create(CreateAccountRequest {
            name: "Personal".to_owned(),
            ..Default::default()
        })
        .await
        .expect("create")
        .into_inner();

    let status = client
        .test_connection(TestConnectionRequest { id: created.id })
        .await
        .expect_err("test_connection needs an IMAP server");
    assert_eq!(status.code(), Code::FailedPrecondition);

    server.shutdown().await;
}

#[tokio::test]
async fn account_service_test_connection_unreachable_is_unavailable() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // A fully-configured account pointed at a refused port exercises the whole
    // path (credential resolve -> TLS connect) and must map to UNAVAILABLE.
    let created = client
        .create(CreateAccountRequest {
            name: "Unreachable".to_owned(),
            imap_server: Some("127.0.0.1".to_owned()),
            imap_port: Some(1),
            username: Some("u".to_owned()),
            credential: Some(CredentialRef {
                source: Some(credential_ref::Source::PasswordCommand(
                    "printf pw".to_owned(),
                )),
            }),
            ..Default::default()
        })
        .await
        .expect("create")
        .into_inner();

    let status = client
        .test_connection(TestConnectionRequest { id: created.id })
        .await
        .expect_err("unreachable server");
    assert_eq!(status.code(), Code::Unavailable);

    server.shutdown().await;
}
