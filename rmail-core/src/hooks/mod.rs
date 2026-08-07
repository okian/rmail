//! The event-hook dispatcher (task 67, prd.md #48 "Event Hook Dispatcher"):
//! config-driven shell commands that fire on mail events.
//!
//! [`HookDispatcher`] is a *consumer* of the durable event log built in task
//! 14 (`crate::events`) — it does not build a second event path. Its shape
//! mirrors `crate::ai::dispatch::AiDispatchLoop` deliberately: a daemon-side
//! loop that re-reads [`crate::events::EventLog::since`] from its own
//! in-memory cursor on every tick rather than holding a live
//! [`crate::events::EventLog::subscribe`] subscription open, for the exact
//! reason that module's own docs give — nobody is waiting synchronously on a
//! hook firing, so trading a few seconds of worst-case latency (bounded by
//! the tick interval) against the lag-recovery machinery a live subscription
//! needs is the right call, and the durability guarantee is identical
//! either way: `since`'s cursor always advances by what it *scanned*, not
//! merely what matched.
//!
//! # The cursor starts at "now," never at the beginning of retention
//!
//! This is where this module's shape *diverges* from `AiDispatchLoop`'s,
//! and deliberately so. `AiDispatchLoop` starts its cursor at `0` on every
//! restart and accepts re-scanning the whole retention window, because
//! `AiQueue::enqueue`'s `(message_id, pass)` dedup makes that replay a
//! literal no-op for anything already processed. Hooks have no dedup —
//! there is no natural idempotency key for "ran an operator's shell
//! command" — so starting at `0` would mean every daemon restart re-fires
//! every enabled hook once per matching event in the last 7 days (the
//! default retention window): thousands of real process spawns and
//! thousands of duplicated side effects, not a no-op.
//! [`HookDispatcher::spawn`] instead seeds the cursor from
//! [`crate::events::EventLog::latest_seq`] — the log's current head —
//! before it returns, so a restart only ever fires hooks for events from
//! that moment forward, never for history. Seeding *at spawn* rather than
//! on the first tick is what makes "that moment" mean boot instead of
//! whenever the runtime first polled the tick task; see that method's own
//! docs for the startup-window events the lazier version dropped.
//! [`HookDispatcher::tick`] retains the same lazy seed as a fallback, which
//! is also what direct-`tick` callers (the tests) rely on. The same reasoning governs the
//! retention-gap recovery below: it resets to the *current* head, not to
//! `0`, for the identical reason.
//!
//! # The event JSON goes on stdin only — never into the command
//!
//! This is the one invariant every other design choice in this module
//! serves. A hook's `command`/`args` are operator-configured, trusted
//! strings from the local TOML config file. The event JSON [`run_hook`]
//! pipes to a hook's stdin, by contrast, is data — durably logged, but
//! ultimately derived from mail an *attacker* controls the content of (a
//! subject line, a header, eventually a rule's matched text). If that JSON
//! were ever formatted into the command string and handed to a shell —
//! `format!("{command} '{event_json}'")` executed via `sh -c` — a message
//! whose subject read `"; rm -rf ~ #` would not be inert text a hook script
//! chose to ignore; it would be a second command the attacker wrote,
//! executed with this process's privileges the moment the message synced.
//!
//! [`run_hook`] structurally cannot do that: it spawns `command`/`args`
//! exactly as configured via [`tokio::process::Command::new`]/`args` (never
//! through a shell of *this* module's choosing — see
//! [`crate::config::HookConfig::command`]'s own docs for how an operator who
//! wants shell features opts into one explicitly), and the event JSON is
//! written to the child's stdin handle and nowhere else. There is no code
//! path in this module that concatenates the payload into an argv entry or
//! a command string. `hooks::tests::run_hook_pipes_event_json_on_stdin_only_never_into_argv_or_command`
//! is the regression proof: a payload containing shell metacharacters lands
//! verbatim in the child's stdin and never executes anything.
//!
//! # A hook exceeding its timeout is killed, not abandoned — the whole group
//!
//! Dropping a future that was `.await`ing a child process does not, by
//! itself, terminate the OS process — an abandoned `Child` would keep
//! running as an orphan, its stdout/stderr pipes held open, and (once the
//! kernel's per-user process/fd limits are considered) is a resource leak
//! under any hook that reliably outlives its timeout. On a timeout (or
//! caller cancellation), [`run_hook`] instead calls [`kill_and_abort`],
//! which sends `SIGKILL` and then calls
//! [`tokio::process::Child::wait`] to reap it — a genuine termination a
//! test can observe from *outside* this process (see
//! `hooks::tests::run_hook_exceeding_timeout_is_actually_killed_not_merely_abandoned`,
//! which polls `kill -0 <pid>` after the timeout fires and asserts the
//! process is gone).
//!
//! Killing only the tracked pid is not enough: the documented way an
//! operator opts into shell features is `command = "/bin/sh"`,
//! `args = ["-c", "cmd1; cmd2"]`, and killing the shell does not kill
//! `cmd2` once it has been forked. [`run_hook`] spawns every hook as its
//! own process-group leader (`Command::process_group(0)`) and
//! [`kill_process_group`] signals the negative pid — the whole group per
//! POSIX `kill(2)` — so a timeout/cancellation reaches children the hook
//! itself spawned, not just the one process this module started directly.
//!
//! # Writing stdin, draining stdout/stderr, and waiting all run concurrently
//!
//! A pipe's kernel buffer is a few tens of kilobytes (64 KiB is typical on
//! Linux). A hook that writes more than that to stdout before reading
//! anything from stdin will block on `write()` until something drains it;
//! if this module wrote the whole event payload to stdin *before* starting
//! to read stdout — the naive, sequential-looking implementation — a hook
//! that both reads all of stdin and writes more than a pipe-buffer's worth
//! of stdout would deadlock: the hook blocked writing to a full stdout pipe
//! nobody is draining, and this dispatcher blocked writing to a stdin pipe
//! the hook is not yet reading because it is stuck on that write. [`run_hook`]
//! spawns the stdin writer and both stdout/stderr drainers as independent
//! tasks and awaits all three plus [`tokio::process::Child::wait`]
//! concurrently (`tokio::select!` racing that join against the timeout/
//! cancellation), so none of the four can block on another.
//! `hooks::tests::run_hook_large_stdout_does_not_deadlock` is the regression
//! proof: a hook writing 1 MiB to stdout, run against a small
//! `max_output_bytes` cap, still returns well inside a generous test
//! deadline. [`read_capped`] keeps draining past the cap rather than
//! stopping once retained output is full, for the identical reason — a
//! stopped reader is a pipe nobody is emptying. The drained bytes land in a
//! shared buffer (not a value only the task's own `JoinHandle` could
//! return), so a timeout/cancellation that aborts the drain tasks still
//! reports whatever was captured up to that point — the output an operator
//! most needs when diagnosing exactly the run that hung.
//!
//! # A failing or missing hook does not stall the event consumer
//!
//! [`HookDispatcher::tick`] never propagates a single hook's failure —
//! [`run_hook`] cannot itself error (a spawn failure is reported as an
//! outcome with no exit code, not a `Result::Err`), and
//! [`HookDispatcher::tick`] logs a non-zero exit or timeout via `tracing`
//! and moves on to the next matched hook. Only a failure reading the event
//! log itself can fail a tick, and that follows `AiDispatchLoop`'s own
//! "reset and retry" recovery for a retention gap rather than wedging
//! forever on the first quiet stretch (see [`HookDispatcher::tick`]'s own
//! docs).
//!
//! # One tick's matches are capped
//!
//! A single burst — a large IMAP sync landing thousands of messages, a
//! quiet mailbox catching up after a long gap — could otherwise match
//! thousands of hooks in one `since` page before the `Semaphore` gets a
//! chance to bound anything, ballooning one tick's memory (every match
//! carries its own serialized event payload) and the number of tasks handed
//! to one `JoinSet`. [`HookDispatcher::tick`] stops matching once it has at
//! least [`HookDispatcher::max_batch`] matches queued — checked only at an
//! *event* boundary, never partway through matching one event against every
//! hook, since this dispatcher never revisits an event once its cursor has
//! passed it; stopping mid-event would permanently drop whichever hooks for
//! that event hadn't been matched yet, not merely defer them to the next
//! tick. The cursor lands on the last event actually finished (not at the
//! end of the page being scanned), so the next tick picks up immediately
//! after it — mirroring `AiDispatchLoop`'s own `lease_limit` bound on how
//! much one dispatch cycle takes on at once.

