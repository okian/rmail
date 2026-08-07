//! The rmail daemon library.
//!
//! [`serve_uds`] boots the tonic server on a Unix domain socket exposing gRPC
//! health (`SERVING`), reflection over the `rmail.v1` descriptor set, and the
//! `AccountService`/`SyncService`/`AdminService`/`MailService`/
//! `SearchService`/`AiService` handlers — all wrapped in a
//! [`RequestTraceLayer`] (opens a span per RPC) and an [`AuthLayer`] (enforces
//! per-method capability scope; see `auth::methods` for the table). It is
//! exposed as a library function so both the `rmaild` binary and integration
//! tests drive the same code path.

mod account_service;
mod admin_service;
mod ai_service;
mod audit_service;
mod auth;
mod mail_service;
mod note_service;
mod search_service;
mod sync_service;
mod trace;

pub use account_service::AccountApi;
pub use admin_service::AdminApi;
pub use ai_service::AiApi;
pub use audit_service::AuditApi;
pub use auth::AuthLayer;
pub use mail_service::MailApi;
pub use note_service::NoteApi;
pub use search_service::SearchApi;
pub use sync_service::SyncApi;
pub use trace::RequestTraceLayer;

use std::future::Future;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rmail_core::ai::{
    self, AiDispatchLoop, AiPauseFlag, AiQueue, AiWorkerPool, BatchClient, BatchCoordinator,
    DeepPassGate, DeepPassHandler, PassHandler, PolicyEngine, Provider as AiProvider,
    QueueOptions as AiQueueOptions, TriagePassHandler,
};
use rmail_core::embed::hash::HashEmbedder;
use rmail_core::embed::Embedder;
use rmail_core::events::{EventLog, Retention};
use rmail_core::imap::mutate::LiveImapMutator;
use rmail_core::index::semantic::VECTOR_DIM;
use rmail_core::index::{IndexQueue, QueueOptions as IndexQueueOptions};
use rmail_core::mail::MailStore;
use rmail_core::notes::NoteStore;
use rmail_core::rank::l1::Weights;
use rmail_core::sync::{SyncEngine, SyncOptions};
use rmail_core::{Config, Database};
use rmail_proto::v1::account_service_server::AccountServiceServer;
use rmail_proto::v1::admin_service_server::AdminServiceServer;
use rmail_proto::v1::ai_service_server::AiServiceServer;
use rmail_proto::v1::audit_service_server::AuditServiceServer;
use rmail_proto::v1::mail_service_server::MailServiceServer;
use rmail_proto::v1::note_service_server::NoteServiceServer;
use rmail_proto::v1::search_service_server::SearchServiceServer;
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

    /// `[search.rank_weights]` named a key that is not a real feature, or
    /// gave one a non-finite value — caught before the socket is even bound,
    /// so a typo'd override fails the daemon loudly at startup instead of
    /// silently reverting every request to the unmodified cold-start table.
    #[error("invalid search.rank_weights: {0}")]
    InvalidRankWeights(#[from] rmail_core::rank::l1::RankError),

    /// `ai.policy` named an account that is not configured, or carries a
    /// malformed rule — caught here, before any socket is bound, for the
    /// same reason `InvalidRankWeights` is: a misconfigured policy should
    /// fail the daemon loudly at startup, not silently protect nothing.
    ///
    /// Deliberately not `#[from]`: this function's own `rmail_core::Error`
    /// source is exactly one call
    /// ([`rmail_core::ai::PolicyEngine::from_config`]), mapped explicitly at
    /// that call site — a blanket `#[from] rmail_core::Error` would silently
    /// re-label *any* future `?` on that type anywhere else in this
    /// function as "invalid ai.policy," which is not what it would mean.
    #[error("invalid ai.policy: {0}")]
    InvalidAiPolicy(#[source] rmail_core::Error),
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
    // Semantic indexing off: this is the short form the tests use, and warming
    // an embedder here would make every one of them load — or, on a cold cache,
    // *download* — a several-hundred-megabyte model. Callers that want the real
    // thing pass their own config.
    let mut config = Config::default();
    config.index.semantic.enabled = false;
    serve_uds_with_config(socket_path, db, config, shutdown).await
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
            // Saturating, not `.ok()`: an out-of-range value mapping to
            // `None` would read as *unlimited*, turning a config typo into
            // unbounded disk growth — the exact failure the zero case is
            // guarded against.
            max_rows: Some(i64::try_from(config.grpc.events.retention_rows).unwrap_or(i64::MAX)),
            max_age: Some(Duration::from_secs(
                u64::from(config.grpc.events.retention_days) * 24 * 60 * 60,
            )),
        },
    );
    let engine = SyncEngine::new(db.clone(), events, SyncOptions::default());

    serve_uds_with_engine(socket_path, db, engine, &config, shutdown).await
}

