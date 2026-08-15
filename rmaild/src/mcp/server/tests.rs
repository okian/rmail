//! MCP protocol semantics, driven without a transport or a daemon.
//!
//! `tools/call` needs a real gRPC server and lives in
//! `rmaild/tests/mcp_server.rs`; everything reachable without one is here,
//! because a protocol bug that only shows up behind a socket is a protocol
//! bug nobody debugs.

use super::*;
use serde_json::json;

/// A server with no reachable daemon.
///
/// Every method exercised here — `initialize`, `ping`, `tools/list`, the
/// error paths of `tools/call` — answers before any request would be sent, so
/// the channel is never used. `Channel::from_static(...).connect_lazy()` is
/// what makes that honest: it builds a channel without dialling, so a test
/// that accidentally *did* reach the RPC would fail loudly on connect rather
/// than quietly pass.
fn server(scopes: Vec<Scope>) -> McpServer {
    let channel = Channel::from_static("http://127.0.0.1:1").connect_lazy();
    McpServer::new(
        channel,
        Principal {
            scopes,
            bearer: None,
        },
        CallLimits {
            max_frames: 8,
            timeout: std::time::Duration::from_secs(5),
        },
        CancellationToken::new(),
    )
    .expect("the surface must project")
}

async fn ask(server: &McpServer, request: Value) -> Value {
    let text = server
        .handle(&request.to_string())
        .await
        .expect("a request with an id must be answered");
    serde_json::from_str(&text).expect("the answer must be JSON")
}

#[tokio::test]
async fn initialize_answers_with_a_protocol_the_client_asked_for() {
    let server = server(vec![Scope::Admin]);
    let response = ask(
        &server,
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2024-11-05" }
        }),
    )
    .await;
    assert_eq!(response["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(response["result"]["serverInfo"]["name"], "rmail");
    assert_eq!(response["id"], 1);
}

#[tokio::test]
async fn an_unknown_protocol_revision_is_answered_with_this_builds_own() {
    let server = server(vec![Scope::Admin]);
    let response = ask(
        &server,
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "1999-01-01" }
        }),
    )
    .await;
    assert_eq!(
        response["result"]["protocolVersion"],
        SUPPORTED_PROTOCOLS[0]
    );
}

#[tokio::test]
async fn a_notification_is_never_answered() {
    let server = server(vec![Scope::Admin]);
    assert!(server
        .handle(&json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }).to_string())
        .await
        .is_none());
    // ...including one naming a method that does not exist: replying to a
    // notification is forbidden even to report the error.
    assert!(server
        .handle(&json!({ "jsonrpc": "2.0", "method": "nonsense" }).to_string())
        .await
        .is_none());
}

#[tokio::test]
async fn tools_list_is_filtered_by_the_callers_scopes() {
    let read = server(vec![Scope::MailRead]);
    let response = ask(
        &read,
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }),
    )
    .await;
    let tools = response["result"]["tools"]
        .as_array()
        .expect("a tool list")
        .clone();
    assert!(!tools.is_empty());
    // The listing is filtered by *scope*, so a tool is present exactly when
    // `mail.read` reaches it — and each is honestly annotated either way. See
    // `projection::tests::MUTATIONS_A_READ_TOKEN_REACHES` for the one row
    // where scope and effect deliberately part company.
    for tool in &tools {
        assert!(
            tool["annotations"]["readOnlyHint"].is_boolean(),
            "{tool} must state whether it is read-only"
        );
    }
    assert!(
        tools.iter().any(|t| t["name"] == "search_mail"),
        "a read token must still be offered search"
    );
    assert!(
        !tools.iter().any(|t| t["name"] == "delete_message"),
        "a read token must not be offered a delete"
    );

    let admin = server(vec![Scope::Admin]);
    let all = ask(
        &admin,
        json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/list" }),
    )
    .await;
    let all_tools = all["result"]["tools"]
        .as_array()
        .expect("a tool list")
        .len();
    assert!(
        all_tools > tools.len(),
        "admin must see more than a read-only token ({all_tools} vs {})",
        tools.len()
    );
}

#[tokio::test]
async fn every_listed_tool_carries_a_schema_a_client_can_use() {
    let server = server(vec![Scope::Admin]);
    let response = ask(
        &server,
        json!({ "jsonrpc": "2.0", "id": 4, "method": "tools/list" }),
    )
    .await;
    for tool in response["result"]["tools"].as_array().expect("tools") {
        assert!(tool["name"].is_string(), "{tool}");
        assert!(tool["description"].is_string(), "{tool}");
        assert_eq!(tool["inputSchema"]["type"], "object", "{tool}");
    }
}

#[tokio::test]
async fn calling_a_tool_that_does_not_exist_is_a_protocol_error() {
    let server = server(vec![Scope::Admin]);
    let response = ask(
        &server,
        json!({
            "jsonrpc": "2.0", "id": 5, "method": "tools/call",
            "params": { "name": "no_such_tool", "arguments": {} }
        }),
    )
    .await;
    assert_eq!(response["error"]["code"], -32601);
}

