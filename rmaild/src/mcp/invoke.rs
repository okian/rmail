//! Dispatching a projected tool call onto the daemon's gRPC surface.
//!
//! # One channel, no extra hop
//!
//! The tool call becomes a gRPC request on the [`Channel`] this process
//! already holds — the same connection every other RPC in the process uses.
//! Nothing is re-serialized through a second local server, no second socket
//! is bound, and no subprocess is spawned; the MCP adapter is a *client* of
//! the daemon in-process, exactly as the CLI and TUI are.
//!
//! # A codec that carries bytes
//!
//! tonic's generated clients name a concrete prost type per method, which is
//! the per-RPC code the projection exists to avoid. [`RawCodec`] sidesteps it:
//! [`super::codec`] has already turned the caller's JSON into the request
//! message's wire bytes, so tonic is asked to frame and send an opaque
//! `Vec<u8>` and to hand back the response's bytes unparsed. Length-prefix
//! framing, compression, trailers and status handling stay tonic's job; only
//! the message body is ours.
//!
//! # Streams are bounded twice
//!
//! `tools/call` is a request/response exchange with no way to say "and here
//! is another frame later," while several projected RPCs stream indefinitely
//! (`MailService/WatchEvents` ends when the mailbox does, which is never). A
//! call therefore returns a *prefix*: at most `max_frames` messages, and at
//! most `timeout` of wall clock, whichever comes first, with `truncated`
//! saying which. Both bounds are real — an unbounded drain of `WatchEvents`
//! is a tool call that never returns and a process that grows until it dies.
//!
//! Cancellation is threaded rather than implied: the daemon's shutdown token
//! aborts an in-flight drain, and every request carries a gRPC deadline so
//! the server stops working on it too rather than filling a queue nobody is
//! reading.

use std::time::Duration;

use bytes::{Buf as _, BufMut as _};
use serde_json::{json, Value};
use tokio_stream::StreamExt as _;
use tokio_util::sync::CancellationToken;
use tonic::codec::{Codec, DecodeBuf, Decoder, EncodeBuf, Encoder};
use tonic::transport::Channel;
use tonic::{Request, Status};

use super::descriptor::{catalog, Catalog};
use super::projection::Tool;
use super::{codec, McpError};

/// How a call was bounded, so the answer can say what it left out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Truncation {
    /// The stream ended on its own; the frames are all of them.
    Complete,
    /// `max_frames` was reached and the stream was dropped.
    FrameLimit,
    /// The deadline expired first.
    Deadline,
}

impl Truncation {
    /// Whether frames were left unread.
    #[must_use]
    pub const fn is_truncated(self) -> bool {
        !matches!(self, Truncation::Complete)
    }

    fn as_str(self) -> &'static str {
        match self {
            Truncation::Complete => "complete",
            Truncation::FrameLimit => "frame_limit",
            Truncation::Deadline => "deadline",
        }
    }
}

/// What one tool call produced.
#[derive(Debug, Clone)]
pub struct CallOutcome {
    /// The decoded response(s), already JSON.
    pub value: Value,
    /// Whether — and why — the answer is a prefix.
    pub truncation: Truncation,
}

/// Bounds applied to a single tool call.
#[derive(Debug, Clone, Copy)]
pub struct CallLimits {
    /// The most stream frames one call drains.
    pub max_frames: usize,
    /// Wall-clock budget for the whole call, also sent as the gRPC deadline.
    pub timeout: Duration,
}

/// Everything one dynamically dispatched RPC needs that a [`Tool`] would
/// otherwise carry.
///
/// Split out from `Tool` so the same dispatch serves a caller that has no
/// projection at all: `mail api call` (task 42) resolves a method against a
/// [`Catalog`] built from the daemon's reflection response, which yields
/// exactly these five facts and nothing about MCP tool names or scopes.
#[derive(Debug, Clone, Copy)]
pub struct RawCall<'a> {
    /// What to call this method in an error message — the MCP tool name, or
    /// the method spelling the operator typed.
    pub label: &'a str,
    /// The gRPC path, `/rmail.v1.MailService/Get`.
    pub path: &'a str,
    /// Fully-qualified request message name.
    pub input_type: &'a str,
    /// Fully-qualified response message name.
    pub output_type: &'a str,
    /// Whether the server may send more than one response message.
    pub server_streaming: bool,
}

