//! Driven against a real HTTP server on loopback rather than a mocked
//! client, so the request asserted on is the one `reqwest` would actually
//! send, and the bytes decoded are the ones a real SSE response would
//! produce — chunked, unbuffered, over a socket this test controls.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::StreamExt;
use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

use super::*;
use crate::ErrorReason;

// ---------------------------------------------------------------------------
// The mock server
// ---------------------------------------------------------------------------

/// What one request looked like, as the server saw it.
#[derive(Debug, Clone)]
struct Seen {
    api_key: Option<String>,
    anthropic_version: Option<String>,
    body: serde_json::Value,
}

/// How the mock answers one connection.
#[derive(Debug, Clone)]
enum Reply {
    /// A complete JSON body, sent with a `Content-Length`.
    Json { status: u16, body: String },
    /// A Server-Sent-Events body: no `Content-Length`, each chunk written and
    /// flushed as its own `write`. `close_after` controls whether the
    /// connection closes once every chunk is sent (a clean end-of-stream) or
    /// is left open indefinitely (to exercise a caller that must cancel
    /// rather than wait forever).
    Sse {
        status: u16,
        chunks: Vec<String>,
        close_after: bool,
    },
}

/// An HTTP server that answers from a queue of canned replies and records
/// what it was asked. A queue rather than one fixed reply so a test can
/// express "fail twice, then succeed" — the retry behavior worth testing
/// spans several requests, not one.
struct Server {
    endpoint: String,
    seen: Arc<Mutex<Vec<Seen>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        // A `JoinHandle` does not abort on drop, so without this every test
        // leaves an accept loop and a bound port running for the life of the
        // process.
        self.task.abort();
    }
}

impl Server {
    async fn json(status: u16, body: impl Into<String>) -> Self {
        Self::queued(vec![Reply::Json {
            status,
            body: body.into(),
        }])
        .await
    }

    async fn sse(chunks: Vec<String>) -> Self {
        Self::queued(vec![Reply::Sse {
            status: 200,
            chunks,
            close_after: true,
        }])
        .await
    }

    /// Like [`Server::sse`], but the connection is never closed by the
    /// server — only a client that cancels ends it.
    async fn sse_hanging(chunks: Vec<String>) -> Self {
        Self::queued(vec![Reply::Sse {
            status: 200,
            chunks,
            close_after: false,
        }])
        .await
    }

    /// Answer each request from `replies` in turn, repeating the last one
    /// once the queue is exhausted.
    async fn queued(replies: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        let replies = Arc::new(Mutex::new(VecDeque::from(replies)));
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let recorder = Arc::clone(&recorder);
                let reply = {
                    let mut queue = replies
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let fallback = Reply::Json {
                        status: 500,
                        body: String::new(),
                    };
                    if queue.len() > 1 {
                        queue.pop_front().unwrap_or(fallback)
                    } else {
                        queue.front().cloned().unwrap_or(fallback)
                    }
                };
                tokio::spawn(handle_connection(stream, recorder, reply));
            }
        });
        Self {
            endpoint: format!("http://{addr}/v1/messages"),
            seen,
            task,
        }
    }

    fn requests(&self) -> Vec<Seen> {
        self.seen.lock().map(|log| log.clone()).unwrap_or_default()
    }
}

async fn handle_connection(mut stream: TcpStream, recorder: Arc<Mutex<Vec<Seen>>>, reply: Reply) {
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    let Some((head_end, length)) = read_request_head(&mut stream, &mut raw, &mut buf).await else {
        return;
    };
    let text = String::from_utf8_lossy(&raw).to_string();
    let seen = Seen {
        api_key: header_value(&text, "x-api-key"),
        anthropic_version: header_value(&text, "anthropic-version"),
        body: serde_json::from_str(&String::from_utf8_lossy(&raw[head_end..head_end + length]))
            .unwrap_or(serde_json::Value::Null),
    };
    if let Ok(mut log) = recorder.lock() {
        log.push(seen);
    }
    match reply {
        Reply::Json { status, body } => {
            let response = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.flush().await;
        }
        Reply::Sse {
            status,
            chunks,
            close_after,
        } => {
            let header = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: text/event-stream\r\nConnection: {}\r\n\r\n",
                if close_after { "close" } else { "keep-alive" }
            );
            if stream.write_all(header.as_bytes()).await.is_err() {
                return;
            }
            let _ = stream.flush().await;
            for chunk in &chunks {
                if stream.write_all(chunk.as_bytes()).await.is_err() {
                    return;
                }
                let _ = stream.flush().await;
            }
            if close_after {
                let _ = stream.shutdown().await;
            } else {
                // No further bytes, and the socket stays open: a caller that
                // does not cancel would wait here. The test's own tokio
                // runtime tears this task down when the test returns.
                tokio::time::sleep(Duration::from_secs(30)).await;
            }
        }
    }
}

