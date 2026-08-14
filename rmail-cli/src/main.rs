//! The `mail` CLI — a thin gRPC client for the rmail daemon.

mod hook_cli;
mod index_cli;
mod keymap;
mod keys_cli;
mod note_cli;
mod outbox_cli;
mod search_cli;
mod tag_cli;
mod tui;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use note_cli::{NoteAction, NotesArgs};
use outbox_cli::{FollowupAction, OutboxArgs, SendArgs, UndoArgs};
use rmail_core::socket_path_from_env;
use rmail_proto::v1::admin_service_client::AdminServiceClient;
use rmail_proto::v1::ai_policy_service_client::AiPolicyServiceClient;
use rmail_proto::v1::ai_safety_service_client::AiSafetyServiceClient;
use rmail_proto::v1::ai_service_client::AiServiceClient;
use rmail_proto::v1::sync_service_client::SyncServiceClient;
use rmail_proto::v1::{
    analyze_event, AnalyzeMessageRequest, BudgetCaps, BudgetClass, BudgetWindowCaps, ClassSpend,
    ConfirmInjectionRequest, EventKind, GetSpendRequest, GetSummaryRequest, GetUsageRequest,
    InjectionSeverity, ListTokensRequest, MintTokenRequest, RetryFailedRequest, RevokeTokenRequest,
    ScanInjectionRequest, ScanInjectionResponse, SetBudgetRequest, SetPausedRequest,
    SuggestReplyRequest, Summary, SyncFolderRequest, SyncMode, WatchEventsRequest,
};
use search_cli::{SearchArgs, SimilarArgs};
use tag_cli::{TagArgs, TagsArgs, UntagArgs};
use tokio_stream::StreamExt;
use tonic_health::pb::health_check_response::ServingStatus;
use tonic_health::pb::health_client::HealthClient;
use tonic_health::pb::HealthCheckRequest;

/// rmail command-line client.
#[derive(Debug, Parser)]
#[command(name = "mail", version, about = "rmail command-line client")]
struct Cli {
    /// Path to the rmaild gRPC Unix domain socket (defaults to $RMAIL_SOCKET).
    #[arg(long, global = true, env = rmail_core::SOCKET_ENV)]
    socket: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Round-trip a gRPC health check against rmaild.
    Ping,
    /// Synchronize an account's mail.
    Sync {
        /// Account to sync.
        #[arg(long)]
        account: i64,
        /// Sync only this mailbox (default: every folder, priority order).
        #[arg(long)]
        mailbox: Option<i64>,
        /// Force the full UID-window walk instead of a delta pass.
        #[arg(long)]
        full: bool,
        /// After syncing, follow the event stream until interrupted.
        #[arg(long)]
        watch: bool,
    },
    /// Manage capability tokens (`AdminService.MintToken/RevokeToken/ListTokens`).
    Token {
        #[command(subcommand)]
        action: TokenAction,
    },
    /// The terminal UI: folders, message list, preview (`tui`).
    Tui(tui::TuiArgs),
    /// Inspect and rebind the TUI's keys (`keys.toml`; see `keys_cli`'s own
    /// module docs on why this edits a file rather than calling an RPC).
    Keys {
        #[command(subcommand)]
        action: keys_cli::KeysAction,
    },
    /// Ranked search over the local index (`SearchService.Search`).
    Search(SearchArgs),
    /// Embedding-kNN neighbors of a message (`SearchService.Semantic`).
    Similar(SimilarArgs),
    /// AI pipeline verbs (`AiService`).
    Ai {
        #[command(subcommand)]
        action: AiAction,
    },
    /// Add/edit/delete a note on a message or thread (`NoteService`).
    Note {
        #[command(subcommand)]
        action: NoteAction,
    },
    /// List notes on a message or thread (`NoteService.ListNotes`).
    Notes(NotesArgs),
    /// Event hooks: config-driven shell commands on mail events
    /// (`HookService`; `add` edits the local config file directly — see
    /// `hook_cli`'s own module docs).
    Hook {
        #[command(subcommand)]
        action: hook_cli::HookAction,
    },
    /// Index maintenance: coverage, drain, verify, gc, rebuild
    /// (`IndexService`).
    Index {
        #[command(subcommand)]
        action: index_cli::IndexAction,
    },
    /// Entities extracted from mail, by kind (`IndexService.ListEntities`).
    Entities(index_cli::EntitiesArgs),
    /// Apply one or more tags to a message, thread, or bulk selection
    /// (`TagService.AddTag`/`BulkTag`).
    Tag(TagArgs),
    /// Bulk-apply tags to every message a filter-only query selects
    /// (`TagService.BulkTag`).
    #[command(name = "tag-bulk")]
    TagBulk {
        /// Filter-only query (`from:`/`to:`/`subject:`/`is:`/`in:`/
        /// `has:attachment`/`tag:`).
        #[arg(long)]
        query: String,
        #[arg(long)]
        account: i64,
        /// Tag name(s) to apply.
        #[arg(required = true)]
        tags: Vec<String>,
    },
    /// Remove one or more tags from a message or thread
    /// (`TagService.RemoveTag`).
    Untag(UntagArgs),
    /// List tags, or create one (`TagService.ListTags`/`CreateTag`).
    Tags(TagsArgs),
    /// Print a message's pending AI tag suggestions
    /// (`TagService.SuggestTags`). Never triggers a model call — task 57
    /// owns generating suggestions; this only displays what is pending.
    #[command(name = "suggest-tags")]
    SuggestTags {
        /// Message id.
        message_id: i64,
    },
    /// Accept pending suggestions by id, as printed by `suggest-tags`
    /// (`TagService.ResolveSuggestion`).
    #[command(name = "accept-tags")]
    AcceptTags {
        #[arg(required = true)]
        message_tag_ids: Vec<i64>,
    },
    /// Reject pending suggestions by id, as printed by `suggest-tags`
    /// (`TagService.ResolveSuggestion`).
    #[command(name = "reject-tags")]
    RejectTags {
        #[arg(required = true)]
        message_tag_ids: Vec<i64>,
    },
    /// Send a message now (undoable) or schedule it for later
    /// (`SendSchedulerService.ScheduleSend`).
    Send(SendArgs),
    /// Cancel a send inside its undo window, or any scheduled message
    /// (`SendSchedulerService.CancelScheduled`).
    Undo(UndoArgs),
    /// Inspect and manage the outbox (`SendSchedulerService`).
    Outbox(OutboxArgs),
    /// Follow-up reminders on sent mail (`SendSchedulerService`).
    Followup {
        #[command(subcommand)]
        action: FollowupAction,
    },
}

