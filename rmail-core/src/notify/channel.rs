//! Where a fired notification actually goes.
//!
//! # Nothing here leaves the machine
//!
//! There is exactly one delivering implementation, [`DesktopChannel`], and it
//! spawns `osascript(1)` on the local host. That is the entire egress surface
//! of this subsystem: no webhook, no push service, no HTTP client. prd.md's
//! privacy posture is that mail content does not leave the machine unless the
//! operator has explicitly arranged for it to, and a notification carrying a
//! subject line to a third-party push endpoint would be precisely that,
//! arranged by nobody. A remote channel is a feature that needs its own
//! opt-in surface and its own review; it is not something to grow quietly as
//! another arm of this trait.
//!
//! Even locally, what a notification says is minimized by default: the sender
//! and the tier always, the subject only if `notify.include_subject`, the
//! model's one-line reason only if `notify.include_reason` (off by default,
//! because the reason is derived from the *body* and can restate what a
//! deliberately vague subject withheld). Message bodies are never included
//! under any setting — there is no configuration that puts one in a
//! notification.
//!
//! # The argument is data, never a script
//!
//! `osascript -e 'display notification "…"'` is a script string, and a
//! subject line is attacker-controlled text. Interpolating one into the other
//! is the same class of bug `crate::hooks` exists to avoid, with AppleScript
//! in place of `sh`: a subject containing `" & (do shell script "…") & "`
//! would stop being text and start being code.
//!
//! So [`DesktopChannel`] never interpolates. It passes the script as a
//! constant with `run(argv)` handler parameters, and hands the untrusted
//! strings as *arguments* after `--`, where `osascript` binds them to the
//! handler's parameter list without ever parsing them as AppleScript.
//! `notify::tests::a_hostile_subject_is_passed_as_an_argument_never_as_script`
//! is the regression proof.
//!
//! # An unavailable channel is an outcome, not an error
//!
//! [`NotifyChannel::deliver`] returns `Result<(), DeliveryError>` and the
//! engine treats a failure as "retry, then record `failed`" — never as a
//! reason to stop the tick or wedge the loop. A machine with no `osascript`
//! (a Linux box, a container, a stripped image) must keep scoring, keep
//! streaming alerts to `mail notify watch`, and keep an honest record that
//! the desktop ping did not happen.

use std::process::Stdio;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;

/// What one notification says, already filtered by `notify.include_*`.
///
/// Built by [`super::NotifyEngine`], never by a channel: what may be shown is
/// a policy decision, and a channel that could reach back for the subject it
/// was not given would make that policy advisory.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Notification {
    /// Title line — the sender, or the account when there is no sender.
    pub title: String,
    /// Subtitle — the tier, so a glance distinguishes "high" from "critical".
    pub subtitle: String,
    /// Body — subject and/or reason, per config. May be empty.
    pub body: String,
}

/// Why a delivery attempt did not happen.
#[derive(Debug, thiserror::Error)]
pub enum DeliveryError {
    /// This platform (or this configuration) has no desktop notifier.
    #[error("no desktop notification channel is available: {0}")]
    Unavailable(String),
    /// The notifier ran and refused, or could not be run.
    #[error("the desktop notifier failed: {0}")]
    Failed(String),
    /// The notifier did not finish inside `notify.delivery_timeout`.
    #[error("the desktop notifier did not finish within {0:?}")]
    TimedOut(Duration),
}

/// A place a notification can be delivered.
#[async_trait]
pub trait NotifyChannel: Send + Sync + std::fmt::Debug {
    /// A short name for logs and `ScoreMessage`'s answer.
    fn name(&self) -> &'static str;

    /// Deliver `notification`, or say why not.
    ///
    /// # Errors
    /// [`DeliveryError`] for an unavailable, failing, or hung notifier. Must
    /// never panic and must never block the runtime: the engine awaits this
    /// inside its tick.
    async fn deliver(&self, notification: &Notification) -> Result<(), DeliveryError>;
}

/// The macOS Notification Center channel.
///
/// Selected by `notify.channel = "auto"` on macOS. On every other platform
/// [`resolve`] picks [`NullChannel`] instead, so this type never has to guess
/// whether `osascript` means anything.
#[derive(Debug, Clone)]
pub struct DesktopChannel {
    timeout: Duration,
    /// The binary to run. Configurable only so tests can point it at a stub;
    /// there is no config key for it, because "run this program on every new
    /// mail" is not something a config file should be able to say.
    program: String,
}

/// The AppleScript `osascript` runs.
///
/// A frozen constant with a `run(argv)` handler: the three untrusted strings
/// arrive as `argv` items, bound by `osascript` itself, and are never parsed
/// as script. See the module docs.
///
/// `display notification` shows nothing when its body is empty on some macOS
/// versions, so an empty body is replaced with a single space by the caller
/// rather than by branching the script — a second script string would be a
/// second thing to review.
const OSASCRIPT_PROGRAM: &str = "on run argv
    display notification (item 1 of argv) with title (item 2 of argv) subtitle (item 3 of argv)