use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::config::{HookEvent, HooksConfig};
use crate::error::{Error, ErrorReason};
use crate::events::{Event, EventKind, EventLog};

/// How many durable-log events one [`HookDispatcher::tick`] page reads at a
/// time — the same value and reasoning as `ai::dispatch::DRAIN_PAGE`: small
/// enough that a single page never holds an initial sync's worth of events
/// in memory, large enough that a restart's catch-up does not cost one
/// round trip per event.
const DRAIN_PAGE: i64 = 500;

/// Default interval between dispatch ticks — matches
/// `ai::dispatch::DEFAULT_TICK_INTERVAL`'s reasoning: short enough that a
/// hook fires within a few seconds of the event that triggers it, long
/// enough that an idle mailbox is not polling the database several times a
/// second for nothing.
pub const DEFAULT_TICK_INTERVAL: Duration = Duration::from_secs(5);

/// Floor on [`crate::config::HooksConfig::tick_interval`]. Small enough that a
/// test (or an operator who genuinely wants near-immediate hooks) can ask for
/// a fast loop, large enough that a `tick_interval = "0s"` typo cannot turn
/// the dispatcher into a busy loop against the event log.
pub const MIN_TICK_INTERVAL: Duration = Duration::from_millis(10);

/// Default cap on how many hook matches one [`HookDispatcher::tick`] queues
/// before stopping — see the module docs' "One tick's matches are capped."
/// Matches `ai::dispatch::DEFAULT_LEASE_LIMIT`'s order of magnitude times a
/// generous multiplier: hooks are typically few, but a burst against several
/// enabled hooks at once should still be bounded well short of "the whole
/// page."
pub const DEFAULT_MAX_BATCH: usize = 200;

/// The chunk size [`read_capped`] reads with — small enough to bound one
/// iteration's memory, large enough that draining a large stream costs a
/// reasonable number of syscalls.
const READ_CHUNK: usize = 8192;

// ---------------------------------------------------------------------------
// run_hook: the process-execution primitive
// ---------------------------------------------------------------------------