#[derive(Debug, Subcommand)]
enum AiAction {
    /// Queue depth, today's tokens/cost, headroom, and pause state
    /// (`AiService.GetUsage`).
    Status,
    /// Force a fresh deep-pass (re)analysis of one message, streaming
    /// progress as it arrives (`AiService.AnalyzeMessage`).
    Process {
        /// Message id to analyze.
        message_id: i64,
    },
    /// Print a message's cached AI summary (`AiService.GetSummary`) —
    /// never triggers a model call.
    Summary {
        /// Message id.
        message_id: i64,
        /// Print the raw structured result instead of a formatted view.
        #[arg(long)]
        json: bool,
    },
    /// Print a message's suggested reply, generating one now if none is
    /// cached yet (`AiService.SuggestReply`).
    Reply {
        /// Message id.
        message_id: i64,
    },
    /// Requeue quarantined AI jobs.
    Retry {
        /// Requeue every job that exhausted its retries
        /// (`AiService.RetryFailed`, `AiQueue::revive_all_dead`) — the only
        /// retry mode this build supports.
        #[arg(long)]
        failed: bool,
    },
    /// Pause the daemon's AI dispatch loop (`AiService.SetPaused`). Cached
    /// results stay readable; nothing new is enqueued or dispatched.
    Pause,
    /// Resume the daemon's AI dispatch loop (`AiService.SetPaused`).
    Resume,
    /// Token/cost usage against the configured caps (`AiService.GetUsage`).
    Cost {
        /// Show this calendar month's rollup instead of today's.
        #[arg(long)]
        month: bool,
    },
    /// Per-account and global spend budgets
    /// (`AiPolicyService.SetBudget`/`GetSpend`).
    Budget {
        #[command(subcommand)]
        action: BudgetAction,
    },
    /// Scan one message for prompt-injection signals, exactly as the AI
    /// pipeline sees it (`AiSafetyService.ScanInjection`). Makes no model
    /// call and costs nothing.
    ///
    /// A rule that matched on a `claude_is` verdict will not fire its
    /// actions on a message flagged at or above
    /// `ai.injection.block_actions_at` until a human confirms it — that is
    /// what `--confirm` is for. Read the excerpts before you do: confirming
    /// says "I have looked at what this message tried and I still want the
    /// rule to act on it".
    #[command(name = "scan-injection")]
    ScanInjection {
        /// Message id.
        message_id: i64,
        /// Confirm the reported findings, releasing any withheld rule
        /// actions on this message (`AiSafetyService.ConfirmInjection`).
        #[arg(long)]
        confirm: bool,
        /// Withdraw a confirmation given earlier, so the shield withholds
        /// again.
        #[arg(long, conflicts_with = "confirm")]
        revoke: bool,
    },
}

