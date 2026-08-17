//! The local-only model path: inference that cannot leave the machine.
//!
//! # What this module is actually for
//!
//! Wiring an on-device backend is the easy half. The half that carries the
//! weight is the *guarantee*: that a mailbox an operator marked local-only has
//! no route to a network provider, and cannot grow one by accident. This
//! module is built around three separate mechanisms for that, of decreasing
//! strength, and it is worth being precise about which is which — a guarantee
//! is only worth what enforces it.
//!
//! **1. Configuration-level, structural.** `ai.provider = "local"` means
//! [`crate::ai::provider::build`] constructs a [`LocalProvider`] and *never
//! calls* `ClaudeProvider::new`. There is no HTTP client, no endpoint, and no
//! API key for AI generation anywhere in the process: the daemon's one shared
//! `Arc<dyn Provider>` *is* the local backend, so every existing caller —
//! every pass, every RPC, every queue worker — inherits the property without a
//! line of code changing and without any of them checking anything.
//! `local_configuration_builds_no_network_client` is the probe: it builds with
//! an `api_key_command` that fails, and a build that succeeds anyway is proof
//! the Claude arm was not taken.
//!
//! The Message Batches client ([`crate::ai::queue::BatchClient`]) is a
//! *second*, independent egress — its own HTTP client, its own key resolution,
//! reached without an `Arc<dyn Provider>` at all — so `provider::build`'s arm
//! does not cover it. [`hosted_clients_permitted`] is the one predicate the
//! daemon's wiring consults before constructing it, and mechanism 3 checks
//! that the wiring still does.
//!
//! **2. Dispatch-level, on the actual path.** [`resolve_egress`] combines the
//! policy decision, the per-account override and the daemon default into one
//! [`Egress`], and [`crate::ai::queue::AiWorkerPool`] calls it for every job
//! before it picks a provider. This is what makes "forced by policy for
//! local-only mail" real rather than aspirational: a `local_only` folder on a
//! daemon that has a local backend is *served on-device*, not refused, which
//! is the behavior the requirement actually asks for. It is a runtime check,
//! not a structural one — stated plainly because the difference matters.
//!
//! **3. Source-level, checked.** `tests.rs`'s
//! `every_network_capable_client_is_built_in_a_listed_file` walks the whole
//! workspace and fails **by name** if any file outside a short, reasoned list
//! constructs a network client — and each listed file must still contain the
//! guard token its row claims gates it, so removing the guard fails the test
//! too. This is the same shape as [`crate::ai::injection`]'s fenced-prompt
//! gate, and exists for the same reason: the leak that actually happened on
//! this codebase was a *new* sink nobody thought to look for, added in a
//! sibling worktree while every gate was green.
//!
//! What is deliberately **not** claimed:
//!
//! - The request-scoped call sites that predate this module
//!   ([`crate::ai::gate::admit`], [`crate::rank::l2`],
//!   [`crate::ai::rag::context`], [`crate::compose::reply`]) still *refuse* a
//!   non-`permits_network()` decision rather than routing it on-device. Mail
//!   in a `local_only` folder therefore gets on-device triage and summaries
//!   (mechanism 2, the queue path) but no local ad-hoc draft or ask. That is a
//!   gap, not an equivalence.
//! - "No egress" here means no egress *for AI generation*. A local-only daemon
//!   can still be configured with hosted embeddings
//!   (`index.semantic.provider = "voyage"`), outbound webhooks, or OAuth token
//!   refreshes. `hosted_clients_permitted` covers the model path; the ALLOWED
//!   table in `tests.rs` names every other client and what it carries.
//!
//! # The verbs are the same verbs
//!
//! [`LocalProvider`] implements [`Provider`], the same trait
//! `ClaudeProvider` does, so summarize (`ai::triage`, `ai::deep`), draft
//! (`compose::reply`), ask (`ai::rag`) and every other pass reach it unchanged
//! — that is what "the same verb surface" means here, and it is why this
//! module adds no pass of its own. Embeddings were already local
//! ([`crate::embed::local`], `index.semantic.backend = "local"`), and
//! deliberately share one cache directory (`RMAIL_MODEL_CACHE`) with this
//! path so provisioning an air-gapped host is one step rather than two.
//!
//! # Everything it produces is labelled
//!
//! A response's `model` is the configured model id under
//! [`LOCAL_MODEL_PREFIX`] (`local/qwen2.5-3b-instruct-q4_k_m`), and
//! [`crate::ai::audit`]'s pricing table prices that prefix at zero — a local
//! call genuinely costs nothing, and saying so explicitly keeps it out of the
//! "unpriced model" warning path that exists for a pricing *gap*.
//!
//! How far that label travels depends on the caller, and the difference is
//! worth knowing:
//!
//! - The **queue path** — the one this module actually routes
//!   ([`crate::ai::queue::AiWorkerPool`]) — writes `response.model` into
//!   `ai_ledger.model`, so a locally generated row is identifiable from
//!   storage alone and is charged at zero.
//! - Most **request-scoped** callers record the model they *asked* for rather
//!   than the one that answered. Those callers do not route on-device today
//!   (see the "not claimed" list above), so the two agree in practice — but a
//!   later task that routes one of them locally must switch it to
//!   `response.model` first, or its ledger rows will read as hosted and be
//!   priced as hosted.
//!
//! # Degradation is a first-class outcome
//!
//! A local model is absent far more often than a hosted one is down, so the
//! failure modes are named rather than lumped into "internal error", following
//! [`crate::embed::local`]'s split between "not provisioned" (a
//! [`Error::FailedPrecondition`] naming the fix) and a genuine fault:
//!
//! | state | outcome |
//! |---|---|
//! | `ai.local.runtime_command` empty | `FailedPrecondition` naming the field — at daemon start under `ai.provider = "local"`, per call otherwise |
//! | runtime binary not on `PATH` | `FailedPrecondition` naming the binary |
//! | weights file absent | `FailedPrecondition` naming the path and `RMAIL_MODEL_CACHE` |
//! | weights smaller than `min_model_bytes` | `FailedPrecondition` saying it looks like an interrupted download |
//! | runtime killed by a signal | `ResourceExhausted` saying the model may be too large for this machine (this is what the OOM killer looks like from here) |
//! | runtime exits non-zero | `Internal` with a bounded stderr tail |
//! | runtime produces nothing | `Internal` — an empty completion is not a valid turn |
//! | generation exceeds `timeout_secs` | `DeadlineExceeded`, child killed (process group), not abandoned |
//! | request cancelled | `DeadlineExceeded`, child killed |
//!
//! # Inference is CPU work, and it is spawned, not awaited on the runtime
//!
//! [`engine::CommandEngine`] runs the operator's runtime as a child process
//! through [`crate::hooks::run_hook`] — the same process supervisor the hook
//! dispatcher uses, which already handles the parts that are easy to get
//! wrong: concurrent stdin write / stdout drain / wait (a sequential version
//! deadlocks on a full pipe), a process-group kill on timeout so a
//! `sh -c "a; b"` wrapper's children die too, and a `wait` after the kill so
//! nothing is left a zombie. Nothing blocks a runtime thread, and cancellation
//! reaches the model, not just the future waiting on it.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use tokio_util::sync::CancellationToken;

