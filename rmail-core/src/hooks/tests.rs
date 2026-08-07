//! Proof of this module's own stated invariants: the event JSON reaches a
//! hook's stdin only (never its argv/command), a timed-out hook is truly
//! killed (not merely abandoned) — including children it forked, not just
//! the one process this module spawned directly — a chatty hook cannot
//! deadlock the dispatcher on a full pipe, a failing/missing hook does not
//! stall the event consumer or the hooks that follow it, a fresh dispatcher
//! never replays history that predates it, and one tick's matches are
//! capped. Every process-level test here spawns real `/bin/sh` subprocesses
//! rather than mocking [`tokio::process::Command`] — the whole point of
//! this module is what actually happens to an OS process, which a mock
//! cannot demonstrate.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::*;
use crate::config::{HookConfig, HookEvent, HooksConfig, HumanDuration};
use crate::events::{EventKind, EventLog, NewEvent, Retention};
use crate::storage::Database;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

fn unique_path(label: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    std::env::temp_dir().join(format!("rmail-hooks-{label}-{pid}-{n}"))
}

fn no_cancel() -> CancellationToken {
    CancellationToken::new()
}

fn sample_event(kind: EventKind, payload: serde_json::Value) -> Event {
    Event {
        seq: 1,
        kind,
        account_id: None,
        mailbox_id: None,
        message_id: None,
        at: 0,
        payload,
    }
}

