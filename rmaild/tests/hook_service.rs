//! Integration test: drive `HookService` end-to-end against an in-process
//! tonic server over a Unix domain socket — `ListHooks` (including disabled
//! hooks), `TestHook` (a synthetic sample event, a caller-supplied
//! `event_json`, malformed JSON, an unknown hook name, a timeout), and a
//! real-daemon-boot proof that `HookDispatcher::spawn` is actually wired
//! into `serve_uds_with_engine_and_mail_store` — the same "prove the
//! wiring, not just the type in isolation" test `ai_service.rs`'s own
//! module docs describe for the AI dispatch loop.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rmail_core::config::{HookConfig, HookEvent, HooksConfig, HumanDuration};
use rmail_core::events::{EventKind, EventLog, NewEvent, Retention};
use rmail_core::sync::{SyncEngine, SyncOptions};
use rmail_core::{Config, Database};
use rmail_proto::v1::hook_service_client::HookServiceClient;
use rmail_proto::v1::{HookEvent as ProtoHookEvent, ListHooksRequest, TestHookRequest};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tonic::transport::Channel;
use tonic::Code;

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn unique_path(label: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    std::env::temp_dir().join(format!("rmail-hooksvc-{label}-{pid}-{n}"))
}

struct TestServer {
    socket: PathBuf,
    db_path: PathBuf,
    shutdown: oneshot::Sender<()>,
    handle: JoinHandle<Result<(), rmaild::ServeError>>,
}

impl TestServer {
    /// A server whose `hooks.hooks` are exactly `hooks`, every other
    /// `[hooks]` setting at its default — AI and semantic indexing
    /// disabled, since this suite has nothing to do with either and both
    /// would otherwise add unrelated background work (an AI dispatch tick,
    /// a model warm-up) to a test whose only concern is
    /// `HookService`/`HookDispatcher`.
    async fn start(hooks: Vec<HookConfig>) -> Self {
        Self::start_with_hooks_config(HooksConfig {
            hooks,
            ..HooksConfig::default()
        })
        .await
    }

