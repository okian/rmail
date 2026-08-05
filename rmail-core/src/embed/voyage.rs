//! The hosted embedder: Voyage AI.
//!
//! # When this is the right choice
//!
//! Voyage's models are better than anything that fits comfortably on a laptop,
//! and for a mailbox where the text is not sensitive that is worth the egress.
//! It is never the default, because choosing to send every message body to a
//! third party has to be a decision somebody made on purpose.
//!
//! # The key is never in the config file
//!
//! `api_key_command` names a command; its stdout is the key. That keeps the
//! secret in the keychain or the password manager it already lives in, keeps it
//! out of the file people paste into bug reports, and reuses the resolution
//! path the IMAP credentials already use — including its timeout, its
//! never-echo rule and its error mapping.
//!
//! # Rate limiting is local and pessimistic
//!
//! `rpm` is enforced here rather than discovered from 429s, because a backfill
//! over a large mailbox will otherwise find the limit at full speed and spend
//! its time being rejected. Pacing costs nothing when the caller is slower than
//! the limit anyway.

use std::time::Duration;

use tokio::sync::Mutex;
use tokio::time::Instant;

use crate::config::VoyageConfig;
use crate::credential::CredentialSource;
use crate::embed::{truncate, Embedder, Embedding, MAX_BATCH};
use crate::error::Error;

/// Where requests go, unless a test points them somewhere else.
const DEFAULT_ENDPOINT: &str = "https://api.voyageai.com/v1/embeddings";

/// How long one request may take.
///
/// Embedding is on the path of a user's query. A hosted call that hangs must
/// become an error the ranker can degrade around, not a query that never
/// returns.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Longest an embed call will wait for its rate-limit slot.
///
/// `rpm` can be configured down to one, which is a 60-second gap; a queue of
/// chunks behind that makes the last caller wait minutes with nothing but the
/// per-request timeout to show for it. Matched to [`REQUEST_TIMEOUT`], because
/// a wait longer than the request itself is allowed to take is past the point
/// where anyone still wants the answer, and failing is more useful than
/// arriving too late.
const MAX_PACING_WAIT: Duration = REQUEST_TIMEOUT;

/// Most text one request carries, across all its inputs.
///
/// `MAX_BATCH * MAX_INPUT_BYTES` is 512 KiB, comfortably past the per-request
/// token cap of every hosted embedding API — which comes back as a 400 for the
/// whole batch rather than a shortfall on one input. Splitting on bytes as well
/// as on count is what keeps a batch of long messages from failing wholesale.
const MAX_REQUEST_BYTES: usize = 96 * 1024;

/// Most of an upstream error body to repeat back to a client.
///
/// The body comes from a third party and is unbounded. Everything but
/// `Internal` is emitted verbatim as the `tonic::Status` message, so a
/// multi-megabyte body would become a multi-megabyte `grpc-message` trailer —
/// which does not merely look bad, it exceeds HTTP/2 header limits and turns a
/// clean error into a transport failure. The full body goes to the log.
const MAX_UPSTREAM_DETAIL: usize = 200;

/// The Voyage-backed embedder.
#[derive(Debug)]
pub struct VoyageEmbedder {
    model: String,
    dim: usize,
    endpoint: String,
    key_source: CredentialSource,
    client: reqwest::Client,
    /// Minimum spacing between requests, from `rpm`.
    spacing: Duration,
    /// When the next request may go out.
    next_slot: Mutex<Instant>,
}