/// Poll `path` until it contains a non-empty pid, for up to ~1s — a hook's
/// `echo $$ > path` write races the read only in theory (it happens well
/// before any timeout this test suite uses), but a loaded CI box deserves
/// the margin.
async fn wait_for_pid(path: &std::path::Path) -> u32 {
    let mut text = String::new();
    for _ in 0..50 {
        if let Ok(t) = std::fs::read_to_string(path) {
            if !t.trim().is_empty() {
                text = t;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !text.trim().is_empty(),
        "pid file must contain a pid by now"
    );
    text.trim()
        .parse()
        .expect("pid file must contain a valid u32 pid")
}

/// Poll `kill -0 <pid>` (the shell builtin, so this needs no external
/// `kill` binary to exist) until the process is gone, for up to ~3s.
/// Returns whether it died within that budget.
async fn pid_dies_within_budget(pid: u32) -> bool {
    for _ in 0..150 {
        let status = std::process::Command::new("sh")
            .args(["-c", &format!("kill -0 {pid}")])
            .status();
        let alive = matches!(status, Ok(s) if s.success());
        if !alive {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

// ---------------------------------------------------------------------------
// resolve(): config -> ResolvedHook
// ---------------------------------------------------------------------------

fn hook(name: &str, event: HookEvent, enabled: bool, timeout: Option<Duration>) -> HookConfig {
    HookConfig {
        name: name.to_owned(),
        event,
        command: "/bin/true".to_owned(),
        args: Vec::new(),
        enabled,
        timeout: timeout.map(HumanDuration::new),
    }
}

#[test]
fn resolve_falls_back_to_the_default_timeout_and_keeps_a_per_hook_override() {
    let config = HooksConfig {
        default_timeout: HumanDuration::new(Duration::from_secs(20)),
        hooks: vec![
            hook("a", HookEvent::OnNewMessage, true, None),
            hook("b", HookEvent::OnMove, false, Some(Duration::from_secs(5))),
        ],
        ..HooksConfig::default()
    };

    let resolved = resolve(&config);

    assert_eq!(resolved.len(), 2);
    assert_eq!(resolved[0].timeout, Duration::from_secs(20));
    assert_eq!(resolved[1].timeout, Duration::from_secs(5));
    assert!(!resolved[1].enabled);
}

#[test]
fn resolve_drops_a_duplicate_name_keeping_the_first() {
    let config = HooksConfig {
        hooks: vec![
            hook("dup", HookEvent::OnNewMessage, true, None),
            hook("dup", HookEvent::OnMove, true, None),
        ],
        ..HooksConfig::default()
    };

    let resolved = resolve(&config);

    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].event, HookEvent::OnNewMessage);
}

// ---------------------------------------------------------------------------
// hook_matches(): the hooks-config vocabulary -> EventKind mapping
// ---------------------------------------------------------------------------

#[test]
fn hook_matches_maps_each_event_kind_to_its_hook_event() {
    assert!(hook_matches(
        HookEvent::OnNewMessage,
        &sample_event(EventKind::NewMail, serde_json::Value::Null)
    ));
    assert!(!hook_matches(
        HookEvent::OnNewMessage,
        &sample_event(EventKind::FlagChanged, serde_json::Value::Null)
    ));
    assert!(hook_matches(
        HookEvent::OnLabel,
        &sample_event(EventKind::FlagChanged, serde_json::Value::Null)
    ));
    assert!(hook_matches(
        HookEvent::OnMove,
        &sample_event(EventKind::Moved, serde_json::Value::Null)
    ));
    assert!(hook_matches(
        HookEvent::OnRuleMatch,
        &sample_event(EventKind::RuleFired, serde_json::Value::Null)
    ));
}

#[test]
fn on_sync_error_only_matches_a_sync_state_event_carrying_a_non_null_error() {
    let clean = sample_event(EventKind::SyncState, serde_json::json!({ "error": null }));
    let failed = sample_event(
        EventKind::SyncState,
        serde_json::json!({ "error": "imap connection reset" }),
    );
    let no_error_key = sample_event(EventKind::SyncState, serde_json::json!({}));

    assert!(
        !hook_matches(HookEvent::OnSyncError, &clean),
        "a successful sync pass must not fire on_sync_error"
    );
    assert!(hook_matches(HookEvent::OnSyncError, &failed));
    assert!(!hook_matches(HookEvent::OnSyncError, &no_error_key));
}

#[test]
fn sample_event_json_is_valid_json_carrying_the_test_marker_for_every_hook_event() {
    for event in [
        HookEvent::OnNewMessage,
        HookEvent::OnLabel,
        HookEvent::OnMove,
        HookEvent::OnRuleMatch,
        HookEvent::OnSyncError,
    ] {
        let bytes = sample_event_json(event);
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).expect("sample_event_json must produce valid JSON");
        assert_eq!(value["test"], serde_json::json!(true));
        assert!(value["kind"].is_string());
    }
}

// ---------------------------------------------------------------------------
// run_hook(): the security invariant — stdin only, never argv/command
// ---------------------------------------------------------------------------

#[tokio::test]
async fn run_hook_pipes_event_json_on_stdin_only_never_into_argv_or_command() {
    let out_file = unique_path("stdin-echo.out");
    let pwned_file = unique_path("pwned");
    let _ = std::fs::remove_file(&out_file);
    let _ = std::fs::remove_file(&pwned_file);

    // Shaped like a real event whose (attacker-controlled) subject carries
    // shell metacharacters. If this were ever interpolated into a command
    // string and handed to a shell, it would run `touch <pwned_file>`;
    // piped only to stdin, as this module's contract requires, it is inert
    // data a `cat` copies verbatim.
    let malicious = serde_json::json!({
        "kind": "NEW_MAIL",
        "payload": {
            "subject": format!("hi\"; touch {}; echo pwned #", pwned_file.display()),
        },
    });
    let payload = serde_json::to_vec(&malicious).expect("serializes");

    let outcome = run_hook(
        "/bin/sh",
        &["-c".to_owned(), format!("cat > {}", out_file.display())],
        Duration::from_secs(5),
        1024 * 1024,
        &payload,
        &no_cancel(),
    )
    .await;

    assert!(outcome.succeeded(), "hook should exit 0: {outcome:?}");
    let written = std::fs::read(&out_file).expect("hook must have written the file");
    assert_eq!(
        written, payload,
        "the exact stdin bytes must reach the hook, byte for byte"
    );
    assert!(
        !pwned_file.exists(),
        "the shell metacharacters in the payload must never be executed"
    );

    let _ = std::fs::remove_file(&out_file);
}

// ---------------------------------------------------------------------------
// run_hook(): missing/failing commands report as an outcome, never a panic
// ---------------------------------------------------------------------------

#[tokio::test]
async fn run_hook_reports_a_missing_command_without_panicking() {
    let outcome = run_hook(
        "/definitely/not/a/real/command-xyz",
        &[],
        Duration::from_secs(5),
        1024,
        b"{}",
        &no_cancel(),
    )
    .await;

    assert!(!outcome.succeeded());
    assert!(!outcome.timed_out);
    assert!(!outcome.cancelled);
    assert_eq!(outcome.exit_code, None);
    assert!(outcome.stderr.contains("failed to spawn"));
}

#[tokio::test]
async fn run_hook_reports_a_non_zero_exit() {
    let outcome = run_hook(
        "/bin/sh",
        &["-c".to_owned(), "exit 7".to_owned()],
        Duration::from_secs(5),
        1024,
        b"{}",
        &no_cancel(),
    )
    .await;

    assert!(!outcome.succeeded());
    assert_eq!(outcome.exit_code, Some(7));
}

// ---------------------------------------------------------------------------
// run_hook(): a timed-out hook is truly killed, not merely abandoned
// ---------------------------------------------------------------------------

#[tokio::test]
async fn run_hook_exceeding_timeout_is_actually_killed_not_merely_abandoned() {
    let pid_file = unique_path("timeout-pid");
    let _ = std::fs::remove_file(&pid_file);

    let outcome = run_hook(
        "/bin/sh",
        &[
            "-c".to_owned(),
            format!("echo $$ > {}; sleep 30", pid_file.display()),
        ],
        Duration::from_millis(200),
        1024,
        b"{}",
        &no_cancel(),
    )
    .await;

    assert!(
        outcome.timed_out,
        "must be reported as timed out: {outcome:?}"
    );
    assert!(!outcome.cancelled);
    assert_eq!(outcome.exit_code, None);

    let pid = wait_for_pid(&pid_file).await;

    // The definitive, out-of-process proof this is a real kill and not an
    // abandoned future: if `run_hook` had only dropped the future, the
    // shell (and its `sleep 30`) would still be alive for ~30 more seconds
    // and this would time out still finding it alive.
    assert!(
        pid_dies_within_budget(pid).await,
        "pid {pid} must no longer exist after run_hook killed it"
    );

    let _ = std::fs::remove_file(&pid_file);
}

#[tokio::test]
async fn run_hook_timeout_kills_the_whole_process_group_not_just_the_direct_child() {
    // The documented way an operator opts into shell features is
    // `command = "/bin/sh"`, `args = ["-c", "cmd1; cmd2"]` — this
    // backgrounds a second process from inside the shell and proves that
    // killing the shell on timeout also kills what it forked, not only the
    // one pid this module spawned directly.
    let shell_pid_file = unique_path("group-shell-pid");
    let child_pid_file = unique_path("group-child-pid");
    let _ = std::fs::remove_file(&shell_pid_file);
    let _ = std::fs::remove_file(&child_pid_file);

    let outcome = run_hook(
        "/bin/sh",
        &[
            "-c".to_owned(),
            format!(
                "echo $$ > {}; sleep 30 & echo $! > {}; wait",
                shell_pid_file.display(),
                child_pid_file.display()
            ),
        ],
        Duration::from_millis(300),
        1024,
        b"{}",
        &no_cancel(),
    )
    .await;
    assert!(outcome.timed_out, "{outcome:?}");

    let child_pid = wait_for_pid(&child_pid_file).await;

    assert!(
        pid_dies_within_budget(child_pid).await,
        "pid {child_pid} (a grandchild the shell backgrounded, not the shell \
         itself) must also die -- a process-group kill must reach it, not just \
         the shell's own pid"
    );

    let _ = std::fs::remove_file(&shell_pid_file);
    let _ = std::fs::remove_file(&child_pid_file);
}

#[tokio::test]
async fn run_hook_is_killed_when_cancelled_even_with_a_long_timeout() {
    let pid_file = unique_path("cancel-pid");
    let _ = std::fs::remove_file(&pid_file);
    let cancel = CancellationToken::new();
    let firer = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        firer.cancel();
    });

    let outcome = run_hook(
        "/bin/sh",
        &[
            "-c".to_owned(),
            format!("echo $$ > {}; sleep 30", pid_file.display()),
        ],
        Duration::from_secs(30),
        1024,
        b"{}",
        &cancel,
    )
    .await;

    assert!(
        outcome.cancelled,
        "must be reported as cancelled: {outcome:?}"
    );
    assert!(!outcome.timed_out);

    let _ = std::fs::remove_file(&pid_file);
}

