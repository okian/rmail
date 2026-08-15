//! Transport framing, session lifecycle, and the two refusals that keep the
//! SSE endpoint from being a web page's remote control.

use super::*;

fn open(sessions: &Sessions) -> (String, mpsc::Receiver<String>) {
    sessions.open().expect("a session under the cap")
}

#[test]
fn a_session_is_handed_its_endpoint_event_before_anything_else() {
    let sessions = Sessions::default();
    let (id, mut queue) = open(&sessions);
    let first = queue
        .try_recv()
        .expect("the endpoint event is already queued");
    assert!(first.starts_with("event: endpoint\n"), "{first}");
    assert!(
        first.ends_with("\n\n"),
        "an SSE frame ends with a blank line"
    );
    assert!(
        first.contains(&format!("sessionId={id}")),
        "the endpoint must name the session: {first}"
    );
}

/// The id is the only thing between a blind `POST /message` and this server's
/// whole authority, so it has to be unguessable rather than merely unique.
#[test]
fn a_session_id_is_long_random_hex_and_never_repeats() {
    let sessions = Sessions::default();
    let mut seen = std::collections::HashSet::new();
    for _ in 0..64 {
        let (id, queue) = open(&sessions);
        assert_eq!(id.len(), 32, "128 bits of hex, got {id:?}");
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()), "not hex: {id:?}");
        assert!(seen.insert(id), "a session id repeated");
        // Held so the guard-free receiver does not free the slot mid-loop.
        drop(queue);
    }
    // A pid-and-counter id would have produced a shared prefix across every
    // one of these; random ones share nothing.
    let prefixes: std::collections::HashSet<&str> = seen.iter().map(|id| &id[..8]).collect();
    assert!(
        prefixes.len() > 60,
        "ids share a prefix, which suggests they are not random: {} distinct",
        prefixes.len()
    );
}

#[test]
fn a_closed_session_can_no_longer_be_posted_to() {
    let sessions = Sessions::default();
    let (id, _queue) = open(&sessions);
    assert!(sessions.sender(&id).is_some());
    sessions.close(&id);
    assert!(
        sessions.sender(&id).is_none(),
        "a POST to a closed session must 404 rather than queue into a stream nobody reads"
    );
}

/// The leak this guard exists for: a disconnecting client must free its slot.
///
/// Before `SessionGuard`, removal hung off the frame stream ending — which
/// could only happen once the sender was removed, which only `close` did. The
/// cycle meant every `GET /sse` leaked an entry forever, and MCP clients
/// reconnect their stream routinely.
#[test]
fn dropping_the_event_stream_frees_the_session() {
    let sessions = Arc::new(Sessions::default());
    let (id, queue) = open(&sessions);
    let guard = SessionGuard {
        sessions: Arc::clone(&sessions),
        id: id.clone(),
        queue,
    };
    assert_eq!(sessions.len(), 1);

    drop(guard);
    assert_eq!(
        sessions.len(),
        0,
        "a disconnected client must not leave its session behind"
    );
    assert!(sessions.sender(&id).is_none());
}

#[test]
fn the_session_registry_is_capped() {
    let sessions = Sessions::default();
    let mut held = Vec::new();
    for _ in 0..MAX_SSE_SESSIONS {
        held.push(sessions.open().expect("under the cap"));
    }
    assert!(
        sessions.open().is_none(),
        "an unbounded reconnect loop must be refused, not remembered"
    );
}

#[test]
fn an_unknown_session_has_no_sender() {
    let sessions = Sessions::default();
    assert!(sessions.sender("not-a-session").is_none());
}

/// The check that answers a browser. Loopback binding does not: a page the
/// user visits reaches `127.0.0.1` perfectly happily, and a `text/plain` body
/// is a CORS simple request with no preflight to block it.
#[test]
fn a_request_carrying_an_origin_is_refused() {
    let mut with_origin = hyper::HeaderMap::new();
    with_origin.insert(
        hyper::header::ORIGIN,
        hyper::header::HeaderValue::from_static("https://evil.example"),
    );
    assert!(
        !is_allowed_origin(&with_origin),
        "a browser origin must never drive this endpoint"
    );
    // Even `null`, which a sandboxed iframe sends.
    let mut null_origin = hyper::HeaderMap::new();
    null_origin.insert(
        hyper::header::ORIGIN,
        hyper::header::HeaderValue::from_static("null"),
    );
    assert!(!is_allowed_origin(&null_origin));

    assert!(
        is_allowed_origin(&hyper::HeaderMap::new()),
        "an MCP client sends no Origin and must be served"
    );
}