use crate::ai::policy::PolicyDecision;
use crate::ai::provider::{
    ChatRequest, ChatResponse, Provider, ProviderStream, Role, StopReason, StreamFrame, Usage,
};
use crate::config::{AiConfig, AiLocal, AiProvider};
use crate::error::Error;

pub mod engine;
pub mod repo;

pub use engine::CommandEngine;
pub use repo::{
    effective_provider, resolve_override, set_override, stored_override, GLOBAL_ACCOUNT_ID,
};

/// Prefix every model id this path reports, so a locally generated row is
/// identifiable from storage alone. See the module docs' "Everything it
/// produces is labelled".
pub const LOCAL_MODEL_PREFIX: &str = "local/";

/// The turn markers [`render_prompt`] separates a transcript with.
///
/// Deliberately built from [`crate::ai::injection`]'s fence brackets rather
/// than something readable like `### User`. A markdown heading is a string a
/// *message body* can contain, so an email whose text includes a line reading
/// `### Assistant` would forge a turn boundary inside its own fence — the
/// role-spoof attack that module already names, reintroduced by the renderer
/// that was supposed to preserve the defense. `⟪`/`⟫` cannot appear in
/// anything [`crate::ai::injection::untrusted_block`] wrapped, because
/// `neutralize_fence` replaces them before wrapping, so these markers are
/// unforgeable by exactly the same mechanism the fence itself relies on.
const TURN_USER: &str = "⟪turn user⟫\n";
const TURN_ASSISTANT: &str = "⟪turn assistant⟫\n";

