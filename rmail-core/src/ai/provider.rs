//! [`Provider`] and its one implementation: the Anthropic Messages API.
//!
//! # Non-streaming and streaming share one retry path
//!
//! [`ClaudeProvider::send_with_retry`] is used by both [`Provider::complete`]
//! and [`Provider::stream`] — establishing the request is retried identically
//! either way. What differs is what happens *after* a successful response:
//! `complete` decodes one JSON body; `stream` hands the response to a
//! background task that decodes Server-Sent Events as they arrive. Once
//! streaming has started, nothing here retries — resuming a partial SSE
//! stream is not something this API supports, and re-sending the whole
//! request after tokens have already reached the caller would duplicate
//! output rather than recover it.
//!
//! # A `refusal` is not a `StopReason`
//!
//! `stop_reason: "refusal"` — the model's safety classifiers declining the
//! request — is deliberately not a member of [`StopReason`]. It is caught the
//! moment it is seen (in [`ChatResponse::from_raw`] for the non-streaming
//! path, in [`SseDecoder::decode_event`] for the streaming one) and turned
//! into an [`Error::FailedPrecondition`] instead. That makes it impossible to
//! represent a refusal as a `StopReason` a caller could match past, and keeps
//! it structurally invisible to [`ClaudeProvider::send_with_retry`] — a
//! refusal is decoded from a 200 response's body, long after the retry loop
//! has already decided that attempt succeeded, so there is no path by which
//! this codebase's retry policy could ever apply to one.
//!
//! # SSE decoding buffers on bytes, not on `str`
//!
//! [`SseDecoder`] looks for event boundaries (`\n\n`) in the raw byte buffer
//! before ever attempting a UTF-8 conversion. A chunk boundary from the
//! network can land anywhere, including mid multi-byte character; finding the
//! boundary in bytes and only decoding once a complete event has accumulated
//! means a split character is never mistaken for invalid input — it simply
//! has not been decoded yet.

use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::{Stream, StreamExt};
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use crate::config::{AiConfig, AiProvider, AiRetry};
use crate::credential::{CredentialSource, Secret};
use crate::error::Error;

/// Where requests go, unless a test points them somewhere else.
const DEFAULT_ENDPOINT: &str = "https://api.anthropic.com/v1/messages";

/// The Messages API version this client speaks.
///
/// `pub(crate)`, not private: `ai::queue`'s Message Batches client (task 47)
/// speaks the same API version over its own `reqwest::Client` and must not
/// let that copy drift from this one.
pub(crate) const ANTHROPIC_VERSION: &str = "2023-06-01";

// No `anthropic-beta` header: structured outputs (`output_config.format`) and
// the 1-hour prompt-cache `ttl` are both current, non-beta surface — neither
// carries a beta flag in Anthropic's published request shapes. If a future
// API revision re-guards either behind a beta header, requests using this
// module will start getting rejected rather than silently losing the
// feature, which is the direction to fail in.

/// Bounds only the TCP+TLS handshake, not the response — a streaming call can
/// legitimately run for minutes.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Applied only to non-streaming calls. Generous, because a deep-pass call
/// may think for a while before it answers; a streaming caller relies on
/// `cancel` instead (see the [`Provider`] trait docs).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// Most of an upstream error body to repeat back to a client; the rest goes
/// to the log. Unbounded would make a multi-megabyte body into a
/// multi-megabyte `grpc-message` trailer at the gRPC boundary.
const MAX_UPSTREAM_DETAIL: usize = 200;

/// Backpressure between the SSE reader task and its consumer: enough that a
/// burst of deltas does not thrash the channel, small enough that a consumer
/// which stops reading does not let the reader run far ahead of it.
const SSE_CHANNEL_CAPACITY: usize = 32;

/// The most undecoded bytes [`SseDecoder`] will hold while waiting for an
/// event boundary (`\n\n`) to arrive. A single real SSE event is a few
/// hundred bytes to a few KiB; a server that never sends `\n\n` — wedged,
/// misbehaving, or actively hostile — would otherwise grow this buffer
/// without limit.
const MAX_SSE_BUFFER: usize = 1024 * 1024;

// ---------------------------------------------------------------------------
// Request/response vocabulary
// ---------------------------------------------------------------------------

/// A conversation turn's speaker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// The human (or the mail being analyzed, folded into a user turn).
    User,
    /// A prior model response, for few-shot context or multi-turn history.
    Assistant,
}

impl Role {
    fn as_str(self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
        }
    }
}

/// One turn in a conversation.
#[derive(Debug, Clone)]
pub struct ChatMessage {
    /// Who said it.
    pub role: Role,
    /// The turn's text.
    pub content: String,
}

/// A JSON Schema the response must validate against.
///
/// Sent as `output_config.format` with `type: "json_schema"`, which
/// guarantees the response is valid JSON matching `schema` — the caller never
/// regexes model output to extract structure.
///
/// There is deliberately no `strict` field here. On the real Messages API,
/// `strict` is a property of *tool* definitions (`tools[].strict`), not of
/// `output_config.format` — structured output's validity guarantee is
/// unconditional once `output_config.format` is set, with no separate toggle
/// to send. A `strict` key on this request body would be a field the API
/// does not define.
#[derive(Debug, Clone)]
pub struct OutputFormat {
    /// The schema.
    pub schema: serde_json::Value,
}

impl OutputFormat {
    /// A structured-output constraint from a JSON Schema.
    #[must_use]
    pub fn json_schema(schema: serde_json::Value) -> Self {
        Self { schema }
    }
}

