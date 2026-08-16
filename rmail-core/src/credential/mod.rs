//! Credential providers.
//!
//! An account never stores its password — only a [`CredentialSource`] describing
//! *how* to obtain it: a shell command, an environment variable name, or a
//! macOS Keychain service. [`CredentialSource::resolve`] fetches the secret
//! lazily into a [`Secret`], whose `Debug`/`Display` redact the value so it can
//! never leak into logs.

use std::fmt;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::error::Error;

/// Wall-clock bound on a credential command so a wedged process cannot pin a
/// blocking-pool thread.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);

/// A resolved secret whose value never appears in `Debug`/`Display` output.
///
/// Use [`Secret::expose`] only at the exact point the raw value is needed
/// (e.g. an IMAP LOGIN), never for logging.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wrap a raw secret value.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Borrow the raw secret. Handle with care — never log the result.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(***)")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

/// Deserialize straight into a [`Secret`], so a token arriving in a JSON body
/// is redacted from the moment it is parsed rather than after somebody
/// remembers to wrap it.
///
/// There is deliberately no `Serialize`: a `Secret` that can be serialized is
/// a `Secret` that lands in the next structured log line or JSON response
/// somebody derives. Writing one out is always explicit, via
/// [`Secret::expose`], at the one boundary that needs it.
impl<'de> serde::Deserialize<'de> for Secret {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer).map(Self::new)
    }
}

/// Where an account's password comes from. Holds only a reference (a command,
/// env var name, or keychain service) — never the secret.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum CredentialSource {
    /// No credential configured.
    #[default]
    None,
    /// A shell command whose trimmed stdout is the password.
    Command(String),
    /// The name of an environment variable holding the password.
    Env(String),
    /// A macOS Keychain generic-password service name (looked up with the
    /// account username as the account field).
    Keychain(String),
    /// OAuth2: the account authenticates with a short-lived bearer token
    /// obtained by [`crate::oauth`], not with a password. The string is the
    /// Keychain service holding the grant, addressed the same way
    /// [`CredentialSource::Keychain`] is.
    ///
    /// This is a source with no password to resolve, which is why
    /// [`CredentialSource::resolve`] refuses it rather than returning
    /// something: an access token has to be *refreshed*, which is async and
    /// needs the broker's per-account lock, and a caller that got a bare
    /// string back here would send it as an IMAP `LOGIN` password — which
    /// every OAuth provider rejects.
    OAuth(String),
}

impl CredentialSource {
    /// Reconstruct from the `(secret_kind, secret_ref)` persisted on an account.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidArgument`] for an unknown kind or a missing
    /// reference where one is required.
    pub fn from_stored(kind: &str, reference: Option<&str>) -> Result<Self, Error> {
        match kind {
            "none" => Ok(Self::None),
            "command" | "env" | "keychain" | "oauth" => {
                let reference = reference.ok_or_else(|| {
                    Error::invalid_argument(format!(
                        "credential kind {kind:?} requires a reference"
                    ))
                })?;
                Ok(match kind {
                    "command" => Self::Command(reference.to_owned()),
                    "env" => Self::Env(reference.to_owned()),
                    "keychain" => Self::Keychain(reference.to_owned()),
                    _ => Self::OAuth(reference.to_owned()),
                })
            }
            other => Err(Error::invalid_argument(format!(
                "unknown credential kind {other:?} (use none, command, env, keychain, oauth)"
            ))),
        }
    }

    /// The persisted `secret_kind` string for this source.
    #[must_use]
    pub fn kind(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Command(_) => "command",
            Self::Env(_) => "env",
            Self::Keychain(_) => "keychain",
            Self::OAuth(_) => "oauth",
        }
    }

    /// Whether this account authenticates with an OAuth2 bearer token
    /// (XOAUTH2) rather than a password.
    #[must_use]
    pub const fn is_oauth(&self) -> bool {
        matches!(self, Self::OAuth(_))
    }

    /// The persisted `secret_ref` (the command/env-name/keychain-service).
    #[must_use]
    pub fn reference(&self) -> Option<&str> {
        match self {
            Self::None => None,
            Self::Command(r) | Self::Env(r) | Self::Keychain(r) | Self::OAuth(r) => Some(r),
        }
    }

    /// Resolve the secret now. `username` is used as the Keychain account field.
    ///
    /// Returns `Ok(None)` when the source is [`CredentialSource::None`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::Unauthenticated`] if the secret cannot be obtained
    /// (missing env var, command failure, keychain miss, unsupported platform).
    pub fn resolve(&self, username: Option<&str>) -> Result<Option<Secret>, Error> {
        match self {
            Self::None => Ok(None),
            Self::Env(name) => std::env::var(name)
                .map(|v| Some(Secret::new(v)))
                .map_err(|_| {
                    Error::unauthenticated(format!("environment variable {name:?} is not set"))
                }),
            Self::Command(command) => resolve_command(command).map(Some),
            Self::Keychain(service) => resolve_keychain(service, username).map(Some),
            // Refused rather than resolved: see the variant's own docs. The
            // caller must go through `crate::oauth::OAuthBroker`, which is
            // async (it may have to refresh) and serializes per account.
            Self::OAuth(_) => Err(Error::failed_precondition(
                "this account authenticates with OAuth2 (XOAUTH2); it has no password to resolve",
            )),
        }
    }
}