/// The result of one hook execution, whether from a real dispatch or an
/// on-demand [`TestHook`](crate) run.
#[derive(Debug, Clone, Default)]
pub struct HookOutcome {
    /// The hook exceeded its timeout and was killed before finishing.
    pub timed_out: bool,
    /// The hook was killed because `cancel` fired (daemon shutdown) before
    /// it finished.
    pub cancelled: bool,
    /// The process's exit code. `None` when the process could not be
    /// spawned at all, or was killed rather than exiting on its own —
    /// mirrors [`std::process::ExitStatus::code`]'s own absence contract.
    pub exit_code: Option<i32>,
    /// Captured stdout, truncated to the caller's `max_output_bytes` and
    /// lossily decoded as UTF-8 (a hook's output is diagnostic text for an
    /// operator, not a byte-exact contract). Populated even on a timeout or
    /// cancellation, with whatever was captured before the kill — see the
    /// module docs.
    pub stdout: String,
    /// Captured stderr, same truncation/decoding/partial-capture behavior
    /// as `stdout`. Carries a synthesized message (never the raw OS error
    /// text of a *hook's own* failure, only this dispatcher's) when the
    /// process could not be spawned at all.
    pub stderr: String,
    /// Wall-clock time from spawn attempt to this outcome being produced.
    pub duration: Duration,
}

impl HookOutcome {
    /// Whether the hook ran to completion and exited zero. `false` for a
    /// timeout, a cancellation, a spawn failure, or any non-zero exit.
    #[must_use]
    pub fn succeeded(&self) -> bool {
        !self.timed_out && !self.cancelled && self.exit_code == Some(0)
    }
}

/// Run `command`/`args` with `stdin_payload` on its stdin (and only its
/// stdin — see the module docs), bounded by `timeout` and `cancel`.
///
/// Spawning and waiting never block the async runtime: this is
/// [`tokio::process::Command`] throughout, not `std::process::Command`
/// wrapped in `spawn_blocking`, so the daemon's other work is never held up
/// by a slow or wedged hook.
///
/// Never returns an `Err` — a hook that could not even be spawned (e.g. the
/// command does not exist) is reported as a normal [`HookOutcome`] with no
/// exit code and a diagnostic `stderr`, the same as any other hook failure.
/// This is deliberate: the caller (a dispatch tick processing a burst of
/// matched hooks, or the `TestHook` RPC) must treat "the operator's command
/// is broken" as an ordinary, expected outcome to report — never as a
/// reason to abort the whole batch or the RPC.
pub async fn run_hook(
    command: &str,
    args: &[String],
    timeout: Duration,
    max_output_bytes: usize,
    stdin_payload: &[u8],
    cancel: &CancellationToken,
) -> HookOutcome {
    let started = Instant::now();

    let mut cmd = Command::new(command);
    cmd.args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Belt-and-braces alongside the explicit kill on timeout/cancel
        // below: if this future itself were ever dropped without reaching
        // either branch (a panic elsewhere in the same task, an `abort()`),
        // the child still does not outlive it.
        .kill_on_drop(true);
    // Makes this child its own process-group leader, so a timeout/
    // cancellation can reach children *it* forks (the documented
    // `sh -c "cmd1; cmd2"` pattern) via `kill_process_group`, not just the
    // one pid this module spawned directly — see the module docs.
    #[cfg(unix)]
    cmd.process_group(0);

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(error) => {
            return HookOutcome {
                stderr: format!("failed to spawn hook {command:?}: {error}"),
                duration: started.elapsed(),
                ..HookOutcome::default()
            };
        }
    };

    // `Stdio::piped()` on all three above guarantees these are `Some`; the
    // `else` arms are unreachable in practice and exist only so this
    // function never needs `.unwrap()`/`.expect()` to get past the `Option`.
    let (Some(mut stdin), Some(mut stdout), Some(mut stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        kill_and_abort(&mut child, []).await;
        return HookOutcome {
            stderr: "internal error: hook process stdio was not wired as piped".to_owned(),
            duration: started.elapsed(),
            ..HookOutcome::default()
        };
    };

    // Drained into concurrently-shared buffers rather than each reader
    // task's own return value, so a timeout/cancellation — which aborts
    // these tasks rather than awaiting their result — still has whatever
    // was captured up to the moment of the kill. See the module docs' own
    // "Writing stdin, draining stdout/stderr, and waiting all run
    // concurrently."
    let stdout_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let stderr_buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));

    let payload = stdin_payload.to_vec();
    let write_handle = tokio::spawn(async move {
        // Errors here (most commonly `BrokenPipe`, when the hook exits
        // without reading stdin at all) are expected, not a dispatcher
        // bug, and silently dropped rather than surfaced.
        let _ = stdin.write_all(&payload).await;
        // `shutdown()` on a pipe's write half is a no-op on Unix (there is
        // no half-close to perform beyond closing the fd) — the real EOF
        // signal is `stdin` being dropped when this task ends. Kept for
        // symmetry with `AsyncWrite` implementations where it does matter,
        // and so a future refactor holding `stdin` open past this point
        // does not silently stop closing it.
        let _ = stdin.shutdown().await;
    });
    let stdout_handle = tokio::spawn({
        let stdout_buf = Arc::clone(&stdout_buf);
        async move {
            let _ = read_capped(&mut stdout, max_output_bytes, &stdout_buf).await;
        }
    });
    let stderr_handle = tokio::spawn({
        let stderr_buf = Arc::clone(&stderr_buf);
        async move {
            let _ = read_capped(&mut stderr, max_output_bytes, &stderr_buf).await;
        }
    });
    // Kept independently of the `JoinHandle`s above so the timeout/
    // cancellation branches below can still stop them after the handles
    // themselves have been moved into the `select!`'s first branch.
    let write_abort = write_handle.abort_handle();
    let stdout_abort = stdout_handle.abort_handle();
    let stderr_abort = stderr_handle.abort_handle();

    /// What the race below decided, holding no borrow of `child` — the kill
    /// path needs `&mut child` again *after* the race is fully evaluated
    /// (see below), which only typechecks if nothing here still borrows it.
    enum Raced {
        Finished(std::io::Result<std::process::ExitStatus>),
        TimedOut,
        Cancelled,
    }

    // Write/drain/wait race concurrently against the timeout and
    // cancellation — see the module docs' "Writing stdin, draining
    // stdout/stderr, and waiting all run concurrently" for why sequencing
    // any of these risks a full-pipe deadlock. The whole `select!`
    // expression's internal futures (including the one borrowing `child`
    // for `child.wait()`) are dropped by the time it evaluates to `raced`,
    // which is what makes the `&mut child` reuse in the timeout/cancelled
    // arms below legal.
    let raced = tokio::select! {
        joined = async { tokio::join!(write_handle, stdout_handle, stderr_handle, child.wait()) } => {
            let (_write, _stdout, _stderr, status) = joined;
            Raced::Finished(status)
        }
        () = tokio::time::sleep(timeout) => Raced::TimedOut,
        () = cancel.cancelled() => Raced::Cancelled,
    };

    match raced {
        Raced::Finished(status) => HookOutcome {
            timed_out: false,
            cancelled: false,
            exit_code: status.ok().and_then(|s| s.code()),
            stdout: lossy(take_buf(&stdout_buf)),
            stderr: lossy(take_buf(&stderr_buf)),
            duration: started.elapsed(),
        },
        Raced::TimedOut => {
            kill_and_abort(&mut child, [write_abort, stdout_abort, stderr_abort]).await;
            HookOutcome {
                timed_out: true,
                cancelled: false,
                exit_code: None,
                stdout: lossy(take_buf(&stdout_buf)),
                stderr: lossy(take_buf(&stderr_buf)),
                duration: started.elapsed(),
            }
        }
        Raced::Cancelled => {
            kill_and_abort(&mut child, [write_abort, stdout_abort, stderr_abort]).await;
            HookOutcome {
                timed_out: false,
                cancelled: true,
                exit_code: None,
                stdout: lossy(take_buf(&stdout_buf)),
                stderr: lossy(take_buf(&stderr_buf)),
                duration: started.elapsed(),
            }
        }
    }
}

