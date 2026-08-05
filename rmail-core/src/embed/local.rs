//! The local ONNX embedder: `bge-small-en-v1.5` on this machine.
//!
//! # Why this is the default
//!
//! Mail is the most sensitive corpus most people own. A hosted embedding API
//! sees every message body it is asked to index, which for a mailbox is
//! effectively all of it. Running the model here means the text never leaves
//! the host, and `bge-small-en-v1.5` is small enough (33M parameters, 384
//! dimensions) that this is not a sacrifice made for privacy — it is a
//! genuinely good retrieval model that happens to fit.
//!
//! # Loading is lazy and happens once
//!
//! The model is several hundred megabytes of session state. It is built inside
//! a [`OnceCell`] so that constructing the embedder — which every daemon and
//! every test does — costs nothing, the load happens at most once however many
//! tasks race for it, and [`Embedder::warm`] at daemon start can pay for it
//! before a user is waiting on a query.
//!
//! # Provisioning is the one online step
//!
//! The weights are fetched from Hugging Face into a cache directory on first
//! use and read from disk ever after. That first fetch is the only thing in
//! this backend that needs a network, and an operator who cannot allow one can
//! populate the cache out of band and point `RMAIL_MODEL_CACHE` at it.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use fastembed::{EmbeddingModel, InitOptions, TextEmbedding};
use tokio::sync::{OnceCell, Semaphore};

use crate::config::LocalEmbedConfig;
use crate::embed::{truncate, Embedder, Embedding, MAX_BATCH};
use crate::error::Error;

/// Where the weights live, unless the environment says otherwise.
const CACHE_ENV: &str = "RMAIL_MODEL_CACHE";

/// The local ONNX embedder.
///
/// # One inference at a time, waited for as an async task
///
/// `fastembed` needs `&mut` to run an inference and an ONNX session is not
/// something to hold two of, so the session sits behind a blocking [`Mutex`].
/// Taking that lock *inside* `spawn_blocking` would be sound but wasteful in a
/// way that reaches other subsystems: eight concurrent embeds measured 6.3x the
/// latency of one, with seven blocking-pool threads parked on a mutex doing
/// nothing — and that pool is shared with the credential commands IMAP login
/// runs on. The [`Semaphore`] is acquired *before* the blocking task is
/// spawned, so waiters suspend as async tasks and the mutex is uncontended by
/// the time anyone reaches it.
pub struct LocalEmbedder {
    model: String,
    dim: usize,
    cache: PathBuf,
    allow_download: bool,
    session: OnceCell<Arc<Mutex<TextEmbedding>>>,
    /// One permit: the session admits one inference at a time.
    permit: Semaphore,
}

impl std::fmt::Debug for LocalEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hand-written because an ONNX session has no useful `Debug` and
        // several hundred megabytes of it would not be worth reading anyway.
        f.debug_struct("LocalEmbedder")
            .field("model", &self.model)
            .field("dim", &self.dim)
            .field("cache", &self.cache)
            .field("loaded", &self.session.initialized())
            .finish()
    }
}

impl LocalEmbedder {
    /// An embedder for the configured model. Does no I/O.
    #[must_use]
    pub fn new(config: &LocalEmbedConfig) -> Self {
        Self {
            model: config.model.clone(),
            dim: config.dim as usize,
            cache: if config.cache_dir.trim().is_empty() {
                cache_dir()
            } else {
                PathBuf::from(config.cache_dir.trim())
            },
            allow_download: config.allow_download,
            session: OnceCell::new(),
            permit: Semaphore::new(1),
        }
    }

