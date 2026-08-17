//! gRPC -> MCP auto-projection (task 53): the tool surface, generated.
//!
//! prd.md's design invariant has three clauses and this module is the third:
//! *"If gRPC can do it, Claude can do it (via MCP auto-projection)."* The
//! first two are enforced by `rmail_core::parity` and `rmail-cli::parity`;
//! this one is enforced by construction, because the tool list is **derived**
//! rather than written down. There is no table of tools in this file to
//! forget to update.
//!
//! # The join
//!
//! One row per tool comes out of three sources, each keyed by the same string
//! — the fully-qualified gRPC method path:
//!
//! | source | what it contributes |
//! |---|---|
//! | `rmail_proto::FILE_DESCRIPTOR_SET` | the set of RPCs, whether each streams, and its request message's fields (the "arg mapping") |
//! | [`rmail_core::parity::Command`] | the tool's name, its one-line description, and whether it is safe or mutating |
//! | [`crate::auth::methods`] | the capability scope a caller must hold |
//!
//! Task 41 built the middle column for exactly this and left the other two
//! out of it on purpose: the descriptor set already knows about streaming and
//! argument shape, and duplicating the scope table would mean two answers to
//! "may this caller run this tool" — see `rmail_core::parity`'s own module
//! docs. [`ToolSurface::build`] walks the descriptor set and looks each method
//! up in the other two. Neither lookup can miss:
//! `parity::tests::every_rpc_in_the_descriptor_set_has_a_command` and
//! `auth::methods::tests::every_rpc_in_the_descriptor_set_has_a_scope_row`
//! fail the suite by name first, which is why this module has no
//! "unknown RPC" policy to invent — it treats a miss as the build error it is
//! and refuses to serve a partial surface.
//!
//! # Safe vs mutating, and why `Effect` is load-bearing here
//!
//! MCP is where a wrong [`Effect`] stops being a documentation bug: a
//! mutating RPC advertised with `readOnlyHint: true` is an operation an agent
//! will call freely, without asking, because the tool list told it that was
//! safe. [`Effect`]'s own doc comment draws the line at *authority* rather
//! than at persistence, which is why `ComposeService/RenderDraft` is
//! `Mutate` (it emits transmissible octets) and why `RuleService/BacktestRule`
//! is (it spends at the provider).
//!
//! This module deliberately reports **both** halves of the annotation —
//! `readOnlyHint` from `Effect`, and the required scope from the auth table —
//! rather than deriving one from the other. They answer different questions
//! ("does calling it change anything" vs "what must the caller hold"), and
//! `auth::methods::tests::effect_and_scope_agree_about_what_each_capability_does`
//! is what keeps them from disagreeing.
//!
//! # Scope gating is advisory here and enforced there
//!
//! [`ToolSurface::authorize`] and [`super::tools::Visibility`] filter the
//! surface a caller sees and refuse a call the caller's scopes do not cover.
//! Both are a *client-side* courtesy: the request still travels through
//! [`crate::AuthLayer`], which fails closed against the same table. If the
//! two ever disagreed, the daemon wins — which is the correct direction, and
//! the reason this module never becomes the only thing standing between an
//! agent and a mutation.
//!
//! # The seam task 54 was built on
//!
//! Task 54 ("MCP tool surface & scope-filtered listing") needed to filter the
//! projected surface by a caller's granted scopes without re-deriving
//! anything. [`Tool::granted_by`] is the per-tool predicate that does it,
//! borrowing out of a surface built once; [`Tool::requirement`] exposes the
//! underlying [`Requirement`] for a caller that needs to explain *why* a tool
//! is absent, and [`Tool::command`] hands back the parity row so the PRD's
//! named tool set can be checked against the registry rather than against a
//! second list.
//!
//! Scope turned out not to be the only question — see
//! [`super::tools::Visibility`], which pairs it with whether a given
//! connection offers state-changing tools at all. **That type owns both the
//! listing and the gate**, and this module deliberately no longer offers a
//! scope-only listing for anything to build one out of by accident: a listing
//! and a gate computed from different predicates is exactly how a surface
//! comes to advertise what it will refuse.

use rmail_core::auth::Scope;
use rmail_core::parity::{Command, Effect};
use serde_json::{json, Value};

use super::descriptor::{catalog, Method};
use super::{schema, McpError};
use crate::auth::{methods, Requirement};

/// One MCP tool: an RPC, the parity row that names it, and the argument
/// schema derived from its request message.
#[derive(Debug, Clone)]
pub struct Tool {
    command: Command,
    server_streaming: bool,
    input_type: String,
    output_type: String,
    input_schema: Value,
    requirement: &'static Requirement,
}