/// Read until the request's headers and body have both fully arrived.
async fn read_request_head(
    stream: &mut TcpStream,
    raw: &mut Vec<u8>,
    buf: &mut [u8; 4096],
) -> Option<(usize, usize)> {
    loop {
        let n = stream.read(buf).await.unwrap_or(0);
        if n == 0 {
            return None;
        }
        raw.extend_from_slice(&buf[..n]);
        let text = String::from_utf8_lossy(raw).to_string();
        if let Some(at) = text.find("\r\n\r\n") {
            let length = header_value(&text, "content-length")
                .and_then(|v| v.parse::<usize>().ok())
                .unwrap_or(0);
            if raw.len() >= at + 4 + length {
                return Some((at + 4, length));
            }
        }
    }
}

/// A case-insensitive `name: value` header lookup over raw request text.
fn header_value(text: &str, name: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim()
            .eq_ignore_ascii_case(name)
            .then(|| value.trim().to_owned())
    })
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn test_config(api_key_command: &str) -> AiConfig {
    AiConfig {
        api_key_command: api_key_command.to_owned(),
        // Fast, tiny backoff so retry tests do not spend real wall-clock
        // time waiting out a policy meant for a live, rate-limited API.
        retry: AiRetry {
            max_attempts: 3,
            base_delay_ms: 2,
            max_delay_ms: 10,
        },
        ..AiConfig::default()
    }
}

fn provider(server: &Server) -> ClaudeProvider {
    ClaudeProvider::new(&test_config("printf secret-key"))
        .unwrap()
        .with_endpoint(&server.endpoint)
}

fn usage_json(input: u64, output: u64, cache_creation: u64, cache_read: u64) -> serde_json::Value {
    json!({
        "input_tokens": input,
        "output_tokens": output,
        "cache_creation_input_tokens": cache_creation,
        "cache_read_input_tokens": cache_read,
    })
}

fn message_body(text: &str, stop_reason: &str, usage: serde_json::Value) -> String {
    json!({
        "id": "msg_1",
        "model": "claude-haiku-4-5",
        "content": [{"type": "text", "text": text}],
        "stop_reason": stop_reason,
        "usage": usage,
    })
    .to_string()
}

fn refusal_body(category: &str, explanation: &str) -> String {
    json!({
        "id": "msg_refused",
        "model": "claude-opus-4-8",
        "content": [],
        "stop_reason": "refusal",
        "stop_details": {"category": category, "explanation": explanation},
        "usage": usage_json(5, 0, 0, 0),
    })
    .to_string()
}

/// Format one SSE event the way the real API does: an `event: <type>` line
/// naming the event, then the `data:` line carrying its JSON body. The
/// decoder only reads `data:` lines, but the fixtures should still look like
/// the real wire format rather than a simplified stand-in for it.
fn sse_event(value: serde_json::Value) -> String {
    let kind = value
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("message");
    format!("event: {kind}\ndata: {value}\n\n")
}

/// A complete, well-formed streaming session: `message_start` through
/// `message_stop`, with `delta` text deltas in between.
fn sse_session(
    text_deltas: &[&str],
    stop_reason: &str,
    initial_usage: serde_json::Value,
) -> Vec<String> {
    let mut chunks = vec![sse_event(json!({
        "type": "message_start",
        "message": {"id": "msg_1", "model": "claude-haiku-4-5", "usage": initial_usage},
    }))];
    chunks.push(sse_event(
        json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
    ));
    for delta in text_deltas {
        chunks.push(sse_event(json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": delta},
        })));
    }
    chunks.push(sse_event(json!({"type": "content_block_stop", "index": 0})));
    chunks.push(sse_event(json!({
        "type": "message_delta",
        "delta": {"stop_reason": stop_reason},
        "usage": {"output_tokens": text_deltas.len()},
    })));
    chunks.push(sse_event(json!({"type": "message_stop"})));
    chunks
}

async fn collect_frames(mut stream: ProviderStream) -> Vec<Result<StreamFrame, Error>> {
    let mut out = Vec::new();
    while let Some(frame) = stream.next().await {
        out.push(frame);
    }
    out
}

/// Assert `frame` is `Ok(expected)`.
///
/// A hand-rolled comparison rather than `assert_eq!`, because [`Error`] does
/// not implement `PartialEq` — its `Display` text is a client-facing message,
/// not a value meant to be compared — so `Result<StreamFrame, Error>` cannot
/// be compared with `==` even when only the `Ok` side is under test.
#[track_caller]
fn assert_frame(frame: &Result<StreamFrame, Error>, expected: StreamFrame) {
    match frame {
        Ok(actual) => assert_eq!(*actual, expected),
        // `unreachable!`, not `panic!`: this workspace denies `panic!`
        // outside tests and does not carve out a test exemption for it (only
        // `unwrap`/`expect` get one, via `clippy.toml`).
        Err(e) => unreachable!("expected {expected:?}, got Err({e})"),
    }
}

