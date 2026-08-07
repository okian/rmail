//! Integration test: drive the compiled `mail search`/`mail similar`
//! subcommands end-to-end against an in-process daemon (task 33's
//! `SearchService`, reached exactly the way a real user would — over the
//! Unix socket, through the built `mail` binary via `CARGO_BIN_EXE_mail`).
//!
//! `rmail-cli/src/search_cli.rs`'s own unit tests already prove the
//! `--json` schema shape and the terminal-sanitization logic in isolation,
//! with no daemon involved (this crate has no lib target, so that is the
//! only place those pure functions can be unit tested at all). What this
//! suite proves instead is everything that only means something once the
//! compiled binary is actually parsing flags, opening a real gRPC
//! connection, and printing to a real stdout: CLI flags reaching
//! `SearchRequest`'s corresponding fields, the `--json` output actually
//! being valid newline-delimited JSON with the documented keys, and the
//! human-readable path rendering something a person can read.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::cell::Cell;
use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::Output;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rmail_core::embed::hash::HashEmbedder;
use rmail_core::embed::Embedder;
use rmail_core::index::fts::FtsIndex;
use rmail_core::index::semantic::{SemanticIndex, VECTOR_DIM};
use rmail_core::index::{extract_message, IndexQueue, QueueOptions, PRIORITY_NORMAL};
use rmail_core::{config::IndexSemanticConfig, repo, Config, Database};
use serde_json::Value;
use tokio::process::Command;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

static COUNTER: AtomicU32 = AtomicU32::new(0);

// ---------------------------------------------------------------------------
// Test harness
// ---------------------------------------------------------------------------

struct TestServer {
    socket: PathBuf,
    db_path: PathBuf,
    db: Database,
    fts: FtsIndex,
    queue: IndexQueue,
    account_id: i64,
    mailbox_id: i64,
    next_uid: Cell<i64>,
    shutdown: oneshot::Sender<()>,
    handle: JoinHandle<Result<(), rmaild::ServeError>>,
}

impl TestServer {
    /// Boot a daemon over a fresh, empty database. Semantic *indexing*
    /// stays off (the config default a bare `Config::default()` would also
    /// give, made explicit here) — tests that need a dense candidate embed
    /// straight into `vec_chunks` themselves via [`Self::embed`], the same
    /// precedent `rmaild/tests/search_service.rs`'s own
    /// `semantic_search_returns_only_dense_sourced_hits` sets, rather than
    /// paying to load (or, cold, download) a real ONNX model just to prove
    /// a CLI flag reaches the wire.
    async fn start() -> Self {
        let mut config = Config::default();
        config.index.semantic.enabled = false;

        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let socket = PathBuf::from("/tmp").join(format!("rmail-cli-search-{pid}-{n}.sock"));
        let db_path = std::env::temp_dir().join(format!("rmail-cli-search-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", db_path.display())));
        }
        let db = Database::open(&db_path).expect("open db");

        let (account_id, mailbox_id) = db
            .with_write(move |c| {
                let account_id = repo::insert_account(
                    c,
                    &repo::NewAccount {
                        name: format!("Personal-{n}"),
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
                Ok((account_id, mailbox_id))
            })
            .expect("seed account/mailbox");

        let fts = FtsIndex::new(db.clone(), config.search.bm25_weights.clone());
        let queue = IndexQueue::new(db.clone(), QueueOptions::default());

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
            fts,
            queue,
            account_id,
            mailbox_id,
            next_uid: Cell::new(1),
            shutdown: shutdown_tx,
            handle,
        }
    }

    /// A second account + mailbox on the same daemon, for the `--account`
    /// filter test.
    async fn insert_account(&self, name: &str) -> (i64, i64) {
        let name = name.to_owned();
        self.db
            .with_write(move |c| {
                let account_id = repo::insert_account(
                    c,
                    &repo::NewAccount {
                        name,
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
                Ok((account_id, mailbox_id))
            })
            .expect("seed second account/mailbox")
    }

    /// Insert, extract, and lexically index a message — mirrors
    /// `rmaild/tests/search_service.rs`'s identical helper, the established
    /// pattern for seeding FTS-searchable content in this workspace.
    async fn index(&self, new: repo::NewMessage) -> i64 {
        let uid = self.next_uid.get();
        self.next_uid.set(uid + 1);
        let account_id = if new.account_id != 0 {
            new.account_id
        } else {
            self.account_id
        };
        let mailbox_id = if new.mailbox_id != 0 {
            new.mailbox_id
        } else {
            self.mailbox_id
        };
        let new = repo::NewMessage {
            account_id,
            mailbox_id,
            uid,
            uidvalidity: 1,
            ..new
        };
        let message_id = self
            .db
            .with_write(move |c| repo::insert_message(c, &new))
            .expect("insert message");
        extract_message(&self.db, &self.queue, message_id, PRIORITY_NORMAL)
            .await
            .expect("extract message");
        self.fts.index_message(message_id).await.expect("fts index");
        message_id
    }

    /// Embed an already-indexed message's content directly into
    /// `vec_chunks`, over the same deterministic fallback embedder
    /// (`embed::hash::HashEmbedder`, matching `VECTOR_DIM`) the daemon's own
    /// `SearchApi` builds when `index.semantic.enabled = false` — see
    /// `Self::start`'s doc comment.
    async fn embed(&self, message_id: i64) {
        let embedder: Arc<dyn Embedder> = Arc::new(HashEmbedder::new(VECTOR_DIM));
        let semantic_index =
            SemanticIndex::new(self.db.clone(), embedder, &IndexSemanticConfig::default());
        semantic_index
            .index_message(message_id)
            .await
            .expect("semantic index message");
    }

    /// Run the compiled `mail` binary against this daemon and wait for it to
    /// exit.
    async fn run<I, S>(&self, args: I) -> Output
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Command::new(env!("CARGO_BIN_EXE_mail"))
            .args(args)
            .env(rmail_core::SOCKET_ENV, &self.socket)
            .output()
            .await
            .expect("run mail binary")
    }

    async fn stop(self) {
        let _ = self.shutdown.send(());
        let _ = tokio::time::timeout(Duration::from_secs(10), self.handle).await;
        for suffix in ["", "-wal", "-shm"] {
            let _ =
                std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.db_path.display())));
        }
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// `output.stdout`, decoded and with a trailing newline (if any) trimmed —
/// the common case for asserting on a full run's printed text.
fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Parse `--json` output as newline-delimited JSON: one object per
/// non-empty line, in the order printed.
fn parse_ndjson(output: &Output) -> Vec<Value> {
    stdout(output)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|e| panic!("invalid JSON line {line:?}: {e}"))
        })
        .collect()
}

