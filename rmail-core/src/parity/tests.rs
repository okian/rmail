//! The drift checks.
//!
//! Every test that matters here reconciles [`Command`] against something
//! *generated* from a surface — the compiled descriptor set, the compiled
//! action registry — rather than against a second list a human wrote. Two
//! hand-written lists agreeing proves only that somebody edited both; that is
//! the failure mode this whole module exists to rule out, so it must not be
//! the shape of its own tests.
//!
//! The CLI half of the reconciliation lives in `rmail-cli` (`src/parity.rs`),
//! because `clap`'s command tree is built from the `Cli` type in that crate's
//! binary and is not reachable from here.

use std::collections::{BTreeMap, BTreeSet};

use super::{Command, Effect, LOCAL_ACTIONS, LOCAL_CLI};
use crate::keymap::Action;

/// Every `(fully.qualified.Service, Method)` pair in the compiled protos.
///
/// Decoded from the descriptor set `build.rs` emits, so it is whatever
/// `proto/rmail/v1/*.proto` currently declares — never a list maintained
/// alongside it. `rmaild::auth::methods`' own scope-row check reads the same
/// bytes for the same reason.
fn descriptor_rpcs() -> BTreeSet<String> {
    use prost::Message as _;

    let set = prost_types::FileDescriptorSet::decode(rmail_proto::FILE_DESCRIPTOR_SET)
        .expect("the compiled descriptor set must decode");

    let mut out = BTreeSet::new();
    for file in &set.file {
        let package = file.package();
        for service in &file.service {
            let fq = if package.is_empty() {
                service.name().to_owned()
            } else {
                format!("{package}.{}", service.name())
            };
            for method in &service.method {
                out.insert(format!("/{fq}/{}", method.name()));
            }
        }
    }
    assert!(!out.is_empty(), "descriptor set contained no services");
    out
}

/// Every RPC the daemon serves is a capability.
///
/// This is prd.md's "If gRPC can do it, Claude can do it (via MCP
/// auto-projection)" as a test: task 53 projects one MCP tool per row here, so
/// an RPC with no row is an RPC no agent can ever call, silently. Reconciled
/// against the descriptor set, so a service added tomorrow fails this by name
/// whether or not anyone remembered this file.
#[test]
fn every_rpc_in_the_descriptor_set_has_a_command() {
    for rpc in descriptor_rpcs() {
        assert!(
            Command::for_rpc(&rpc).is_some(),
            "{rpc} is served but has no row in the parity registry, so no MCP tool projects \
             from it and no surface can claim to mirror it. Add a `Command` variant."
        );
    }
}

/// No row names an RPC that does not exist.
///
/// The mirror of the check above, and the one that catches a renamed or
/// deleted method: the row would simply stop matching anything, `for_rpc`
/// would return `None` for the real path, and every claim this table makes
/// about that capability would be about nothing at all.
#[test]
fn every_command_names_an_rpc_that_exists() {
    let served = descriptor_rpcs();
    for command in Command::ALL {
        assert!(
            served.contains(command.rpc()),
            "{} names {}, which no compiled proto declares — the row describes a capability \
             that does not exist",
            command.name(),
            command.rpc()
        );
    }
}

/// One row per RPC, and no RPC claimed twice.
///
/// [`Command::for_rpc`] returns the *first* match, so a duplicated path would
/// leave a second row unreachable — including its MCP tool name, which would
/// then never be projected while looking, in the source, as though it were.
#[test]
fn no_rpc_is_claimed_by_two_commands() {
    let mut seen: BTreeMap<&str, &str> = BTreeMap::new();
    for command in Command::ALL {
        let first = seen.insert(command.rpc(), command.name());
        assert!(
            first.is_none(),
            "{} and {} both claim {}",
            first.unwrap_or_default(),
            command.name(),
            command.rpc()
        );
    }
}

/// Every path is `/rmail.v1.<Service>/<Method>`.
///
/// [`Command::service`]/[`Command::method`] are infallible and return `""` for
/// a malformed path rather than panicking; this is what makes that safe. The
/// `rmail.v1` prefix is checked too: a row under some other package would be
/// a capability this workspace does not serve, and the descriptor check above
/// would report it only as "does not exist", which is a less useful thing to
/// read at 2am.
#[test]
fn every_rpc_path_is_well_formed() {
    for command in Command::ALL {
        let rpc = command.rpc();
        assert!(
            rpc.starts_with("/rmail.v1."),
            "{}'s path {rpc} is not in the rmail.v1 package",
            command.name()
        );
        assert!(
            !command.service().is_empty() && !command.method().is_empty(),
            "{}'s path {rpc} does not split into a service and a method",
            command.name()
        );
        assert!(
            command.service().ends_with("Service"),
            "{}'s service {} does not follow the <Name>Service convention",
            command.name(),
            command.service()
        );
    }
}

