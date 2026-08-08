//! Integration test: drive the compiled `mail index` / `mail entities` verbs
//! against a real in-process daemon, over the socket, exactly as an operator
//! would.
//!
//! `rmaild/tests/index_service.rs` covers the *service*. This covers the
//! *binary* — argument shapes, the output an operator reads, and the fact that
//! each verb reaches the RPC it claims to. The two are not interchangeable: a
//! CLI can be wired to the wrong method, print the wrong field, or refuse a
//! flag combination the daemon would have accepted, and nothing on the service
//! side would notice.
//!
//! Everything runs against one daemon, in one test, on purpose. Booting a
//! daemon per verb would multiply the slowest part of this suite by ten to
//! check ten things that share all their setup.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use rmail_core::repo;
use tokio::process::Command;
use tokio::sync::oneshot;

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn unique_path(label: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    std::env::temp_dir().join(format!("rmail-cli-index-{label}-{pid}-{n}"))
}

struct Daemon {
    socket: PathBuf,
    db_path: PathBuf,
    db: rmail_core::Database,
    shutdown: oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<Result<(), rmaild::ServeError>>,
}

impl Daemon {
    async fn start() -> Self {
        let socket = unique_path("sock");
        let db_path = unique_path("db");
        let db = rmail_core::Database::open(&db_path).expect("open db");

        let mut config = rmail_core::Config::default();
        // The convention every daemon-booting suite in this workspace follows:
        // a real ONNX backend would load — or download — a few hundred
        // megabytes of weights per test process.
        config.index.semantic.enabled = false;
        config.ai.enabled = false;
        // The background worker off, so what this test observes is what the
        // *CLI* asked for rather than a timer racing it.
        config.index.enabled = false;

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
        assert!(ready, "daemon never became ready");

        Self {
            socket,
            db_path,
            db,
            shutdown: shutdown_tx,
            handle,
        }
    }

    /// Seed `count` messages with entity-bearing bodies.
    async fn seed(&self, count: i64) {
        self.db
            .write(move |c| {
                let account_id = repo::insert_account(
                    c,
                    &repo::NewAccount {
                        name: "Personal".to_owned(),
                        ..Default::default()
                    },
                )?;
                let mailbox_id = repo::insert_mailbox(
                    c,
                    &repo::NewMailbox {
                        account_id,
                        name: "INBOX".to_owned(),
                        ..Default::default()
                    },
                )?;
                for uid in 1..=count {
                    repo::insert_message(
                        c,
                        &repo::NewMessage {
                            account_id,
                            mailbox_id,
                            uid,
                            uidvalidity: 1,
                            subject: Some(format!("Invoice {uid}")),
                            from_addr: Some("ada@example.com".to_owned()),
                            body_text: Some(
                                "Invoice INV-2024-0231, questions to ada@example.com.".to_owned(),
                            ),
                            date: Some(1_700_000_000 + uid),
                            internaldate: Some(1_700_000_000 + uid),
                            ..Default::default()
                        },
                    )?;
                }
                Ok(())
            })
            .await
            .unwrap();
    }

