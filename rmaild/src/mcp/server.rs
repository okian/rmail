//! MCP semantics over JSON-RPC 2.0: `initialize`, `tools/list`, `tools/call`.
//!
//! Transport-free on purpose. [`McpServer::handle`] takes one JSON-RPC message
//! and returns the reply, or `None` for a notification; [`super::transport`]
//! moves those strings over stdio or SSE. Keeping the two apart is what lets
//! the protocol be tested without a socket, a pipe, or a spawned process, and
//! what keeps a second transport from becoming a second implementation of
//! `tools/call`.
//!
//! # Which failures are JSON-RPC errors and which are results
//!
//! The MCP specification draws a line that is easy to get backwards, and
//! getting it backwards costs a model its ability to recover:
//!
//! - **Protocol errors** — an unknown tool, arguments that do not fit the
//!   schema — are JSON-RPC `error` objects. A client is expected to treat them
//!   as "you called this wrong" and, for `-32602`, to let the model correct
//!   itself and retry.
//! - **Execution failures** — the daemon refused the call, the RPC returned
//!   `NOT_FOUND`, the stream died — are *successful* JSON-RPC responses whose
//!   `result` carries `isError: true`. That is what puts the failure text into
//!   the model's context, where it can reason about it, instead of into a
//!   client-side error path the model never sees.
//!
//! A refusal — the caller's scopes do not cover the tool, or this server is
//! serving a read-only surface — is the second kind even though it is caught
//! before the call leaves this process: "you do not have permission to send
//! mail" is something the agent must know and work around, not a malformed
//! request.
//!
//! # Untrusted text
//!
//! Everything a tool returns is mail — text written by whoever sent it, on its
//! way into a model's context. Every string in a result is passed through
//! [`rmail_core::ai::injection::sanitize_model_text`], the same pass the AI
//! pipeline applies before a body reaches Claude, which strips bidi controls
//! and invisible characters. Those are the characters that let a subject line
//! render as one thing and read as another; an MCP surface that skipped the
//! pass would be the one hole in an otherwise-consistent policy, and it would
//! be the hole facing the agent with the most authority.

use std::borrow::Cow;
use std::sync::Arc;

use rmail_core::auth::Scope;
use serde_json::{json, Map, Value};
use tokio_util::sync::CancellationToken;
use tonic::transport::Channel;
use tracing::Instrument as _;

use super::invoke::{self, CallLimits};
use super::projection::ToolSurface;
use super::tools::{Mutations, Visibility};
use super::McpError;

/// MCP protocol revisions this server implements.
///
/// Newest first. A client that asks for one of these is answered with the one
/// it asked for; anything else is answered with the newest, which the
/// specification says is the correct way to say "I do not speak your version,
/// here is mine" — the client then decides whether to continue.
const SUPPORTED_PROTOCOLS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];

/// Who is calling, and what they may do.
///
/// `scopes` filters the projected surface; `bearer` is what the daemon
/// actually authenticates. They are two different things on purpose — see
/// [`super`]'s module docs on where enforcement lives — and the daemon is
/// free to disagree with `scopes`, in which case the call is refused
/// server-side and the refusal is reported to the model.
#[derive(Debug, Clone, Default)]
pub struct Principal {
    /// The scopes the tool list is filtered by.
    pub scopes: Vec<Scope>,
    /// Bearer token presented on every RPC, if any. Absent means the daemon's
    /// Unix-peer-uid path decides (implicit admin for the owning user).
    pub bearer: Option<String>,
    /// Whether this connection offers mutating tools at all.
    ///
    /// Orthogonal to `scopes`, and defaulting to
    /// [`Mutations::AsScoped`] — the setting under which the listing and the
    /// daemon agree exactly. See [`super::tools`] for why the alternative is a
    /// deliberate opt-in rather than what "read-only scopes" implies.
    pub mutations: Mutations,
}

/// An MCP server over one gRPC channel to the daemon.
#[derive(Clone)]
pub struct McpServer {
    channel: Channel,
    surface: Arc<ToolSurface>,
    principal: Arc<Principal>,
    /// Derived from `principal` once, at construction: every listing and every
    /// call goes through it, so the two cannot describe different surfaces.
    visibility: Arc<Visibility>,
    limits: CallLimits,
    cancel: CancellationToken,
}