/// Whether this configuration may construct hosted AI clients at all.
///
/// The one predicate the daemon's wiring consults before building anything
/// that can carry mail to a hosted model. It exists because
/// [`crate::ai::provider::build`] is *not* the only such construction:
/// [`crate::ai::queue::BatchClient`] is a second, independent egress that
/// never touches an `Arc<dyn Provider>`, so a `match` on the provider enum in
/// `build` alone would leave `ai.provider = "local"` shipping mail to the
/// Batches API — which is precisely what it did before this function existed.
///
/// A single named predicate rather than an inline `matches!` at each site so
/// the source gate can check that each site still calls it.
#[must_use]
pub fn hosted_clients_permitted(config: &AiConfig) -> bool {
    matches!(config.provider, AiProvider::Claude)
}

/// The instruction appended when a caller asked for structured output.
///
/// Hosted structured output is a *guarantee* enforced by the API; a local
/// runtime has no such facility, so this is the honest substitute: ask, then
/// verify (see [`extract_json`]). It is appended after the caller's system
/// prompt, never before and never interleaved, so the fence clause
/// [`crate::ai::injection::with_data_boundary`] put there stays byte-intact
/// and keeps meaning what it says.
const SCHEMA_INSTRUCTION: &str = "\n\nRespond with a single JSON value and \
nothing else -- no prose before or after it, no code fence. It must validate \
against this JSON Schema:\n";

// ---------------------------------------------------------------------------
// Where a call is allowed to go
// ---------------------------------------------------------------------------

/// Which backend a call may use — the one thing this module ultimately
/// decides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Egress {
    /// On-device inference. Nothing leaves the machine.
    Local,
    /// A hosted provider. This is the only variant that can egress.
    Network,
}

impl Egress {
    /// The spelling used in logs, on the wire, and in the CLI's output.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Network => "network",
        }
    }
}

/// Combine the daemon default, an account's stored override, and the resolved
/// [`PolicyDecision`] into one [`Egress`].
///
/// The order is the property, and it is deliberately *not* symmetric:
///
/// 1. A decision that is not visible ([`crate::ai::AiPolicyMode::Forbidden`])
///    is refused outright. It is not "route it locally" — forbidden mail is
///    not eligible for AI at all, and quietly downgrading it to on-device
///    inference would process mail an operator said to leave alone.
/// 2. A decision that does not permit network resolves [`Egress::Local`]
///    **whatever the override says**. This is the "forced by policy" half of
///    the acceptance: an operator can move an account on-device, but cannot
///    move a local-only folder off it. `permits_network()` is checked rather
///    than matching on `Allowed`, so a future policy mode fails closed.
/// 3. Only then does the selection apply: the account's override if it has
///    one, else the daemon-wide `ai.provider`.
///
/// # Errors
///
/// [`Error::FailedPrecondition`] for a forbidden decision, carrying the mode
/// so the caller's message says which rule refused it.
pub fn resolve_egress(
    default: AiProvider,
    account_override: Option<AiProvider>,
    decision: &PolicyDecision,
) -> Result<Egress, Error> {
    if !decision.is_visible() {
        return Err(Error::failed_precondition(format!(
            "ai policy resolved {:?} for this account/folder; no model call is permitted, \
             on-device or otherwise",
            decision.mode
        )));
    }
    if !decision.permits_network() {
        return Ok(Egress::Local);
    }
    Ok(match account_override.unwrap_or(default) {
        AiProvider::Local => Egress::Local,
        AiProvider::Claude => Egress::Network,
    })
}

// ---------------------------------------------------------------------------
// The on-device engine seam
// ---------------------------------------------------------------------------