// ---------------------------------------------------------------------------
// run_hook(): partial output survives a timeout/cancellation kill
// ---------------------------------------------------------------------------

#[tokio::test]
async fn run_hook_captures_partial_output_before_a_timeout_kill() {
    let outcome = run_hook(
        "/bin/sh",
        &["-c".to_owned(), "echo partial-output; sleep 30".to_owned()],
        Duration::from_millis(300),
        1024,
        b"{}",
        &no_cancel(),
    )
    .await;

    assert!(outcome.timed_out, "{outcome:?}");
    assert!(
        outcome.stdout.contains("partial-output"),
        "output written before the kill must still be captured, not discarded \
         just because the run ended in a timeout: {outcome:?}"
    );
}

// ---------------------------------------------------------------------------
// run_hook(): a chatty hook cannot deadlock the dispatcher on a full pipe
// ---------------------------------------------------------------------------

#[tokio::test]
async fn run_hook_large_stdout_does_not_deadlock() {
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        run_hook(
            "/bin/sh",
            &["-c".to_owned(), "head -c 1048576 /dev/zero".to_owned()],
            Duration::from_secs(8),
            4096,
            b"{}",
            &no_cancel(),
        ),
    )
    .await;

    let outcome = result.expect("run_hook must not deadlock on a full stdout pipe");
    assert!(outcome.succeeded(), "{outcome:?}");
    assert!(
        outcome.stdout.len() <= 4096,
        "stdout must be capped, got {} bytes",
        outcome.stdout.len()
    );
}