/// One call to [`Provider::complete`] or [`Provider::stream`].
#[derive(Debug, Clone)]
pub struct ChatRequest {
    /// The model id, e.g. `claude-haiku-4-5`.
    pub model: String,
    /// The hard cap on tokens the model may generate.
    pub max_tokens: u32,
    /// The system prompt.
    ///
    /// Keep this text identical across calls that should share a cache
    /// entry: prompt caching is a byte-identical-prefix match, and a frozen
    /// system prompt with `cache_control` (see [`ClaudeProvider::build_body`])
    /// is that prefix. If `output_format` is set, keep its schema stable too
    /// — it does not itself sit inside the `cache_control` boundary, but a
    /// request whose shape changes every call is not one caching helps
    /// regardless of which bytes are technically covered. Content that varies
    /// per call (the redacted body, prior summaries) belongs in `messages`,
    /// not here.
    pub system: Option<String>,
    /// The conversation so far, oldest first.
    pub messages: Vec<ChatMessage>,
    /// A JSON Schema the response must validate against.
    pub output_format: Option<OutputFormat>,
}

impl ChatRequest {
    /// A request for `model`, capped at `max_tokens` output tokens.
    #[must_use]
    pub fn new(model: impl Into<String>, max_tokens: u32) -> Self {
        Self {
            model: model.into(),
            max_tokens,
            system: None,
            messages: Vec::new(),
            output_format: None,
        }
    }

    /// Set the system prompt.
    #[must_use]
    pub fn system(mut self, system: impl Into<String>) -> Self {
        self.system = Some(system.into());
        self
    }

    /// Append a turn.
    #[must_use]
    pub fn message(mut self, role: Role, content: impl Into<String>) -> Self {
        self.messages.push(ChatMessage {
            role,
            content: content.into(),
        });
        self
    }

    /// Append a user turn.
    #[must_use]
    pub fn user(self, content: impl Into<String>) -> Self {
        self.message(Role::User, content)
    }

    /// Append an assistant turn.
    #[must_use]
    pub fn assistant(self, content: impl Into<String>) -> Self {
        self.message(Role::Assistant, content)
    }

    /// Constrain the response to a JSON Schema.
    #[must_use]
    pub fn output_format(mut self, format: OutputFormat) -> Self {
        self.output_format = Some(format);
        self
    }
}

/// Why the model stopped generating.
///
/// `refusal` is deliberately absent — see the module docs. By the time a
/// caller sees a `StopReason` at all, the turn succeeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The model finished naturally.
    EndTurn,
    /// Hit the `max_tokens` cap.
    MaxTokens,
    /// Hit a configured stop sequence.
    StopSequence,
    /// The model wants to call a tool.
    ToolUse,
    /// A server-side tool loop paused; resend to continue.
    PauseTurn,
}

/// Token accounting for one request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    /// Tokens processed at full price (neither served from nor written to
    /// the prompt cache).
    pub input_tokens: u32,
    /// Tokens generated.
    pub output_tokens: u32,
    /// Tokens written to the prompt cache this request (billed once, at a
    /// premium).
    pub cache_creation_input_tokens: u32,
    /// Tokens served from the prompt cache — the field prompt-caching tests
    /// verify is non-zero on a warm request.
    pub cache_read_input_tokens: u32,
}

/// One decoded event from [`Provider::stream`].
#[derive(Debug, Clone, PartialEq)]
pub enum StreamFrame {
    /// A slice of assistant-visible text, in arrival order. Concatenating
    /// every `Token` in a stream reproduces the final text.
    Token(String),
    /// A tool-use content block began.
    ///
    /// `ChatRequest` has no `tools` field yet, so nothing this provider can
    /// build today gives the model a reason to emit one of these — the
    /// variant exists so the frame vocabulary is complete for whichever later
    /// task adds tool definitions to the request side, and so this decoder
    /// does not need to change shape when it does.
    ToolUseStart {
        /// The tool call's id, echoed back in the eventual `tool_result`.
        id: String,
        /// The tool being called.
        name: String,
    },
    /// Final token accounting for the turn. Always the second-to-last frame
    /// a successful stream produces.
    Usage(Usage),
    /// The turn is complete. Always the last frame a successful stream
    /// produces.
    Done {
        /// Why generation stopped.
        stop_reason: StopReason,
    },
}

/// A live [`Provider::stream`] response.
pub type ProviderStream = Pin<Box<dyn Stream<Item = Result<StreamFrame, Error>> + Send>>;

// ---------------------------------------------------------------------------
// The trait
// ---------------------------------------------------------------------------

/// A backend that turns a [`ChatRequest`] into a model response.
///
/// # `cancel` covers both cancellation and deadlines
///
/// There is no separate deadline parameter. A caller with a deadline
/// expresses it the way the rest of this codebase does — a
/// [`CancellationToken`] cancelled by a timer, alongside whatever cancels it
/// explicitly (see `sync::idle`'s `sleep_or_cancel` for the established
/// pattern). That keeps deadline policy where it belongs — with the caller,
/// ultimately the gRPC request's own deadline — rather than baked into every
/// backend.
///
/// Implementations must be cheap to clone or share behind an [`Arc`]: one
/// provider serves the whole process.
#[async_trait]
pub trait Provider: Send + Sync + std::fmt::Debug {
    /// Send a request and wait for the complete response.
    ///
    /// # Errors
    ///
    /// Backend-specific: an unreachable network, an upstream 4xx/5xx after
    /// retries are exhausted, or the provider declining the request
    /// (`refusal` — never retried; see the module docs).
    async fn complete(
        &self,
        request: &ChatRequest,
        cancel: &CancellationToken,
    ) -> Result<ChatResponse, Error>;