/// [`assert_frame`], applied element-by-element to a whole stream.
#[track_caller]
fn assert_ok_frames(frames: &[Result<StreamFrame, Error>], expected: &[StreamFrame]) {
    assert_eq!(frames.len(), expected.len(), "{frames:?}");
    for (frame, expected) in frames.iter().zip(expected) {
        assert_frame(frame, expected.clone());
    }
}

fn no_cancel() -> CancellationToken {
    CancellationToken::new()
}

// ---------------------------------------------------------------------------
// complete(): decoding, structured output, cache headers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_completion_is_decoded_and_the_key_comes_from_the_command() {
    let server = Server::json(
        200,
        message_body("hello", "end_turn", usage_json(10, 5, 0, 0)),
    )
    .await;
    let p = provider(&server);
    let request = ChatRequest::new("claude-haiku-4-5", 100).user("hi");

    let response = p.complete(&request, &no_cancel()).await.unwrap();

    assert_eq!(response.text, "hello");
    assert_eq!(response.stop_reason, StopReason::EndTurn);
    assert_eq!(response.usage.input_tokens, 10);
    assert_eq!(response.usage.output_tokens, 5);

    let seen = server.requests();
    assert_eq!(seen.len(), 1);
    assert_eq!(
        seen[0].api_key.as_deref(),
        Some("secret-key"),
        "the key is read from the command's stdout, not hardcoded"
    );
    assert_eq!(
        seen[0].anthropic_version.as_deref(),
        Some(ANTHROPIC_VERSION)
    );
    assert_eq!(seen[0].body["model"], "claude-haiku-4-5");
    assert_eq!(seen[0].body["max_tokens"], 100);
    assert_eq!(seen[0].body["stream"], false);
    assert_eq!(seen[0].body["messages"][0]["role"], "user");
    assert_eq!(seen[0].body["messages"][0]["content"], "hi");
}

#[derive(Debug, Deserialize, PartialEq)]
struct Triage {
    category: String,
    priority: String,
}

#[tokio::test]
async fn structured_output_is_decoded_without_regex() {
    let server = Server::json(
        200,
        message_body(
            r#"{"category":"invoice","priority":"high"}"#,
            "end_turn",
            usage_json(20, 8, 0, 0),
        ),
    )
    .await;
    let p = provider(&server);
    let schema = json!({
        "type": "object",
        "properties": {"category": {"type": "string"}, "priority": {"type": "string"}},
        "required": ["category", "priority"],
        "additionalProperties": false,
    });
    let request = ChatRequest::new("claude-haiku-4-5", 100)
        .user("triage this")
        .output_format(OutputFormat::json_schema(schema.clone()));

    let response = p.complete(&request, &no_cancel()).await.unwrap();
    let triage: Triage = response.structured().unwrap();

    assert_eq!(
        triage,
        Triage {
            category: "invoice".to_owned(),
            priority: "high".to_owned(),
        }
    );
    let seen = server.requests();
    assert_eq!(
        seen[0].body["output_config"]["format"]["type"],
        "json_schema"
    );
    assert_eq!(seen[0].body["output_config"]["format"]["schema"], schema);
}

#[tokio::test]
async fn structured_output_that_does_not_match_is_an_internal_error() {
    // The API is supposed to guarantee this never happens; if it does, that
    // is the provider's contract breaking, not the caller's mistake — hence
    // `Internal`, not `InvalidArgument`.
    let server = Server::json(
        200,
        message_body("not json at all", "end_turn", usage_json(1, 1, 0, 0)),
    )
    .await;
    let p = provider(&server);
    let request = ChatRequest::new("claude-haiku-4-5", 100)
        .user("x")
        .output_format(OutputFormat::json_schema(json!({"type": "object"})));

    let response = p.complete(&request, &no_cancel()).await.unwrap();
    let err = response.structured::<Triage>().unwrap_err();
    assert_eq!(err.reason(), ErrorReason::Internal);
}

#[tokio::test]
async fn a_frozen_system_prompt_is_sent_with_an_hour_long_cache_control() {
    let server = Server::json(
        200,
        message_body("ok", "end_turn", usage_json(1, 1, 0, 500)),
    )
    .await;
    let p = provider(&server); // default prompt_cache: enabled, ttl 1h
    let request = ChatRequest::new("claude-opus-4-8", 100)
        .system("you are a triage assistant")
        .user("...");

    let response = p.complete(&request, &no_cancel()).await.unwrap();

    assert_eq!(
        response.usage.cache_read_input_tokens, 500,
        "usage.cache_read_input_tokens is what a caller verifies caching against"
    );
    let seen = server.requests();
    let system = &seen[0].body["system"];
    assert_eq!(system[0]["type"], "text");
    assert_eq!(system[0]["text"], "you are a triage assistant");
    assert_eq!(system[0]["cache_control"]["type"], "ephemeral");
    assert_eq!(system[0]["cache_control"]["ttl"], "1h");
}