/// Kill (SIGKILL, whole process group where supported) and reap `child`,
/// then abort the given helper tasks — the shared tail of [`run_hook`]'s
/// timeout and cancellation branches.
///
/// `wait`ing after the kill is what makes this a real termination a caller
/// can observe from outside the process (no zombie left behind), not merely
/// this future giving up on it — see the module docs' "A hook exceeding its
/// timeout is killed, not abandoned."
async fn kill_and_abort<const N: usize>(child: &mut Child, aborts: [tokio::task::AbortHandle; N]) {
    if let Some(pid) = child.id() {
        kill_process_group(pid);
    }
    // Belt-and-braces alongside the process-group kill above: also goes
    // through tokio's own single-pid path, in case `process_group(0)` had
    // no effect for this child (a non-Unix target, or a command that
    // re-parented itself out of the group before the signal landed).
    let _ = child.start_kill();
    let _ = child.wait().await;
    for abort in aborts {
        abort.abort();
    }
}

/// Send `SIGKILL` to the whole process group `pid` leads, per POSIX
/// `kill(2)`'s "a negative pid signals the group" convention — see the
/// module docs' "A hook exceeding its timeout is killed, not abandoned —
/// the whole group" for why only killing `pid` itself is not enough for a
/// hook that forked children.
///
/// A no-op on non-Unix targets (this crate's own `[target.'cfg(unix)']`
/// dependency on `libc` matches `tokio::process::Command::process_group`'s
/// identical `#[cfg(unix)]` gate at the call site in [`run_hook`] that makes
/// this signal meaningful in the first place).
#[cfg(unix)]
fn kill_process_group(pid: u32) {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return;
    };
    // SAFETY: `kill(2)` with a negative pid signals the whole process
    // group rather than a single process. A pid that is out of range,
    // already reaped, or not a process-group leader is not memory-unsafe
    // here — at worst a harmless `ESRCH`/`EPERM`, both silently discarded
    // (this call's whole point is best-effort cleanup, not a reportable
    // operation).
    unsafe {
        libc::kill(-pid, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_process_group(_pid: u32) {}

/// Move `buf`'s contents out, leaving it empty — used to pull whatever
/// [`read_capped`] captured out of a shared buffer after a hook finishes,
/// times out, or is cancelled.
fn take_buf(buf: &Mutex<Vec<u8>>) -> Vec<u8> {
    std::mem::take(&mut *buf.lock().unwrap_or_else(PoisonError::into_inner))
}

/// Lossily decode captured process output as UTF-8 — diagnostic text for an
/// operator, not a byte-exact contract (see [`HookOutcome::stdout`]).
fn lossy(bytes: Vec<u8>) -> String {
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Read `reader` to EOF, appending at most `cap` bytes total into `out` but
/// continuing to drain everything past that cap.
///
/// Stopping at `cap` instead of draining to EOF is the pipe-deadlock bug
/// this module exists to avoid: a hook writing more than `cap` bytes would
/// fill the kernel pipe buffer and block on `write()` forever once this
/// side stopped reading, which — since [`run_hook`] waits on the child
/// concurrently with this read — would hang the whole hook run rather than
/// merely losing the untruncated tail of its output.
async fn read_capped<R>(reader: &mut R, cap: usize, out: &Mutex<Vec<u8>>) -> std::io::Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut chunk = [0u8; READ_CHUNK];
    loop {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            return Ok(());
        }
        // Locked only for the length of one `extend_from_slice`, never
        // across an `.await` — a hook aborted mid-read must not be able to
        // leave this poisoned or held.
        let mut guard = out.lock().unwrap_or_else(PoisonError::into_inner);
        if guard.len() < cap {
            let take = (cap - guard.len()).min(n);
            guard.extend_from_slice(&chunk[..take]);
        }
    }
}

// ---------------------------------------------------------------------------
// Config resolution
// ---------------------------------------------------------------------------

/// A [`HookConfig`](crate::config::HookConfig) with its timeout fully
/// resolved (per-hook override, or [`HooksConfig::default_timeout`]) — what
/// [`HookDispatcher`] and `HookService` (`rmaild`) both actually operate on.
#[derive(Debug, Clone)]
pub struct ResolvedHook {
    /// See [`crate::config::HookConfig::name`].
    pub name: String,
    /// See [`crate::config::HookConfig::event`].
    pub event: HookEvent,
    /// See [`crate::config::HookConfig::command`].
    pub command: String,
    /// See [`crate::config::HookConfig::args`].
    pub args: Vec<String>,
    /// See [`crate::config::HookConfig::enabled`].
    pub enabled: bool,
    /// [`crate::config::HookConfig::timeout`] if set, else
    /// [`HooksConfig::default_timeout`].
    pub timeout: Duration,
}

/// Resolve every hook in `config`, applying the default timeout where a hook
/// does not override it.
///
/// Includes disabled hooks — callers that only want the ones a dispatcher
/// actually fires (not `ListHooks`, which shows every configured hook) must
/// filter on [`ResolvedHook::enabled`] themselves, as [`HookDispatcher::new`]
/// does.
///
/// A duplicate `name` is not a config-load error (TOML has no native way to
/// reject a duplicate key across array-of-table elements — see
/// [`crate::config::HookConfig::name`]'s own docs), so later duplicates are
/// dropped here with a logged warning rather than silently shadowing an
/// earlier one in `ListHooks`/`TestHook`/dispatch matching in three
/// different, possibly inconsistent ways.
#[must_use]
pub fn resolve(config: &HooksConfig) -> Vec<ResolvedHook> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::with_capacity(config.hooks.len());
    for hook in &config.hooks {
        if !seen.insert(hook.name.clone()) {
            tracing::warn!(
                name = %hook.name,
                "duplicate hook name in config; keeping the first occurrence only"
            );
            continue;
        }
        out.push(ResolvedHook {
            name: hook.name.clone(),
            event: hook.event,
            command: hook.command.clone(),
            args: hook.args.clone(),
            enabled: hook.enabled,
            timeout: hook
                .timeout
                .map_or(config.default_timeout.as_duration(), |t| t.as_duration()),
        });
    }
    out
}