    /// Send a request and stream the response as it is generated.
    ///
    /// The returned stream ends with a [`StreamFrame::Done`] on success or an
    /// `Err` — never both, and never neither: a stream that just stops
    /// without producing either means the connection dropped mid-turn, which
    /// this trait surfaces as an `Err` rather than a silent truncation.
    ///
    /// # Errors
    ///
    /// Whatever [`Provider::complete`] would return, for failures discovered
    /// before the stream opens. Failures discovered mid-stream arrive as an
    /// `Err` item on the stream itself, not from this method.
    async fn stream(
        &self,
        request: &ChatRequest,
        cancel: &CancellationToken,
    ) -> Result<ProviderStream, Error>;
}

/// Build the configured provider.
///
/// # Errors
///
/// [`Error::FailedPrecondition`] if the configured backend cannot be built at
/// all — no `api_key_command`, or the local backend, which is not
/// implemented yet — caught here, at daemon start, rather than on the first
/// AI call hours later.
pub fn build(config: &AiConfig) -> Result<Arc<dyn Provider>, Error> {
    match config.provider {
        AiProvider::Claude => Ok(Arc::new(ClaudeProvider::new(config)?)),
        AiProvider::Local => Err(Error::failed_precondition(
            "the local AI provider is not implemented yet; set `ai.provider = \"claude\"` \
             or `ai.enabled = false`"
                .to_owned(),
        )),
    }
}

// ---------------------------------------------------------------------------
// The Claude backend
// ---------------------------------------------------------------------------

/// A completed, non-streaming turn.
#[derive(Debug, Clone)]
pub struct ChatResponse {
    /// The provider's id for this response, for correlation in logs/audit.
    pub id: String,
    /// The model that actually produced it (may differ from the request's
    /// alias-resolved model id).
    pub model: String,
    /// Why generation stopped.
    pub stop_reason: StopReason,
    /// Every text content block, concatenated in order.
    pub text: String,
    /// Token accounting, including cache activity.
    pub usage: Usage,
}

impl ChatResponse {
    /// `pub(crate)`: `ai::queue`'s Message Batches result path (task 47)
    /// decodes a batch result's `message` field — the same non-streaming
    /// `Message` object shape a live `POST /v1/messages` response uses —
    /// through this exact conversion, so refusal handling, stop-reason
    /// parsing, and usage accounting stay in one place rather than a second
    /// copy that could drift from this one.
    pub(crate) fn from_raw(raw: RawMessage) -> Result<Self, Error> {
        let raw_stop_reason = raw.stop_reason.as_deref().unwrap_or("end_turn");
        if raw_stop_reason == "refusal" {
            return Err(refusal_error(
                raw.stop_details
                    .as_ref()
                    .and_then(|d| d.category.as_deref()),
                raw.stop_details
                    .as_ref()
                    .and_then(|d| d.explanation.as_deref()),
            ));
        }
        let stop_reason = parse_stop_reason(raw_stop_reason)?;
        let text = raw
            .content
            .into_iter()
            .filter_map(|block| match block {
                RawContentBlock::Text { text } => Some(text),
                RawContentBlock::Other => None,
            })
            .collect::<Vec<_>>()
            .join("");
        Ok(Self {
            id: raw.id,
            model: raw.model,
            stop_reason,
            text,
            usage: Usage::from(raw.usage),
        })
    }

    /// Parse [`ChatResponse::text`] as JSON.
    ///
    /// Only meaningful when the request carried an [`OutputFormat`]: that is
    /// what makes this safe to do without regex or ad-hoc extraction, since
    /// the API guarantees the text is valid JSON matching the schema. A
    /// parse failure here means the provider's structured-output contract
    /// broke, not that the caller did anything wrong.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] if [`ChatResponse::text`] is not valid JSON for
    /// `T`.
    pub fn structured<T: serde::de::DeserializeOwned>(&self) -> Result<T, Error> {
        serde_json::from_str(&self.text).map_err(|e| {
            Error::internal(format!(
                "claude's structured output did not match the requested schema: {e}"
            ))
        })
    }
}

/// The Claude-backed [`Provider`]: the Anthropic Messages API over
/// `reqwest` + `rustls`.
#[derive(Debug)]
pub struct ClaudeProvider {
    endpoint: String,
    key_source: CredentialSource,
    client: reqwest::Client,
    retry: AiRetry,
    prompt_cache_enabled: bool,
    cache_ttl: Duration,
}

