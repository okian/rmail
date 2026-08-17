//! End-to-end coverage of `mail api` and the exit-code contract, driving the
//! compiled binary against real servers (task 42).
//!
//! Two servers, because they answer different questions:
//!
//! - A Unix-socket `rmaild` for the ordinary path. Over a Unix socket the
//!   caller's uid matches the daemon's, so `rmaild::auth` grants implicit
//!   admin and every method is reachable — which is what makes this the right
//!   place to check that reflection discovers the surface and that a real RPC
//!   round-trips.
//! - A TCP server carrying the *same* `AuthLayer` for the refusal path. There
//!   is no Unix peer to trust over TCP, so the bearer token is the only
//!   principal and a narrow one is genuinely refused. Testing a denial over
//!   the Unix socket would be impossible: the peer-uid check short-circuits
//!   before the token is looked at (see `rmaild::auth`'s "Two principals"),
//!   so a `mail.read` token presented by the daemon's own user is simply
//!   ignored.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::process::Output;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use rmail_core::auth::{NewToken, Scope};
use rmail_proto::v1::admin_service_server::AdminServiceServer;
use tokio::process::Command;
use tokio::sync::oneshot;

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn scratch(tag: &str, extension: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "rmail-cli-api-{tag}-{}-{n}.{extension}",
        std::process::id()
    ))
}

/// An in-process `rmaild` on a Unix socket.
struct Daemon {
    socket: PathBuf,
    db_path: PathBuf,
    db: rmail_core::Database,
    shutdown: oneshot::Sender<()>,
    handle: tokio::task::JoinHandle<Result<(), rmaild::ServeError>>,
}

impl Daemon {
    async fn start(tag: &str) -> Self {
        let socket = scratch(tag, "sock");
        let db_path = scratch(tag, "db");
        let db = rmail_core::Database::open(&db_path).expect("open db");
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let server_socket = socket.clone();
        let server_db = db.clone();
        let handle = tokio::spawn(async move {
            rmaild::serve_uds(&server_socket, server_db, async move {
                let _ = shutdown_rx.await;
            })
            .await
        });

        let mut ready = false;
        for _ in 0..400 {
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
        let _ = std::fs::remove_file(&self.socket);
    }
}

/// Run `mail` with the daemon's socket in the environment.
async fn mail(socket: &PathBuf, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_mail"))
        .args(args)
        .env(rmail_core::SOCKET_ENV, socket)
        // The harness must not inherit a developer's own token or format.
        .env_remove("RMAIL_TOKEN")
        .env_remove("RMAIL_FORMAT")
        .output()
        .await
        .expect("run mail")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn exit_code(output: &Output) -> i32 {
    output.status.code().expect("mail exited normally")
}

// ---------------------------------------------------------------------------
// Reflection
// ---------------------------------------------------------------------------

/// `mail api reflect` discovers the daemon's surface by asking it, and the
/// listing covers the whole capability registry rather than a prefix of it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reflect_lists_every_service_the_daemon_serves() {
    let daemon = Daemon::start("reflect").await;

    let output = mail(&daemon.socket, &["api", "reflect", "--format", "json"]).await;
    assert_eq!(
        exit_code(&output),
        0,
        "mail api reflect failed: {}",
        stderr(&output)
    );
    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap_or_else(|e| {
        panic!(
            "--format json must be one document: {e}\n{}",
            stdout(&output)
        )
    });

    let services = value["services"].as_array().expect("a services array");
    let names: Vec<&str> = services
        .iter()
        .filter_map(|s| s["service"].as_str())
        .collect();
    assert!(
        names.contains(&"rmail.v1.MailService"),
        "reflection did not report MailService: {names:?}"
    );

    // Every capability row's service must appear: reflection is what `api
    // call` resolves against, so a service missing here is a method nobody
    // can call generically.
    let mut missing: Vec<&str> = Vec::new();
    for command in rmail_core::parity::Command::ALL {
        if !names.contains(&command.service()) {
            missing.push(command.service());
        }
    }
    missing.sort_unstable();
    missing.dedup();
    assert!(
        missing.is_empty(),
        "reflection did not report these services: {missing:?}"
    );

    let mail_service = services
        .iter()
        .find(|s| s["service"] == "rmail.v1.MailService")
        .expect("MailService");
    let methods = mail_service["methods"].as_array().expect("methods");
    let list = methods
        .iter()
        .find(|m| m["name"] == "List")
        .expect("MailService/List");
    assert_eq!(list["server_streaming"], serde_json::json!(true));
    assert_eq!(list["input_type"], "rmail.v1.ListMessagesRequest");

    daemon.stop().await;
}

// ---------------------------------------------------------------------------
// Calling
// ---------------------------------------------------------------------------

/// A real unary RPC, invoked by name with a JSON body, answering JSON.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn call_round_trips_a_unary_rpc() {
    let daemon = Daemon::start("call").await;