/// Build the configured embedder and start loading its model in the background.
///
/// Returns the embedder so the caller can keep it alive; the returned guard also
/// stops the warming task when it is dropped, so a shutdown during a model load
/// does not wait on it. Warming is deliberately not awaited: loading is hundreds
/// of megabytes of work, and a daemon that accepts no connection until it
/// finishes is worse than one whose first semantic query is slow.
#[must_use = "the returned guard is what keeps the loaded model alive; dropping it immediately frees the weights the warm-up just paid for"]
fn warm_embedder(config: &Config) -> Option<WarmEmbedder> {
    if !config.index.semantic.enabled {
        tracing::debug!("semantic indexing is disabled; no embedder will be loaded");
        return None;
    }
    let embedder = match rmail_core::embed::build(&config.index.semantic) {
        Ok(embedder) => embedder,
        Err(error) => {
            tracing::warn!(%error, "no embedder configured; search will be lexical only");
            return None;
        }
    };
    let warming = tokio::spawn({
        let embedder = std::sync::Arc::clone(&embedder);
        async move {
            let started = std::time::Instant::now();
            match embedder.warm().await {
                Ok(()) => tracing::info!(
                    model = embedder.model(),
                    elapsed_ms = started.elapsed().as_millis(),
                    "embedder warm"
                ),
                // Not fatal: a daemon whose other twenty features do not need
                // embeddings should not fail to start because a model cache is
                // unprovisioned. The cost is that semantic search loads the
                // model on first use, or degrades to lexical.
                Err(error) => tracing::warn!(
                    %error,
                    "could not warm the embedder; semantic search will load it \
                     on first use or degrade to lexical"
                ),
            }
        }
    });
    Some(WarmEmbedder {
        embedder,
        warming: Some(warming),
    })
}

/// Keeps a warmed embedder loaded and its warming task bounded by the server's
/// lifetime.
struct WarmEmbedder {
    embedder: std::sync::Arc<dyn rmail_core::embed::Embedder>,
    warming: Option<tokio::task::JoinHandle<()>>,
}

impl WarmEmbedder {
    /// The embedder being kept loaded — cloned (an `Arc` bump, not a second
    /// model load) by `SearchApi`'s own construction so the daemon's search
    /// path reuses the identical warmed instance instead of building a
    /// second one.
    fn embedder(&self) -> &std::sync::Arc<dyn rmail_core::embed::Embedder> {
        &self.embedder
    }

    /// The embedder's model id.
    #[cfg(test)]
    fn model(&self) -> &str {
        self.embedder.model()
    }
}

impl Drop for WarmEmbedder {
    fn drop(&mut self) {
        // A detached warm-up outlives the server that asked for it, and the
        // runtime waits on blocking tasks at drop — so a shutdown arriving
        // during a model load would block on it.
        if let Some(warming) = self.warming.take() {
            warming.abort();
        }
    }
}

/// [`serve_uds`] over a caller-supplied [`SyncEngine`].
///
/// The engine owns the event log, and the log's in-process fan-out only reaches
/// subscribers of *that instance* — a second `EventLog` over the same database
/// shares the durable rows but not the live channel. Tests that need to append
/// events the server's stream will see must therefore hand in the same engine
/// the server uses, which is what this exists for.
///
/// The `MailService` handler is built here over a [`LiveImapMutator`] — a
/// real (if test-scale) IMAP client. Tests exercising `MailService`'s
/// IMAP-reflection ordering want a fake instead (there is no live server to
/// dial in-process); see [`serve_uds_with_engine_and_mail_store`] for the
/// entry point that takes a caller-built [`MailStore`].
///
/// `config` is read for `SearchService`'s wiring (task 33) — the embedder to
/// warm/reuse, `[search]` settings, and `[index.semantic]` — the same
/// `Config` a caller building its own `SyncEngine` presumably loaded
/// already, so this takes it by reference rather than re-deriving a default.
///
/// # Errors
///
/// As [`serve_uds`], plus [`ServeError::InvalidRankWeights`] if
/// `config.search.rank_weights` names a key that is not a real feature or
/// gives one a non-finite value.
pub async fn serve_uds_with_engine<F>(
    socket_path: impl AsRef<Path>,
    db: Database,
    engine: SyncEngine,
    config: &Config,
    shutdown: F,
) -> Result<(), ServeError>
where
    F: Future<Output = ()> + Send + 'static,
{
    // A fresh IMAP connection per mutation (see `LiveImapMutator`'s docs) —
    // cheap to construct, since it is only ever `Arc`-cloned, not pooled.
    let mail_store = MailStore::new(
        db.clone(),
        engine.events().clone(),
        std::sync::Arc::new(LiveImapMutator::new(db.clone())),
    );
    serve_uds_with_engine_and_mail_store(socket_path, db, engine, mail_store, config, shutdown)
        .await
}