impl VoyageEmbedder {
    /// An embedder for the configured account.
    ///
    /// # Errors
    ///
    /// [`Error::FailedPrecondition`] if the HTTP client cannot be built or the
    /// configuration names no key command — the latter is caught here rather
    /// than at first query so that a misconfigured daemon fails at start,
    /// where somebody is watching.
    pub fn new(config: &VoyageConfig) -> Result<Self, Error> {
        if config.api_key_command.trim().is_empty() {
            return Err(Error::failed_precondition(
                "voyage embeddings need `api_key_command`; the key is read from a \
                 command's output and is never stored in the config file"
                    .to_owned(),
            ));
        }
        // As in the IMAP client: the provider is chosen explicitly rather than
        // inferred from features, because inference panics on the first
        // handshake once anything pulls in a second provider.
        crate::transport::install_crypto_provider();
        let client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| {
                Error::failed_precondition(format!("could not build an HTTP client: {e}"))
            })?;
        // A zero or absurd `rpm` would divide by zero or pace at a standstill;
        // clamped rather than rejected, for the same reason `dim` is.
        let rpm = config.rpm.clamp(1, 100_000);
        Ok(Self {
            model: config.model.clone(),
            dim: config.dim as usize,
            endpoint: DEFAULT_ENDPOINT.to_owned(),
            key_source: CredentialSource::Command(config.api_key_command.clone()),
            client,
            spacing: Duration::from_secs_f64(60.0 / f64::from(rpm)),
            next_slot: Mutex::new(Instant::now()),
        })
    }

    /// Point this embedder at another endpoint.
    ///
    /// Exists so the tests can drive a local server. Nothing in production
    /// calls it, and the default is the real API.
    #[must_use]
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    /// Wait until this caller's turn.
    ///
    /// The slot is advanced while the lock is held, so concurrent callers queue
    /// behind one another instead of all reading the same instant and firing
    /// together — which is the failure a rate limiter that only *checks* the
    /// clock has.
    ///
    /// # Errors
    ///
    /// [`Error::Unavailable`] if the wait would exceed [`MAX_PACING_WAIT`]. The
    /// slot is *not* consumed in that case: a caller that gives up must not
    /// leave a reservation behind, or the limiter throttles harder than the
    /// configured rate for every caller after it.
    async fn pace(&self) -> Result<(), Error> {
        let wait = {
            let mut slot = self.next_slot.lock().await;
            let now = Instant::now();
            let at = (*slot).max(now);
            let wait = at - now;
            if wait > MAX_PACING_WAIT {
                return Err(Error::unavailable(format!(
                    "the voyage rate limit would delay this request by {}s; \
                     raise `rpm` or embed less at once",
                    wait.as_secs()
                )));
            }
            *slot = at + self.spacing;
            wait
        };
        if !wait.is_zero() {
            tokio::time::sleep(wait).await;
        }
        Ok(())
    }

    /// Embed one batch, already within [`MAX_BATCH`].
    async fn embed_batch(&self, key: &str, chunk: &[String]) -> Result<Vec<Embedding>, Error> {
        self.pace().await?;
        let started = std::time::Instant::now();
        let inputs: Vec<&str> = chunk.iter().map(|t| truncate(t)).collect();
        let response = self
            .client
            .post(&self.endpoint)
            .bearer_auth(key)
            .json(&serde_json::json!({
                "model": self.model,
                "input": inputs,
                // Retrieval models embed a query and a document differently;
                // this is the document side. The query side is a separate call
                // by design, so the two never get confused for one another.
                "input_type": "document",
            }))
            .send()
            .await
            .map_err(|e| Error::unavailable(format!("voyage request failed: {e}")))?;

        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            tracing::warn!(%status, body = %body, "voyage rejected a request");
            let detail = clip(&body);
            // Distinguished so a caller can tell "fix your key" from "try
            // again later"; collapsing them into one error makes a transient
            // outage look like a configuration mistake and vice versa.
            return Err(match status.as_u16() {
                401 | 403 => Error::unauthenticated(format!(
                    "voyage rejected the API key ({status}): {detail}"
                )),
                429 => Error::unavailable(format!("voyage rate limited this client: {detail}")),
                400..=499 => Error::invalid_argument(format!(
                    "voyage rejected the request ({status}): {detail}"
                )),
                _ => Error::unavailable(format!("voyage returned {status}: {detail}")),
            });
        }

        // `Unavailable`, not `Internal`: a body that will not parse is an
        // upstream fault, most often a truncated response, and a retry policy
        // that treats it as internal will not retry something that would very
        // likely succeed on the next attempt.
        let parsed: EmbeddingResponse = response.json().await.map_err(|e| {
            tracing::warn!(error = %e, "could not read the voyage response");
            Error::unavailable("the voyage response could not be read".to_owned())
        })?;

        if parsed.data.len() != chunk.len() {
            return Err(Error::internal(format!(
                "voyage returned {} vectors for {} inputs",
                parsed.data.len(),
                chunk.len()
            )));
        }
        // Ordered by `index`, not by arrival. The API documents the order but
        // the contract this trait makes — vector `i` belongs to input `i` — is
        // not one to leave to a remote service's ordering guarantee, because
        // getting it wrong attaches every vector to the wrong message and
        // nothing downstream can detect it.
        let mut data = parsed.data;
        data.sort_by_key(|d| d.index);
        let mut out = Vec::with_capacity(data.len());
        for (position, item) in data.into_iter().enumerate() {
            if item.index != position {
                return Err(Error::internal(format!(
                    "voyage returned index {} where {position} was expected",
                    item.index
                )));
            }
            if item.embedding.len() != self.dim {
                return Err(Error::internal(format!(
                    "model {:?} produced {} dimensions, configured for {}",
                    self.model,
                    item.embedding.len(),
                    self.dim
                )));
            }
            out.push(Embedding::new(item.embedding));
        }
        tracing::debug!(
            batch = chunk.len(),
            elapsed_ms = started.elapsed().as_millis(),
            "voyage batch embedded"
        );
        Ok(out)
    }
}

