//! Integration tests: start the in-process gRPC server over a Unix domain
//! socket and exercise the health and reflection services, including an
//! error path.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tonic::transport::Channel;
use tonic_health::pb::health_check_response::ServingStatus;
use tonic_health::pb::health_client::HealthClient;
use tonic_health::pb::HealthCheckRequest;
use tonic_reflection::pb::v1::server_reflection_client::ServerReflectionClient;
use tonic_reflection::pb::v1::server_reflection_request::MessageRequest;
use tonic_reflection::pb::v1::server_reflection_response::MessageResponse;
use tonic_reflection::pb::v1::ServerReflectionRequest;

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A short, unique socket path under `/tmp` (kept well under the macOS
/// 104-byte sockaddr_un limit).
fn unique_socket_path() -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    PathBuf::from("/tmp").join(format!("rmail-test-{pid}-{n}.sock"))
}

/// A running in-process server plus the handles to shut it down and join it.
struct TestServer {
    socket: PathBuf,
    shutdown: oneshot::Sender<()>,
    handle: JoinHandle<Result<(), rmaild::ServeError>>,
}

impl TestServer {
    /// Spawn the server and wait until it answers a health check.
    async fn start() -> Self {
        let socket = unique_socket_path();
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let server_socket = socket.clone();
        let handle = tokio::spawn(async move {
            rmaild::serve_uds(&server_socket, async move {
                let _ = shutdown_rx.await;
            })
            .await
        });

        // Readiness = the server actually answers, not just that the file exists.
        let mut ready = false;
        for _ in 0..200 {
            if let Ok(channel) = rmail_core::connect_uds(&socket).await {
                let mut client = HealthClient::new(channel);
                if client
                    .check(HealthCheckRequest {
                        service: String::new(),
                    })
                    .await
                    .is_ok()
                {
                    ready = true;
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(ready, "server {} never became ready", socket.display());
        Self {
            socket,
            shutdown: shutdown_tx,
            handle,
        }
    }

    async fn channel(&self) -> Channel {
        rmail_core::connect_uds(&self.socket)
            .await
            .expect("connect to rmaild over uds")
    }

    async fn shutdown(self) {
        self.shutdown.send(()).expect("send shutdown");
        let result = self.handle.await.expect("join server task");
        result.expect("server ran and shut down cleanly");
        assert!(
            !self.socket.exists(),
            "socket {} should be unlinked on shutdown",
            self.socket.display()
        );
    }
}

#[tokio::test]
async fn health_check_reports_serving() {
    let server = TestServer::start().await;

    let mut client = HealthClient::new(server.channel().await);
    let response = client
        .check(HealthCheckRequest {
            service: String::new(),
        })
        .await
        .expect("health check rpc");
    assert_eq!(response.into_inner().status(), ServingStatus::Serving);

    server.shutdown().await;
}

#[tokio::test]
async fn health_check_unknown_service_is_not_found() {
    let server = TestServer::start().await;

    let mut client = HealthClient::new(server.channel().await);
    let status = client
        .check(HealthCheckRequest {
            service: "rmail.v1.DoesNotExist".to_owned(),
        })
        .await
        .expect_err("unknown service should be rejected");
    assert_eq!(status.code(), tonic::Code::NotFound);

    server.shutdown().await;
}

#[tokio::test]
async fn reflection_service_responds() {
    let server = TestServer::start().await;

    let mut client = ServerReflectionClient::new(server.channel().await);
    let request = ServerReflectionRequest {
        host: String::new(),
        message_request: Some(MessageRequest::ListServices(String::new())),
    };
    let mut inbound = client
        .server_reflection_info(tokio_stream::iter(vec![request]))
        .await
        .expect("reflection rpc")
        .into_inner();

    let response = inbound
        .message()
        .await
        .expect("reflection stream")
        .expect("reflection response present");
    // The reflection endpoint must answer with a list-services response (rather
    // than an error), proving the descriptor set loaded and reflection is wired.
    assert!(
        matches!(
            response.message_response,
            Some(MessageResponse::ListServicesResponse(_))
        ),
        "expected a ListServices response, got {:?}",
        response.message_response
    );

    server.shutdown().await;
}
