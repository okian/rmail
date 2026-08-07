//! Integration test: drive the compiled `mail hook add/list/test`
//! subcommands end-to-end. `add` is exercised directly against a temp
//! config file (no daemon involved — see `hook_cli`'s own module docs on
//! why `add` is a local file edit, not an RPC); `list`/`test` run against a
//! real in-process daemon, the same harness `token.rs` uses.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use rmail_core::config::{HookConfig, HookEvent, HooksConfig};
use tokio::process::Command;
use tokio::sync::oneshot;

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn unique_path(label: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    std::env::temp_dir().join(format!("rmail-cli-hook-{label}-{pid}-{n}"))
}

// ---------------------------------------------------------------------------
// `mail hook add`: a local config-file edit, no daemon involved
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hook_add_appends_a_valid_hook_that_config_load_can_read_back() {
    let config_path = unique_path("add.toml");
    let _ = std::fs::remove_file(&config_path);

    let output = Command::new(env!("CARGO_BIN_EXE_mail"))
        .args([
            "hook",
            "add",
            "on_new_message",
            "--name",
            "notify-new-mail",
            "--timeout",
            "15s",
            "--",
            "/usr/local/bin/notify",
            "--loud",
        ])
        .env(rmail_core::transport::CONFIG_ENV, &config_path)
        .output()
        .await
        .expect("run mail hook add");
    assert!(
        output.status.success(),
        "mail hook add failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let cfg = rmail_core::Config::load(&config_path).expect("the written config must parse");
    assert_eq!(cfg.hooks.hooks.len(), 1);
    let hook = &cfg.hooks.hooks[0];
    assert_eq!(hook.name, "notify-new-mail");
    assert_eq!(hook.event, HookEvent::OnNewMessage);
    assert_eq!(hook.command, "/usr/local/bin/notify");
    assert_eq!(hook.args, vec!["--loud".to_owned()]);
    assert!(hook.enabled);
    assert_eq!(
        hook.timeout.map(|t| t.as_duration()),
        Some(Duration::from_secs(15))
    );

    let _ = std::fs::remove_file(&config_path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hook_add_twice_appends_both_and_preserves_the_first() {
    let config_path = unique_path("append.toml");
    let _ = std::fs::remove_file(&config_path);

    for name in ["first-hook", "second-hook"] {
        let output = Command::new(env!("CARGO_BIN_EXE_mail"))
            .args(["hook", "add", "on_move", "--name", name, "--", "/bin/true"])
            .env(rmail_core::transport::CONFIG_ENV, &config_path)
            .output()
            .await
            .expect("run mail hook add");
        assert!(output.status.success(), "adding {name} failed");
    }

    let cfg = rmail_core::Config::load(&config_path).expect("parses");
    let names: Vec<&str> = cfg.hooks.hooks.iter().map(|h| h.name.as_str()).collect();
    assert_eq!(names, vec!["first-hook", "second-hook"]);

    let _ = std::fs::remove_file(&config_path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hook_add_rejects_a_duplicate_name_and_writes_nothing() {
    let config_path = unique_path("dup.toml");
    let _ = std::fs::remove_file(&config_path);

    let first = Command::new(env!("CARGO_BIN_EXE_mail"))
        .args(["hook", "add", "on_move", "--name", "dup", "--", "/bin/true"])
        .env(rmail_core::transport::CONFIG_ENV, &config_path)
        .output()
        .await
        .expect("run mail hook add");
    assert!(first.status.success());

    let second = Command::new(env!("CARGO_BIN_EXE_mail"))
        .args([
            "hook",
            "add",
            "on_label",
            "--name",
            "dup",
            "--",
            "/bin/false",
        ])
        .env(rmail_core::transport::CONFIG_ENV, &config_path)
        .output()
        .await
        .expect("run mail hook add");
    assert!(
        !second.status.success(),
        "a duplicate hook name must be rejected"
    );

    let cfg = rmail_core::Config::load(&config_path).expect("parses");
    assert_eq!(
        cfg.hooks.hooks.len(),
        1,
        "the rejected duplicate must not have been written"
    );
    assert_eq!(cfg.hooks.hooks[0].command, "/bin/true");

    let _ = std::fs::remove_file(&config_path);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hook_add_fails_loudly_rather_than_silently_when_the_config_cannot_be_read() {
    // A directory at the config path is unreadable as a file (`EISDIR`) --
    // standing in for any I/O error that is not "the file does not exist
    // yet." Before treating only `NotFound` as "start from empty," this
    // silently discarded the read error and `mail hook add` would write a
    // brand-new one-hook config over whatever a misconfigured
    // `RMAIL_CONFIG` was actually pointing at.
    let config_path = unique_path("unreadable-dir");
    let _ = std::fs::remove_dir_all(&config_path);
    std::fs::create_dir_all(&config_path).expect("create a directory at the config path");

    let output = Command::new(env!("CARGO_BIN_EXE_mail"))
        .args(["hook", "add", "on_move", "--name", "x", "--", "/bin/true"])
        .env(rmail_core::transport::CONFIG_ENV, &config_path)
        .output()
        .await
        .expect("run mail hook add");

    assert!(
        !output.status.success(),
        "an unreadable config path must fail loudly, not silently succeed"
    );
    assert!(
        !String::from_utf8_lossy(&output.stderr).is_empty(),
        "the failure must be reported to the operator"
    );

    let _ = std::fs::remove_dir_all(&config_path);
}

// ---------------------------------------------------------------------------
// `mail hook list` / `mail hook test`: real RPCs against a running daemon
// ---------------------------------------------------------------------------

struct Daemon {
    socket: PathBuf,
    db_path: PathBuf,
    shutdown: oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<Result<(), rmaild::ServeError>>,
}

impl Daemon {
    async fn start(hooks: Vec<HookConfig>) -> Self {
        let socket = unique_path("sock");
        let db_path = unique_path("db");
        let db = rmail_core::Database::open(&db_path).expect("open db");

        let mut config = rmail_core::Config::default();
        config.index.semantic.enabled = false;
        config.ai.enabled = false;
        config.hooks = HooksConfig {
            hooks,
            ..HooksConfig::default()
        };

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let server_socket = socket.clone();
        let handle = tokio::spawn(async move {
            rmaild::serve_uds_with_config(&server_socket, db, config, async move {
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
        let _ = self.shutdown.send(());
        let _ = tokio::time::timeout(Duration::from_secs(10), self.handle).await;
        for suffix in ["", "-wal", "-shm"] {
            let _ =
                std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.db_path.display())));
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hook_list_and_test_round_trip_against_a_running_daemon() {
    let marker = unique_path("marker");
    let _ = std::fs::remove_file(&marker);

    let daemon = Daemon::start(vec![HookConfig {
        name: "echo".to_owned(),
        event: HookEvent::OnNewMessage,
        command: "/bin/sh".to_owned(),
        args: vec!["-c".to_owned(), format!("touch {}", marker.display())],
        enabled: true,
        timeout: None,
    }])
    .await;

    // List.
    let output = Command::new(env!("CARGO_BIN_EXE_mail"))
        .args(["hook", "list"])
        .env(rmail_core::SOCKET_ENV, &daemon.socket)
        .output()
        .await
        .expect("run mail hook list");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "mail hook list failed: {stdout}");
    assert!(
        stdout.contains("echo"),
        "expected the hook name in: {stdout}"
    );
    assert!(
        stdout.contains("enabled"),
        "expected the enabled status in: {stdout}"
    );

    // Test.
    let output = Command::new(env!("CARGO_BIN_EXE_mail"))
        .args(["hook", "test", "echo"])
        .env(rmail_core::SOCKET_ENV, &daemon.socket)
        .output()
        .await
        .expect("run mail hook test");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "mail hook test failed: {stdout}");
    assert!(
        stdout.contains("exit code 0"),
        "expected a successful run in: {stdout}"
    );
    assert!(
        marker.exists(),
        "mail hook test must have actually run the configured command"
    );

    let _ = std::fs::remove_file(&marker);
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hook_test_on_an_unknown_name_fails_with_a_clear_error() {
    let daemon = Daemon::start(vec![]).await;

    let output = Command::new(env!("CARGO_BIN_EXE_mail"))
        .args(["hook", "test", "does-not-exist"])
        .env(rmail_core::SOCKET_ENV, &daemon.socket)
        .output()
        .await
        .expect("run mail hook test");
    assert!(!output.status.success());
    assert!(!String::from_utf8_lossy(&output.stderr).is_empty());

    daemon.stop().await;
}
