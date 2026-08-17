//! `mail api call <Method> <json>` — every RPC the daemon serves, from a
//! shell (task 42).
//!
//! # Reflection, not a method table
//!
//! The method being called is resolved against the descriptor set the
//! *daemon's own* `grpc.reflection.v1.ServerReflection` service reports, not
//! against `rmail_proto::FILE_DESCRIPTOR_SET` that this binary was compiled
//! with. A hand-written table would have to be edited for every new RPC and
//! would be wrong the first time somebody forgot; the compiled-in set would be
//! *silently* wrong in a subtler way — a `mail` from one release talking to an
//! `rmaild` from another would encode field numbers the server has moved,
//! producing a request that decodes into the wrong shape rather than an error.
//! Asking the server what it serves makes that a clean "no such method".
//!
//! # One JSON <-> protobuf bridge in this workspace
//!
//! The translation is `rmaild::mcp::codec`, reached through
//! `rmaild::mcp::invoke::call_dynamic`, and not a second implementation. Its
//! edge cases were expensive to get right and are exactly the ones a shell
//! caller hits: 64-bit fields rendered as strings past 2^53 (so `jq` does not
//! quietly round an id), unknown request keys **refused** rather than dropped
//! (so `{"account": 1}` where the field is `account_id` fails instead of
//! returning a mailbox-wide answer that looks filtered), enums by name, and
//! `null` meaning "absent" rather than "zero". A second bridge would have had
//! to re-derive all four, and would drift.
//!
//! # It is not a way around anything
//!
//! The request goes over the same channel, with the same `authorization`
//! header, to the same `rmaild::AuthLayer` as every other verb — which checks
//! the per-method scope requirement and fails closed. There is no client-side
//! allow-list here *by design*: adding one would be a second policy to keep in
//! sync with the daemon's, and the daemon's is the one that is enforceable.
//! What the caller may do is exactly what its principal may do; a method it is
//! not scoped for comes back `PERMISSION_DENIED` and exits 5.
//!
//! And the answer is untrusted data like any other: it is rendered through
//! `crate::format`, whose JSON writer escapes every character a terminal would
//! act on, so a subject carrying an ANSI sequence prints as text.
//!
//! # Streams are buffered, deliberately
//!
//! `call_dynamic` drains a server stream into an array bounded by
//! `--max-frames` and the deadline. That gives up the frame-by-frame latency
//! `mail notify watch --format ndjson` has, and it is the right trade here:
//! this verb exists to *invoke* an arbitrary method, the purpose-built verbs
//! exist to follow a stream, and re-implementing the drain to save a buffer
//! would mean a second copy of the truncation semantics. What comes out still
//! says whether it is a prefix and why.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use anyhow::{Context as _, Result};
use prost::Message as _;
use prost_types::{FileDescriptorProto, FileDescriptorSet};
use rmaild::mcp::descriptor::Catalog;
use rmaild::mcp::{CallLimits, RawCall};
use serde_json::Value;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tonic::transport::Channel;
use tonic_reflection::pb::v1::server_reflection_client::ServerReflectionClient;
use tonic_reflection::pb::v1::server_reflection_request::MessageRequest;
use tonic_reflection::pb::v1::server_reflection_response::MessageResponse;
use tonic_reflection::pb::v1::ServerReflectionRequest;

use crate::client::Client;
use crate::format::{Classified, ExitCode};

/// How many descriptor-fetch rounds the dependency walk will do before giving
/// up.
///
/// `proto/rmail/v1` is one package importing a handful of well-known types, so
/// two rounds is already generous; the bound exists so a server that answered
/// every `FileByFilename` with a file naming a *new* dependency could not spin
/// this loop forever. Reflection responses are attacker-influenced in exactly
/// the same sense every other response is.
const MAX_DEPENDENCY_ROUNDS: usize = 8;

