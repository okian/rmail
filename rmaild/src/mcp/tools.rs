//! The tool surface an agent is actually offered (task 54).
//!
//! [`super::projection`] derives *every* served RPC as a tool. This module is
//! the layer directly above it: given who is connecting, which of those tools
//! appear in `tools/list`, and which `tools/call` will dispatch. It derives
//! nothing of its own — [`ToolSurface`] is built once per process and
//! [`Visibility`] borrows out of it — so the rule task 53 established holds
//! here too: there is no list of tools in this file.
//!
//! # "A read-only token's tool list contains only read tools"
//!
//! That sentence is task 54's acceptance, and satisfying it naively would be a
//! bug. The reason is that two different tables answer two different questions
//! about the same RPC, and they do not perfectly coincide:
//!
//! | table | question | answer for `SearchService/LogFeedback` |
//! |---|---|---|
//! | [`rmail_core::parity::Effect`] | does calling it change anything? | `Mutate` — it writes rows a later search's ranking reads |
//! | [`crate::auth::methods`] | what must the caller hold? | `mail.read` — the authority granted is "make my own future searches better" |
//!
//! Both judgments are argued at length where they are made, and both are
//! right. What follows from them is that a `mail.read` connection can reach
//! exactly one mutating tool, `log_search_feedback`, and
//! `super::projection::tests::MUTATIONS_A_READ_TOKEN_REACHES` pins that set by
//! name so a second row cannot join it quietly.
//!
//! ## Why the default filter is scope and only scope
//!
//! Filtering the listing by [`Effect`] would hide `log_search_feedback` from a
//! `mail.read` connection **while the daemon would still run it if asked**.
//! That is the worse of the two failures. A listing that admits a mutation is
//! at least true, and `readOnlyHint` already tells the agent which kind of
//! tool it is looking at; a listing that under-reports is a lie in the
//! direction that costs a capability — the agent concludes it cannot act, and
//! the daemon disagrees. So [`Mutations::AsScoped`], the default, filters by
//! [`crate::auth::Requirement::satisfied_by`] and nothing else, and the
//! listing is exactly the set of calls this connection will send and the
//! daemon will accept.
//!
//! ## How the acceptance's posture is offered honestly
//!
//! The sentence still names something real and worth having: "read and
//! summarize freely but cannot send" as a property of the surface rather than
//! of the model's restraint. [`Mutations::Withheld`] provides it, and the
//! thing that makes it honest rather than a shorter lie is that it does not
//! filter only the listing: a withheld tool is also refused by
//! [`Visibility::authorize`], with [`McpError::Withheld`] saying so. The
//! listing therefore still describes this connection exactly — the withheld
//! calls are ones this process genuinely will not send.
//! `tests::nothing_withheld_from_the_listing_is_still_callable` is that
//! property, checked over the whole surface rather than on a sample.
//!
//! ## What it does **not** promise
//!
//! Not "this agent writes no byte anywhere". The line it draws is
//! [`Effect`]'s, which is *authority* — changing mail, spending at a provider,
//! or emitting something that carries the power to. `SearchService/Search` is
//! [`Effect::Read`] by that measure and survives, and every page it serves
//! still appends a row to the local learning log (`search.learning`, on by
//! default; see `rmail_core::feedback`). An operator who needs literally no
//! writes turns that off too. Stating this precisely matters because the same
//! sentence is handed to the model in `initialize`'s `instructions`, and an
//! agent that repeats an overclaim to its human is worse than one that says
//! nothing.
//!
//! What withholding does cost, concretely: `log_search_feedback` goes with the
//! rest, so the agent's *explicit* signals — this hit was the one I wanted —
//! never reach the ranker, even though its impressions still do. That is a
//! trade to make deliberately, which is why it is a flag and not the default.
//!
//! ## The option not taken
//!
//! The third way to make the acceptance's sentence true is to move
//! `LogFeedback` to `mail.write` so the two tables agree. It is rejected here
//! rather than merely unimplemented: [`crate::auth::methods`]' own row argues
//! that a read-only token is precisely the one that should be able to improve
//! its own search, and bounds what the call can do (a `query_id` this daemon
//! minted, checked inside the write's transaction against the messages that
//! query actually showed). Re-deciding that from this module would delete a
//! capability prd.md deliberately wants an agent to have, and would do it by
//! moving a scope rather than by changing what the tool does.

use rmail_core::auth::Scope;
use rmail_core::parity::Effect;

use super::projection::{Tool, ToolSurface};
use super::McpError;

/// Whether a connection offers the tools that change state.
///
/// Orthogonal to scope on purpose — see this module's docs. Scope is what the
/// daemon will accept; this is what this process is willing to offer and send.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mutations {
    /// Offer every tool the caller's scopes reach, mutating or not.
    ///
    /// The default, and the only setting under which the listing and the
    /// daemon's own answer to "may this caller run this" are the same set.
    #[default]
    AsScoped,
    /// Offer only [`Effect::Read`] tools, whatever the scopes reach — and
    /// refuse the others rather than merely hiding them.
    Withheld,
}