fn assert_success(output: &Output, context: &str) {
    assert!(
        output.status.success(),
        "{context}: exit status {:?}\nstdout: {}\nstderr: {}",
        output.status,
        stdout(output),
        stderr(output)
    );
}

// ---------------------------------------------------------------------------
// `mail search`: human-readable output
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_prints_a_human_readable_hit_with_score_snippet_and_sources() {
    let server = TestServer::start().await;
    server
        .index(repo::NewMessage {
            subject: Some("Quarterly budgetary review".to_owned()),
            body_text: Some("The budgetary review is attached for the quarter.".to_owned()),
            from_addr: Some("finance@example.com".to_owned()),
            date: Some(1_700_000_000),
            ..Default::default()
        })
        .await;
    server
        .index(repo::NewMessage {
            subject: Some("Team lunch".to_owned()),
            body_text: Some("Let's get lunch on Friday.".to_owned()),
            date: Some(1_700_000_100),
            ..Default::default()
        })
        .await;

    let output = server.run(["search", "budgetary"]).await;
    assert_success(&output, "mail search budgetary");
    let text = stdout(&output);
    assert!(
        text.contains("Quarterly budgetary review"),
        "expected the matching subject in: {text}"
    );
    assert!(
        !text.contains("Team lunch"),
        "the non-matching message must not appear in: {text}"
    );
    assert!(text.contains("lexical"), "expected a source tag in: {text}");
    assert!(
        text.contains("finance@example.com"),
        "expected the sender in: {text}"
    );

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_with_no_matches_prints_a_placeholder_in_human_mode() {
    let server = TestServer::start().await;
    server
        .index(repo::NewMessage {
            subject: Some("Team lunch".to_owned()),
            body_text: Some("Let's get lunch on Friday.".to_owned()),
            ..Default::default()
        })
        .await;

    let output = server.run(["search", "nonexistenttermxyz"]).await;
    assert_success(&output, "mail search nonexistenttermxyz");
    assert!(
        stdout(&output).contains("no results"),
        "expected a 'no results' placeholder in: {}",
        stdout(&output)
    );

    server.stop().await;
}

/// `-tag:newsletter` starts with a hyphen, which the operator grammar
/// defines as negation, not a CLI flag. `mail search` must accept it as
/// query text without requiring the user to type `mail search --
/// -tag:newsletter` — see `SearchArgs::query`'s own `allow_hyphen_values`
/// doc comment for why.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_accepts_a_hyphen_prefixed_query_without_the_double_dash_escape_hatch() {
    let server = TestServer::start().await;
    server
        .index(repo::NewMessage {
            subject: Some("Weekly newsletter".to_owned()),
            body_text: Some("This week's roundup.".to_owned()),
            ..Default::default()
        })
        .await;

    let output = server.run(["search", "-tag:newsletter"]).await;
    assert_success(&output, "mail search -tag:newsletter");
    assert!(
        !stderr(&output).to_lowercase().contains("unrecognized"),
        "clap must not treat the query as an unknown flag: {}",
        stderr(&output)
    );

    server.stop().await;
}