/// [`serve_uds_with_engine`] over a caller-supplied [`MailStore`] as well —
/// for tests that need `MailService`'s IMAP calls to go through a fake
/// [`rmail_core::imap::mutate::ImapMutator`] rather than
/// [`LiveImapMutator`]'s real (dial-out) one. Production code has no reason
/// to call this directly; [`serve_uds_with_engine`] builds the same
/// `MailStore` `serve_uds`/`serve_uds_with_config` would.
///
/// `mail_store` should be built over the *same* `EventLog` instance as
/// `engine` (i.e. `engine.events().clone()`) — see [`serve_uds_with_engine`]'s
/// docs for why a second `EventLog` over the same database is not equivalent.
///
/// This is also where `SearchService`'s embedder is built and warmed — see
/// this function's own body: previously (before task 33) the embedder was
/// warmed one layer up in [`serve_uds_with_config`] and then dropped with
/// nothing left to hand it to. Consolidating warm-up and `SearchApi`
/// construction into one place is what keeps a real (ONNX) model from being
/// loaded twice — once to warm, once for search — since "one embedder serves
/// the whole process" is [`rmail_core::embed::Embedder`]'s own documented
/// contract.
///
/// # Errors
///
/// As [`serve_uds`], plus [`ServeError::InvalidRankWeights`] under the same
/// condition [`serve_uds_with_engine`] documents.
pub async fn serve_uds_with_engine_and_mail_store<F>(
    socket_path: impl AsRef<Path>,
    db: Database,
    engine: SyncEngine,
    mail_store: MailStore,
    config: &Config,
    shutdown: F,
) -> Result<(), ServeError>
where
    F: Future<Output = ()> + Send + 'static,
{
    // Validated before any socket/filesystem side effect: a typo'd
    // `[search.rank_weights]` key should fail the daemon loudly at startup,
    // not bind a socket first and then fail — see
    // `rank::l1::Weights::from_config`'s own docs on why nothing validated
    // this automatically before `SearchService` existed to call it.
    let rank_weights = Weights::from_config(&config.search.rank_weights)?;
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

    // The daemon's own effective uid, read off `tmp_path` — the socket this
    // process just bound under a name nothing else can predict — rather than
    // `path` after the rename below, and rather than a raw `geteuid()` call.
    // The auth layer grants implicit admin to a Unix-socket peer only when
    // its *kernel-reported* uid (`getpeereid(2)` on Darwin, `SO_PEERCRED` on
    // Linux — see `tokio::net::UnixStream::peer_cred`, surfaced per-request
    // via `tonic::transport::server::UdsConnectInfo`) matches this value, so
    // the `0600` permission set above is defense-in-depth, not the only gate
    // — and reading it here, not after `path` is publicly reachable, closes
    // the TOCTOU window a stat-after-rename would leave open if `path`'s
    // parent were ever a pre-existing, not-owner-only directory (the one case
    // this function deliberately does not chmod — see below).
    let admin_uid = std::fs::metadata(&tmp_path)?.uid();

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
                // Prune once at startup before sleeping. A daemon restarted
                // more often than the interval would otherwise never prune at
                // all, which is exactly the machine that most needs it.
                if let Err(error) = events.prune().await {
                    tracing::warn!(%error, "event log prune failed");
                }
                tokio::select! {
                    () = stopping.cancelled() => return,
                    () = tokio::time::sleep(PRUNE_INTERVAL) => {}
                }
            }
        }
    });

    let admin_service = AdminServiceServer::new(AdminApi::new(db.clone()));
    let account_service = AccountServiceServer::new(AccountApi::new(db.clone()));
    let audit_service = AuditServiceServer::new(AuditApi::new(db.clone(), stopping.clone()));
    let sync_service = SyncServiceServer::new(SyncApi::new(engine, stopping.clone()));
    let mail_service = MailServiceServer::new(MailApi::new(mail_store, stopping.clone()));

    // A dedicated `IndexQueue` handle rather than reusing the AI subsystem's
    // (below) — `IndexQueue` is a cheap, stateless wrapper over `db` (see its
    // own docs), so a second instance costs nothing and keeps this task's
    // wiring independent of the AI subsystem's.
    let note_store = NoteStore::new(
        db.clone(),
        IndexQueue::new(db.clone(), IndexQueueOptions::default()),
        config.notes.index,
    );
    let note_service = NoteServiceServer::new(NoteApi::new(note_store, stopping.clone()));

    // Held for the lifetime of the server (bound here, dropped only when this
    // function returns): a model loaded into an `Arc` that a warming task
    // then drops is a model that is immediately freed — the log line claims
    // success and the first query pays for the load all over again.
    // `SearchApi` gets a clone of the *same* `Arc`, not a second embedder, so
    // a real (ONNX) model is loaded at most once per daemon process.
    let warm = warm_embedder(config);
    let embedder: Arc<dyn Embedder> = warm
        .as_ref()
        .map(WarmEmbedder::embedder)
        .cloned()
        .unwrap_or_else(|| {
            // Semantic indexing disabled, or the configured backend failed to
            // build (already logged by `warm_embedder`/`embed::build`) —
            // search still needs *something* to embed queries with. The
            // deterministic hash fallback (`embed::hash`'s own docs: "exists
            // so the retrieval pipeline has one code path instead of two")
            // keeps the dense retriever's code path live rather than absent;
            // with `vec_chunks` unpopulated either way, it costs nothing real.
            Arc::new(HashEmbedder::new(VECTOR_DIM)) as Arc<dyn Embedder>
        });
    let search_service = SearchServiceServer::new(SearchApi::new(
        db.clone(),
        embedder,
        rank_weights,
        config.search.clone(),
        &config.index.semantic,
        stopping.clone(),
    ));

    // AI subsystem: the durable queue, the two pass handlers, the worker
    // pool/batch coordinator that drive them, and the dispatch loop that
    // closes the loop between "a message synced" and "a triage job ran" —
    // see `rmail_core::ai::dispatch`'s own module docs for why task 50 owns
    // that wiring and what gap it closes.
    //
    // `AiService` is always registered (reflection and the auth scope table
    // must see every RPC regardless of runtime config); `ai_active` gates
    // real *work*, not registration. A disabled or misconfigured AI
    // subsystem falls back to `ai_service::NullProvider`, so every
    // provider-calling RPC fails fast with `FAILED_PRECONDITION` instead of
    // ever dialing out, and the dispatch loop is simply never spawned —
    // prd.md's "AI down → AiService health NOT_SERVING, mail features
    // unaffected" (health-service granularity is left for a later task; the
    // effect on served behavior is already correct).
    let ai_policy =
        Arc::new(PolicyEngine::from_config(config).map_err(ServeError::InvalidAiPolicy)?);
    let (ai_provider, ai_active): (Arc<dyn AiProvider>, bool) = if config.ai.enabled {
        match ai::provider::build(&config.ai) {
            Ok(provider) => (provider, true),
            Err(error) => {
                tracing::warn!(
                    %error,
                    "could not build the configured AI provider; AI features are disabled \
                     until this is fixed"
                );
                (Arc::new(ai_service::NullProvider), false)
            }
        }
    } else {
        tracing::info!("ai.enabled = false; AI features are disabled on this daemon");
        (Arc::new(ai_service::NullProvider), false)
    };

    let ai_queue = AiQueue::new(db.clone(), AiQueueOptions::default());
    let index_queue = IndexQueue::new(db.clone(), IndexQueueOptions::default());
    let deep_handler = Arc::new(DeepPassHandler::new(
        db.clone(),
        index_queue,
        config.ai.models.deep.clone(),
        config.ai.deep_pass.clone(),
    ));
    let triage_handler: Arc<dyn PassHandler> = Arc::new(
        TriagePassHandler::new(db.clone(), config.ai.models.triage.clone())
            .with_deep_pass_gate(DeepPassGate::new(config.ai.deep_pass.clone())),
    );
    let ai_handlers: Vec<Arc<dyn PassHandler>> = vec![
        triage_handler,
        Arc::clone(&deep_handler) as Arc<dyn PassHandler>,
    ];

    let ai_worker_pool = AiWorkerPool::new(
        db.clone(),
        ai_queue.clone(),
        Arc::clone(&ai_provider),
        Arc::clone(&ai_policy),
        config.ai.limits.clone(),
        config.ai.privacy.clone(),
        ai_handlers.clone(),
        "rmaild-ai-worker",
        events.clone(),
    );
    // Shared with `AiApi` below — a forced `AnalyzeMessage`/`SuggestReply`
    // call must draw from the *same* concurrency/RPM budget this pool
    // enforces for its own queued dispatch, not a second, independent one
    // that would let the two paths together exceed `ai.limits` in practice.
    // See `AiWorkerPool::semaphore`/`AiApi::new`'s own docs.
    let ai_semaphore = ai_worker_pool.semaphore();
    let ai_rate_limiter = ai_worker_pool.rate_limiter();

    // Always starts unpaused, regardless of `ai_active` — a disabled daemon
    // is reported via `GetUsage.enabled = false`, not by pretending it is
    // "paused" (which would misleadingly imply `mail ai resume` could make
    // it start running).
    let ai_pause = AiPauseFlag::new(false);
    let ai_service = AiServiceServer::new(AiApi::new(
        db.clone(),
        ai_queue.clone(),
        events.clone(),
        Arc::clone(&deep_handler),
        Arc::clone(&ai_provider),
        Arc::clone(&ai_policy),
        config.ai.privacy.clone(),
        config.ai.limits.clone(),
        ai_pause.clone(),
        ai_active,
        ai_semaphore,
        ai_rate_limiter,
        stopping.clone(),
    ));

    // Only spawned when the subsystem is actually active — a disabled/
    // misconfigured daemon must not poll the event log and lease an empty
    // queue forever for nothing, and must never try to build a batch client
    // pointed at a provider that was just declared unusable.
    let ai_dispatch_handle = if ai_active {
        let mut dispatch = AiDispatchLoop::new(events.clone(), ai_queue.clone(), ai_worker_pool)
            .with_pause_flag(ai_pause);
        if config.ai.batching.enabled {
            match BatchClient::new() {
                Ok(client) => match BatchCoordinator::new(
                    db.clone(),
                    ai_queue.clone(),
                    client,
                    config.ai.api_key_command.clone(),
                    Arc::clone(&ai_policy),
                    config.ai.limits.clone(),
                    config.ai.privacy.clone(),
                    config.ai.batching.clone(),
                    ai_handlers,
                    events.clone(),
                ) {
                    Ok(coordinator) => {
                        dispatch = dispatch.with_batch(
                            Arc::new(coordinator),
                            vec![
                                ai_service::TRIAGE_PASS.to_owned(),
                                ai_service::DEEP_PASS.to_owned(),
                            ],
                        );
                    }
                    Err(error) => tracing::warn!(
                        %error,
                        "could not build the ai batch coordinator; batch mode disabled"
                    ),
                },
                Err(error) => tracing::warn!(
                    %error,
                    "could not build the ai batch client; batch mode disabled"
                ),
            }
        }
        Some(dispatch.spawn(stopping.clone()))
    } else {
        None
    };

    let incoming = UnixListenerStream::new(listener);
    let serve_result = Server::builder()
        // Every RPC runs inside a request-tracing span; the auth layer sits
        // inside it (so a denied request is still traced) and outside every
        // service (so no service can be added later without it).
        .layer(RequestTraceLayer::new())
        .layer(AuthLayer::new(db, admin_uid))
        .add_service(health_service)
        .add_service(reflection)
        .add_service(admin_service)
        .add_service(audit_service)
        .add_service(account_service)
        .add_service(sync_service)
        .add_service(mail_service)
        .add_service(note_service)
        .add_service(search_service)
        .add_service(ai_service)
        .serve_with_incoming_shutdown(incoming, shutdown)
        .await;

    stopping.cancel();
    let _ = pruner.await;
    if let Some(handle) = ai_dispatch_handle {
        let _ = handle.await;
    }

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

#[cfg(test)]
mod warm_tests {
    use super::{warm_embedder, Config};

    #[tokio::test]
    async fn a_disabled_semantic_index_loads_no_model() {
        // The daemon's default path is also every integration test's path, so
        // an unconditional load here made each of them pay for — and on a cold
        // cache *download* — a hundred and thirty megabytes of weights. Worse,
        // for a product whose claim is that nothing leaves the host, it made
        // contacting Hugging Face at start-up unconditional and unswitchable.
        let mut config = Config::default();
        config.index.semantic.enabled = false;
        assert!(warm_embedder(&config).is_none());
    }

    #[tokio::test]
    async fn an_enabled_semantic_index_keeps_its_embedder_alive() {
        // Retention *is* the warm-up: an embedder moved into the warming task
        // and dropped when it finishes has loaded a model that is immediately
        // freed, so the log claims success and the first query pays again.
        let config = Config::default();
        let warm = warm_embedder(&config).expect("an embedder for the default config");
        assert!(!warm.model().is_empty());
        drop(warm);
    }
}
