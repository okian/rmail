//! `mail agent run` and `mail agent log` — task 69's human surface
//! (prd.md feature 47).
//!
//! # `--dry-run` is the default, and `--mutate` is the flag you have to type
//!
//! prd.md spells the verb `mail agent run [--dry-run]`, and both flags exist:
//! `--dry-run` says out loud what happens anyway, `--mutate` is the only way
//! to make the agent touch anything, and the two conflict so a command line
//! carrying both is a mistake rather than a coin toss. The wire field is
//! `mutate` for the same reason — proto3's zero value is then the safe one, so
//! a client that forgets the field gets a dry run.
//!
//! Nothing on this command line can widen what the agent may do. The archive
//! destination, the label list, the snooze ceiling and the three bounds are
//! all `[agent]` configuration; the flags here choose an account, a mailbox, a
//! policy, and whether to act at all.
//!
//! # Everything printed is sanitized, including the reasons
//!
//! Two of the columns are hostile by construction: the subject and sender come
//! from a stranger, and the `reason` is *model*-authored text written while
//! reading that stranger's message. The daemon has already put the reason
//! through `injection::sanitize_model_text` (bidi overrides, invisibles), and
//! this file additionally strips control characters before anything reaches a
//! terminal — the same rule `find_cli::sanitize` and `search_cli` apply, for
//! the same reason: an `ESC` byte in a subject line is interpreted, not shown.
//!
//! `--json` needs no such pass and does not get one: `serde_json` escapes it,
//! and pre-sanitizing would silently corrupt a caller's data.

use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};
use rmail_proto::v1::agent_service_client::AgentServiceClient;
use rmail_proto::v1::{
    AgentAction, AgentActionEntry, AgentActionOutcome, AgentStopReason, GetAgentRunLogRequest,
    RunInboxAgentRequest,
};

/// `mail agent <action>`.
#[derive(Debug, Subcommand)]
pub enum AgentCommand {
    /// Walk a mailbox once, deciding one action per message
    /// (`AgentService.RunInboxAgent`).
    ///
    /// A dry run unless `--mutate` is given: it decides, prints what it would
    /// do, and changes nothing. Even with `--mutate` the daemon refuses unless
    /// `agent.allow_mutations = true`.
    Run(RunArgs),
    /// What the agent has already done, run by run, with its reasons
    /// (`AgentService.GetAgentRunLog`).
    ///
    /// Only mutating runs appear: a dry run writes nothing, by design.
    Log(LogArgs),
}

/// `mail agent run --account <id> [--policy ...] [--dry-run|--mutate]`.
#[derive(Debug, Args)]
pub struct RunArgs {
    /// The account to walk.
    #[arg(long)]
    account: i64,
    /// The mailbox to walk. Defaults to `agent.mailbox` (INBOX).
    #[arg(long)]
    mailbox: Option<String>,
    /// Your standing policy, in your own words: "archive newsletters, escalate
    /// anything from my team, draft a reply to scheduling requests".
    ///
    /// This is instruction text and reaches the model outside the
    /// untrusted-content fence — unlike the mail itself, which is always
    /// fenced. Left off, the agent strongly prefers doing nothing.
    #[arg(long, default_value = "")]
    policy: String,
    /// Decide and print, change nothing. This is the default; the flag is
    /// accepted so a command can say so.
    #[arg(long)]
    dry_run: bool,
    /// Actually perform the decided actions. Requires
    /// `agent.allow_mutations = true` on the daemon and a token holding
    /// mail.read + mail.write + ai.invoke + automation.
    #[arg(long, conflicts_with = "dry_run")]
    mutate: bool,
    /// One JSON document instead of the rendered table.
    #[arg(long)]
    json: bool,
}

/// `mail agent log --account <id> [--limit N]`.
#[derive(Debug, Args)]
pub struct LogArgs {
    /// The account whose runs to read.
    #[arg(long)]
    account: i64,
    /// How many runs, newest first. Defaults to `agent.log_limit`.
    #[arg(long)]
    limit: Option<u32>,
    /// One JSON document instead of the rendered log.
    #[arg(long)]
    json: bool,
}