/// Whether `hook_event` fires for `event` — the mapping from the hooks
/// config's product-facing vocabulary onto the durable event bus's wire
/// vocabulary. See [`HookEvent`]'s own doc comment for why `OnSyncError` is
/// a filter on `SyncState`'s payload rather than a distinct `EventKind`.
#[must_use]
pub fn hook_matches(hook_event: HookEvent, event: &Event) -> bool {
    match hook_event {
        HookEvent::OnNewMessage => event.kind == EventKind::NewMail,
        HookEvent::OnLabel => event.kind == EventKind::FlagChanged,
        HookEvent::OnMove => event.kind == EventKind::Moved,
        HookEvent::OnRuleMatch => event.kind == EventKind::RuleFired,
        HookEvent::OnSyncError => {
            event.kind == EventKind::SyncState
                && event
                    .payload
                    .get("error")
                    .is_some_and(|value| !value.is_null())
        }
    }
}

/// The exact bytes a real dispatch pipes to a matched hook's stdin: the
/// durable event, verbatim — `seq`/`kind`/scope ids/`at`/`payload`, the same
/// fields [`Event`] itself carries. `kind` is rendered as its stable wire
/// string ([`EventKind::as_str`]), matching what is actually stored in
/// `events.kind` rather than a Rust-specific `Debug` spelling.
///
/// Message content (a subject, a body) is *not* included here, because no
/// [`Event`] this codebase publishes carries it today — `NewMail`'s payload
/// is `null`; see `sync::engine::LogSink`. A hook that needs it looks the
/// message up itself via `message_id` (e.g. `mail get`/`MailService.Get`),
/// which keeps this dispatcher itself blind to mail content: there is
/// structurally nothing here for a future bug to leak, and the stdin-only
/// contract this module enforces (see the module docs) protects whatever
/// event payloads carry today *and* whatever a future event kind adds.
fn event_stdin_json(event: &Event) -> Vec<u8> {
    let value = serde_json::json!({
        "seq": event.seq,
        "kind": event.kind.as_str(),
        "account_id": event.account_id,
        "mailbox_id": event.mailbox_id,
        "message_id": event.message_id,
        "at": event.at,
        "payload": event.payload,
    });
    // Cannot fail: every field is either a plain integer/string or
    // `event.payload`, which `EventLog::append_all` already required to
    // serialize successfully before this event was ever durably stored.
    serde_json::to_vec(&value).unwrap_or_default()
}