impl Tool {
    /// The MCP tool name — [`Command::tool`], so the registry is the single
    /// place a tool is named.
    #[must_use]
    pub fn name(&self) -> &'static str {
        self.command.tool()
    }

    /// The capability row this tool projects.
    #[must_use]
    pub const fn command(&self) -> Command {
        self.command
    }

    /// The gRPC method this tool calls.
    #[must_use]
    pub fn rpc(&self) -> &'static str {
        self.command.rpc()
    }

    /// Whether calling it changes anything — [`Command::effect`].
    #[must_use]
    pub fn effect(&self) -> Effect {
        self.command.effect()
    }

    /// Whether the RPC streams its response.
    #[must_use]
    pub const fn is_streaming(&self) -> bool {
        self.server_streaming
    }

    /// The fully-qualified request message name.
    #[must_use]
    pub fn input_type(&self) -> &str {
        &self.input_type
    }

    /// The fully-qualified response message name.
    #[must_use]
    pub fn output_type(&self) -> &str {
        &self.output_type
    }

    /// The JSON Schema an MCP client sees as `inputSchema`.
    #[must_use]
    pub const fn input_schema(&self) -> &Value {
        &self.input_schema
    }

    /// What the caller must hold for the daemon to run it.
    #[must_use]
    pub const fn requirement(&self) -> &'static Requirement {
        self.requirement
    }

    /// Whether `granted` satisfies this tool's requirement.
    ///
    /// Delegates to [`Requirement::satisfied_by`] for every requirement
    /// *except* [`Requirement::SelfAuthenticated`], which this always refuses
    /// regardless of `granted` — MCP's one deliberate divergence from "one
    /// definition of satisfied".
    ///
    /// `SelfAuthenticated` (`ClientAuthService/LoginPassword`, today) is
    /// reachable with *no* granted scope at all by design — that is the
    /// entire point of a login endpoint, and `authorize()` in `crate::auth`
    /// correctly short-circuits it exactly like [`Requirement::Public`]. But
    /// unlike `Public`, calling it *does* something: on success it mints a
    /// fresh [`Scope::Admin`] token, unconditionally, regardless of what
    /// scope the calling connection already held. Reusing `satisfied_by`
    /// here would mean an MCP connection scoped to `mail.read` alone — the
    /// exact caller a narrow token exists to restrict — sees and can invoke
    /// a tool that mints itself full admin. That is not a mutation whose
    /// authority the caller already had (the bar
    /// `MUTATIONS_A_READ_TOKEN_REACHES` polices for every other exception);
    /// it is privilege escalation, so it does not belong on that list either
    /// — it is refused here, unconditionally, before any scope comparison.
    ///
    /// The daemon's own gRPC layer is untouched by this: `crate::auth::authorize`
    /// calls [`Requirement::satisfied_by`] directly, never through this
    /// method, so `mail auth login`/`LoginPassword` over plain gRPC is
    /// unaffected — only MCP listing and dispatch refuse it.
    #[must_use]
    pub fn granted_by(&self, granted: &[Scope]) -> bool {
        if matches!(self.requirement, Requirement::SelfAuthenticated) {
            return false;
        }
        self.requirement.satisfied_by(granted)
    }

    /// The description handed to the model.
    ///
    /// [`Command::summary`] plus the two facts an agent cannot infer from it:
    /// that a streaming RPC returns a bounded prefix rather than everything,
    /// and which scope the call needs (so a `PERMISSION_DENIED` is
    /// actionable rather than mysterious).
    #[must_use]
    pub fn description(&self) -> String {
        let mut text = self.command.summary().to_owned();
        if self.server_streaming {
            text.push_str(
                " Streams: this tool returns a bounded prefix of the stream, and says so when \
                 there was more.",
            );
        }
        text.push_str(&format!(" Requires {}.", self.requirement.describe()));
        text
    }

    /// This tool as the object MCP's `tools/list` returns.
    ///
    /// `annotations.readOnlyHint` is [`Effect::Read`] and nothing else — the
    /// one hint here that carries information the registry actually has.
    ///
    /// `destructiveHint` is stated rather than omitted only so a client that
    /// reads it does not have to infer the default, and it is deliberately
    /// *not* a second axis: [`Effect`] draws one line, so "adds a tag" and
    /// "expunges a message" get the same value. Distinguishing them honestly
    /// would need a third annotation in `rmail_core::parity`, and inventing
    /// the distinction here — where the only input is `Effect` — would be
    /// making it up. `openWorldHint` is true because most of these tools reach
    /// an IMAP server or a model provider.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let read_only = self.effect() == Effect::Read;
        json!({
            "name": self.name(),
            "description": self.description(),
            "inputSchema": self.input_schema,
            "annotations": {
                "title": self.name(),
                "readOnlyHint": read_only,
                "destructiveHint": !read_only,
                "openWorldHint": true,
            },
        })
    }
}

