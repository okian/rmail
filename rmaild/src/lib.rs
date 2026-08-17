//! The rmail daemon library.
//!
//! [`serve_uds`] boots the tonic server on a Unix domain socket exposing gRPC
//! health (`SERVING`), reflection over the `rmail.v1` descriptor set, and the
//! `AccountService`/`SyncService`/`AdminService`/`AuditService`/
//! `MailService`/`NoteService`/`TagService`/`SearchService`/
//! `SavedSearchService`/`FinderService`/`ComposeService`/`SendSchedulerService`/`AiService`/
//! `AnalyticsService`/
//! `AiPolicyService`/`AiSafetyService`/`IndexService`/`HookService`/`RuleService`/
//! `ExportService` handlers — all wrapped in a
//! [`RequestTraceLayer`] (opens a span per RPC) and an [`AuthLayer`] (enforces
//! per-method capability scope; see `auth::methods` for the table). It is
//! exposed as a library function so both the `rmaild` binary and integration
//! tests drive the same code path.

mod account_service;
mod admin_service;
mod ai_policy_service;
mod ai_safety_service;
mod ai_service;
mod analytics_service;
mod attachment_service;
mod audit_service;
mod auth;
mod compose_service;
mod config_service;
mod export_service;
mod extract_service;
mod finder_service;
mod hook_service;
mod idempotency;
mod index_service;
mod mail_service;
pub mod mcp;
mod note_service;
mod notification_service;
mod rule_service;
mod saved_search_service;
mod search_service;
mod send_scheduler_service;
mod stream;
mod sync_service;
mod tag_service;
mod trace;
mod webhook_service;

pub use account_service::AccountApi;
pub use admin_service::AdminApi;
pub use ai_safety_service::AiSafetyApi;
pub use ai_service::AiApi;
pub use analytics_service::AnalyticsApi;
pub use attachment_service::AttachmentApi;
pub use audit_service::AuditApi;
pub use auth::AuthLayer;
/// The per-method capability requirement, re-exported because
/// [`mcp::Tool::requirement`] hands it back: an MCP client that is refused a
/// tool needs to be told which scope to mint, and re-deriving that from a
/// second table is the drift `rmail_core::parity` exists to prevent.
pub use auth::Requirement;
pub use compose_service::ComposeApi;
pub use config_service::ConfigApi;
pub use export_service::ExportApi;
pub use extract_service::{ExtractApi, LinkApi};
pub use finder_service::FinderApi;
pub use hook_service::HookApi;
pub use index_service::IndexApi;
pub use mail_service::MailApi;
pub use note_service::NoteApi;
pub use notification_service::NotificationApi;
pub use rule_service::RuleApi;
pub use saved_search_service::SavedSearchApi;
pub use search_service::{AskSearch, SearchApi};
pub use send_scheduler_service::SendSchedulerApi;
pub use sync_service::SyncApi;
pub use tag_service::TagApi;
pub use trace::RequestTraceLayer;
pub use webhook_service::WebhookApi;

use std::future::Future;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use rmail_core::ai::{
    self, AiDispatchLoop, AiPauseFlag, AiQueue, AiWorkerPool, AskRetriever, BatchClient,
    BatchCoordinator, DeepPassGate, DeepPassHandler, PassHandler, PolicyEngine,
    Provider as AiProvider, QueueOptions as AiQueueOptions, RateLimiter, TriagePassHandler,
};
use rmail_core::compose::reply::ReplyDrafter;
use rmail_core::compose::DraftStore;
use rmail_core::digest::{DigestEngine, DigestScheduler};
use rmail_core::embed::hash::HashEmbedder;
use rmail_core::embed::Embedder;
use rmail_core::events::{EventLog, Retention};
use rmail_core::extract::{ExtractEngine, ExtractModel};
use rmail_core::finder::index::FinderIndex;
use rmail_core::finder::store::FinderStore;
use rmail_core::finder::Finder;
use rmail_core::hooks::HookDispatcher;
use rmail_core::imap::mutate::LiveImapMutator;
use rmail_core::index::semantic::{SemanticIndex, VECTOR_DIM};
use rmail_core::index::{
    FtsIndex, IndexAdmin, IndexLoop, IndexPauseFlag, IndexPipeline, IndexQueue,
    QueueOptions as IndexQueueOptions,
};
use rmail_core::mail::MailStore;
use rmail_core::notes::NoteStore;
use rmail_core::notify::{NotifyEngine, NotifyPassHandler, NotifyPolicy};
use rmail_core::outbox::followup::track::FollowupTracker;
use rmail_core::outbox::{
    FollowupStore, ImapSentAppender, LettreSender, OutboxStore, SendPolicy, SendScheduler,
};
use rmail_core::query::QueryCompiler;
use rmail_core::rank::l1::Weights;
use rmail_core::rank::l2::{ClaudeReranker, L2Stage, Reranker as CoreReranker};
use rmail_core::rules::{
    ActionRunner, Classifier, ClaudeClassifier, RuleEngine, RuleEvaluator, RuleSynthesizer,
};
use rmail_core::saved_search::SavedSearchStore;
use rmail_core::send::preflight::PreflightGuardian;
use rmail_core::smart_folder::nl::NlSmartFolderCompiler;
use rmail_core::smart_folder::{SmartFolderEvaluator, SmartFolderStore};
use rmail_core::sync::{SyncEngine, SyncOptions};
use rmail_core::tags::ai::{SuggestTagsPassHandler, SuggestionEngine};
use rmail_core::tags::TagStore;
use rmail_core::webhooks::WebhookDispatcher;
use rmail_core::{Config, Database};
use rmail_proto::v1::account_service_server::AccountServiceServer;
use rmail_proto::v1::admin_service_server::AdminServiceServer;
use rmail_proto::v1::ai_policy_service_server::AiPolicyServiceServer;
use rmail_proto::v1::ai_safety_service_server::AiSafetyServiceServer;
use rmail_proto::v1::ai_service_server::AiServiceServer;
use rmail_proto::v1::analytics_service_server::AnalyticsServiceServer;
use rmail_proto::v1::attachment_service_server::AttachmentServiceServer;
use rmail_proto::v1::audit_service_server::AuditServiceServer;
use rmail_proto::v1::compose_service_server::ComposeServiceServer;
use rmail_proto::v1::config_service_server::ConfigServiceServer;
use rmail_proto::v1::export_service_server::ExportServiceServer;
use rmail_proto::v1::extract_service_server::ExtractServiceServer;
use rmail_proto::v1::finder_service_server::FinderServiceServer;
use rmail_proto::v1::hook_service_server::HookServiceServer;
use rmail_proto::v1::index_service_server::IndexServiceServer;
use rmail_proto::v1::link_service_server::LinkServiceServer;
use rmail_proto::v1::mail_service_server::MailServiceServer;
use rmail_proto::v1::note_service_server::NoteServiceServer;
use rmail_proto::v1::notification_service_server::NotificationServiceServer;
use rmail_proto::v1::rule_service_server::RuleServiceServer;
use rmail_proto::v1::saved_search_service_server::SavedSearchServiceServer;
use rmail_proto::v1::search_service_server::SearchServiceServer;
use rmail_proto::v1::send_scheduler_service_server::SendSchedulerServiceServer;
use rmail_proto::v1::sync_service_server::SyncServiceServer;
use rmail_proto::v1::tag_service_server::TagServiceServer;
use rmail_proto::v1::webhook_service_server::WebhookServiceServer;
use tokio::net::UnixListener;
use tokio::sync::Semaphore;
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

    /// The OAuth broker could not be built (its HTTP client would not
    /// construct). Fails at start-up for the same reason the two above do:
    /// an account configured for OAuth would otherwise fail on its first
    /// sync, long after anyone was watching.
    ///
    /// Not `#[from]`, for the reason [`ServeError::InvalidAiPolicy`] gives.
    #[error("could not build the OAuth broker: {0}")]
    OAuthBroker(#[source] rmail_core::Error),

    /// `[notify.quiet_hours]` is enabled but its times or timezone do not
    /// parse — caught here, before any socket is bound, for the same reason
    /// `InvalidAiPolicy` is. Note that an unrecognized *threshold* is
    /// deliberately not fatal (it resolves to "deliver nothing" and warns);
    /// only an unparseable quiet-hours window fails the boot, because a
    /// window that cannot be evaluated has no safe reading at all.
    #[error("invalid notify.quiet_hours: {0}")]
    InvalidNotify(#[source] rmail_core::Error),
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
/// `TagService` is wired here too, over a [`TagStore`] built the same way
/// `mail_store` is expected to have been (a real [`LiveImapMutator`]) —
/// tests that need `TagService`'s IMAP calls to go through a fake mutator
/// instead should call [`serve_uds_with_stores`] directly, the identical
/// seam this function already is for `MailStore`.
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
    let tag_store = TagStore::new(
        db.clone(),
        std::sync::Arc::new(LiveImapMutator::new(db.clone())),
        config.tags.clone(),
    );
    serve_uds_with_stores(
        socket_path,
        db,
        engine,
        mail_store,
        tag_store,
        config,
        shutdown,
    )
    .await
}

/// The network-facing dependencies a caller may supply instead of letting
/// this module build them.
///
/// Production has no reason to use this: [`serve_uds`] and every wrapper
/// below it build the real Anthropic client from `[ai]`. It exists because
/// there is otherwise **no hermetic path through the daemon's own boot
/// wiring** to a provider-calling RPC — `ClaudeProvider`'s endpoint is not
/// configurable at the [`Config`] level (see `rmail_core::ai::provider`'s own
/// docs), so a test either fakes the provider here or assembles a handler by
/// hand and tests something other than what ships.
///
/// Task 51 left that gap deliberately and named task 52 as the task that
/// would need it: `AskMailbox` is retrieval *and* a model call, so a test that
/// hand-built the handler would be testing neither the real search pipeline
/// nor the real wiring between them. This is the same "fake the one
/// network-facing dependency, wire everything else for real" seam
/// [`serve_uds_with_engine_and_mail_store`] already is for `ImapMutator`.
///
/// A supplied `ai_provider` also makes the AI subsystem *active* — the
/// dispatch loop spawns, the reranker is built, `GetUsage.enabled` is true —
/// because a daemon handed a working provider is, by construction, a daemon
/// whose AI features work.
#[derive(Clone, Default)]
pub struct Injected {
    /// Stands in for the client `ai::provider::build` would have made.
    pub ai_provider: Option<Arc<dyn AiProvider>>,
    /// Stands in for the Claude listwise reranker Stage 5 would have built.
    /// `None` with an injected provider still yields a real
    /// [`ClaudeReranker`] over that provider, which is usually what a test
    /// wants; supply one only to pin the *order* a rerank produces.
    pub reranker: Option<Arc<dyn CoreReranker>>,
    /// Where `AccountService.Autoconfigure`'s probes look (task 80).
    ///
    /// The second network-facing dependency with no `[config]` knob, and for
    /// the same reason as the first: the real endpoints are ISPDB, a domain's
    /// own autoconfig document and a DNS-over-HTTPS resolver, none of which a
    /// test may touch. `None` uses them. A test supplies endpoints pointing
    /// at a loopback server and gets the whole RPC — scope check, handler,
    /// probes, validator, TOML rendering — wired exactly as it ships.
    pub autoconfig_endpoints: Option<rmail_core::autoconfig::ProbeEndpoints>,
}

impl std::fmt::Debug for Injected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Injected")
            .field("ai_provider", &self.ai_provider.is_some())
            .field("reranker", &self.reranker.is_some())
            .field("autoconfig_endpoints", &self.autoconfig_endpoints.is_some())
            .finish()
    }
}