    // Mint through the generic path, then read it back through the typed one:
    // if `api call` had encoded anything wrongly, the token would not exist
    // under the name it was given.
    let output = mail(
        &daemon.socket,
        &[
            "api",
            "call",
            "AdminService.MintToken",
            r#"{"name":"from-api-call","scopes":["mail.read"]}"#,
        ],
    )
    .await;
    assert_eq!(
        exit_code(&output),
        0,
        "mail api call failed: {}",
        stderr(&output)
    );
    let minted: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("JSON response");
    assert_eq!(minted["name"], "from-api-call");
    assert!(
        minted["token"]
            .as_str()
            .is_some_and(|t| t.starts_with("rmail_tok_")),
        "the response carried no bearer secret: {minted}"
    );
    // A 64-bit id inside IEEE-754's exact range is a JSON number; past 2^53 it
    // becomes a string so `jq` cannot silently round it. That rule belongs to
    // `rmaild::mcp::codec` and is pinned there and in `format::tests`; what
    // this asserts is only that the id survived as an integer at all.
    assert_eq!(
        minted["id"].as_i64(),
        Some(1),
        "the minted id did not round-trip: {minted}"
    );

    let listed = mail(&daemon.socket, &["token", "list"]).await;
    assert!(
        stdout(&listed).contains("from-api-call"),
        "the token minted through `api call` is not in `mail token list`: {}",
        stdout(&listed)
    );

    daemon.stop().await;
}

/// A server-streaming method answers a bounded prefix, and `--format ndjson`
/// unwraps it to one line per gRPC frame.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_streaming_call_is_bounded_and_ndjson_is_one_line_per_frame() {
    let daemon = Daemon::start("stream").await;

    let output = mail(
        &daemon.socket,
        &[
            "api",
            "call",
            "AdminService.ListTokens",
            "{}",
            "--format",
            "ndjson",
        ],
    )
    .await;
    assert_eq!(exit_code(&output), 0, "{}", stderr(&output));
    // A unary method in ndjson is a single line — the format is "one JSON
    // value per line", and a unary RPC is a stream of one.
    assert_eq!(stdout(&output).lines().count(), 1, "{}", stdout(&output));

    // `MailService/List` streams; with no mailbox it ends immediately, which
    // still has to produce a well-formed (empty) frame list rather than an
    // error.
    let output = mail(
        &daemon.socket,
        &[
            "api",
            "call",
            "MailService.List",
            r#"{"mailbox_id":1,"page_size":5}"#,
            "--max-frames",
            "2",
        ],
    )
    .await;
    let rendered = stdout(&output);
    if exit_code(&output) == 0 {
        let value: serde_json::Value = serde_json::from_str(&rendered).expect("JSON");
        assert!(value["frames"].is_array(), "{rendered}");
        assert!(
            value["frames"].as_array().map(Vec::len).unwrap_or(0) <= 2,
            "--max-frames was not honoured: {rendered}"
        );
    } else {
        // An empty database can legitimately answer NOT_FOUND for mailbox 1;
        // what must hold either way is that the code is the classified one and
        // not a blanket 1.
        assert_eq!(exit_code(&output), 6, "{}", stderr(&output));
    }

    daemon.stop().await;
}