/// A variant is named after the RPC it governs.
///
/// Mechanical on purpose: `MailService/Get` is `MailGet`, and nothing else.
/// A row whose name and path disagree is a row someone will eventually read,
/// believe, and act on — the auth table's own history has three separate
/// instances of a provisional row being trusted because its *name* looked
/// right.
#[test]
fn every_variant_is_named_after_its_rpc() {
    for command in Command::ALL {
        let service = command
            .service()
            .trim_start_matches("rmail.v1.")
            .trim_end_matches("Service");
        let expected = format!("{service}{}", command.method());
        assert_eq!(
            command.name(),
            expected,
            "{} governs {} and should therefore be named {expected}",
            command.name(),
            command.rpc()
        );
    }
}

/// Tool names are unique, and shaped the way MCP tool names are.
///
/// Uniqueness first: [`Command::for_tool`] is how task 53 dispatches a tool
/// call back onto an RPC, and two rows sharing a name would route half of
/// them to the wrong capability. The shape check is not cosmetic either — a
/// tool name is part of the contract an agent is prompted with, and one
/// containing a `/` or a capital would be a name some MCP client rejects.
#[test]
fn every_tool_name_is_unique_and_snake_case() {
    let mut seen: BTreeMap<&str, &str> = BTreeMap::new();
    for command in Command::ALL {
        let tool = command.tool();
        assert!(!tool.is_empty(), "{} has no tool name", command.name());
        assert!(
            tool.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
            "{}'s tool name {tool:?} is not lower_snake_case",
            command.name()
        );
        assert!(
            !tool.starts_with('_') && !tool.ends_with('_'),
            "{}'s tool name {tool:?} starts or ends with an underscore",
            command.name()
        );
        let first = seen.insert(tool, command.name());
        assert!(
            first.is_none(),
            "{} and {} both project the tool {tool:?}",
            first.unwrap_or_default(),
            command.name()
        );
        assert_eq!(
            Command::for_tool(tool),
            Some(*command),
            "{tool:?} must resolve back to {}",
            command.name()
        );
    }
}

/// Every capability describes itself.
///
/// The summary is what task 53 hands an agent as the generated tool's
/// description — an empty or duplicated one is a tool an agent cannot choose
/// between and a human cannot audit.
#[test]
fn every_command_has_a_distinct_summary() {
    let mut seen: BTreeMap<&str, &str> = BTreeMap::new();
    for command in Command::ALL {
        let summary = command.summary();
        assert!(
            !summary.trim().is_empty(),
            "{} has no summary",
            command.name()
        );
        let first = seen.insert(summary, command.name());
        assert!(
            first.is_none(),
            "{} and {} share the summary {summary:?}; an agent picking between the two tools \
             has nothing to go on",
            first.unwrap_or_default(),
            command.name()
        );
    }
}

/// Every TUI action is either a capability or declared UI-local.
///
/// The second vocabulary this registry has to stay level with: task 84's
/// `actions!` ids are what `keys.toml` binds, what `?` prints, and what the
/// command palette resolves to. Reconciled against [`Action::ALL`], which the
/// macro generates from the same list that defines the enum — so a new action
/// fails here by name, and the author has to say whether it is a capability
/// (a row's `actions:`) or a movement (`LOCAL_ACTIONS`).
#[test]
fn every_tui_action_is_a_capability_or_declared_local() {
    for action in Action::ALL {
        let is_local = LOCAL_ACTIONS.contains(action);
        let backing: Vec<&str> = Command::for_action(*action).map(Command::name).collect();
        assert!(
            is_local || !backing.is_empty(),
            "the TUI action {:?} ({}) is neither backed by a capability nor listed in \
             LOCAL_ACTIONS — add it to a Command's `actions:` if it does something to mail, \
             or to LOCAL_ACTIONS if it only moves the cursor",
            action.id(),
            action.describe()
        );
        assert!(
            !(is_local && !backing.is_empty()),
            "the TUI action {:?} is listed in LOCAL_ACTIONS *and* claimed by {backing:?}",
            action.id()
        );
    }
}

