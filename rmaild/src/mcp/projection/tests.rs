//! The acceptance of task 53, as tests: annotation -> tool generation, scope
//! gating, and mutating-tool denial under a read-only token.
//!
//! Each of these asserts against something *generated* — the compiled
//! descriptor set, the parity registry, the scope table — rather than against
//! a list written here. A test that compared the projection to a hand-written
//! roster of tool names would prove only that somebody edited both, which is
//! the failure `rmail_core::parity` exists to prevent, moved into its own test
//! file.

use super::*;
use crate::mcp::tools::Visibility;
use rmail_core::parity::Command;

/// The scope set `prd.md` describes handing an agent: "read and summarize
/// freely but cannot send".
fn read_only() -> Vec<Scope> {
    vec![Scope::MailRead]
}

/// The listing a connection holding `granted` gets, with no effect policy on
/// top — which is the scope-only filter these tests are about.
///
/// Task 54 folded the scope-only listing into [`Visibility`] so that a listing
/// and a gate cannot be computed from different predicates, which is why this
/// goes through [`Visibility::scoped`] rather than a method on
/// [`ToolSurface`]. [`Mutations::AsScoped`] is the default, so this is the
/// same set as before, obtained through the path production actually uses.
fn scoped(granted: Vec<Scope>) -> Visibility {
    Visibility::scoped(granted)
}

#[test]
fn every_served_rpc_projects_to_exactly_one_tool() {
    let surface = ToolSurface::build().expect("the surface must build");
    let catalog = catalog().expect("catalog");

    assert_eq!(
        surface.tools().len(),
        catalog.methods().len(),
        "the projection must cover every RPC the descriptor set declares"
    );
    for method in catalog.methods() {
        let Some(projected) = surface
            .tools()
            .iter()
            .find(|tool| tool.rpc() == method.path)
        else {
            unreachable!("{} projects to no tool", method.path)
        };
        let Some(row) = Command::for_rpc(&method.path) else {
            unreachable!("{} has no parity row", method.path)
        };
        assert_eq!(
            projected.name(),
            row.tool(),
            "the tool name must come from the parity registry, not from this module"
        );
    }
}

/// A new RPC yields a new tool with zero extra code.
///
/// Stated as a property rather than by adding a fake method: the projection
/// is a total function of `catalog().methods()`, so "every method has a tool"
/// (above) plus "no tool exists without a method" (here) is exactly the claim
/// — an RPC that landed tomorrow would be in `methods()` the moment it
/// compiled, and therefore in the surface.
#[test]
fn no_tool_exists_that_no_rpc_backs() {
    let surface = ToolSurface::build().expect("surface");
    let catalog = catalog().expect("catalog");
    for tool in surface.tools() {
        assert!(
            catalog.methods().iter().any(|m| m.path == tool.rpc()),
            "{} projects from {}, which the descriptor set does not declare",
            tool.name(),
            tool.rpc()
        );
    }
}

#[test]
fn the_annotation_comes_from_the_parity_registry() {
    let surface = ToolSurface::build().expect("surface");
    for tool in surface.tools() {
        let row = Command::for_rpc(tool.rpc()).expect("a parity row");
        assert_eq!(tool.effect(), row.effect());
        assert!(
            tool.description().starts_with(row.summary()),
            "{}'s description must open with the registry's own summary",
            tool.name()
        );
        // `Command::for_tool` is the reverse direction task 41 left for
        // dispatch; if it disagreed with `tool()` a call would land on a
        // different RPC than the one advertised.
        assert_eq!(Command::for_tool(tool.name()), Some(row));
    }
}

#[test]
fn read_only_hint_is_exactly_effect_read() {
    let surface = ToolSurface::build().expect("surface");
    for tool in surface.tools() {
        let json = tool.to_json();
        assert_eq!(
            json["annotations"]["readOnlyHint"],
            serde_json::Value::Bool(tool.effect() == Effect::Read),
            "{} advertises a readOnlyHint that disagrees with its Effect",
            tool.name()
        );
        assert_eq!(
            json["annotations"]["destructiveHint"],
            serde_json::Value::Bool(tool.effect() == Effect::Mutate),
        );
    }
}