    /// The loaded model, loading it if this is the first ask.
    async fn session(&self) -> Result<Arc<Mutex<TextEmbedding>>, Error> {
        let session = self
            .session
            .get_or_try_init(|| async {
                let model = known_model(&self.model)?;
                let cache = self.cache.clone();
                let name = self.model.clone();
                // The weight download is a TLS connection made by a client
                // this crate does not own. It reaches the same choke point as
                // the IMAP and HTTP clients so the process has exactly one
                // provider however the fetch is implemented.
                crate::transport::install_crypto_provider();
                // Checked here, by us, because it cannot be checked anywhere
                // else: `fastembed`'s downloader ignores `HF_HUB_OFFLINE`, so
                // there is no way to tell it not to fetch. Without this, the
                // first search on an unprovisioned host silently pulls a
                // hundred and thirty megabytes from Hugging Face — from a
                // backend whose entire selling point is that nothing leaves
                // the host.
                if !self.allow_download && !cached(&cache, &name) {
                    return Err(Error::failed_precondition(format!(
                        "the weights for {name:?} are not in the model cache and \
                         downloading is off. Set \
                         `index.semantic.local.allow_download = true` to fetch them \
                         once, or populate the cache out of band and point \
                         {CACHE_ENV} at it."
                    )));
                }
                tracing::info!(model = %name, cache = %cache.display(), "loading local embedder");
                // Loading is a long CPU- and disk-bound operation, and on a
                // cold cache a network one. It must not sit on a runtime
                // thread while every other task waits.
                tokio::task::spawn_blocking(move || {
                    TextEmbedding::try_new(
                        InitOptions::new(model)
                            .with_cache_dir(cache)
                            .with_show_download_progress(false),
                    )
                })
                .await
                .map_err(|e| Error::internal(format!("embedder load task failed: {e}")))?
                .map_err(|e| {
                    // Logged in full, reported in brief. A loader error string
                    // carries the cache path — which is under `$HOME` — and a
                    // `FailedPrecondition` message goes to clients verbatim.
                    tracing::warn!(model = %name, error = %e, "local embedder failed to load");
                    // A precondition, not an internal fault: the usual cause is
                    // an unprovisioned cache on a host with no egress, which is
                    // an operator action rather than a bug, and the message has
                    // to say which action.
                    Error::failed_precondition(format!(
                        "could not load local embedding model {name:?}. \
                         Provision the weights on a host with network access, \
                         or point {CACHE_ENV} at a directory that already has them."
                    ))
                })
                .map(|session| Arc::new(Mutex::new(session)))
            })
            .await?;
        Ok(Arc::clone(session))
    }
}

#[async_trait::async_trait]
impl Embedder for LocalEmbedder {
    fn model(&self) -> &str {
        &self.model
    }

    fn dim(&self) -> usize {
        self.dim
    }

    #[tracing::instrument(
        skip(self, texts),
        fields(model = %self.model, batch = texts.len(), elapsed_ms)
    )]
    async fn embed(&self, texts: &[String]) -> Result<Vec<Embedding>, Error> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let started = std::time::Instant::now();
        let session = self.session().await?;
        let mut out = Vec::with_capacity(texts.len());
        for chunk in texts.chunks(MAX_BATCH) {
            // Truncated here rather than left to the tokenizer: every model
            // silently drops what will not fit, and doing it ourselves makes
            // the limit uniform across backends and — because it always cuts
            // the same prefix — keeps re-embedding unchanged content
            // reproducible, which is what the content-hash cache rests on.
            let batch: Vec<String> = chunk.iter().map(|t| truncate(t).to_owned()).collect();
            let session = Arc::clone(&session);
            // Queued here, as an async task, rather than on a blocking thread.
            let _permit = self
                .permit
                .acquire()
                .await
                .map_err(|_| Error::internal("the embedder was shut down".to_owned()))?;
            // Inference is CPU-bound for as long as the batch takes; on a
            // runtime thread it would stall every other task in the process,
            // including the ones serving the query that asked for it.
            let vectors = tokio::task::spawn_blocking(move || {
                // Recovered rather than propagated. A panic inside ort poisons
                // the mutex for the life of the process, and refusing every
                // subsequent embed leaves the daemon permanently and silently
                // degraded — the error a client sees is a redacted "internal
                // error" with nothing to diagnose. The session itself is no
                // more broken than it was the instant before.
                let mut guard = session.lock().unwrap_or_else(|poisoned| {
                    tracing::warn!(
                        "the embedding session was poisoned by an earlier panic; \
                         continuing with it"
                    );
                    PoisonError::into_inner(poisoned)
                });
                guard
                    .embed(batch, None)
                    .map_err(|e| Error::internal(format!("embedding failed: {e}")))
            })
            .await
            .map_err(|e| Error::internal(format!("embedding task failed: {e}")))??;
            if vectors.len() != chunk.len() {
                // The contract is one vector per input at the same index. A
                // backend that returns a different number has broken the zip
                // every caller does, and quietly returning the short list would
                // attach each vector to the wrong message.
                return Err(Error::internal(format!(
                    "embedder returned {} vectors for {} inputs",
                    vectors.len(),
                    chunk.len()
                )));
            }
            for vector in vectors {
                if vector.len() != self.dim {
                    return Err(Error::internal(format!(
                        "model {:?} produced {} dimensions, configured for {}",
                        self.model,
                        vector.len(),
                        self.dim
                    )));
                }
                out.push(Embedding::new(vector));
            }
        }
        tracing::Span::current().record("elapsed_ms", started.elapsed().as_millis());
        Ok(out)
    }
}