    /// Run `mail <args>` against this daemon and return its stdout, asserting
    /// it succeeded.
    async fn ok(&self, args: &[&str]) -> String {
        let output = Command::new(env!("CARGO_BIN_EXE_mail"))
            .args(args)
            .env(rmail_core::SOCKET_ENV, &self.socket)
            .output()
            .await
            .unwrap_or_else(|e| panic!("running `mail {}`: {e}", args.join(" ")));
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        assert!(
            output.status.success(),
            "`mail {}` failed: stdout={stdout} stderr={}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        stdout
    }

    /// Run `mail <args>` expecting a non-zero exit, and return stderr.
    async fn fails(&self, args: &[&str]) -> String {
        let output = Command::new(env!("CARGO_BIN_EXE_mail"))
            .args(args)
            .env(rmail_core::SOCKET_ENV, &self.socket)
            .output()
            .await
            .unwrap_or_else(|e| panic!("running `mail {}`: {e}", args.join(" ")));
        assert!(
            !output.status.success(),
            "`mail {}` should have failed but printed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout)
        );
        String::from_utf8_lossy(&output.stderr).into_owned()
    }

    fn count(&self, table: &str) -> i64 {
        let sql = format!("SELECT count(*) FROM {table}");
        self.db
            .with_read(move |c| c.query_row(&sql, [], |r| r.get(0)))
            .unwrap()
    }

    async fn stop(self) {
        let _ = self.shutdown.send(());
        let _ = tokio::time::timeout(Duration::from_secs(30), self.handle).await;
        let _ = std::fs::remove_file(&self.socket);
        for suffix in ["", "-wal", "-shm"] {
            let _ =
                std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.db_path.display())));
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn every_index_verb_reaches_the_daemon_and_reports_what_it_did() {
    let daemon = Daemon::start().await;
    daemon.seed(3).await;

    // status, on a store nothing has indexed. The coverage denominator is the
    // message count, so this is 0.0% rather than a vacuous 100%.
    let status = daemon.ok(&["index", "status"]).await;
    assert!(status.contains("Messages      3"), "{status}");
    assert!(status.contains("lexical 0.0%"), "{status}");
    assert!(
        status.contains("semantic off"),
        "a switched-off stage says so rather than showing 0%: {status}"
    );
    assert!(
        status.contains("worker stopped"),
        "index.enabled = false, so the background worker is not running: {status}"
    );

    // reindex: enqueue the stale work and drain it.
    let reindexed = daemon.ok(&["index", "reindex"]).await;
    assert!(reindexed.contains("0 queued"), "{reindexed}");
    assert!(reindexed.contains("0 failed"), "{reindexed}");
    assert_eq!(daemon.count("fts_messages"), 3, "the drain really ran");

    let status = daemon.ok(&["index", "status"]).await;
    assert!(status.contains("lexical 100.0%"), "{status}");
    assert!(status.contains("lexical 0s"), "and caught up: {status}");

    // run: a bare drain over an empty queue is a no-op that still reports.
    let ran = daemon.ok(&["index", "run"]).await;
    assert!(ran.contains("0 done"), "{ran}");

    // verify: clean, and it says so in one line rather than a table of zeroes.
    let verified = daemon.ok(&["index", "verify"]).await;
    assert_eq!(verified.trim(), "index clean");

    // gc: nothing orphaned yet.
    let collected = daemon.ok(&["index", "gc"]).await;
    assert_eq!(collected.trim(), "nothing to collect");

    // embed --backfill: no chunks exist (semantic is off), so there is nothing
    // to re-embed and the pass is a clean no-op.
    let embedded = daemon.ok(&["index", "embed", "--backfill"]).await;
    assert!(embedded.contains("0 done"), "{embedded}");

    // entities: the extractor found the invoice id and the address.
    let entities = daemon.ok(&["entities", "invoice_id"]).await;
    assert!(
        entities.contains("INV-2024-0231"),
        "an invoice reference normalizes to upper case, unlike an address: {entities}"
    );
    // The value filter is case-insensitive against norms that are not
    // consistently cased — this one is stored upper, and is typed lower.
    let filtered = daemon
        .ok(&["entities", "invoice_id", "--value", "inv-2024"])
        .await;
    assert!(filtered.contains("INV-2024-0231"), "{filtered}");
    let emails = daemon.ok(&["entities", "email", "--value", "ADA"]).await;
    assert!(
        emails.contains("ada@example.com"),
        "the --value filter folds case: {emails}"
    );
    assert!(
        emails.contains("3 messages"),
        "and the mention counts are real: {emails}"
    );

    // stop / start last, so the worker this turns on cannot race the drains
    // above. `index.enabled = false` starts it paused, so the first `stop` is a
    // no-op that still has to report the truth.
    let stopped = daemon.ok(&["index", "stop"]).await;
    assert!(stopped.contains("stopped"), "{stopped}");
    let started = daemon.ok(&["index", "start"]).await;
    assert!(started.contains("running"), "{started}");
    let status = daemon.ok(&["index", "status"]).await;
    assert!(
        status.contains("worker running"),
        "and status agrees with the switch: {status}"
    );

    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_destructive_verbs_refuse_before_they_reach_the_daemon() {
    let daemon = Daemon::start().await;
    daemon.seed(2).await;
    daemon.ok(&["index", "reindex"]).await;
    let entities = daemon.count("entities");
    assert!(entities > 0);

    // No --all, no --kind: nothing to say what to destroy.
    let err = daemon.fails(&["index", "rebuild"]).await;
    assert!(err.contains("--all"), "{err}");

    // Both, which contradict each other.
    let err = daemon
        .fails(&["index", "rebuild", "--all", "--kind", "lexical"])
        .await;
    assert!(err.contains("contradict"), "{err}");

    // No --yes and no terminal to ask on — a CI job's exact situation.
    let err = daemon.fails(&["index", "rebuild", "--all"]).await;
    assert!(err.contains("--yes"), "{err}");

    assert_eq!(
        daemon.count("entities"),
        entities,
        "not one of those refusals deleted anything"
    );
    assert_eq!(daemon.count("fts_messages"), 2);

    // And an unknown entity kind is rejected by the daemon rather than
    // answered with an empty page.
    let err = daemon.fails(&["entities", "not_a_kind"]).await;
    assert!(err.contains("not_a_kind"), "{err}");

    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rebuild_with_yes_wipes_the_named_stage_and_recomputes_it() {
    let daemon = Daemon::start().await;
    daemon.seed(2).await;
    daemon.ok(&["index", "reindex"]).await;
    assert_eq!(daemon.count("fts_messages"), 2);
    let entities = daemon.count("entities");

    let out = daemon
        .ok(&["index", "rebuild", "--kind", "lexical", "--yes"])
        .await;
    assert!(out.contains("dropped"), "the wipe is reported: {out}");
    assert!(out.contains("0 queued"), "and the drain finished: {out}");

    assert_eq!(
        daemon.count("fts_messages"),
        2,
        "the lexical index was rebuilt, not merely emptied"
    );
    assert_eq!(
        daemon.count("entities"),
        entities,
        "and the stage that was not named is untouched"
    );

    daemon.stop().await;
}