impl ClaudeProvider {
    /// A provider for the configured account.
    ///
    /// # Errors
    ///
    /// [`Error::FailedPrecondition`] if the HTTP client cannot be built or
    /// the configuration names no key command — caught here rather than at
    /// first call, so a misconfigured daemon fails at start, where somebody
    /// is watching.
    pub fn new(config: &AiConfig) -> Result<Self, Error> {
        if config.api_key_command.trim().is_empty() {
            return Err(Error::failed_precondition(
                "the claude provider needs `ai.api_key_command`; the key is read from a \
                 command's output and is never stored in the config file"
                    .to_owned(),
            ));
        }
        // As in the IMAP client and the Voyage embedder: the provider is
        // chosen explicitly rather than inferred from crate features, because
        // inference fails at runtime on the first handshake once anything
        // pulls in a second provider.
        crate::transport::install_crypto_provider();
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|e| {
                Error::failed_precondition(format!("could not build an HTTP client: {e}"))
            })?;
        Ok(Self {
            endpoint: DEFAULT_ENDPOINT.to_owned(),
            key_source: CredentialSource::Command(config.api_key_command.clone()),
            client,
            retry: config.retry.clone(),
            prompt_cache_enabled: config.prompt_cache.enabled,
            cache_ttl: config.prompt_cache.ttl.as_duration(),
        })
    }

    /// Point this provider at another endpoint.
    ///
    /// Exists so the tests can drive a local server. Nothing in production
    /// calls it, and the default is the real API.
    #[must_use]
    pub fn with_endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = endpoint.into();
        self
    }

    /// Resolved once per call rather than held in the struct, so the key can
    /// be rotated in the keychain without restarting the daemon and does not
    /// sit in the process image between calls.
    async fn resolve_key(&self) -> Result<Secret, Error> {
        let source = self.key_source.clone();
        tokio::task::spawn_blocking(move || source.resolve(None))
            .await
            .map_err(|e| Error::internal(format!("credential command task failed: {e}")))??
            .ok_or_else(|| {
                Error::unauthenticated("the claude api_key_command produced nothing".to_owned())
            })
    }

    /// Build the request body. `system` carries `cache_control` when prompt
    /// caching is enabled, so a frozen system prompt plus a stable
    /// `output_config.format` schema forms the cached prefix the module docs
    /// describe — everything that varies per call belongs in `messages`.
    fn build_body(&self, request: &ChatRequest, stream: bool) -> serde_json::Value {
        let messages: Vec<serde_json::Value> = request
            .messages
            .iter()
            .map(|m| serde_json::json!({ "role": m.role.as_str(), "content": m.content }))
            .collect();
        let mut body = serde_json::json!({
            "model": request.model,
            "max_tokens": request.max_tokens,
            "messages": messages,
            "stream": stream,
        });
        if let Some(system) = &request.system {
            body["system"] = if self.prompt_cache_enabled {
                serde_json::json!([{
                    "type": "text",
                    "text": system,
                    "cache_control": {
                        "type": "ephemeral",
                        "ttl": cache_ttl_str(self.cache_ttl),
                    },
                }])
            } else {
                serde_json::Value::String(system.clone())
            };
        }
        if let Some(format) = &request.output_format {
            body["output_config"] = serde_json::json!({
                "format": { "type": "json_schema", "schema": format.schema },
            });
        }
        body
    }

    /// Send `body`, retrying retryable failures with backoff. Returns the
    /// final HTTP response whether or not it is a success — the caller maps
    /// a non-success status to an [`Error`], since that differs between a
    /// JSON body ([`Provider::complete`]) and a byte stream
    /// ([`Provider::stream`]) only in how the body is read afterward, not in
    /// the retry policy applied to getting a response at all.
    async fn send_with_retry(
        &self,
        key: &str,
        body: &serde_json::Value,
        per_request_timeout: Option<Duration>,
        cancel: &CancellationToken,
    ) -> Result<reqwest::Response, Error> {
        let mut attempt: u32 = 0;
        loop {
            attempt += 1;
            let mut req = self
                .client
                .post(&self.endpoint)
                .header("x-api-key", key)
                .header("anthropic-version", ANTHROPIC_VERSION)
                .json(body);
            if let Some(timeout) = per_request_timeout {
                req = req.timeout(timeout);
            }
            let sent = tokio::select! {
                () = cancel.cancelled() => {
                    return Err(Error::deadline_exceeded(
                        "the claude request was cancelled before it completed".to_owned(),
                    ));
                }
                result = req.send() => result,
            };
            match sent {
                Ok(response) if response.status().is_success() => return Ok(response),
                Ok(response)
                    if !is_retryable_status(response.status())
                        || attempt >= self.retry.max_attempts =>
                {
                    return Ok(response);
                }
                Ok(response) => {
                    tracing::warn!(
                        status = %response.status(),
                        attempt,
                        "claude request failed, retrying"
                    );
                }
                Err(e) if attempt >= self.retry.max_attempts => {
                    return Err(Error::unavailable(format!("claude request failed: {e}")));
                }
                Err(e) => {
                    tracing::warn!(error = %e, attempt, "claude request failed, retrying");
                }
            }
            let delay = backoff_delay(&self.retry, attempt);
            if wait_or_cancel(delay, cancel).await.is_cancelled() {
                return Err(Error::deadline_exceeded(
                    "the claude request was cancelled while waiting to retry".to_owned(),
                ));
            }
        }
    }
}

#[async_trait]
impl Provider for ClaudeProvider {
    #[tracing::instrument(skip(self, request, cancel), fields(model = %request.model))]
    async fn complete(
        &self,
        request: &ChatRequest,
        cancel: &CancellationToken,
    ) -> Result<ChatResponse, Error> {
        validate(request)?;
        // Cheap up front: skips paying for a `spawn_blocking` credential
        // command (up to `credential::COMMAND_TIMEOUT`) on a call that was
        // already given up on before it started.
        if cancel.is_cancelled() {
            return Err(Error::deadline_exceeded(
                "the claude request was already cancelled".to_owned(),
            ));
        }
        let key = self.resolve_key().await?;
        let body = self.build_body(request, false);
        let response = self
            .send_with_retry(key.expose(), &body, Some(REQUEST_TIMEOUT), cancel)
            .await?;
        let status = response.status();
        if !status.is_success() {
            return Err(read_http_error(status, response, cancel).await);
        }
        let raw: RawMessage = response.json().await.map_err(|e| {
            tracing::warn!(error = %e, "could not read the claude response");
            Error::unavailable("the claude response could not be read".to_owned())
        })?;
        let response = ChatResponse::from_raw(raw)?;
        tracing::debug!(
            stop_reason = ?response.stop_reason,
            input_tokens = response.usage.input_tokens,
            output_tokens = response.usage.output_tokens,
            cache_read_input_tokens = response.usage.cache_read_input_tokens,
            "claude completion received"
        );
        Ok(response)
    }