impl std::fmt::Debug for McpServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("McpServer")
            .field("tools", &self.surface.tools().len())
            .field("scopes", &self.principal.scopes)
            // Never the token itself: this type is `Debug`-printed in traces.
            .field("bearer", &self.principal.bearer.is_some())
            .field("mutations", &self.principal.mutations)
            .field("limits", &self.limits)
            .finish()
    }
}

impl McpServer {
    /// Build a server over `channel`, projecting the whole gRPC surface.
    ///
    /// # Errors
    ///
    /// [`McpError::Descriptor`] if the projection cannot be derived — see
    /// [`ToolSurface::build`], which fails loudly rather than serving a
    /// partial tool list.
    pub fn new(
        channel: Channel,
        principal: Principal,
        limits: CallLimits,
        cancel: CancellationToken,
    ) -> Result<Self, McpError> {
        let surface = ToolSurface::build()?;
        let visibility = Visibility::new(principal.scopes.clone(), principal.mutations);
        tracing::info!(
            tools = surface.tools().len(),
            visible = visibility.list(&surface).count(),
            scopes = %Scope::join(&principal.scopes),
            mutations = visibility.mutations().label(),
            "projected the gRPC surface as MCP tools"
        );
        Ok(Self {
            channel,
            surface: Arc::new(surface),
            principal: Arc::new(principal),
            visibility: Arc::new(visibility),
            limits,
            cancel,
        })
    }

    /// The whole projected surface, unfiltered.
    ///
    /// The seam for a caller that wants to inspect the projection without
    /// speaking JSON-RPC — `mail mcp serve --list`, and task 54's checks that
    /// the PRD's named tool set is present.
    #[must_use]
    pub fn surface(&self) -> &ToolSurface {
        &self.surface
    }

    /// What this connection lists and will call — exactly what `tools/list`
    /// returns, without going through JSON-RPC to get it.
    #[must_use]
    pub fn visible_tools(&self) -> Vec<&super::projection::Tool> {
        self.visibility.list(&self.surface).collect()
    }

    /// The policy behind [`McpServer::visible_tools`], for a caller that needs
    /// to report *why* the listing is what it is (`mail mcp serve --list`).
    #[must_use]
    pub fn visibility(&self) -> &Visibility {
        &self.visibility
    }

    /// Handle one JSON-RPC message. `None` means it was a notification and
    /// there is nothing to send back.
    pub async fn handle(&self, message: &str) -> Option<String> {
        let response = self.dispatch(message).await?;
        match serde_json::to_string(&response) {
            Ok(text) => Some(text),
            Err(error) => {
                // Serializing a `Value` we built ourselves cannot fail in
                // practice; if it somehow did, the client is owed *something*
                // on the wire rather than a silently dropped reply.
                tracing::error!(%error, "could not serialize an MCP response");
                Some(
                    r#"{"jsonrpc":"2.0","id":null,"error":{"code":-32603,"message":"response could not be serialized"}}"#
                        .to_owned(),
                )
            }
        }
    }

