//! The two ways an MCP client reaches [`super::McpServer`]: stdio and SSE.
//!
//! Both are thin. A transport's whole job is to deliver one JSON-RPC message
//! at a time to [`super::McpServer::handle`] and to put the reply back on the
//! wire; neither knows what a tool is. That is why `mail mcp serve --stdio`
//! and `--sse` cannot answer `tools/list` differently.
//!
//! # stdio
//!
//! Newline-delimited JSON on stdin/stdout, which is what the MCP stdio
//! transport specifies. Two consequences shape the code:
//!
//! - **Nothing but protocol may be written to stdout.** A stray `println!`
//!   corrupts the stream and the client disconnects with a parse error. Logs
//!   go to stderr — which is exactly where `tracing_subscriber`'s default
//!   writer puts them, so this is a rule to not break rather than one to
//!   implement.
//! - **A message must not contain a newline.** `serde_json::to_string`
//!   escapes them inside strings and emits none between tokens, so the
//!   invariant holds by construction rather than by post-processing.
//!
//! Requests are handled one at a time, in arrival order. MCP allows
//! concurrent in-flight requests, and serializing them costs latency when an
//! agent fires several tool calls at once — but the alternative is
//! interleaved writes to a single pipe, which needs a writer task and an
//! ordering story for a gain no MCP client currently asks for. Stated here so
//! the limit is a decision rather than an oversight.
//!
//! # SSE
//!
//! The HTTP transport MCP defined before Streamable HTTP: the client opens
//! `GET /sse` and holds it, receiving an `endpoint` event naming where to
//! POST, then sends each request to `POST /message?sessionId=…` and reads
//! every reply off the stream it is already holding.
//!
//! # What guards the SSE endpoint, and what does not
//!
//! There is no authentication on this port: whoever can reach it and knows a
//! session id gets the scopes `mail mcp serve` was started with. Three things
//! stand in the way, and it is worth being precise about which threat each
//! one answers, because the obvious one answers the least.
//!
//! - **Loopback only.** [`serve_sse`] refuses any other address rather than
//!   warning about it, which keeps the port off the network. It does *not*
//!   keep a browser out: a page the user visits reaches `127.0.0.1` happily.
//! - **`Origin` refused.** [`is_allowed_origin`] rejects any request carrying
//!   one. This is the check that answers the browser, and it is the one the
//!   MCP specification requires for local HTTP transports — a cross-origin
//!   `fetch` with a `text/plain` body is a CORS *simple request*, so no
//!   preflight blocks it and the attacker never needs to read the response to
//!   have caused the send, the delete, or the provider spend.
//! - **Unguessable session ids.** 128 CSPRNG bits ([`session_id`]), so the
//!   remaining path — blind POSTs to guessed ids — is not walkable.
//!
//! What none of them answers is another process on the same host running as
//! another user: a TCP port on loopback has no file permissions, where the
//! daemon's Unix socket does. `--stdio` is the transport with no such
//! exposure, and it is the one MCP clients launch by default.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use http_body_util::combinators::BoxBody;
use http_body_util::{BodyExt as _, Full, Limited, StreamBody};
use hyper::body::{Bytes, Frame, Incoming};
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncBufRead, AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_stream::StreamExt as _;
use tokio_util::sync::CancellationToken;

use super::{McpError, McpServer};

/// One HTTP response body, boxed so the fixed and streaming cases share a
/// type.
type HttpBody = BoxBody<Bytes, Infallible>;

/// The longest single JSON-RPC message either transport accepts.
///
/// A client that sends an unterminated line would otherwise grow this
/// process's memory without bound, and a `tools/call` argument object is
/// kilobytes at the outside. One megabyte is far past anything legitimate and
/// far below anything that hurts.
const MAX_MESSAGE_BYTES: usize = 1024 * 1024;

/// How many replies a single SSE session may have queued before the writer
/// applies backpressure. Small on purpose: a client that is not reading its
/// own stream is one this server should stop working for, not one to buffer
/// for.
const SSE_QUEUE_DEPTH: usize = 32;