    #[tracing::instrument(skip(self, request, cancel), fields(model = %request.model))]
    async fn stream(
        &self,
        request: &ChatRequest,
        cancel: &CancellationToken,
    ) -> Result<ProviderStream, Error> {
        validate(request)?;
        if cancel.is_cancelled() {
            return Err(Error::deadline_exceeded(
                "the claude request was already cancelled".to_owned(),
            ));
        }
        let key = self.resolve_key().await?;
        let body = self.build_body(request, true);
        let response = self
            .send_with_retry(key.expose(), &body, None, cancel)
            .await?;
        let status = response.status();
        if !status.is_success() {
            return Err(read_http_error(status, response, cancel).await);
        }
        Ok(spawn_sse_reader(response, cancel.clone()))
    }
}

/// Read a non-success response's body as the detail for the [`Error`] it
/// becomes, bounded by `cancel` — a server that sends a status line and then
/// stalls must not hang this past the point the caller gave up on the whole
/// call.
async fn read_http_error(
    status: reqwest::StatusCode,
    response: reqwest::Response,
    cancel: &CancellationToken,
) -> Error {
    let body_text = tokio::select! {
        () = cancel.cancelled() => {
            return Error::deadline_exceeded(
                "the claude request was cancelled while reading the error response".to_owned(),
            );
        }
        body = response.text() => body.unwrap_or_else(|e| {
            tracing::warn!(error = %e, "could not read the claude error response body");
            String::new()
        }),
    };
    map_http_error(status, &body_text)
}

/// Reject an obviously-malformed request before it costs a network round
/// trip or a credential-command invocation.
fn validate(request: &ChatRequest) -> Result<(), Error> {
    if request.model.trim().is_empty() {
        return Err(Error::invalid_argument(
            "a chat request needs a model".to_owned(),
        ));
    }
    if request.max_tokens == 0 {
        return Err(Error::invalid_argument(
            "max_tokens must be greater than zero".to_owned(),
        ));
    }
    if request.messages.is_empty() {
        return Err(Error::invalid_argument(
            "a chat request needs at least one message".to_owned(),
        ));
    }
    Ok(())
}

/// 408 and 409 join 429 and 5xx here to match the retry classification the
/// official Anthropic SDKs apply by default (their `max_retries` covers
/// "408/409/429/5xx + connection errors") — a locally-reimplemented retry
/// policy should not be pickier than the one the API's own clients ship.
fn is_retryable_status(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 409 | 429) || status.is_server_error()
}

/// Map a final (non-retryable, or retries-exhausted) HTTP failure to an
/// [`Error`]. Distinguished by status so a caller can tell "fix your key"
/// from "try again later" rather than getting one error for both.
fn map_http_error(status: reqwest::StatusCode, body: &str) -> Error {
    let detail = clip(body);
    match status.as_u16() {
        401 | 403 => {
            Error::unauthenticated(format!("claude rejected the API key ({status}): {detail}"))
        }
        // Reached only once retries are exhausted — see `is_retryable_status`
        // and `send_with_retry`. Transient by nature even after giving up,
        // so `Unavailable` rather than `InvalidArgument`.
        408 | 409 | 429 => Error::unavailable(format!(
            "claude request still failing after retries: {detail}"
        )),
        400..=499 => {
            Error::invalid_argument(format!("claude rejected the request ({status}): {detail}"))
        }
        _ => Error::unavailable(format!("claude returned {status}: {detail}")),
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

/// Anthropic's `cache_control.ttl` accepts exactly two values. A configured
/// TTL below the one-hour threshold rounds down to the shorter of the two
/// rather than being rejected — a config typo should degrade the cache's
/// lifetime, not the provider's ability to start.
fn cache_ttl_str(ttl: Duration) -> &'static str {
    if ttl >= Duration::from_secs(3600) {
        "1h"
    } else {
        "5m"
    }
}

/// The delay before retry attempt `attempt` (1-indexed): exponential off
/// `base_delay_ms`, capped at `max_delay_ms`, then jittered.
fn backoff_delay(retry: &AiRetry, attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(20);
    let base_ms = retry
        .base_delay_ms
        .saturating_mul(1u64.checked_shl(shift).unwrap_or(u64::MAX));
    let capped_ms = base_ms.min(retry.max_delay_ms);
    // `as` casts, not `try_from`: this is a millisecond delay, not a value
    // whose exact precision matters, and none of `clippy::cast_precision_loss`
    // / `cast_possible_truncation` / `cast_sign_loss` are enabled for this
    // workspace (only the pedantic group carries them) — no `#[allow]` needed.
    let jittered_ms = (capped_ms as f64 * jitter_multiplier()) as u64;
    Duration::from_millis(jittered_ms.max(1))
}

/// A multiplier in `[0.5, 1.0)` for backoff jitter.
///
/// Full jitter (`[0, cap]`) risks two callers who both hit the cap rolling
/// near zero and retrying together — the thundering herd this exists to
/// avoid. Equal jitter keeps a floor at half the delay instead.
///
/// The randomness comes from the wall clock's sub-second component mixed
/// with a per-process call counter, not the `rand` crate: nothing here needs
/// to resist an adversary, only avoid synchronized retries, and a dependency
/// for one number per retry is not worth adding.
fn jitter_multiplier() -> f64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = u64::from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0),
    );
    // A golden-ratio multiplicative mix so consecutive counter values do not
    // produce visibly consecutive fractions.
    let mixed = nanos ^ counter.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    let frac = (mixed % 1_000_000) as f64 / 1_000_000.0;
    0.5 + 0.5 * frac
}