/// What [`LocalProvider`] knows about the machine's inference runtime.
///
/// Separate from [`Provider`] so the two concerns stay apart: `Provider`
/// speaks in turns, roles and stop reasons, while an engine takes a rendered
/// prompt and gives back text. A linked engine (candle, llama.cpp as a
/// library) implements this trait and nothing above it changes — that is the
/// whole reason the seam exists rather than [`LocalProvider`] spawning
/// processes itself.
#[async_trait]
pub trait LocalEngine: Send + Sync + std::fmt::Debug {
    /// The model id, without [`LOCAL_MODEL_PREFIX`].
    fn model(&self) -> &str;

    /// Generate a completion for `prompt`, stopping at roughly `max_tokens`.
    ///
    /// Must honor `cancel` by terminating the inference, not merely by
    /// dropping the future: a local model that keeps running holds a CPU core
    /// away from the rest of the daemon.
    ///
    /// # Errors
    ///
    /// Engine-specific. See the module docs' degradation table for the
    /// mapping [`engine::CommandEngine`] uses.
    async fn generate(
        &self,
        prompt: &str,
        max_tokens: u32,
        cancel: &CancellationToken,
    ) -> Result<String, Error>;

    /// Whether this engine could serve a call right now, and if not, what an
    /// operator has to do about it. Never fails: "not ready, because X" is the
    /// answer, not an error.
    async fn readiness(&self) -> LocalReadiness;
}

/// Whether the local path can serve a call, and why not if it cannot.
///
/// This is the *inspection* half of the operator surface — `AiPolicyService.
/// GetAiProvider` / `mail ai provider status` render it. A capability that can
/// only be selected, never inspected, leaves an operator to discover a missing
/// model from a failed summary hours later.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalReadiness {
    /// Whether a call would get past the preconditions.
    pub ready: bool,
    /// The model id, without [`LOCAL_MODEL_PREFIX`].
    pub model: String,
    /// One line an operator can act on. Never empty, ready or not.
    pub detail: String,
}

// ---------------------------------------------------------------------------
// The provider
// ---------------------------------------------------------------------------

/// A [`Provider`] that runs entirely on this machine.
///
/// This type holds no HTTP client, no endpoint and no credential, and the
/// module's source gate keeps it that way. Compare `ClaudeProvider`, whose
/// every field is one of those things.
#[derive(Debug)]
pub struct LocalProvider {
    engine: Arc<dyn LocalEngine>,
    max_prompt_bytes: usize,
    /// Correlation ids for the audit ledger. Local inference has no
    /// server-assigned response id, and a ledger row with an empty `provider_id`
    /// cannot be tied back to a log line.
    calls: AtomicU64,
}

impl LocalProvider {
    /// The provider for `config`'s runtime. Does no I/O and never fails —
    /// provisioning is checked per call (and reported by
    /// [`Self::readiness`]), because an operator who drops the weights into
    /// place should not have to restart the daemon. Configuration errors are
    /// a different thing and are caught eagerly by [`check_config`].
    #[must_use]
    pub fn new(config: &AiLocal) -> Self {
        Self::with_engine(
            Arc::new(CommandEngine::new(config)),
            config.max_prompt_bytes,
        )
    }

    /// [`Self::new`] with an engine supplied directly — the seam a linked
    /// runtime, or a test, plugs into.
    #[must_use]
    pub fn with_engine(engine: Arc<dyn LocalEngine>, max_prompt_bytes: usize) -> Self {
        Self {
            engine,
            max_prompt_bytes,
            calls: AtomicU64::new(0),
        }
    }

    /// The model id this provider labels its output with, including
    /// [`LOCAL_MODEL_PREFIX`].
    #[must_use]
    pub fn model_id(&self) -> String {
        format!("{LOCAL_MODEL_PREFIX}{}", self.engine.model())
    }

    /// Whether a call would succeed right now — see [`LocalReadiness`].
    pub async fn readiness(&self) -> LocalReadiness {
        self.engine.readiness().await
    }
}

