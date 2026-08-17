//! `mail daemon start | status | stop` (task 42).
//!
//! # Explicit, not automatic
//!
//! prd.md allows either "daemon auto-start" or a `FAILED_PRECONDITION`, and
//! this build chooses the second: [`crate::client::connect`] refuses with a
//! message naming `mail daemon start` when there is no socket, and only this
//! verb ever spawns a process. A `mail search` that silently forked a
//! long-lived daemon would be a surprise in exactly the environments where a
//! surprise is expensive — a CI job that now has an orphan holding a SQLite
//! file, a container whose "one process" is suddenly two, an SSH one-liner
//! that leaves something behind. The refusal is instant (the socket either
//! exists or it does not), so the other half of the requirement holds too:
//! `mail` with no daemon never hangs.
//!
//! # Which `rmaild`
//!
//! `$RMAILD_BIN`, then a sibling of the running `mail` binary, then `rmaild`
//! on `PATH`. The sibling rule is what makes a release tarball work with no
//! configuration — `scripts/package-macos.sh` ships the two binaries next to
//! each other — and the env var is what makes a development tree, a test, and
//! a packaged install all reach the daemon they mean.
//!
//! # Stopping something this process did not start
//!
//! `start` records the child's pid next to the socket (`<socket>.pid`);
//! `stop` reads it and sends `SIGTERM`, which is the signal `rmaild`'s own
//! `shutdown_signal` waits for, then waits for the socket to stop answering.
//! A daemon started some other way (a service manager, a shell) leaves no such
//! file, and `stop` says so rather than guessing at a pid — killing a process
//! identified by anything less than "I started it" is not a thing a CLI should
//! do on the strength of a socket path.
//!
//! The pid file is advisory, and the order matters: `stop` asks the *socket*
//! before it reads the file. A pid file outlives a reboot and pids are
//! recycled, so "the file says 4321" is evidence about a process that existed
//! once, not about the one running now — reading it first and signalling
//! immediately is how a CLI SIGTERMs an unrelated process and then reports
//! success. If nothing answers the socket there is nothing to stop, whatever
//! the file says, and the stale record is deleted without a signal being sent.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context as _, Result};
use clap::Subcommand;
use serde_json::json;
use tonic_health::pb::health_check_response::ServingStatus;
use tonic_health::pb::health_client::HealthClient;
use tonic_health::pb::HealthCheckRequest;

use crate::format::{self, Classified, ExitCode, OutputFormat};

/// `mail daemon <action>`.
#[derive(Debug, Subcommand)]
pub enum DaemonAction {
    /// Start rmaild in the background and wait for it to answer.
    Start {
        /// Give up after this many seconds waiting for the socket to answer.
        #[arg(long, default_value_t = 30)]
        timeout: u64,
    },
    /// Report whether rmaild is serving, and where.
    Status,
    /// Stop the rmaild this machine's `mail daemon start` started.
    Stop {
        /// Give up after this many seconds waiting for it to exit.
        #[arg(long, default_value_t = 20)]
        timeout: u64,
    },
}

/// Environment variable naming the `rmaild` binary to spawn.
pub(crate) const DAEMON_BIN_ENV: &str = "RMAILD_BIN";

/// Dispatch `mail daemon <action>`.
///
/// # Errors
/// Any failure spawning, probing or stopping the daemon.
pub async fn run(socket: &Path, action: DaemonAction) -> Result<()> {
    // These three verbs are about *this machine's* daemon: they spawn a
    // process, read a local pid file and send a local signal. `--addr` names
    // something on another host that none of that can reach, and answering
    // about the local socket while the operator asked about a remote endpoint
    // is worse than refusing.
    if let Some(addr) = crate::client::remote_addr() {
        return Err(Classified::new(
            ExitCode::Usage,
            format!(
                "`mail daemon` manages the local daemon and cannot reach --addr {addr}; use \
                 `mail api ping --addr {addr}` to ask whether a remote one is serving"
            ),
        ));
    }
    match action {
        DaemonAction::Start { timeout } => start(socket, Duration::from_secs(timeout)).await,
        DaemonAction::Status => status(socket).await,
        DaemonAction::Stop { timeout } => stop(socket, Duration::from_secs(timeout)).await,
    }
}