/// Whether a wait ended in cancellation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Waited {
    Elapsed,
    Cancelled,
}

impl Waited {
    fn is_cancelled(self) -> bool {
        self == Self::Cancelled
    }
}

/// Sleep for `duration`, returning early if `cancel` fires.
async fn wait_or_cancel(duration: Duration, cancel: &CancellationToken) -> Waited {
    tokio::select! {
        () = tokio::time::sleep(duration) => Waited::Elapsed,
        () = cancel.cancelled() => Waited::Cancelled,
    }
}

/// Hand a successful streaming response to a background task that decodes it
/// and forwards frames over a channel, and wrap the receiving end as the
/// [`ProviderStream`] the caller sees.
///
/// Every wait in the reader loop — for more bytes, and for channel capacity —
/// is raced against both `cancel` and the consumer disappearing (`tx.closed`),
/// so cancellation is honored even when the connection is idle (nothing to
/// read, so `bytes.next()` would otherwise never resolve) and even when the
/// consumer has stopped polling (the channel is full, so a plain `send` would
/// otherwise block forever). Either way the task returns and drops the
/// [`reqwest::Response`], which closes the underlying connection — this is
/// what "upstream abort on cancel/deadline" means in practice.
fn spawn_sse_reader(response: reqwest::Response, cancel: CancellationToken) -> ProviderStream {
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<StreamFrame, Error>>(SSE_CHANNEL_CAPACITY);
    tokio::spawn(
        async move {
            let mut decoder = SseDecoder::new();
            let mut bytes = Box::pin(response.bytes_stream());
            loop {
                let next = tokio::select! {
                    () = cancel.cancelled() => {
                        notify_cancelled(&tx);
                        return;
                    }
                    () = tx.closed() => return,
                    chunk = bytes.next() => chunk,
                };
                let Some(chunk) = next else {
                    // Upstream closed the connection. If `message_stop` was
                    // never seen, that is a truncated stream, not a clean
                    // end, and must not be silently reported as one. Best
                    // effort: if the channel is full the consumer already has
                    // frames queued and will see the stream simply end.
                    if !decoder.finished {
                        let _ = tx.try_send(Err(Error::unavailable(
                            "claude closed the stream before it finished".to_owned(),
                        )));
                    }
                    return;
                };
                let chunk = match chunk {
                    Ok(chunk) => chunk,
                    Err(e) => {
                        let _ = tx.try_send(Err(Error::unavailable(format!(
                            "claude stream read failed: {e}"
                        ))));
                        return;
                    }
                };
                for frame in decoder.push(&chunk) {
                    let terminal = matches!(frame, Ok(StreamFrame::Done { .. }) | Err(_));
                    let sent = tokio::select! {
                        () = cancel.cancelled() => {
                            notify_cancelled(&tx);
                            return;
                        }
                        result = tx.send(frame) => result,
                    };
                    if sent.is_err() {
                        // The consumer dropped the stream — its own
                        // cancellation path already tore it down — so there
                        // is nothing left to read for; stop rather than keep
                        // draining upstream.
                        return;
                    }
                    if terminal {
                        return;
                    }
                }
            }
        }
        .instrument(tracing::Span::current()),
    );
    Box::pin(ReceiverStream::new(rx))
}

/// Tell the consumer a cancellation cut the stream short, so it sees an `Err`
/// rather than a stream that just stops — the same "`Done` or `Err`, never
/// neither" contract [`Provider::stream`] documents for every other case.
///
/// `try_send`, not `send().await`: the reader is already tearing down because
/// of the cancellation this call is reporting, so waiting for channel
/// capacity here would reintroduce the exact "blocked past a cancel" failure
/// mode this function exists to close off. If the channel happens to be full,
/// the consumer still learns the stream was cut short — just from the
/// channel closing with no further frames, rather than from this message.
fn notify_cancelled(tx: &tokio::sync::mpsc::Sender<Result<StreamFrame, Error>>) {
    let _ = tx.try_send(Err(Error::deadline_exceeded(
        "the claude stream was cancelled before it finished".to_owned(),
    )));
}

/// Incrementally decodes Anthropic's SSE stream into [`StreamFrame`]s.
///
/// Bytes arrive from the network in arbitrary chunks — one read might carry
/// half an event or a dozen. This buffers until a full `\n\n`-delimited event
/// is available and only then decodes it, so a chunk boundary landing
/// mid-JSON never produces a parse error for content that simply has not
/// fully arrived yet.
struct SseDecoder {
    buffer: Vec<u8>,
    usage: Usage,
    stop_reason: Option<StopReason>,
    /// Set once a [`StreamFrame::Done`] or an error has been produced. No
    /// further bytes are decoded after that — a well-behaved server closes
    /// the connection at that point anyway, and a misbehaving one that kept
    /// sending must not resurrect a stream the caller has already seen end.
    finished: bool,
}