/// Validate the *configuration* of the local path, without touching the disk.
///
/// Split from provisioning deliberately. A missing weights file is an
/// operator action pending; an empty `runtime_command` or a runtime argument
/// that is a URL is a configuration mistake that will never fix itself, and
/// under `ai.provider = "local"` it is caught at daemon start rather than on
/// the first summary hours later.
///
/// # Errors
///
/// [`Error::FailedPrecondition`] naming the field to fix.
pub fn check_config(config: &AiLocal) -> Result<(), Error> {
    if config.model.trim().is_empty() {
        return Err(Error::failed_precondition(
            "`ai.local.model` is empty; it names the model every locally generated \
             output is labelled with"
                .to_owned(),
        ));
    }
    let Some(program) = config.runtime_command.first() else {
        return Err(Error::failed_precondition(
            "the local AI path needs an on-device runtime: set \
             `ai.local.runtime_command` (e.g. [\"llama-cli\", \"-m\", \"%model%\", \
             \"-n\", \"%max_tokens%\", \"--no-display-prompt\", \"-f\", \"/dev/stdin\"]), \
             or set `ai.provider = \"claude\"`"
                .to_owned(),
        ));
    };
    if program.trim().is_empty() {
        return Err(Error::failed_precondition(
            "`ai.local.runtime_command`'s first element is the program to run and is empty"
                .to_owned(),
        ));
    }
    // Belt-and-braces against the one way an "on-device" runtime could still
    // egress: an operator pointing it at a remote inference endpoint. This
    // cannot police what the binary does once it runs — a wrapper script can
    // do anything — but it does stop the obvious, silent version, where a
    // command that reads like a curl invocation is accepted as local because
    // nothing ever looked at it.
    if let Some(argument) = config
        .runtime_command
        .iter()
        .find(|argument| argument.contains("://"))
    {
        return Err(Error::failed_precondition(format!(
            "`ai.local.runtime_command` contains {argument:?}, which is a URL; the local \
             AI path runs an on-device executable and must not be pointed at a remote \
             inference endpoint. Use `ai.provider = \"claude\"` for a hosted backend."
        )));
    }
    if config.timeout_secs == 0 {
        return Err(Error::failed_precondition(
            "`ai.local.timeout_secs` is 0; a generation would be killed before it started"
                .to_owned(),
        ));
    }
    if config.max_prompt_bytes == 0 || config.max_output_bytes == 0 {
        return Err(Error::failed_precondition(
            "`ai.local.max_prompt_bytes` and `ai.local.max_output_bytes` must be non-zero"
                .to_owned(),
        ));
    }
    Ok(())
}

#[async_trait]
impl Provider for LocalProvider {
    #[tracing::instrument(
        skip(self, request, cancel),
        fields(
            model = %self.engine.model(),
            egress = Egress::Local.as_str(),
            prompt_bytes,
            elapsed_ms,
        )
    )]
    async fn complete(
        &self,
        request: &ChatRequest,
        cancel: &CancellationToken,
    ) -> Result<ChatResponse, Error> {
        let started = std::time::Instant::now();
        let prompt = render_prompt(request, self.max_prompt_bytes)?;
        tracing::Span::current().record("prompt_bytes", prompt.len());

        let raw = self
            .engine
            .generate(&prompt, request.max_tokens, cancel)
            .await?;
        let text = match request.output_format.as_ref() {
            Some(_) => extract_json(&raw)?,
            None => raw.trim().to_owned(),
        };
        if text.is_empty() {
            return Err(Error::internal(
                "the local model produced an empty completion".to_owned(),
            ));
        }

        let output_tokens = estimate_tokens(&text);
        let response = ChatResponse {
            id: format!("local-{}", self.calls.fetch_add(1, Ordering::Relaxed)),
            model: self.model_id(),
            // A subprocess reports no stop reason, so this is inferred: an
            // output that reached the cap almost certainly hit it. Reported
            // rather than always claiming `EndTurn`, because a caller that
            // retries on truncation would otherwise never learn it was
            // truncated.
            stop_reason: if output_tokens >= request.max_tokens {
                StopReason::MaxTokens
            } else {
                StopReason::EndTurn
            },
            text,
            usage: Usage {
                input_tokens: estimate_tokens(&prompt),
                output_tokens,
                // No prompt cache on-device: nothing is written to one and
                // nothing is served from one, and reporting invented cache
                // activity would make the cache-hit metrics of a mixed
                // deployment meaningless.
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            },
        };
        tracing::Span::current().record("elapsed_ms", started.elapsed().as_millis());
        Ok(response)
    }

    /// The frame contract, from a backend that cannot stream.
    ///
    /// A child process hands back its output when it is done, so there is
    /// nothing to emit incrementally and this deliberately does not pretend
    /// otherwise by chunking a finished string on a timer. What it does
    /// preserve is the *contract* every streaming caller relies on — text
    /// frames, then [`StreamFrame::Usage`], then [`StreamFrame::Done`], or an
    /// error and no `Done` — so `AnalyzeMessage`, `DraftReply` and `AskMailbox`
    /// work against the local path unchanged. Failures are discovered before
    /// the stream opens and are returned from here, which is exactly what the
    /// trait says to do with them.
    async fn stream(
        &self,
        request: &ChatRequest,
        cancel: &CancellationToken,
    ) -> Result<ProviderStream, Error> {
        let response = self.complete(request, cancel).await?;
        let frames = vec![
            Ok(StreamFrame::Token(response.text)),
            Ok(StreamFrame::Usage(response.usage)),
            Ok(StreamFrame::Done {
                stop_reason: response.stop_reason,
            }),
        ];
        Ok(Box::pin(futures::stream::iter(frames)))
    }
}