/// An unknown method is `NOT_FOUND` (6), a malformed body is a usage error
/// (2), and an unrecognised *field* is `INVALID_ARGUMENT` (8) rather than
/// being dropped — the last is the codec behaviour this verb inherits and the
/// one that would otherwise return a filtered-looking answer to an unfiltered
/// query.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_failure_modes_have_distinct_documented_exit_codes() {
    let daemon = Daemon::start("codes").await;

    let unknown = mail(
        &daemon.socket,
        &["api", "call", "MailService.Teleport", "{}"],
    )
    .await;
    assert_eq!(exit_code(&unknown), 6, "{}", stderr(&unknown));

    let malformed = mail(&daemon.socket, &["api", "call", "MailService.Get", "{oops"]).await;
    assert_eq!(exit_code(&malformed), 2, "{}", stderr(&malformed));

    let unknown_field = mail(
        &daemon.socket,
        &[
            "api",
            "call",
            "AdminService.MintToken",
            r#"{"nam":"typo","scopes":["admin"]}"#,
        ],
    )
    .await;
    assert_eq!(
        exit_code(&unknown_field),
        8,
        "an unrecognised field must be refused, not dropped: {}",
        stderr(&unknown_field)
    );

    // A real NOT_FOUND from the daemon, distinct from "no such method" only in
    // the message — both are 6, which is the point: a script branching on
    // "the thing is not there" gets one number.
    let missing = mail(
        &daemon.socket,
        &["api", "call", "MailService.Get", r#"{"id":"999999"}"#],
    )
    .await;
    assert_eq!(exit_code(&missing), 6, "{}", stderr(&missing));

    daemon.stop().await;
}

/// With no daemon, `mail` refuses immediately with the documented
/// `FAILED_PRECONDITION` code — it does not hang, and it names the fix.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_daemon_is_a_fast_failed_precondition_that_names_daemon_start() {
    let socket = scratch("absent", "sock");
    assert!(!socket.exists());

    let started = std::time::Instant::now();
    let output = mail(&socket, &["api", "ping"]).await;
    assert_eq!(exit_code(&output), 9, "{}", stderr(&output));
    assert!(
        stderr(&output).contains("mail daemon start"),
        "{}",
        stderr(&output)
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "a missing daemon must not hang: {:?}",
        started.elapsed()
    );
}

/// `mail api ping` reports serving and a latency, in every format.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn api_ping_reports_serving_in_every_format() {
    let daemon = Daemon::start("ping").await;

    let table = mail(&daemon.socket, &["api", "ping"]).await;
    assert_eq!(exit_code(&table), 0, "{}", stderr(&table));
    assert!(stdout(&table).contains("SERVING"), "{}", stdout(&table));

    let json = mail(&daemon.socket, &["api", "ping", "--format", "json"]).await;
    let value: serde_json::Value = serde_json::from_str(&stdout(&json)).expect("one document");
    assert_eq!(value["serving"], serde_json::json!(true));
    assert!(value["latency_ms"].is_number(), "{value}");

    daemon.stop().await;
}

// ---------------------------------------------------------------------------
// The refusal path: --addr, --token, and the scope table
// ---------------------------------------------------------------------------

