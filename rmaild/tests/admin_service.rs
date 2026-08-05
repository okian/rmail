//! Integration test: drive `AdminService` end-to-end against an in-process
//! tonic server over a Unix domain socket. A client connected over that
//! socket is the Unix-peer-uid principal, so these calls exercise the auth
//! layer's implicit-admin path for real — the same path CLI/TUI/MCP use —
//! while `rmaild/src/auth/tests.rs` covers the bearer-token/deny paths a
//! trusted local client never takes.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use rmail_proto::v1::admin_service_client::AdminServiceClient;
use rmail_proto::v1::{ListTokensRequest, MintTokenRequest, RevokeTokenRequest};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tonic::client::Grpc;
use tonic::codec::ProstCodec;
use tonic::transport::Channel;
use tonic::{Code, Request};

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
        let socket = PathBuf::from("/tmp").join(format!("rmail-admin-{pid}-{n}.sock"));
        let db_path = std::env::temp_dir().join(format!("rmail-admin-{pid}-{n}.db"));
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

    async fn client(&self) -> AdminServiceClient<Channel> {
        let channel = rmail_core::connect_uds(&self.socket).await.unwrap();
        AdminServiceClient::new(channel)
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
async fn mint_list_revoke_round_trip_over_the_trusted_socket() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    // Mint: the Unix-socket peer is implicit admin, so no token is needed to
    // call an admin-scoped RPC.
    let minted = client
        .mint_token(MintTokenRequest {
            name: "ci".to_owned(),
            scopes: vec!["mail.read".to_owned(), "ai.invoke".to_owned()],
            ttl_secs: None,
        })
        .await
        .expect("mint")
        .into_inner();
    assert!(minted.token.starts_with("rmail_tok_"));
    assert_eq!(minted.name, "ci");
    assert_eq!(minted.scopes, vec!["mail.read", "ai.invoke"]);
    assert!(minted.expires_at.is_none());

    // List: metadata only, never the secret or its hash.
    let listed = client
        .list_tokens(ListTokensRequest {})
        .await
        .expect("list")
        .into_inner();
    assert_eq!(listed.tokens.len(), 1);
    assert_eq!(listed.tokens[0].id, minted.id);
    assert!(!listed.tokens[0].revoked);

    // Revoke.
    let revoked = client
        .revoke_token(RevokeTokenRequest { id: minted.id })
        .await
        .expect("revoke")
        .into_inner();
    assert!(revoked.revoked);

    let listed = client
        .list_tokens(ListTokensRequest {})
        .await
        .expect("list after revoke")
        .into_inner();
    assert!(listed.tokens[0].revoked);

    server.shutdown().await;
}

#[tokio::test]
async fn mint_rejects_an_unparseable_scope() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let status = client
        .mint_token(MintTokenRequest {
            name: "bad".to_owned(),
            scopes: vec!["not-a-real-scope".to_owned()],
            ttl_secs: None,
        })
        .await
        .expect_err("bad scope should be rejected");
    assert_eq!(status.code(), Code::InvalidArgument);

    server.shutdown().await;
}

#[tokio::test]
async fn revoke_of_an_unknown_id_is_not_found() {
    let server = TestServer::start().await;
    let mut client = server.client().await;

    let status = client
        .revoke_token(RevokeTokenRequest { id: 424_242 })
        .await
        .expect_err("unknown id should be not found");
    assert_eq!(status.code(), Code::NotFound);

    server.shutdown().await;
}

#[tokio::test]
async fn a_health_check_over_the_same_socket_needs_no_admin_trust() {
    // Sanity check that the auth layer's `Public` rows are not accidentally
    // shadowed by adding `AdminService`/its layer to the server.
    let server = TestServer::start().await;
    let channel = rmail_core::connect_uds(&server.socket).await.unwrap();
    let mut health = tonic_health::pb::health_client::HealthClient::new(channel);
    let response = health
        .check(tonic_health::pb::HealthCheckRequest {
            service: String::new(),
        })
        .await
        .expect("health check");
    assert_eq!(
        response.into_inner().status(),
        tonic_health::pb::health_check_response::ServingStatus::Serving
    );

    server.shutdown().await;
}

#[tokio::test]
async fn the_auth_layer_is_actually_wired_into_the_running_server() {
    // The decisive proof that `AuthLayer` is installed on the real server —
    // not just exercised in isolation by `rmaild/src/auth/tests.rs`. This
    // socket's peer is the trusted local admin, so if the auth layer were
    // missing (or the method table lookup skipped), routing an unregistered
    // path would fall through to tonic's own catch-all, which answers
    // UNIMPLEMENTED. `authorize()` checks the method table *before* any
    // credential is consulted, so it denies this with PERMISSION_DENIED even
    // for a trusted admin peer — a different, specific code that only the
    // auth layer (not tonic's router) produces.
    let server = TestServer::start().await;
    let channel = rmail_core::connect_uds(&server.socket).await.unwrap();

    let mut grpc = Grpc::new(channel);
    grpc.ready().await.expect("channel ready");
    let path = http::uri::PathAndQuery::from_static("/rmail.v1.NotAService/Nope");
    // The message type is irrelevant — the auth layer denies this before the
    // request body is ever decoded. Reusing an existing proto message avoids
    // needing a throwaway type.
    let codec = ProstCodec::<ListTokensRequest, ListTokensRequest>::default();
    let status = grpc
        .unary(Request::new(ListTokensRequest {}), path, codec)
        .await
        .expect_err("an unregistered method must be denied, not routed");
    assert_eq!(
        status.code(),
        Code::PermissionDenied,
        "got {status:?} — UNIMPLEMENTED here would mean the auth layer is not \
         actually wrapping the server"
    );

    server.shutdown().await;
}
