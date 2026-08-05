//! Integration test: drive the compiled `mail token create/list/revoke`
//! subcommands end-to-end against an in-process daemon.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use tokio::process::Command;
use tokio::sync::oneshot;

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn unique_socket_path() -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    PathBuf::from("/tmp").join(format!("rmail-cli-token-{pid}-{n}.sock"))
}

struct Daemon {
    socket: PathBuf,
    db_path: PathBuf,
    shutdown: oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<Result<(), rmaild::ServeError>>,
}

impl Daemon {
    async fn start() -> Self {
        let socket = unique_socket_path();
        let db_path = std::env::temp_dir().join(format!(
            "rmail-cli-token-{}-{}.db",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let db = rmail_core::Database::open(&db_path).expect("open db");
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
        assert!(ready, "daemon never became ready");

        Self {
            socket,
            db_path,
            shutdown: shutdown_tx,
            handle,
        }
    }

    async fn stop(self) {
        self.shutdown.send(()).expect("send shutdown");
        self.handle
            .await
            .expect("join server")
            .expect("server ran cleanly");
        for suffix in ["", "-wal", "-shm"] {
            let _ =
                std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.db_path.display())));
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn token_create_list_revoke_round_trip() {
    let daemon = Daemon::start().await;

    // Create.
    let output = Command::new(env!("CARGO_BIN_EXE_mail"))
        .args([
            "token",
            "create",
            "--name",
            "ci",
            "--scope",
            "mail.read,ai.invoke",
        ])
        .env(rmail_core::SOCKET_ENV, &daemon.socket)
        .output()
        .await
        .expect("run mail token create");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "mail token create failed: stdout={stdout} stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("rmail_tok_"),
        "expected a bearer token in: {stdout}"
    );

    // Extract the id from `id:      <n>`.
    let id: i64 = stdout
        .lines()
        .find_map(|line| line.strip_prefix("id:"))
        .map(str::trim)
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| panic!("could not find token id in: {stdout}"));

    // List.
    let output = Command::new(env!("CARGO_BIN_EXE_mail"))
        .arg("token")
        .arg("list")
        .env(rmail_core::SOCKET_ENV, &daemon.socket)
        .output()
        .await
        .expect("run mail token list");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "mail token list failed: {stdout}");
    assert!(
        stdout.contains("ci"),
        "expected the minted token in: {stdout}"
    );
    assert!(
        stdout.contains("active"),
        "expected active status in: {stdout}"
    );

    // Revoke.
    let output = Command::new(env!("CARGO_BIN_EXE_mail"))
        .args(["token", "revoke", &id.to_string()])
        .env(rmail_core::SOCKET_ENV, &daemon.socket)
        .output()
        .await
        .expect("run mail token revoke");
    assert!(
        output.status.success(),
        "mail token revoke failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // List again shows it revoked.
    let output = Command::new(env!("CARGO_BIN_EXE_mail"))
        .arg("token")
        .arg("list")
        .env(rmail_core::SOCKET_ENV, &daemon.socket)
        .output()
        .await
        .expect("run mail token list");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("revoked"),
        "expected revoked status in: {stdout}"
    );

    daemon.stop().await;
}