#[derive(Debug, Subcommand)]
enum BudgetAction {
    /// Store the caps for one scope (`AiPolicyService.SetBudget`).
    ///
    /// A cap left off the command line is left *uncapped*, not set to zero:
    /// the enforcer's boundary is `>=`, so `--daily-hard-usd 0` forbids all
    /// spending while omitting it forbids none. Setting a budget replaces
    /// whatever was stored for that scope, so pass every cap you want in
    /// force, not just the one you are changing.
    Set {
        /// Account id to budget, or 0 (the default) for the global budget
        /// every call counts toward.
        #[arg(long, default_value_t = 0)]
        account: i64,
        /// Budget the bulk sub-budget (backlog work) instead of the
        /// everything budget. A bulk call is checked against both.
        #[arg(long)]
        bulk: bool,
        /// Downgrade the model (opus -> sonnet -> haiku) at or above this
        /// many dollars spent today.
        #[arg(long)]
        daily_soft_usd: Option<f64>,
        /// Block dispatch at or above this many dollars spent today.
        #[arg(long)]
        daily_hard_usd: Option<f64>,
        /// Downgrade the model at or above this many tokens spent today.
        #[arg(long)]
        daily_soft_tokens: Option<i64>,
        /// Block dispatch at or above this many tokens spent today.
        #[arg(long)]
        daily_hard_tokens: Option<i64>,
        /// Downgrade the model at or above this many dollars spent this
        /// calendar month.
        #[arg(long)]
        monthly_soft_usd: Option<f64>,
        /// Block dispatch at or above this many dollars spent this calendar
        /// month.
        #[arg(long)]
        monthly_hard_usd: Option<f64>,
        /// Downgrade the model at or above this many tokens spent this
        /// calendar month.
        #[arg(long)]
        monthly_soft_tokens: Option<i64>,
        /// Block dispatch at or above this many tokens spent this calendar
        /// month.
        #[arg(long)]
        monthly_hard_tokens: Option<i64>,
    },
    /// Spend so far today and this month against the caps in force
    /// (`AiPolicyService.GetSpend`).
    Status {
        /// Account id to report, or 0 (the default) for the global budget.
        #[arg(long, default_value_t = 0)]
        account: i64,
    },
}

#[derive(Debug, Subcommand)]
enum TokenAction {
    /// Mint a new capability token. The bearer secret is printed exactly
    /// once — it cannot be recovered later, only revoked.
    Create {
        /// Human-readable label (e.g. "ci", "claude-agent").
        #[arg(long)]
        name: String,
        /// Scope(s) to grant: mail.read, mail.write, mail.send, ai.invoke,
        /// ai.spend:<usd>, mailbox:<name>, automation, admin. Repeatable
        /// and/or comma-separated, e.g. `--scope mail.read --scope
        /// ai.invoke` or `--scope mail.read,ai.invoke`. NOTE: ai.spend and
        /// mailbox are accepted and stored but not yet enforced by any RPC —
        /// a mailbox-only token grants nothing today, it does not restrict.
        #[arg(long = "scope", required = true, value_delimiter = ',')]
        scopes: Vec<String>,
        /// Time-to-live, e.g. "24h", "90d". Omit for no expiry.
        #[arg(long)]
        ttl: Option<String>,
    },
    /// List tokens (metadata only — never the secret or its hash).
    List,
    /// Revoke a token by id.
    Revoke {
        /// Token id (as printed by `mail token create`/`list`).
        id: i64,
    },
}

