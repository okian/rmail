//! The `mail` CLI — a thin gRPC client for the rmail daemon.

mod note_cli;
mod search_cli;

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use note_cli::{NoteAction, NotesArgs};
use rmail_core::socket_path_from_env;
use rmail_proto::v1::admin_service_client::AdminServiceClient;
use rmail_proto::v1::ai_service_client::AiServiceClient;
use rmail_proto::v1::sync_service_client::SyncServiceClient;
use rmail_proto::v1::{
    analyze_event, AnalyzeMessageRequest, EventKind, GetSummaryRequest, GetUsageRequest,
    ListTokensRequest, MintTokenRequest, RetryFailedRequest, RevokeTokenRequest, SetPausedRequest,
    SuggestReplyRequest, Summary, SyncFolderRequest, SyncMode, WatchEventsRequest,
};
use search_cli::{SearchArgs, SimilarArgs};
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
        },
        Command::Note { action } => note_cli::dispatch(&socket, action).await,
        Command::Notes(args) => note_cli::list(&socket, args).await,
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