/// Dispatch `mail agent <action>`.
///
/// # Errors
/// A transport or RPC failure, or a write to stdout failing.
pub async fn run(socket: &Path, action: AgentCommand) -> Result<()> {
    match action {
        AgentCommand::Run(args) => run_agent(socket, args).await,
        AgentCommand::Log(args) => log(socket, args).await,
    }
}

async fn run_agent(socket: &Path, args: RunArgs) -> Result<()> {
    let channel = connect(socket).await?;
    let response = AgentServiceClient::new(channel)
        .run_inbox_agent(RunInboxAgentRequest {
            account_id: args.account,
            mailbox: args.mailbox.unwrap_or_default(),
            policy: args.policy,
            mutate: args.mutate,
        })
        .await
        .context("RunInboxAgent RPC failed")?
        .into_inner();

    let mut out = std::io::stdout().lock();
    if args.json {
        writeln!(
            out,
            "{}",
            serde_json::to_string(&serde_json::json!({
                "run_id": response.run_id,
                "mutated": response.mutated,
                "mailbox": response.mailbox,
                "stop_reason": stop_reason_name(response.stop_reason),
                "iterations": response.iterations,
                "model_calls": response.model_calls,
                "actions_applied": response.actions_applied,
                "actions": response.actions.iter().map(entry_json).collect::<Vec<_>>(),
            }))?
        )?;
        return Ok(());
    }

    // The banner leads with the thing a reader most needs to know, because
    // "this changed your mail" and "this changed nothing" look identical
    // otherwise and the table below is the same either way.
    if response.mutated {
        writeln!(
            out,
            "mutating run {} over {} — {} action(s) applied",
            response.run_id, response.mailbox, response.actions_applied
        )?;
    } else {
        writeln!(
            out,
            "dry run over {} — nothing was changed",
            response.mailbox
        )?;
    }
    writeln!(
        out,
        "{} message(s) considered, {} model call(s), stopped: {}",
        response.iterations,
        response.model_calls,
        stop_reason_name(response.stop_reason)
    )?;
    if response.actions.is_empty() {
        writeln!(out, "(no messages to consider)")?;
        return Ok(());
    }
    writeln!(out)?;
    for entry in &response.actions {
        write_entry(&mut out, entry)?;
    }
    Ok(())
}

async fn log(socket: &Path, args: LogArgs) -> Result<()> {
    let channel = connect(socket).await?;
    let response = AgentServiceClient::new(channel)
        .get_agent_run_log(GetAgentRunLogRequest {
            account_id: args.account,
            limit: args.limit.unwrap_or_default(),
        })
        .await
        .context("GetAgentRunLog RPC failed")?
        .into_inner();

    let mut out = std::io::stdout().lock();
    if args.json {
        writeln!(
            out,
            "{}",
            serde_json::to_string(&serde_json::json!({
                "runs": response.runs.iter().map(|run| serde_json::json!({
                    "id": run.id,
                    "account_id": run.account_id,
                    "mailbox": run.mailbox,
                    "policy": run.policy,
                    "started_at": run.started_at,
                    "finished_at": run.finished_at,
                    "stop_reason": stop_reason_name(run.stop_reason),
                    "iterations": run.iterations,
                    "model_calls": run.model_calls,
                    "actions_applied": run.actions_applied,
                    "actions": run.actions.iter().map(entry_json).collect::<Vec<_>>(),
                })).collect::<Vec<_>>(),
            }))?
        )?;
        return Ok(());
    }

    if response.runs.is_empty() {
        writeln!(out, "(no agent runs recorded)")?;
        return Ok(());
    }
    for run in &response.runs {
        writeln!(
            out,
            "run {} over {}  {} action(s) applied of {} considered  [{}]",
            run.id,
            sanitize(&run.mailbox),
            run.actions_applied,
            run.iterations,
            stop_reason_name(run.stop_reason)
        )?;
        // The policy is echoed because the log is unreadable without it: the
        // same archive is correct under one policy and wrong under another,
        // and a reader auditing a run needs both halves in front of them.
        if !run.policy.is_empty() {
            writeln!(out, "  policy: {}", sanitize(&run.policy))?;
        }
        for entry in &run.actions {
            write_entry(&mut out, entry)?;
        }
        writeln!(out)?;
    }
    Ok(())
}