/// A synthetic sample event JSON for `event`, shaped identically to
/// [`event_stdin_json`]'s real output — what `TestHook` (`rmaild`) pipes to
/// a hook's stdin when the caller supplies no `event_json` of their own.
/// Carries `"test": true` so a hook script can distinguish a dry run from a
/// real dispatch if it cares to.
#[must_use]
pub fn sample_event_json(event: HookEvent) -> Vec<u8> {
    let (kind, payload) = match event {
        HookEvent::OnNewMessage => (EventKind::NewMail, serde_json::Value::Null),
        HookEvent::OnLabel => (
            EventKind::FlagChanged,
            serde_json::json!({ "uid": 1, "flags": ["\\Seen"] }),
        ),
        HookEvent::OnMove => (
            EventKind::Moved,
            serde_json::json!({
                "uid": 1,
                "from_mailbox_id": 1,
                "to_mailbox_id": 2,
                "to_mailbox": "Archive",
            }),
        ),
        HookEvent::OnRuleMatch => (
            EventKind::RuleFired,
            serde_json::json!({ "rule": "sample-rule" }),
        ),
        HookEvent::OnSyncError => (
            EventKind::SyncState,
            serde_json::json!({ "folder": "INBOX", "error": "sample sync error" }),
        ),
    };
    let value = serde_json::json!({
        "seq": 0,
        "kind": kind.as_str(),
        "account_id": 1,
        "mailbox_id": 1,
        "message_id": serde_json::Value::Null,
        "at": 0,
        "payload": payload,
        "test": true,
    });
    serde_json::to_vec(&value).unwrap_or_default()
}

// ---------------------------------------------------------------------------
// The dispatcher
// ---------------------------------------------------------------------------

/// What one [`HookDispatcher::tick`] did — for logging and tests, not a
/// contract any caller needs to branch on.
#[derive(Debug, Clone, Copy, Default)]
pub struct HookTickReport {
    /// Hooks matched and run this tick (of any outcome).
    pub fired: u64,
    /// Hooks that ran but exited non-zero, or could not be spawned at all.
    pub failed: u64,
    /// Hooks that exceeded their timeout and were killed.
    pub timed_out: u64,
    /// This tick stopped matching early because it hit
    /// [`HookDispatcher::max_batch`] — more matches remain for the next
    /// tick to pick up. See the module docs' "One tick's matches are
    /// capped."
    pub capped: bool,
}

/// The daemon-side consumer of the durable event log: matches newly-logged
/// events against configured hooks and runs the matches, bounded by a
/// `Semaphore(max_concurrency)` shared across every hook. One instance per
/// daemon process.
#[derive(Clone)]
pub struct HookDispatcher {
    events: EventLog,
    /// Enabled hooks only — see [`resolve`]'s own docs on why `ListHooks`
    /// (which wants every hook) cannot share this filtered list.
    hooks: Vec<ResolvedHook>,
    /// Shared with `HookApi::test_hook` (`rmaild`) — a `TestHook` RPC draws
    /// from the *same* concurrency budget this dispatcher enforces for its
    /// own matched hooks, rather than a second, independent budget that
    /// could double `hooks.max_concurrency`'s real ceiling in practice.
    /// Mirrors `AiWorkerPool::semaphore`'s identical reasoning and the
    /// identical reason `AiApi::AnalyzeMessage` shares its pool's semaphore
    /// rather than minting its own.
    semaphore: Arc<Semaphore>,
    max_output_bytes: usize,
    tick_interval: Duration,
    max_batch: usize,
    /// Negative until [`HookDispatcher::spawn`] seeds it from
    /// [`EventLog::latest_seq`] (or, for a direct-`tick` caller that never
    /// spawns, until the first `tick()` does) — see the module docs' "The
    /// cursor starts at 'now'."
    cursor: Arc<AtomicI64>,
}

impl std::fmt::Debug for HookDispatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookDispatcher")
            .field(
                "hooks",
                &self.hooks.iter().map(|h| &h.name).collect::<Vec<_>>(),
            )
            .field("tick_interval", &self.tick_interval)
            .field("max_batch", &self.max_batch)
            .finish_non_exhaustive()
    }
}

impl HookDispatcher {
    /// Cursor value meaning "not yet seeded from the event log" — never a
    /// real `seq` (those are always `>= 0`; `EventLog::since` itself
    /// rejects a negative cursor as invalid input), so it can never be
    /// confused with a genuine position.
    const UNSEEDED_CURSOR: i64 = -1;

    /// Build a dispatcher over `events`, driving every *enabled* hook in
    /// `config`.
    #[must_use]
    pub fn new(events: EventLog, config: &HooksConfig) -> Self {
        let hooks: Vec<ResolvedHook> = resolve(config).into_iter().filter(|h| h.enabled).collect();
        Self {
            events,
            hooks,
            semaphore: Arc::new(Semaphore::new(config.max_concurrency.max(1) as usize)),
            max_output_bytes: config.max_output_bytes as usize,
            // A zero interval would spin the dispatch loop as fast as the
            // runtime allows, hammering the event log with no gain — a config
            // typo degrades to "tick as fast as is sane", never to a busy loop.
            tick_interval: config.tick_interval.as_duration().max(MIN_TICK_INTERVAL),
            max_batch: DEFAULT_MAX_BATCH,
            cursor: Arc::new(AtomicI64::new(Self::UNSEEDED_CURSOR)),
        }
    }

