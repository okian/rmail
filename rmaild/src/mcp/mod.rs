//! The MCP adapter: every gRPC method this daemon serves, projected as a
//! Model Context Protocol tool (task 53).
//!
//! prd.md's design invariant ends *"If gRPC can do it, Claude can do it (via
//! MCP auto-projection)"*, and the word that does the work is **projection**.
//! There is no table of tools in this module. [`projection::ToolSurface`]
//! walks the compiled descriptor set and joins each method against the two
//! tables that already describe it — `rmail_core::parity` for its name,
//! description and effect, `crate::auth::methods` for the scope it needs — so
//! a new RPC becomes a new tool the moment it compiles, and a *missing* row
//! fails the build of the surface by name rather than quietly shipping a
//! shorter tool list.
//!
//! # The four layers
//!
//! | module | what it owns |
//! |---|---|
//! | [`descriptor`] | the compiled `FileDescriptorSet`, indexed by name |
//! | [`schema`] | a request message -> the tool's `inputSchema` (the "arg mapping") |
//! | [`projection`] | the join, the tool list, and scope gating |
//! | [`codec`] | JSON <-> protobuf wire, dynamically, from the descriptor |
//! | [`invoke`] | one tool call -> one gRPC request on the daemon's channel |
//! | [`server`] | MCP/JSON-RPC semantics: `initialize`, `tools/list`, `tools/call` |
//! | [`transport`] | stdio and SSE, both feeding [`server::McpServer`] |
//!
//! # Where enforcement lives
//!
//! The surface a caller sees is filtered by its granted scopes, and a call it
//! is not scoped for is refused before it leaves this process. Neither is the
//! security boundary. Every request still crosses [`crate::AuthLayer`], which
//! fails closed against the same table; the filtering here exists so an agent
//! is not shown tools it will only be refused, and so a refusal names the
//! scope to mint rather than surfacing as an opaque `PERMISSION_DENIED` in the
//! middle of a task. If the two ever disagree, the daemon wins.
//!
//! # What a tool call is bounded by
//!
//! A `tools/call` is request/response, and several projected RPCs stream
//! without end (`MailService/WatchEvents` stops when the mailbox does). A call
//! therefore returns a bounded *prefix* and says so — see [`invoke`] for the
//! two bounds and why both are needed.

mod codec;
mod descriptor;
mod invoke;
pub mod projection;
mod schema;
mod server;
mod transport;

pub use invoke::{CallLimits, CallOutcome, Truncation};
pub use projection::{Tool, ToolSurface};
pub use server::{McpServer, Principal};
pub use transport::{serve_sse, serve_stdio};

/// Everything the MCP adapter can fail at.
///
/// Deliberately *not* mapped to [`tonic::Status`]: this module is a gRPC
/// **client**, and the only `Status` it ever holds is one the daemon sent it.
/// The boundary these errors cross is JSON-RPC, so [`McpError::code`] maps
/// them to JSON-RPC error codes instead — see CLAUDE.md's rule about
/// `tonic::Status` living only at the gRPC boundary.
#[derive(Debug, thiserror::Error)]
pub enum McpError {
    /// The compiled descriptor set does not contain something the projection
    /// needs. Always a build-time inconsistency (a missing parity row, a
    /// missing scope row, an unresolvable type), never a bad request.
    #[error("the projection cannot be built: {0}")]
    Descriptor(String),

    /// A protobuf body did not match the descriptor that claims to describe
    /// it.
    #[error("malformed protobuf: {0}")]
    Wire(String),

    /// No RPC projects to the tool name the client called.
    #[error("no such tool: {0}")]
    UnknownTool(String),

    /// The tool exists, and the caller's scopes do not cover it.
    #[error("{tool} requires {requires}, which this caller does not hold")]
    Denied {
        /// The tool that was refused.
        tool: String,
        /// What the caller would need, in the words `mail token create` takes.
        requires: String,
    },

    /// The `arguments` object does not fit the RPC's request message.
    #[error("invalid arguments: {0}")]
    InvalidArguments(String),

    /// The client sent something that is not a well-formed JSON-RPC request.
    #[error("invalid JSON-RPC request: {0}")]
    Protocol(String),

    /// The daemon refused or failed the call.
    ///
    /// Boxed because `tonic::Status` carries its metadata map inline and is an
    /// order of magnitude larger than every other variant here — unboxed it
    /// would set the size of every `Result` in the codec, which is on the hot
    /// path for each field of each message (`clippy::result_large_err`).
    #[error("the rmail daemon returned {}: {}", .0.code(), .0.message())]
    Rpc(Box<tonic::Status>),

    /// The transport failed (a broken pipe on stdio, a closed socket on SSE).
    #[error("transport error: {0}")]
    Io(#[from] std::io::Error),

    /// The server is shutting down, or the caller went away, while a call was
    /// in flight.
    #[error("the call was cancelled")]
    Cancelled,

    /// *This process* gave up waiting, rather than the daemon reporting
    /// anything.
    ///
    /// Distinct from a [`McpError::Rpc`] carrying `DEADLINE_EXCEEDED` on
    /// purpose: synthesizing a `Status` locally and rendering it as "the rmail
    /// daemon returned DeadlineExceeded" tells the model the server failed
    /// when in fact the client stopped waiting, which is a different thing to
    /// reason about and a different thing to retry.
    #[error("{tool} did not finish within {after:?}")]
    Timeout {
        /// The tool that ran out of time.
        tool: String,
        /// How long it was given.
        after: std::time::Duration,
    },

    /// The channel to the daemon was not usable — it is not running, or its
    /// request buffer never became ready. Also local knowledge, for the reason
    /// [`McpError::Timeout`] gives.
    #[error("the rmail daemon is not accepting requests: {0}")]
    Unavailable(String),
}

/// Written out rather than derived with `#[from]`, because the variant boxes
/// its payload — but `?` on a `Result<_, tonic::Status>` must keep working, or
/// every gRPC call site grows a `map_err`.
impl From<tonic::Status> for McpError {
    fn from(status: tonic::Status) -> Self {
        McpError::Rpc(Box::new(status))
    }
}

impl McpError {
    /// The JSON-RPC error code this maps to.
    ///
    /// The three standard codes carry real meaning to a client:
    /// `-32601` (method not found) makes an MCP client re-read the tool list,
    /// `-32602` (invalid params) makes a model correct its arguments and
    /// retry, and `-32603` (internal) does not. Collapsing everything into
    /// `-32603` — the tempting shortcut — turns a fixable argument mistake
    /// into a dead end.
    #[must_use]
    pub const fn code(&self) -> i64 {
        match self {
            McpError::Protocol(_) => -32600,
            McpError::UnknownTool(_) => -32601,
            McpError::InvalidArguments(_) => -32602,
            // Application-defined range (-32000..=-32099). A denial is not a
            // malformed request and not a server fault; a client that retried
            // it identically would be refused identically.
            McpError::Denied { .. } => -32003,
            McpError::Cancelled => -32001,
            McpError::Timeout { .. } => -32002,
            McpError::Unavailable(_) => -32004,
            McpError::Descriptor(_) | McpError::Wire(_) | McpError::Rpc(_) | McpError::Io(_) => {
                -32603
            }
        }
    }
}