/// Deadline for the health-check RPC so a wedged daemon cannot hang the CLI.
const HEALTH_RPC_TIMEOUT: Duration = Duration::from_secs(10);

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let socket = cli.socket.unwrap_or_else(socket_path_from_env);

    match cli.command {
        Command::Ping => ping(&socket).await,
        Command::Sync {
            account,
            mailbox,
            full,
            watch,
        } => sync(&socket, account, mailbox, full, watch).await,
        Command::Token { action } => match action {
            TokenAction::Create { name, scopes, ttl } => {
                token_create(&socket, name, scopes, ttl).await
            }
            TokenAction::List => token_list(&socket).await,
            TokenAction::Revoke { id } => token_revoke(&socket, id).await,
        },
        Command::Tui(args) => tui::run(&socket, args).await,
        Command::Keys { action } => keys_cli::run(action),
        Command::Search(args) => search_cli::search(&socket, args).await,
        Command::Similar(args) => search_cli::similar(&socket, args).await,
        Command::Ai { action } => match action {
            AiAction::Status => ai_status(&socket).await,
            AiAction::Process { message_id } => ai_process(&socket, message_id).await,
            AiAction::Summary { message_id, json } => ai_summary(&socket, message_id, json).await,
            AiAction::Reply { message_id } => ai_reply(&socket, message_id).await,
            AiAction::Retry { failed } => ai_retry(&socket, failed).await,
            AiAction::Pause => ai_set_paused(&socket, true).await,
            AiAction::Resume => ai_set_paused(&socket, false).await,
            AiAction::Cost { month } => ai_cost(&socket, month).await,
            AiAction::Budget { action } => ai_budget(&socket, action).await,
            AiAction::ScanInjection {
                message_id,
                confirm,
                revoke,
            } => ai_scan_injection(&socket, message_id, confirm, revoke).await,
        },
        Command::Note { action } => note_cli::dispatch(&socket, action).await,
        Command::Notes(args) => note_cli::list(&socket, args).await,
        Command::Hook { action } => hook_cli::run(&socket, action).await,
        Command::Index { action } => index_cli::run(&socket, action).await,
        Command::Entities(args) => index_cli::entities(&socket, args).await,
        Command::Tag(args) => tag_cli::tag(&socket, args).await,
        Command::TagBulk {
            query,
            account,
            tags,
        } => tag_cli::bulk_tag(&socket, account, query, tags).await,
        Command::Untag(args) => tag_cli::untag(&socket, args).await,
        Command::Tags(args) => tag_cli::tags(&socket, args).await,
        Command::SuggestTags { message_id } => tag_cli::suggest_tags(&socket, message_id).await,
        Command::Send(args) => outbox_cli::send(&socket, args).await,
        Command::Undo(args) => outbox_cli::undo(&socket, args).await,
        Command::Outbox(args) => outbox_cli::outbox(&socket, args).await,
        Command::Followup { action } => outbox_cli::followup(&socket, action).await,
        Command::AcceptTags { message_tag_ids } => {
            tag_cli::resolve_suggestions(&socket, message_tag_ids, true).await
        }
        Command::RejectTags { message_tag_ids } => {
            tag_cli::resolve_suggestions(&socket, message_tag_ids, false).await
        }
    }
}

/// Trigger a sync pass and, with `--watch`, keep streaming what changes after
/// it.
async fn sync(
    socket: &Path,
    account_id: i64,
    mailbox_id: Option<i64>,
    full: bool,
    watch: bool,
) -> Result<()> {
    let channel = rmail_core::connect_uds(socket)
        .await
        .with_context(|| format!("connecting to rmaild at {}", socket.display()))?;
    let mut client = SyncServiceClient::new(channel);

    let response = client
        .sync_folder(SyncFolderRequest {
            account_id,
            mailbox_id,
            mode: if full {
                SyncMode::Full as i32
            } else {
                SyncMode::Auto as i32
            },
        })
        .await
        .context("sync RPC failed")?
        .into_inner();

    let mut failures = 0;
    for folder in &response.folders {
        match &folder.error {
            Some(error) => {
                failures += 1;
                println!("{:<24} failed: {error}", folder.mailbox_name);
            }
            None => println!(
                "{:<24} {:<10} +{} new  ~{} flags  -{} gone",
                folder.mailbox_name,
                folder.strategy,
                folder.new_messages,
                folder.flag_updates,
                folder.expunged
            ),
        }
    }
    if response.folders.is_empty() {
        println!("no folders to sync");
    }

    if watch {
        // Resume from where this pass ended, so the stream shows what happens
        // *next* rather than replaying what was just reported.
        println!("watching from seq {}…", response.latest_seq);
        watch_events(&mut client, account_id, response.latest_seq).await?;
    }

    if failures > 0 {
        bail!("{failures} folder(s) failed to sync");
    }
    Ok(())
}

/// Follow the event stream until the daemon closes it or the user interrupts.
async fn watch_events(
    client: &mut SyncServiceClient<tonic::transport::Channel>,
    account_id: i64,
    since_seq: i64,
) -> Result<()> {
    let mut stream = client
        .watch_events(WatchEventsRequest {
            account_id,
            since_seq,
            kinds: Vec::new(),
        })
        .await
        .context("watch RPC failed")?
        .into_inner();

    loop {
        // Ctrl-C ends the watch cleanly rather than killing the process
        // mid-write, so a terminal is never left with a half-printed line.
        let next = tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                println!();
                return Ok(());
            }
            next = stream.next() => next,
        };
        match next {
            Some(Ok(event)) => {
                let kind = EventKind::try_from(event.kind)
                    .map(|k| k.as_str_name().trim_start_matches("EVENT_KIND_").to_owned())
                    .unwrap_or_else(|_| format!("KIND_{}", event.kind));
                println!(
                    "seq {:<8} {:<14} {}",
                    event.seq,
                    kind,
                    event.payload.trim_matches('"')
                );
            }
            Some(Err(status)) => {
                // A retention gap is the one stream error a client is expected
                // to act on, and the daemon reports where to resume in
                // structured metadata rather than in the message text.
                bail!(
                    "event stream ended: {} ({})",
                    status.message(),
                    status.code()
                );
            }
            None => return Ok(()),
        }
    }
}