impl SseDecoder {
    fn new() -> Self {
        Self {
            buffer: Vec::new(),
            usage: Usage::default(),
            stop_reason: None,
            finished: false,
        }
    }

    /// Feed newly received bytes and return every frame the newly complete
    /// event(s) produce, in order.
    fn push(&mut self, bytes: &[u8]) -> Vec<Result<StreamFrame, Error>> {
        // `\r` is dropped rather than treated as part of a `\r\n\r\n`
        // boundary some SSE servers use: no JSON payload this API sends ever
        // contains a raw carriage return (control characters inside a JSON
        // string must be escaped), so stripping it up front lets the rest of
        // this decoder look for a plain `\n\n` regardless of which line
        // ending the server used.
        self.buffer
            .extend(bytes.iter().copied().filter(|&b| b != b'\r'));
        let mut out = Vec::new();
        if !self.finished && self.buffer.len() > MAX_SSE_BUFFER {
            out.push(Err(Error::unavailable(format!(
                "claude sent {} bytes without a complete stream event (limit {MAX_SSE_BUFFER})",
                self.buffer.len()
            ))));
            self.finished = true;
            self.buffer.clear();
            return out;
        }
        while !self.finished {
            let Some(boundary) = self.buffer.windows(2).position(|w| w == b"\n\n") else {
                break;
            };
            let event = self.buffer[..boundary].to_vec();
            self.buffer.drain(..boundary + 2);
            match self.decode_event(&event) {
                Ok(frames) => {
                    for frame in frames {
                        let terminal = matches!(frame, StreamFrame::Done { .. });
                        out.push(Ok(frame));
                        if terminal {
                            self.finished = true;
                        }
                    }
                }
                Err(e) => {
                    out.push(Err(e));
                    self.finished = true;
                }
            }
        }
        out
    }

    /// Decode one complete `\n\n`-delimited SSE event into zero or more
    /// frames.
    ///
    /// Zero: most event types (`ping`, `content_block_stop`, thinking
    /// deltas) carry nothing this trait's four-frame vocabulary represents.
    /// Two: `message_stop` is where usage and the stop reason — both known
    /// since the `message_delta` a moment earlier — are actually emitted, so
    /// one wire event produces both a `Usage` and a `Done` frame.
    fn decode_event(&mut self, event: &[u8]) -> Result<Vec<StreamFrame>, Error> {
        let text = std::str::from_utf8(event)
            .map_err(|_| Error::unavailable("claude sent a non-UTF-8 stream event".to_owned()))?;
        let data: String = text
            .lines()
            .filter_map(|line| line.strip_prefix("data:").map(str::trim_start))
            .collect::<Vec<_>>()
            .join("\n");
        if data.is_empty() {
            // A comment line, an `event:` line with no `data:`, or a
            // keepalive.
            return Ok(Vec::new());
        }
        let value: serde_json::Value = serde_json::from_str(&data).map_err(|e| {
            Error::unavailable(format!("claude sent an unparseable stream event: {e}"))
        })?;
        match value.get("type").and_then(serde_json::Value::as_str) {
            Some("message_start") => {
                if let Some(usage) = value.pointer("/message/usage") {
                    self.usage = merge_initial_usage(self.usage, usage);
                }
                Ok(Vec::new())
            }
            Some("content_block_start") => {
                if value
                    .pointer("/content_block/type")
                    .and_then(serde_json::Value::as_str)
                    == Some("tool_use")
                {
                    // No fallback to an empty string: an id is how the
                    // eventual `tool_result` gets correlated back to this
                    // call, and a `ToolUseStart` with an empty id is not a
                    // degraded-but-usable frame, it is one nothing downstream
                    // could ever match up.
                    let id = value
                        .pointer("/content_block/id")
                        .and_then(serde_json::Value::as_str)
                        .ok_or_else(|| {
                            Error::unavailable(
                                "claude started a tool_use block with no id".to_owned(),
                            )
                        })?
                        .to_owned();
                    let name = value
                        .pointer("/content_block/name")
                        .and_then(serde_json::Value::as_str)
                        .ok_or_else(|| {
                            Error::unavailable(
                                "claude started a tool_use block with no name".to_owned(),
                            )
                        })?
                        .to_owned();
                    Ok(vec![StreamFrame::ToolUseStart { id, name }])
                } else {
                    Ok(Vec::new())
                }
            }
            Some("content_block_delta") => {
                if value
                    .pointer("/delta/type")
                    .and_then(serde_json::Value::as_str)
                    == Some("text_delta")
                {
                    let text = value
                        .pointer("/delta/text")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    Ok(vec![StreamFrame::Token(text)])
                } else {
                    // `thinking_delta`, `input_json_delta`, `citations_delta`:
                    // not part of this trait's frame vocabulary yet. Ignored
                    // rather than treated as an error, so this decoder stays
                    // forward compatible with delta kinds the API adds later.
                    Ok(Vec::new())
                }
            }
            Some("message_delta") => {
                if let Some(output_tokens) = value
                    .pointer("/usage/output_tokens")
                    .and_then(serde_json::Value::as_u64)
                {
                    self.usage.output_tokens = u32::try_from(output_tokens).unwrap_or(u32::MAX);
                }
                if let Some(raw_stop_reason) = value
                    .pointer("/delta/stop_reason")
                    .and_then(serde_json::Value::as_str)
                {
                    if raw_stop_reason == "refusal" {
                        let category = value
                            .pointer("/delta/stop_details/category")
                            .and_then(serde_json::Value::as_str);
                        let explanation = value
                            .pointer("/delta/stop_details/explanation")
                            .and_then(serde_json::Value::as_str);
                        return Err(refusal_error(category, explanation));
                    }
                    self.stop_reason = Some(parse_stop_reason(raw_stop_reason)?);
                }
                Ok(Vec::new())
            }
            Some("message_stop") => {
                let stop_reason = self.stop_reason.take().ok_or_else(|| {
                    Error::unavailable(
                        "claude ended the stream without ever sending a stop reason".to_owned(),
                    )
                })?;
                Ok(vec![
                    StreamFrame::Usage(self.usage),
                    StreamFrame::Done { stop_reason },
                ])
            }
            Some("error") => {
                let message = value
                    .pointer("/error/message")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("no further detail")
                    .to_owned();
                Err(Error::unavailable(format!(
                    "claude stream error: {message}"
                )))
            }
            // `ping`, `content_block_stop`, and anything this build does not
            // know about yet: no frame, and specifically not an error — a
            // future event type must not fail every stream sent through an
            // older build.
            _ => Ok(Vec::new()),
        }
    }
}