    /// The typed half of [`McpServer::handle`].
    async fn dispatch(&self, message: &str) -> Option<Value> {
        let request: Value = match serde_json::from_str(message) {
            Ok(value) => value,
            Err(error) => {
                return Some(error_response(
                    Value::Null,
                    -32700,
                    &format!("could not parse JSON: {error}"),
                ))
            }
        };

        // JSON-RPC batching was removed in MCP 2025-06-18, and accepting it
        // silently would mean answering an array with a single object.
        if request.is_array() {
            return Some(error_response(
                Value::Null,
                -32600,
                "JSON-RPC batches are not supported; send one request per message",
            ));
        }

        let id = request.get("id").cloned().unwrap_or(Value::Null);
        let Some(method) = request.get("method").and_then(Value::as_str) else {
            return Some(error_response(id, -32600, "no \"method\" in the request"));
        };
        let params = request.get("params").cloned().unwrap_or(Value::Null);

        // A request without an `id` is a notification: the specification
        // forbids replying to one at all, including with an error.
        let is_notification = request.get("id").is_none();

        let result = match method {
            "initialize" => Ok(self.initialize(&params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(self.list_tools()),
            "tools/call" => self.call_tool(&params).await,
            // Notifications this server does not need to act on, named
            // explicitly so a genuinely unknown one is still reported when it
            // carries an id.
            "notifications/initialized" | "notifications/cancelled" => return None,
            other => Err(McpError::Protocol(format!("unknown method {other:?}"))),
        };

        if is_notification {
            if let Err(error) = result {
                tracing::warn!(method, %error, "an MCP notification failed");
            }
            return None;
        }
        Some(match result {
            Ok(value) => json!({ "jsonrpc": "2.0", "id": id, "result": value }),
            Err(error) => {
                tracing::warn!(method, %error, "an MCP request failed");
                error_response(id, error.code(), &error.to_string())
            }
        })
    }

    /// `initialize`: announce the protocol revision and what this server can
    /// do.
    fn initialize(&self, params: &Value) -> Value {
        let requested = params.get("protocolVersion").and_then(Value::as_str);
        let version = requested
            .filter(|v| SUPPORTED_PROTOCOLS.contains(v))
            .unwrap_or_else(|| SUPPORTED_PROTOCOLS[0]);
        if requested.is_some_and(|r| r != version) {
            tracing::info!(
                requested = requested.unwrap_or_default(),
                offered = version,
                "the MCP client asked for a protocol revision this build does not implement"
            );
        }
        json!({
            "protocolVersion": version,
            "capabilities": {
                // `listChanged: false` is the truth and worth stating: the
                // surface is derived from a descriptor set compiled into this
                // binary, so it cannot change while the process runs.
                "tools": { "listChanged": false },
            },
            "serverInfo": {
                "name": "rmail",
                "title": "rmail — local mail, projected from gRPC",
                "version": env!("CARGO_PKG_VERSION"),
            },
            "instructions": format!(
                "Every tool here is one rmail gRPC method, generated from the compiled service \
                 definitions. Tools marked readOnlyHint observe; the rest change mail, spend at \
                 a model provider, or produce something that can. This connection is scoped to: \
                 {}.{}",
                if self.principal.scopes.is_empty() {
                    "no declared scope".to_owned()
                } else {
                    Scope::join(&self.principal.scopes)
                },
                // Stated rather than left to be inferred from a short list: an
                // agent that knows the surface is read-only can say so to its
                // human instead of planning around a tool it will be refused.
                //
                // Worded to what `Effect` actually draws the line at, because
                // this string ends up in the model's context and the model
                // will repeat it: search still appends to the local learning
                // log (`search.learning`), so "changes nothing at all" would
                // be a claim this server does not deliver.
                match self.principal.mutations {
                    Mutations::AsScoped => "",
                    Mutations::Withheld =>
                        " This server was started read-only: every tool that changes mail, spends \
                         at a model provider, or produces something that can, is withheld, and \
                         calling one is refused here rather than by the daemon. Searching still \
                         records locally what it showed you.",
                }
            ),
        })
    }

    /// `tools/list`, filtered to what this caller may actually invoke.
    fn list_tools(&self) -> Value {
        let tools: Vec<Value> = self
            .visibility
            .list(&self.surface)
            .map(super::projection::Tool::to_json)
            .collect();
        json!({ "tools": tools })
    }

    /// `tools/call`.
    async fn call_tool(&self, params: &Value) -> Result<Value, McpError> {
        let name = params
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| McpError::Protocol("tools/call needs a \"name\"".to_owned()))?;
        let arguments = match params.get("arguments") {
            None | Some(Value::Null) => Value::Object(Map::new()),
            Some(value) => value.clone(),
        };

        let tool = match self.visibility.authorize(&self.surface, name) {
            Ok(tool) => tool,
            // A refusal — for want of a scope, or because this server is
            // read-only — is an execution outcome the agent has to work
            // around, not a malformed request. See this module's own docs; the
            // text differs because the two have different fixes.
            Err(error @ (McpError::Denied { .. } | McpError::Withheld { .. })) => {
                // Traced here rather than by `dispatch`, which only sees the
                // `Err` arm and never this one — without this line the single
                // path an operator most wants to see (an agent repeatedly
                // reaching for mutations it does not have) produces no
                // structured output at all.
                tracing::warn!(
                    tool = name,
                    reason = if matches!(error, McpError::Withheld { .. }) {
                        "withheld"
                    } else {
                        "denied"
                    },
                    scopes = %Scope::join(self.visibility.granted()),
                    "refused an MCP tool call before it left this process"
                );
                return Ok(tool_error(&error.to_string()));
            }
            Err(error) => return Err(error),
        };

        let span = tracing::info_span!(
            "mcp.tools/call",
            tool = tool.name(),
            rpc = tool.rpc(),
            effect = ?tool.effect(),
            streaming = tool.is_streaming(),
            frames = tracing::field::Empty,
            truncation = tracing::field::Empty,
            outcome = tracing::field::Empty,
        );
        // `Instrument`, not `span.enter()`. An `Entered` guard held across the
        // `.await` below stays entered on the worker thread while this task is
        // parked on the gRPC round trip, so every *other* task polled on that
        // thread would emit its events inside this span. Under `--sse`, where
        // connections are spawned onto a multi-thread runtime alongside the
        // daemon's own tasks, that is real cross-task trace contamination.
        let outcome = invoke::call(
            &self.channel,
            tool,
            &arguments,
            self.limits,
            self.principal.bearer.as_deref(),
            &self.cancel,
        )
        .instrument(span.clone())
        .await;

        match outcome {
            Ok(outcome) => {
                span.record("truncation", tracing::field::debug(outcome.truncation));
                span.record("outcome", "ok");
                let mut value = outcome.value;
                sanitize(&mut value);
                if let Some(frames) = value.get("frame_count").and_then(Value::as_u64) {
                    span.record("frames", frames);
                }
                let text = serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".to_owned());
                Ok(json!({
                    "content": [{ "type": "text", "text": text }],
                    "structuredContent": value,
                    "isError": false,
                }))
            }
            // Everything reachable from here is an execution failure: the
            // daemon refused it, the RPC errored, we ran out of time, the
            // channel was unusable, or we are shutting down. All of it belongs
            // in the model's context rather than in a client-side error path
            // the model never sees.
            Err(
                error @ (McpError::Rpc(_)
                | McpError::Cancelled
                | McpError::Timeout { .. }
                | McpError::Unavailable(_)),
            ) => {
                span.record("outcome", "error");
                Ok(tool_error(&error.to_string()))
            }
            Err(error) => Err(error),
        }
    }
}

/// A `tools/call` result carrying a failure the model should see.
///
/// Sanitized like any other tool output, and for the same reason: a failure
/// message is not necessarily this daemon's own words. A `tonic::Status`
/// message is `rmail_core::Error`'s `Display`, which for (say) a failed
/// `test_account_connection` carries the remote IMAP server's own response
/// text — attacker-influenced text on its way into a model's context, which
/// is exactly what the sanitizing pass exists for.
fn tool_error(message: &str) -> Value {
    let mut text = Value::String(message.to_owned());
    sanitize(&mut text);
    json!({
        "content": [{ "type": "text", "text": text }],
        "isError": true,
    })
}

/// A JSON-RPC error object.
fn error_response(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

/// Strip bidi controls and invisible characters from every string in `value`,
/// in place.
///
/// See this module's docs: mail text on its way into a model's context gets
/// the same pass `rmail_core::ai` already applies on its own paths. Object
/// *keys* are proto field names from the compiled descriptor set, not
/// attacker-controlled, so only values are rewritten — and map keys, which do
/// come from data, are rewritten with them.
fn sanitize(value: &mut Value) {
    match value {
        Value::String(text) => {
            if let Cow::Owned(clean) = rmail_core::ai::injection::sanitize_model_text(text) {
                *text = clean;
            }
        }
        Value::Array(items) => items.iter_mut().for_each(sanitize),
        Value::Object(map) => {
            let rewritten: Vec<String> = map
                .keys()
                .filter(|key| {
                    matches!(
                        rmail_core::ai::injection::sanitize_model_text(key),
                        Cow::Owned(_)
                    )
                })
                .cloned()
                .collect();
            for key in rewritten {
                if let Some(mut entry) = map.remove(&key) {
                    sanitize(&mut entry);
                    let clean = rmail_core::ai::injection::sanitize_model_text(&key).into_owned();
                    map.insert(clean, entry);
                }
            }
            map.values_mut().for_each(sanitize);
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

#[cfg(test)]
mod tests;