/// How many SSE sessions may be open at once.
///
/// A cap rather than a comment, because the registry is keyed by a live client
/// and a client that reconnects on every hiccup is the normal case, not the
/// pathological one. Generous for the intended use (one agent, one stream) and
/// small enough that an unbounded reconnect loop is refused rather than
/// remembered.
const MAX_SSE_SESSIONS: usize = 64;

/// Serve MCP over stdin/stdout until end-of-input or `cancel`.
///
/// # Errors
///
/// [`McpError::Io`] if reading stdin or writing stdout fails for any reason
/// other than the client closing the pipe, which is an ordinary end of
/// session and returns `Ok`.
pub async fn serve_stdio(server: McpServer, cancel: CancellationToken) -> Result<(), McpError> {
    let mut reader = BufReader::new(tokio::io::stdin());
    let mut stdout = tokio::io::stdout();
    tracing::info!("MCP stdio transport ready");

    loop {
        let line = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                tracing::info!("MCP stdio transport shutting down");
                return Ok(());
            }
            line = read_capped_line(&mut reader) => line,
        };
        let line = match line {
            Ok(Some(line)) => line,
            Ok(None) => {
                tracing::info!("MCP client closed stdin");
                return Ok(());
            }
            // Answered rather than dropped: a client that sent something too
            // large is owed an explanation, and `id` is unrecoverable from a
            // message that was never parsed. `read_capped_line` has already
            // resynchronized to the next newline, so the session continues.
            Err(TooLong) => {
                write_line(
                    &mut stdout,
                    &format!(
                        r#"{{"jsonrpc":"2.0","id":null,"error":{{"code":-32600,"message":"message exceeds {MAX_MESSAGE_BYTES} bytes"}}}}"#
                    ),
                )
                .await?;
                continue;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = server.handle(&line).await {
            write_line(&mut stdout, &response).await?;
        }
    }
}

/// A line that ran past [`MAX_MESSAGE_BYTES`]. Carries nothing: the point is
/// that the bytes were *not* accumulated.
struct TooLong;

/// Read one newline-terminated message, refusing to buffer more than
/// [`MAX_MESSAGE_BYTES`] of it.
///
/// `AsyncBufReadExt::lines` and `read_until` both grow their buffer until the
/// delimiter arrives, so a length check afterwards is a check the allocation
/// already happened — which is exactly what the cap exists to prevent. This
/// walks `fill_buf`/`consume` instead, so an over-long line is discarded as it
/// streams and the reader resynchronizes to the next newline rather than
/// leaving a partial line to be parsed as the next message.
///
/// `Ok(None)` is end of input. An I/O failure is mapped to end-of-input by the
/// caller only when it is a disconnect; otherwise it propagates.
async fn read_capped_line<R>(reader: &mut R) -> Result<Option<String>, TooLong>
where
    R: AsyncBufRead + Unpin,
{
    let mut out: Vec<u8> = Vec::new();
    let mut overflowed = false;
    loop {
        let available = match reader.fill_buf().await {
            Ok(bytes) => bytes,
            // A read error ends the session; it is not distinguishable from
            // end-of-input for this transport's purposes, and the caller
            // treats both the same way.
            Err(_) => return Ok(None),
        };
        if available.is_empty() {
            if overflowed {
                return Err(TooLong);
            }
            return Ok((!out.is_empty()).then(|| String::from_utf8_lossy(&out).into_owned()));
        }
        match available.iter().position(|byte| *byte == b'\n') {
            Some(index) => {
                if !overflowed && out.len() + index <= MAX_MESSAGE_BYTES {
                    out.extend_from_slice(&available[..index]);
                } else {
                    overflowed = true;
                }
                reader.consume(index + 1);
                if overflowed {
                    return Err(TooLong);
                }
                return Ok(Some(String::from_utf8_lossy(&out).into_owned()));
            }
            None => {
                let taken = available.len();
                if !overflowed && out.len() + taken <= MAX_MESSAGE_BYTES {
                    out.extend_from_slice(available);
                } else {
                    // Past the cap: keep draining to find the newline, but
                    // stop accumulating. `out` is dropped either way.
                    overflowed = true;
                    out = Vec::new();
                }
                reader.consume(taken);
            }
        }
    }
}

