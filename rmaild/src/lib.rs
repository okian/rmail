//! The rmail daemon library.
//!
//! [`serve_uds`] boots the tonic server on a Unix domain socket exposing gRPC
//! health (`SERVING`), reflection over the `rmail.v1` descriptor set, and
//! `AccountService` — all wrapped in a [`RequestTraceLayer`] that opens a span
//! per RPC. It is exposed as a library function so both the `rmaild` binary and
//! integration tests drive the same code path.

mod account_service;
mod sync_service;
mod trace;

pub use account_service::AccountApi;
pub use sync_service::SyncApi;
pub use trace::RequestTraceLayer;

use std::future::Future;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use rmail_core::events::{EventLog, Retention};
use rmail_core::sync::{SyncEngine, SyncOptions};
use rmail_core::{Config, Database};
use rmail_proto::v1::account_service_server::AccountServiceServer;
use rmail_proto::v1::sync_service_server::SyncServiceServer;
use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;
use tokio_util::sync::CancellationToken;
use tonic::transport::Server;

/// How often the event log is pruned to its retention bounds.
///
/// Retention is a bound, not a deadline: pruning hourly keeps the log within
/// an hour's growth of its limit, which is the difference between a bound that
/// holds and one that is checked so often it costs more than it saves.
const PRUNE_INTERVAL: Duration = Duration::from_secs(60 * 60);