/// Cap on how many descriptor files one `mail api call` will accept.
const MAX_DESCRIPTOR_FILES: usize = 512;

/// Cap on how many requests one reflection exchange will send.
///
/// Rounds alone are not a bound on *work*: one round issues a request per
/// unresolved dependency, and a single `FileDescriptorProto` may declare
/// arbitrarily many — as may `ListServices`. Without this, a server that
/// answered each lookup with a file naming a hundred new imports would drive
/// an unbounded request loop and an unbounded `requested` set inside eight
/// "rounds". Generous next to the ~25 files `proto/rmail/v1` actually has.
const MAX_REFLECTION_REQUESTS: usize = 1024;

/// How long the whole reflection exchange gets.
///
/// The bidi stream has no gRPC deadline of its own unless `--deadline` was
/// given, and a server that accepts the stream and never answers would
/// otherwise wedge `mail api reflect` forever — against this binary's promise
/// that nothing hangs. Overridden by `--deadline` in both directions.
const DEFAULT_REFLECT_TIMEOUT: Duration = Duration::from_secs(20);

/// Everything the daemon says it serves.
pub(crate) struct Reflected {
    /// Service names, in the order reflection listed them.
    pub(crate) services: Vec<String>,
    /// The methods of those services, indexed by descriptor.
    pub(crate) catalog: Catalog,
}

/// Ask the daemon for its descriptor set over `grpc.reflection.v1`.
///
/// # Errors
///
/// [`ExitCode::Unimplemented`] if the daemon serves no reflection service —
/// which is a real answer rather than a transport failure, and names what is
/// missing. Otherwise the RPC's own failure, or a descriptor set that does not
/// decode.
pub(crate) async fn reflect(client: Client, timeout: Option<Duration>) -> Result<Reflected> {
    let budget = timeout.unwrap_or(DEFAULT_REFLECT_TIMEOUT);
    // The whole exchange, not each frame: a server that answers every request
    // one millisecond before a per-frame timeout would still be able to hold
    // the CLI indefinitely across enough frames.
    tokio::time::timeout(budget, reflect_within(client))
        .await
        .map_err(|_| {
            Classified::new(
                ExitCode::DeadlineExceeded,
                format!("the daemon did not finish describing itself within {budget:?}"),
            )
        })?
}