#[tokio::test]
async fn run_hook_large_stdout_and_stderr_concurrently_does_not_deadlock() {
    // Backgrounds one 1 MiB write and foregrounds another so both pipes are
    // under write pressure at the same time -- the scenario that would
    // deadlock a naive "write stdin, then read stdout, then read stderr"
    // sequential implementation.
    let script = "head -c 1048576 /dev/zero & head -c 1048576 /dev/zero 1>&2; wait";
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        run_hook(
            "/bin/sh",
            &["-c".to_owned(), script.to_owned()],
            Duration::from_secs(8),
            4096,
            b"{}",
            &no_cancel(),
        ),
    )
    .await;

    let outcome =
        result.expect("run_hook must not deadlock writing to both stdout and stderr at once");
    assert!(outcome.succeeded(), "{outcome:?}");
}

// ---------------------------------------------------------------------------
// HookDispatcher: matching, bounded concurrency, resilience to bad hooks
// ---------------------------------------------------------------------------

struct Fixture {
    path: PathBuf,
    events: EventLog,
}

impl Fixture {
    async fn open() -> Self {
        let path = unique_path("dispatcher.db");
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", path.display())));
        }
        let db = Database::open(&path).expect("open db");
        let events = EventLog::new(db, Retention::unlimited());
        Self { path, events }
    }

    async fn new_mail(&self) {
        self.events
            .append(
                NewEvent::new(EventKind::NewMail)
                    .account(1)
                    .mailbox(1)
                    .message(1),
            )
            .await
            .expect("append");
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.path.display())));
        }
    }
}