/// Where the pid of a `mail daemon start`ed child is recorded.
fn pid_path(socket: &Path) -> PathBuf {
    let mut path = socket.as_os_str().to_owned();
    path.push(".pid");
    PathBuf::from(path)
}

/// Whether the daemon at `socket` answers a health check.
///
/// `Ok(None)` means "nothing is listening" — an ordinary state, not a failure
/// — so that `status` can report it and `start` can decide to spawn.
async fn probe(socket: &Path) -> Result<Option<ServingStatus>> {
    if !socket.exists() {
        return Ok(None);
    }
    let Ok(channel) = rmail_core::connect_uds(socket).await else {
        // A socket file with nothing behind it: the usual leftover after a
        // daemon was killed. Indistinguishable from "not running" to a caller,
        // and treated as such.
        return Ok(None);
    };
    let mut health = HealthClient::new(channel);
    let mut request = tonic::Request::new(HealthCheckRequest {
        service: String::new(),
    });
    request.set_timeout(Duration::from_secs(5));
    match health.check(request).await {
        Ok(response) => Ok(Some(
            ServingStatus::try_from(response.into_inner().status).unwrap_or(ServingStatus::Unknown),
        )),
        Err(status) if status.code() == tonic::Code::Unavailable => Ok(None),
        Err(status) => Err(anyhow::Error::new(status).context("Health/Check RPC failed")),
    }
}

/// Resolve which `rmaild` to spawn.
fn daemon_binary() -> Result<PathBuf> {
    if let Some(explicit) = std::env::var_os(DAEMON_BIN_ENV) {
        return Ok(PathBuf::from(explicit));
    }
    if let Ok(current) = std::env::current_exe() {
        if let Some(sibling) = current.parent().map(|dir| dir.join("rmaild")) {
            if sibling.is_file() {
                return Ok(sibling);
            }
        }
    }
    // Left unqualified for the OS to resolve against `PATH`. A wrong answer
    // here surfaces as a spawn failure naming the binary, which is a better
    // error than this function inventing one.
    Ok(PathBuf::from("rmaild"))
}

