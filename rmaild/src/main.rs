//! rmail daemon entry point.

use anyhow::{Context, Result};
use rmail_core::telemetry::{self, LogFormat};
use rmail_core::{config_path_from_env, db_path_from_env, socket_path_from_env, Config, Database};

#[tokio::main]
async fn main() -> Result<()> {
    // Install the tracing subscriber before anything else logs. Format is
    // selected by RMAIL_LOG_FORMAT (text|json); levels by RUST_LOG.
    telemetry::init(LogFormat::from_env())?;

    let socket = socket_path_from_env();
    let db_path = db_path_from_env();
    let config_path = config_path_from_env();
    // A missing config file is the normal first-run state, not a failure — the
    // defaults are the documented ones. A *malformed* one is a failure, because
    // silently ignoring it would run with settings the operator did not choose.
    let config = Config::load_or_default(&config_path)
        .with_context(|| format!("loading config from {}", config_path.display()))?;
    tracing::info!(
        socket = %socket.display(),
        db = %db_path.display(),
        config = %config_path.display(),
        "starting rmaild"
    );

    // Open the local database, running pending migrations idempotently.
    let db = Database::open(&db_path)
        .with_context(|| format!("opening database at {}", db_path.display()))?;

    rmaild::serve_uds_with_config(&socket, db, config, rmaild::shutdown_signal()).await?;

    tracing::info!("rmaild stopped");
    Ok(())
}
