//! Integration test: drive the compiled `mail` binary end-to-end against an
//! in-process daemon, covering both the success round-trip and the
//! connection-failure error path.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tokio::process::Command;
use tokio::sync::oneshot;

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn unique_socket_path(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    PathBuf::from("/tmp").join(format!("rmail-cli-{tag}-{pid}-{n}.sock"))
}

/// `mail ping` against a running daemon prints `Serving` and exits 0.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ping_round_trips_health_check() {
    let socket = unique_socket_path("ok");
    let db_path = std::env::temp_dir().join(format!(
        "rmail-cli-ping-{}-{}.db",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let db = rmail_core::Database::open(&db_path).expect("open db");
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server_socket = socket.clone();
    let server = tokio::spawn(async move {
        rmaild::serve_uds(&server_socket, db, async move {
            let _ = shutdown_rx.await;
        })
        .await
    });

    // Wait for the daemon to answer.
    let mut ready = false;
    for _ in 0..200 {
        if rmail_core::connect_uds(&socket).await.is_ok() {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(ready, "daemon never became ready");

    let output = Command::new(env!("CARGO_BIN_EXE_mail"))
        .arg("ping")
        .env(rmail_core::SOCKET_ENV, &socket)
        .output()
        .await
        .expect("run mail ping");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "mail ping failed: status={:?} stdout={stdout} stderr={}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("Serving"),
        "expected 'Serving' in output, got: {stdout}"
    );

    shutdown_tx.send(()).expect("send shutdown");
    server
        .await
        .expect("join server")
        .expect("server ran cleanly");

    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", db_path.display())));
    }
}

/// `mail ping` against a missing socket exits non-zero (error path).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ping_fails_when_daemon_absent() {
    let socket = unique_socket_path("absent");
    assert!(!socket.exists());

    let output = Command::new(env!("CARGO_BIN_EXE_mail"))
        .arg("ping")
        .env(rmail_core::SOCKET_ENV, &socket)
        .output()
        .await
        .expect("run mail ping");

    assert!(
        !output.status.success(),
        "mail ping should fail when the daemon is absent"
    );
}