#[tokio::test]
async fn a_short_configured_ttl_renders_as_five_minutes() {
    let server = Server::json(200, message_body("ok", "end_turn", usage_json(1, 1, 0, 0))).await;
    let mut config = test_config("printf k");
    config.prompt_cache.ttl = crate::config::HumanDuration::new(Duration::from_secs(60));
    let p = ClaudeProvider::new(&config)
        .unwrap()
        .with_endpoint(&server.endpoint);
    let request = ChatRequest::new("claude-haiku-4-5", 10)
        .system("s")
        .user("u");

    p.complete(&request, &no_cancel()).await.unwrap();

    assert_eq!(
        server.requests()[0].body["system"][0]["cache_control"]["ttl"],
        "5m"
    );
}

#[tokio::test]
async fn disabling_prompt_caching_sends_a_plain_system_string() {
    let server = Server::json(200, message_body("ok", "end_turn", usage_json(1, 1, 0, 0))).await;
    let mut config = test_config("printf k");
    config.prompt_cache.enabled = false;
    let p = ClaudeProvider::new(&config)
        .unwrap()
        .with_endpoint(&server.endpoint);
    let request = ChatRequest::new("claude-haiku-4-5", 10)
        .system("plain system")
        .user("u");

    p.complete(&request, &no_cancel()).await.unwrap();

    let seen = server.requests();
    assert_eq!(
        seen[0].body["system"], "plain system",
        "no cache_control, and no array wrapper, when caching is off"
    );
}

// ---------------------------------------------------------------------------
// Retry / backoff
// ---------------------------------------------------------------------------

#[tokio::test]
async fn retryable_failures_are_retried_with_backoff_then_succeed() {
    let server = Server::queued(vec![
        Reply::Json {
            status: 500,
            body: r#"{"error":"try again"}"#.to_owned(),
        },
        Reply::Json {
            status: 429,
            body: r#"{"error":"rate limited"}"#.to_owned(),
        },
        Reply::Json {
            status: 200,
            body: message_body("recovered", "end_turn", usage_json(1, 1, 0, 0)),
        },
    ])
    .await;
    let p = provider(&server);
    let request = ChatRequest::new("claude-haiku-4-5", 10).user("x");

    let response = p.complete(&request, &no_cancel()).await.unwrap();

    assert_eq!(response.text, "recovered");
    assert_eq!(server.requests().len(), 3, "one request per attempt");
}