    /// As [`Self::start`], but with the full `[hooks]` table under the
    /// caller's control — for tests that need a non-default
    /// `max_concurrency`/`default_timeout`/etc., not just the hook list.
    async fn start_with_hooks_config(hooks_config: HooksConfig) -> Self {
        let socket = unique_path("sock");
        let db_path = unique_path("db");
        let db = Database::open(&db_path).expect("open db");

        let mut config = Config::default();
        config.index.semantic.enabled = false;
        config.ai.enabled = false;
        config.hooks = hooks_config;

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let server_socket = socket.clone();
        let server_db = db.clone();
        let handle = tokio::spawn(async move {
            rmaild::serve_uds_with_config(&server_socket, server_db, config, async move {
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

    async fn client(&self) -> HookServiceClient<Channel> {
        HookServiceClient::new(rmail_core::connect_uds(&self.socket).await.unwrap())
    }

    async fn shutdown(self) {
        let _ = self.shutdown.send(());
        let _ = tokio::time::timeout(Duration::from_secs(10), self.handle).await;
        for suffix in ["", "-wal", "-shm"] {
            let _ =
                std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.db_path.display())));
        }
        let _ = std::fs::remove_file(&self.socket);
    }
}

fn hook(
    name: &str,
    event: HookEvent,
    command: &str,
    args: Vec<String>,
    enabled: bool,
) -> HookConfig {
    HookConfig {
        name: name.to_owned(),
        event,
        command: command.to_owned(),
        args,
        enabled,
        timeout: None,
    }
}

// ---------------------------------------------------------------------------
// ListHooks
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_hooks_returns_every_configured_hook_including_disabled() {
    let server = TestServer::start(vec![
        hook(
            "enabled-one",
            HookEvent::OnNewMessage,
            "/bin/true",
            vec![],
            true,
        ),
        hook(
            "disabled-one",
            HookEvent::OnMove,
            "/bin/false",
            vec![],
            false,
        ),
    ])
    .await;

    let response = server
        .client()
        .await
        .list_hooks(ListHooksRequest {})
        .await
        .unwrap()
        .into_inner();

    assert_eq!(response.hooks.len(), 2);
    let disabled = response
        .hooks
        .iter()
        .find(|h| h.name == "disabled-one")
        .expect("the disabled hook must still be listed");
    assert!(!disabled.enabled);
    assert_eq!(
        ProtoHookEvent::try_from(disabled.event).unwrap(),
        ProtoHookEvent::OnMove
    );
    let enabled = response
        .hooks
        .iter()
        .find(|h| h.name == "enabled-one")
        .expect("the enabled hook must be listed");
    assert!(enabled.enabled);
    assert_eq!(enabled.command, "/bin/true");

    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// TestHook: sample event vs. caller-supplied event_json
// ---------------------------------------------------------------------------

#[tokio::test]
async fn test_hook_runs_against_a_synthetic_sample_event_when_none_is_supplied() {
    let out_file = unique_path("sample.out");
    let _ = std::fs::remove_file(&out_file);
    let server = TestServer::start(vec![hook(
        "echo",
        HookEvent::OnNewMessage,
        "/bin/sh",
        vec!["-c".to_owned(), format!("cat > {}", out_file.display())],
        true,
    )])
    .await;

    let response = server
        .client()
        .await
        .test_hook(TestHookRequest {
            name: "echo".to_owned(),
            event_json: None,
        })
        .await
        .unwrap()
        .into_inner();

    assert!(!response.timed_out);
    assert_eq!(response.exit_code, Some(0));
    let written = std::fs::read_to_string(&out_file).expect("hook must have written its stdin");
    assert!(
        written.contains("\"test\":true"),
        "the synthetic sample must carry the test marker: {written}"
    );

    let _ = std::fs::remove_file(&out_file);
    server.shutdown().await;
}

#[tokio::test]
async fn test_hook_pipes_caller_supplied_event_json_verbatim_and_never_executes_it() {
    let out_file = unique_path("rpc-stdin.out");
    let pwned_file = unique_path("rpc-pwned");
    let _ = std::fs::remove_file(&out_file);
    let _ = std::fs::remove_file(&pwned_file);

    let server = TestServer::start(vec![hook(
        "echo",
        HookEvent::OnNewMessage,
        "/bin/sh",
        vec!["-c".to_owned(), format!("cat > {}", out_file.display())],
        true,
    )])
    .await;

    // A metacharacter-laden payload arriving through the RPC field itself,
    // not just `run_hook` in isolation — the acceptance criterion is that
    // shell metacharacters in an attacker-influenced subject cannot affect
    // what runs, proven here at the actual gRPC boundary a real MCP/CLI
    // caller would use.
    let malicious = format!(
        r#"{{"subject":"hi\"; touch {}; echo pwned #"}}"#,
        pwned_file.display()
    );

    let response = server
        .client()
        .await
        .test_hook(TestHookRequest {
            name: "echo".to_owned(),
            event_json: Some(malicious.clone()),
        })
        .await
        .unwrap()
        .into_inner();

    assert_eq!(response.exit_code, Some(0));
    let written = std::fs::read_to_string(&out_file).unwrap();
    assert_eq!(
        written, malicious,
        "the exact event_json must reach the hook's stdin, unmodified"
    );
    assert!(
        !pwned_file.exists(),
        "shell metacharacters in event_json must never be executed"
    );

    let _ = std::fs::remove_file(&out_file);
    server.shutdown().await;
}

#[tokio::test]
async fn test_hook_rejects_malformed_event_json() {
    let server = TestServer::start(vec![hook(
        "echo",
        HookEvent::OnNewMessage,
        "/bin/true",
        vec![],
        true,
    )])
    .await;

    let status = server
        .client()
        .await
        .test_hook(TestHookRequest {
            name: "echo".to_owned(),
            event_json: Some("not valid json {".to_owned()),
        })
        .await
        .expect_err("malformed JSON must be rejected before it ever reaches the hook");

    assert_eq!(status.code(), Code::InvalidArgument);
    server.shutdown().await;
}

#[tokio::test]
async fn test_hook_on_an_unknown_name_is_not_found() {
    let server = TestServer::start(vec![]).await;

    let status = server
        .client()
        .await
        .test_hook(TestHookRequest {
            name: "does-not-exist".to_owned(),
            event_json: None,
        })
        .await
        .expect_err("no such hook is configured");

    assert_eq!(status.code(), Code::NotFound);
    server.shutdown().await;
}

#[tokio::test]
async fn test_hook_reports_a_timeout() {
    let server = TestServer::start(vec![HookConfig {
        name: "slow".to_owned(),
        event: HookEvent::OnNewMessage,
        command: "/bin/sleep".to_owned(),
        args: vec!["30".to_owned()],
        enabled: true,
        timeout: Some(HumanDuration::new(Duration::from_millis(200))),
    }])
    .await;

    let response = server
        .client()
        .await
        .test_hook(TestHookRequest {
            name: "slow".to_owned(),
            event_json: None,
        })
        .await
        .unwrap()
        .into_inner();

    assert!(response.timed_out);
    assert!(!response.cancelled);
    assert_eq!(response.exit_code, None);

    server.shutdown().await;
}

#[tokio::test]
async fn test_hook_concurrency_is_bounded_by_the_configured_semaphore() {
    // What this proves: a burst of concurrent `TestHook` calls is bounded
    // by whatever semaphore `HookApi` was constructed with -- the same
    // "peak in flight" technique `rmail-core::hooks::tests`'
    // `dispatcher_bounds_concurrency_under_a_burst_of_events` uses for the
    // dispatcher itself, a marker file per live child sampled concurrently
    // with the run.
    //
    // What this does *not* independently prove: that the semaphore is the
    // *same Arc instance* `HookDispatcher` uses for real dispatch (as
    // opposed to `HookApi` minting an identically-sized but independent
    // one) -- a black-box RPC test cannot distinguish those two without a
    // slow, ~5s-tick-interval end-to-end run mixing real dispatched events
    // with concurrent `TestHook` calls. That sharing is instead verified
    // directly at the wiring site: `rmaild::serve_uds_with_engine_and_mail_store`
    // calls `hook_dispatcher.semaphore()` -- an `Arc::clone` of the exact
    // instance `hook_dispatcher.spawn(...)` then consumes -- and hands that
    // clone to `HookApi::new` (see `hook_service.rs`'s own module docs and
    // `rmaild/src/lib.rs`'s hook-wiring block).
    let marker_dir = unique_path("testhook-concurrency-markers");
    let _ = std::fs::remove_dir_all(&marker_dir);
    std::fs::create_dir_all(&marker_dir).expect("create marker dir");

    let server = TestServer::start_with_hooks_config(HooksConfig {
        max_concurrency: 1,
        default_timeout: HumanDuration::new(Duration::from_secs(5)),
        hooks: vec![HookConfig {
            name: "slow".to_owned(),
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
    })
    .await;

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

    let mut runs = Vec::new();
    for _ in 0..4 {
        let mut client = server.client().await;
        runs.push(tokio::spawn(async move {
            client
                .test_hook(TestHookRequest {
                    name: "slow".to_owned(),
                    event_json: None,
                })
                .await
        }));
    }
    for run in runs {
        let response = run
            .await
            .expect("task join")
            .expect("TestHook RPC")
            .into_inner();
        assert_eq!(response.exit_code, Some(0));
    }

    sampling.store(false, Ordering::SeqCst);
    let _ = sampler.await;
    let peak = peak.load(Ordering::SeqCst);

    assert!(
        peak <= 1,
        "peak concurrent TestHook runs {peak} exceeded max_concurrency = 1"
    );
    assert!(
        peak >= 1,
        "test never actually exercised concurrency (peak was {peak}); \
         it would pass even with a broken semaphore"
    );

    server.shutdown().await;
}

// ---------------------------------------------------------------------------
// The real daemon boot wires the dispatcher, not just HookApi in isolation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_real_daemon_boot_wires_the_hook_dispatcher_end_to_end() {
    let socket = unique_path("daemon.sock");
    let db_path = unique_path("daemon.db");
    let marker = unique_path("daemon-marker");
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_file(&marker);

    let db = Database::open(&db_path).expect("open db");
    let events = EventLog::new(db.clone(), Retention::unlimited());
    let engine = SyncEngine::new(db.clone(), events.clone(), SyncOptions::default());

    let mut config = Config::default();
    config.index.semantic.enabled = false;
    config.ai.enabled = false;
    // The production default is 5s, which left this test's fixed 12s budget
    // room for only two ticks -- on a loaded machine (the full suite in one
    // container) a `sleep(5s)` overruns far enough that the budget can elapse
    // having contained only the boot tick, and the test failed for reasons
    // that had nothing to do with the wiring it exists to prove. Ticking
    // quickly here decouples "is the dispatcher wired in" from "did the
    // scheduler hit a 5s deadline on time".
    config.hooks.tick_interval = HumanDuration::new(Duration::from_millis(50));
    config.hooks.hooks = vec![HookConfig {
        name: "marker".to_owned(),
        event: HookEvent::OnNewMessage,
        command: "/bin/sh".to_owned(),
        args: vec!["-c".to_owned(), format!("touch {}", marker.display())],
        enabled: true,
        timeout: None,
    }];

    // Kept for the failure diagnostics below: `config` itself is moved into
    // the daemon task.
    let hooks_cfg = config.hooks.clone();

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let server_socket = socket.clone();
    let server_db = db.clone();
    let handle = tokio::spawn(async move {
        rmaild::serve_uds_with_engine(&server_socket, server_db, engine, &config, async move {
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
    assert!(ready, "the real daemon never became ready");

    // The same event `sync::engine::LogSink` appends the moment a message
    // lands -- this is "sync a message" for the purposes of this test.
    events
        .append(
            NewEvent::new(EventKind::NewMail)
                .account(1)
                .mailbox(1)
                .message(1),
        )
        .await
        .unwrap();

    // The dispatch loop ticks once immediately on spawn and then every
    // `DEFAULT_TICK_INTERVAL` (5s) after -- poll well past that so an event
    // appended just after the first tick is still caught by the second.
    // Wait against a wall-clock deadline rather than an iteration count: on a
    // contended machine a `sleep(100ms)` overruns, so a fixed 120 iterations
    // is not the 12s it looks like.
    //
    // And re-append periodically rather than betting everything on the first
    // event. `HookDispatcher` deliberately does not retry a hook that failed
    // to *spawn* -- there is no idempotency key for "ran an operator's shell
    // command", so one attempt per event is the documented design (see the
    // `hooks` module docs). That is correct behaviour, but it means a single
    // `fork`/`exec` losing to memory pressure would fail this test for a
    // reason that has nothing to do with the wiring it exists to prove. Each
    // fresh event is a fresh dispatch, so the assertion below is "the daemon
    // boot dispatches NewMail to hooks", not "one fork succeeded first try".
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    let mut found = false;
    let mut appended = 1_u32;
    let mut next_append = std::time::Instant::now() + Duration::from_secs(3);
    while std::time::Instant::now() < deadline {
        if marker.exists() {
            found = true;
            break;
        }
        if std::time::Instant::now() >= next_append {
            events
                .append(
                    NewEvent::new(EventKind::NewMail)
                        .account(1)
                        .mailbox(1)
                        .message(1),
                )
                .await
                .unwrap();
            appended += 1;
            next_append = std::time::Instant::now() + Duration::from_secs(3);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Diagnostics captured only on failure, to tell the plausible causes apart
    // rather than guessing: is the event even in the log, and would a
    // dispatcher built the same way fire on it right now?
    let diagnosis = if found {
        String::new()
    } else {
        let page = events.since(0, 64).await;
        let in_log = match &page {
            Ok(p) => format!("{} event(s) readable from seq 0", p.events.len()),
            Err(e) => format!("event log unreadable: {e}"),
        };
        let probe = rmail_core::hooks::HookDispatcher::new(events.clone(), &hooks_cfg);
        let probe_marker = marker.exists();
        let ticked = probe
            .tick(&tokio_util::sync::CancellationToken::new())
            .await;
        let fired = match ticked {
            // A fresh dispatcher seeds its cursor lazily on this first tick,
            // so it reports 0 fired even on a healthy log -- what matters is
            // whether it errored.
            Ok(r) => format!("probe tick ok (fired={}, capped={})", r.fired, r.capped),
            Err(e) => format!("probe tick FAILED: {e}"),
        };
        format!(
            " -- diagnostics: {in_log}; marker before probe={probe_marker}; {fired}; \
             hooks.enabled={}, hook count={}, events appended={appended}",
            hooks_cfg.enabled,
            hooks_cfg.hooks.len()
        )
    };

    assert!(
        found,
        "the real daemon boot must run the configured hook for a synced NewMail event -- if \
         this fails, HookDispatcher::spawn was likely removed from \
         serve_uds_with_engine_and_mail_store{diagnosis}"
    );

    let _ = shutdown_tx.send(());
    let _ = tokio::time::timeout(Duration::from_secs(10), handle).await;
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", db_path.display())));
    }
    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_file(&marker);
}
