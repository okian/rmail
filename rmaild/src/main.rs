//! rmail daemon entry point.

use anyhow::Result;
use rmail_core::socket_path_from_env;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    // Minimal tracing init; the full telemetry baseline lands in task 4.
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    let socket = socket_path_from_env();
    tracing::info!(socket = %socket.display(), "starting rmaild");

    rmaild::serve_uds(&socket, rmaild::shutdown_signal()).await?;

    tracing::info!("rmaild stopped");
    Ok(())
}