/// Run `tool` with `arguments`, over `channel`.
///
/// # Errors
///
/// [`McpError::InvalidArguments`] if the arguments do not fit the request
/// message, [`McpError::Rpc`] if the daemon refused or failed the call
/// (including the `PERMISSION_DENIED` a caller gets when the client-side
/// gating in [`super::projection`] and the server's own table disagree), and
/// [`McpError::Wire`]/[`McpError::Descriptor`] if a response body does not
/// match the compiled descriptor set.
pub async fn call(
    channel: &Channel,
    tool: &Tool,
    arguments: &Value,
    limits: CallLimits,
    bearer: Option<&str>,
    cancel: &CancellationToken,
) -> Result<CallOutcome, McpError> {
    call_dynamic(
        channel,
        catalog()?,
        &RawCall {
            label: tool.name(),
            path: tool.rpc(),
            input_type: tool.input_type(),
            output_type: tool.output_type(),
            server_streaming: tool.is_streaming(),
        },
        arguments,
        limits,
        bearer,
        cancel,
    )
    .await
}

/// [`call`] against an explicit descriptor set, for a caller with no
/// projection.
///
/// `catalog` is a parameter rather than [`catalog()`] because the two callers
/// disagree about which descriptor set is authoritative, on purpose: the MCP
/// projection is a surface *this process* serves, so the compiled-in set is
/// the right one; `mail api call` is a separate process talking to whatever
/// `rmaild` is on the far end of the socket, so the set that server's
/// reflection service reports is the right one.
///
/// # Errors
///
/// As [`call`].
pub async fn call_dynamic(
    channel: &Channel,
    catalog: &Catalog,
    call: &RawCall<'_>,
    arguments: &Value,
    limits: CallLimits,
    bearer: Option<&str>,
    cancel: &CancellationToken,
) -> Result<CallOutcome, McpError> {
    let body = codec::encode(catalog, call.input_type, arguments)?;

    let path = http::uri::PathAndQuery::try_from(call.path).map_err(|e| {
        McpError::Descriptor(format!("{} is not a valid gRPC path: {e}", call.path))
    })?;

    let mut grpc = tonic::client::Grpc::new(channel.clone());
    let mut request = Request::new(body);
    // The server-side half of the bound below. Without it a cancelled drain
    // leaves the daemon working on a stream nobody will read.
    request.set_timeout(limits.timeout);
    if let Some(token) = bearer {
        let header = format!("Bearer {token}").parse().map_err(|_| {
            // The token came from a command line or an env var, so a value
            // that cannot be a header is an operator mistake worth naming —
            // and worth naming *without* echoing the secret into a log.
            McpError::InvalidArguments(
                "the bearer token contains characters that cannot go in an HTTP header".to_owned(),
            )
        })?;
        request.metadata_mut().insert("authorization", header);
    }

    // One instant for the whole call, so `ready`, the request and the drain
    // share a budget rather than each getting a fresh `limits.timeout`.
    let deadline = tokio::time::Instant::now() + limits.timeout;
    let timed_out = || McpError::Timeout {
        tool: call.label.to_owned(),
        after: limits.timeout,
    };

    let work = async {
        // Inside the budget, not before it: `Channel::poll_ready` pends while
        // tower's buffer is saturated, so a `ready()` awaited outside would
        // hang past both the deadline and the cancellation token.
        tokio::time::timeout_at(deadline, grpc.ready())
            .await
            .map_err(|_| timed_out())?
            .map_err(|e| McpError::Unavailable(format!("{e}")))?;

        if call.server_streaming {
            drain(
                &mut grpc,
                catalog,
                request,
                path,
                call,
                limits.max_frames,
                deadline,
            )
            .await
        } else {
            let response = tokio::time::timeout_at(deadline, grpc.unary(request, path, RawCodec))
                .await
                .map_err(|_| timed_out())??;
            let value = codec::decode(catalog, call.output_type, response.get_ref())?;
            Ok(CallOutcome {
                value,
                truncation: Truncation::Complete,
            })
        }
    };

    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(McpError::Cancelled),
        outcome = work => outcome,
    }
}