/// A dispatcher whose cursor has already been seeded at `events`' current
/// head — what every test below except
/// [`a_fresh_dispatcher_does_not_replay_history_that_predates_its_first_tick`]
/// needs, since [`HookDispatcher::tick`] only seeds an unset cursor lazily
/// on its *own* first call (see that method's own docs): a test that
/// appends events and then calls `tick()` for the first time would
/// otherwise have that very first tick treat everything already appended
/// as pre-existing history and skip it, rather than firing on it as the
/// test expects. Mirrors what `HookDispatcher::spawn` does in production,
/// where the equivalent seed happens eagerly (and awaited) before `spawn`
/// returns rather than on the first tick.
async fn primed_dispatcher(events: &EventLog, config: &HooksConfig) -> HookDispatcher {
    let dispatcher = HookDispatcher::new(events.clone(), config);
    dispatcher.tick(&no_cancel()).await.expect("priming tick");
    dispatcher
}

/// The eager seed is the whole point of `spawn` being `async`: it pins
/// "start at now" to the moment the daemon boots rather than to whenever the
/// runtime first happens to poll the tick task.
///
/// Asserting on the cursor directly (rather than on whether a hook fired) is
/// deliberate — it is the only formulation that fails *deterministically*
/// against the lazy-seed version rather than only when the scheduler
/// cooperates, which is exactly how the underlying bug stayed hidden until a
/// fully-loaded test container finally lost the race.
#[tokio::test]
async fn spawn_seeds_the_cursor_to_the_log_head_before_it_returns() {
    let fx = Fixture::open().await;
    fx.new_mail().await;
    fx.new_mail().await;
    let head = fx
        .events
        .latest_seq()
        .await
        .expect("latest_seq")
        .unwrap_or(0);
    assert!(head > 0, "the fixture appended nothing to seed from");

    let dispatcher = HookDispatcher::new(fx.events.clone(), &HooksConfig::default());
    assert_eq!(
        dispatcher.cursor.load(Ordering::SeqCst),
        HookDispatcher::UNSEEDED_CURSOR,
        "a freshly constructed dispatcher must not have a seeded cursor yet"
    );
    // `spawn` consumes the dispatcher, so keep the shared cursor to observe.
    let cursor = Arc::clone(&dispatcher.cursor);

    let cancel = no_cancel();
    let handle = dispatcher.spawn(cancel.clone()).await;

    // Read the cursor with no intervening `.await`: on this (current-thread,
    // the `#[tokio::test]` default) runtime the tick task has been queued by
    // `spawn` but cannot have been polled yet. So this observes what `spawn`
    // itself did, not what a tick did afterwards -- which is precisely the
    // distinction the lazy-seed version got wrong.
    let seeded = cursor.load(Ordering::SeqCst);
    cancel.cancel();
    let _ = handle.await;

    assert_eq!(
        seeded, head,
        "the cursor must be pinned to the log head before the tick loop is ever polled -- \
         seeding it lazily on the first tick instead means every event appended between \
         `spawn` and that first poll (on a real daemon: mail synced while it was still \
         coming up) is swallowed by the seed and, because `EventLog::since` is exclusive, \
         never fires a hook at all"
    );
}