/// The mutating tools a `mail.read` token can nonetheless reach, named.
///
/// The projection filters by **scope**, never by [`Effect`], and the two do
/// not perfectly coincide. Filtering by `Effect` instead would be the wrong
/// fix: it would hide a tool the daemon would in fact run, which is a
/// capability silently lost rather than a hazard avoided (see
/// `Requirement::satisfied_by`'s own docs).
///
/// One row makes them differ today. `SearchService/LogFeedback` is
/// [`Effect::Mutate`] — it writes rows a later search's ranking reads, which
/// is an effect a later call observes — while `auth::methods` deliberately
/// puts it at `mail.read`, arguing at length that a read-only token is
/// exactly the one that should be able to contribute the click data that
/// makes its own future searches better. Both judgments are defensible and
/// both are documented where they are made; what must not happen is a *third*
/// one appearing here unnoticed.
///
/// So the set is pinned by name. A new RPC that lands in it fails this test
/// by name, and a human then decides whether the scope row is too weak or the
/// effect too strong — instead of an agent quietly gaining a mutation that a
/// read-only token was supposed not to have.
/// `pub(crate)` so `mcp::tools`' own tests can assert against the *same* set
/// rather than restating it: task 54's default listing must contain exactly
/// these, and its `--read-only` listing exactly none of them. A second copy of
/// this list is how the two would drift.
pub(crate) const MUTATIONS_A_READ_TOKEN_REACHES: &[&str] = &["log_search_feedback"];

/// The acceptance's "mutating tools gated by capability-token scope".
///
/// Not a spot check on a couple of names: every mutating tool must be
/// unreachable with `mail.read` alone, save the declared exceptions above. A
/// single mutation that slipped through is one an agent would call freely.
#[test]
fn no_undeclared_mutating_tool_is_reachable_with_a_read_only_token() {
    let surface = ToolSurface::build().expect("surface");
    let granted = read_only();
    let mut checked = 0;
    for tool in surface.tools() {
        if tool.effect() != Effect::Mutate || MUTATIONS_A_READ_TOKEN_REACHES.contains(&tool.name())
        {
            continue;
        }
        checked += 1;
        assert!(
            !tool.granted_by(&granted),
            "{} ({}) mutates and is reachable with mail.read alone — it requires {}. Either its \
             scope row is too weak or it belongs in MUTATIONS_A_READ_TOKEN_REACHES with an \
             argument for why.",
            tool.name(),
            tool.rpc(),
            tool.requirement().describe()
        );
    }
    assert!(
        checked > 40,
        "only {checked} mutating tools examined; the surface looks wrong"
    );
}

/// The exception list is exhaustive, and it is not empty by accident.
#[test]
fn the_mutations_a_read_token_reaches_are_exactly_the_declared_ones() {
    let surface = ToolSurface::build().expect("surface");
    let visibility = scoped(read_only());
    let mut reachable: Vec<&str> = visibility
        .list(&surface)
        .filter(|tool| tool.effect() == Effect::Mutate)
        .map(Tool::name)
        .collect();
    reachable.sort_unstable();
    assert_eq!(
        reachable, MUTATIONS_A_READ_TOKEN_REACHES,
        "the set of mutating tools a mail.read token reaches has changed; see \
         MUTATIONS_A_READ_TOKEN_REACHES"
    );
}

#[test]
fn a_read_only_token_sees_only_tools_it_can_call() {
    let surface = ToolSurface::build().expect("surface");
    let granted = read_only();
    let visibility = scoped(granted.clone());
    let visible: Vec<&Tool> = visibility.list(&surface).collect();

    assert!(!visible.is_empty(), "a read token must see something");
    for tool in &visible {
        assert!(
            tool.granted_by(&granted),
            "{} is listed to a caller whose scopes do not reach it",
            tool.name()
        );
    }
    // ...and the ones it cannot call really are absent, rather than the
    // filter having quietly matched everything.
    assert!(
        visible.len() < surface.tools().len(),
        "the filter returned the whole surface"
    );
    assert!(
        !visible.iter().any(|tool| tool.name() == "delete_message"),
        "delete_message must not be listed to a read-only token"
    );
}