/// The same `AuthLayer` the daemon installs, over TCP, where the bearer token
/// is the only principal.
async fn tcp_server(
    db: rmail_core::Database,
) -> (String, oneshot::Sender<()>, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a loopback port");
    let addr = listener.local_addr().expect("local addr").to_string();
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

    let reflection = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(rmail_proto::FILE_DESCRIPTOR_SET)
        .build_v1()
        .expect("build reflection");

    let handle = tokio::spawn(async move {
        let _ = tonic::transport::Server::builder()
            // `admin_uid` is irrelevant over TCP — there is no Unix peer to
            // compare it against — so the layer falls through to the bearer
            // token, which is exactly the principal this test needs.
            .layer(rmaild::AuthLayer::new(db.clone(), 0))
            .add_service(reflection)
            .add_service(AdminServiceServer::new(rmaild::AdminApi::new(db)))
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::TcpListenerStream::new(listener),
                async move {
                    let _ = shutdown_rx.await;
                },
            )
            .await;
    });

    (addr, shutdown_tx, handle)
}

/// Run `mail` against a TCP endpoint, optionally with a token.
async fn mail_tcp(addr: &str, token: Option<&str>, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_mail"));
    command
        .args(args)
        .args(["--addr", addr, "--insecure"])
        .env_remove("RMAIL_TOKEN")
        .env_remove("RMAIL_FORMAT")
        // A socket that does not exist, to prove `--addr` is what is being
        // used: if the TCP path silently fell back to the Unix socket this
        // would fail with FAILED_PRECONDITION instead.
        .env(rmail_core::SOCKET_ENV, "/nonexistent/rmail.sock");
    if let Some(token) = token {
        command.args(["--token", token]);
    }
    command.output().await.expect("run mail")
}

/// `api call` goes through the daemon's own scope table and is refused for a
/// method the caller's token does not cover — with a code distinct from
/// "not found" and from "no credential at all".
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_method_the_token_lacks_scope_for_is_refused_with_its_own_code() {
    let daemon = Daemon::start("scope").await;
    let (addr, shutdown, handle) = tcp_server(daemon.db.clone()).await;

    let narrow = rmail_core::auth::mint(
        &daemon.db,
        NewToken {
            name: "narrow".to_owned(),
            scopes: vec![Scope::MailRead],
            ttl_secs: None,
        },
    )
    .await
    .expect("mint a narrow token");
    let wide = rmail_core::auth::mint(
        &daemon.db,
        NewToken {
            name: "wide".to_owned(),
            scopes: vec![Scope::Admin],
            ttl_secs: None,
        },
    )
    .await
    .expect("mint an admin token");

    // Reflection is public, so discovery works with no credential at all —
    // otherwise `api call` could not even resolve the method it is about to
    // be refused for.
    let reflected = mail_tcp(&addr, None, &["api", "reflect"]).await;
    assert_eq!(
        exit_code(&reflected),
        0,
        "reflection must be reachable without a token: {}",
        stderr(&reflected)
    );

    // No credential: UNAUTHENTICATED (4), not PERMISSION_DENIED.
    let anonymous = mail_tcp(
        &addr,
        None,
        &["api", "call", "AdminService.ListTokens", "{}"],
    )
    .await;
    assert_eq!(
        exit_code(&anonymous),
        4,
        "no token must be unauthenticated: {}",
        stderr(&anonymous)
    );

    // A valid credential that does not cover the method: PERMISSION_DENIED (5).
    let refused = mail_tcp(
        &addr,
        Some(&narrow.secret),
        &["api", "call", "AdminService.ListTokens", "{}"],
    )
    .await;
    assert_eq!(
        exit_code(&refused),
        5,
        "a scope shortfall must be permission-denied: {}",
        stderr(&refused)
    );
    assert_ne!(
        exit_code(&refused),
        exit_code(&anonymous),
        "\"no credential\" and \"wrong scope\" must be different exit codes"
    );

    // The same call with an admin token succeeds, which is what proves the
    // refusal above was the scope table and not the transport.
    let allowed = mail_tcp(
        &addr,
        Some(&wide.secret),
        &["api", "call", "AdminService.ListTokens", "{}"],
    )
    .await;
    assert_eq!(
        exit_code(&allowed),
        0,
        "an admin token must be accepted: {}",
        stderr(&allowed)
    );
    let value: serde_json::Value = serde_json::from_str(&stdout(&allowed)).expect("JSON");
    assert!(value["tokens"].is_array(), "{value}");

    let _ = shutdown.send(());
    let _ = handle.await;
    daemon.stop().await;
}