async fn ping(socket: &Path) -> Result<()> {
    let channel = rmail_core::connect_uds(socket)
        .await
        .with_context(|| format!("connecting to rmaild at {}", socket.display()))?;

    let mut client = HealthClient::new(channel);
    let response = tokio::time::timeout(
        HEALTH_RPC_TIMEOUT,
        client.check(HealthCheckRequest {
            service: String::new(),
        }),
    )
    .await
    .context("health check RPC timed out")?
    .context("health check RPC failed")?;

    let status = response.into_inner().status();
    println!("rmaild health: {status:?}");

    if status == ServingStatus::Serving {
        Ok(())
    } else {
        bail!("rmaild is not serving (status: {status:?})");
    }
}

/// Mint a capability token and print its bearer secret. This is the only
/// moment the secret is ever visible — `ListTokens` returns metadata only.
async fn token_create(
    socket: &Path,
    name: String,
    scopes: Vec<String>,
    ttl: Option<String>,
) -> Result<()> {
    let ttl_secs = ttl
        .as_deref()
        .map(|s| {
            rmail_core::config::parse_human_duration(s)
                .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        })
        .transpose()
        .map_err(|e| anyhow::anyhow!("invalid --ttl: {e}"))?;

    let channel = rmail_core::connect_uds(socket)
        .await
        .with_context(|| format!("connecting to rmaild at {}", socket.display()))?;
    let mut client = AdminServiceClient::new(channel);
    let response = client
        .mint_token(MintTokenRequest {
            name,
            scopes,
            ttl_secs,
        })
        .await
        .context("MintToken RPC failed")?
        .into_inner();

    println!("id:      {}", response.id);
    println!("name:    {}", response.name);
    println!("scopes:  {}", response.scopes.join(","));
    if let Some(expires_at) = response.expires_at {
        println!("expires: {expires_at} (unix seconds)");
    } else {
        println!("expires: never");
    }
    println!();
    println!("token:   {}", response.token);
    println!();
    println!(
        "Store this now — it cannot be shown again. Revoke with `mail token revoke {}`.",
        response.id
    );
    Ok(())
}

/// List tokens (metadata only).
async fn token_list(socket: &Path) -> Result<()> {
    let channel = rmail_core::connect_uds(socket)
        .await
        .with_context(|| format!("connecting to rmaild at {}", socket.display()))?;
    let mut client = AdminServiceClient::new(channel);
    let response = client
        .list_tokens(ListTokensRequest {})
        .await
        .context("ListTokens RPC failed")?
        .into_inner();

    if response.tokens.is_empty() {
        println!("no tokens");
        return Ok(());
    }
    for token in response.tokens {
        let status = if token.revoked { "revoked" } else { "active" };
        println!(
            "{:<6} {:<20} {:<8} {}",
            token.id,
            token.name,
            status,
            token.scopes.join(",")
        );
    }
    Ok(())
}

/// Revoke a token by id.
async fn token_revoke(socket: &Path, id: i64) -> Result<()> {
    let channel = rmail_core::connect_uds(socket)
        .await
        .with_context(|| format!("connecting to rmaild at {}", socket.display()))?;
    let mut client = AdminServiceClient::new(channel);
    client
        .revoke_token(RevokeTokenRequest { id })
        .await
        .context("RevokeToken RPC failed")?;
    println!("revoked token {id}");
    Ok(())
}

// ---------------------------------------------------------------------------
// `mail ai ...`
// ---------------------------------------------------------------------------

async fn ai_client(socket: &Path) -> Result<AiServiceClient<tonic::transport::Channel>> {
    let channel = rmail_core::connect_uds(socket)
        .await
        .with_context(|| format!("connecting to rmaild at {}", socket.display()))?;
    Ok(AiServiceClient::new(channel))
}

/// Queue depth, today's tokens/cost, headroom, and pause state.
async fn ai_status(socket: &Path) -> Result<()> {
    let usage = ai_client(socket)
        .await?
        .get_usage(GetUsageRequest {})
        .await
        .context("GetUsage RPC failed")?
        .into_inner();

    println!("enabled: {}", usage.enabled);
    println!("paused:  {}", usage.paused);
    if let Some(queue) = &usage.queue {
        println!(
            "queue:   {} ready, {} backing off, {} leased, {} dead",
            queue.ready, queue.backing_off, queue.leased, queue.dead
        );
    }
    if let Some(today) = &usage.today {
        println!(
            "today:   {} request(s), {} tokens, ${:.4}",
            today.requests,
            today.input_tokens + today.output_tokens,
            today.cost_usd
        );
    }
    println!(
        "caps:    ${:.2}/day, ${:.2}/month, {} tokens/day",
        usage.daily_cost_cap_usd, usage.monthly_cost_cap_usd, usage.daily_token_cap
    );
    Ok(())
}