async fn reflect_within(client: Client) -> Result<Reflected> {
    let mut reflection = ServerReflectionClient::new(client);
    // Bounded: the walk below sends at most one request per known file, and a
    // channel that cannot outrun the responses keeps this from buffering a
    // request per descriptor in memory.
    let (tx, rx) = tokio::sync::mpsc::channel(8);
    let mut stream = reflection
        .server_reflection_info(ReceiverStream::new(rx))
        .await
        .map_err(reflection_error)?
        .into_inner();

    // Counted, not just rounded: see `MAX_REFLECTION_REQUESTS`.
    let sent = std::cell::Cell::new(0usize);
    let send = |request: MessageRequest| {
        let tx = tx.clone();
        let count = sent.get() + 1;
        sent.set(count);
        async move {
            if count > MAX_REFLECTION_REQUESTS {
                return Err(Classified::new(
                    ExitCode::ResourceExhausted,
                    format!(
                        "the daemon's reflection service asked for more than \
                         {MAX_REFLECTION_REQUESTS} descriptor lookups to describe itself"
                    ),
                ));
            }
            tx.send(ServerReflectionRequest {
                host: String::new(),
                message_request: Some(request),
            })
            .await
            .map_err(|_| {
                Classified::new(
                    ExitCode::Unavailable,
                    "the reflection stream closed before the request was sent",
                )
            })
        }
    };

    send(MessageRequest::ListServices(String::new())).await?;
    let services = match next_response(&mut stream).await? {
        MessageResponse::ListServicesResponse(list) => {
            list.service.into_iter().map(|s| s.name).collect::<Vec<_>>()
        }
        other => return Err(unexpected(&other, "a service listing")),
    };
    if services.is_empty() {
        return Err(Classified::new(
            ExitCode::Unimplemented,
            "the daemon's reflection service lists no services",
        ));
    }

    // One request per service, then one per still-unresolved dependency. Keyed
    // by file *name* rather than by content: reflection answers a symbol
    // lookup with the file that declares it plus (usually) its transitive
    // imports, so the same file comes back repeatedly and only the first copy
    // is worth keeping.
    let mut files: BTreeMap<String, FileDescriptorProto> = BTreeMap::new();
    let mut requested: BTreeSet<String> = BTreeSet::new();

    for service in &services {
        // The reflection service is itself a service, and asking it to
        // describe itself is noise in a listing of what rmaild does. It is
        // also the one service `mail api call` has no business invoking.
        if service.starts_with("grpc.reflection.") {
            continue;
        }
        send(MessageRequest::FileContainingSymbol(service.clone())).await?;
        collect(next_response(&mut stream).await?, &mut files)?;
    }

    for _ in 0..MAX_DEPENDENCY_ROUNDS {
        let missing: Vec<String> = files
            .values()
            .flat_map(|f| f.dependency.iter().cloned())
            .filter(|dep| !files.contains_key(dep) && !requested.contains(dep))
            .collect();
        if missing.is_empty() {
            break;
        }
        for dep in missing {
            requested.insert(dep.clone());
            send(MessageRequest::FileByFilename(dep)).await?;
            collect(next_response(&mut stream).await?, &mut files)?;
        }
    }

    // Closing the request half tells the daemon the exchange is over; without
    // it the bidi stream stays open until the process exits.
    drop(tx);
    drop(stream);

    let set = FileDescriptorSet {
        file: files.into_values().collect(),
    };
    let catalog = Catalog::build(&set.encode_to_vec())
        .context("indexing the descriptor set the daemon's reflection service returned")?;
    Ok(Reflected { services, catalog })
}

/// Read one reflection response, turning the protocol's own error frame into
/// an error rather than a value.
async fn next_response(
    stream: &mut tonic::Streaming<tonic_reflection::pb::v1::ServerReflectionResponse>,
) -> Result<MessageResponse> {
    let response = stream
        .message()
        .await
        .map_err(reflection_error)?
        .context("the reflection stream ended before answering")?;
    match response.message_response {
        Some(MessageResponse::ErrorResponse(error)) => Err(Classified::new(
            // The reflection service reports its own failures in-band, with a
            // gRPC status code in the payload rather than on the stream. Using
            // that code keeps `mail api call NoSuchService.Method` exiting 6
            // (not found) rather than a generic failure.
            ExitCode::of_status(tonic::Code::from_i32(error.error_code)),
            format!(
                "the daemon's reflection service refused: {}",
                crate::terminal_safe(&error.error_message)
            ),
        )),
        Some(other) => Ok(other),
        None => Err(Classified::new(
            ExitCode::Internal,
            "the daemon's reflection service sent an empty response",
        )),
    }
}

fn collect(
    response: MessageResponse,
    files: &mut BTreeMap<String, FileDescriptorProto>,
) -> Result<()> {
    let MessageResponse::FileDescriptorResponse(payload) = response else {
        return Err(unexpected(&response, "a file descriptor"));
    };
    for bytes in payload.file_descriptor_proto {
        if files.len() >= MAX_DESCRIPTOR_FILES {
            return Err(Classified::new(
                ExitCode::ResourceExhausted,
                format!("the daemon's reflection service returned more than {MAX_DESCRIPTOR_FILES} descriptor files"),
            ));
        }
        let file = FileDescriptorProto::decode(bytes.as_slice())
            .context("decoding a FileDescriptorProto from the reflection response")?;
        files.entry(file.name().to_owned()).or_insert(file);
    }
    Ok(())
}