/// Cut an upstream detail down to something safe to hand a client.
fn clip(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.len() <= MAX_UPSTREAM_DETAIL {
        return trimmed.to_owned();
    }
    let mut end = MAX_UPSTREAM_DETAIL;
    while end > 0 && !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… ({} bytes)", &trimmed[..end], trimmed.len())
}

/// Split `texts` into requests bounded by both count and total bytes.
fn request_chunks(texts: &[String]) -> Vec<&[String]> {
    let mut chunks = Vec::new();
    let mut start = 0usize;
    let mut bytes = 0usize;
    for (at, text) in texts.iter().enumerate() {
        let size = truncate(text).len();
        // `at > start` so a single oversized input still forms a chunk of its
        // own rather than an empty one — it is already capped at
        // `MAX_INPUT_BYTES`, so it cannot be worse than that.
        if at > start && (at - start >= MAX_BATCH || bytes + size > MAX_REQUEST_BYTES) {
            chunks.push(&texts[start..at]);
            start = at;
            bytes = 0;
        }
        bytes += size;
    }
    if start < texts.len() {
        chunks.push(&texts[start..]);
    }
    chunks
}

#[async_trait::async_trait]
impl Embedder for VoyageEmbedder {
    fn model(&self) -> &str {
        &self.model
    }

    fn dim(&self) -> usize {
        self.dim
    }

    #[tracing::instrument(
        skip(self, texts),
        fields(model = %self.model, batch = texts.len())
    )]
    async fn embed(&self, texts: &[String]) -> Result<Vec<Embedding>, Error> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        // Resolved once per call rather than held in the struct, so the key can
        // be rotated in the keychain without restarting the daemon and does not
        // sit in the process image between calls. Not a zeroization guarantee:
        // `Secret` wraps a `String` whose buffer is freed but not scrubbed, and
        // it is copied again into the `Authorization` header.
        let source = self.key_source.clone();
        let key = tokio::task::spawn_blocking(move || source.resolve(None))
            .await
            .map_err(|e| Error::internal(format!("key command task failed: {e}")))??
            .ok_or_else(|| {
                Error::unauthenticated("the voyage key command produced nothing".to_owned())
            })?;

        let mut out = Vec::with_capacity(texts.len());
        for chunk in request_chunks(texts) {
            out.extend(self.embed_batch(key.expose(), chunk).await?);
        }
        Ok(out)
    }

    async fn warm(&self) -> Result<(), Error> {
        // Not the default implementation: that embeds a document, which for
        // this backend means a billable third-party API call every time the
        // daemon starts. Resolving the key proves the part that is actually
        // worth proving at start-up — that the configured command works — and
        // there is no model to load.
        let source = self.key_source.clone();
        tokio::task::spawn_blocking(move || source.resolve(None))
            .await
            .map_err(|e| Error::internal(format!("key command task failed: {e}")))??;
        Ok(())
    }
}

/// The shape of a Voyage embeddings response.
#[derive(serde::Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingDatum>,
}

#[derive(serde::Deserialize)]
struct EmbeddingDatum {
    index: usize,
    embedding: Vec<f32>,
}

#[cfg(test)]
mod tests;
