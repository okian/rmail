//! The on-device runtime, run as a child process.
//!
//! # Why a process and not a linked library
//!
//! See [`crate::config::AiLocal`]'s own docs for the build-cost argument. The
//! consequence worth stating here is a *good* one for this task: inference
//! runs in another process entirely, so the CPU-bound part of a local
//! generation cannot stall a tokio runtime thread no matter how long it takes,
//! and a runtime that wedges or grows without bound is killed rather than
//! being a leak inside this daemon.
//!
//! # `%model%` is what declares that a runtime loads weights from disk
//!
//! `llama-cli -m /path/model.gguf` needs a weights file to exist before it is
//! worth spawning; `ollama run qwen2.5` manages its own and has no such path.
//! Rather than guess, this module keys the weights precondition on the
//! operator's own argv: a command containing `%model%` is declaring "I load
//! this file", and gets the file-exists / not-truncated checks. A command
//! without it is taken at its word and gets none.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::ai::local::{LocalEngine, LocalReadiness};
use crate::config::AiLocal;
use crate::embed::MODEL_CACHE_ENV;
use crate::error::Error;

/// The `%model%` placeholder, replaced with the resolved weights path.
const MODEL_PLACEHOLDER: &str = "%model%";

/// The `%max_tokens%` placeholder, replaced with the request's output cap.
const MAX_TOKENS_PLACEHOLDER: &str = "%max_tokens%";

/// How much of a failing runtime's stderr to repeat back. The rest goes to the
/// log — an unbounded tail becomes an unbounded `grpc-message` trailer at the
/// gRPC boundary, the same bound `ai::provider` puts on an upstream error
/// body.
const MAX_STDERR_DETAIL: usize = 300;

/// An on-device runtime invoked as `argv`, prompt on stdin, completion on
/// stdout.
#[derive(Debug, Clone)]
pub struct CommandEngine {
    model: String,
    model_file: String,
    argv: Vec<String>,
    timeout: Duration,
    max_output_bytes: usize,
    min_model_bytes: u64,
}

impl CommandEngine {
    /// The engine `config` describes. Does no I/O.
    #[must_use]
    pub fn new(config: &AiLocal) -> Self {
        Self {
            model: config.model.clone(),
            model_file: config.model_file.clone(),
            argv: config.runtime_command.clone(),
            timeout: Duration::from_secs(config.timeout_secs),
            max_output_bytes: config.max_output_bytes,
            min_model_bytes: config.min_model_bytes,
        }
    }

    /// Whether this runtime loads a weights file this daemon has to find.
    fn loads_weights(&self) -> bool {
        self.argv
            .iter()
            .any(|argument| argument.contains(MODEL_PLACEHOLDER))
    }

    /// The weights path: [`AiLocal::model_file`] as given if absolute,
    /// otherwise relative to the shared model cache.
    fn model_path(&self) -> PathBuf {
        let configured = Path::new(self.model_file.trim());
        if configured.is_absolute() {
            return configured.to_path_buf();
        }
        crate::embed::model_cache_dir().join(configured)
    }