/// Every RPC this daemon serves, projected as an MCP tool.
#[derive(Debug, Clone)]
pub struct ToolSurface {
    tools: Vec<Tool>,
}

impl ToolSurface {
    /// Derive the whole surface from the compiled descriptor set.
    ///
    /// # Errors
    ///
    /// [`McpError::Descriptor`] if the descriptor set does not decode, if a
    /// method has no parity row or no scope row, or if a request message
    /// references a type the set does not contain. Each of those is a build
    /// error that the workspace's own drift tests fail on first; refusing to
    /// build the surface is what keeps a half-projected tool list from
    /// reaching an agent, where a *missing* tool reads as "rmail cannot do
    /// this".
    pub fn build() -> Result<Self, McpError> {
        let catalog = catalog()?;
        let mut tools = Vec::with_capacity(catalog.methods().len());
        for method in catalog.methods() {
            tools.push(Self::project(method)?);
        }
        tools.sort_by_key(Tool::name);
        // Two RPCs projecting to one tool name would make `for_tool` — and
        // therefore dispatch — ambiguous. `parity::tests` pins tool-name
        // uniqueness in the registry; this is the same property checked
        // where it would actually bite.
        if let Some(pair) = tools.windows(2).find(|w| w[0].name() == w[1].name()) {
            return Err(McpError::Descriptor(format!(
                "{} and {} both project to the MCP tool {:?}",
                pair[0].rpc(),
                pair[1].rpc(),
                pair[0].name()
            )));
        }
        Ok(Self { tools })
    }

    /// Project one method, or say precisely which table is missing a row.
    fn project(method: &Method) -> Result<Tool, McpError> {
        let command = Command::for_rpc(&method.path).ok_or_else(|| {
            McpError::Descriptor(format!(
                "{} is served but has no row in rmail_core::parity, so it cannot be projected \
                 (every_rpc_in_the_descriptor_set_has_a_command names the row to add)",
                method.path
            ))
        })?;
        let requirement = methods::lookup(&method.path).ok_or_else(|| {
            McpError::Descriptor(format!(
                "{} is served but has no row in the capability-scope table, so every call to it \
                 is denied by the fail-closed default",
                method.path
            ))
        })?;
        if method.client_streaming {
            return Err(McpError::Descriptor(format!(
                "{} is client-streaming; one tools/call carries one argument object, so there is \
                 nothing honest to send as its second request message",
                method.path
            )));
        }
        let input_schema = schema::input_schema(catalog()?, &method.input_type)?;
        Ok(Tool {
            command,
            server_streaming: method.server_streaming,
            input_type: method.input_type.clone(),
            output_type: method.output_type.clone(),
            input_schema,
            requirement,
        })
    }

    /// Every projected tool, ordered by name.
    #[must_use]
    pub fn tools(&self) -> &[Tool] {
        &self.tools
    }

    /// The tool an MCP `tools/call` names, whatever the caller's scopes.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&Tool> {
        self.tools.iter().find(|tool| tool.name() == name)
    }

    /// Resolve a tool by name and check `granted` against it.
    ///
    /// # Errors
    ///
    /// [`McpError::UnknownTool`] if no RPC projects to that name;
    /// [`McpError::Denied`] if one does but the caller's scopes do not cover
    /// it. The two are deliberately different: a client that mistyped a name
    /// and a client that is under-scoped need different fixes, and collapsing
    /// them into "unknown tool" would hide a mutation the caller was refused.
    ///
    /// `pub(crate)` rather than `pub`: this answers the **scope** question
    /// only, and a connection's gate must also apply
    /// [`super::tools::Visibility`]'s effect policy. Leaving it reachable
    /// outside the crate — [`ToolSurface`] is handed out by
    /// `McpServer::surface` — would make "go through `Visibility`" advice
    /// rather than a property.
    pub(crate) fn authorize(&self, name: &str, granted: &[Scope]) -> Result<&Tool, McpError> {
        let tool = self
            .get(name)
            .ok_or_else(|| McpError::UnknownTool(name.to_owned()))?;
        if tool.granted_by(granted) {
            Ok(tool)
        } else {
            Err(McpError::Denied {
                tool: name.to_owned(),
                requires: tool.requirement().describe(),
            })
        }
    }
}

/// `pub(crate)` only so `super::tools`' tests can assert against this module's
/// `MUTATIONS_A_READ_TOKEN_REACHES` rather than restating it. Test-only either
/// way — `#[cfg(test)]` keeps it out of every real build.
#[cfg(test)]
pub(crate) mod tests;
