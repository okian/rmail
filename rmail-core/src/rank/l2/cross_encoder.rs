//! The local ONNX cross-encoder (prd.md Stage 5, backend 1): "a small local
//! reranker (e.g. a MiniLM/bge-reranker via ONNX) scores `(query,
//! message_text)` pairs jointly — far more precise than bi-encoder cosine,
//! cheap enough for 50 pairs (<80 ms), fully offline, zero egress."
//!
//! # Why a cross-encoder beats the cosine the dense retriever already has
//!
//! [`crate::retrieve::DenseRetriever`] scores with a *bi*-encoder: the query
//! and the message are embedded separately and compared by cosine, which is
//! what makes a kNN index possible at all (the message vectors are computed
//! at index time, before any query exists). The price is that the two texts
//! never meet — the model cannot notice that "invoice" in the query and
//! "invoice" in the message refer to the same invoice. A cross-encoder
//! concatenates the pair and runs attention across both, which is far more
//! precise and far too expensive to run over a whole mailbox. Running it over
//! the top-K only, after four cheaper stages have cut the field, is the entire
//! design of prd.md's cascade.
//!
//! # Never on the runtime, and never twice at once
//!
//! Inference is CPU-bound for as long as the batch takes, and an ONNX session
//! is not something to hold two of. This module mirrors
//! [`crate::embed::local::LocalEmbedder`]'s established shape exactly: a
//! [`Semaphore`] acquired as an async task *before* a blocking thread is
//! spawned (so waiters suspend instead of parking a blocking-pool thread on a
//! mutex), then [`tokio::task::spawn_blocking`] around the session lock. The
//! reasoning is that module's, verbatim, and diverging from it here would put
//! two different concurrency disciplines on the same shared blocking pool.
//!
//! # The model file is provisioned, never vendored
//!
//! `bge-reranker-base` is hundreds of megabytes; it is not in this repository
//! and never will be. It is fetched into the shared model cache
//! (`$RMAIL_MODEL_CACHE`, the same directory the local embedder uses) on
//! first use *only* when `search.reranker.cross_encoder_allow_download` says
//! so — off by default, because a search box that silently dials Hugging Face
//! is not the offline-first reranker this backend advertises. An
//! unprovisioned cache is a [`Error::FailedPrecondition`] naming the exact
//! two ways to fix it, and [`super::L2Stage`] turns that into "keep the L1
//! order," so an operator who never provisions the model gets a working
//! search that is merely not reranked.
//!
//! # Built without the `onnx` feature
//!
//! The whole backend still exists (so the config, the wiring, and the
//! degradation path are compiled and tested either way) and every call
//! returns [`Error::FailedPrecondition`] saying which feature is missing.
//! That is the same posture [`crate::embed`] takes for its own ONNX path.

#[cfg(feature = "onnx")]
use std::sync::Arc;
#[cfg(feature = "onnx")]
use std::time::Instant;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use super::{RerankCandidate, RerankVerdict, Reranker};
use crate::config::RerankerConfig;
use crate::error::Error;

/// The tokenizer sequence length the pair is truncated to. 512 is
/// `bge-reranker-base`'s trained maximum; asking for more silently degrades
/// quality rather than extending context.
#[cfg(feature = "onnx")]
const MAX_SEQUENCE: usize = 512;

/// How many `(query, document)` pairs go to the session at once. The window
/// is `search.top_k_rerank` (default 50) end to end, so this is about peak
/// memory during one rerank, not about throughput across many.
#[cfg(feature = "onnx")]
const BATCH: usize = 16;

/// How long an "the model is not provisioned" verdict is trusted before the
/// filesystem is probed again. Long enough that a stock daemon does not scan
/// a directory per search, short enough that provisioning the weights takes
/// effect without a restart.
#[cfg(feature = "onnx")]
const PROBE_BACKOFF: std::time::Duration = std::time::Duration::from_secs(60);