    /// Everything that must be true before spawning is worth attempting,
    /// checked in the order an operator fixes them.
    ///
    /// Doing this *before* the spawn is what makes the outcome mapping in
    /// [`Self::generate`] unambiguous: once these pass, a `None` exit code
    /// from a process that was neither timed out nor cancelled can only mean
    /// it died on a signal, rather than also possibly meaning "there was
    /// nothing to spawn".
    ///
    /// # Errors
    ///
    /// [`Error::FailedPrecondition`] naming the thing to fix — see the module
    /// docs' degradation table in [`super`].
    fn preflight(&self) -> Result<(), Error> {
        let Some(program) = self.argv.first() else {
            return Err(Error::failed_precondition(
                "the local AI path has no runtime configured; set \
                 `ai.local.runtime_command` to the on-device model runner to use \
                 (e.g. llama-cli), or set `ai.provider = \"claude\"`"
                    .to_owned(),
            ));
        };
        if !program_exists(program) {
            return Err(Error::failed_precondition(format!(
                "the local AI runtime {program:?} was not found on PATH (or is not \
                 executable); install it or correct `ai.local.runtime_command`"
            )));
        }
        if !self.loads_weights() {
            // No `%model%`: the runtime owns its own weights. Nothing here can
            // usefully check them, and inventing a check would refuse a
            // perfectly good `ollama run …` for a file it never reads.
            return Ok(());
        }
        let path = self.model_path();
        let metadata = std::fs::metadata(&path).map_err(|_| {
            Error::failed_precondition(format!(
                "the local model weights are not at {}. Provision them there, or point \
                 {MODEL_CACHE_ENV} at a directory that already has them, or correct \
                 `ai.local.model_file`.",
                path.display()
            ))
        })?;
        if !metadata.is_file() {
            return Err(Error::failed_precondition(format!(
                "the local model weights path {} is not a file",
                path.display()
            )));
        }
        if metadata.len() < self.min_model_bytes {
            // The half-downloaded case, called out separately for the reason
            // `embed::local::cached` calls out an empty snapshot directory: an
            // interrupted fetch otherwise surfaces as an incomprehensible
            // loader error deep inside someone else's binary.
            return Err(Error::failed_precondition(format!(
                "the local model weights at {} are only {} bytes, below \
                 `ai.local.min_model_bytes` ({}) — this looks like an interrupted \
                 download. Re-fetch the model.",
                path.display(),
                metadata.len(),
                self.min_model_bytes
            )));
        }
        Ok(())
    }

    /// [`Self::preflight`] on a blocking thread.
    ///
    /// The checks are filesystem round trips (`stat` on the weights, a walk of
    /// `PATH`), which on an NFS- or SMB-mounted model cache are not the
    /// microseconds a runtime thread can absorb.
    async fn preflight_off_thread(&self) -> Result<(), Error> {
        let engine = self.clone();
        tokio::task::spawn_blocking(move || engine.preflight())
            .await
            .unwrap_or_else(|e| Err(Error::internal(format!("preflight task failed: {e}"))))
    }

    /// `argv` with the placeholders filled in for this call.
    fn argv_for(&self, max_tokens: u32) -> Vec<String> {
        let model = self.model_path().display().to_string();
        let max_tokens = max_tokens.to_string();
        self.argv
            .iter()
            .map(|argument| {
                argument
                    .replace(MODEL_PLACEHOLDER, &model)
                    .replace(MAX_TOKENS_PLACEHOLDER, &max_tokens)
            })
            .collect()
    }
}

#[async_trait::async_trait]
impl LocalEngine for CommandEngine {
    fn model(&self) -> &str {
        &self.model
    }