/// Force a fresh deep-pass analysis, printing tokens as they stream in and
/// the final structured result once the daemon has persisted it.
async fn ai_process(socket: &Path, message_id: i64) -> Result<()> {
    let mut stream = ai_client(socket)
        .await?
        .analyze_message(AnalyzeMessageRequest { message_id })
        .await
        .context("AnalyzeMessage RPC failed")?
        .into_inner();

    let mut printed_any_token = false;
    while let Some(event) = stream.next().await {
        let event = event.context("analyze stream ended with an error")?;
        match event.event {
            Some(analyze_event::Event::Token(token)) => {
                print!("{token}");
                printed_any_token = true;
                let _ = std::io::Write::flush(&mut std::io::stdout());
            }
            Some(analyze_event::Event::ToolUseStart(tool)) => {
                println!("\n[tool use: {}]", tool.name);
            }
            // Streamed live as it arrives; the durable count that matters
            // (and is billed) lives in the audit ledger, not this echo.
            Some(analyze_event::Event::Usage(_)) => {}
            Some(analyze_event::Event::Done(done)) => {
                if printed_any_token {
                    println!();
                }
                println!("stop_reason: {}", done.stop_reason);
                if let Some(summary) = done.result {
                    println!();
                    print_summary(&summary);
                }
            }
            None => {}
        }
    }
    Ok(())
}

/// Print a message's cached AI summary. Never calls the model.
async fn ai_summary(socket: &Path, message_id: i64, json: bool) -> Result<()> {
    let summary = ai_client(socket)
        .await?
        .get_summary(GetSummaryRequest { message_id })
        .await
        .context("GetSummary RPC failed")?
        .into_inner();

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&summary_to_json(&summary))?
        );
    } else {
        print_summary(&summary);
    }
    Ok(())
}

/// Print a message's suggested reply, generating one now (subject to
/// `ai.limits`' spend caps) if none is cached yet.
async fn ai_reply(socket: &Path, message_id: i64) -> Result<()> {
    let summary = ai_client(socket)
        .await?
        .suggest_reply(SuggestReplyRequest { message_id })
        .await
        .context("SuggestReply RPC failed")?
        .into_inner();

    match summary.suggested_reply {
        Some(reply) if !reply.trim().is_empty() => println!("{reply}"),
        _ => println!("no suggested reply for this message"),
    }
    Ok(())
}

/// Requeue every quarantined AI job.
async fn ai_retry(socket: &Path, failed: bool) -> Result<()> {
    if !failed {
        bail!("mail ai retry currently only supports `--failed` (requeue every dead job)");
    }
    let response = ai_client(socket)
        .await?
        .retry_failed(RetryFailedRequest {})
        .await
        .context("RetryFailed RPC failed")?
        .into_inner();
    println!("revived {} job(s)", response.revived);
    Ok(())
}

/// Pause or resume the daemon's AI dispatch loop.
async fn ai_set_paused(socket: &Path, paused: bool) -> Result<()> {
    let response = ai_client(socket)
        .await?
        .set_paused(SetPausedRequest { paused })
        .await
        .context("SetPaused RPC failed")?
        .into_inner();
    println!(
        "ai dispatch loop {}",
        if response.paused { "paused" } else { "resumed" }
    );
    Ok(())
}

/// Token/cost usage for today or, with `--month`, this calendar month.
async fn ai_cost(socket: &Path, month: bool) -> Result<()> {
    let usage = ai_client(socket)
        .await?
        .get_usage(GetUsageRequest {})
        .await
        .context("GetUsage RPC failed")?
        .into_inner();

    let Some(period) = (if month { usage.month } else { usage.today }) else {
        println!("no usage recorded");
        return Ok(());
    };
    println!(
        "{}: {} request(s), {} input tokens, {} output tokens, ${:.4}",
        period.day, period.requests, period.input_tokens, period.output_tokens, period.cost_usd
    );
    Ok(())
}

async fn ai_policy_client(
    socket: &Path,
) -> Result<AiPolicyServiceClient<tonic::transport::Channel>> {
    let channel = rmail_core::connect_uds(socket)
        .await
        .with_context(|| format!("connecting to rmaild at {}", socket.display()))?;
    Ok(AiPolicyServiceClient::new(channel))
}

async fn ai_safety_client(
    socket: &Path,
) -> Result<AiSafetyServiceClient<tonic::transport::Channel>> {
    let channel = rmail_core::connect_uds(socket)
        .await
        .with_context(|| format!("connecting to rmaild at {}", socket.display()))?;
    Ok(AiSafetyServiceClient::new(channel))
}