end run";

impl DesktopChannel {
    /// A channel bounded by `timeout` per attempt.
    #[must_use]
    pub fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            program: "osascript".to_owned(),
        }
    }

    /// Run `program` instead of `osascript` — tests only.
    #[must_use]
    pub fn with_program(mut self, program: impl Into<String>) -> Self {
        self.program = program.into();
        self
    }
}

#[async_trait]
impl NotifyChannel for DesktopChannel {
    fn name(&self) -> &'static str {
        "desktop"
    }

    async fn deliver(&self, notification: &Notification) -> Result<(), DeliveryError> {
        let body = if notification.body.is_empty() {
            " ".to_owned()
        } else {
            notification.body.clone()
        };
        let mut command = tokio::process::Command::new(&self.program);
        command
            .arg("-e")
            .arg(OSASCRIPT_PROGRAM)
            // Everything after `--` is `argv` for the handler above. The
            // strings below are attacker-influenced (a subject line, a
            // sender's display name) and reach `osascript` only here.
            .arg("--")
            .arg(&body)
            .arg(&notification.title)
            .arg(&notification.subtitle)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let child = command.spawn().map_err(|e| {
            // A missing binary is `Unavailable`, not `Failed`: the engine
            // logs them differently, and "there is no notifier on this
            // machine" is an operator fact rather than a transient fault.
            if e.kind() == std::io::ErrorKind::NotFound {
                DeliveryError::Unavailable(format!("{} is not on PATH", self.program))
            } else {
                DeliveryError::Failed(format!("could not spawn {}: {e}", self.program))
            }
        })?;
        let waited = tokio::time::timeout(self.timeout, child.wait_with_output()).await;
        match waited {
            Ok(Ok(output)) if output.status.success() => Ok(()),
            Ok(Ok(output)) => Err(DeliveryError::Failed(format!(
                "{} exited {:?}: {}",
                self.program,
                output.status.code(),
                String::from_utf8_lossy(&output.stderr).trim()
            ))),
            Ok(Err(e)) => Err(DeliveryError::Failed(format!(
                "{} could not be waited on: {e}",
                self.program
            ))),
            // `wait_with_output`'s future is dropped here, and `kill_on_drop`
            // above is what makes that a real termination rather than an
            // orphaned process — the same guarantee `crate::hooks` spells out
            // at length for hook children.
            Err(_) => Err(DeliveryError::TimedOut(self.timeout)),
        }
    }
}

/// A channel that delivers nowhere and says so.
///
/// What `notify.channel = "none"` resolves to, and what a non-macOS host
/// resolves `auto` to. Deliberately an error rather than a silent success:
/// a headless daemon's notifications should be recorded `failed`, which is
/// visible, rather than `delivered`, which would be a lie an operator has no
/// way to catch.
#[derive(Debug, Clone, Copy, Default)]
pub struct NullChannel;

#[async_trait]
impl NotifyChannel for NullChannel {
    fn name(&self) -> &'static str {
        "none"
    }

    async fn deliver(&self, _notification: &Notification) -> Result<(), DeliveryError> {
        Err(DeliveryError::Unavailable(
            "no desktop notifier is configured for this host".to_owned(),
        ))
    }
}

/// Resolve `notify.channel` for this host.
#[must_use]
pub fn resolve(channel: crate::config::NotifyChannel, timeout: Duration) -> Arc<dyn NotifyChannel> {
    match channel {
        crate::config::NotifyChannel::Auto if cfg!(target_os = "macos") => {
            Arc::new(DesktopChannel::new(timeout))
        }
        crate::config::NotifyChannel::Auto | crate::config::NotifyChannel::None => {
            Arc::new(NullChannel)
        }
    }
}

/// A channel that records what it was asked to deliver, for tests.
///
/// Lives in the non-test tree on purpose: `rmaild`'s own integration tests
/// need it too, and a `#[cfg(test)]` type is invisible across a crate
/// boundary.
#[derive(Debug, Default)]
pub struct RecordingChannel {
    delivered: Mutex<Vec<Notification>>,
    /// When set, every delivery fails with this message instead — how a test
    /// exercises the unavailable-channel path.
    fail_with: Option<String>,
}

impl RecordingChannel {
    /// A channel that accepts everything and remembers it.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A channel that refuses everything with `message`.
    #[must_use]
    pub fn failing(message: impl Into<String>) -> Self {
        Self {
            delivered: Mutex::new(Vec::new()),
            fail_with: Some(message.into()),
        }
    }

    /// Everything delivered so far, in order.
    #[must_use]
    pub fn delivered(&self) -> Vec<Notification> {
        self.delivered
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl NotifyChannel for RecordingChannel {
    fn name(&self) -> &'static str {
        "recording"
    }

    async fn deliver(&self, notification: &Notification) -> Result<(), DeliveryError> {
        if let Some(message) = &self.fail_with {
            return Err(DeliveryError::Unavailable(message.clone()));
        }
        self.delivered
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(notification.clone());
        Ok(())
    }
}