/// [`serve_uds_with_engine_and_mail_store`] over a caller-supplied
/// [`TagStore`] as well — for tests that need `TagService`'s IMAP calls to
/// go through a fake [`rmail_core::imap::mutate::ImapMutator`] rather than
/// [`LiveImapMutator`]'s real one, the identical reason
/// [`serve_uds_with_engine_and_mail_store`] exists for `MailStore`.
/// Production code has no reason to call this directly.
///
/// This is the function every other `serve_uds*` entry point in this module
/// ultimately delegates to; extracting it (rather than adding a `tag_store`
/// parameter to [`serve_uds_with_engine_and_mail_store`] directly) is what
/// keeps every existing caller's signature — and therefore every sibling
/// task's own test harness already calling it — unchanged by task 55's
/// addition.
///
/// # Errors
///
/// As [`serve_uds`], plus [`ServeError::InvalidRankWeights`] under the same
/// condition [`serve_uds_with_engine`] documents.
pub async fn serve_uds_with_stores<F>(
    socket_path: impl AsRef<Path>,
    db: Database,
    engine: SyncEngine,
    mail_store: MailStore,
    tag_store: TagStore,
    config: &Config,
    shutdown: F,
) -> Result<(), ServeError>
where
    F: Future<Output = ()> + Send + 'static,
{
    serve_uds_injected(
        socket_path,
        db,
        engine,
        mail_store,
        tag_store,
        config,
        Injected::default(),
        shutdown,
    )
    .await
}