/// One action line, then its reason and any detail, indented under it.
fn write_entry(out: &mut impl Write, entry: &AgentActionEntry) -> Result<()> {
    let argument = sanitize(&entry.argument);
    writeln!(
        out,
        "  [{}] {}{}  {}  <{}>",
        outcome_name(entry.outcome),
        action_name(entry.action),
        if argument.is_empty() {
            String::new()
        } else {
            format!(" {argument}")
        },
        sanitize(&entry.subject),
        sanitize(&entry.sender),
    )?;
    if !entry.reason.is_empty() {
        writeln!(out, "      reason: {}", sanitize(&entry.reason))?;
    }
    if !entry.detail.is_empty() {
        writeln!(out, "      {}", sanitize(&entry.detail))?;
    }
    Ok(())
}

fn entry_json(entry: &AgentActionEntry) -> serde_json::Value {
    serde_json::json!({
        "id": entry.id,
        "message_id": entry.message_id,
        "rfc_message_id": entry.rfc_message_id,
        "subject": entry.subject,
        "sender": entry.sender,
        "action": action_name(entry.action),
        "argument": entry.argument,
        "reason": entry.reason,
        "outcome": outcome_name(entry.outcome),
        "detail": entry.detail,
        "decided_at": entry.decided_at,
    })
}

/// The stable `--json` spelling of an action. Written out rather than derived
/// from the generated enum's `as_str_name`, which would print
/// `AGENT_ACTION_ARCHIVE` and would change shape if the enum were renamed —
/// the reasoning `find_cli::kind_name` gives.
fn action_name(action: i32) -> &'static str {
    match AgentAction::try_from(action).unwrap_or(AgentAction::Unspecified) {
        AgentAction::Archive => "archive",
        AgentAction::Label => "label",
        AgentAction::Snooze => "snooze",
        AgentAction::DraftReply => "draft_reply",
        AgentAction::Escalate => "escalate",
        AgentAction::None => "none",
        AgentAction::Unspecified => "unknown",
    }
}

fn outcome_name(outcome: i32) -> &'static str {
    match AgentActionOutcome::try_from(outcome).unwrap_or(AgentActionOutcome::Unspecified) {
        AgentActionOutcome::Attempted => "attempted",
        AgentActionOutcome::Applied => "applied",
        AgentActionOutcome::Failed => "failed",
        AgentActionOutcome::Withheld => "withheld",
        AgentActionOutcome::Refused => "refused",
        AgentActionOutcome::Planned => "would",
        AgentActionOutcome::Unspecified => "unknown",
    }
}

fn stop_reason_name(reason: i32) -> &'static str {
    match AgentStopReason::try_from(reason).unwrap_or(AgentStopReason::Unspecified) {
        AgentStopReason::Running => "running",
        AgentStopReason::Completed => "completed",
        AgentStopReason::IterationCap => "iteration cap",
        AgentStopReason::ActionCap => "action cap",
        AgentStopReason::Deadline => "deadline",
        AgentStopReason::Cancelled => "cancelled",
        AgentStopReason::Error => "error",
        AgentStopReason::Unspecified => "unknown",
    }
}

async fn connect(socket: &Path) -> Result<tonic::transport::Channel> {
    rmail_core::connect_uds(socket)
        .await
        .with_context(|| format!("connecting to rmaild at {}", socket.display()))
}

/// Strip control characters from text that came out of a message or a model.
///
/// See the module docs. Identical in shape to `find_cli::sanitize`, and
/// deliberately not shared with it: that one belongs to the finder's own
/// rendering and a shared helper would invite one call site's needs to loosen
/// the other's.
fn sanitize(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c == '\t' || c == '\n' || c == '\r' {
                ' '
            } else {
                c
            }
        })
        .filter(|c| !c.is_control())
        .collect()
}

#[cfg(test)]
mod tests;