    /// Override the default tick interval.
    #[must_use]
    pub fn with_tick_interval(mut self, interval: Duration) -> Self {
        self.tick_interval = interval;
        self
    }

    /// Override the default per-tick batch cap ([`DEFAULT_MAX_BATCH`]) —
    /// see the module docs' "One tick's matches are capped."
    #[must_use]
    pub fn with_max_batch(mut self, max_batch: usize) -> Self {
        self.max_batch = max_batch.max(1);
        self
    }

    /// How many enabled hooks this dispatcher drives — for tests and
    /// logging.
    #[must_use]
    pub fn hook_count(&self) -> usize {
        self.hooks.len()
    }

    /// This dispatcher's `Semaphore(max_concurrency)` — shared, not
    /// cloned-fresh, with `HookApi::test_hook` (`rmaild`). See the
    /// `semaphore` field's own doc comment for why.
    #[must_use]
    pub fn semaphore(&self) -> Arc<Semaphore> {
        Arc::clone(&self.semaphore)
    }

    /// One dispatch cycle: drain newly-logged events since this
    /// dispatcher's cursor, match each against every configured hook, and
    /// run every match — bounded by `Semaphore(max_concurrency)`, each run
    /// bounded by its own timeout, at most [`Self::max_batch`] matches
    /// queued (see the module docs' "One tick's matches are capped"), and
    /// all matches for this tick awaited before returning (mirroring
    /// `AiWorkerPool::dispatch_pending`'s own "await the whole batch, let
    /// the semaphore bound how much of it runs at once" shape).
    ///
    /// # The cursor is seeded from the log's current head, not its start
    ///
    /// The first call after this dispatcher is constructed (cursor still
    /// [`Self::UNSEEDED_CURSOR`]) seeds it from
    /// [`crate::events::EventLog::latest_seq`] before draining anything —
    /// see the module docs' "The cursor starts at 'now'" for why replaying
    /// the whole retention window, safe for `AiDispatchLoop`, is not safe
    /// here.
    ///
    /// # Retention gaps self-heal rather than wedging the loop
    ///
    /// Same shape as `AiDispatchLoop::drain_new_mail`'s recovery: this
    /// cursor is never persisted, so a quiet mailbox can let retention
    /// prune the log out from under it. [`crate::events::EventLog::since`]
    /// answers that with [`ErrorReason::OutOfRange`], recovered from here
    /// by resetting to the log's *current* head (not `0` — see the module
    /// docs again for why the "replay from the beginning" recovery
    /// `AiDispatchLoop` uses would replay every enabled hook here) and
    /// retrying once per tick call, rather than propagating the error and
    /// wedging on the first quiet stretch a long-running daemon hits.
    ///
    /// # Errors
    /// A mapped storage error from reading the event log. Never a single
    /// hook's own failure — see the module docs' "A failing or missing hook
    /// does not stall the event consumer."
    #[tracing::instrument(skip(self, cancel), fields(since, fired, capped, next_cursor))]
    pub async fn tick(&self, cancel: &CancellationToken) -> Result<HookTickReport, Error> {
        let mut since = self.cursor.load(Ordering::SeqCst);
        if since == Self::UNSEEDED_CURSOR {
            since = self.events.latest_seq().await?.unwrap_or(0);
        }
        let mut cursor = since;
        let mut matched: Vec<(ResolvedHook, Vec<u8>)> = Vec::new();
        let mut capped = false;
        let mut recovered_once = false;
        'drain: loop {
            let page = match self.events.since(cursor, DRAIN_PAGE).await {
                Ok(page) => page,
                Err(error) if error.reason() == ErrorReason::OutOfRange && !recovered_once => {
                    let head = self.events.latest_seq().await?.unwrap_or(0);
                    tracing::warn!(
                        cursor,
                        head,
                        %error,
                        "hook dispatch cursor fell behind the event log's retention window; \
                         resuming from the current head rather than replaying history"
                    );
                    cursor = head;
                    recovered_once = true;
                    continue;
                }
                Err(error) => return Err(error),
            };
            let got = page.events.len();
            for event in &page.events {
                // Advance precisely to what has actually been considered so
                // far, not just to the page's own scanned bound — if the
                // batch cap below stops this loop mid-page, the next tick
                // must resume right after this event, not skip the rest of
                // the page it never looked at.
                cursor = event.seq;
                // The cap is only ever checked at an *event* boundary, never
                // inside the hook loop below. `matched` has no dedup and
                // this dispatcher never revisits an event once its cursor
                // has passed it — stopping mid-event (after hook 2 of 3
                // matched the same event, say) would permanently drop hook
                // 3's invocation for it: the next tick resumes strictly
                // after this event's `seq` and can never see it again. This
                // can overshoot `max_batch` by at most `self.hooks.len() -
                // 1`, which is the same bound `page.next_seq`'s own
                // "scanned to the page boundary, not exactly `limit`"
                // contract already accepts.
                for hook in &self.hooks {
                    if hook_matches(hook.event, event) {
                        matched.push((hook.clone(), event_stdin_json(event)));
                    }
                }
                if matched.len() >= self.max_batch {
                    capped = true;
                    break 'drain;
                }
            }
            cursor = page.next_seq;
            if i64::try_from(got).unwrap_or(i64::MAX) < DRAIN_PAGE {
                break;
            }
        }
        self.cursor.store(cursor, Ordering::SeqCst);

