//! The MCP adapter end to end, against a real in-process tonic server.
//!
//! `mcp::projection`'s own tests prove the surface is generated correctly and
//! `mcp::server`'s prove the protocol; neither of them sends a byte. What is
//! only provable here is the claim that makes the projection worth anything:
//! that a tool call encoded from JSON by
//! `mcp::codec` — with no per-RPC code anywhere — actually reaches the
//! handler, and that its answer decodes back.
//!
//! Two of these tests exist for one property in particular: **the daemon is
//! the enforcement point.** The scope filtering in `mcp::projection` is a
//! courtesy, and a test suite that only ever exercised the courtesy would
//! pass just as happily if the auth layer had been removed.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use rmail_core::auth::Scope;
use rmail_core::events::{EventKind as CoreEventKind, EventLog, NewEvent, Retention};
use rmail_core::sync::{SyncEngine, SyncOptions};
use rmaild::mcp::{CallLimits, McpServer, Principal};
use serde_json::{json, Value};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tonic_health::pb::health_client::HealthClient;
use tonic_health::pb::HealthCheckRequest;

static COUNTER: AtomicU32 = AtomicU32::new(0);

fn unique(prefix: &str, extension: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    // `/tmp` rather than `temp_dir()` for the socket: macOS caps `sockaddr_un`
    // at 104 bytes and the default temp dir eats most of that.
    PathBuf::from("/tmp").join(format!("rmail-{prefix}-{pid}-{n}.{extension}"))
}

struct TestServer {
    socket: PathBuf,
    db_path: PathBuf,
    /// The *same* `EventLog` instance the server uses — a second one over the
    /// same database shares the durable rows but not the live fan-out, so a
    /// test that appended through one would never be seen by a stream served
    /// from the other (see `rmaild::serve_uds_with_engine`'s own docs).
    log: EventLog,
    shutdown: Option<oneshot::Sender<()>>,
    handle: Option<JoinHandle<Result<(), rmaild::ServeError>>>,
}