#[tokio::test]
async fn a_non_retryable_status_is_not_retried() {
    let server = Server::json(400, r#"{"error":"bad request"}"#.to_owned()).await;
    let p = provider(&server); // max_attempts: 3
    let request = ChatRequest::new("claude-haiku-4-5", 10).user("x");

    let err = p.complete(&request, &no_cancel()).await.unwrap_err();

    assert_eq!(err.reason(), ErrorReason::InvalidArgument);
    assert_eq!(
        server.requests().len(),
        1,
        "a 400 is the caller's mistake, not a transient fault"
    );
}

#[tokio::test]
async fn retries_are_exhausted_and_the_final_status_is_reported() {
    let server = Server::json(500, r#"{"error":"down"}"#.to_owned()).await;
    let p = provider(&server); // max_attempts: 3

    let err = p
        .complete(
            &ChatRequest::new("claude-haiku-4-5", 10).user("x"),
            &no_cancel(),
        )
        .await
        .unwrap_err();

    assert_eq!(err.reason(), ErrorReason::Unavailable);
    assert_eq!(server.requests().len(), 3);
}

#[tokio::test]
async fn unauthorized_and_rate_limited_are_told_apart() {
    // "Fix your key" and "try again later" call for opposite responses; one
    // error for both sends an operator to the wrong place half the time.
    let unauthorized = Server::json(401, r#"{"error":"bad key"}"#.to_owned()).await;
    let err = provider(&unauthorized)
        .complete(
            &ChatRequest::new("claude-haiku-4-5", 10).user("x"),
            &no_cancel(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.reason(), ErrorReason::Unauthenticated);
    assert_eq!(unauthorized.requests().len(), 1, "401 is not retried");

    let forbidden = Server::json(403, r#"{"error":"no access"}"#.to_owned()).await;
    let err = provider(&forbidden)
        .complete(
            &ChatRequest::new("claude-haiku-4-5", 10).user("x"),
            &no_cancel(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.reason(), ErrorReason::Unauthenticated);
    assert_eq!(forbidden.requests().len(), 1, "403 is not retried");

    let limited = Server::json(429, r#"{"error":"slow down"}"#.to_owned()).await;
    let err = provider(&limited)
        .complete(
            &ChatRequest::new("claude-haiku-4-5", 10).user("x"),
            &no_cancel(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.reason(), ErrorReason::Unavailable);
    assert_eq!(
        limited.requests().len(),
        3,
        "429 is retried up to max_attempts, unlike 401/403"
    );
}

// ---------------------------------------------------------------------------
// refusal: an error, never retried
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_refusal_is_an_error_and_is_never_retried() {
    let server = Server::json(200, refusal_body("cyber", "declined for policy reasons")).await;
    let p = provider(&server); // max_attempts: 3 — must not matter here

    let err = p
        .complete(
            &ChatRequest::new("claude-opus-4-8", 10).user("x"),
            &no_cancel(),
        )
        .await
        .unwrap_err();

    assert_eq!(err.reason(), ErrorReason::FailedPrecondition);
    assert!(err.to_string().contains("cyber"), "{err}");
    assert_eq!(
        server.requests().len(),
        1,
        "a refusal is a definite answer from a reachable server, not a fault to retry"
    );
}

#[tokio::test]
async fn a_refusal_mid_stream_ends_the_stream_as_an_error_not_a_done_frame() {
    let chunks = vec![
        sse_event(json!({
            "type": "message_start",
            "message": {"id": "msg_1", "model": "claude-opus-4-8", "usage": usage_json(3, 0, 0, 0)},
        })),
        sse_event(
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
        ),
        sse_event(json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": "partial"},
        })),
        sse_event(json!({
            "type": "message_delta",
            "delta": {
                "stop_reason": "refusal",
                "stop_details": {"category": "bio", "explanation": "no"},
            },
            "usage": {"output_tokens": 1},
        })),
    ];
    let server = Server::sse(chunks).await;
    let p = provider(&server);

    let stream = p
        .stream(
            &ChatRequest::new("claude-opus-4-8", 10).user("x"),
            &no_cancel(),
        )
        .await
        .unwrap();
    let frames = collect_frames(stream).await;

    assert_eq!(frames.len(), 2, "{frames:?}");
    assert_frame(&frames[0], StreamFrame::Token("partial".to_owned()));
    let err = frames[1].as_ref().unwrap_err();
    assert_eq!(err.reason(), ErrorReason::FailedPrecondition);
    assert!(err.to_string().contains("bio"), "{err}");
}

// ---------------------------------------------------------------------------
// Streaming frames
// ---------------------------------------------------------------------------

#[tokio::test]
async fn streaming_deltas_map_to_token_usage_and_done_frames() {
    // Non-zero cache fields: the acceptance criterion is specifically that
    // `usage.cache_read_input_tokens` round-trips onto the `Usage` frame, and
    // an all-zero fixture can't distinguish a correct field mapping from a
    // swapped or mistyped one.
    let server = Server::sse(sse_session(
        &["Hello", " world"],
        "end_turn",
        usage_json(7, 0, 40, 500),
    ))
    .await;
    let p = provider(&server);

    let stream = p
        .stream(
            &ChatRequest::new("claude-haiku-4-5", 10).user("x"),
            &no_cancel(),
        )
        .await
        .unwrap();
    let frames = collect_frames(stream).await;

    assert_ok_frames(
        &frames,
        &[
            StreamFrame::Token("Hello".to_owned()),
            StreamFrame::Token(" world".to_owned()),
            StreamFrame::Usage(Usage {
                input_tokens: 7,
                output_tokens: 2,
                cache_creation_input_tokens: 40,
                cache_read_input_tokens: 500,
            }),
            StreamFrame::Done {
                stop_reason: StopReason::EndTurn,
            },
        ],
    );
    assert_eq!(server.requests()[0].body["stream"], true);
}

#[tokio::test]
async fn a_tool_use_block_start_becomes_a_tool_use_start_frame() {
    let chunks = vec![
        sse_event(json!({
            "type": "message_start",
            "message": {"id": "msg_1", "model": "claude-opus-4-8", "usage": usage_json(4, 0, 0, 0)},
        })),
        sse_event(json!({
            "type": "content_block_start",
            "index": 0,
            "content_block": {"type": "tool_use", "id": "toolu_1", "name": "lookup_contact"},
        })),
        sse_event(json!({"type": "content_block_stop", "index": 0})),
        sse_event(json!({
            "type": "message_delta",
            "delta": {"stop_reason": "tool_use"},
            "usage": {"output_tokens": 3},
        })),
        sse_event(json!({"type": "message_stop"})),
    ];
    let server = Server::sse(chunks).await;
    let p = provider(&server);

    let stream = p
        .stream(
            &ChatRequest::new("claude-opus-4-8", 10).user("x"),
            &no_cancel(),
        )
        .await
        .unwrap();
    let frames = collect_frames(stream).await;

    assert_frame(
        &frames[0],
        StreamFrame::ToolUseStart {
            id: "toolu_1".to_owned(),
            name: "lookup_contact".to_owned(),
        },
    );
    assert_frame(
        frames.last().expect("at least one frame"),
        StreamFrame::Done {
            stop_reason: StopReason::ToolUse,
        },
    );
}

#[tokio::test]
async fn a_stream_closed_before_message_stop_is_reported_as_an_error() {
    // A well-behaved server always ends with `message_stop`. One that closes
    // the connection before that must not be mistaken for a clean end — the
    // caller would otherwise treat a truncated turn as a complete one.
    let chunks = vec![
        sse_event(json!({
            "type": "message_start",
            "message": {"id": "msg_1", "model": "claude-haiku-4-5", "usage": usage_json(1, 0, 0, 0)},
        })),
        sse_event(json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": "cut off"},
        })),
    ];
    let server = Server::sse(chunks).await;
    let p = provider(&server);

    let stream = p
        .stream(
            &ChatRequest::new("claude-haiku-4-5", 10).user("x"),
            &no_cancel(),
        )
        .await
        .unwrap();
    let frames = collect_frames(stream).await;

    assert_frame(&frames[0], StreamFrame::Token("cut off".to_owned()));
    let err = frames.last().unwrap().as_ref().unwrap_err();
    assert_eq!(err.reason(), ErrorReason::Unavailable);
}

#[tokio::test]
async fn cancelling_a_pending_stream_stops_it_promptly() {
    let chunks = vec![
        sse_event(json!({
            "type": "message_start",
            "message": {"id": "msg_1", "model": "claude-haiku-4-5", "usage": usage_json(1, 0, 0, 0)},
        })),
        sse_event(json!({
            "type": "content_block_delta",
            "index": 0,
            "delta": {"type": "text_delta", "text": "partial"},
        })),
    ];
    // Hangs after `chunks`: the only way this stream ends is cancellation.
    let server = Server::sse_hanging(chunks).await;
    let p = provider(&server);
    let cancel = CancellationToken::new();

    let mut stream = p
        .stream(&ChatRequest::new("claude-haiku-4-5", 10).user("x"), &cancel)
        .await
        .unwrap();

    // Prove the stream is actually live before cancelling it.
    match stream.next().await {
        Some(Ok(StreamFrame::Token(text))) => assert_eq!(text, "partial"),
        other => unreachable!("expected a Token frame, got {other:?}"),
    }

    cancel.cancel();
    let started = std::time::Instant::now();
    let next = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("cancellation must not require waiting out the server's 30s hang");

    // `Done` or `Err`, never neither (see the `Provider::stream` docs): a
    // cancelled stream must not just stop with no signal at all, or a
    // consumer cannot tell a cut-short turn from a complete one.
    match next {
        Some(Err(e)) => assert_eq!(e.reason(), ErrorReason::DeadlineExceeded),
        other => unreachable!("expected a DeadlineExceeded frame, got {other:?}"),
    }
    assert!(
        stream.next().await.is_none(),
        "nothing follows the cancellation frame"
    );
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "took {:?} to notice cancellation",
        started.elapsed()
    );
}

#[tokio::test]
async fn cancelling_unblocks_a_reader_stuck_writing_to_a_full_channel() {
    // More text deltas than `SSE_CHANNEL_CAPACITY`, delivered as one burst
    // with nothing draining the stream: without racing `cancel` against the
    // channel `send` too (not just against reading more bytes), the reader
    // task would block forever trying to deliver frames past the channel's
    // capacity — holding the upstream connection open indefinitely, which is
    // exactly the "leaked task" this module's cancellation handling exists to
    // rule out.
    let deltas: Vec<String> = (0..64).map(|n| format!("t{n}")).collect();
    let delta_refs: Vec<&str> = deltas.iter().map(String::as_str).collect();
    let server = Server::sse(sse_session(&delta_refs, "end_turn", usage_json(1, 0, 0, 0))).await;
    let p = provider(&server);
    let cancel = CancellationToken::new();

    let stream = p
        .stream(&ChatRequest::new("claude-haiku-4-5", 10).user("x"), &cancel)
        .await
        .unwrap();

    // Give the reader task time to read the whole burst and fill the
    // channel — nothing here drains it, so if the burst arrived in one
    // network read the reader is now parked in a blocked `send`.
    tokio::time::sleep(Duration::from_millis(100)).await;
    cancel.cancel();

    // Drain everything. If the reader were stuck on a blocked `send` past
    // the cancellation, this would hang until the timeout fires.
    let frames = tokio::time::timeout(Duration::from_secs(5), collect_frames(stream))
        .await
        .expect("a cancelled reader must not hang even with a full, undrained channel");

    assert!(
        !frames.is_empty(),
        "some buffered frames should still arrive"
    );
}

#[tokio::test]
async fn a_cancelled_token_stops_a_completion_before_it_sends_anything() {
    let server = Server::json(
        200,
        message_body("unreachable", "end_turn", usage_json(1, 1, 0, 0)),
    )
    .await;
    let p = provider(&server);
    let cancel = CancellationToken::new();
    cancel.cancel();

    let err = tokio::time::timeout(
        Duration::from_secs(5),
        p.complete(&ChatRequest::new("claude-haiku-4-5", 10).user("x"), &cancel),
    )
    .await
    .expect("a pre-cancelled call must not hang")
    .unwrap_err();

    assert_eq!(err.reason(), ErrorReason::DeadlineExceeded);
    assert!(
        server.requests().is_empty(),
        "a cancelled caller must not have anything in flight"
    );
}

// ---------------------------------------------------------------------------
// Malformed responses, bad configuration, validation
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_body_that_will_not_parse_is_reported_as_an_outage() {
    // Retry policy treats `Internal` as a provider-contract break, not a
    // transient fault. A truncated or malformed upstream body is exactly the
    // kind of thing likely to succeed on the next attempt, so it must not be
    // classified in a way that discourages retrying it in a caller's own loop.
    let server = Server::json(200, "not json at all".to_owned()).await;
    let err = provider(&server)
        .complete(
            &ChatRequest::new("claude-haiku-4-5", 10).user("x"),
            &no_cancel(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.reason(), ErrorReason::Unavailable);
}

#[tokio::test]
async fn an_unrecognized_stop_reason_is_an_internal_error() {
    // A `stop_reason` this build has never heard of is API drift worth
    // surfacing loudly rather than silently coercing to some default.
    let server = Server::json(
        200,
        message_body("x", "some_future_reason", usage_json(1, 1, 0, 0)),
    )
    .await;
    let err = provider(&server)
        .complete(
            &ChatRequest::new("claude-haiku-4-5", 10).user("x"),
            &no_cancel(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.reason(), ErrorReason::Internal);
}

#[tokio::test]
async fn an_upstream_error_body_is_clipped_before_it_reaches_a_client() {
    let huge = "z".repeat(64 * 1024);
    let server = Server::json(400, huge).await;
    let err = provider(&server)
        .complete(
            &ChatRequest::new("claude-haiku-4-5", 10).user("x"),
            &no_cancel(),
        )
        .await
        .unwrap_err();
    assert_eq!(err.reason(), ErrorReason::InvalidArgument);
    assert!(
        err.to_string().len() < 512,
        "the message was {} bytes",
        err.to_string().len()
    );
}

#[tokio::test]
async fn a_missing_api_key_command_fails_at_construction() {
    let err = ClaudeProvider::new(&test_config("   ")).unwrap_err();
    assert_eq!(err.reason(), ErrorReason::FailedPrecondition);
}

#[tokio::test]
async fn a_failing_key_command_is_unauthenticated_and_sends_no_request() {
    let server = Server::json(200, message_body("x", "end_turn", usage_json(1, 1, 0, 0))).await;
    let p = ClaudeProvider::new(&test_config("exit 1"))
        .unwrap()
        .with_endpoint(&server.endpoint);

    let err = p
        .complete(
            &ChatRequest::new("claude-haiku-4-5", 10).user("x"),
            &no_cancel(),
        )
        .await
        .unwrap_err();

    assert_eq!(err.reason(), ErrorReason::Unauthenticated);
    assert!(server.requests().is_empty(), "no request without a key");
}

#[tokio::test]
async fn an_invalid_request_is_rejected_before_any_network_call() {
    let server = Server::json(200, message_body("x", "end_turn", usage_json(1, 1, 0, 0))).await;
    let p = provider(&server);

    let no_messages = ChatRequest::new("claude-haiku-4-5", 10);
    let err = p.complete(&no_messages, &no_cancel()).await.unwrap_err();
    assert_eq!(err.reason(), ErrorReason::InvalidArgument);

    let zero_tokens = ChatRequest::new("claude-haiku-4-5", 0).user("x");
    let err = p.complete(&zero_tokens, &no_cancel()).await.unwrap_err();
    assert_eq!(err.reason(), ErrorReason::InvalidArgument);

    assert!(server.requests().is_empty());
}

// ---------------------------------------------------------------------------
// build(): the seam later tasks (a local provider) plug into
// ---------------------------------------------------------------------------

#[test]
fn build_returns_a_claude_provider_for_the_claude_backend() {
    let config = test_config("printf k");
    assert!(build(&config).is_ok());
}

#[test]
fn build_refuses_the_unimplemented_local_backend() {
    let config = AiConfig {
        provider: AiProvider::Local,
        ..test_config("printf k")
    };
    let err = build(&config).unwrap_err();
    assert_eq!(err.reason(), ErrorReason::FailedPrecondition);
}

// ---------------------------------------------------------------------------
// Backoff and jitter: pure functions, tested directly
// ---------------------------------------------------------------------------

#[test]
fn backoff_grows_exponentially_until_it_hits_the_cap() {
    let retry = AiRetry {
        max_attempts: 10,
        base_delay_ms: 100,
        max_delay_ms: 2_000,
    };
    // Bound each attempt against its *own* jitter-free base — never against
    // another attempt's jittered value. Jitter draws are independent, so two
    // delays near the cap can land in either order regardless of which
    // attempt number produced them; the growth claim this test checks is
    // about the base the jitter is applied to, not about the jittered
    // outputs staying sorted.
    for attempt in 1..=8 {
        let uncapped_ms = retry.base_delay_ms.saturating_mul(1u64 << (attempt - 1));
        let base_ms = uncapped_ms.min(retry.max_delay_ms);
        let delay = backoff_delay(&retry, attempt);
        assert!(
            delay >= Duration::from_millis(base_ms / 2) && delay <= Duration::from_millis(base_ms),
            "attempt {attempt}: {delay:?} outside [{}, {base_ms}] ms (uncapped base {uncapped_ms}ms)",
            base_ms / 2
        );
    }
    // By attempt 8, `100ms * 2^7 = 12,800ms` is well past the 2,000ms cap —
    // confirm the *base* the jitter multiplies is actually capped (the
    // jittered value itself never equals the cap exactly, since the jitter
    // multiplier is always strictly less than 1.0).
    let uncapped_ms_at_8 = retry.base_delay_ms.saturating_mul(1u64 << 7);
    assert!(
        uncapped_ms_at_8 > retry.max_delay_ms,
        "test setup: attempt 8 should be past the cap"
    );
    assert!(backoff_delay(&retry, 8) <= Duration::from_millis(retry.max_delay_ms));
}

#[test]
fn backoff_never_returns_zero() {
    // A delay of zero would make "backoff" a no-op — indistinguishable from
    // retrying in a tight loop.
    let retry = AiRetry {
        max_attempts: 3,
        base_delay_ms: 0,
        max_delay_ms: 0,
    };
    for attempt in 1..=5 {
        assert!(backoff_delay(&retry, attempt) >= Duration::from_millis(1));
    }
}

#[test]
fn jitter_multiplier_stays_in_the_documented_range() {
    for _ in 0..1000 {
        let m = jitter_multiplier();
        assert!((0.5..1.0).contains(&m), "{m} outside [0.5, 1.0)");
    }
}

#[test]
fn jitter_multiplier_is_not_a_constant() {
    // Not a statistical claim — just a regression guard against a mixer that
    // always lands on the same value (e.g. a seed that never actually varies).
    let values: std::collections::HashSet<u64> =
        (0..20).map(|_| jitter_multiplier().to_bits()).collect();
    assert!(values.len() > 1, "1000 draws should not all collide");
}

// ---------------------------------------------------------------------------
// SseDecoder: driven directly, byte-by-byte
// ---------------------------------------------------------------------------

#[test]
fn an_event_split_across_many_single_byte_pushes_still_decodes() {
    // The module's own claim is that a chunk boundary can land anywhere,
    // including mid-JSON. One byte at a time is the extreme case of that.
    let event = sse_event(json!({
        "type": "content_block_delta",
        "index": 0,
        "delta": {"type": "text_delta", "text": "hi"},
    }));
    let mut decoder = SseDecoder::new();
    let mut frames = Vec::new();
    for byte in event.as_bytes() {
        frames.extend(decoder.push(std::slice::from_ref(byte)));
    }
    assert_eq!(frames.len(), 1, "{frames:?}");
    assert_frame(&frames[0], StreamFrame::Token("hi".to_owned()));
}

#[test]
fn a_split_multi_byte_character_does_not_produce_a_parse_error() {
    // "café" — the 'é' is two UTF-8 bytes. Split the push exactly inside it.
    let event = sse_event(json!({
        "type": "content_block_delta",
        "index": 0,
        "delta": {"type": "text_delta", "text": "café"},
    }));
    let bytes = event.as_bytes();
    let split_at = event.find("é").expect("test fixture contains é") + 1; // mid-character
    let mut decoder = SseDecoder::new();
    let mut frames = decoder.push(&bytes[..split_at]);
    assert!(frames.is_empty(), "no complete event yet: {frames:?}");
    frames.extend(decoder.push(&bytes[split_at..]));
    assert_eq!(frames.len(), 1, "{frames:?}");
    assert_frame(&frames[0], StreamFrame::Token("café".to_owned()));
}

#[test]
fn a_crlf_line_ending_boundary_is_recognized() {
    let raw = "event: content_block_delta\r\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"hi\"}}\r\n\r\n";
    let mut decoder = SseDecoder::new();
    let frames = decoder.push(raw.as_bytes());
    assert_eq!(frames.len(), 1, "{frames:?}");
    assert_frame(&frames[0], StreamFrame::Token("hi".to_owned()));
}

#[test]
fn a_stream_that_never_sends_a_boundary_is_bounded_not_unbounded() {
    let mut decoder = SseDecoder::new();
    let junk = vec![b'x'; MAX_SSE_BUFFER + 1];
    let frames = decoder.push(&junk);
    assert_eq!(frames.len(), 1, "{frames:?}");
    let err = frames[0].as_ref().unwrap_err();
    assert_eq!(err.reason(), ErrorReason::Unavailable);
    assert!(
        decoder.buffer.is_empty(),
        "the oversized buffer must be dropped"
    );
}