/// Whatever a read token is shown, it is told the truth about what each tool
/// does. This is the property that actually protects an agent: a tool it may
/// call is one thing, a tool it may call *freely* is another.
#[test]
fn a_mutating_tool_is_never_advertised_as_read_only() {
    let surface = ToolSurface::build().expect("surface");
    let visibility = scoped(read_only());
    for tool in visibility.list(&surface) {
        if tool.effect() == Effect::Mutate {
            assert_eq!(
                tool.to_json()["annotations"]["readOnlyHint"],
                serde_json::Value::Bool(false),
                "{} mutates and must not be advertised as safe",
                tool.name()
            );
        }
    }
}

#[test]
fn an_admin_token_sees_every_tool() {
    let surface = ToolSurface::build().expect("surface");
    let visibility = scoped(vec![Scope::Admin]);
    let visible = visibility.list(&surface).count();
    assert_eq!(
        visible,
        surface.tools().len(),
        "admin satisfies every scope, so it must see the whole surface"
    );
}

#[test]
fn authorize_refuses_a_mutating_tool_under_a_read_token() {
    let surface = ToolSurface::build().expect("surface");
    let error = surface
        .authorize("delete_message", &read_only())
        .expect_err("a read-only token must be refused a delete");
    let McpError::Denied { tool, requires } = &error else {
        unreachable!("expected a denial, got {error:?}")
    };
    assert_eq!(tool, "delete_message");
    assert!(
        requires.contains("mail.write"),
        "the refusal must name the scope to mint, got {requires:?}"
    );
    // The same call with the scope the refusal named must succeed, or the
    // message is pointing at the wrong fix.
    assert!(surface
        .authorize("delete_message", &[Scope::MailWrite])
        .is_ok());
}

#[test]
fn authorize_distinguishes_an_unknown_tool_from_a_denied_one() {
    let surface = ToolSurface::build().expect("surface");
    let error = surface
        .authorize("no_such_tool", &[Scope::Admin])
        .expect_err("an unknown tool must not resolve");
    assert!(matches!(error, McpError::UnknownTool(_)), "{error:?}");
    assert_eq!(error.code(), -32601, "unknown tool is JSON-RPC -32601");

    let denied = surface
        .authorize("delete_message", &read_only())
        .expect_err("denied");
    assert_ne!(
        denied.code(),
        -32601,
        "a denial must not look like a missing tool"
    );
}

/// A token that can ask the mailbox a question needs both halves of an
/// `AllOf` row, and the projection must apply the conjunction rather than
/// treating it as a disjunction.
#[test]
fn an_all_of_row_needs_every_scope_in_it() {
    let surface = ToolSurface::build().expect("surface");
    let ask = surface
        .get("ask_mailbox")
        .expect("ask_mailbox is projected");
    assert!(!ask.granted_by(&[Scope::MailRead]));
    assert!(!ask.granted_by(&[Scope::AiInvoke]));
    assert!(ask.granted_by(&[Scope::MailRead, Scope::AiInvoke]));
    assert!(ask.granted_by(&[Scope::Admin]));
}

/// `SendSchedulerService/CancelScheduled` is the one `AnyOf` row, and either
/// scope alone must be enough — the undo window exists so a human can
/// intercept an AI-originated send, and the token holding it is the one that
/// scheduled it.
#[test]
fn an_any_of_row_needs_only_one_scope_in_it() {
    let surface = ToolSurface::build().expect("surface");
    let cancel = surface
        .get("cancel_scheduled_send")
        .expect("cancel_scheduled_send is projected");
    assert!(cancel.granted_by(&[Scope::MailSend]));
    assert!(cancel.granted_by(&[Scope::MailWrite]));
    assert!(!cancel.granted_by(&[Scope::MailRead]));
}