impl Mutations {
    /// A short label for a log line or a `--list` header.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Mutations::AsScoped => "as scoped",
            Mutations::Withheld => "read-only (mutating tools withheld)",
        }
    }
}

/// What one connection may list and call.
///
/// The pair of a caller's granted scopes and this process's own policy about
/// mutating tools. Both halves are applied by [`Visibility::permits`], and
/// both the listing ([`Visibility::list`]) and the gate
/// ([`Visibility::authorize`]) are defined in terms of it — which is what
/// keeps them from describing different surfaces.
#[derive(Debug, Clone, Default)]
pub struct Visibility {
    granted: Vec<Scope>,
    mutations: Mutations,
}

impl Visibility {
    /// A connection holding `granted`, under `mutations`.
    #[must_use]
    pub fn new(granted: Vec<Scope>, mutations: Mutations) -> Self {
        Self { granted, mutations }
    }

    /// A connection holding `granted`, offering everything those scopes reach.
    #[must_use]
    pub fn scoped(granted: Vec<Scope>) -> Self {
        Self::new(granted, Mutations::AsScoped)
    }

    /// The scopes this connection claims.
    #[must_use]
    pub fn granted(&self) -> &[Scope] {
        &self.granted
    }

    /// This connection's policy on mutating tools.
    #[must_use]
    pub const fn mutations(&self) -> Mutations {
        self.mutations
    }

    /// Whether this connection both may and will call `tool`.
    ///
    /// The single predicate behind the listing and the gate. Anything that
    /// consults one of those without going through this would be a second
    /// answer to the same question, which is the drift the whole projection is
    /// built to avoid.
    #[must_use]
    pub fn permits(&self, tool: &Tool) -> bool {
        tool.granted_by(&self.granted) && !self.withholds(tool)
    }

    /// Whether this connection's own policy — not the caller's scopes —
    /// refuses `tool`.
    ///
    /// Phrased as "not [`Effect::Read`]" rather than "is [`Effect::Mutate`]"
    /// so a third [`Effect`] variant would land on the refusing side by
    /// default. A new kind of effect nobody has classified yet is not
    /// something to offer a read-only agent on the strength of an `==`.
    #[must_use]
    pub fn withholds(&self, tool: &Tool) -> bool {
        self.mutations == Mutations::Withheld && tool.effect() != Effect::Read
    }

    /// The tools this connection lists.
    ///
    /// Written as a filter over [`Visibility::permits`] rather than over
    /// [`ToolSurface::visible_to`] plus a second condition, so that the
    /// listing is *literally* the predicate [`Visibility::authorize`] applies
    /// and not a re-derivation of it. A second spelling of the same rule is
    /// how a listing and a gate come to disagree.
    pub fn list<'a>(&'a self, surface: &'a ToolSurface) -> impl Iterator<Item = &'a Tool> + 'a {
        surface.tools().iter().filter(|tool| self.permits(tool))
    }

    /// Resolve a tool by name and apply the policy, saying which half refused.
    ///
    /// # Errors
    ///
    /// [`McpError::UnknownTool`] if no RPC projects to that name;
    /// [`McpError::Withheld`] if one does and this connection withholds it;
    /// [`McpError::Denied`] if the caller's scopes do not cover it.
    ///
    /// The three are deliberately distinct. A withheld tool must never be
    /// reported as an unknown one: an agent told "no such tool" re-reads the
    /// list, finds it still absent, and has nowhere to go, whereas one told
    /// the surface is read-only can say so to the human who started it.
    ///
    /// Where *both* halves refuse, the message names both. Reporting only the
    /// withholding would send an operator to restart without `--read-only`
    /// and straight into a scope denial on the next call; reporting only the
    /// scope would send them to mint a token this server would refuse anyway.
    /// Neither constraint binds alone, so neither is the answer alone.
    pub fn authorize<'a>(
        &self,
        surface: &'a ToolSurface,
        name: &str,
    ) -> Result<&'a Tool, McpError> {
        let tool = surface
            .get(name)
            .ok_or_else(|| McpError::UnknownTool(name.to_owned()))?;
        if self.permits(tool) {
            return Ok(tool);
        }
        if self.withholds(tool) {
            return Err(McpError::Withheld {
                tool: name.to_owned(),
                // Empty in the common case, so the message stays one sentence
                // when there is one thing to fix.
                scope_shortfall: if tool.granted_by(&self.granted) {
                    String::new()
                } else {
                    format!(
                        " It also requires {}, which this caller does not hold, so lifting the \
                         read-only surface alone would not be enough.",
                        tool.requirement().describe()
                    )
                },
            });
        }
        // Delegated rather than re-implemented: the scope denial, and the
        // words it names the missing scope with, have one definition. Only
        // reached once the policy has already refused, so the second lookup
        // is on the cold path.
        surface.authorize(name, &self.granted)
    }
}

#[cfg(test)]
mod tests;