/// The local cross-encoder reranker.
pub struct CrossEncoderReranker {
    model: String,
    #[cfg(feature = "onnx")]
    cache: std::path::PathBuf,
    #[cfg(feature = "onnx")]
    allow_download: bool,
    #[cfg(feature = "onnx")]
    session: tokio::sync::OnceCell<Arc<std::sync::Mutex<fastembed::TextRerank>>>,
    /// One inference at a time — see the module docs. `Arc` because the
    /// permit is *moved into* the blocking task rather than held by the
    /// awaiting future: a stage timeout drops that future, and a permit
    /// released while the detached inference still owns the session mutex
    /// would let the next rerank park a blocking-pool thread on that mutex —
    /// precisely the outcome this semaphore exists to prevent.
    #[cfg(feature = "onnx")]
    permit: Arc<tokio::sync::Semaphore>,
    /// When the model was last found to be absent, so a stock daemon
    /// (`rerank = "auto"`, nothing provisioned — the steady state) does not
    /// re-probe the filesystem on every keystroke. Re-probed after
    /// [`PROBE_BACKOFF`] so provisioning the weights takes effect without a
    /// daemon restart.
    #[cfg(feature = "onnx")]
    absent_since: std::sync::Mutex<Option<std::time::Instant>>,
}

impl std::fmt::Debug for CrossEncoderReranker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hand-written for the identical reason `LocalEmbedder`'s is: an ONNX
        // session has no useful `Debug` and several hundred megabytes of it
        // would not be worth reading anyway.
        let mut out = f.debug_struct("CrossEncoderReranker");
        out.field("model", &self.model);
        #[cfg(feature = "onnx")]
        out.field("loaded", &self.session.initialized());
        out.finish_non_exhaustive()
    }
}

impl CrossEncoderReranker {
    /// A reranker for the configured model. Does no I/O — the session loads
    /// on first use so daemon start does not pay for a model nobody has
    /// searched with yet.
    #[must_use]
    pub fn new(config: &RerankerConfig) -> Self {
        Self {
            model: config.cross_encoder_model.clone(),
            #[cfg(feature = "onnx")]
            cache: if config.cross_encoder_cache_dir.trim().is_empty() {
                crate::embed::local::cache_dir()
            } else {
                std::path::PathBuf::from(config.cross_encoder_cache_dir.trim())
            },
            #[cfg(feature = "onnx")]
            allow_download: config.cross_encoder_allow_download,
            #[cfg(feature = "onnx")]
            session: tokio::sync::OnceCell::new(),
            #[cfg(feature = "onnx")]
            permit: Arc::new(tokio::sync::Semaphore::new(1)),
            #[cfg(feature = "onnx")]
            absent_since: std::sync::Mutex::new(None),
        }
    }

    /// The loaded session, loading it if this is the first ask.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] for a model id this build cannot load,
    /// [`Error::FailedPrecondition`] for an unprovisioned cache or a loader
    /// failure.
    #[cfg(feature = "onnx")]
    async fn session(&self) -> Result<Arc<std::sync::Mutex<fastembed::TextRerank>>, Error> {
        // `get_or_try_init` leaves the cell uninitialized on error, so
        // without this every search on an unprovisioned daemon would re-run
        // the whole probe. The backoff makes that once per
        // `PROBE_BACKOFF`, while still letting a freshly-provisioned cache be
        // picked up without a restart.
        if self.recently_absent() {
            return Err(self.absent_error());
        }
        let session = self
            .session
            .get_or_try_init(|| async {
                let model = known_model(&self.model)?;
                let cache = self.cache.clone();
                let name = self.model.clone();
                // The weight download is a TLS connection made by a client
                // this crate does not own; it reaches the same choke point as
                // every other one so the process has exactly one provider.
                crate::transport::install_crypto_provider();
                let allow_download = self.allow_download;
                // Every filesystem touch — the cache probe as well as the
                // load — runs on the blocking pool. `crate::embed::local::cached`
                // is a synchronous `read_dir`, and on the default
                // configuration this path is reached once per search, so
                // running it inline would put a directory scan on a runtime
                // worker thread on the query hot path.
                let loaded = tokio::task::spawn_blocking(move || {
                    // Checked here because it cannot be checked anywhere else:
                    // `fastembed`'s downloader ignores `HF_HUB_OFFLINE`, so
                    // there is no way to tell it not to fetch — see
                    // `embed::local::LocalEmbedder::session`'s identical note.
                    if !allow_download && !crate::embed::local::cached(&cache, &name) {
                        return Ok(None);
                    }
                    tracing::info!(
                        model = %name,
                        cache = %cache.display(),
                        "loading the local cross-encoder reranker"
                    );
                    fastembed::TextRerank::try_new(
                        fastembed::RerankInitOptions::new(model)
                            .with_cache_dir(cache)
                            .with_max_length(MAX_SEQUENCE)
                            .with_show_download_progress(false),
                    )
                    .map(Some)
                    .map_err(|e| {
                        // Logged in full, reported in brief by the caller: a
                        // loader error string carries the cache path, which
                        // is under `$HOME`.
                        tracing::warn!(model = %name, error = %e, "the cross-encoder failed to load");
                    })
                })
                .await
                .map_err(|e| Error::internal(format!("reranker load task failed: {e}")))?;
                match loaded {
                    Ok(Some(session)) => Ok(Arc::new(std::sync::Mutex::new(session))),
                    Ok(None) => {
                        self.mark_absent();
                        Err(self.absent_error())
                    }
                    Err(()) => Err(Error::failed_precondition(format!(
                        "could not load the cross-encoder model {:?}; search results keep \
                         their L1 order. Provision the weights on a host with network access, \
                         or point {} at a directory that already has them.",
                        self.model,
                        crate::embed::local::CACHE_ENV
                    ))),
                }
            })
            .await?;
        Ok(Arc::clone(session))
    }