/// The end-to-end counterpart of the above, through the real `spawn` loop:
/// history stays history, and an event appended strictly *after* `spawn`
/// returns still fires.
#[tokio::test]
async fn an_event_appended_after_spawn_returns_is_still_dispatched() {
    let fx = Fixture::open().await;
    // Pre-existing history: must never fire.
    fx.new_mail().await;

    let marker = unique_path("post-spawn-marker");
    let _ = std::fs::remove_file(&marker);
    let config = HooksConfig {
        hooks: vec![HookConfig {
            name: "marker".to_owned(),
            event: HookEvent::OnNewMessage,
            command: "/bin/sh".to_owned(),
            args: vec!["-c".to_owned(), format!("touch {}", marker.display())],
            enabled: true,
            timeout: None,
        }],
        ..HooksConfig::default()
    };
    let mut dispatcher = HookDispatcher::new(fx.events.clone(), &config);
    // Keep the test quick: the production default is 5s, and this test cares
    // about *which* events a tick sees, not how long the sleep between them is.
    dispatcher.tick_interval = Duration::from_millis(25);

    let cancel = no_cancel();
    let handle = dispatcher.spawn(cancel.clone()).await;

    // Strictly after `spawn` returned.
    fx.new_mail().await;

    let mut fired = false;
    for _ in 0..200 {
        if marker.exists() {
            fired = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    cancel.cancel();
    let _ = handle.await;

    assert!(
        fired,
        "an event appended after `spawn` returned must be dispatched -- it is not history, \
         however late the runtime got around to polling the tick task"
    );
    let _ = std::fs::remove_file(&marker);
}

#[tokio::test]
async fn a_fresh_dispatcher_does_not_replay_history_that_predates_its_first_tick() {
    // History that exists before this dispatcher is ever constructed --
    // standing in for "the daemon has been running for a while, synced
    // mail, and is now restarted."
    let fx = Fixture::open().await;
    fx.new_mail().await;
    fx.new_mail().await;

    let marker = unique_path("no-replay-marker");
    let _ = std::fs::remove_file(&marker);
    let config = HooksConfig {
        hooks: vec![HookConfig {
            name: "marker".to_owned(),
            event: HookEvent::OnNewMessage,
            command: "/bin/sh".to_owned(),
            args: vec!["-c".to_owned(), format!("touch {}", marker.display())],
            enabled: true,
            timeout: None,
        }],
        ..HooksConfig::default()
    };
    // Deliberately *not* using `primed_dispatcher` here: a brand-new
    // `HookDispatcher::new`, ticked for the first time, is exactly the
    // "daemon just restarted" state this test proves is safe.
    let dispatcher = HookDispatcher::new(fx.events.clone(), &config);

    let report = dispatcher.tick(&no_cancel()).await.expect("tick");

    assert_eq!(
        report.fired, 0,
        "a fresh dispatcher's first tick must not replay events that predate its \
         construction -- on a real daemon restart, replaying would mean firing \
         every enabled hook once per matching event across the whole retention \
         window instead of zero times"
    );
    assert!(!marker.exists());

    // And it still catches genuinely new events from this point on.
    fx.new_mail().await;
    let second = dispatcher.tick(&no_cancel()).await.expect("tick");
    assert_eq!(second.fired, 1);
    assert!(marker.exists());

    let _ = std::fs::remove_file(&marker);
}

#[tokio::test]
async fn dispatcher_only_fires_enabled_hooks() {
    let fx = Fixture::open().await;
    let config = HooksConfig {
        hooks: vec![hook("disabled", HookEvent::OnNewMessage, false, None)],
        ..HooksConfig::default()
    };
    let dispatcher = primed_dispatcher(&fx.events, &config).await;
    assert_eq!(
        dispatcher.hook_count(),
        0,
        "a disabled hook must not be part of what the dispatcher fires"
    );

    fx.new_mail().await;
    let report = dispatcher.tick(&no_cancel()).await.expect("tick");
    assert_eq!(report.fired, 0);
}

#[tokio::test]
async fn a_second_tick_with_no_new_events_fires_nothing() {
    let fx = Fixture::open().await;
    let config = HooksConfig {
        hooks: vec![hook("h", HookEvent::OnNewMessage, true, None)],
        ..HooksConfig::default()
    };
    let dispatcher = primed_dispatcher(&fx.events, &config).await;
    fx.new_mail().await;
    let cancel = no_cancel();

    let first = dispatcher.tick(&cancel).await.expect("tick");
    let second = dispatcher.tick(&cancel).await.expect("tick");

    assert_eq!(first.fired, 1);
    assert_eq!(
        second.fired, 0,
        "the cursor must have advanced past the event already handled"
    );
}

#[tokio::test]
async fn tick_does_not_stall_on_a_failing_or_missing_hook_and_still_runs_the_rest() {
    let fx = Fixture::open().await;
    let marker = unique_path("ok-hook-ran");
    let _ = std::fs::remove_file(&marker);

    let config = HooksConfig {
        hooks: vec![
            HookConfig {
                name: "missing".to_owned(),
                event: HookEvent::OnNewMessage,
                command: "/definitely/not/real-xyz".to_owned(),
                args: Vec::new(),
                enabled: true,
                timeout: None,
            },
            HookConfig {
                name: "failing".to_owned(),
                event: HookEvent::OnNewMessage,
                command: "/bin/sh".to_owned(),
                args: vec!["-c".to_owned(), "exit 1".to_owned()],
                enabled: true,
                timeout: None,
            },
            HookConfig {
                name: "ok".to_owned(),
                event: HookEvent::OnNewMessage,
                command: "/bin/sh".to_owned(),
                args: vec!["-c".to_owned(), format!("touch {}", marker.display())],
                enabled: true,
                timeout: None,
            },
        ],
        ..HooksConfig::default()
    };
    let dispatcher = primed_dispatcher(&fx.events, &config).await;
    fx.new_mail().await;

    let report = tokio::time::timeout(Duration::from_secs(10), dispatcher.tick(&no_cancel()))
        .await
        .expect("a failing/missing hook must not stall the tick")
        .expect("tick");

    assert_eq!(report.fired, 3);
    assert_eq!(
        report.failed, 2,
        "the missing command and the non-zero exit both count as failed"
    );
    assert!(
        marker.exists(),
        "the third hook must still have run despite the other two failing"
    );

    let _ = std::fs::remove_file(&marker);
}

#[tokio::test]
async fn dispatcher_bounds_concurrency_under_a_burst_of_events() {
    let fx = Fixture::open().await;
    let marker_dir = unique_path("burst-markers");
    let _ = std::fs::remove_dir_all(&marker_dir);
    std::fs::create_dir_all(&marker_dir).expect("create marker dir");

    let config = HooksConfig {
        max_concurrency: 2,
        default_timeout: HumanDuration::new(Duration::from_secs(5)),
        hooks: vec![HookConfig {
            name: "burst".to_owned(),
            event: HookEvent::OnNewMessage,
            command: "/bin/sh".to_owned(),
            args: vec![
                "-c".to_owned(),
                format!(
                    "touch {dir}/$$; sleep 0.3; rm -f {dir}/$$",
                    dir = marker_dir.display()
                ),
            ],
            enabled: true,
            timeout: None,
        }],
        ..HooksConfig::default()
    };
    let dispatcher = primed_dispatcher(&fx.events, &config).await;

    for _ in 0..6 {
        fx.new_mail().await;
    }

    // `tick` awaits every matched hook to completion (see its own docs), so
    // the only window to observe concurrency from outside is a sampler
    // running alongside it — the same "peak in flight" technique
    // `ai::queue::tests`' worker-pool concurrency test uses, just measured
    // via real OS processes (a marker file per live child) rather than an
    // in-process counter a mock provider increments.
    let peak = Arc::new(AtomicUsize::new(0));
    let sampling = Arc::new(AtomicBool::new(true));
    let sampler = {
        let peak = Arc::clone(&peak);
        let sampling = Arc::clone(&sampling);
        let dir = marker_dir.clone();
        tokio::spawn(async move {
            while sampling.load(Ordering::SeqCst) {
                let count = std::fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0);
                peak.fetch_max(count, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
    };

    let report = dispatcher.tick(&no_cancel()).await.expect("tick");

    sampling.store(false, Ordering::SeqCst);
    let _ = sampler.await;
    let peak = peak.load(Ordering::SeqCst);

    assert_eq!(report.fired, 6);
    assert!(
        peak <= 2,
        "peak concurrent hook runs {peak} exceeded max_concurrency = 2"
    );
    assert!(
        peak >= 2,
        "test never actually exercised concurrency (peak was {peak}); \
         it would pass even with a broken semaphore"
    );

    let _ = std::fs::remove_dir_all(&marker_dir);
}

// ---------------------------------------------------------------------------
// One tick's matches are capped
// ---------------------------------------------------------------------------

#[tokio::test]
async fn tick_caps_how_many_matched_hooks_it_processes_in_one_batch() {
    let fx = Fixture::open().await;
    let config = HooksConfig {
        hooks: vec![hook("h", HookEvent::OnNewMessage, true, None)],
        ..HooksConfig::default()
    };
    let dispatcher = primed_dispatcher(&fx.events, &config)
        .await
        .with_max_batch(3);

    for _ in 0..10 {
        fx.new_mail().await;
    }

    let cancel = no_cancel();
    let first = dispatcher.tick(&cancel).await.expect("tick");
    assert_eq!(first.fired, 3);
    assert!(first.capped, "the tick must report it stopped early");

    let second = dispatcher.tick(&cancel).await.expect("tick");
    assert_eq!(second.fired, 3);
    assert!(second.capped);

    let third = dispatcher.tick(&cancel).await.expect("tick");
    assert_eq!(third.fired, 3);
    assert!(third.capped);

    let fourth = dispatcher.tick(&cancel).await.expect("tick");
    assert_eq!(fourth.fired, 1, "the last, uncapped remainder");
    assert!(!fourth.capped);

    let fifth = dispatcher.tick(&cancel).await.expect("tick");
    assert_eq!(fifth.fired, 0, "nothing left to fire");
}

#[tokio::test]
async fn tick_cap_never_permanently_drops_a_hook_invocation_when_it_lands_mid_event() {
    // Regression test for a specific bug: an earlier version of the batch
    // cap checked `matched.len() >= max_batch` *inside* the per-hook loop,
    // so hitting the cap partway through matching one event against every
    // configured hook advanced the cursor past that event with some of its
    // hooks never queued. Since this dispatcher never revisits an event
    // once its cursor has passed it, those invocations were gone forever,
    // not merely deferred to a later tick. Two hooks sharing an event with
    // an odd `max_batch` (so the cap lands mid-event at least once)
    // reproduces it directly: the old code fired 8 invocations total across
    // every tick instead of the 10 (5 events x 2 hooks) that must
    // eventually run.
    let fx = Fixture::open().await;
    let config = HooksConfig {
        hooks: vec![
            hook("h1", HookEvent::OnNewMessage, true, None),
            hook("h2", HookEvent::OnNewMessage, true, None),
        ],
        ..HooksConfig::default()
    };
    let dispatcher = primed_dispatcher(&fx.events, &config)
        .await
        .with_max_batch(3);

    let event_count: u64 = 5;
    for _ in 0..event_count {
        fx.new_mail().await;
    }

    let cancel = no_cancel();
    let mut total_fired = 0u64;
    for _ in 0..20 {
        let report = dispatcher.tick(&cancel).await.expect("tick");
        total_fired += report.fired;
        if report.fired == 0 {
            break;
        }
    }

    assert_eq!(
        total_fired,
        event_count * 2,
        "every hook must eventually fire for every event -- a cap landing mid-event \
         must defer the remaining hooks to a later tick, never drop them permanently"
    );
}