/// Write one newline-delimited message and flush it.
///
/// The flush is load-bearing: stdout is a pipe here, `tokio::io::Stdout`
/// buffers, and an MCP client blocks waiting for a reply that is sitting in
/// this process's buffer.
async fn write_line(stdout: &mut tokio::io::Stdout, message: &str) -> Result<(), McpError> {
    match async {
        stdout.write_all(message.as_bytes()).await?;
        stdout.write_all(b"\n").await?;
        stdout.flush().await
    }
    .await
    {
        Ok(()) => Ok(()),
        Err(error) if is_disconnect(&error) => {
            tracing::info!("MCP client closed stdout");
            Ok(())
        }
        Err(error) => Err(McpError::Io(error)),
    }
}

/// Whether an I/O error is the peer hanging up rather than a real fault.
fn is_disconnect(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
    )
}

/// Serve MCP over HTTP+SSE on `addr` until `cancel`.
///
/// # Errors
///
/// [`McpError::InvalidArguments`] if `addr` is not a loopback address — see
/// this module's docs for why that is refused rather than warned about; and
/// [`McpError::Io`] if the port cannot be bound.
pub async fn serve_sse(
    server: McpServer,
    addr: SocketAddr,
    cancel: CancellationToken,
) -> Result<(), McpError> {
    if !addr.ip().is_loopback() {
        return Err(McpError::InvalidArguments(format!(
            "{addr} is not a loopback address; the MCP SSE endpoint is unauthenticated and \
             grants every caller the scopes this server was started with, so it may only be \
             bound to 127.0.0.1 or ::1"
        )));
    }
    let listener = TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    tracing::info!(%bound, "MCP SSE transport ready at http://{bound}/sse");

    let sessions = Arc::new(Sessions::default());
    loop {
        let (stream, peer) = tokio::select! {
            biased;
            () = cancel.cancelled() => {
                tracing::info!("MCP SSE transport shutting down");
                return Ok(());
            }
            accepted = listener.accept() => match accepted {
                Ok(pair) => pair,
                // One failed accept (a peer that vanished between the SYN and
                // the accept, or a transient fd exhaustion) must not take the
                // listener down with it.
                Err(error) => {
                    tracing::warn!(%error, "MCP SSE accept failed");
                    continue;
                }
            },
        };

        let server = server.clone();
        let sessions = Arc::clone(&sessions);
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let service = service_fn(move |request| {
                let server = server.clone();
                let sessions = Arc::clone(&sessions);
                async move { Ok::<_, Infallible>(route(request, server, sessions).await) }
            });
            let connection = hyper::server::conn::http1::Builder::new()
                // An SSE stream is a response that never ends, so the
                // connection must not be reaped for being idle mid-stream.
                .keep_alive(true)
                .serve_connection(TokioIo::new(stream), service);
            tokio::select! {
                () = cancel.cancelled() => {}
                result = connection => {
                    if let Err(error) = result {
                        tracing::debug!(%peer, %error, "MCP SSE connection ended");
                    }
                }
            }
        });
    }
}

/// The reply queues of the SSE sessions currently open.
#[derive(Default)]
struct Sessions {
    open: std::sync::Mutex<std::collections::HashMap<String, mpsc::Sender<String>>>,
}