/// Read the input/cache token counts off `message_start.message.usage`.
/// `output_tokens` is left alone — it starts at zero there and is filled in
/// by `message_delta` as generation proceeds.
fn merge_initial_usage(mut usage: Usage, raw: &serde_json::Value) -> Usage {
    let get = |field: &str| {
        raw.get(field)
            .and_then(serde_json::Value::as_u64)
            .and_then(|v| u32::try_from(v).ok())
            .unwrap_or(0)
    };
    usage.input_tokens = get("input_tokens");
    usage.cache_creation_input_tokens = get("cache_creation_input_tokens");
    usage.cache_read_input_tokens = get("cache_read_input_tokens");
    usage
}

/// Build the error a `refusal` becomes.
///
/// Not [`Error::Unavailable`]: a refusal is a definite, deliberate answer
/// from a reachable provider, not a transient fault. Using the same reason
/// this codebase uses for other non-retryable, provider-declined states
/// keeps refusal handling consistent with the rest of the error taxonomy
/// rather than inventing a one-off category for it.
fn refusal_error(category: Option<&str>, explanation: Option<&str>) -> Error {
    let detail = match (category, explanation) {
        (Some(c), Some(e)) => format!("category={c}: {e}"),
        (Some(c), None) => format!("category={c}"),
        (None, Some(e)) => e.to_owned(),
        (None, None) => "no further detail".to_owned(),
    };
    Error::failed_precondition(format!("claude declined the request ({detail})"))
}

/// Parse a wire `stop_reason` string. `"refusal"` is handled by callers
/// before reaching here (see the module docs) — this function only ever sees
/// the values that mean the turn succeeded.
///
/// # Errors
///
/// [`Error::Internal`]: a `stop_reason` this build does not know about is a
/// genuine API-drift surprise worth surfacing loudly, the same treatment
/// `EventKind::parse` gives an unrecognized wire string.
fn parse_stop_reason(raw: &str) -> Result<StopReason, Error> {
    match raw {
        "end_turn" => Ok(StopReason::EndTurn),
        "max_tokens" => Ok(StopReason::MaxTokens),
        "stop_sequence" => Ok(StopReason::StopSequence),
        "tool_use" => Ok(StopReason::ToolUse),
        "pause_turn" => Ok(StopReason::PauseTurn),
        other => Err(Error::internal(format!(
            "claude returned an unrecognized stop_reason {other:?}"
        ))),
    }
}

// ---------------------------------------------------------------------------
// Wire shapes (non-streaming)
// ---------------------------------------------------------------------------

/// `pub(crate)`: named directly by `ai::queue`'s batch-result decoding (see
/// [`ChatResponse::from_raw`]) so a batch result's `message` field is parsed
/// by the identical `Deserialize` impl a live response uses, not a
/// hand-rolled second copy of this wire shape.
#[derive(Debug, serde::Deserialize)]
pub(crate) struct RawMessage {
    id: String,
    model: String,
    #[serde(default)]
    content: Vec<RawContentBlock>,
    stop_reason: Option<String>,
    #[serde(default)]
    stop_details: Option<RawStopDetails>,
    #[serde(default)]
    usage: RawUsage,
}

#[derive(Debug, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RawContentBlock {
    Text {
        text: String,
    },
    /// `tool_use`, `thinking`, and anything future: not part of a
    /// non-streaming [`ChatResponse`]'s text today (tool blocks are a
    /// streaming-only concept for this trait — see [`StreamFrame::ToolUseStart`]).
    #[serde(other)]
    Other,
}

#[derive(Debug, Default, serde::Deserialize)]
struct RawStopDetails {
    category: Option<String>,
    explanation: Option<String>,
}

#[derive(Debug, Default, serde::Deserialize)]
struct RawUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_creation_input_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}

impl From<RawUsage> for Usage {
    fn from(raw: RawUsage) -> Self {
        Self {
            input_tokens: u32::try_from(raw.input_tokens).unwrap_or(u32::MAX),
            output_tokens: u32::try_from(raw.output_tokens).unwrap_or(u32::MAX),
            cache_creation_input_tokens: u32::try_from(raw.cache_creation_input_tokens)
                .unwrap_or(u32::MAX),
            cache_read_input_tokens: u32::try_from(raw.cache_read_input_tokens).unwrap_or(u32::MAX),
        }
    }
}

#[cfg(test)]
mod tests;