/// No row names a TUI action twice, and `for_action` finds every claim.
#[test]
fn action_claims_resolve_back_to_their_commands() {
    for command in Command::ALL {
        let mut seen = BTreeSet::new();
        for action in command.actions() {
            assert!(
                seen.insert(*action),
                "{} lists the action {:?} twice",
                command.name(),
                action.id()
            );
            assert!(
                Command::for_action(*action).any(|c| c == *command),
                "{} claims {:?} but for_action does not find it",
                command.name(),
                action.id()
            );
        }
    }
}

/// A `mail` verb is claimed by a capability or declared client-side, never
/// both, and never twice within one row.
///
/// The *other* half of the CLI reconciliation — that no `mail` verb exists
/// which nothing here claims — needs `clap`'s tree and therefore lives in
/// `rmail-cli`. This half is what stops the two escape hatches overlapping:
/// a path in both places would let a verb look backed by an RPC in one file
/// and deliberately local in the other.
#[test]
fn no_cli_path_is_both_backed_and_declared_local() {
    let local: BTreeSet<&str> = LOCAL_CLI.iter().map(|(path, _)| *path).collect();
    assert_eq!(
        local.len(),
        LOCAL_CLI.len(),
        "LOCAL_CLI lists a path more than once"
    );
    for (path, reason) in LOCAL_CLI {
        assert!(
            !reason.trim().is_empty(),
            "LOCAL_CLI's {path:?} has no reason; an escape hatch nobody justified is a gap"
        );
        let backing: Vec<&str> = Command::for_cli(path).map(Command::name).collect();
        assert!(
            backing.is_empty(),
            "`mail {path}` is declared client-side but is also claimed by {backing:?}"
        );
    }
    for command in Command::ALL {
        let mut seen = BTreeSet::new();
        for path in command.cli() {
            assert!(
                seen.insert(*path),
                "{} lists the CLI path {path:?} twice",
                command.name()
            );
            assert!(
                !path.is_empty() && !path.contains("  ") && path.trim() == *path,
                "{}'s CLI path {path:?} is not a plain space-separated subcommand path",
                command.name()
            );
        }
    }
}

/// The lookups agree with the table.
#[test]
fn lookups_find_every_row() {
    for command in Command::ALL {
        assert_eq!(Command::for_rpc(command.rpc()), Some(*command));
        assert_eq!(Command::for_tool(command.tool()), Some(*command));
    }
    assert_eq!(Command::for_rpc("/rmail.v1.DoesNotExist/Method"), None);
    assert_eq!(Command::for_tool("no_such_tool"), None);
}

/// Reading is not writing.
///
/// A sanity check on the one field task 53 gates tools with: at least one row
/// of each effect, so a future edit that collapsed `Effect` into a constant
/// (or defaulted every row to `Read`) fails here rather than silently
/// exposing every mutating RPC as a safe tool.
#[test]
fn both_effects_are_used() {
    let mutating = Command::ALL
        .iter()
        .filter(|c| c.effect().is_mutating())
        .count();
    assert!(mutating > 0, "no capability mutates anything");
    assert!(
        mutating < Command::ALL.len(),
        "every capability mutates something"
    );
    assert!(Effect::Mutate.is_mutating());
    assert!(!Effect::Read.is_mutating());
}

/// The capabilities the CLI reaches are the ones the CLI's own tests name.
///
/// A cheap consistency check on the `cli:` lists themselves: a path is only
/// ever declared on rows whose service could plausibly serve it. Written as a
/// spot check on the verbs that reach two RPCs each, because those are the
/// rows where a later edit is most likely to drop one half — `mail sync` (a
/// pass, then the event stream), `mail ai scan-injection` (a scan, then an
/// optional confirmation), and `mail list` (one mailbox, or every account's
/// inbox under `--all`).
#[test]
fn the_verbs_that_reach_two_rpcs_still_do() {
    let sync: BTreeSet<&str> = Command::for_cli("sync").map(Command::rpc).collect();
    assert!(sync.contains("/rmail.v1.SyncService/SyncFolder"));
    assert!(sync.contains("/rmail.v1.SyncService/WatchEvents"));

    let scan: BTreeSet<&str> = Command::for_cli("ai scan-injection")
        .map(Command::rpc)
        .collect();
    assert!(scan.contains("/rmail.v1.AiSafetyService/ScanInjection"));
    assert!(scan.contains("/rmail.v1.AiSafetyService/ConfirmInjection"));

    let list: BTreeSet<&str> = Command::for_cli("list").map(Command::rpc).collect();
    assert!(list.contains("/rmail.v1.MailService/List"));
    assert!(list.contains("/rmail.v1.MailService/ListUnified"));
}