impl Sessions {
    /// Register a new session and hand back its id and its frame queue.
    ///
    /// The queue carries whole SSE frames rather than bare JSON so the
    /// mandatory `endpoint` event — which has a different event name from
    /// every frame after it — can be queued here, before the response is
    /// returned and therefore before any `POST /message` can race it.
    ///
    /// `None` once [`MAX_SSE_SESSIONS`] are open.
    fn open(&self) -> Option<(String, mpsc::Receiver<String>)> {
        let id = session_id();
        let (tx, rx) = mpsc::channel(SSE_QUEUE_DEPTH);
        // Capacity is `SSE_QUEUE_DEPTH` and this is the first send, so it
        // cannot fail; ignoring the result keeps the signature honest without
        // an `expect`.
        let _ = tx.try_send(format!(
            "event: endpoint\ndata: /message?sessionId={id}\n\n"
        ));
        let mut open = self.open.lock().ok()?;
        if open.len() >= MAX_SSE_SESSIONS {
            tracing::warn!(
                sessions = open.len(),
                "refusing a new MCP SSE session; the cap is reached"
            );
            return None;
        }
        open.insert(id.clone(), tx);
        Some((id, rx))
    }

    fn sender(&self, id: &str) -> Option<mpsc::Sender<String>> {
        self.open.lock().ok()?.get(id).cloned()
    }

    fn close(&self, id: &str) {
        if let Ok(mut open) = self.open.lock() {
            open.remove(id);
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.open.lock().map_or(0, |open| open.len())
    }
}

/// An unguessable session id.
///
/// 128 bits from the OS CSPRNG, not a pid and a counter. The id is the only
/// thing between a `POST /message` and this server's whole authority, and a
/// pid-plus-counter id is a space small enough to walk from JavaScript: a page
/// the user happens to visit can fire cross-origin POSTs (a `text/plain` body
/// is a CORS *simple* request, so no preflight blocks it) and never needs to
/// read a response to have caused the side effect. [`is_allowed_origin`] is
/// the primary defence; this is the one that holds if the first is ever
/// bypassed.
fn session_id() -> String {
    use argon2::password_hash::rand_core::{OsRng, RngCore};
    use std::fmt::Write as _;

    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    bytes
        .iter()
        .fold(String::with_capacity(32), |mut id, byte| {
            // Writing into a `String` is infallible; the `Result` exists only
            // because `fmt::Write` is generic over sinks that can fail.
            let _ = write!(id, "{byte:02x}");
            id
        })
}

/// Whether a request's `Origin` may be served.
///
/// Only *absent* passes. An MCP client is not a browser and sends no `Origin`;
/// anything that does send one is a web page, and a web page must never be
/// able to drive this endpoint. The MCP specification requires this check on
/// local HTTP transports for exactly that reason, and binding loopback does
/// not substitute for it — a browser on this machine reaches `127.0.0.1`
/// perfectly happily, and `mode: "no-cors"` means the attacker never has to
/// read the response to have caused the send, the delete, or the spend.
/// Takes the headers rather than the request so it can be exercised without
/// constructing a `hyper::body::Incoming`, which has no public constructor.
fn is_allowed_origin(headers: &hyper::HeaderMap) -> bool {
    headers.get(hyper::header::ORIGIN).is_none()
}

/// Removes a session from the registry when its event stream is dropped.
///
/// Hanging the removal off `Drop` rather off the stream ending is not a style
/// choice. The sender lives in [`Sessions`], so the receiver only ends once
/// the sender is removed — and removing it when the stream ended would have
/// been a cycle: the stream waiting on the close, the close waiting on the
/// stream. Hyper *drops* a response body when a client disconnects, which is
/// the event that actually happens, so that is the event this hangs off.
/// Without it every `GET /sse` leaked an entry forever, and MCP clients
/// reconnect their stream routinely.
struct SessionGuard {
    sessions: Arc<Sessions>,
    id: String,
    queue: mpsc::Receiver<String>,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.sessions.close(&self.id);
    }
}

impl tokio_stream::Stream for SessionGuard {
    type Item = String;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.queue.poll_recv(cx)
    }
}