/// In structured mode the failure is itself JSON on stderr, carrying the same
/// classification as the exit code — so a pipeline can branch on `.code`
/// without matching English.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_structured_failure_is_json_on_stderr_carrying_its_code() {
    let daemon = Daemon::start("errjson").await;

    let output = mail(
        &daemon.socket,
        &[
            "api",
            "call",
            "MailService.Teleport",
            "{}",
            "--format",
            "json",
        ],
    )
    .await;
    assert_eq!(exit_code(&output), 6, "{}", stderr(&output));
    assert!(
        stdout(&output).is_empty(),
        "stdout must stay a clean document channel: {}",
        stdout(&output)
    );
    // The last line, not the whole of stderr: anything linked into the binary
    // may have written a diagnostic there first (the ONNX runtime prints a CPU
    // warning on some hosts). The contract is that the *failure* is one JSON
    // object, not that stderr carries nothing else.
    let last = stderr(&output)
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
        .expect("stderr carries the failure")
        .to_owned();
    let value: serde_json::Value =
        serde_json::from_str(last.trim()).unwrap_or_else(|e| panic!("{e}: {last}"));
    assert_eq!(value["code"], "not_found");
    assert_eq!(value["exit_code"], serde_json::json!(6));
    assert!(
        value["error"]
            .as_str()
            .is_some_and(|e| e.contains("Teleport")),
        "the failure must say what was asked for: {value}"
    );

    daemon.stop().await;
}

/// A verb in `format::STRUCTURED` must actually *emit* JSON, not merely be
/// listed.
///
/// `every_cli_verb_declares_how_it_answers_format_json` checks list membership
/// and nothing else, so five verbs shipped listed-but-unwired: they consulted
/// only their own legacy `--json` flag and printed their human table to a
/// `--format json` caller — the exact failure the whole design exists to
/// prevent. Only running them can catch that.
///
/// The verbs here are the ones an empty daemon can answer without setup.
/// Adding a verb to `STRUCTURED` that needs fixtures does not have to be
/// listed here, but a verb that *can* be driven cheaply should be.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn structured_verbs_actually_emit_json() {
    let daemon = Daemon::start("emits").await;

    for (verb, args) in [
        ("ping", vec!["ping"]),
        ("api ping", vec!["api", "ping"]),
        ("api reflect", vec!["api", "reflect"]),
        ("ai status", vec!["ai", "status"]),
        ("token list", vec!["token", "list"]),
        ("find", vec!["find", "anything"]),
        ("search models", vec!["search", "models"]),
        ("daemon status", vec!["daemon", "status"]),
        ("notify score", vec!["notify", "score", "1"]),
    ] {
        for format in ["json", "ndjson"] {
            let mut argv = args.clone();
            argv.extend(["--format", format]);
            let output = mail(&daemon.socket, &argv).await;
            // A verb may legitimately *fail* against an empty database
            // (`notify score 1` has no such message). What it must never do is
            // succeed while printing something that is not JSON.
            if !output.status.success() {
                assert!(
                    stdout(&output).trim().is_empty(),
                    "`mail {verb} --format {format}` failed but still wrote to stdout: {}",
                    stdout(&output)
                );
                continue;
            }
            let text = stdout(&output);
            let trimmed = text.trim();
            assert!(
                !trimmed.is_empty(),
                "`mail {verb} --format {format}` printed nothing"
            );
            if format == "ndjson" {
                for line in trimmed.lines() {
                    serde_json::from_str::<serde_json::Value>(line).unwrap_or_else(|e| {
                        panic!("`mail {verb} --format ndjson` wrote a non-JSON line ({e}): {line}")
                    });
                }
            } else {
                serde_json::from_str::<serde_json::Value>(trimmed).unwrap_or_else(|e| {
                    panic!("`mail {verb} --format json` wrote a non-JSON document ({e}): {text}")
                });
            }
        }
    }

    daemon.stop().await;
}