/// Whether `cache` already holds a usable snapshot of `model`.
///
/// The layout is Hugging Face's: `models--<org>--<model>/snapshots/<rev>/…`.
/// Matched on the suffix rather than the full name because the organization
/// that publishes a given ONNX export is `fastembed`'s business, not ours, and
/// pinning it here would break on an upstream re-host.
fn cached(cache: &Path, model: &str) -> bool {
    let suffix = format!("--{}", model.to_lowercase());
    let Ok(entries) = std::fs::read_dir(cache) else {
        return false;
    };
    entries.filter_map(Result::ok).any(|entry| {
        let name = entry.file_name().to_string_lossy().to_lowercase();
        name.starts_with("models--")
            && name.ends_with(&suffix)
            // A directory left behind by an interrupted fetch has the outer
            // shape and no contents; treating that as provisioned turns a
            // clear precondition into a confusing loader error.
            && std::fs::read_dir(entry.path().join("snapshots"))
                .is_ok_and(|mut snaps| snaps.next().is_some())
    })
}

/// Where weights are cached.
fn cache_dir() -> PathBuf {
    if let Ok(dir) = std::env::var(CACHE_ENV) {
        return PathBuf::from(dir);
    }
    if let Ok(dir) = std::env::var("XDG_CACHE_HOME") {
        return PathBuf::from(dir).join("rmail").join("models");
    }
    std::env::var("HOME").map_or_else(
        |_| PathBuf::from(".rmail-models"),
        |home| {
            PathBuf::from(home)
                .join(".cache")
                .join("rmail")
                .join("models")
        },
    )
}

/// Map a configured model id onto one this build can actually load.
///
/// # Errors
///
/// [`Error::InvalidArgument`] naming the models that are available. A typo in
/// a config file should say what to write instead, not fail somewhere deep in
/// a model loader with a Hugging Face 404.
fn known_model(id: &str) -> Result<EmbeddingModel, Error> {
    // Dimensionality is a property of the model, and a mismatch between it and
    // `dim` is caught at embed time rather than trusted here, because the
    // vectors already in the index were produced by whatever was configured
    // then.
    match id {
        "bge-small-en-v1.5" => Ok(EmbeddingModel::BGESmallENV15),
        "bge-base-en-v1.5" => Ok(EmbeddingModel::BGEBaseENV15),
        "bge-large-en-v1.5" => Ok(EmbeddingModel::BGELargeENV15),
        "all-MiniLM-L6-v2" => Ok(EmbeddingModel::AllMiniLML6V2),
        "multilingual-e5-small" => Ok(EmbeddingModel::MultilingualE5Small),
        "multilingual-e5-base" => Ok(EmbeddingModel::MultilingualE5Base),
        other => Err(Error::invalid_argument(format!(
            "unknown local embedding model {other:?}; supported: bge-small-en-v1.5, \
             bge-base-en-v1.5, bge-large-en-v1.5, all-MiniLM-L6-v2, \
             multilingual-e5-small, multilingual-e5-base"
        ))),
    }
}

#[cfg(test)]
mod tests;
