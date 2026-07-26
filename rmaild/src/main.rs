//! rmail daemon entry point.

use anyhow::Result;
use rmail_core::socket_path_from_env;
use rmail_core::telemetry::{self, LogFormat};

#[tokio::main]
async fn main() -> Result<()> {
    // Install the tracing subscriber before anything else logs. Format is
    // selected by RMAIL_LOG_FORMAT (text|json); levels by RUST_LOG.
    telemetry::init(LogFormat::from_env())?;

    let socket = socket_path_from_env();
    tracing::info!(socket = %socket.display(), "starting rmaild");

    rmaild::serve_uds(&socket, rmaild::shutdown_signal()).await?;

    tracing::info!("rmaild stopped");
    Ok(())
}