/// A verb with no curated schema refuses `--format json` rather than printing
/// its table, and names the generic path that does answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_verb_without_a_schema_refuses_format_json_instead_of_printing_a_table() {
    // No daemon is needed: the refusal happens before any connection, which is
    // itself the point — a caller learns the flag is unsupported without
    // waiting on a network.
    let socket = scratch("nofmt", "sock");
    let output = mail(&socket, &["tags", "--format", "json"]).await;
    assert_eq!(exit_code(&output), 12, "{}", stderr(&output));
    assert!(
        stdout(&output).is_empty(),
        "a caller who asked for JSON must not be handed a table: {}",
        stdout(&output)
    );
    assert!(
        stderr(&output).contains("mail api call"),
        "the refusal must name the generic path: {}",
        stderr(&output)
    );
}

/// `mail daemon status` answers in every format, and says "not running"
/// without failing — a status verb that exited non-zero for its own answer
/// would be unusable under `set -e`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn daemon_status_answers_running_and_not_running() {
    let absent = scratch("status-absent", "sock");
    let output = mail(&absent, &["daemon", "status", "--format", "json"]).await;
    assert_eq!(exit_code(&output), 0, "{}", stderr(&output));
    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("one document");
    assert_eq!(value["running"], serde_json::json!(false));
    assert_eq!(value["status"], serde_json::Value::Null);

    let daemon = Daemon::start("status").await;
    let output = mail(&daemon.socket, &["daemon", "status", "--format", "json"]).await;
    assert_eq!(exit_code(&output), 0, "{}", stderr(&output));
    let value: serde_json::Value = serde_json::from_str(&stdout(&output)).expect("one document");
    assert_eq!(value["running"], serde_json::json!(true));
    assert_eq!(value["status"], "SERVING");
    daemon.stop().await;
}

/// `--deadline` travels as a gRPC deadline. Set to the minimum, a call that
/// has to do real work comes back as `DEADLINE_EXCEEDED` (10) rather than
/// hanging or being reported as a generic failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deadline_is_honoured_and_classified() {
    let daemon = Daemon::start("deadline").await;
    // A generous deadline must not break an ordinary call — the flag has to be
    // usable, not merely present.
    let ok = mail(
        &daemon.socket,
        &["api", "ping", "--deadline", "30", "--format", "json"],
    )
    .await;
    assert_eq!(exit_code(&ok), 0, "{}", stderr(&ok));

    // Zero is rejected as a *usage* error (2), not the generic failure (1):
    // asserting only `!= 0` would pass against a code that told a script
    // nothing.
    let zero = mail(&daemon.socket, &["api", "ping", "--deadline", "0"]).await;
    assert_eq!(exit_code(&zero), 2, "{}", stderr(&zero));
    assert!(stderr(&zero).contains("--deadline"), "{}", stderr(&zero));

    // The same for the other flag contradictions, which share the code.
    for args in [
        vec!["--tls-cert", "/nope.pem", "--addr", "127.0.0.1:1"],
        vec!["--insecure"],
        vec!["--tls-ca", "/nope.pem"],
    ] {
        let mut argv = vec!["api", "ping"];
        argv.extend(args.iter().copied());
        let refused = mail(&daemon.socket, &argv).await;
        assert_eq!(exit_code(&refused), 2, "{args:?}: {}", stderr(&refused));
    }

    // And the deadline itself reaches the wire. `--deadline 1` against
    // `api call` gives reflection *and* the call one second between them; the
    // point is only that a deadline that small classifies as
    // DEADLINE_EXCEEDED(10) or succeeds, never as an unclassified failure.
    let tight = mail(
        &daemon.socket,
        &[
            "api",
            "call",
            "AdminService.ListTokens",
            "{}",
            "--deadline",
            "1",
        ],
    )
    .await;
    assert!(
        matches!(exit_code(&tight), 0 | 10),
        "a tight deadline must succeed or be classified as deadline_exceeded, got {}: {}",
        exit_code(&tight),
        stderr(&tight)
    );

    daemon.stop().await;
}