/// `mail ai scan-injection <id> [--confirm|--revoke]`.
///
/// Always scans first, even when confirming: a confirmation is consent to a
/// specific set of findings (the daemon clears it when a re-scan turns up
/// different ones), so confirming without having just seen them would be
/// consenting to whatever a stale row happened to hold.
async fn ai_scan_injection(
    socket: &Path,
    message_id: i64,
    confirm: bool,
    revoke: bool,
) -> Result<()> {
    let mut client = ai_safety_client(socket).await?;
    let scan = client
        .scan_injection(ScanInjectionRequest { message_id })
        .await
        .context("ScanInjection RPC failed")?
        .into_inner();
    print_injection_scan(&scan);

    if !confirm && !revoke {
        return Ok(());
    }
    if !scan.flagged {
        // Not an error: the user asked for a state this message is already
        // in. Saying so is more useful than a NOT_FOUND from the daemon.
        println!(
            "\nnothing to {}: this message is not flagged",
            if confirm { "confirm" } else { "revoke" }
        );
        return Ok(());
    }
    let flag = client
        .confirm_injection(ConfirmInjectionRequest {
            message_id,
            confirmed: confirm,
        })
        .await
        .context("ConfirmInjection RPC failed")?
        .into_inner()
        .flag;
    match flag {
        Some(flag) if flag.confirmed_at > 0 => {
            println!("\nconfirmed: AI-decided rule actions may now act on message {message_id}")
        }
        Some(_) => println!(
            "\nconfirmation withdrawn: AI-decided rule actions on message {message_id} are \
             withheld again"
        ),
        None => println!("\nthe daemon returned no flag"),
    }
    Ok(())
}

fn print_injection_scan(scan: &ScanInjectionResponse) {
    if !scan.flagged {
        println!("message {}: no prompt-injection signals", scan.message_id);
        return;
    }
    let severity = match InjectionSeverity::try_from(scan.severity) {
        Ok(InjectionSeverity::Hostile) => "hostile",
        Ok(InjectionSeverity::Suspicious) => "suspicious",
        _ => "unknown",
    };
    println!("message {}: FLAGGED ({severity})", scan.message_id);
    println!("kinds:   {}", scan.kinds.join(", "));
    println!(
        "actions: {}",
        if scan.actions_withheld {
            "WITHHELD — a rule matching on claude_is will not act on this message"
        } else if scan.confirmed_at > 0 {
            "allowed (confirmed)"
        } else {
            "allowed (below the configured block threshold)"
        }
    );
    println!("\nwhat it tried:");
    for detection in &scan.detections {
        println!("  [{}] {}", detection.kind, detection.excerpt);
    }
}

/// `mail ai budget set/status`.
async fn ai_budget(socket: &Path, action: BudgetAction) -> Result<()> {
    match action {
        BudgetAction::Set {
            account,
            bulk,
            daily_soft_usd,
            daily_hard_usd,
            daily_soft_tokens,
            daily_hard_tokens,
            monthly_soft_usd,
            monthly_hard_usd,
            monthly_soft_tokens,
            monthly_hard_tokens,
        } => {
            let class = if bulk {
                BudgetClass::Bulk
            } else {
                BudgetClass::All
            };
            let response = ai_policy_client(socket)
                .await?
                .set_budget(SetBudgetRequest {
                    account_id: account,
                    class: class.into(),
                    caps: Some(BudgetCaps {
                        daily: Some(BudgetWindowCaps {
                            soft_usd: daily_soft_usd,
                            hard_usd: daily_hard_usd,
                            soft_tokens: daily_soft_tokens,
                            hard_tokens: daily_hard_tokens,
                        }),
                        monthly: Some(BudgetWindowCaps {
                            soft_usd: monthly_soft_usd,
                            hard_usd: monthly_hard_usd,
                            soft_tokens: monthly_soft_tokens,
                            hard_tokens: monthly_hard_tokens,
                        }),
                    }),
                })
                .await
                .context("SetBudget RPC failed")?
                .into_inner();
            println!(
                "budget stored for {} ({})",
                scope_label(response.account_id),
                class_label(response.class())
            );
            if let Some(caps) = &response.caps {
                print_caps(caps);
            }
            Ok(())
        }
        BudgetAction::Status { account } => {
            let spend = ai_policy_client(socket)
                .await?
                .get_spend(GetSpendRequest {
                    account_id: account,
                })
                .await
                .context("GetSpend RPC failed")?
                .into_inner();
            println!(
                "{} — day {}, month {}",
                scope_label(spend.account_id),
                spend.day,
                spend.month
            );
            for class in [spend.all.as_ref(), spend.bulk.as_ref()]
                .into_iter()
                .flatten()
            {
                print_class_spend(class);
            }
            Ok(())
        }
    }
}

fn scope_label(account_id: i64) -> String {
    if account_id == 0 {
        "global budget".to_owned()
    } else {
        format!("account {account_id}")
    }
}

fn class_label(class: BudgetClass) -> &'static str {
    match class {
        BudgetClass::All => "all",
        BudgetClass::Bulk => "bulk",
        BudgetClass::Unspecified => "unspecified",
    }
}