/// Run a shell command and take its trimmed stdout as the secret, bounded by
/// [`COMMAND_TIMEOUT`] so a wedged command is killed rather than pinning a
/// blocking-pool thread. Never echoes the command's output (it may be secret).
fn resolve_command(command: &str) -> Result<Secret, Error> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| Error::unauthenticated(format!("credential command failed to start: {e}")))?;

    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if start.elapsed() >= COMMAND_TIMEOUT {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(Error::unauthenticated(
                        "credential command timed out".to_owned(),
                    ));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => {
                return Err(Error::unauthenticated(format!(
                    "credential command failed: {e}"
                )))
            }
        }
    };

    if !status.success() {
        return Err(Error::unauthenticated(
            "credential command exited with a non-zero status".to_owned(),
        ));
    }

    // The password is small, so reading after exit won't deadlock the pipe.
    let mut raw = Vec::new();
    if let Some(mut stdout) = child.stdout.take() {
        stdout.read_to_end(&mut raw).map_err(|e| {
            Error::unauthenticated(format!("failed to read credential command output: {e}"))
        })?;
    }
    let value = String::from_utf8(raw).map_err(|_| {
        Error::unauthenticated("credential command produced non-UTF-8 output".to_owned())
    })?;
    let trimmed = value.trim_end_matches(['\r', '\n']);
    if trimmed.is_empty() {
        return Err(Error::unauthenticated(
            "credential command produced no output".to_owned(),
        ));
    }
    Ok(Secret::new(trimmed))
}

/// Resolve a macOS Keychain generic password.
#[cfg(target_os = "macos")]
fn resolve_keychain(service: &str, username: Option<&str>) -> Result<Secret, Error> {
    let account = username.ok_or_else(|| {
        Error::unauthenticated("keychain lookup requires the account username".to_owned())
    })?;
    let bytes = security_framework::passwords::get_generic_password(service, account)
        .map_err(|e| Error::unauthenticated(format!("keychain lookup failed: {e}")))?;
    let value = String::from_utf8(bytes)
        .map_err(|_| Error::unauthenticated("keychain secret is not valid UTF-8".to_owned()))?;
    Ok(Secret::new(value))
}

/// Keychain is unavailable off macOS.
#[cfg(not(target_os = "macos"))]
fn resolve_keychain(_service: &str, _username: Option<&str>) -> Result<Secret, Error> {
    Err(Error::unauthenticated(
        "keychain credentials are only supported on macOS".to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_debug_and_display_redact() {
        let secret = Secret::new("hunter2");
        assert_eq!(format!("{secret:?}"), "Secret(***)");
        assert_eq!(format!("{secret}"), "***");
        assert!(!format!("{secret:?}").contains("hunter2"));
        assert_eq!(secret.expose(), "hunter2");
    }

    #[test]
    fn none_resolves_to_no_secret() {
        assert!(CredentialSource::None.resolve(None).unwrap().is_none());
    }

    #[test]
    fn env_source_resolves_and_errors_when_missing() {
        // Use a process-unique var name to stay hermetic under threaded tests.
        let var = format!("RMAIL_TEST_CRED_{}", std::process::id());
        // Missing -> Unauthenticated.
        let missing = CredentialSource::Env(var.clone()).resolve(None);
        assert!(matches!(missing, Err(ref e) if e.reason() == crate::ErrorReason::Unauthenticated));

        // Present -> resolved value.
        // SAFETY: single-threaded within this test; var name is process-unique.
        std::env::set_var(&var, "s3cr3t");
        let resolved = CredentialSource::Env(var.clone())
            .resolve(None)
            .unwrap()
            .unwrap();
        assert_eq!(resolved.expose(), "s3cr3t");
        std::env::remove_var(&var);
    }

    #[test]
    fn command_source_resolves_trimmed_stdout() {
        let secret = CredentialSource::Command("printf 'pw-from-cmd\\n'".to_owned())
            .resolve(None)
            .unwrap()
            .unwrap();
        assert_eq!(secret.expose(), "pw-from-cmd", "trailing newline trimmed");
    }

    #[test]
    fn command_failure_is_unauthenticated_and_does_not_echo_output() {
        let err = CredentialSource::Command("echo leaky-secret; exit 1".to_owned())
            .resolve(None)
            .expect_err("non-zero exit must error");
        assert_eq!(err.reason(), crate::ErrorReason::Unauthenticated);
        assert!(
            !err.to_string().contains("leaky-secret"),
            "command output must not leak into the error: {err}"
        );
    }

    #[test]
    fn empty_command_output_is_rejected() {
        let err = CredentialSource::Command("true".to_owned())
            .resolve(None)
            .expect_err("empty output must error");
        assert_eq!(err.reason(), crate::ErrorReason::Unauthenticated);
    }

    #[test]
    fn from_stored_roundtrips_and_validates() {
        assert_eq!(
            CredentialSource::from_stored("command", Some("sh -c x")).unwrap(),
            CredentialSource::Command("sh -c x".to_owned())
        );
        assert_eq!(
            CredentialSource::from_stored("none", None).unwrap(),
            CredentialSource::None
        );
        // Missing reference where required.
        assert!(CredentialSource::from_stored("env", None).is_err());
        // Unknown kind.
        assert!(CredentialSource::from_stored("bogus", Some("x")).is_err());

        let src = CredentialSource::Keychain("fastmail".to_owned());
        assert_eq!(src.kind(), "keychain");
        assert_eq!(src.reference(), Some("fastmail"));
    }
}