/// The two routes the SSE transport serves.
async fn route(
    request: Request<Incoming>,
    server: McpServer,
    sessions: Arc<Sessions>,
) -> Response<HttpBody> {
    // Checked before the method/path match, so it covers every route this
    // server will ever grow. See `is_allowed_origin`.
    if !is_allowed_origin(request.headers()) {
        tracing::warn!(
            origin = ?request.headers().get(hyper::header::ORIGIN),
            path = request.uri().path(),
            "refused a cross-origin request to the MCP SSE endpoint"
        );
        return text_response(
            StatusCode::FORBIDDEN,
            "this endpoint does not serve browser origins",
        );
    }
    let path = request.uri().path().to_owned();
    match (request.method(), path.as_str()) {
        (&hyper::Method::GET, "/sse") => open_stream(&sessions),
        (&hyper::Method::POST, "/message") => {
            let query = request.uri().query().unwrap_or_default().to_owned();
            post_message(request, &query, server, &sessions).await
        }
        _ => text_response(StatusCode::NOT_FOUND, "not found"),
    }
}

/// `GET /sse`: hand the client its POST endpoint, then relay replies.
fn open_stream(sessions: &Arc<Sessions>) -> Response<HttpBody> {
    let Some((id, queue)) = sessions.open() else {
        return text_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "too many open MCP sessions",
        );
    };

    // The guard removes the session when hyper drops this body — which is
    // what a client disconnect actually looks like. See `SessionGuard`.
    let stream = SessionGuard {
        sessions: Arc::clone(sessions),
        id,
        queue,
    }
    .map(|frame| Ok::<_, Infallible>(Frame::data(Bytes::from(frame))));

    let mut response = Response::new(StreamBody::new(stream).boxed());
    response.headers_mut().insert(
        hyper::header::CONTENT_TYPE,
        hyper::header::HeaderValue::from_static("text/event-stream"),
    );
    response.headers_mut().insert(
        hyper::header::CACHE_CONTROL,
        hyper::header::HeaderValue::from_static("no-store"),
    );
    response
}

/// `POST /message?sessionId=…`: run the request, queue the reply on the
/// session's stream, and acknowledge.
async fn post_message(
    request: Request<Incoming>,
    query: &str,
    server: McpServer,
    sessions: &Arc<Sessions>,
) -> Response<HttpBody> {
    let Some(session) = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("sessionId="))
        .map(str::to_owned)
    else {
        return text_response(StatusCode::BAD_REQUEST, "no sessionId");
    };
    let Some(sender) = sessions.sender(&session) else {
        return text_response(StatusCode::NOT_FOUND, "no such session");
    };

    // `Limited` refuses past the cap *while reading*, rather than after the
    // allocation the cap exists to prevent has already happened.
    let body = match Limited::new(request.into_body(), MAX_MESSAGE_BYTES)
        .collect()
        .await
    {
        Ok(collected) => collected.to_bytes(),
        Err(error) => {
            return text_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                &format!(
                    "could not read the request body within {MAX_MESSAGE_BYTES} bytes: {error}"
                ),
            )
        }
    };
    let Ok(text) = std::str::from_utf8(&body) else {
        return text_response(StatusCode::BAD_REQUEST, "body is not UTF-8");
    };

    if let Some(response) = server.handle(text).await {
        // `try_send`, not `send`. A full queue means the client is not reading
        // the stream it opened, and awaiting would park this POST — holding a
        // connection and a task — until it did. Failing is the honest answer,
        // and it is what this comment claimed before the code matched it.
        if let Err(error) = sender.try_send(format!("event: message\ndata: {response}\n\n")) {
            return match error {
                mpsc::error::TrySendError::Full(_) => text_response(
                    StatusCode::TOO_MANY_REQUESTS,
                    "the event stream is not being read",
                ),
                mpsc::error::TrySendError::Closed(_) => {
                    text_response(StatusCode::GONE, "the event stream is closed")
                }
            };
        }
    }
    text_response(StatusCode::ACCEPTED, "")
}

fn text_response(status: StatusCode, body: &str) -> Response<HttpBody> {
    let mut response = Response::new(Full::new(Bytes::from(body.to_owned())).boxed());
    *response.status_mut() = status;
    response
}

#[cfg(test)]
mod tests;
