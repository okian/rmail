//! `mail daemon`'s local behaviour: which binary it would spawn, what it does
//! with a pid file, and that every path is bounded.
//!
//! Starting a *real* `rmaild` is not done here — this crate cannot name that
//! binary (`CARGO_BIN_EXE_*` covers only its own) — so the spawn paths are
//! exercised against a stub that never listens, which is the failure mode
//! worth pinning anyway: `mail daemon start` must give up and say why rather
//! than wait forever.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::{AtomicU32, Ordering};

use super::*;

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn scratch(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "rmail-cli-daemon-{tag}-{}-{n}.sock",
        std::process::id()
    ))
}

/// The pid file sits beside the socket, so two daemons on two sockets do not
/// share one.
#[test]
fn the_pid_file_is_derived_from_the_socket_path() {
    let socket = PathBuf::from("/run/rmail/rmaild.sock");
    assert_eq!(
        pid_path(&socket),
        PathBuf::from("/run/rmail/rmaild.sock.pid")
    );
    assert_ne!(
        pid_path(&PathBuf::from("/a.sock")),
        pid_path(&PathBuf::from("/b.sock"))
    );
}

/// Serializes the tests that set `$RMAILD_BIN`.
///
/// `std::env::set_var` is process-global, and nextest runs each test in its
/// own process while `cargo test` runs them as threads in one — so under
/// `cargo test` two of these would race and each would see the other's value.
/// The lock costs nothing and removes the difference between the two runners.
static ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Set `$RMAILD_BIN` for the duration of `body`, restoring it afterwards.
///
/// `body` returns a *future* and it is awaited inside the guard. Taking a
/// plain closure and restoring before the caller awaited would have put the
/// variable back before `start` ever read it — which is exactly what the first
/// version of this helper did, and the tests failed on the real `rmaild`
/// lookup instead of the stub they meant to use.
async fn with_daemon_bin<F: std::future::Future>(
    value: &str,
    body: impl FnOnce() -> F,
) -> F::Output {
    let guard = ENV.lock().await;
    let previous = std::env::var_os(DAEMON_BIN_ENV);
    std::env::set_var(DAEMON_BIN_ENV, value);
    let out = body().await;
    match previous {
        Some(value) => std::env::set_var(DAEMON_BIN_ENV, value),
        None => std::env::remove_var(DAEMON_BIN_ENV),
    }
    drop(guard);
    out
}

/// `$RMAILD_BIN` wins, so a test, a development tree and a packaged install
/// can each point at the daemon they mean.
#[tokio::test]
async fn the_env_var_selects_the_daemon_binary() {
    let chosen = with_daemon_bin("/opt/rmail/bin/rmaild", || async {
        daemon_binary().unwrap()
    })
    .await;
    assert_eq!(chosen, PathBuf::from("/opt/rmail/bin/rmaild"));
}

/// Nothing listening is an ordinary answer, not an error — `mail daemon
/// status` has to be able to say "not running" and exit 0.
#[tokio::test]
async fn probing_an_absent_socket_answers_not_running() {
    let socket = scratch("absent");
    assert!(probe(&socket).await.unwrap().is_none());
}

/// A leftover socket file with nothing behind it is the usual state after a
/// daemon was killed, and reads the same as "not running".
#[tokio::test]
async fn a_stale_socket_file_reads_as_not_running() {
    let socket = scratch("stale");
    std::fs::write(&socket, b"not a socket").unwrap();
    let answer = probe(&socket).await;
    let _ = std::fs::remove_file(&socket);
    assert!(answer.unwrap().is_none());
}

/// `stop` with nothing running says so rather than signalling a pid it
/// invented.
#[tokio::test]
async fn stopping_a_daemon_that_is_not_running_is_a_precondition_failure() {
    let socket = scratch("stop-absent");
    let error = stop(&socket, Duration::from_secs(1))
        .await
        .expect_err("nothing to stop");
    assert_eq!(ExitCode::of(&error), ExitCode::FailedPrecondition);
    assert!(format!("{error:#}").contains("not running"), "{error:#}");
}

