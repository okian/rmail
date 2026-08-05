//! The `mail` CLI — a thin gRPC client for the rmail daemon.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use rmail_core::socket_path_from_env;
use rmail_proto::v1::admin_service_client::AdminServiceClient;
use rmail_proto::v1::sync_service_client::SyncServiceClient;
use rmail_proto::v1::{
    EventKind, ListTokensRequest, MintTokenRequest, RevokeTokenRequest, SyncFolderRequest,
    SyncMode, WatchEventsRequest,
};
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