/// The acceptance's "mutating-tool denial under read token", at the protocol
/// level: the refusal reaches the model as a result it can act on, and it
/// names the scope to mint.
#[tokio::test]
async fn a_mutating_tool_is_denied_to_a_read_only_token() {
    let server = server(vec![Scope::MailRead]);
    let response = ask(
        &server,
        json!({
            "jsonrpc": "2.0", "id": 6, "method": "tools/call",
            "params": { "name": "delete_message", "arguments": { "message_id": 1 } }
        }),
    )
    .await;
    assert_eq!(
        response["result"]["isError"], true,
        "the denial must reach the model: {response}"
    );
    let text = response["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or_default();
    assert!(text.contains("delete_message"), "{text}");
    assert!(text.contains("mail.write"), "{text}");
    assert!(
        response.get("error").is_none(),
        "a denial is a result, not a JSON-RPC error: {response}"
    );
}

/// Arguments that do not fit the request message are a JSON-RPC `-32602`, so
/// the client lets the model correct itself rather than giving up.
#[tokio::test]
async fn arguments_that_do_not_fit_the_schema_are_invalid_params() {
    let server = server(vec![Scope::Admin]);
    let response = ask(
        &server,
        json!({
            "jsonrpc": "2.0", "id": 7, "method": "tools/call",
            "params": { "name": "get_message", "arguments": { "identifier": 1 } }
        }),
    )
    .await;
    assert_eq!(response["error"]["code"], -32602, "{response}");
    let message = response["error"]["message"].as_str().unwrap_or_default();
    assert!(message.contains("identifier"), "{message}");
}

#[tokio::test]
async fn malformed_json_is_a_parse_error_rather_than_a_dropped_message() {
    let server = server(vec![Scope::Admin]);
    let text = server.handle("{not json").await.expect("an answer");
    let response: Value = serde_json::from_str(&text).expect("the answer is JSON");
    assert_eq!(response["error"]["code"], -32700);
}

#[tokio::test]
async fn a_json_rpc_batch_is_refused_rather_than_half_answered() {
    let server = server(vec![Scope::Admin]);
    let text = server
        .handle(&json!([{ "jsonrpc": "2.0", "id": 1, "method": "ping" }]).to_string())
        .await
        .expect("an answer");
    let response: Value = serde_json::from_str(&text).expect("JSON");
    assert_eq!(response["error"]["code"], -32600);
}

#[tokio::test]
async fn ping_answers() {
    let server = server(vec![Scope::MailRead]);
    let response = ask(
        &server,
        json!({ "jsonrpc": "2.0", "id": 8, "method": "ping" }),
    )
    .await;
    assert_eq!(response["result"], json!({}));
}

#[tokio::test]
async fn an_unknown_method_with_an_id_is_reported() {
    let server = server(vec![Scope::Admin]);
    let response = ask(
        &server,
        json!({ "jsonrpc": "2.0", "id": 9, "method": "resources/list" }),
    )
    .await;
    assert_eq!(response["error"]["code"], -32600);
}

#[test]
fn sanitizing_strips_bidi_controls_and_invisibles_from_values() {
    // U+202E RIGHT-TO-LEFT OVERRIDE in a subject is the attack: it renders as
    // one thing and reads as another.
    let mut value = json!({
        "subject": "invoice\u{202e}fdp.exe",
        "body": ["zero\u{200b}width"],
        "nested": { "from": "a\u{2066}b" },
    });
    sanitize(&mut value);
    assert_eq!(value["subject"], "invoicefdp.exe");
    assert_eq!(value["body"][0], "zerowidth");
    assert_eq!(value["nested"]["from"], "ab");
}

#[test]
fn sanitizing_leaves_ordinary_text_untouched() {
    let mut value = json!({ "subject": "Re: lunch — 12:30", "n": 5, "ok": true });
    let before = value.clone();
    sanitize(&mut value);
    assert_eq!(value, before);
}

/// Map keys come from data, so they get the same pass the values do.
#[test]
fn sanitizing_rewrites_a_data_derived_key() {
    let mut value = json!({ "labels": { "urg\u{202e}ent": 1 } });
    sanitize(&mut value);
    assert_eq!(value, json!({ "labels": { "urgent": 1 } }));
}

/// A failure message is not necessarily this daemon's own words: a
/// `tonic::Status` carries `rmail_core::Error`'s `Display`, which for a failed
/// remote login includes the *server's* response text. It gets the same pass
/// the successful path does, or it would be the one string that skipped it.
#[test]
fn a_tool_error_is_sanitized_like_any_other_output() {
    // An RLO filename spoof: `safe<RLO>gnp.exe` renders to a reader as
    // `safeexe.png` while actually naming an executable. Sanitizing must strip
    // the override so the text says what it is.
    let result = tool_error("upstream said: safe\u{202e}gnp.exe");
    assert_eq!(result["isError"], true);
    assert_eq!(result["content"][0]["text"], "upstream said: safegnp.exe");
}

/// A local timeout must not be reported as the daemon having failed: the model
/// retries those differently, and the daemon may be perfectly healthy.
#[test]
fn a_local_timeout_is_not_attributed_to_the_daemon() {
    let error = McpError::Timeout {
        tool: "search_mail".to_owned(),
        after: std::time::Duration::from_secs(30),
    };
    let text = error.to_string();
    assert!(text.contains("search_mail"), "{text}");
    assert!(
        !text.contains("daemon returned"),
        "a client-side give-up must not claim the server answered: {text}"
    );
    assert_eq!(error.code(), -32002);
}