/// Drain a server stream into an array, stopping at `max_frames` or at
/// `deadline`, whichever comes first.
///
/// The deadline is enforced *here* rather than by wrapping the whole call in a
/// `timeout`, and the difference is the point: a timeout around the drain
/// drops the future, and with it every frame already collected — so a
/// `watch_mail_events` that saw three events and then waited would report
/// failure and lose the three. Ending the loop instead returns the prefix and
/// says why it stopped, which is what this function promises its caller and
/// what the tool description promises the model.
async fn drain(
    grpc: &mut tonic::client::Grpc<Channel>,
    catalog: &Catalog,
    request: Request<Vec<u8>>,
    path: http::uri::PathAndQuery,
    call: &RawCall<'_>,
    max_frames: usize,
    deadline: tokio::time::Instant,
) -> Result<CallOutcome, McpError> {
    let mut stream =
        tokio::time::timeout_at(deadline, grpc.server_streaming(request, path, RawCodec))
            .await
            .map_err(|_| McpError::Timeout {
                tool: call.label.to_owned(),
                after: deadline.elapsed(),
            })??
            .into_inner();

    let mut frames = Vec::new();
    let mut truncation = Truncation::Complete;
    loop {
        let next = tokio::select! {
            biased;
            () = tokio::time::sleep_until(deadline) => {
                truncation = Truncation::Deadline;
                break;
            }
            next = stream.next() => next,
        };
        match next {
            None => break,
            Some(Ok(bytes)) => {
                frames.push(codec::decode(catalog, call.output_type, &bytes)?);
                if frames.len() >= max_frames {
                    truncation = Truncation::FrameLimit;
                    break;
                }
            }
            // The `grpc-timeout` this call carried is the server's half of the
            // same bound. Reaching it with a prefix in hand is the identical
            // outcome to reaching ours, so it is reported the same way rather
            // than discarding frames the caller already paid for.
            Some(Err(status))
                if status.code() == tonic::Code::DeadlineExceeded && !frames.is_empty() =>
            {
                truncation = Truncation::Deadline;
                break;
            }
            Some(Err(status)) => return Err(status.into()),
        }
    }
    // Dropping the stream sends RST_STREAM, which is what tells the daemon to
    // stop producing — the same cancellation every other streaming client in
    // this workspace relies on.
    drop(stream);

    let count = frames.len();
    Ok(CallOutcome {
        value: json!({
            "frames": frames,
            "frame_count": count,
            "truncated": truncation.is_truncated(),
            "reason": truncation.as_str(),
        }),
        truncation,
    })
}

/// A tonic codec whose messages are already-encoded protobuf bodies.
#[derive(Debug, Clone, Copy, Default)]
struct RawCodec;

impl Codec for RawCodec {
    type Encode = Vec<u8>;
    type Decode = Vec<u8>;
    type Encoder = RawCodec;
    type Decoder = RawCodec;

    fn encoder(&mut self) -> Self::Encoder {
        *self
    }

    fn decoder(&mut self) -> Self::Decoder {
        *self
    }
}

impl Encoder for RawCodec {
    type Item = Vec<u8>;
    type Error = Status;

    fn encode(&mut self, item: Self::Item, dst: &mut EncodeBuf<'_>) -> Result<(), Self::Error> {
        // `EncodeBuf` grows on demand; reserving first keeps a large request
        // to one allocation rather than one per doubling.
        dst.reserve(item.len());
        dst.put_slice(&item);
        Ok(())
    }
}

impl Decoder for RawCodec {
    type Item = Vec<u8>;
    type Error = Status;

    fn decode(&mut self, src: &mut DecodeBuf<'_>) -> Result<Option<Self::Item>, Self::Error> {
        // tonic has already framed the message: `src` holds exactly one
        // message body, so "everything remaining" is the whole of it.
        let mut out = vec![0u8; src.remaining()];
        src.copy_to_slice(&mut out);
        Ok(Some(out))
    }
}