/// An unreadable `--tls-ca` is a bad *argument* (8), not a missing mailbox
/// (`NOT_FOUND`, 6) — the raw `io::Error` would have said the latter.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreadable_certificate_is_an_invalid_argument() {
    let socket = scratch("tls", "sock");
    let output = Command::new(env!("CARGO_BIN_EXE_mail"))
        .args([
            "api",
            "ping",
            "--addr",
            "127.0.0.1:1",
            "--tls-ca",
            "/nonexistent/ca.pem",
        ])
        .env(rmail_core::SOCKET_ENV, &socket)
        .env_remove("RMAIL_TOKEN")
        .env_remove("RMAIL_FORMAT")
        .output()
        .await
        .expect("run mail");
    assert_eq!(
        output.status.code(),
        Some(8),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// `mail daemon` manages the *local* daemon, so `--addr` must be refused
/// rather than silently answered about the Unix socket.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn daemon_verbs_refuse_a_remote_address() {
    let socket = scratch("daemon-addr", "sock");
    let output = Command::new(env!("CARGO_BIN_EXE_mail"))
        .args(["daemon", "status", "--addr", "127.0.0.1:1", "--insecure"])
        .env(rmail_core::SOCKET_ENV, &socket)
        .env_remove("RMAIL_TOKEN")
        .env_remove("RMAIL_FORMAT")
        .output()
        .await
        .expect("run mail");
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("mail api ping"),
        "the refusal must name what does work: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// `mail export` keeps its archive formats under `--archive-format`, and the
/// global `--format`/`$RMAIL_FORMAT` can no longer reach that field.
///
/// This is the corruption the reviewer found: `clap` merges a global argument
/// and a subcommand argument sharing an id by value-source precedence and
/// writes the winner into *both*, so `RMAIL_FORMAT=json mail export -o
/// backup.mbox` wrote a JSON archive into a `.mbox` file with no diagnostic.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_global_format_cannot_reach_an_export_archive_format() {
    let socket = scratch("export-fmt", "sock");
    let out = scratch("export-fmt", "mbox");

    // With `$RMAIL_FORMAT` set, `mail export` must refuse the *global* flag
    // (it has no curated schema) rather than quietly changing the archive
    // format. Either way it must never reach the daemon having decided to
    // write JSON.
    let output = Command::new(env!("CARGO_BIN_EXE_mail"))
        .args(["export", "--thread", "1", "-o"])
        .arg(&out)
        .env(rmail_core::SOCKET_ENV, &socket)
        .env("RMAIL_FORMAT", "json")
        .env_remove("RMAIL_TOKEN")
        .output()
        .await
        .expect("run mail");
    // 12 = the documented refusal for a verb with no structured rendering.
    // Not 9 (it never connected) and never 0.
    assert_eq!(
        output.status.code(),
        Some(12),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!out.exists(), "nothing may be written");

    // The old spelling now fails legibly instead of doing the wrong thing.
    let renamed = Command::new(env!("CARGO_BIN_EXE_mail"))
        .args(["export", "--thread", "1", "--format", "mbox", "-o"])
        .arg(&out)
        .env(rmail_core::SOCKET_ENV, &socket)
        .env_remove("RMAIL_FORMAT")
        .env_remove("RMAIL_TOKEN")
        .output()
        .await
        .expect("run mail");
    assert_eq!(renamed.status.code(), Some(2), "clap rejects the value");
    assert!(
        String::from_utf8_lossy(&renamed.stderr).contains("ndjson"),
        "the error should list the global flag's own values: {}",
        String::from_utf8_lossy(&renamed.stderr)
    );
}