fn unexpected(response: &MessageResponse, wanted: &str) -> anyhow::Error {
    let got = match response {
        MessageResponse::FileDescriptorResponse(_) => "a file descriptor",
        MessageResponse::AllExtensionNumbersResponse(_) => "extension numbers",
        MessageResponse::ListServicesResponse(_) => "a service listing",
        MessageResponse::ErrorResponse(_) => "an error",
    };
    Classified::new(
        ExitCode::Internal,
        format!("the reflection service answered with {got} where {wanted} was expected"),
    )
}

fn reflection_error(status: tonic::Status) -> anyhow::Error {
    if status.code() == tonic::Code::Unimplemented {
        return Classified::new(
            ExitCode::Unimplemented,
            "this daemon does not serve gRPC reflection, so `mail api reflect`/`mail api call` \
             cannot discover its methods",
        );
    }
    anyhow::Error::new(status).context("the ServerReflection RPC failed")
}

// ---------------------------------------------------------------------------
// Resolving a method name
// ---------------------------------------------------------------------------

/// Resolve the spelling a user typed to exactly one method of `catalog`.
///
/// Accepted spellings, all of which appear in prd.md, a proto file or a gRPC
/// error message at some point:
/// `MailService.List`, `rmail.v1.MailService.List`,
/// `rmail.v1.MailService/List`, `/rmail.v1.MailService/List`.
///
/// # Errors
///
/// [`ExitCode::NotFound`] when nothing matches, or when a bare service name
/// matches more than one package — the ambiguity is reported with the
/// candidates rather than resolved by picking one.
pub(crate) fn resolve<'a>(
    catalog: &'a Catalog,
    spelling: &str,
) -> Result<&'a rmaild::mcp::descriptor::Method> {
    let normalized = spelling.trim().trim_start_matches('/');
    let (service, method) = match normalized.rsplit_once('/') {
        Some(parts) => parts,
        None => normalized.rsplit_once('.').ok_or_else(|| {
            Classified::new(
                ExitCode::Usage,
                format!(
                    "`{}` is not a method name; write it as `MailService.List` or \
                     `rmail.v1.MailService/List`",
                    crate::terminal_safe(spelling)
                ),
            )
        })?,
    };
    if service.is_empty() || method.is_empty() {
        return Err(Classified::new(
            ExitCode::Usage,
            format!(
                "`{}` is not a method name; write it as `MailService.List`",
                crate::terminal_safe(spelling)
            ),
        ));
    }

    let matches: Vec<&rmaild::mcp::descriptor::Method> = catalog
        .methods()
        .iter()
        .filter(|candidate| {
            let Some((candidate_service, candidate_method)) =
                candidate.path.trim_start_matches('/').rsplit_once('/')
            else {
                return false;
            };
            candidate_method == method
                && (candidate_service == service
                    || candidate_service
                        .rsplit_once('.')
                        .is_some_and(|(_, tail)| tail == service))
        })
        .collect();

    match matches.as_slice() {
        [one] => Ok(one),
        [] => Err(Classified::new(
            ExitCode::NotFound,
            format!(
                "the daemon serves no method `{}`. `mail api reflect` lists what it does serve.",
                crate::terminal_safe(spelling)
            ),
        )),
        many => Err(Classified::new(
            ExitCode::Usage,
            format!(
                "`{}` is ambiguous: {}. Qualify it with its package.",
                crate::terminal_safe(spelling),
                // Method paths come off the wire from the reflection service, so
                // they are remote strings like any other and must not be able
                // to drive the terminal they are about to be printed to.
                many.iter()
                    .map(|m| crate::terminal_safe(&m.path))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )),
    }
}

// ---------------------------------------------------------------------------
// Invoking
// ---------------------------------------------------------------------------

