//! The `mail` CLI — a thin gRPC client for the rmail daemon.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use rmail_core::socket_path_from_env;
use rmail_proto::v1::sync_service_client::SyncServiceClient;
use rmail_proto::v1::{EventKind, SyncFolderRequest, SyncMode, WatchEventsRequest};
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