/// [`serve_uds_with_stores`] with the network-facing dependencies supplied by
/// the caller — see [`Injected`] for what that is for and why production never
/// calls this.
///
/// This is the function every other `serve_uds*` entry point ultimately
/// delegates to. It takes [`Injected`] as one parameter rather than two so a
/// later seam (an embedder, an SMTP sender) is a field here instead of a
/// fourth `serve_uds_with_…` layer and another round of harness churn across
/// every sibling test.
///
/// # Errors
///
/// As [`serve_uds_with_stores`].
#[allow(clippy::too_many_arguments)]
pub async fn serve_uds_injected<F>(
    socket_path: impl AsRef<Path>,
    db: Database,
    engine: SyncEngine,
    mail_store: MailStore,
    tag_store: TagStore,
    config: &Config,
    injected: Injected,
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
        let db = db.clone();
        async move {
            loop {
                // Prune once at startup before sleeping. A daemon restarted
                // more often than the interval would otherwise never prune at
                // all, which is exactly the machine that most needs it.
                if let Err(error) = events.prune().await {
                    tracing::warn!(%error, "event log prune failed");
                }
                // The move-annotation escrow's safety net. Rows here are
                // normally consumed within seconds by the destination folder's
                // next sync; these are the ones whose message never arrived,
                // and nothing but age will ever clear them. See
                // `rmail_core::mail::annotations`.
                match db
                    .write(|conn| rmail_core::mail::annotations::expire(conn))
                    .await
                {
                    Ok(0) => {}
                    Ok(expired) => tracing::info!(
                        expired,
                        "reaped move-annotation escrow rows whose message never arrived"
                    ),
                    Err(error) => tracing::warn!(%error, "move-annotation escrow prune failed"),
                }
                tokio::select! {
                    () = stopping.cancelled() => return,
                    () = tokio::time::sleep(PRUNE_INTERVAL) => {}
                }
            }
        }
    });

    // One replay fence, shared by every mutating RPC that takes an
    // `idempotency_key`: keys are a single namespace (see
    // `rmail_core::idempotency`), so a second store would be a second
    // namespace and a key could then be reused across it.
    let idempotency = rmail_core::idempotency::IdempotencyStore::new(
        db.clone(),
        config.grpc.idempotency.retention.into(),
        config.grpc.idempotency.in_flight.into(),
    );
    let admin_service = AdminServiceServer::new(AdminApi::new(db.clone()));
    // Registered further down, once the AI provider and policy engine exist:
    // `Autoconfigure`'s model fallback needs both, and `AccountApi` is built
    // here because everything else on that service needs neither.
    let account_api =
        AccountApi::new(db.clone(), stopping.clone()).map_err(ServeError::OAuthBroker)?;
    let audit_service = AuditServiceServer::new(AuditApi::new(db.clone(), stopping.clone()));
    // Built further down, once the AI provider and policy engine exist:
    // `AnalyticsService` is registered with the rest of the services below,
    // and `GenerateDigest`'s engine is attached to it there.
    let analytics_api = AnalyticsApi::new(db.clone(), stopping.clone());
    let sync_service = SyncServiceServer::new(SyncApi::new(engine, stopping.clone()));
    // `ExportService` needs nothing but the database and the shutdown token:
    // an export is a local read that never touches IMAP or a model provider
    // (see `rmail_core::export`'s module docs on why `--with-ai` attaches
    // stored artifacts rather than producing new ones).
    let export_service = ExportServiceServer::new(ExportApi::new(db.clone(), stopping.clone()));
    // Cloned before the store moves into its own service: the rules engine's
    // action runner mutates mail and tags through the *same* stores the
    // services do, so a rule-applied flag honours the same IMAP reflection and
    // a rule-applied tag honours the same per-tag sync mode.
    let rules_mail_store = mail_store.clone();
    let rules_tag_store = tag_store.clone();
    // The finder's `BatchAction` runs through the *same* store, so archiving
    // from a picker and archiving from `mail archive` are one operation with
    // one IMAP reconciliation and one event.
    let finder_mail_store = mail_store.clone();
    let mail_service = MailServiceServer::new(MailApi::new(
        mail_store,
        idempotency.clone(),
        stopping.clone(),
    ));
    // `TagService` is built further down, after the AI provider/policy/limits
    // handles exist: `SuggestTags` classifies on demand (task 57) and must
    // draw on the *same* semaphore and rate limiter every other AI call site
    // in this process shares.
    //
    // `ComposeService`'s draft CRUD needs nothing but the database: drafts are
    // local, and it deliberately stops short of SMTP (task 61 owns
    // submission), so there is no client, pool, or background loop here.
    //
    // Task 62's `DraftReply`/`RewriteDraft` do need the provider, so the
    // handler is *built* here and the drafter is attached further down, where
    // the provider exists — the same two-step `SendSchedulerApi` uses. The
    // revision RPCs are on the always-available half on purpose: a user who
    // turns AI off after a rewrite must still be able to revert it.
    let compose_api = ComposeApi::new(
        DraftStore::new(db.clone()),
        idempotency.clone(),
        db.clone(),
        stopping.clone(),
    );
    // The keymap the TUI reads directly; served for palette/MCP clients that
    // have no other way to discover the action-id registry. See
    // `config_service`'s own docs for why the daemon serves a client-side file.
    let config_service = ConfigServiceServer::new(ConfigApi::new(
        rmail_core::keymap::file::keys_path_from_env(),
    ));

    // Scheduled send (task 61). `SendScheduler` is always registered — the
    // reflection set and the auth scope table must see every RPC regardless of
    // runtime config, the convention `AiService`/`HookService` established —
    // and the *loop* is what config gates. The loop is always spawned, though,
    // because unlike the AI dispatcher it has durable work waiting for it: an
    // outbox row scheduled by a previous run is mail the user has already
    // pressed send on, and a daemon that declines to drain it is a daemon that
    // silently swallows outgoing mail.
    //
    // The *handler* is assembled further down, after the AI provider exists:
    // task 63's pre-send guardian and waiting-on tracker both call a model,
    // and both must draw on the one provider and the one `ai.limits` budget
    // this process has rather than minting a second of either. The stores and
    // the scheduler loop are built here, where they always were, because
    // neither needs a provider and the loop must not wait on one.
    let outbox_store = OutboxStore::new(db.clone());
    let followup_store = FollowupStore::new(db.clone());
    let send_api_stores = (outbox_store.clone(), followup_store.clone());
    let send_handle = SendScheduler::new(
        outbox_store,
        followup_store,
        Arc::new(LettreSender::new(db.clone(), config.send.smtp_security)),
        events.clone(),
        SendPolicy::from_config(&config.send),
        // Stable across restarts on purpose: the worker name is the fence a
        // completion is checked against, and a per-boot random one would make
        // every restart look like a different worker to a lease that is still
        // live.
        "rmaild-send",
    )
    // A fresh IMAP connection per append, the same shape `LiveImapMutator`
    // uses. Filing is best effort — see `rmail_core::outbox::sent`.
    .with_sent_appender(Arc::new(ImapSentAppender::new(db.clone())))
    .spawn(stopping.clone());

    // A dedicated `IndexQueue` handle rather than reusing the AI subsystem's
    // (below) — `IndexQueue` is a cheap, stateless wrapper over `db` (see its
    // own docs), so a second instance costs nothing and keeps this task's
    // wiring independent of the AI subsystem's.
    let note_store = NoteStore::new(
        db.clone(),
        IndexQueue::new(db.clone(), IndexQueueOptions::default()),
        config.notes.index,
    );
    let note_service = NoteServiceServer::new(NoteApi::new(
        note_store,
        stopping.clone(),
        idempotency.clone(),
    ));
    // `HookService` is always registered (reflection and the auth scope
    // table must see every RPC regardless of runtime config, the same
    // convention `AiService`/`ai_active` established below); `hooks.enabled`
    // gates only whether the background dispatcher consumer actually runs —
    // `TestHook` stays available either way, since it is an operator-invoked
    // dry run, not "did the automatic dispatcher fire" (see
    // `rmail_core::config::HooksConfig::enabled`'s own docs). The
    // dispatcher is therefore always *built* (so its semaphore exists for
    // `HookApi` to share — see `hook_service`'s own module docs) but only
    // conditionally *spawned*, and only when it actually drives at least
    // one enabled hook: an empty or fully-disabled hook list would
    // otherwise still pay a retention-window paging scan at boot and a
    // query every tick for nothing to match against.
    let hook_dispatcher = HookDispatcher::new(events.clone(), &config.hooks);
    // Shared with the rules engine's `run_hook` action for the same reason
    // `HookApi` shares it: three independent budgets would sum to three times
    // the ceiling `hooks.max_concurrency` configures.
    let hook_semaphore = hook_dispatcher.semaphore();
    let hook_service = HookServiceServer::new(HookApi::new(
        &config.hooks,
        stopping.clone(),
        hook_dispatcher.semaphore(),
    ));
    let hook_dispatch_handle = if config.hooks.enabled && hook_dispatcher.hook_count() > 0 {
        Some(hook_dispatcher.spawn(stopping.clone()).await)
    } else {
        tracing::info!(
            enabled = config.hooks.enabled,
            hooks = hook_dispatcher.hook_count(),
            "the hook dispatch loop is not running on this daemon"
        );
        None
    };

    // `WebhookService` is always registered, for the reason `HookService`
    // above gives — reflection and the auth scope table must see every RPC
    // regardless of runtime config — and here it matters more than usual:
    // `webhooks.enabled` defaults to *false*, so on a stock daemon this
    // service is the only way an operator can register, list and remove
    // destinations, and a service that vanished with the dispatcher would
    // make the feature unconfigurable until somebody first enabled it blind.
    // What `enabled` gates is the sending.
    let webhook_service = WebhookServiceServer::new(WebhookApi::new(
        db.clone(),
        config.ai.privacy.clone(),
        config.webhooks.enabled,
    ));
    let webhook_dispatch_handle = if config.webhooks.enabled {
        match WebhookDispatcher::new(
            db.clone(),
            events.clone(),
            &config.webhooks,
            config.ai.privacy.clone(),
        ) {
            Ok(dispatcher) => Some(dispatcher.spawn(stopping.clone()).await),
            Err(error) => {
                // A client that cannot be built is a TLS/transport problem,
                // not a reason to refuse to boot: every other surface still
                // works, and the queue simply does not drain until it is
                // fixed. Loud, and not fatal.
                tracing::error!(%error, "the outbound webhook dispatcher could not start");
                None
            }
        }
    } else {
        tracing::info!(
            "the outbound webhook dispatch loop is not running on this daemon \
             (webhooks.enabled is false); destinations can still be registered and listed"
        );
        None
    };

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

    // The AI provider and policy engine, built here rather than with the rest
    // of the AI subsystem below because search's L2 rerank stage (task 51)
    // needs the provider and is constructed first. One provider per process
    // is the point — a second one would mean a second API-key resolution and
    // a second HTTP client, and would make `ai.limits` accounting a fiction.
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
    let (ai_provider, ai_active): (Arc<dyn AiProvider>, bool) =
        if let Some(provider) = injected.ai_provider.clone() {
            // A caller-supplied provider bypasses `ai.enabled` on purpose: the
            // switch exists to stop the daemon dialling out, and a caller that
            // handed in its own client has already decided what "dialling out"
            // means here. See `Injected`'s own docs.
            tracing::info!("using a caller-supplied AI provider; ai.enabled is not consulted");
            (provider, true)
        } else if config.ai.enabled {
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

    // The one `ai.limits` concurrency/pacing budget for this process. Created
    // here rather than inside `AiWorkerPool` (which is built further down)
    // because search's L2 rerank needs it *first* and must draw on the same
    // budget the queue does — two independent pairs would let the daemon
    // exceed `max_concurrency`/`requests_per_minute` in practice. The pool
    // adopts this pair via `with_capacity` below; `AiApi` takes it from the
    // pool's own accessors, as it always has.
    let ai_semaphore = Arc::new(Semaphore::new(
        config.ai.limits.max_concurrency.max(1) as usize
    ));
    let ai_rate_limiter = Arc::new(RateLimiter::new(config.ai.limits.requests_per_minute));

    // Stage 5. The Claude backend is wired only when the AI subsystem is
    // genuinely active: with `NullProvider` behind it, `search.rerank =
    // "claude"` would spend a redaction pass and a budget check per query to
    // reach a provider that always refuses. `None` degrades to the L1 order
    // one step earlier and says so once, in the log, rather than per search.
    // The local cross-encoder is always built — it is offline, and
    // `L2Stage::new` loads no model until a search actually asks for one.
    //
    // `ai_policy` is threaded in because a rerank reads message *text*: it is
    // the only stage of search that does, so it is the only one that has to
    // honor `accounts.ai.enabled` and `ai.policy`'s per-folder rules.
    let claude_reranker: Option<Arc<dyn CoreReranker>> = injected.reranker.clone().or_else(|| {
        ai_active.then(|| {
            Arc::new(ClaudeReranker::new(
                Arc::clone(&ai_provider),
                db.clone(),
                &config.search.reranker,
                config.ai.limits.clone(),
                config.ai.privacy.clone(),
                Arc::clone(&ai_semaphore),
                Arc::clone(&ai_rate_limiter),
            )) as Arc<dyn CoreReranker>
        })
    });
    let search_api = SearchApi::new(
        db.clone(),
        Arc::clone(&embedder),
        rank_weights,
        config.search.clone(),
        &config.index.semantic,
        L2Stage::new(
            db.clone(),
            &config.search,
            Arc::clone(&ai_policy),
            claude_reranker,
        ),
        stopping.clone(),
    );
    // Stage 0's NL->plan compiler (task 58). Wired only when the AI subsystem
    // is genuinely active, for the reason the Claude reranker above is: with
    // `NullProvider` behind it, `CompileQuery` would run a redaction pass and
    // a budget check to reach a provider that always refuses, and
    // `UNIMPLEMENTED` is the honest answer to "compile this sentence" on a
    // daemon that has nothing to compile it with.
    //
    // `ai.models.deep` rather than `triage`: turning a sentence into a query
    // is the one-off reasoning job the first names, and it is what
    // `RuleService/SynthesizeRule` — the same shape of work — already uses.
    // It shares `ai_semaphore`/`ai_rate_limiter` with the worker pool so the
    // one `ai.limits` budget bounds this path too.
    let query_compiler = ai_active.then(|| {
        QueryCompiler::new(
            db.clone(),
            Arc::clone(&ai_provider),
            Arc::clone(&ai_policy),
            config.ai.privacy.clone(),
            config.ai.limits.clone(),
            config.ai.models.deep.clone(),
            Arc::clone(&ai_semaphore),
            Arc::clone(&ai_rate_limiter),
        )
    });
    let search_api = match query_compiler.clone() {
        Some(compiler) => search_api.with_query_compiler(compiler),
        None => search_api,
    };
    let search_service = SearchServiceServer::new(search_api.clone());

    // The implicit-feedback log's retention sweep (task 64). Separate from
    // the event-log pruner above only because `SearchApi` — which owns the
    // one `FeedbackStore` configured from `search.learning` and
    // `[search.feedback]` — does not exist yet at that point; cloning its
    // store rather than building a second one is what keeps "the policy the
    // search path writes under" and "the policy retention enforces" the same
    // object.
    //
    // Runs regardless of `search.learning`, and prunes once before its first
    // sleep for the reason the event pruner does: a daemon restarted more
    // often than the interval would otherwise never prune at all, which is
    // exactly the machine that most needs it. Turning learning off should
    // also *retire* what was already collected rather than freezing it on
    // disk forever, which only happens if this loop keeps running.
    let feedback_pruner = tokio::spawn({
        let feedback = search_api.feedback().clone();
        let stopping = stopping.clone();
        async move {
            loop {
                if let Err(error) = feedback.prune().await {
                    tracing::warn!(%error, "search feedback prune failed");
                }
                tokio::select! {
                    () = stopping.cancelled() => return,
                    () = tokio::time::sleep(PRUNE_INTERVAL) => {}
                }
            }
        }
    });

    // The indexing subsystem (task 24): the pipeline that runs the stages, the
    // loop that keeps it fed, and the operator surface over both.
    //
    // `IndexService` is always registered — reflection and the scope table must
    // see every RPC regardless of runtime config, the same convention
    // `AiService`/`HookService` follow.
    //
    // `index.enabled = false` starts the background worker *paused* rather than
    // not spawning it. One mechanism instead of two: `mail index status` then
    // reports "stopped" truthfully, and `mail index start` genuinely starts it
    // — where a not-spawned loop would have made `status` claim the worker was
    // running and `start` silently do nothing. A paused tick costs one atomic
    // load every couple of seconds.
    //
    // The `FtsIndex`/`SemanticIndex` here are built the same way `SearchApi`
    // builds its own, over the *same* embedder `Arc`: one model per process,
    // and one definition of what "indexed" means for the thing that writes the
    // index and the thing that reads it.
    let indexer_queue = IndexQueue::new(db.clone(), IndexQueueOptions::default());
    let indexer_semantic =
        SemanticIndex::new(db.clone(), Arc::clone(&embedder), &config.index.semantic);
    let index_pipeline = IndexPipeline::new(
        db.clone(),
        indexer_queue.clone(),
        FtsIndex::new(db.clone(), config.search.bm25_weights.clone()),
        indexer_semantic.clone(),
        &config.index,
    )
    .with_pause_flag(IndexPauseFlag::new(!config.index.enabled));
    if !config.index.enabled {
        tracing::info!(
            "index.enabled = false; the background indexer starts stopped (`mail index start` \
             turns it on, `mail index run` drains on demand regardless)"
        );
    }
    let index_admin = IndexAdmin::new(
        db.clone(),
        indexer_queue,
        indexer_semantic,
        &config.index,
        index_pipeline.pause_flag(),
    );
    let index_service = IndexServiceServer::new(IndexApi::new(
        index_admin,
        index_pipeline.clone(),
        stopping.clone(),
        config.search.cache,
    ));
    // `index.workers` sizes the batch, not a thread pool, and the distinction
    // is worth stating: every stage writes through SQLite's single writer
    // connection, so four jobs at once would serialize on it anyway. What the
    // knob genuinely buys is how much work one pass takes on between polls —
    // which, since a saturated batch skips the tick interval entirely (see
    // `IndexLoop::spawn`), is the amount of queue read per lease round trip.
    let index_handle = IndexLoop::new(events.clone(), index_pipeline)
        .with_lease_limit(
            i64::from(config.index.workers.max(1))
                .saturating_mul(rmail_core::index::pipeline::DEFAULT_LEASE_LIMIT),
        )
        .spawn(stopping.clone());

    // Saved searches + deterministic smart folders (task 35). `SavedSearchApi`
    // holds a clone of the *same* `SearchApi` the `SearchService` above
    // serves, so `RunSavedSearch` re-runs a saved query through the one
    // pipeline in this process rather than a second one of its own.
    //
    // The smart folder store applies its `auto_tag` action through the same
    // `TagStore` `TagService` uses (so a rule-applied tag honours the tag's
    // own IMAP sync mode) and publishes `notify` actions to the same
    // `EventLog` every other subsystem consumes.
    let smart_folder_store = SmartFolderStore::new(db.clone(), tag_store.clone(), events.clone());
    //
    // The NL half (task 58) compiles through the *same* `QueryCompiler`
    // `SearchService.CompileQuery` serves from, so a plan confirmed with
    // `mail search --nl` and a folder defined from the same sentence share one
    // cache entry and one charge. It embeds with the same `Arc<dyn Embedder>`
    // the index and the search pipeline use — a second embedder here would
    // freeze a query vector from one model into a folder searching an index
    // built by another, which produces cosines that mean nothing and still
    // sort.
    let saved_search_api = SavedSearchApi::new(
        db.clone(),
        SavedSearchStore::new(db.clone()),
        smart_folder_store.clone(),
        search_api.clone(),
        stopping.clone(),
    );
    let saved_search_api = match query_compiler {
        Some(compiler) => saved_search_api.with_nl_compiler(NlSmartFolderCompiler::new(
            compiler,
            Arc::clone(&embedder),
            smart_folder_store.clone(),
        )),
        None => saved_search_api,
    };
    let saved_search_service = SavedSearchServiceServer::new(saved_search_api);
    // Membership is always live on read, so this loop exists only to keep
    // *actions* following sync — see `rmail_core::smart_folder`'s docs.
    let smart_folder_handle = SmartFolderEvaluator::new(smart_folder_store, events.clone())
        .spawn(stopping.clone())
        .await;

    // The fuzzy finder (task 59). One `FinderStore` per daemon, shared by the
    // drain loop that maintains it and the handler that queries it — see
    // `rmail_core::finder`'s module docs on why the query side deliberately
    // holds no `Database` at all.
    let finder_store = Arc::new(std::sync::RwLock::new(FinderStore::new()));
    let finder_index = FinderIndex::new(db.clone(), Arc::clone(&finder_store), &config.finder);
    let finder_service = FinderServiceServer::new(FinderApi::new(
        Finder::new(finder_store, &config.finder),
        finder_index.clone(),
        finder_mail_store,
        db.clone(),
        &config.finder,
        config.rules.archive_mailbox.clone(),
        stopping.clone(),
    ));
    // `finder.enabled = false` stops the index being maintained; the service
    // still answers, over whatever the store holds (nothing, on a daemon that
    // has never had the loop running). One mechanism rather than two: an
    // absent RPC would make a disabled finder indistinguishable from an old
    // daemon, where a served-but-empty one says plainly what is going on
    // through `IndexStatus`.
    let finder_handle = config
        .finder
        .enabled
        .then(|| finder_index.spawn(stopping.clone()));
    if !config.finder.enabled {
        tracing::info!("finder.enabled = false; the finder index is not maintained");
    }

    // AI subsystem: the durable queue, the two pass handlers, the worker
    // pool/batch coordinator that drive them, and the dispatch loop that
    // closes the loop between "a message synced" and "a triage job ran" —
    // see `rmail_core::ai::dispatch`'s own module docs for why task 50 owns
    // that wiring and what gap it closes. The provider and the policy engine
    // are built further up, before `SearchApi`, because the L2 rerank stage
    // needs them.
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
            .with_injection_config(config.ai.injection.clone())
            .with_deep_pass_gate(DeepPassGate::new(config.ai.deep_pass.clone())),
    );
    // Registered unconditionally, like every other handler: `notify.enabled`
    // gates whether a `NewMail` event *enqueues* a scoring job (see
    // `AiDispatchLoop::with_notify_pass`), not whether the pool can run one
    // that already exists. A job enqueued before the operator turned the
    // feature off must still be able to complete rather than sit in the queue
    // forever.
    //
    // The policy and the age bound are the two gates that keep this pass from
    // spending money it should not: the first declines accounts whose
    // notifications are off, the second declines mail that arrived too long
    // ago to be worth interrupting anyone about — which is what makes the
    // first boot after `notify.enabled = true` safe, since the AI dispatch
    // loop replays a whole retention window of `NewMail` events on every
    // restart. Both are checked before any request is built.
    let notify_policy = NotifyPolicy::from_config(&config.notify, &config.accounts);
    let notify_handler: Arc<dyn PassHandler> = Arc::new(
        NotifyPassHandler::new(db.clone(), config.ai.models.notify.clone())
            .with_injection_config(config.ai.injection.clone())
            .with_policy(notify_policy.clone())
            .with_max_message_age(config.notify.max_message_age.as_duration()),
    );
    // Auto-tagging (task 57). Registered unconditionally for the same reason
    // the notification pass is: `tags.ai.enabled`/`suggest_on_new_mail` gate
    // whether a `NewMail` event *enqueues* one of these jobs (see
    // `AiDispatchLoop::with_suggest_tags_pass`), not whether the pool can
    // finish one enqueued before the operator switched the feature off. The
    // handler declines the same config in its own `build_request` anyway, so
    // such a job terminates without ever reaching a provider.
    let suggest_tags_handler: Arc<dyn PassHandler> = Arc::new(
        SuggestTagsPassHandler::new(db.clone(), tag_store.clone(), config.tags.ai.clone())
            .with_injection_config(config.ai.injection.clone()),
    );
    let ai_handlers: Vec<Arc<dyn PassHandler>> = vec![
        triage_handler,
        Arc::clone(&deep_handler) as Arc<dyn PassHandler>,
        notify_handler,
        suggest_tags_handler,
    ];

    // `TagService`, deferred from further up so `SuggestTags`' on-demand
    // classifier can share this process's one `ai.limits` budget rather than
    // minting a second pair (see `ai_semaphore`'s own comment). The engine is
    // wired only when the AI subsystem is genuinely active: behind
    // `NullProvider` every call would spend a policy resolution, a budget
    // check and a redaction pass to reach a provider that always refuses, so a
    // disabled daemon keeps `SuggestTags` as the pending-only replay instead —
    // the background pass's work stays readable either way.
    let tag_api = TagApi::new(tag_store.clone());
    let tag_api = if ai_active {
        tag_api.with_suggestions(
            SuggestionEngine::new(
                db.clone(),
                tag_store.clone(),
                Arc::clone(&ai_provider),
                Arc::clone(&ai_policy),
                config.ai.limits.clone(),
                config.ai.privacy.clone(),
                config.ai.injection.clone(),
                config.tags.ai.clone(),
                Arc::clone(&ai_semaphore),
                Arc::clone(&ai_rate_limiter),
            ),
            stopping.clone(),
        )
    } else {
        tag_api
    };
    let tag_service = TagServiceServer::new(tag_api);

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
    )
    // The pair built above, shared rather than a second one of this pool's
    // own — see its own comment and `AiWorkerPool::with_capacity`'s docs.
    .with_capacity(Arc::clone(&ai_semaphore), Arc::clone(&ai_rate_limiter));
    // The rules engine draws from the same two, for the identical reason —
    // see `rmail_core::rules::gate`. Cloned from the pair built above rather
    // than read back off the pool: task 51 moved their construction ahead of
    // `SearchApi` so the reranker could share them too, so the pool is no
    // longer where they originate.
    let rules_ai_semaphore = Arc::clone(&ai_semaphore);
    let rules_ai_rate_limiter = Arc::clone(&ai_rate_limiter);
    // The digest engine draws from the same pair, for the identical reason.
    // Cloned here rather than at its construction site further down because
    // `ai_semaphore`/`ai_rate_limiter` are moved into `AiApi::new` before it.
    let digest_ai_semaphore = Arc::clone(&ai_semaphore);
    let digest_ai_rate_limiter = Arc::clone(&ai_rate_limiter);

    // Always starts unpaused, regardless of `ai_active` — a disabled daemon
    // is reported via `GetUsage.enabled = false`, not by pretending it is
    // "paused" (which would misleadingly imply `mail ai resume` could make
    // it start running).
    let ai_pause = AiPauseFlag::new(false);

    // Ask-your-attachment (task 74). Built before `AiService` only because
    // `search_api` is moved into the mailbox-RAG retriever below; it takes a
    // clone of the *same* `AttachmentSearch` `SearchService.SearchAttachments`
    // answers from, so a question and an attachment search rank the same
    // documents the same way, and the *same* `ai.limits` semaphore and rate
    // limiter every other AI call site in this process shares.
    //
    // Registered unconditionally, like every other service: `AskAttachment`
    // declines with FAILED_PRECONDITION on a daemon whose AI subsystem is off
    // rather than disappearing from the reflection set and the fail-closed
    // scope table.
    // Structured extraction (task 75). The model half is `Some` only when the
    // AI subsystem is actually active, and it shares the *same* semaphore and
    // rate limiter as every other call site in this process — minting fresh
    // ones would double the operator's configured `ai.limits` ceiling, which
    // `rmail_core::ai::gate` documents at length.
    //
    // The engine itself is built either way, because its native routes need no
    // provider at all: a workbook's tables, an `.ics`'s events and a message's
    // links are all available on a daemon with `ai.enabled = false`.
    let extract_model = ai_active.then(|| {
        Arc::new(ExtractModel::new(
            db.clone(),
            Arc::clone(&ai_provider),
            Arc::clone(&ai_policy),
            config.ai.privacy.clone(),
            config.ai.limits.clone(),
            config.extract.model.clone(),
            Arc::clone(&ai_semaphore),
            Arc::clone(&ai_rate_limiter),
        ))
    });
    let extract_engine = ExtractEngine::new(db.clone(), extract_model, config.extract.clone());

    let attachment_service = AttachmentServiceServer::new(
        AttachmentApi::new(
            db.clone(),
            Arc::clone(&ai_provider),
            Arc::clone(&ai_policy),
            search_api.attachments().clone(),
            config.ai.privacy.clone(),
            config.ai.limits.clone(),
            config.ai.ask.clone(),
            Arc::clone(&ai_semaphore),
            Arc::clone(&ai_rate_limiter),
            ai_active,
            stopping.clone(),
        )
        .with_extract(extract_engine.clone()),
    );

    let extract_service =
        ExtractServiceServer::new(ExtractApi::new(extract_engine.clone(), stopping.clone()));
    let link_service = LinkServiceServer::new(LinkApi::new(extract_engine, stopping.clone()));

    let ai_service = AiServiceServer::new(
        AiApi::new(
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
            Arc::clone(&ai_semaphore),
            Arc::clone(&ai_rate_limiter),
            stopping.clone(),
        )
        // Mailbox RAG (task 52), over the *same* `SearchApi` `SearchService` and
        // `SavedSearchService` serve — one pipeline, so a question and a search
        // box rank the same corpus the same way. Wired unconditionally, like the
        // service registration itself: `AskMailbox` declines on a daemon whose AI
        // subsystem is off (see `AiApi::ask_mailbox`) rather than disappearing
        // from the reflection set and the scope table.
        .with_ask(
            Arc::new(AskSearch::new(search_api)) as Arc<dyn AskRetriever>,
            config.ai.ask.clone(),
        ),
    );

    // The budget control plane. Built unconditionally, unlike `ai_service`'s
    // dispatch loop: an operator must be able to set and inspect a budget on
    // a daemon whose AI subsystem is off (that is exactly when a spend cap
    // gets tightened), and neither RPC touches a provider.
    let ai_policy_service = AiPolicyServiceServer::new(ai_policy_service::AiPolicyApi::new(
        db.clone(),
        config.ai.limits.clone(),
    ));

    // The prompt-injection shield's read-and-confirm surface. Also
    // unconditional, and for a sharper reason than the budget plane's: a
    // daemon whose AI subsystem is off can still be holding withheld rule
    // actions from when it was on, and the RPC that releases them must not
    // become unreachable at exactly the moment an operator turns AI off to
    // deal with the problem. Neither RPC touches a provider.
    let ai_safety_service = AiSafetyServiceServer::new(ai_safety_service::AiSafetyApi::new(
        db.clone(),
        config.ai.privacy.clone(),
        config.ai.injection.clone(),
    ));

    // Only spawned when the subsystem is actually active — a disabled/
    // misconfigured daemon must not poll the event log and lease an empty
    // queue forever for nothing, and must never try to build a batch client
    // pointed at a provider that was just declared unusable.
    let ai_dispatch_handle = if ai_active {
        let mut dispatch = AiDispatchLoop::new(events.clone(), ai_queue.clone(), ai_worker_pool)
            .with_pause_flag(ai_pause)
            .with_notify_pass(config.notify.enabled)
            .with_suggest_tags_pass(config.tags.ai.enabled && config.tags.ai.suggest_on_new_mail);
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

    // The `SendScheduler` handler, assembled here rather than next to its
    // stores because task 63's two model-backed additions need the provider.
    //
    // Both are wired only when the AI subsystem is genuinely active: with
    // `NullProvider` behind them, every `PreflightCheck` would spend a policy
    // resolution and a redaction pass to reach a provider that always
    // refuses, and the automatic check on `ScheduleSend` would log a
    // degradation on every single message this daemon sends. `None` says the
    // same thing once, structurally — see `SendSchedulerApi::guardian`.
    //
    // Both share `ai_semaphore`/`ai_rate_limiter` with the worker pool and
    // the rules engine, for the reason `rmail_core::ai::gate` documents: a
    // second pair would let the paths together exceed `ai.limits`.
    //
    // `send.preflight.warn_if_unrecognized` runs here, once, at the only
    // point in the process where a misconfigured `block_at` can still be
    // reported before it silently stops refusing anything.
    config.send.preflight.warn_if_unrecognized();
    let (outbox_store_for_api, followup_store_for_api) = send_api_stores;
    let mut send_scheduler_api = SendSchedulerApi::new(
        outbox_store_for_api,
        followup_store_for_api.clone(),
        db.clone(),
        config.send.clone(),
        idempotency.clone(),
        stopping.clone(),
    );
    if ai_active {
        send_scheduler_api = send_scheduler_api
            .with_guardian(PreflightGuardian::new(
                db.clone(),
                Arc::clone(&ai_provider),
                Arc::clone(&ai_policy),
                config.ai.privacy.clone(),
                config.ai.limits.clone(),
                config.send.preflight.clone(),
                Arc::clone(&ai_semaphore),
                Arc::clone(&ai_rate_limiter),
            ))
            .with_tracker(FollowupTracker::new(
                db.clone(),
                followup_store_for_api,
                Arc::clone(&ai_provider),
                Arc::clone(&ai_policy),
                config.ai.privacy.clone(),
                config.ai.limits.clone(),
                config.send.followup.clone(),
                Arc::clone(&ai_semaphore),
                Arc::clone(&ai_rate_limiter),
            ));
    } else {
        tracing::info!(
            "the AI subsystem is inactive; the pre-send guardian does not run and \
             PreflightCheck/TrackFollowup/DraftNudge answer FAILED_PRECONDITION"
        );
    }
    let send_scheduler_service = SendSchedulerServiceServer::new(send_scheduler_api);

    // AI reply drafting (task 62), attached to the already-built
    // `ComposeService` handler now that the provider exists. Wired only when
    // the AI subsystem is genuinely active, for the reason the guardian above
    // is: with `NullProvider` behind it every `DraftReply` would spend a
    // policy resolution, a thread read and a redaction pass to reach a
    // provider that always refuses. `None` says so once, structurally — see
    // `ComposeApi::drafter`.
    //
    // Shares `ai_semaphore`/`ai_rate_limiter` with the worker pool and every
    // other call site, for the reason `rmail_core::ai::gate` documents: a
    // second pair would let the paths together exceed `ai.limits`.
    let compose_api = if ai_active {
        compose_api.with_drafter(ReplyDrafter::new(
            db.clone(),
            Arc::clone(&ai_provider),
            Arc::clone(&ai_policy),
            config.ai.privacy.clone(),
            config.ai.limits.clone(),
            config.send.reply.clone(),
            Arc::clone(&ai_semaphore),
            Arc::clone(&ai_rate_limiter),
        ))
    } else {
        tracing::info!(
            "the AI subsystem is inactive; DraftReply/RewriteDraft answer FAILED_PRECONDITION \
             (draft CRUD and revision cycling are unaffected)"
        );
        compose_api
    };
    let compose_service = ComposeServiceServer::new(compose_api);

    // The rules engine (task 66). Registered unconditionally — the reflection
    // set and the auth scope table must see every RPC regardless of runtime
    // config — and `rules.enabled` gates only the background evaluator, so
    // creating, listing, evaluating and backtesting deterministic rules keeps
    // working on a daemon whose automatic path is off.
    //
    // The classifier shares `ai_semaphore`/`ai_rate_limiter` with the AI
    // worker pool and `AiApi`: a rules engine evaluating every new message is
    // exactly the workload `ai.limits` exists to bound, and a second
    // independent budget would let the two paths together exceed it (see
    // `rmail_core::rules::gate`). It also shares the hook dispatcher's
    // semaphore for the same reason, so a `run_hook` action and a real event
    // dispatch cannot together exceed `hooks.max_concurrency`.
    //
    // `ai.models.triage` classifies and `ai.models.deep` synthesizes rather
    // than a `[rules]` model knob — a `claude_is` verdict is precisely the
    // cheap, high-volume work the first names, and writing a rule from a
    // sentence is the one-off reasoning job the second names.
    let rule_engine = RuleEngine::new(
        db.clone(),
        config.rules.rule_limits(),
        Arc::new(ClaudeClassifier::new(
            db.clone(),
            Arc::clone(&ai_provider),
            Arc::clone(&ai_policy),
            config.ai.privacy.clone(),
            config.ai.limits.clone(),
            config.ai.models.triage.clone(),
            config.rules.max_examples as usize,
            Arc::clone(&rules_ai_semaphore),
            Arc::clone(&rules_ai_rate_limiter),
        )) as Arc<dyn Classifier>,
        ActionRunner::new(
            db.clone(),
            rules_mail_store,
            rules_tag_store,
            DraftStore::new(db.clone()),
            events.clone(),
            rmail_core::hooks::resolve(&config.hooks),
            hook_semaphore,
            usize::try_from(config.hooks.max_output_bytes).unwrap_or(usize::MAX),
            config.rules.archive_mailbox.clone(),
        ),
        Arc::clone(&ai_policy),
        config.rules.max_window_messages as usize,
    )
    .with_injection_config(config.ai.injection.clone());
    let rule_service = RuleServiceServer::new(RuleApi::new(
        rule_engine.clone(),
        RuleSynthesizer::new(
            rule_engine.clone(),
            Arc::clone(&ai_provider),
            Arc::clone(&ai_policy),
            config.ai.privacy.clone(),
            config.ai.limits.clone(),
            config.ai.models.deep.clone(),
            rules_ai_semaphore,
            rules_ai_rate_limiter,
        ),
        config.rules.dry_run_days,
        stopping.clone(),
    ));
    let rule_evaluator_handle = if config.rules.enabled {
        Some(
            RuleEvaluator::new(rule_engine, events.clone())
                .with_tick_interval(config.rules.tick_interval.as_duration())
                .with_max_batch(config.rules.max_batch as usize)
                .spawn(stopping.clone())
                .await,
        )
    } else {
        tracing::info!(
            "rules.enabled = false; rules are not evaluated automatically on new mail \
             (RuleService still serves create/list/evaluate/backtest)"
        );
        None
    };

    // The priority notification engine (task 81). `NotificationService` is
    // always registered, for the same reason `HookService`/`RuleService` are:
    // reflection and the fail-closed scope table must see every RPC
    // regardless of runtime config. `notify.enabled` gates two things —
    // whether a `NewMail` event enqueues a scoring job at all (wired into
    // `AiDispatchLoop` above) and whether the delivery loop runs.
    // `StreamAlerts` stays useful on a disabled daemon: it replays whatever
    // was delivered before the feature was switched off.
    let notify_engine = NotifyEngine::from_config(db.clone(), &config.notify, &config.accounts)
        .map_err(ServeError::InvalidNotify)?;
    let notification_service = NotificationServiceServer::new(NotificationApi::new(
        db.clone(),
        notify_engine.clone(),
        ai_queue.clone(),
        // Both switches, not just `notify.enabled`: the scoring pass runs on
        // the AI worker pool, which is only driven when `ai_active`. A daemon
        // with notifications on but AI off would otherwise answer `QUEUED` to
        // `ScoreMessage` and then never run the job — a promise it structurally
        // cannot keep, and a queue row that would sit there forever thanks to
        // `AiQueue::enqueue`'s own `(message_id, pass)` dedup.
        config.notify.enabled && ai_active,
        stopping.clone(),
    ));
    let notify_handle = if config.notify.enabled {
        tracing::info!(
            channel = notify_engine.channel_name(),
            "the priority notification engine is running"
        );
        Some(notify_engine.spawn(stopping.clone()))
    } else {
        tracing::info!("notify.enabled = false; new mail is not scored for notification");
        None
    };

    // The periodic AI digest (task 70). Two switches, and they gate different
    // things — the same split `notify.enabled` already established:
    //
    // - `ai_active` gates the *engine*. With `NullProvider` behind it every
    //   briefing would scan a window, cluster it, and then fail at the
    //   provider, so the handler declines up front instead (see
    //   `AnalyticsApi::with_digest`) and the scheduler is never spawned.
    // - `digest.enabled` gates only the *timer*. `GenerateDigest` and `mail
    //   digest` keep working on a daemon with the schedule off, because what
    //   the switch exists to control is unattended, recurring spend, not the
    //   operator's own explicit request.
    //
    // The engine draws on the same `ai.limits` semaphore and rate limiter as
    // every other AI call site in this process: a digest is exactly the kind
    // of wide, bursty work `ai.limits` exists to bound, and a second budget
    // would let the two together exceed it.
    let digest_engine = ai_active.then(|| {
        DigestEngine::new(
            db.clone(),
            Arc::clone(&ai_provider),
            Arc::clone(&ai_policy),
            config.ai.privacy.clone(),
            config.ai.limits.clone(),
            config.digest.clone(),
            Arc::clone(&digest_ai_semaphore),
            Arc::clone(&digest_ai_rate_limiter),
        )
    });
    // The three model-backed analytics reports (task 72). Gated on
    // `ai_active` for the reason the digest engine above is: with
    // `NullProvider` behind them each would scan the mailbox and then fail at
    // the provider, and `FAILED_PRECONDITION` up front is the honest answer.
    //
    // `GetContactInsight`'s numbers and `ListSubscriptions`' header/behaviour
    // verdicts do *not* depend on any of this — the handler serves those
    // straight off the database whether or not an engine is attached, and
    // declines only the halves that would call a model.
    //
    // `ai.models.deep` on all three, and the same
    // semaphore/rate-limiter pair as every other ad-hoc call site: these are
    // one-off reasoning jobs answering a human who is waiting, exactly the
    // shape `QueryCompiler` and `RuleService/SynthesizeRule` already use.
    let mut analytics_api = analytics_api;
    if let Some(engine) = digest_engine.clone() {
        analytics_api = analytics_api.with_digest(engine);
    }
    if ai_active {
        analytics_api = analytics_api
            .with_contact_briefer(rmail_core::analytics::ContactBriefer::new(
                db.clone(),
                Arc::clone(&ai_provider),
                Arc::clone(&ai_policy),
                config.ai.privacy.clone(),
                config.ai.limits.clone(),
                config.ai.models.deep.clone(),
                Arc::clone(&ai_semaphore),
                Arc::clone(&ai_rate_limiter),
            ))
            .with_subscription_classifier(rmail_core::analytics::SubscriptionClassifier::new(
                db.clone(),
                Arc::clone(&ai_provider),
                Arc::clone(&ai_policy),
                config.ai.privacy.clone(),
                config.ai.limits.clone(),
                config.ai.models.deep.clone(),
                Arc::clone(&ai_semaphore),
                Arc::clone(&ai_rate_limiter),
            ))
            .with_analytics_asker(rmail_core::analytics::AnalyticsAsker::new(
                db.clone(),
                Arc::clone(&ai_provider),
                Arc::clone(&ai_policy),
                config.ai.privacy.clone(),
                config.ai.limits.clone(),
                config.ai.models.deep.clone(),
                Arc::clone(&ai_semaphore),
                Arc::clone(&ai_rate_limiter),
            ));
    }
    let analytics_service = AnalyticsServiceServer::new(analytics_api);

    // Account autoconfiguration (task 80). The engine is built whenever its
    // HTTP client can be; only the *model fallback* depends on `ai_active`,
    // and a daemon without a provider still probes ISPDB/autodiscover/SRV
    // perfectly well — it simply refuses `allow_model_fallback` rather than
    // silently answering "nothing found" to a caller who asked for a guess.
    //
    // The model is `ai.models.deep`, not the cheap tier: this runs a handful
    // of times in an account's whole life, on one small request, and the cost
    // of a wrong hostname is a user typing their password at a machine that
    // is not their mail provider. The budget enforcer may still downgrade it.
    // The pacing pair is the process-wide one, as everywhere else.
    let account_service = AccountServiceServer::new(
        match rmail_core::autoconfig::Autoconfigurator::new(
            db.clone(),
            injected.autoconfig_endpoints.clone().unwrap_or_default(),
        ) {
            Ok(engine) => {
                let engine = if ai_active {
                    engine.with_inferrer(rmail_core::autoconfig::infer::SettingsInferrer::new(
                        db.clone(),
                        Arc::clone(&ai_provider),
                        Arc::clone(&ai_policy),
                        config.ai.limits.clone(),
                        Arc::clone(&ai_semaphore),
                        Arc::clone(&ai_rate_limiter),
                        config.ai.models.deep.clone(),
                    ))
                } else {
                    engine
                };
                account_api.with_autoconfig(engine)
            }
            Err(error) => {
                tracing::warn!(
                    %error,
                    "could not build the autoconfiguration engine; \
                     AccountService.Autoconfigure will decline"
                );
                account_api
            }
        },
    );
    let digest_handle = match (&digest_engine, config.digest.enabled) {
        (Some(engine), true) => {
            let scheduler = DigestScheduler::new(engine.clone(), db.clone());
            tracing::info!(
                interval_seconds = engine.interval_seconds(),
                tick_interval = ?scheduler.tick_interval(),
                "the periodic AI digest is running"
            );
            Some(scheduler.spawn(stopping.clone()))
        }
        (Some(_), false) => {
            tracing::info!(
                "digest.enabled = false; no periodic digest is produced (GenerateDigest and \
                 `mail digest` still answer on demand)"
            );
            None
        }
        (None, _) => {
            tracing::info!("the AI subsystem is off; no digest is produced on this daemon");
            None
        }
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
        .add_service(analytics_service)
        .add_service(account_service)
        .add_service(sync_service)
        .add_service(mail_service)
        .add_service(export_service)
        .add_service(note_service)
        .add_service(tag_service)
        .add_service(compose_service)
        .add_service(config_service)
        .add_service(send_scheduler_service)
        .add_service(search_service)
        .add_service(index_service)
        .add_service(saved_search_service)
        .add_service(finder_service)
        .add_service(ai_service)
        .add_service(attachment_service)
        .add_service(extract_service)
        .add_service(link_service)
        .add_service(ai_policy_service)
        .add_service(ai_safety_service)
        .add_service(hook_service)
        .add_service(rule_service)
        .add_service(notification_service)
        .add_service(webhook_service)
        .serve_with_incoming_shutdown(incoming, shutdown)
        .await;

    stopping.cancel();
    let _ = pruner.await;
    let _ = feedback_pruner.await;
    if let Some(handle) = ai_dispatch_handle {
        let _ = handle.await;
    }
    if let Some(handle) = hook_dispatch_handle {
        let _ = handle.await;
    }
    if let Some(handle) = webhook_dispatch_handle {
        let _ = handle.await;
    }
    if let Some(handle) = rule_evaluator_handle {
        let _ = handle.await;
    }
    if let Some(handle) = notify_handle {
        let _ = handle.await;
    }
    if let Some(handle) = digest_handle {
        let _ = handle.await;
    }
    let _ = index_handle.await;
    let _ = smart_folder_handle.await;
    if let Some(handle) = finder_handle {
        let _ = handle.await;
    }
    // Awaited rather than dropped: an in-flight SMTP conversation that is
    // abandoned mid-`DATA` is exactly the crash the at-most-once fence exists
    // to survive, and paying for a recovery on every clean shutdown would be
    // a self-inflicted one.
    let _ = send_handle.await;

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
