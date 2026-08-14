//! The CLI half of the feature-parity drift check (task 41).
//!
//! `rmail_core::parity` reconciles its capability registry against the
//! compiled descriptor set and the compiled action registry. It cannot do the
//! same for the CLI: the command tree is built by `clap` from the [`Cli`] type
//! in *this* binary, which no library crate can see. So this half lives here,
//! and it is the half that enforces the direction prd.md states first — "if
//! the CLI can do it, gRPC can do it".
//!
//! # Against `clap`, not against a list
//!
//! [`invocable_paths`] walks `Cli::command()` — the same tree `mail --help`
//! prints and the same one that parses an argv — so a subcommand added
//! anywhere in this crate appears here the moment it compiles, whether or not
//! anyone remembered this file. A check that compared the registry to a
//! hand-written list of verbs would prove only that somebody edited both.
//!
//! # What counts as a command
//!
//! A node with a *required* subcommand is a namespace, not a verb: `mail ai`
//! alone does nothing, it only groups `mail ai status` and friends. `clap`
//! already knows this, because `#[command(subcommand)] action: AiAction`
//! (rather than `Option<AiAction>`) is what sets `subcommand_required`. So the
//! distinction is read off the tree rather than kept in a second list of
//! "groups" here — which matters, because three nodes in this CLI are *both*:
//! `mail search <query>` and `mail search eval`, `mail tags` and
//! `mail tags create`, `mail outbox` and `mail outbox show` are each a verb
//! with children, and any hand-maintained group list would have had to get
//! that right by memory.
//!
//! The whole module is test-only — it asserts about the binary rather than
//! contributing to it — which is why the tests sit here directly instead of
//! in a sibling `tests.rs`.

use clap::CommandFactory as _;
use rmail_core::parity::{Command, LOCAL_CLI};

use crate::Cli;

/// Every subcommand path a user can actually invoke, space separated and
/// without the leading `mail` (`"ai budget set"`).
///
/// Excludes namespaces (see the module docs) and `clap`'s own generated
/// `help` subcommand, which is not a capability of anything.
fn invocable_paths() -> Vec<String> {
    fn walk(command: &clap::Command, prefix: &str, out: &mut Vec<String>) {
        for sub in command.get_subcommands() {
            let name = sub.get_name();
            // Only `help`, and only because it is `clap`'s own and describes
            // the binary rather than doing anything. A `hide = true` verb is
            // still typable and is still a capability the CLI has, so it is
            // deliberately *not* skipped — and skipping a hidden node would
            // silently drop its whole subtree from the check as well.
            if name == "help" {
                continue;
            }
            let path = if prefix.is_empty() {
                name.to_owned()
            } else {
                format!("{prefix} {name}")
            };
            // A node that *requires* a subcommand cannot be invoked on its
            // own, so it is a grouping and not a verb. One that merely allows
            // one (`Option<Action>` in the derive) is both, and is listed.
            if !sub.is_subcommand_required_set() {
                out.push(path.clone());
            }
            walk(sub, &path, out);
        }
    }

    let mut out = Vec::new();
    walk(&Cli::command(), "", &mut out);
    out
}

/// Every `mail` verb is backed by a capability, or declared client-side.
///
/// prd.md's "If the CLI can do it, gRPC can do it. If gRPC can't do it, it
/// isn't a feature," as a test that fails by name. Adding a subcommand
/// without a row is the drift this exists to catch: the verb ships, agents
/// and gRPC clients cannot do the same thing, and nothing anywhere says so.
#[test]
fn every_cli_command_is_backed_by_a_capability() {
    let local: Vec<&str> = LOCAL_CLI.iter().map(|(path, _)| *path).collect();
    for path in invocable_paths() {
        let backed = Command::for_cli(&path).next().is_some();
        assert!(
            backed || local.contains(&path.as_str()),
            "`mail {path}` has no row in rmail_core::parity — add a Command variant whose \
             `cli:` names it, or, if it deliberately talks to no daemon, an entry in \
             LOCAL_CLI saying why"
        );
    }
}

/// Every CLI path a row names actually exists.
///
/// The mirror direction, and the one that catches a renamed or deleted verb:
/// the row would keep claiming a subcommand nobody can type, `for_cli` would
/// answer `None` for the real one, and the check above would go on passing
/// because it only ever looks at paths `clap` reports.
#[test]
fn every_claimed_cli_path_exists() {
    let invocable = invocable_paths();
    for command in Command::ALL {
        for path in command.cli() {
            assert!(
                invocable.contains(&(*path).to_owned()),
                "{} claims `mail {path}`, which is not an invocable subcommand of the `mail` \
                 binary",
                command.name()
            );
        }
    }
    for (path, _) in LOCAL_CLI {
        assert!(
            invocable.contains(&(*path).to_owned()),
            "LOCAL_CLI declares `mail {path}` client-side, but no such subcommand exists"
        );
    }
}

/// The namespaces really are namespaces.
///
/// [`invocable_paths`] leans entirely on `clap`'s `subcommand_required` flag
/// to tell a verb from a grouping; if that ever stopped being true — a derive
/// change, or an `Option<…>` added to a group's field — every verb under the
/// group would silently drop out of the check above and the drift net would
/// have a hole in exactly the shape of a whole subcommand family. Pinning two
/// known groups and two known verbs-with-children keeps that visible.
#[test]
fn groupings_are_excluded_and_verbs_with_children_are_not() {
    let paths = invocable_paths();
    for grouping in ["ai", "ai budget", "index", "note", "hook", "token", "keys"] {
        assert!(
            !paths.contains(&grouping.to_owned()),
            "`mail {grouping}` is a grouping and should not be listed as an invocable command"
        );
    }
    for verb in ["search", "tags", "outbox", "ai status", "keys set"] {
        assert!(
            paths.contains(&verb.to_owned()),
            "`mail {verb}` is invocable and must be checked for a backing capability"
        );
    }
}

/// The tree is walked to its leaves, not just its top level.
///
/// `mail ai budget set` is three levels down; a walk that stopped at depth one
/// would report a clean parity check having examined almost nothing.
#[test]
fn the_walk_reaches_the_deepest_command() {
    let paths = invocable_paths();
    assert!(paths.contains(&"ai budget set".to_owned()));
    assert!(paths.contains(&"search eval".to_owned()));
    assert!(
        paths.len() > 40,
        "only {} invocable commands found — the walk is not reaching the tree",
        paths.len()
    );
}