/// A stale pid file must never cause a signal to be sent.
///
/// This is the bug the reviewer found: a pid file survives a reboot and pids
/// are recycled, so reading the number and signalling it immediately SIGTERMs
/// whatever now owns that pid — and then reports "stopped". The fix is
/// ordering: nothing answering the socket means there is nothing to stop,
/// whatever the file says.
///
/// The probe is a live process the test owns. If `stop` signalled it, it would
/// die; the assertion is that it is still running afterwards.
#[tokio::test]
async fn a_stale_pid_file_never_signals_the_process_it_names() {
    let socket = scratch("stale-pid");
    let pid_file = pid_path(&socket);
    assert!(
        !socket.exists(),
        "this test is about a pid file with no daemon behind it"
    );

    // A real, live, unrelated process — standing in for whatever inherited the
    // recycled pid.
    let mut victim = tokio::process::Command::new("/bin/sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let pid = victim.id().unwrap();
    std::fs::write(&pid_file, pid.to_string()).unwrap();

    let error = stop(&socket, Duration::from_secs(1))
        .await
        .expect_err("there is no daemon on that socket");
    assert_eq!(ExitCode::of(&error), ExitCode::FailedPrecondition);
    assert!(
        !pid_file.exists(),
        "a stale pid file must not be left behind"
    );

    // The victim is untouched: `try_wait` answers `None` for a process still
    // running, and would answer `Some(SIGTERM)` had the old code run.
    let alive = victim.try_wait().unwrap();
    let _ = victim.kill().await;
    let _ = victim.wait().await;
    assert!(
        alive.is_none(),
        "`mail daemon stop` signalled an unrelated process on the strength of a stale pid file \
         (it exited {alive:?})"
    );
}

/// A daemon that never listens must not hang `mail daemon start` — it has to
/// give up on the timeout and say which binary it started.
#[tokio::test]
async fn start_gives_up_when_the_spawned_daemon_never_listens() {
    let socket = scratch("never-listens");
    // A stub that starts fine, stays up, and never binds anything — so `start`
    // cannot short-circuit on "it exited" and has to reach its own timeout.
    // A script rather than `/bin/sleep` directly, because `daemon_binary`
    // yields a path with no arguments and `sleep` with no operand exits 1
    // (which this test would then pass for the wrong reason — it did).
    let stub = std::env::temp_dir().join(format!(
        "rmail-cli-stub-{}-{}.sh",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&stub, "#!/bin/sh\nexec sleep 30\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let started = std::time::Instant::now();
    let error = with_daemon_bin(&stub.display().to_string(), || {
        start(&socket, Duration::from_millis(300))
    })
    .await
    .expect_err("the stub never serves gRPC");
    let _ = std::fs::remove_file(&stub);

    // Clean up the child this test deliberately orphaned.
    if let Ok(recorded) = std::fs::read_to_string(pid_path(&socket)) {
        if let Ok(pid) = recorded.trim().parse::<i32>() {
            // SAFETY: as in `stop` — `kill` has no memory-safety contract.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
    }
    let _ = std::fs::remove_file(pid_path(&socket));

    assert_eq!(ExitCode::of(&error), ExitCode::FailedPrecondition);
    assert!(format!("{error:#}").contains("did not answer"), "{error:#}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "start must honour its own timeout: took {:?}",
        started.elapsed()
    );
}

/// A daemon that dies on startup is reported as such, at once, rather than
/// making the operator wait out the whole timeout for a message that says
/// nothing.
#[tokio::test]
async fn start_reports_a_daemon_that_exits_immediately() {
    let socket = scratch("exits");
    let started = std::time::Instant::now();
    // 30s of timeout the test must *not* spend: if `start` had dropped the
    // `Child` it could only report "did not answer within 30s".
    let error = with_daemon_bin("/bin/false", || start(&socket, Duration::from_secs(30)))
        .await
        .expect_err("/bin/false serves nothing");
    let _ = std::fs::remove_file(pid_path(&socket));

    assert_eq!(ExitCode::of(&error), ExitCode::FailedPrecondition);
    let rendered = format!("{error:#}");
    assert!(
        rendered.contains("exited immediately"),
        "the real reason must win over the timeout's guess: {rendered}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "an immediate exit must be noticed immediately: took {:?}",
        started.elapsed()
    );
    assert!(
        !pid_path(&socket).exists(),
        "a daemon that never ran must not leave a pid file behind"
    );
}

/// A binary that does not exist fails with the path and with the env var that
/// overrides it, rather than a bare "No such file or directory".
#[tokio::test]
async fn start_names_the_binary_and_the_override_when_it_cannot_spawn() {
    let socket = scratch("no-binary");
    let error = with_daemon_bin("/nonexistent/rmaild-does-not-exist", || {
        start(&socket, Duration::from_millis(200))
    })
    .await
    .expect_err("there is no such binary");
    let rendered = format!("{error:#}");
    assert!(rendered.contains("rmaild-does-not-exist"), "{rendered}");
    assert!(rendered.contains(DAEMON_BIN_ENV), "{rendered}");
}