// ---------------------------------------------------------------------------
// Prompt rendering
// ---------------------------------------------------------------------------

/// Render a [`ChatRequest`] as the plain-text transcript an on-device runtime
/// reads from stdin.
///
/// The caller's `system` text is copied **verbatim and first**. That is not
/// stylistic: every AI pass in this crate builds its system prompt through
/// [`crate::ai::injection::with_data_boundary`], and the clause it appends is
/// what gives the `⟪untrusted …⟫` markers around message content their
/// meaning. Rewriting, reordering or summarizing the system text here would
/// silently strip a security control on its way to the model —
/// `the_fence_survives_rendering` is the probe.
///
/// Over-long prompts are refused rather than truncated: truncation drops the
/// *end* of the transcript, which is where the actual instruction and the
/// assistant cue live, so a truncated prompt does not ask a smaller question —
/// it asks a different one.
///
/// # Errors
///
/// [`Error::InvalidArgument`] if the rendered prompt exceeds
/// `max_prompt_bytes`, or if the request carries no turns at all.
fn render_prompt(request: &ChatRequest, max_prompt_bytes: usize) -> Result<String, Error> {
    if request.messages.is_empty() {
        return Err(Error::invalid_argument(
            "a local completion needs at least one message turn".to_owned(),
        ));
    }
    let mut out = String::new();
    if let Some(system) = request.system.as_ref() {
        out.push_str(system);
        out.push_str("\n\n");
    }
    if let Some(format) = request.output_format.as_ref() {
        out.push_str(SCHEMA_INSTRUCTION);
        let schema = serde_json::to_string_pretty(&format.schema)
            .map_err(|e| Error::internal(format!("could not render the output schema: {e}")))?;
        out.push_str(&schema);
        out.push_str("\n\n");
    }
    for message in &request.messages {
        out.push_str(match message.role {
            Role::User => TURN_USER,
            Role::Assistant => TURN_ASSISTANT,
        });
        out.push_str(&message.content);
        out.push_str("\n\n");
    }
    // The cue the model continues from. Without it a base-model runtime tends
    // to continue the *user* turn instead of answering it.
    out.push_str(TURN_ASSISTANT);

    if out.len() > max_prompt_bytes {
        return Err(Error::invalid_argument(format!(
            "the rendered prompt is {} bytes, over `ai.local.max_prompt_bytes` ({}); \
             reduce the content sent to the local model rather than truncating it",
            out.len(),
            max_prompt_bytes
        )));
    }
    Ok(out)
}