/// Errors returned while standing up or running the gRPC server.
#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    /// Binding the Unix domain socket failed.
    #[error("failed to bind unix socket at {path}: {source}")]
    Bind {
        /// Socket path that could not be bound.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// A filesystem operation around the socket (mkdir, chmod, unlink) failed.
    #[error("socket filesystem error: {0}")]
    Io(#[from] std::io::Error),

    /// The tonic transport layer failed while serving.
    #[error("gRPC transport error: {0}")]
    Transport(#[from] tonic::transport::Error),

    /// Building the reflection service failed.
    #[error("gRPC reflection setup error: {0}")]
    Reflection(#[from] tonic_reflection::server::Error),
}

/// Serve the rmail gRPC surface on a Unix domain socket until `shutdown`
/// resolves.
///
/// The socket is created with `0600` permissions (owner-only). A stale socket
/// left by a previous run is removed first, and the socket file is unlinked on
/// graceful shutdown. The server exposes:
///
/// - `grpc.health.v1.Health` (reporting `SERVING`), and
/// - gRPC reflection (v1) over the compiled `rmail.v1` descriptor set.
///
/// # Errors
///
/// Returns [`ServeError`] if the socket cannot be prepared/bound, the
/// reflection service cannot be built, or the transport fails while serving.
pub async fn serve_uds<F>(
    socket_path: impl AsRef<Path>,
    db: Database,
    shutdown: F,
) -> Result<(), ServeError>
where
    F: Future<Output = ()> + Send + 'static,
{
    serve_uds_with_config(socket_path, db, Config::default(), shutdown).await
}

/// [`serve_uds`] with an explicit configuration.
///
/// Split out so the binary can pass the loaded config while tests keep the
/// short form.
///
/// # Errors
///
/// As [`serve_uds`].
pub async fn serve_uds_with_config<F>(
    socket_path: impl AsRef<Path>,
    db: Database,
    config: Config,
    shutdown: F,
) -> Result<(), ServeError>
where
    F: Future<Output = ()> + Send + 'static,
{
    let events = EventLog::new(
        db.clone(),
        Retention {
            max_rows: i64::try_from(config.grpc.events.retention_rows).ok(),
            max_age: Some(Duration::from_secs(
                u64::from(config.grpc.events.retention_days) * 24 * 60 * 60,
            )),
        },
    );
    let engine = SyncEngine::new(db.clone(), events, SyncOptions::default());
    serve_uds_with_engine(socket_path, db, engine, shutdown).await
}

/// [`serve_uds`] over a caller-supplied [`SyncEngine`].
///
/// The engine owns the event log, and the log's in-process fan-out only reaches
/// subscribers of *that instance* — a second `EventLog` over the same database
/// shares the durable rows but not the live channel. Tests that need to append
/// events the server's stream will see must therefore hand in the same engine
/// the server uses, which is what this exists for.
///
/// # Errors
///
/// As [`serve_uds`].
pub async fn serve_uds_with_engine<F>(
    socket_path: impl AsRef<Path>,
    db: Database,
    engine: SyncEngine,
    shutdown: F,
) -> Result<(), ServeError>
where
    F: Future<Output = ()> + Send + 'static,
{
    let path = socket_path.as_ref().to_path_buf();

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            // Create the runtime directory owner-only. We only set the mode when
            // we create it — never chmod a pre-existing (possibly shared) dir.
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)?;
        }
    }

    // Bind to a temporary sibling path, lock it down to 0600, then atomically
    // rename it into place. This closes the window in which a freshly bound
    // socket is reachable at its final path under umask-derived (world-visible)
    // permissions, and atomically replaces any stale socket left by a prior run.
    let tmp_path = path.with_file_name(format!(
        "{}.tmp.{}",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "rmaild.sock".to_owned()),
        std::process::id()
    ));
    let _ = std::fs::remove_file(&tmp_path);

    let listener = UnixListener::bind(&tmp_path).map_err(|source| ServeError::Bind {
        path: path.clone(),
        source,
    })?;
    // Restrict the socket to the owning user before it is reachable at `path`.
    if let Err(e) = std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600)) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e.into());
    }
    if let Err(e) = std::fs::rename(&tmp_path, &path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(e.into());
    }

    let (mut health_reporter, health_service) = tonic_health::server::health_reporter();
    // Empty service name is the server-wide health per the gRPC health spec.
    health_reporter
        .set_service_status("", tonic_health::ServingStatus::Serving)
        .await;

    let reflection = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(rmail_proto::FILE_DESCRIPTOR_SET)
        .build_v1()?;

    let events = engine.events().clone();

    // One token stops everything the server spawned. It is cancelled when
    // shutdown *begins*, not after the transport returns: tonic's graceful
    // shutdown waits for active connections to close, and a client parked on an
    // open event stream keeps its connection alive indefinitely. Cancelling
    // first ends those streams, which lets their connections close, which lets
    // the shutdown the user asked for actually happen.
    let stopping = CancellationToken::new();
    let shutdown = {
        let stopping = stopping.clone();
        async move {
            shutdown.await;
            stopping.cancel();
        }
    };
    let pruner = tokio::spawn({
        let events = events.clone();
        let stopping = stopping.clone();
        async move {
            loop {
                tokio::select! {
                    () = stopping.cancelled() => return,
                    () = tokio::time::sleep(PRUNE_INTERVAL) => {}
                }
                if let Err(error) = events.prune().await {
                    // Retention falling behind is a disk problem, not a
                    // correctness one: the log stays gapless either way.
                    tracing::warn!(%error, "event log prune failed");
                }
            }
        }
    });

    let account_service = AccountServiceServer::new(AccountApi::new(db));
    let sync_service = SyncServiceServer::new(SyncApi::new(engine, stopping.clone()));

    let incoming = UnixListenerStream::new(listener);
    let serve_result = Server::builder()
        // Every RPC runs inside a request-tracing span.
        .layer(RequestTraceLayer::new())
        .add_service(health_service)
        .add_service(reflection)
        .add_service(account_service)
        .add_service(sync_service)
        .serve_with_incoming_shutdown(incoming, shutdown)
        .await;

    stopping.cancel();
    let _ = pruner.await;

    // Best-effort cleanup so the next boot starts clean regardless of outcome.
    let _ = std::fs::remove_file(&path);

    serve_result.map_err(ServeError::from)
}

/// Resolve when the process receives SIGINT (Ctrl-C) or SIGTERM.
///
/// Used as the graceful-shutdown trigger for [`serve_uds`] in the binary.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            // If we cannot install the SIGTERM handler, fall back to waiting on
            // Ctrl-C alone rather than shutting down immediately.
            Err(_) => std::future::pending::<()>().await,
        }
    };

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