async fn start(socket: &Path, timeout: Duration) -> Result<()> {
    if let Some(status) = probe(socket).await? {
        report(socket, Some(status), "already running")?;
        return Ok(());
    }

    let binary = daemon_binary()?;
    let mut child = tokio::process::Command::new(&binary)
        // Its own session, so a Ctrl-C in the shell that started it does not
        // also reach the daemon: `mail daemon start` is supposed to leave
        // something running.
        .process_group(0)
        // Inherited, deliberately: the daemon's log is its operator's
        // business, and swallowing it here would hide the reason a start
        // failed. stdin is closed because it has no console.
        .stdin(std::process::Stdio::null())
        .env(rmail_core::SOCKET_ENV, socket)
        .spawn()
        .with_context(|| {
            format!(
                "spawning {} (set {DAEMON_BIN_ENV} to name the daemon binary)",
                binary.display()
            )
        })?;
    let pid = child.id().ok_or_else(|| {
        Classified::new(
            ExitCode::Internal,
            "the spawned daemon reported no process id",
        )
    })?;
    // The `Child` is *kept*, not forgotten, for the length of the wait below:
    // it is the only thing that can tell "still starting" from "exited two
    // milliseconds ago with a config error", and without it a daemon that
    // fails instantly makes the operator wait out the whole timeout to be told
    // nothing useful. `tokio::process::Command` does not reap on drop, so
    // letting it go at the end of this function detaches the daemon exactly as
    // intended.
    let pid_file = pid_path(socket);
    if let Some(parent) = pid_file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(&pid_file, pid.to_string())
        .with_context(|| format!("recording the daemon pid in {}", pid_file.display()))?;

    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(status) = probe(socket).await? {
            report(socket, Some(status), "started")?;
            return Ok(());
        }
        // Checked before the deadline, so the real reason wins over the
        // timeout's guess.
        if let Ok(Some(exited)) = child.try_wait() {
            let _ = std::fs::remove_file(&pid_file);
            return Err(Classified::new(
                ExitCode::FailedPrecondition,
                format!(
                    "{} exited immediately ({exited}) without serving {}; its own output says why",
                    binary.display(),
                    socket.display()
                ),
            ));
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(Classified::new(
                ExitCode::FailedPrecondition,
                format!(
                    "{} was started (pid {pid}) but did not answer on {} within {}s; check its \
                     log",
                    binary.display(),
                    socket.display(),
                    timeout.as_secs()
                ),
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn status(socket: &Path) -> Result<()> {
    let status = probe(socket).await?;
    report(
        socket,
        status,
        if status.is_some() {
            "running"
        } else {
            "not running"
        },
    )?;
    // A status verb that exited non-zero for "not running" would make
    // `mail daemon status` unusable in `set -e` scripts that are *asking*.
    // The answer is in the output; the exit code says the question was
    // answered.
    Ok(())
}

async fn stop(socket: &Path, timeout: Duration) -> Result<()> {
    let pid_file = pid_path(socket);

    // **Before** the pid file is even read. A pid file outlives a reboot, and
    // pids are recycled, so "the file says 4321" is not evidence that 4321 is
    // this daemon — it is evidence about a process that existed once. Asking
    // the socket first is what turns that into a fact: nothing answering means
    // there is no daemon to stop, whatever the file says, and the only correct
    // action is to delete the stale record. Signalling first (which is what
    // this did) meant a recycled pid got a SIGTERM aimed at somebody else's
    // process, followed by "stopped" and exit 0.
    let Some(_) = probe(socket).await? else {
        let stale = pid_file.exists();
        let _ = std::fs::remove_file(&pid_file);
        return Err(Classified::new(
            ExitCode::FailedPrecondition,
            format!(
                "rmaild is not running on {}{}",
                socket.display(),
                if stale {
                    format!(" (removed the stale {})", pid_file.display())
                } else {
                    String::new()
                }
            ),
        ));
    };

    let recorded = std::fs::read_to_string(&pid_file).ok();
    let Some(pid) = recorded
        .as_deref()
        .and_then(|s| s.trim().parse::<i32>().ok())
    else {
        return Err(Classified::new(
            ExitCode::FailedPrecondition,
            format!(
                "rmaild is running on {} but was not started by `mail daemon start` (no usable \
                 {}), so this command will not guess at its process id — stop it the way it was \
                 started",
                socket.display(),
                pid_file.display()
            ),
        ));
    };

    // SIGTERM, which `rmaild::shutdown_signal` waits for, so the daemon closes
    // its listeners and flushes rather than being torn out from under SQLite.
    // SAFETY: `kill` with a positive pid and a valid signal number has no
    // memory-safety contract; the FFI call is unsafe only because libc is.
    let sent = unsafe { libc::kill(pid, libc::SIGTERM) };
    if sent != 0 {
        let error = std::io::Error::last_os_error();
        let _ = std::fs::remove_file(&pid_file);
        return Err(Classified::new(
            match error.kind() {
                // The recorded process is gone: the daemon already stopped and
                // left its pid file behind. Not a failure to report as one.
                std::io::ErrorKind::NotFound => ExitCode::FailedPrecondition,
                std::io::ErrorKind::PermissionDenied => ExitCode::PermissionDenied,
                _ => ExitCode::Failure,
            },
            format!("signalling rmaild (pid {pid}): {error}"),
        ));
    }

    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if probe(socket).await?.is_none() {
            let _ = std::fs::remove_file(&pid_file);
            report(socket, None, "stopped")?;
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(Classified::new(
                ExitCode::DeadlineExceeded,
                format!(
                    "rmaild (pid {pid}) was sent SIGTERM but was still answering on {} after {}s",
                    socket.display(),
                    timeout.as_secs()
                ),
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// One rendering for all three verbs, so `start`, `status` and `stop` cannot
/// disagree about what a running daemon looks like.
fn report(socket: &Path, status: Option<ServingStatus>, action: &str) -> Result<()> {
    let value = json!({
        "socket": socket.display().to_string(),
        "running": status.is_some(),
        "status": status.map(|s| s.as_str_name()),
        "action": action,
    });
    match format::current() {
        OutputFormat::Table => match status {
            Some(status) => println!("{action}: {} on {}", status.as_str_name(), socket.display()),
            None => println!("{action}: {}", socket.display()),
        },
        OutputFormat::Json => println!("{}", format::to_document(&value)?),
        OutputFormat::Ndjson => println!("{}", format::to_line(&value)?),
    }
    Ok(())
}

#[cfg(test)]
mod tests;