impl TestServer {
    async fn start() -> Self {
        let socket = unique("mcp", "sock");
        let db_path = unique("mcp", "db");
        let db = rmail_core::Database::open(&db_path).unwrap();
        let log = EventLog::new(db.clone(), Retention::default());
        let engine = SyncEngine::new(db.clone(), log.clone(), SyncOptions::default());
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let server_socket = socket.clone();
        let handle = tokio::spawn(async move {
            // Semantic indexing off, the same convention `rmaild::serve_uds`
            // itself follows: an enabled default would make every test here
            // load — or on a cold cache download — an ONNX model purely
            // because `SearchService` is wired up alongside.
            let mut config = rmail_core::Config::default();
            config.index.semantic.enabled = false;
            rmaild::serve_uds_with_engine(&server_socket, db, engine, &config, async move {
                let _ = shutdown_rx.await;
            })
            .await
        });

        let mut ready = false;
        for _ in 0..300 {
            if let Ok(channel) = rmail_core::connect_uds(&socket).await {
                if HealthClient::new(channel)
                    .check(HealthCheckRequest {
                        service: String::new(),
                    })
                    .await
                    .is_ok()
                {
                    ready = true;
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(ready, "server {} never became ready", socket.display());
        Self {
            socket,
            db_path,
            log,
            shutdown: Some(shutdown_tx),
            handle: Some(handle),
        }
    }

    /// An MCP server over this daemon, with the given declared scopes and an
    /// optional bearer token.
    async fn mcp(&self, scopes: Vec<Scope>, bearer: Option<String>) -> McpServer {
        self.mcp_with(
            scopes,
            bearer,
            CallLimits {
                max_frames: 4,
                timeout: Duration::from_secs(20),
            },
        )
        .await
    }

    async fn mcp_with(
        &self,
        scopes: Vec<Scope>,
        bearer: Option<String>,
        limits: CallLimits,
    ) -> McpServer {
        let channel = rmail_core::connect_uds(&self.socket).await.unwrap();
        McpServer::new(
            channel,
            Principal { scopes, bearer },
            limits,
            CancellationToken::new(),
        )
        .expect("the surface must project")
    }

    /// Append `count` events to the log the server streams from.
    async fn seed_events(&self, count: usize) {
        for _ in 0..count {
            self.log
                .append(NewEvent::new(CoreEventKind::NewMail).account(1))
                .await
                .expect("append an event");
        }
    }

    async fn stop(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.handle.take() {
            let _ = tokio::time::timeout(Duration::from_secs(10), handle).await;
        }
        let _ = std::fs::remove_file(&self.socket);
        let _ = std::fs::remove_file(&self.db_path);
    }
}

/// How long any single `tools/call` in this file may take before the test
/// gives up.
///
/// Every call here is bounded by `CallLimits::timeout` already, so this only
/// fires when that bound has itself regressed — which is a *hang*, not a wrong
/// answer, and an unbounded await would wedge the whole suite instead of
/// reporting it. (Confirmed by deleting the frame cap and the deadline arm:
/// without this, the two truncation tests ran for eight minutes and the
/// container had to be killed.)
const CALL_PATIENCE: Duration = Duration::from_secs(45);

/// One `tools/call`, returning the `result` object.
async fn call(server: &McpServer, name: &str, arguments: Value) -> Value {
    let request = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": name, "arguments": arguments },
    });
    let text = tokio::time::timeout(CALL_PATIENCE, server.handle(&request.to_string()))
        .await
        .unwrap_or_else(|_| {
            panic!("{name} did not answer within {CALL_PATIENCE:?}; its own bounds are not binding")
        })
        .expect("a call must be answered");
    let response: Value = serde_json::from_str(&text).expect("JSON");
    assert!(
        response.get("error").is_none(),
        "{name} failed at the protocol level: {response}"
    );
    response["result"].clone()
}

/// A unary RPC, dispatched from JSON with no per-RPC code anywhere.
#[tokio::test]
async fn a_unary_tool_call_reaches_the_handler_and_decodes_back() {
    let server = TestServer::start().await;
    let mcp = server.mcp(vec![Scope::Admin], None).await;

    let result = call(&mcp, "list_tokens", json!({})).await;
    assert_eq!(result["isError"], false, "{result}");
    // An empty database has no tokens, so the response message has no fields
    // on the wire at all — which decodes to `{}` rather than to a `tokens: []`
    // this codec would have had to invent.
    assert_eq!(result["structuredContent"], json!({}), "{result}");

    server.stop().await;
}

/// The full round trip with real arguments: mint a token, read it back.
#[tokio::test]
async fn arguments_encoded_from_json_arrive_as_the_handler_sees_them() {
    let server = TestServer::start().await;
    let mcp = server.mcp(vec![Scope::Admin], None).await;

    let minted = call(
        &mcp,
        "mint_token",
        json!({ "name": "agent", "scopes": ["mail.read"] }),
    )
    .await;
    assert_eq!(minted["isError"], false, "{minted}");
    let structured = &minted["structuredContent"];
    assert_eq!(structured["name"], "agent", "{minted}");
    assert_eq!(structured["scopes"], json!(["mail.read"]), "{minted}");
    assert!(
        structured["token"]
            .as_str()
            .unwrap_or_default()
            .starts_with("rmail_tok_"),
        "{minted}"
    );

    // ...and the listing now sees it, which means the mutation really landed
    // rather than being encoded into a request the daemon ignored.
    let listed = call(&mcp, "list_tokens", json!({})).await;
    let tokens = listed["structuredContent"]["tokens"]
        .as_array()
        .expect("a token list");
    assert_eq!(tokens.len(), 1, "{listed}");
    assert_eq!(tokens[0]["name"], "agent");

    server.stop().await;
}

/// A server-streaming RPC is drained into a bounded array.
#[tokio::test]
async fn a_streaming_tool_call_returns_a_bounded_frame_array() {
    let server = TestServer::start().await;
    let mcp = server.mcp(vec![Scope::Admin], None).await;

    let result = call(&mcp, "search_mail", json!({ "query": "anything" })).await;
    assert_eq!(result["isError"], false, "{result}");
    let structured = &result["structuredContent"];
    assert!(structured["frames"].is_array(), "{result}");
    // An empty index yields no hits, and the stream ends on its own.
    assert_eq!(structured["frame_count"], 0, "{result}");
    assert_eq!(structured["truncated"], false, "{result}");
    assert_eq!(structured["reason"], "complete", "{result}");

    server.stop().await;
}

/// The frame cap: an unbounded drain of a stream that never ends is a tool
/// call that never returns, so the bound has to actually bind.
#[tokio::test]
async fn a_stream_longer_than_max_frames_returns_a_truncated_prefix() {
    let server = TestServer::start().await;
    // `max_frames: 4` (this harness's default) against six events.
    server.seed_events(6).await;
    let mcp = server.mcp(vec![Scope::Admin], None).await;

    let result = call(
        &mcp,
        "watch_mail_events",
        json!({ "account_id": 1, "since_seq": 0 }),
    )
    .await;
    assert_eq!(result["isError"], false, "{result}");
    let structured = &result["structuredContent"];
    assert_eq!(structured["frame_count"], 4, "{result}");
    assert_eq!(structured["truncated"], true, "{result}");
    assert_eq!(structured["reason"], "frame_limit", "{result}");
    assert_eq!(
        structured["frames"].as_array().map(Vec::len),
        Some(4),
        "the prefix must be the frames themselves, not just a count: {result}"
    );

    server.stop().await;
}

/// The deadline returns the prefix rather than discarding it.
///
/// This is the bug the reviewer caught: wrapping the drain in a `timeout`
/// drops the future and with it every frame already collected, so a
/// `watch_mail_events` that saw two events and then waited reported failure
/// and lost both. The bound belongs inside the loop.
#[tokio::test]
async fn a_stream_that_outlives_the_deadline_still_returns_what_it_read() {
    let server = TestServer::start().await;
    server.seed_events(2).await;
    // Room for ten frames but only a second of patience — and `WatchEvents`
    // keeps the stream open after the backlog, so the deadline is what ends
    // this call.
    let mcp = server
        .mcp_with(
            vec![Scope::Admin],
            None,
            CallLimits {
                max_frames: 10,
                timeout: Duration::from_secs(1),
            },
        )
        .await;

    let result = call(
        &mcp,
        "watch_mail_events",
        json!({ "account_id": 1, "since_seq": 0 }),
    )
    .await;
    assert_eq!(
        result["isError"], false,
        "a deadline with frames in hand is a truncated answer, not a failure: {result}"
    );
    let structured = &result["structuredContent"];
    assert_eq!(structured["frame_count"], 2, "{result}");
    assert_eq!(structured["truncated"], true, "{result}");
    assert_eq!(structured["reason"], "deadline", "{result}");

    server.stop().await;
}

/// An RPC that fails is reported to the model, not to a client-side error
/// path it cannot see.
#[tokio::test]
async fn an_rpc_failure_becomes_an_is_error_result_carrying_the_status() {
    let server = TestServer::start().await;
    let mcp = server.mcp(vec![Scope::Admin], None).await;

    let result = call(&mcp, "get_message", json!({ "id": 999_999 })).await;
    assert_eq!(
        result["isError"], true,
        "a missing message must be reported as a tool error: {result}"
    );
    let text = result["content"][0]["text"].as_str().unwrap_or_default();
    assert!(
        text.contains("NotFound") || text.contains("not found"),
        "the status must reach the model: {text}"
    );

    server.stop().await;
}

/// **Over the Unix socket, a bearer token narrows nothing.**
///
/// This pins a trap worth pinning. `rmaild::auth::principal_scopes` grants
/// `Scope::Admin` to a Unix-socket peer whose uid matches the daemon's *before
/// it looks at the `authorization` header at all* — so an operator who mints a
/// read-only token, passes it to `mail mcp serve`, and connects over the local
/// socket gets **admin** at the daemon. The token is not rejected; it is not
/// consulted.
///
/// The consequence for this surface is the one `mcp_cli`'s docs state: over
/// the local socket, `--scope` is the *only* thing narrowing the agent, which
/// makes it a guardrail on the agent rather than a sandbox around it. The
/// bearer path is real and enforced — `rmaild::auth::tests`'
/// `a_read_only_token_is_physically_denied_delete_and_send` covers it — but it
/// is the TCP listener's story, not this one's.
///
/// If this test ever starts failing because a `mail.read` token *is* refused
/// here, that is good news and the docs above it should change with it.
#[tokio::test]
async fn a_bearer_token_does_not_narrow_a_unix_peer_connection() {
    let server = TestServer::start().await;
    let admin = server.mcp(vec![Scope::Admin], None).await;

    let minted = call(
        &admin,
        "mint_token",
        json!({ "name": "read-only-agent", "scopes": ["mail.read"] }),
    )
    .await;
    let token = minted["structuredContent"]["token"]
        .as_str()
        .expect("a bearer secret")
        .to_owned();

    // Declares `admin` (so the client-side gate allows it) and presents a
    // token that grants only `mail.read`.
    let overstated = server.mcp(vec![Scope::Admin], Some(token)).await;
    assert!(
        overstated
            .visible_tools()
            .iter()
            .any(|tool| tool.name() == "delete_message"),
        "the declared scopes must make this a call the client-side gate allows"
    );

    let result = call(&overstated, "delete_message", json!({ "message_id": 1 })).await;
    let text = result["content"][0]["text"].as_str().unwrap_or_default();
    assert!(
        !text.contains("PermissionDenied"),
        "the Unix-peer path grants admin before the token is read, so this must not be a scope \
         refusal: {text}"
    );
    // It reached the handler and failed on the merits — message 1 does not
    // exist — which is also the proof that a dynamically encoded mutating call
    // gets all the way through.
    assert!(
        text.contains("NotFound") || text.contains("not found"),
        "the call must have reached the handler: {text}"
    );

    server.stop().await;
}

/// The client-side gate refuses a mutation *before* it leaves the process,
/// which over the Unix socket is the only thing that will.
#[tokio::test]
async fn a_read_scoped_connection_refuses_a_mutation_it_never_sends() {
    let server = TestServer::start().await;
    let mcp = server.mcp(vec![Scope::MailRead], None).await;

    let result = call(&mcp, "delete_message", json!({ "message_id": 1 })).await;
    assert_eq!(result["isError"], true, "{result}");
    let text = result["content"][0]["text"].as_str().unwrap_or_default();
    assert!(text.contains("mail.write"), "{text}");
    // Not a NOT_FOUND: the call was refused here, not attempted and failed.
    assert!(
        !text.contains("not found"),
        "the refusal must precede the RPC: {text}"
    );

    server.stop().await;
}

/// A read-scoped connection's listing, over a real connection rather than a
/// constructed one: filtered by scope, and honest about each tool's effect.
#[tokio::test]
async fn a_read_scoped_connection_lists_a_filtered_surface() {
    let server = TestServer::start().await;
    let mcp = server.mcp(vec![Scope::MailRead], None).await;

    let text = mcp
        .handle(&json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }).to_string())
        .await
        .expect("an answer");
    let response: Value = serde_json::from_str(&text).unwrap();
    let tools = response["result"]["tools"].as_array().expect("tools");
    assert!(!tools.is_empty());
    assert!(
        tools.iter().any(|tool| tool["name"] == "search_mail"),
        "a read token must still be offered search"
    );
    assert!(
        !tools.iter().any(|tool| tool["name"] == "delete_message"),
        "a read token must not be offered a delete"
    );
    for tool in tools {
        assert!(
            tool["annotations"]["readOnlyHint"].is_boolean(),
            "{tool} must say whether it is read-only"
        );
    }

    server.stop().await;
}

/// Cancellation must abort an in-flight call rather than leaving the task to
/// discover later that nobody is listening.
#[tokio::test]
async fn a_cancelled_server_stops_answering_calls() {
    let server = TestServer::start().await;
    let cancel = CancellationToken::new();
    let channel = rmail_core::connect_uds(&server.socket).await.unwrap();
    let mcp = McpServer::new(
        channel,
        Principal {
            scopes: vec![Scope::Admin],
            bearer: None,
        },
        CallLimits {
            max_frames: 4,
            timeout: Duration::from_secs(20),
        },
        cancel.clone(),
    )
    .unwrap();

    cancel.cancel();
    let result = call(&mcp, "list_tokens", json!({})).await;
    assert_eq!(result["isError"], true, "{result}");
    let text = result["content"][0]["text"].as_str().unwrap_or_default();
    assert!(text.contains("cancelled"), "{text}");

    server.stop().await;
}