/// Pull the first complete JSON value out of a local model's output.
///
/// A hosted structured-output request cannot come back as prose; a local one
/// routinely does — a leading "Sure, here is the JSON:", a ```` ```json ````
/// fence, a trailing explanation. Scanning for the first balanced value and
/// validating it is the honest version of the guarantee this backend cannot
/// make natively.
///
/// The scan is string- and escape-aware, so a brace inside a quoted string
/// (`{"subject": "Re: {urgent}"}`) does not end the value early.
///
/// # Every candidate is tried, not just the first
///
/// A single left-to-right pass is not enough, and the failure is not
/// theoretical: prose like `Here is the "{" JSON: {"a":1}` starts a candidate
/// at the *quoted* brace, and the quote that follows puts a naive scanner
/// inside a phantom string for the rest of the input — reporting "no JSON"
/// while valid JSON sits right there. Local models produce `{placeholder}`
/// prose constantly, and because the queue retries, each false negative costs
/// another full generation. So a candidate that fails to balance or fails to
/// parse restarts the scan one byte later rather than ending it.
///
/// What this does **not** do is validate against the requested schema — that
/// would need a JSON Schema implementation this workspace does not carry — nor
/// extract a top-level scalar (`42`, `"a string"`), which no caller in this
/// codebase asks a model for. The guarantee is "valid JSON object or array, or
/// a clear error", and every caller already handles its own deserialization
/// failing.
///
/// # Errors
///
/// [`Error::Internal`] if the output holds no balanced JSON value, with a
/// bounded excerpt of what it did hold. If it held something that *looked*
/// like JSON but did not parse, the parser's complaint is reported instead —
/// the two are different bugs in the model's behavior and an operator reading
/// a log needs to tell them apart.
fn extract_json(text: &str) -> Result<String, Error> {
    let bytes = text.as_bytes();
    let mut first_parse_error: Option<String> = None;

    for (from, opener) in bytes.iter().copied().enumerate() {
        if opener != b'{' && opener != b'[' {
            continue;
        }
        match scan_balanced(bytes, from) {
            Some(end) => {
                let candidate = text.get(from..=end).unwrap_or_default();
                match serde_json::from_str::<serde_json::Value>(candidate) {
                    Ok(_) => return Ok(candidate.to_owned()),
                    Err(e) => {
                        // Remembered, not returned: a later candidate may
                        // still parse, and only the first complaint is worth
                        // reporting if none does.
                        first_parse_error.get_or_insert_with(|| e.to_string());
                    }
                }
            }
            None => continue,
        }
    }
    Err(match first_parse_error {
        Some(complaint) => Error::internal(format!(
            "the local model's structured output is not valid JSON: {complaint}"
        )),
        None => Error::internal(format!(
            "the local model was asked for JSON and returned none: {:?}",
            excerpt(text)
        )),
    })
}

/// The index of the byte closing the value that opens at `from`, or `None` if
/// it never closes (or closes with the wrong bracket).
fn scan_balanced(bytes: &[u8], from: usize) -> Option<usize> {
    let closer = match bytes.get(from)? {
        b'{' => b'}',
        b'[' => b']',
        _ => return None,
    };
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for (index, byte) in bytes.iter().copied().enumerate().skip(from) {
        if in_string {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => depth += 1,
            b'}' | b']' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    // A mismatched closer (`{ … ]`) is not this value ending;
                    // it is not a value at all.
                    return (byte == closer).then_some(index);
                }
            }
            _ => {}
        }
    }
    None
}

/// A bounded, single-line excerpt for an error message. Model output can be
/// megabytes, and an unbounded excerpt becomes an unbounded `grpc-message`
/// trailer at the gRPC boundary.
fn excerpt(text: &str) -> String {
    const MAX: usize = 160;
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(MAX) {
        Some((cut, _)) => format!("{}…", &flat[..cut]),
        None => flat,
    }
}

/// A byte-count token estimate.
///
/// There is no tokenizer here — the runtime owns that, and it does not report
/// counts. Roughly four bytes per token is the usual English approximation,
/// and it is used only for volume telemetry: a local call is priced at zero
/// (see [`crate::ai::audit`]), so no dollar cap depends on this number being
/// right.
fn estimate_tokens(text: &str) -> u32 {
    u32::try_from(text.len().div_ceil(4)).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests;