    /// Whether the model was found missing recently enough not to look again.
    #[cfg(feature = "onnx")]
    fn recently_absent(&self) -> bool {
        self.absent_since
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some_and(|at| at.elapsed() < PROBE_BACKOFF)
    }

    #[cfg(feature = "onnx")]
    fn mark_absent(&self) {
        *self
            .absent_since
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(std::time::Instant::now());
    }

    /// The one message an operator needs: what is missing, and the two ways
    /// to fix it.
    #[cfg(feature = "onnx")]
    fn absent_error(&self) -> Error {
        Error::failed_precondition(format!(
            "the cross-encoder weights for {:?} are not in the model cache and downloading \
             is off, so search results keep their L1 order. Set \
             `search.reranker.cross_encoder_allow_download = true` to fetch them once, or \
             populate the cache out of band and point {} at it.",
            self.model,
            crate::embed::local::CACHE_ENV
        ))
    }
}

#[async_trait]
impl Reranker for CrossEncoderReranker {
    fn name(&self) -> &'static str {
        "cross_encoder"
    }

    /// Local inference: nothing leaves the machine, which is exactly what
    /// makes this backend usable on `ai.policy` `local_only` mail.
    fn needs_network(&self) -> bool {
        false
    }

    #[cfg(feature = "onnx")]
    #[tracing::instrument(
        skip(self, query, candidates, cancel),
        fields(
            backend = "cross_encoder",
            candidates = candidates.len(),
            model = %self.model,
            elapsed_ms,
        )
    )]
    async fn rerank(
        &self,
        query: &str,
        candidates: &[RerankCandidate],
        cancel: &CancellationToken,
    ) -> Result<Vec<RerankVerdict>, Error> {
        if candidates.is_empty() {
            return Ok(Vec::new());
        }
        // Checked before the session load, which on a cold cache is the
        // single most expensive thing this backend does.
        if cancel.is_cancelled() {
            return Err(Error::deadline_exceeded(
                "the query was superseded before the cross-encoder ran".to_owned(),
            ));
        }
        let session = self.session().await?;
        let documents: Vec<String> = candidates.iter().map(|c| c.document.clone()).collect();
        let query = query.to_owned();

        // Queued as an async task, not on a blocking thread — see the module
        // docs.
        let permit = tokio::select! {
            () = cancel.cancelled() => {
                return Err(Error::deadline_exceeded(
                    "the query was superseded while waiting for the cross-encoder".to_owned(),
                ));
            }
            permit = Arc::clone(&self.permit).acquire_owned() => permit.map_err(|_| {
                Error::internal("the cross-encoder was shut down".to_owned())
            })?,
        };

        let started = Instant::now();
        // Not raced against `cancel`: `spawn_blocking` cannot be interrupted,
        // so racing it would only detach the work, not stop it. The batch is
        // bounded by `search.top_k_rerank` pairs at `MAX_SEQUENCE` tokens
        // (prd.md budgets 50 pairs at <80 ms), and `L2Stage`'s own timeout is
        // what bounds the *stage* — a rerank that outlives its usefulness is
        // discarded by the caller, having cost one bounded burst of CPU.
        let scored = tokio::task::spawn_blocking(move || {
            // Held by the blocking task itself, released only when the
            // inference is genuinely finished — see the `permit` field's own
            // doc comment for the failure this prevents.
            let _permit = permit;
            // Recovered rather than propagated, exactly as
            // `LocalEmbedder::embed` does: a panic inside `ort` poisons this
            // mutex for the life of the process, and refusing every later
            // rerank would leave search permanently and silently degraded.
            let mut guard = session.lock().unwrap_or_else(|poisoned| {
                tracing::warn!(
                    "the cross-encoder session was poisoned by an earlier panic; \
                     continuing with it"
                );
                std::sync::PoisonError::into_inner(poisoned)
            });
            guard
                // `query` by value, not `&str`: `TextRerank::rerank` binds the
                // query and the documents to one `S: AsRef<str>`, so a `&str`
                // query alongside a `Vec<String>` of documents does not
                // unify.
                .rerank(query, &documents, false, Some(BATCH))
                .map_err(|e| Error::internal(format!("cross-encoder inference failed: {e}")))
        })
        .await
        .map_err(|e| Error::internal(format!("cross-encoder task failed: {e}")))??;
        tracing::Span::current().record("elapsed_ms", started.elapsed().as_millis());

        // `fastembed` returns results sorted best-first with an `index` back
        // into the input slice. A result whose index is out of range would
        // mean the library broke its own contract; dropped rather than
        // trusted, so a bad index can never mis-attribute one message's score
        // to another.
        let mut verdicts = Vec::with_capacity(scored.len());
        for result in scored {
            let Some(candidate) = candidates.get(result.index) else {
                tracing::warn!(
                    index = result.index,
                    len = candidates.len(),
                    "the cross-encoder returned an out-of-range index; dropping it"
                );
                continue;
            };
            verdicts.push(RerankVerdict {
                message_id: candidate.message_id,
                score: f64::from(result.score),
                // prd.md's one-line "why" is the Claude backend's; a
                // cross-encoder produces a logit and no prose, and inventing
                // a sentence from it would be a fabricated explanation.
                why: None,
            });
        }
        if verdicts.len() != candidates.len() {
            return Err(Error::internal(format!(
                "the cross-encoder scored {} of {} candidates",
                verdicts.len(),
                candidates.len()
            )));
        }
        Ok(verdicts)
    }

    /// Load the session, which is where "is this model provisioned at all?"
    /// is answered. Costs nothing extra: [`Self::rerank`] would have loaded
    /// it anyway, and the [`tokio::sync::OnceCell`] makes the second ask
    /// free. What it buys is the document fetch skipped when the answer is
    /// no — the default state of an unprovisioned daemon.
    #[cfg(feature = "onnx")]
    async fn ready(&self, cancel: &CancellationToken) -> Result<(), Error> {
        if cancel.is_cancelled() {
            return Err(Error::deadline_exceeded(
                "the query was superseded before the cross-encoder loaded".to_owned(),
            ));
        }
        self.session().await.map(|_| ())
    }

    #[cfg(not(feature = "onnx"))]
    async fn ready(&self, _cancel: &CancellationToken) -> Result<(), Error> {
        Err(Error::failed_precondition(format!(
            "this build has no ONNX runtime (the `onnx` feature is off), so the \
             {:?} cross-encoder cannot run and search results keep their L1 order",
            self.model
        )))
    }

    #[cfg(not(feature = "onnx"))]
    async fn rerank(
        &self,
        _query: &str,
        _candidates: &[RerankCandidate],
        _cancel: &CancellationToken,
    ) -> Result<Vec<RerankVerdict>, Error> {
        Err(Error::failed_precondition(format!(
            "this build has no ONNX runtime (the `onnx` feature is off), so the \
             {:?} cross-encoder cannot run and search results keep their L1 order",
            self.model
        )))
    }
}

/// Map a configured model id onto one this build can actually load.
///
/// # Errors
///
/// [`Error::InvalidArgument`] naming the models that are available — a typo
/// in a config file should say what to write instead, not fail deep inside a
/// model loader with a Hugging Face 404. Mirrors
/// [`crate::embed::local`]'s own `known_model`.
#[cfg(feature = "onnx")]
fn known_model(id: &str) -> Result<fastembed::RerankerModel, Error> {
    match id {
        "bge-reranker-base" => Ok(fastembed::RerankerModel::BGERerankerBase),
        "bge-reranker-v2-m3" => Ok(fastembed::RerankerModel::BGERerankerV2M3),
        "jina-reranker-v1-turbo-en" => Ok(fastembed::RerankerModel::JINARerankerV1TurboEn),
        "jina-reranker-v2-base-multilingual" => {
            Ok(fastembed::RerankerModel::JINARerankerV2BaseMultiligual)
        }
        other => Err(Error::invalid_argument(format!(
            "unknown cross-encoder model {other:?}; supported: bge-reranker-base, \
             bge-reranker-v2-m3, jina-reranker-v1-turbo-en, \
             jina-reranker-v2-base-multilingual"
        ))),
    }
}