        let mut set = tokio::task::JoinSet::new();
        for (hook, payload) in matched {
            let semaphore = Arc::clone(&self.semaphore);
            let max_output_bytes = self.max_output_bytes;
            let cancel = cancel.clone();
            let span = tracing::info_span!("hook_run", hook = %hook.name, event = ?hook.event);
            set.spawn(
                async move {
                    let Ok(_permit) = semaphore.acquire_owned().await else {
                        // The semaphore is never explicitly closed by this
                        // dispatcher; unreachable in practice.
                        return (hook.name, None);
                    };
                    if cancel.is_cancelled() {
                        return (hook.name, None);
                    }
                    let outcome = run_hook(
                        &hook.command,
                        &hook.args,
                        hook.timeout,
                        max_output_bytes,
                        &payload,
                        &cancel,
                    )
                    .await;
                    (hook.name, Some(outcome))
                }
                .instrument(span),
            );
        }

        let mut report = HookTickReport {
            capped,
            ..HookTickReport::default()
        };
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok((_, None)) => {}
                Ok((name, Some(outcome))) => {
                    report.fired += 1;
                    if outcome.timed_out {
                        report.timed_out += 1;
                        tracing::warn!(hook = %name, timeout_ms = outcome.duration.as_millis(), "hook timed out and was killed");
                    } else if !outcome.succeeded() {
                        report.failed += 1;
                        tracing::warn!(
                            hook = %name,
                            exit_code = ?outcome.exit_code,
                            stderr = %log_excerpt(&outcome.stderr),
                            "hook failed"
                        );
                    } else {
                        tracing::debug!(hook = %name, duration_ms = outcome.duration.as_millis(), "hook completed");
                    }
                }
                Err(join_error) => {
                    tracing::error!(error = %join_error, "a hook task panicked or was aborted");
                }
            }
        }

        let span = tracing::Span::current();
        span.record("since", since);
        span.record("fired", report.fired);
        span.record("capped", report.capped);
        span.record("next_cursor", cursor);
        Ok(report)
    }

    /// Seed the cursor to the event log's current head, so that "start at
    /// now" means *now* rather than "whenever the tick task is first
    /// scheduled". Idempotent: a cursor that has already been seeded (or
    /// advanced by a `tick`) is left alone.
    ///
    /// A failure here is deliberately not fatal — it leaves the cursor
    /// unseeded, and [`Self::tick`] falls back to seeding it lazily exactly
    /// as it did before. That costs the boot-window guarantee below on a
    /// database that is already failing to answer a trivial query, but it
    /// never takes the daemon down for it.
    async fn seed_cursor(&self) {
        if self.cursor.load(Ordering::SeqCst) != Self::UNSEEDED_CURSOR {
            return;
        }
        match self.events.latest_seq().await {
            Ok(head) => self.cursor.store(head.unwrap_or(0), Ordering::SeqCst),
            Err(error) => tracing::warn!(
                %error,
                "could not seed the hook dispatch cursor at startup; it will be seeded on \
                 the first tick instead, which may skip events appended in between"
            ),
        }
    }

    /// Spawn the periodic tick loop, running once immediately (so a daemon
    /// restarted more often than the tick interval still makes progress —
    /// the same reasoning `AiDispatchLoop::spawn` and the event-log
    /// pruner task both apply to themselves) and then on the configured
    /// interval, until `cancel` fires.
    ///
    /// # Why this is `async` rather than a bare `tokio::spawn`
    ///
    /// The cursor is seeded here, *before* this returns, rather than inside
    /// the spawned task. `spawn` only queues the task; on a busy runtime the
    /// caller (the daemon boot path) goes on to bind its socket and start
    /// accepting requests well before that task is first polled. Seeding
    /// lazily on the first tick therefore pinned "now" to a scheduling
    /// accident: any event appended in that window — mail synced while the
    /// daemon was still coming up — was swallowed by the seed and, because
    /// [`EventLog::since`] is exclusive, never fired a hook at all.
    ///
    /// Awaiting the seed makes the guarantee the module docs claim actually
    /// hold: every event appended after `spawn(..).await` returns is dispatched,
    /// and everything before it is history.
    pub async fn spawn(self, cancel: CancellationToken) -> tokio::task::JoinHandle<()> {
        self.seed_cursor().await;
        tokio::spawn(async move {
            loop {
                match self.tick(&cancel).await {
                    Ok(report) => tracing::debug!(?report, "hook dispatch tick"),
                    Err(error) => tracing::warn!(%error, "hook dispatch tick failed"),
                }
                tokio::select! {
                    () = cancel.cancelled() => return,
                    () = tokio::time::sleep(self.tick_interval) => {}
                }
            }
        })
    }
}

/// A short prefix of `s`, for a log line — the full text is still in the
/// returned [`HookOutcome`]/`TestHookResponse`; the tracing log only needs
/// enough to diagnose a failure at a glance, not up to `max_output_bytes`
/// (64 KiB by default) of a hook's stderr repeated into the log stream on
/// every failing tick, which is both a log-volume problem and a plausible
/// path for a hook's stderr (which may itself echo something sensitive) to
/// end up duplicated in a place with different retention/access than the
/// hook's own output.
fn log_excerpt(s: &str) -> &str {
    const MAX: usize = 500;
    if s.len() <= MAX {
        return s;
    }
    // Back off to the nearest char boundary so this never panics on a
    // multi-byte UTF-8 sequence straddling the cut point.
    let mut end = MAX;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

#[cfg(test)]
mod tests;
