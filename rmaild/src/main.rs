//! rmail daemon entry point.

use anyhow::{Context, Result};
use rmail_core::telemetry::{self, LogFormat};
use rmail_core::{db_path_from_env, socket_path_from_env, Database};

#[tokio::main]
async fn main() -> Result<()> {
    // Install the tracing subscriber before anything else logs. Format is
    // selected by RMAIL_LOG_FORMAT (text|json); levels by RUST_LOG.
    telemetry::init(LogFormat::from_env())?;

    let socket = socket_path_from_env();
    let db_path = db_path_from_env();
    tracing::info!(socket = %socket.display(), db = %db_path.display(), "starting rmaild");

    // Open the local database, running pending migrations idempotently.
    let db = Database::open(&db_path)
        .with_context(|| format!("opening database at {}", db_path.display()))?;

    rmaild::serve_uds(&socket, db, rmaild::shutdown_signal()).await?;

    tracing::info!("rmaild stopped");
    Ok(())
}