/// One invocation: the method, its argument, and the bounds on it.
///
/// A struct rather than eight parameters — the shape of a generic call is
/// exactly the sort of signature where two `&str`s and two numbers end up
/// transposed at a call site and compile perfectly.
#[derive(Clone, Copy)]
pub(crate) struct Invocation<'a> {
    /// The descriptor set the method was resolved against.
    pub(crate) catalog: &'a Catalog,
    /// The method itself.
    pub(crate) method: &'a rmaild::mcp::descriptor::Method,
    /// How the operator spelled it, for error messages.
    pub(crate) label: &'a str,
    /// The request message as JSON.
    pub(crate) arguments: &'a Value,
    /// The most stream frames to drain.
    pub(crate) max_frames: usize,
    /// Wall-clock budget, also sent as the gRPC deadline.
    pub(crate) timeout: Duration,
    /// The bearer token, if `--token`/`$RMAIL_TOKEN` gave one.
    pub(crate) bearer: Option<&'a str>,
}

/// Invoke one method with one JSON request body.
///
/// # Errors
///
/// [`ExitCode::InvalidArgument`] when the JSON does not fit the request
/// message (including an unrecognised key), and whatever the daemon answered
/// otherwise — `PERMISSION_DENIED` for a method this principal is not scoped
/// for, `NOT_FOUND` for a row that is not there.
pub(crate) async fn invoke(channel: &Channel, call: &Invocation<'_>) -> Result<Value> {
    let Invocation {
        catalog,
        method,
        label,
        arguments,
        max_frames,
        timeout,
        bearer,
    } = *call;
    if method.client_streaming {
        return Err(Classified::new(
            ExitCode::Unimplemented,
            format!(
                "{} is client-streaming: one command line carries one request message, so there \
                 is nothing honest to send as the second",
                // Remote text, as above.
                crate::terminal_safe(&method.path)
            ),
        ));
    }

    let outcome = rmaild::mcp::invoke::call_dynamic(
        channel,
        catalog,
        &RawCall {
            label,
            path: &method.path,
            input_type: &method.input_type,
            output_type: &method.output_type,
            server_streaming: method.server_streaming,
        },
        arguments,
        CallLimits {
            max_frames,
            timeout,
        },
        // Passed here rather than through the channel's interceptor, because
        // `call_dynamic` builds its request over a bare `Channel`: an
        // already-intercepted service would put two `authorization` headers on
        // the wire, and a server is entitled to reject that.
        bearer,
        &CancellationToken::new(),
    )
    .await
    .map_err(mcp_error)?;
    Ok(outcome.value)
}

/// Map the bridge's error type onto this binary's exit codes.
///
/// `McpError` maps itself to *JSON-RPC* codes, which is right for the adapter
/// it was written for and meaningless here — so the translation is explicit
/// rather than reused, and each arm says what a shell should conclude.
fn mcp_error(error: rmaild::mcp::McpError) -> anyhow::Error {
    use rmaild::mcp::McpError;
    let code = match &error {
        McpError::InvalidArguments(_) => ExitCode::InvalidArgument,
        McpError::UnknownTool(_) => ExitCode::NotFound,
        McpError::Denied { .. } | McpError::Withheld { .. } => ExitCode::PermissionDenied,
        McpError::Cancelled => ExitCode::Cancelled,
        McpError::Timeout { .. } => ExitCode::DeadlineExceeded,
        McpError::Unavailable(_) => ExitCode::Unavailable,
        McpError::Rpc(status) => ExitCode::of_status(status.code()),
        // `Protocol` is JSON-RPC framing — unreachable from here, because
        // nothing in this path speaks JSON-RPC — but named rather than
        // swept into a `_` arm so a variant added later fails the build
        // instead of silently becoming "internal".
        McpError::Protocol(_) | McpError::Descriptor(_) | McpError::Wire(_) | McpError::Io(_) => {
            ExitCode::Internal
        }
    };
    Classified::new(code, crate::terminal_safe(&error.to_string()))
}

#[cfg(test)]
mod tests;