/// The refusal that keeps an unauthenticated MCP endpoint off the network.
#[tokio::test]
async fn sse_refuses_to_bind_anything_but_loopback() {
    let server = test_server();

    // Bounded, because the *failure* mode is a hang rather than a wrong
    // answer: a build that dropped the check would bind `0.0.0.0` and serve
    // until cancelled, and an unbounded await here would wedge the suite
    // instead of reporting the regression. (Confirmed by deleting the guard:
    // without this timeout the test ran for nine minutes and had to be
    // killed.)
    let refusal = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        serve_sse(
            server,
            "0.0.0.0:0".parse().expect("a socket address"),
            CancellationToken::new(),
        ),
    )
    .await
    .expect("serve_sse must refuse immediately, not start serving");
    let error = refusal.expect_err("binding a non-loopback address must be refused");
    let text = error.to_string();
    assert!(text.contains("loopback"), "{text}");
}

/// Cancellation has to end the listener, or `mail mcp serve --sse` would
/// ignore Ctrl-C and leave the port bound.
#[tokio::test]
async fn sse_stops_when_cancelled() {
    let cancel = CancellationToken::new();
    let handle = tokio::spawn({
        let cancel = cancel.clone();
        let server = test_server();
        // Port 0: the OS picks a free one, so two runs of the suite in
        // parallel cannot collide.
        async move {
            serve_sse(
                server,
                "127.0.0.1:0".parse().expect("a socket address"),
                cancel,
            )
            .await
        }
    });
    // Give the listener a moment to bind before cancelling, so this exercises
    // the loop's own cancellation arm rather than a race before it starts.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    cancel.cancel();
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), handle)
        .await
        .expect("serve_sse must return promptly after cancellation")
        .expect("the task must not panic");
    assert!(result.is_ok(), "{result:?}");
}

// ---------------------------------------------------------------------------
// stdio framing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_line_is_read_up_to_its_newline() {
    let mut reader = tokio::io::BufReader::new(&b"{\"a\":1}\n{\"b\":2}\n"[..]);
    assert_eq!(
        read_capped_line(&mut reader)
            .await
            .ok()
            .flatten()
            .as_deref(),
        Some("{\"a\":1}")
    );
    assert_eq!(
        read_capped_line(&mut reader)
            .await
            .ok()
            .flatten()
            .as_deref(),
        Some("{\"b\":2}")
    );
    assert!(read_capped_line(&mut reader).await.ok().flatten().is_none());
}

/// The cap has to bite *while reading*. `lines()`/`read_until` would have
/// grown the buffer to the full length first, which is the allocation the cap
/// exists to prevent — a length check afterwards checks that it already
/// happened.
#[tokio::test]
async fn an_over_long_line_is_refused_and_the_reader_resynchronizes() {
    let mut input = vec![b'x'; MAX_MESSAGE_BYTES + 10];
    input.push(b'\n');
    input.extend_from_slice(b"{\"after\":true}\n");
    let mut reader = tokio::io::BufReader::new(&input[..]);

    assert!(
        read_capped_line(&mut reader).await.is_err(),
        "a line past the cap must be refused"
    );
    // ...and the next message is read cleanly rather than as the tail of the
    // one that was refused.
    assert_eq!(
        read_capped_line(&mut reader)
            .await
            .ok()
            .flatten()
            .as_deref(),
        Some("{\"after\":true}")
    );
}

/// A JSON-RPC message can never legally contain a bare newline —
/// `serde_json::to_string` escapes them inside strings and emits none between
/// tokens — which is what makes newline framing safe. Checked against the real
/// serializer rather than asserted in prose.
#[test]
fn a_serialized_response_never_contains_a_newline() {
    let value = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "result": { "text": "a\nb\r\nc", "nested": ["x\ny"] },
    });
    let text = serde_json::to_string(&value).expect("serialize");
    assert!(!text.contains('\n'), "{text}");
    assert!(!text.contains('\r'), "{text}");
}

/// A server with no reachable daemon; every test above answers before a
/// request would be sent.
fn test_server() -> crate::mcp::McpServer {
    let channel = tonic::transport::Channel::from_static("http://127.0.0.1:1").connect_lazy();
    crate::mcp::McpServer::new(
        channel,
        crate::mcp::Principal::default(),
        crate::mcp::CallLimits {
            max_frames: 1,
            timeout: std::time::Duration::from_secs(1),
        },
        CancellationToken::new(),
    )
    .expect("the surface must project")
}