fn print_caps(caps: &BudgetCaps) {
    for (window, window_caps) in [
        ("daily", caps.daily.as_ref()),
        ("monthly", caps.monthly.as_ref()),
    ] {
        let Some(window_caps) = window_caps else {
            continue;
        };
        println!(
            "  {window:<8} soft ${} / hard ${}, soft {} / hard {} tokens",
            opt_usd(window_caps.soft_usd),
            opt_usd(window_caps.hard_usd),
            opt_count(window_caps.soft_tokens),
            opt_count(window_caps.hard_tokens),
        );
    }
}

fn print_class_spend(class: &ClassSpend) {
    let source = if class.stored {
        "set"
    } else {
        "derived from ai.limits"
    };
    println!("\n{} budget ({source}):", class_label(class.class()));
    for (window, spend) in [
        ("daily", class.daily.as_ref()),
        ("monthly", class.monthly.as_ref()),
    ] {
        let Some(spend) = spend else { continue };
        println!(
            "  {window:<8} spent ${:.4}, {} tokens",
            spend.usd, spend.tokens
        );
    }
    if let Some(caps) = &class.caps {
        print_caps(caps);
    }
}

/// A dollar cap, or `-` when that dimension is uncapped. Uncapped is not
/// zero: printing `$0.00` for an absent cap would read as "spend nothing".
fn opt_usd(value: Option<f64>) -> String {
    value.map_or_else(|| "-".to_owned(), |v| format!("{v:.4}"))
}

fn opt_count(value: Option<i64>) -> String {
    value.map_or_else(|| "-".to_owned(), |v| v.to_string())
}

/// A formatted, human-readable rendering of a `Summary` — shared by
/// `mail ai summary` and the terminal frame of `mail ai process`'s stream.
fn print_summary(summary: &Summary) {
    println!("message_id: {}", summary.message_id);
    println!("status:     {}", status_name(summary.status()));
    if let Some(tl_dr) = &summary.tl_dr {
        println!("tl;dr:      {tl_dr}");
    }
    if let Some(category) = &summary.category {
        println!("category:   {category}");
    }
    if let Some(priority) = &summary.priority {
        println!("priority:   {priority}");
    }
    if let Some(sentiment) = &summary.sentiment {
        println!("sentiment:  {sentiment}");
    }
    if let Some(needs_reply) = summary.needs_reply {
        println!("needs_reply: {needs_reply}");
    }
    if !summary.suggested_tags.is_empty() {
        println!("tags:       {}", summary.suggested_tags.join(", "));
    }
    if let Some(text) = &summary.summary {
        println!("\nsummary:\n{text}");
    }
    if !summary.key_points.is_empty() {
        println!("\nkey points:");
        for point in &summary.key_points {
            println!("  - {point}");
        }
    }
    if !summary.todos.is_empty() {
        println!("\ntodos:");
        for todo in &summary.todos {
            let due = todo.due.as_deref().unwrap_or("no due date");
            let owner = todo.owner.as_deref().unwrap_or("unassigned");
            println!("  - {} (due: {due}, owner: {owner})", todo.text);
        }
    }
    if let Some(reply) = &summary.suggested_reply {
        println!("\nsuggested reply:\n{reply}");
    }
}

fn status_name(status: rmail_proto::v1::SummaryStatus) -> &'static str {
    status.as_str_name().trim_start_matches("SUMMARY_STATUS_")
}

/// `Summary` as `serde_json::Value` — the generated proto type does not
/// derive `Serialize` (`build.rs` does not enable prost-build's serde
/// support, and `build.rs` is off limits — see that file's own header), so
/// `mail ai summary --json` builds one by hand rather than leaving `--json`
/// unimplemented.
fn summary_to_json(summary: &Summary) -> serde_json::Value {
    serde_json::json!({
        "message_id": summary.message_id,
        "thread_id": summary.thread_id,
        "status": status_name(summary.status()),
        "triage_model": summary.triage_model,
        "tl_dr": summary.tl_dr,
        "sentiment": summary.sentiment,
        "category": summary.category,
        "priority": summary.priority,
        "needs_reply": summary.needs_reply,
        "suggested_tags": summary.suggested_tags,
        "deep_model": summary.deep_model,
        "summary": summary.summary,
        "thread_summary": summary.thread_summary,
        "key_points": summary.key_points,
        "todos": summary.todos.iter().map(|t| serde_json::json!({
            "text": t.text,
            "due": t.due,
            "owner": t.owner,
        })).collect::<Vec<_>>(),
        "entities": summary.entities.iter().map(|e| serde_json::json!({
            "kind": e.kind,
            "value": e.value,
            "iso": e.iso,
            "amount": e.amount,
            "currency": e.currency,
        })).collect::<Vec<_>>(),
        "suggested_reply": summary.suggested_reply,
    })
}