// ---------------------------------------------------------------------------
// `mail search --json`: the stable contract
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_json_emits_the_documented_field_set_with_offsets_not_markup() {
    let server = TestServer::start().await;
    let message_id = server
        .index(repo::NewMessage {
            subject: Some("Quarterly budgetary review".to_owned()),
            body_text: Some("The budgetary review is attached for the quarter.".to_owned()),
            from_addr: Some("finance@example.com".to_owned()),
            date: Some(1_719_742_320), // 2024-06-30T10:12:00Z
            ..Default::default()
        })
        .await;

    let output = server.run(["search", "budgetary", "--json"]).await;
    assert_success(&output, "mail search budgetary --json");
    let hits = parse_ndjson(&output);
    assert_eq!(hits.len(), 1, "expected exactly one hit in: {hits:?}");
    let hit = &hits[0];

    let object = hit.as_object().expect("hit is a JSON object");
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "date",
            "from",
            "near_duplicates",
            "score",
            "snippet",
            "sources",
            "subject",
            "thread_collapsed",
            "thread_id",
            "uid",
            "why",
        ]
    );

    assert_eq!(hit["uid"], serde_json::json!(message_id));
    assert_eq!(
        hit["subject"],
        serde_json::json!("Quarterly budgetary review")
    );
    assert_eq!(hit["from"], serde_json::json!("finance@example.com"));
    assert_eq!(hit["date"], serde_json::json!("2024-06-30T10:12:00Z"));
    assert!(hit["score"].as_f64().unwrap() > 0.0);
    assert_eq!(hit["why"], Value::Null);
    assert_eq!(hit["thread_collapsed"], serde_json::json!([]));
    assert_eq!(hit["near_duplicates"], serde_json::json!([]));

    let sources = hit["sources"].as_array().expect("sources is an array");
    assert!(
        sources.iter().any(|s| s == "lexical"),
        "expected a lexical source in {sources:?}"
    );

    // The snippet is `{ text, highlights }` -- offsets into `text`, never a
    // string with markup spliced in.
    let snippet = hit["snippet"].as_object().expect("snippet is an object");
    let text = snippet["text"].as_str().expect("snippet.text is a string");
    assert!(!text.is_empty());
    assert!(
        !text.contains('*') && !text.contains('<'),
        "the snippet must carry no rendered markup: {text:?}"
    );
    let highlights = snippet["highlights"]
        .as_array()
        .expect("highlights is an array");
    for range in highlights {
        let start = range["start"].as_u64().expect("start is a number") as usize;
        let end = range["end"].as_u64().expect("end is a number") as usize;
        assert!(
            start < end && end <= text.len(),
            "range {range} out of bounds of {text:?}"
        );
        assert!(
            text.is_char_boundary(start) && text.is_char_boundary(end),
            "range {range} must land on a char boundary of {text:?}"
        );
    }

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_json_with_no_matches_prints_no_lines() {
    let server = TestServer::start().await;
    server
        .index(repo::NewMessage {
            subject: Some("Team lunch".to_owned()),
            body_text: Some("Let's get lunch on Friday.".to_owned()),
            ..Default::default()
        })
        .await;

    let output = server.run(["search", "nonexistenttermxyz", "--json"]).await;
    assert_success(&output, "mail search nonexistenttermxyz --json");
    assert!(
        stdout(&output).trim().is_empty(),
        "expected no NDJSON lines, got: {}",
        stdout(&output)
    );

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_explain_populates_the_structured_why_object() {
    let server = TestServer::start().await;
    server
        .index(repo::NewMessage {
            subject: Some("Quarterly budgetary review".to_owned()),
            body_text: Some("The budgetary review is attached for the quarter.".to_owned()),
            ..Default::default()
        })
        .await;

    let output = server
        .run(["search", "budgetary", "--json", "--explain"])
        .await;
    assert_success(&output, "mail search budgetary --json --explain");
    let hits = parse_ndjson(&output);
    assert_eq!(hits.len(), 1);
    let why = hits[0]["why"]
        .as_object()
        .expect("why is present with --explain");

    assert!(why["score"].as_f64().is_some());
    assert!(why["sources"].as_array().is_some());
    assert_eq!(why["claude_reason"], serde_json::json!(""));
    let features = why["features"].as_array().expect("features is an array");
    assert!(
        !features.is_empty(),
        "expected at least one feature contribution"
    );
    let feature = features[0].as_object().expect("feature is an object");
    for key in ["name", "value", "weight", "weighted_contribution"] {
        assert!(
            feature.contains_key(key),
            "feature missing {key}: {feature:?}"
        );
    }

    server.stop().await;
}

// ---------------------------------------------------------------------------
// Flags map to `SearchRequest` fields
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_mode_lexical_excludes_dense_sourced_candidates() {
    let server = TestServer::start().await;
    let message_id = server
        .index(repo::NewMessage {
            subject: Some("Quarterly budgetary review".to_owned()),
            body_text: Some("The budgetary review is attached for the quarter.".to_owned()),
            ..Default::default()
        })
        .await;
    server.embed(message_id).await;

    // Default (hybrid) mode: the dense retriever also embedded this
    // message, so "dense" must appear among its sources.
    let output = server.run(["search", "budgetary", "--json"]).await;
    assert_success(&output, "mail search budgetary --json");
    let hits = parse_ndjson(&output);
    assert_eq!(hits.len(), 1);
    let hybrid_sources: Vec<String> = hits[0]["sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_owned())
        .collect();
    assert!(
        hybrid_sources.contains(&"dense".to_owned()),
        "expected a dense-sourced hit under default hybrid mode: {hybrid_sources:?}"
    );

    // `--mode lexical` must reach `SearchRequest.mode` and turn dense
    // candidate generation off entirely, regardless of what is embedded.
    let output = server
        .run(["search", "budgetary", "--json", "--mode", "lexical"])
        .await;
    assert_success(&output, "mail search budgetary --json --mode lexical");
    let hits = parse_ndjson(&output);
    assert_eq!(hits.len(), 1);
    let lexical_sources: Vec<String> = hits[0]["sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_owned())
        .collect();
    assert!(
        !lexical_sources.contains(&"dense".to_owned()),
        "--mode lexical must exclude dense-sourced candidates: {lexical_sources:?}"
    );
    assert!(lexical_sources.contains(&"lexical".to_owned()));

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_limit_flag_caps_the_number_of_hits() {
    let server = TestServer::start().await;
    for i in 0..5 {
        server
            .index(repo::NewMessage {
                subject: Some(format!("Quarterly note {i}")),
                body_text: Some("Quarterly filler content for the record.".to_owned()),
                date: Some(1_700_000_000 + i),
                ..Default::default()
            })
            .await;
    }

    let output = server
        .run(["search", "quarterly", "--json", "--limit", "2"])
        .await;
    assert_success(&output, "mail search quarterly --json --limit 2");
    let hits = parse_ndjson(&output);
    assert_eq!(
        hits.len(),
        2,
        "--limit 2 must cap the result count: {hits:?}"
    );

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_account_flag_restricts_to_one_account() {
    let server = TestServer::start().await;
    let (other_account, other_mailbox) = server.insert_account("Work").await;

    let personal_id = server
        .index(repo::NewMessage {
            subject: Some("Quarterly personal note".to_owned()),
            body_text: Some("Quarterly filler content.".to_owned()),
            ..Default::default()
        })
        .await;
    server
        .index(repo::NewMessage {
            account_id: other_account,
            mailbox_id: other_mailbox,
            subject: Some("Quarterly work note".to_owned()),
            body_text: Some("Quarterly filler content.".to_owned()),
            ..Default::default()
        })
        .await;

    let output = server
        .run([
            "search",
            "quarterly",
            "--json",
            "--account",
            &server.account_id.to_string(),
        ])
        .await;
    assert_success(&output, "mail search quarterly --json --account <id>");
    let hits = parse_ndjson(&output);
    assert_eq!(
        hits.len(),
        1,
        "expected only the personal account's hit: {hits:?}"
    );
    assert_eq!(hits[0]["uid"], serde_json::json!(personal_id));

    server.stop().await;
}

// ---------------------------------------------------------------------------
// `mail similar`
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn similar_returns_a_close_neighbor_and_excludes_the_source_message() {
    let server = TestServer::start().await;
    let source_id = server
        .index(repo::NewMessage {
            subject: Some("Quarterly budget review notes".to_owned()),
            body_text: Some(
                "The quarterly budget review covers spending across every department this \
                 quarter."
                    .to_owned(),
            ),
            ..Default::default()
        })
        .await;
    let sibling_id = server
        .index(repo::NewMessage {
            subject: Some("Quarterly budget review draft".to_owned()),
            body_text: Some(
                "This draft quarterly budget review covers spending across most departments \
                 for the quarter."
                    .to_owned(),
            ),
            ..Default::default()
        })
        .await;
    let distractor_id = server
        .index(repo::NewMessage {
            subject: Some("Team lunch on Friday".to_owned()),
            body_text: Some("Let's grab lunch at the new taco place near the office.".to_owned()),
            ..Default::default()
        })
        .await;
    for id in [source_id, sibling_id, distractor_id] {
        server.embed(id).await;
    }

    let output = server
        .run([
            "similar".to_owned(),
            source_id.to_string(),
            "--limit".to_owned(),
            "1".to_owned(),
            "--json".to_owned(),
        ])
        .await;
    assert_success(&output, "mail similar <source> --limit 1 --json");
    let hits = parse_ndjson(&output);
    assert_eq!(hits.len(), 1, "expected exactly one neighbor: {hits:?}");
    assert_ne!(
        hits[0]["uid"],
        serde_json::json!(source_id),
        "the source message must never be its own neighbor"
    );
    assert_eq!(
        hits[0]["uid"],
        serde_json::json!(sibling_id),
        "the vocabulary-overlapping sibling should rank above the unrelated distractor: {hits:?}"
    );

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn similar_on_an_unknown_message_id_fails_with_a_clear_error() {
    let server = TestServer::start().await;
    let output = server.run(["similar", "999999"]).await;
    assert!(
        !output.status.success(),
        "mail similar on an unknown id should fail, not silently print nothing"
    );

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn similar_with_no_neighbors_prints_a_placeholder_in_human_mode() {
    let server = TestServer::start().await;
    let source_id = server
        .index(repo::NewMessage {
            subject: Some("A perfectly ordinary message".to_owned()),
            body_text: Some("Nothing else in this mailbox resembles it.".to_owned()),
            ..Default::default()
        })
        .await;
    server.embed(source_id).await;

    let output = server.run(["similar", &source_id.to_string()]).await;
    assert_success(&output, "mail similar <source>");
    assert!(
        stdout(&output).contains("no similar messages"),
        "expected a placeholder in: {}",
        stdout(&output)
    );

    server.stop().await;
}

// ---------------------------------------------------------------------------
// `mail search eval`: the relevance harness and its exit codes
// ---------------------------------------------------------------------------

/// Write a golden set to a temp file and hand back its path.
///
/// Built per-test rather than pointing at the committed `eval/golden.toml`:
/// this suite's corpus is whatever the individual test seeds, and coupling
/// these assertions to the repo's real golden set would make an unrelated
/// judgment edit break CLI tests that are about flag plumbing and exit
/// codes. `rmaild/tests/eval_service.rs` is where the committed file is
/// exercised against the fixture corpus it actually describes.
fn write_golden(body: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let path =
        std::env::temp_dir().join(format!("rmail-cli-golden-{}-{n}.toml", std::process::id()));
    std::fs::write(&path, body).expect("write golden set");
    path
}

/// Seed one findable message and judge it — the minimum corpus a golden set
/// can score against.
async fn seed_judged(server: &TestServer) -> PathBuf {
    server
        .index(repo::NewMessage {
            message_id: Some("<quarterly@example.com>".to_owned()),
            subject: Some("Quarterly budget review".to_owned()),
            body_text: Some(
                "The quarterly budget review covers headcount and cloud spend.".to_owned(),
            ),
            ..Default::default()
        })
        .await;
    write_golden(
        r#"
version = 1
corpus = "cli-fixture"
[[queries]]
name = "quarterly"
query = "quarterly budget"
judgments = [{ message_id = "<quarterly@example.com>", gain = 3 }]
"#,
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn eval_reports_the_four_metrics_in_a_readable_table() {
    let server = TestServer::start().await;
    let golden = seed_judged(&server).await;

    let output = server
        .run(["search", "eval", "--golden", &golden.to_string_lossy()])
        .await;
    assert_success(&output, "mail search eval");
    let text = stdout(&output);

    assert!(
        text.contains("cli-fixture"),
        "expected the corpus in: {text}"
    );
    for column in ["ndcg@10", "mrr", "recall@50", "p@3"] {
        assert!(text.contains(column), "expected {column} in: {text}");
    }
    assert!(
        text.contains("quarterly"),
        "expected the query row in: {text}"
    );
    assert!(
        text.contains("AGGREGATE"),
        "expected the aggregate in: {text}"
    );

    let _ = std::fs::remove_file(&golden);
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn eval_json_is_one_object_with_the_documented_keys() {
    // Unlike `mail search`, which is newline-delimited per hit: a report is a
    // single value with a single aggregate, so it is one object.
    let server = TestServer::start().await;
    let golden = seed_judged(&server).await;

    let output = server
        .run([
            "search",
            "eval",
            "--golden",
            &golden.to_string_lossy(),
            "--json",
        ])
        .await;
    assert_success(&output, "mail search eval --json");

    let objects = parse_ndjson(&output);
    assert_eq!(
        objects.len(),
        1,
        "a report is one JSON object, not a stream"
    );
    let report = &objects[0];

    let keys: Vec<&str> = report
        .as_object()
        .expect("report is an object")
        .keys()
        .map(String::as_str)
        .collect();
    assert_eq!(keys, vec!["aggregate", "corpus", "per_query"]);

    let aggregate = report["aggregate"].as_object().expect("aggregate object");
    let mut metric_keys: Vec<&str> = aggregate.keys().map(String::as_str).collect();
    metric_keys.sort_unstable();
    assert_eq!(
        metric_keys,
        vec!["mrr", "ndcg_at_10", "p_at_3", "recall_at_50"]
    );

    let per_query = report["per_query"].as_array().expect("per_query array");
    assert_eq!(per_query.len(), 1);
    let mut query_keys: Vec<&str> = per_query[0]
        .as_object()
        .expect("query object")
        .keys()
        .map(String::as_str)
        .collect();
    query_keys.sort_unstable();
    assert_eq!(
        query_keys,
        vec![
            "metrics",
            "name",
            "query",
            "relevant",
            "returned",
            "unresolved"
        ]
    );

    let _ = std::fs::remove_file(&golden);
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn eval_without_a_threshold_reports_and_succeeds() {
    // The developer-reading-numbers mode: no gate, exit 0, even though this
    // corpus scores nothing like perfectly.
    let server = TestServer::start().await;
    server
        .index(repo::NewMessage {
            message_id: Some("<decoy@example.com>".to_owned()),
            subject: Some("Something else entirely".to_owned()),
            body_text: Some("Unrelated to the judged query.".to_owned()),
            ..Default::default()
        })
        .await;
    // Judges a message that exists but that the query will not rank first.
    let golden = write_golden(
        r#"
version = 1
corpus = "cli-fixture"
[[queries]]
name = "unfindable"
query = "wholly unrelated search terms"
judgments = [{ message_id = "<decoy@example.com>", gain = 3 }]
"#,
    );

    let output = server
        .run(["search", "eval", "--golden", &golden.to_string_lossy()])
        .await;
    assert_success(&output, "mail search eval with no threshold");

    let _ = std::fs::remove_file(&golden);
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn eval_fails_the_process_when_ndcg_falls_below_the_threshold() {
    // The acceptance criterion: "CI ... fails the build on an NDCG@10 drop
    // below threshold." A non-zero exit is what makes that true.
    let server = TestServer::start().await;
    server
        .index(repo::NewMessage {
            message_id: Some("<decoy@example.com>".to_owned()),
            subject: Some("Something else entirely".to_owned()),
            body_text: Some("Unrelated to the judged query.".to_owned()),
            ..Default::default()
        })
        .await;
    let golden = write_golden(
        r#"
version = 1
corpus = "cli-fixture"
[[queries]]
name = "unfindable"
query = "wholly unrelated search terms"
judgments = [{ message_id = "<decoy@example.com>", gain = 3 }]
"#,
    );

    let output = server
        .run([
            "search",
            "eval",
            "--golden",
            &golden.to_string_lossy(),
            "--min-ndcg",
            "0.99",
        ])
        .await;
    assert!(
        !output.status.success(),
        "a below-threshold run must exit non-zero\nstdout: {}\nstderr: {}",
        stdout(&output),
        stderr(&output)
    );
    let err = stderr(&output);
    assert!(
        err.contains("NDCG@10"),
        "the failure should name the metric: {err}"
    );
    assert!(
        err.contains("worst queries"),
        "a failure should point at which queries regressed: {err}"
    );

    let _ = std::fs::remove_file(&golden);
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn eval_passes_when_the_threshold_is_met() {
    let server = TestServer::start().await;
    let golden = seed_judged(&server).await;

    let output = server
        .run([
            "search",
            "eval",
            "--golden",
            &golden.to_string_lossy(),
            "--min-ndcg",
            "0.9",
            "--min-mrr",
            "0.9",
        ])
        .await;
    assert_success(&output, "mail search eval meeting its thresholds");

    let _ = std::fs::remove_file(&golden);
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unresolved_judgment_fails_a_gating_run_but_is_waivable() {
    // A fixture that did not seed must not be mistakable for a ranker that
    // got worse — so it fails even though the resolvable query scores
    // perfectly, and says so specifically.
    let server = TestServer::start().await;
    server
        .index(repo::NewMessage {
            message_id: Some("<quarterly@example.com>".to_owned()),
            subject: Some("Quarterly budget review".to_owned()),
            body_text: Some("The quarterly budget review covers cloud spend.".to_owned()),
            ..Default::default()
        })
        .await;
    let golden = write_golden(
        r#"
version = 1
corpus = "cli-fixture"
[[queries]]
name = "quarterly"
query = "quarterly budget"
judgments = [
  { message_id = "<quarterly@example.com>", gain = 3 },
  { message_id = "<never-synced@example.com>", gain = 3 },
]
"#,
    );

    let gated = server
        .run([
            "search",
            "eval",
            "--golden",
            &golden.to_string_lossy(),
            "--min-ndcg",
            "0.1",
        ])
        .await;
    assert!(
        !gated.status.success(),
        "an unresolved judgment must fail a gating run"
    );
    assert!(
        stderr(&gated).contains("<never-synced@example.com>"),
        "the failure should name the missing message: {}",
        stderr(&gated)
    );

    let waived = server
        .run([
            "search",
            "eval",
            "--golden",
            &golden.to_string_lossy(),
            "--min-ndcg",
            "0.1",
            "--allow-unresolved",
        ])
        .await;
    assert_success(&waived, "--allow-unresolved should waive it");

    let _ = std::fs::remove_file(&golden);
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_golden_set_fails_with_the_path_in_the_message() {
    let server = TestServer::start().await;
    let output = server
        .run(["search", "eval", "--golden", "/nonexistent/golden.toml"])
        .await;
    assert!(!output.status.success(), "a missing golden set must fail");
    assert!(
        stderr(&output).contains("/nonexistent/golden.toml"),
        "the error should name the path the user gave: {}",
        stderr(&output)
    );
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_literal_term_eval_is_still_searchable_behind_the_double_dash() {
    // `eval` as a subcommand shadows it as a query; `--` is the documented
    // escape hatch, and it has to actually work or the trade is a bug.
    let server = TestServer::start().await;
    server
        .index(repo::NewMessage {
            subject: Some("Notes on eval methodology".to_owned()),
            body_text: Some("An eval is only as good as its golden set.".to_owned()),
            ..Default::default()
        })
        .await;

    let output = server.run(["search", "--", "eval"]).await;
    assert_success(&output, "mail search -- eval");
    let text = stdout(&output);
    assert!(
        text.contains("eval methodology"),
        "expected the searched message, not a harness report: {text}"
    );
    assert!(
        !text.contains("AGGREGATE"),
        "`-- eval` must search, not run the harness: {text}"
    );

    server.stop().await;
}