    #[tracing::instrument(
        skip(self, prompt, cancel),
        fields(model = %self.model, prompt_bytes = prompt.len(), max_tokens, elapsed_ms)
    )]
    async fn generate(
        &self,
        prompt: &str,
        max_tokens: u32,
        cancel: &CancellationToken,
    ) -> Result<String, Error> {
        tracing::Span::current().record("max_tokens", max_tokens);
        // Off the runtime thread for the same reason `readiness` is: preflight
        // stats the weights file and walks `PATH`, and the model cache is
        // routinely on a network mount where that is not instant.
        self.preflight_off_thread().await?;
        let argv = self.argv_for(max_tokens);
        // `preflight` already established argv is non-empty; this destructure
        // is how that is expressed without an `unwrap`.
        let Some((program, arguments)) = argv.split_first() else {
            return Err(Error::internal(
                "the local runtime command vanished between preflight and spawn".to_owned(),
            ));
        };

        // Reused rather than re-implemented: `hooks::run_hook` already races
        // the stdin write, both output drains and the wait concurrently (a
        // sequential version deadlocks the moment a model's output fills the
        // pipe buffer), kills the whole process group on timeout or
        // cancellation so a wrapper script's children go too, and reaps the
        // child afterwards so nothing is left a zombie. See `super`'s module
        // docs.
        let outcome = crate::hooks::run_hook(
            program,
            arguments,
            self.timeout,
            self.max_output_bytes,
            prompt.as_bytes(),
            cancel,
        )
        .await;
        tracing::Span::current().record("elapsed_ms", outcome.duration.as_millis());

        if outcome.timed_out {
            return Err(Error::deadline_exceeded(format!(
                "the local model did not finish within `ai.local.timeout_secs` ({}s) and \
                 was killed",
                self.timeout.as_secs()
            )));
        }
        if outcome.cancelled {
            return Err(Error::deadline_exceeded(
                "the local generation was cancelled and the runtime was killed".to_owned(),
            ));
        }
        match outcome.exit_code {
            Some(0) => {}
            // `run_hook` also reports `None` when the spawn itself failed,
            // which preflight narrows but cannot eliminate — a binary that
            // disappeared between the check and the spawn, a bad shebang, the
            // wrong architecture, `ENOMEM` on fork. Those are preconditions an
            // operator fixes, not memory pressure, and telling them "the model
            // may be too large" would send them to buy RAM for a typo.
            None if outcome.stderr.starts_with("failed to spawn hook") => {
                tracing::warn!(
                    model = %self.model,
                    stderr = %outcome.stderr,
                    "the local model runtime could not be spawned"
                );
                return Err(Error::failed_precondition(format!(
                    "the local model runtime could not be started: {}",
                    detail(&outcome.stderr)
                )));
            }
            // A process that ran but reports no code was killed by a signal.
            // On this path that is overwhelmingly the OOM killer arriving for
            // a model too large for the machine, which is a resource fact an
            // operator can act on — not an internal fault, and not something
            // retrying the same call will fix.
            None => {
                tracing::warn!(
                    model = %self.model,
                    stderr = %outcome.stderr,
                    "the local model runtime was killed by a signal"
                );
                return Err(Error::resource_exhausted(format!(
                    "the local model runtime was killed by a signal before it finished. \
                     A model of this size may not fit in this machine's memory: {}",
                    detail(&outcome.stderr)
                )));
            }
            Some(code) => {
                tracing::warn!(
                    model = %self.model,
                    code,
                    stderr = %outcome.stderr,
                    "the local model runtime failed"
                );
                return Err(Error::internal(format!(
                    "the local model runtime exited {code}: {}",
                    detail(&outcome.stderr)
                )));
            }
        }
        if outcome.stdout.trim().is_empty() {
            return Err(Error::internal(format!(
                "the local model runtime exited 0 but produced no output: {}",
                detail(&outcome.stderr)
            )));
        }
        Ok(outcome.stdout)
    }

    async fn readiness(&self) -> LocalReadiness {
        match self.preflight_off_thread().await {
            Ok(()) => LocalReadiness {
                ready: true,
                model: self.model.clone(),
                detail: if self.loads_weights() {
                    format!("ready; weights at {}", self.model_path().display())
                } else {
                    "ready; the configured runtime manages its own weights".to_owned()
                },
            },
            Err(error) => LocalReadiness {
                ready: false,
                model: self.model.clone(),
                detail: error.to_string(),
            },
        }
    }
}

/// A bounded, single-line stderr tail for an error message.
fn detail(stderr: &str) -> String {
    let flat = stderr.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.is_empty() {
        return "(no stderr)".to_owned();
    }
    match flat.char_indices().nth(MAX_STDERR_DETAIL) {
        Some((cut, _)) => format!("{}…", &flat[..cut]),
        None => flat,
    }
}

/// Whether `program` is something this host can actually execute.
///
/// A path (anything containing a separator) is checked where it points;
/// a bare name is looked up on `PATH` the way the OS will. Done here rather
/// than left to the spawn so that "you have not installed the runtime" is a
/// precondition naming the binary, distinguishable from a runtime that ran and
/// failed — see [`CommandEngine::preflight`].
fn program_exists(program: &str) -> bool {
    let program = program.trim();
    if program.is_empty() {
        return false;
    }
    if program.contains(std::path::MAIN_SEPARATOR) {
        return is_executable(Path::new(program));
    }
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| is_executable(&dir.join(program)))
}

/// Whether `path` is a file this process could exec.
fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    // Windows has no execute bit; being a regular file on `PATH` is as much as
    // can be checked before the spawn.
    #[cfg(not(unix))]
    {
        true
    }
}