/// Streaming is read off the descriptor set, not written down here.
#[test]
fn streaming_is_taken_from_the_descriptor_set() {
    let surface = ToolSurface::build().expect("surface");
    let catalog = catalog().expect("catalog");
    for tool in surface.tools() {
        let method = catalog
            .methods()
            .iter()
            .find(|m| m.path == tool.rpc())
            .expect("a method");
        assert_eq!(tool.is_streaming(), method.server_streaming);
    }
    assert!(surface.get("list_messages").expect("tool").is_streaming());
    assert!(!surface.get("get_message").expect("tool").is_streaming());
}

/// A streaming tool has to say it returns a prefix. An agent that believed
/// `watch_mail_events` returned the whole history would draw conclusions from
/// a truncated answer.
#[test]
fn a_streaming_tool_says_its_answer_is_bounded() {
    let surface = ToolSurface::build().expect("surface");
    for tool in surface.tools() {
        assert_eq!(
            tool.description().contains("bounded prefix"),
            tool.is_streaming(),
            "{}'s description does not match whether it streams",
            tool.name()
        );
    }
}

/// The generated `inputSchema` is the RPC's own request message.
#[test]
fn the_input_schema_is_the_request_messages_fields() {
    let surface = ToolSurface::build().expect("surface");
    let get = surface.get("get_message").expect("get_message");
    assert_eq!(get.input_type(), "rmail.v1.GetMessageRequest");

    let schema = get.input_schema();
    assert_eq!(schema["type"], "object");
    assert_eq!(
        schema["additionalProperties"],
        serde_json::Value::Bool(false)
    );
    assert!(
        schema["properties"].get("id").is_some(),
        "GetMessageRequest.id must appear as a property: {schema}"
    );

    // Every projected tool advertises a usable object schema, or an agent
    // cannot call it at all.
    for tool in surface.tools() {
        assert_eq!(
            tool.input_schema()["type"],
            "object",
            "{} has no object inputSchema",
            tool.name()
        );
        assert!(
            tool.input_schema().get("properties").is_some(),
            "{} has no properties map",
            tool.name()
        );
    }
}

/// The description tells the agent what scope a call needs, using the same
/// words `mail token create --scope` takes.
#[test]
fn the_description_names_the_scope_to_mint() {
    let surface = ToolSurface::build().expect("surface");
    let delete = surface.get("delete_message").expect("delete_message");
    assert!(
        delete.description().contains("mail.write"),
        "{}",
        delete.description()
    );
    let ask = surface.get("ask_mailbox").expect("ask_mailbox");
    let description = ask.description();
    assert!(description.contains("mail.read"), "{description}");
    assert!(description.contains("ai.invoke"), "{description}");
}

/// The tool JSON is what an MCP client actually parses.
#[test]
fn a_tool_serializes_to_the_mcp_shape() {
    let surface = ToolSurface::build().expect("surface");
    let json = surface.get("search_mail").expect("search_mail").to_json();
    assert_eq!(json["name"], "search_mail");
    assert!(json["description"].is_string());
    assert_eq!(json["inputSchema"]["type"], "object");
    assert_eq!(json["annotations"]["readOnlyHint"], true);
}

/// prd.md names these tools directly; they must be present under exactly
/// those names.
///
/// Not a hand-maintained roster of the whole surface — that is what the
/// generation tests above cover. This pins the handful of names prd.md's own
/// "MCP Tools (search)" section promises, so renaming a `tool:` in the parity
/// registry breaks a documented contract loudly.
#[test]
fn the_tool_names_prd_promises_are_present() {
    let surface = ToolSurface::build().expect("surface");
    for name in [
        "search_mail",
        "semantic_search",
        "explain_ranking",
        "ask_mailbox",
        "get_message",
        "get_thread",
    ] {
        assert!(
            surface.get(name).is_some(),
            "prd.md documents an MCP tool named {name}, which nothing projects to"
        );
    }
}
