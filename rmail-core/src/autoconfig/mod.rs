//! Account autoconfiguration: from an email address to a ready `[[accounts]]`
//! block (task 80, prd.md #32).
//!
//! # The shape of it
//!
//! 1. [`probe`] asks the network: the domain's own autoconfig document,
//!    Mozilla's ISPDB, Microsoft autodiscover, then RFC 6186 SRV records.
//! 2. On a miss — and only when the caller opted in — [`infer`] hands the
//!    domain, its MX records and the probe responses to Claude, which may
//!    *propose* settings.
//! 3. [`validate`] refuses anything that is not a syntactically valid public
//!    hostname, a port in range, and an encrypted transport. Every candidate
//!    goes through it, from every source, before it becomes a
//!    [`ServerSettings`].
//! 4. [`login`] settles it, when the caller supplied a credential reference
//!    *and* the settings came from a probe rather than the model, by logging
//!    in for real — see [`Autoconfigurator::verify`] for why the model is the
//!    exception.
//! 5. [`Autoconfigurator::discover`] renders the result as TOML.
//!
//! # Nothing here is trusted, and nothing here is written
//!
//! Every value in steps 1 and 2 is someone else's text — a domain
//! administrator's, a third-party database's, or a language model's. They are
//! not ranked by trust because they do not need to be: the same validator
//! stands between all of them and a connection, and the same login settles
//! all of them.
//!
//! And this module **stores nothing**. It creates no account, and it changes
//! no existing one. That is what makes the "never silently change an already
//! configured account's security settings" rule structural rather than
//! remembered: there is no code path from a discovery to an `UPDATE`. When an
//! account already exists for the address,
//! [`Proposal::existing_account_id`] names it and the differences appear in
//! [`Proposal::warnings`], for a human to apply or ignore.
//!
//! # Why the settings type cannot express plaintext
//!
//! [`Security`] has two variants, `Tls` and `StartTls`, and no third. A
//! provider that advertises an unencrypted socket type is refused at
//! [`Security::parse`], which is the only way one is constructed from
//! discovered text. So "autoconfig downgraded my connection" is not a bug
//! that has to be prevented at each step; it is a state the types cannot
//! reach.

use std::sync::Arc;

use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::credential::CredentialSource;
use crate::error::Error;
use crate::storage::Database;

pub mod infer;
pub mod login;
pub mod probe;
pub mod validate;

#[cfg(test)]
mod tests;

pub use login::{LoginProbe, TlsLoginProbe};
pub use probe::{ProbeEndpoints, Probes};
pub use validate::Security;

/// Which probe produced a configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// The domain's own `autoconfig.<domain>` document.
    Autoconfig,
    /// Mozilla's ISPDB.
    Ispdb,
    /// Microsoft autodiscover.
    Autodiscover,
    /// RFC 6186 SRV records.
    Srv,
    /// Claude, on a miss from all of the above.
    Model,
}

impl Source {
    /// The wire spelling.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Source::Autoconfig => "autoconfig",
            Source::Ispdb => "ispdb",
            Source::Autodiscover => "autodiscover",
            Source::Srv => "srv",
            Source::Model => "model",
        }
    }
}

/// One validated server: where to connect, how, and as whom.
///
/// Only constructible through [`ServerSettings::from_raw`], which is to say
/// only through [`validate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSettings {
    /// Hostname, lowercased and validated.
    pub host: String,
    /// Port, in `1..=65535`.
    pub port: u16,
    /// The transport. Never plaintext — the type cannot say it.
    pub security: Security,
    /// The username to log in as.
    pub username: String,
}

impl ServerSettings {
    /// Validate a raw candidate server against `email`.
    ///
    /// `default_port` is used only when the document named none — a document
    /// that named a port gets its port validated, not replaced.
    ///
    /// # Errors
    ///
    /// [`Error::FailedPrecondition`], whatever [`validate`] refused: an
    /// unusable hostname, an out-of-range port, or an unencrypted (or
    /// unrecognized) socket type.
    ///
    /// **Not** `INVALID_ARGUMENT`, which is what the validator itself returns
    /// for most of these. The caller's argument — the address — was fine; a
    /// third party's document was not, and telling a client to fix its input
    /// when the input was correct sends it round a loop it cannot escape.
    /// `Security::parse` already draws that line for the plaintext case; this
    /// applies it to the rest.
    fn from_raw(raw: &probe::RawServer, email: &Address, default_port: u16) -> Result<Self, Error> {
        let host = validate::host(&raw.host).map_err(discovery_refused)?;
        let port = if raw.port.trim().is_empty() {
            default_port
        } else {
            let parsed: i64 = raw.port.trim().parse().map_err(|_| {
                Error::failed_precondition(format!(
                    "refusing a discovered port {:?}: it is not a number",
                    raw.port.chars().take(16).collect::<String>()
                ))
            })?;
            validate::port(parsed).map_err(discovery_refused)?
        };
        let security = Security::parse(&raw.security).map_err(discovery_refused)?;
        Ok(Self {
            host,
            port,
            security,
            username: email.expand_username(raw.username.as_deref()),
        })
    }
}

/// Restate a validation refusal as a *precondition* failure.
///
/// The value that failed did not come from the caller — see
/// [`ServerSettings::from_raw`]'s docs. The message is preserved verbatim,
/// because it already names the offending value and says why.
fn discovery_refused(error: Error) -> Error {
    match error.reason() {
        crate::ErrorReason::InvalidArgument => Error::failed_precondition(error.to_string()),
        _ => error,
    }
}

/// The email address being configured, split once and checked once.
#[derive(Debug, Clone)]
pub struct Address {
    /// The whole address.
    pub email: String,
    /// The part before the `@`.
    pub local: String,
    /// The part after, validated as a hostname.
    pub domain: String,
}

impl Address {
    /// Parse and validate an address.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] if it is not exactly one local part, one
    /// `@`, and one valid domain.
    pub fn parse(raw: &str) -> Result<Self, Error> {
        let email = raw.trim().to_owned();
        let (local, domain) = email.split_once('@').ok_or_else(|| {
            Error::invalid_argument("an email address must contain exactly one '@'")
        })?;
        if local.is_empty() || domain.is_empty() || domain.contains('@') {
            return Err(Error::invalid_argument(
                "an email address must contain exactly one '@', with text on both sides",
            ));
        }
        // The local part ends up in a TOML block and in a login; a control
        // character in it is either a mistake or an attempt at one.
        if local
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || c == '"')
        {
            return Err(Error::invalid_argument(
                "the local part of the address contains a control character",
            ));
        }
        let domain = validate::host(domain)?;
        Ok(Self {
            email: format!("{local}@{domain}"),
            local: local.to_owned(),
            domain,
        })
    }

    /// Expand a provider's username template.
    ///
    /// Mozilla autoconfig writes `%EMAILADDRESS%` or `%EMAILLOCALPART%`;
    /// anything else — including a literal username, and including no
    /// template at all — falls back to the whole address, which is what
    /// nearly every provider wants and what a user would have typed.
    ///
    /// A *literal* username is text from the document, and it ends up in a
    /// printed TOML block and on a terminal, so it is accepted only when it
    /// is short, printable ASCII. `char::is_control` alone would not do:
    /// U+202E (RIGHT-TO-LEFT OVERRIDE) is a format character, not a control
    /// one, and reverses everything printed after it. Falling back to the
    /// address for anything else is safe — it is the right answer for nearly
    /// every provider anyway.
    fn expand_username(&self, template: Option<&str>) -> String {
        /// A login name longer than this is not a login name.
        const MAX_USERNAME: usize = 128;
        match template.map(str::trim) {
            Some(t) if t.eq_ignore_ascii_case("%EMAILLOCALPART%") => self.local.clone(),
            Some(t)
                if !t.is_empty()
                    && t.len() <= MAX_USERNAME
                    && !t.eq_ignore_ascii_case("%EMAILADDRESS%")
                    && !t.contains('%')
                    && t.chars().all(|c| c.is_ascii_graphic()) =>
            {
                t.to_owned()
            }
            _ => self.email.clone(),
        }
    }
}

/// What the caller asked for.
#[derive(Debug, Clone)]
pub struct AutoconfigRequest {
    /// The address to configure.
    pub email: String,
    /// How to resolve the password, for the login check. [`CredentialSource::None`]
    /// skips it — and the proposal then says it was not verified.
    pub credential: CredentialSource,
    /// Whether the model fallback may run on a probe miss.
    pub allow_model_fallback: bool,
}

/// The answer: settings, a TOML block, and everything a human should know
/// before applying them.
#[derive(Debug, Clone)]
pub struct Proposal {
    /// Which probe answered.
    pub source: Source,
    /// The incoming server.
    pub imap: ServerSettings,
    /// The outgoing server, when one was discovered.
    pub smtp: Option<ServerSettings>,
    /// A ready-to-paste `[[accounts]]` block.
    pub toml: String,
    /// Whether a real login succeeded.
    pub login_validated: bool,
    /// Why validation did not run, or how it failed. Empty on success.
    pub validation_detail: String,
    /// An account already configured for this address, if any. Nothing about
    /// it was changed.
    pub existing_account_id: Option<i64>,
    /// Everything worth reading before applying this.
    pub warnings: Vec<String>,
}

/// The orchestrator.
#[derive(Clone)]
pub struct Autoconfigurator {
    db: Database,
    probes: Arc<Probes>,
    login: Arc<dyn LoginProbe>,
    inferrer: Option<infer::SettingsInferrer>,
}

impl std::fmt::Debug for Autoconfigurator {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Autoconfigurator")
            .field("model_fallback", &self.inferrer.is_some())
            .finish_non_exhaustive()
    }
}

impl Autoconfigurator {
    /// Build one over the real probes and the real login check.
    ///
    /// # Errors
    ///
    /// [`Error::FailedPrecondition`] if the HTTP client cannot be built.
    pub fn new(db: Database, endpoints: ProbeEndpoints) -> Result<Self, Error> {
        Ok(Self {
            db,
            probes: Arc::new(Probes::new(endpoints)?),
            login: Arc::new(TlsLoginProbe),
            inferrer: None,
        })
    }

    /// Build one over supplied probes and login check — the seam the suite
    /// drives, and the reason no test here reaches the network.
    #[must_use]
    pub fn with_parts(db: Database, probes: Arc<Probes>, login: Arc<dyn LoginProbe>) -> Self {
        Self {
            db,
            probes,
            login,
            inferrer: None,
        }
    }

    /// Enable the model fallback.
    ///
    /// Absent, `allow_model_fallback` is refused rather than silently
    /// ignored: a caller who asked for a guess and got "no configuration
    /// found" would reasonably conclude the model had nothing to offer.
    #[must_use]
    pub fn with_inferrer(mut self, inferrer: infer::SettingsInferrer) -> Self {
        self.inferrer = Some(inferrer);
        self
    }

    /// Discover, validate, verify, render.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] for a malformed address or a discovery that
    /// fails validation; [`Error::NotFound`] when nothing was discovered;
    /// [`Error::FailedPrecondition`] when the model fallback was asked for and
    /// is not wired, or a discovered server offers no encryption; otherwise
    /// whatever the model layer returned.
    #[tracing::instrument(skip(self, request, cancel), fields(domain), err)]
    pub async fn discover(
        &self,
        request: &AutoconfigRequest,
        cancel: &CancellationToken,
    ) -> Result<Proposal, Error> {
        let address = Address::parse(&request.email)?;
        tracing::Span::current().record("domain", address.domain.as_str());

        let report = self.probes.run(&address.domain, cancel).await;
        if cancel.is_cancelled() {
            return Err(Error::cancelled(
                "autoconfiguration was cancelled".to_owned(),
            ));
        }
        let mut warnings = Vec::new();
        let candidate = match report.candidate {
            Some(candidate) => candidate,
            None => {
                self.infer(&address, &report, request, &mut warnings, cancel)
                    .await?
            }
        };

        // Validation, identically for every source.
        let imap = ServerSettings::from_raw(&candidate.imap, &address, 993)?;
        let smtp = candidate
            .smtp
            .as_ref()
            .map(|raw| ServerSettings::from_raw(raw, &address, 587))
            .transpose()?;

        if imap.security.is_weaker_than(Security::Tls) {
            warnings.push(format!(
                "the discovered IMAP server offers {} rather than implicit TLS; rmail's IMAP \
                 client connects with implicit TLS only, so this setting will not sync as it \
                 stands",
                imap.security.as_str()
            ));
        }
        if smtp.is_none() {
            warnings.push(
                "no outgoing (SMTP) server was discovered; add `smtp_server`/`smtp_port` \
                 yourself before sending"
                    .to_owned(),
            );
        }

        let existing = self.existing_account(&address).await?;
        if let Some(account) = &existing {
            warnings.push(format!(
                "account {} ({:?}) is already configured for this address; nothing was \
                 changed, and applying this block is a decision to change it",
                account.id, account.name
            ));
            for difference in differences(account, &imap, smtp.as_ref()) {
                warnings.push(difference);
            }
        }

        let (login_validated, validation_detail) = self
            .verify(&imap, candidate.source, &request.credential, cancel)
            .await;

        let toml = render_toml(
            &address,
            &imap,
            smtp.as_ref(),
            &request.credential,
            candidate.source,
        );
        Ok(Proposal {
            source: candidate.source,
            imap,
            smtp,
            toml,
            login_validated,
            validation_detail,
            existing_account_id: existing.map(|a| a.id),
            warnings,
        })
    }

    /// The model fallback, with its two refusals: not wired, and not asked
    /// for.
    async fn infer(
        &self,
        address: &Address,
        report: &probe::ProbeReport,
        request: &AutoconfigRequest,
        warnings: &mut Vec<String>,
        cancel: &CancellationToken,
    ) -> Result<probe::RawCandidate, Error> {
        if !request.allow_model_fallback {
            return Err(Error::not_found(format!(
                "no autoconfiguration was published for {}; retry with the model fallback \
                 enabled to have Claude propose settings",
                address.domain
            )));
        }
        let Some(inferrer) = &self.inferrer else {
            return Err(Error::failed_precondition(
                "the model fallback was requested but this daemon has no AI provider \
                 configured"
                    .to_owned(),
            ));
        };
        let evidence = infer::Evidence {
            // The domain only. The local part never reaches the model — see
            // `infer`'s module docs.
            domain: address.domain.clone(),
            mx: report.mx.clone(),
            responses: report.responses.clone(),
        };
        let candidate = inferrer.infer(&address.email, &evidence, cancel).await?;
        warnings.push(
            "these settings were proposed by a language model, not published by the \
             provider; check them before you rely on them"
                .to_owned(),
        );
        Ok(candidate)
    }

    /// Verify by login, when there is a credential to verify with and a
    /// hostname somebody published.
    ///
    /// # Why a model-proposed host is never logged into
    ///
    /// Verification means resolving the user's real password and presenting
    /// it to whatever host the discovery named. That is fine for a host the
    /// *domain* published: the user is configuring that domain, and its
    /// administrator is who they are trusting either way. It is not fine for
    /// a host a language model produced from a corpus of attacker-controlled
    /// probe responses. Validation proves such a name is syntactically a
    /// public DNS name; it cannot prove it is the user's mail provider, and
    /// an attacker who can hold a valid certificate for a name they own would
    /// receive the password.
    ///
    /// `allow_model_fallback` is consent to *ask* the model, not consent to
    /// send the credential to its answer — and the answer arrives with a
    /// warning the human has not read yet at the moment this would run. So a
    /// model proposal comes back unverified, saying so.
    async fn verify(
        &self,
        imap: &ServerSettings,
        source: Source,
        credential: &CredentialSource,
        cancel: &CancellationToken,
    ) -> (bool, String) {
        if source == Source::Model {
            return (
                false,
                "not verified: these settings were proposed by a model, and rmail will not \
                 present your password to a host a model named — apply the block and run \
                 `mail account test` once you have checked it"
                    .to_owned(),
            );
        }
        if matches!(credential, CredentialSource::None) {
            return (
                false,
                "not verified: no credential reference was supplied, so no login was \
                 attempted"
                    .to_owned(),
            );
        }
        if matches!(credential, CredentialSource::OAuth(_)) {
            return (
                false,
                "not verified: an OAuth grant is authorized against a configured account, \
                 so it cannot be exercised before the account exists"
                    .to_owned(),
            );
        }
        // Resolving may run a command or prompt the Keychain; neither belongs
        // on the runtime, and the secret never leaves this scope.
        let source = credential.clone();
        let username = imap.username.clone();
        let resolved = tokio::task::spawn_blocking(move || source.resolve(Some(&username))).await;
        let secret = match resolved {
            Ok(Ok(Some(secret))) => secret,
            Ok(Ok(None)) => {
                return (
                    false,
                    "not verified: the credential reference resolved to nothing".to_owned(),
                )
            }
            Ok(Err(error)) => {
                return (
                    false,
                    format!("not verified: the credential could not be resolved: {error}"),
                )
            }
            Err(error) => {
                return (
                    false,
                    format!("not verified: the credential resolution task failed: {error}"),
                )
            }
        };
        if cancel.is_cancelled() {
            return (false, "not verified: cancelled".to_owned());
        }
        match self.login.login(imap, &imap.username, &secret).await {
            Ok(()) => (true, String::new()),
            // A failed login is *not* an error for the RPC: the discovery is
            // still the best answer available, and "here are the settings,
            // the login was refused" is more useful than a bare
            // UNAUTHENTICATED with no settings in it. It is reported, never
            // implied away.
            Err(error) => (false, format!("login failed: {error}")),
        }
    }

    /// An account already configured for this address, matched on the name or
    /// the username (the two places an address is conventionally written).
    async fn existing_account(&self, address: &Address) -> Result<Option<ExistingAccount>, Error> {
        let email = address.email.clone();
        let accounts = crate::account::list(&self.db).await?;
        Ok(accounts
            .into_iter()
            .find(|a| {
                a.name.eq_ignore_ascii_case(&email)
                    || a.username
                        .as_deref()
                        .is_some_and(|u| u.eq_ignore_ascii_case(&email))
            })
            .map(|a| ExistingAccount {
                id: a.id,
                name: a.name,
                imap_server: a.imap_server,
                imap_port: a.imap_port,
                smtp_server: a.smtp_server,
                smtp_port: a.smtp_port,
            }))
    }
}

/// The fields of an already-configured account this module compares against.
#[derive(Debug, Clone)]
struct ExistingAccount {
    id: i64,
    name: String,
    imap_server: Option<String>,
    imap_port: Option<u16>,
    smtp_server: Option<String>,
    smtp_port: Option<u16>,
}

/// Spell out every way the proposal differs from what is already configured.
///
/// The point is not tidiness. An operator who pastes a block over an existing
/// account is changing where their credentials are sent, and that change has
/// to be visible in the answer rather than discoverable by diffing two files.
fn differences(
    account: &ExistingAccount,
    imap: &ServerSettings,
    smtp: Option<&ServerSettings>,
) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(current) = &account.imap_server {
        if !current.eq_ignore_ascii_case(&imap.host) {
            out.push(format!(
                "this proposal points IMAP at {:?}; account {} currently uses {current:?}",
                imap.host, account.id
            ));
        }
    }
    if let Some(current) = account.imap_port {
        if current != imap.port {
            out.push(format!(
                "this proposal uses IMAP port {}; account {} currently uses {current}",
                imap.port, account.id
            ));
        }
    }
    if let (Some(current), Some(smtp)) = (&account.smtp_server, smtp) {
        if !current.eq_ignore_ascii_case(&smtp.host) {
            out.push(format!(
                "this proposal points SMTP at {:?}; account {} currently uses {current:?}",
                smtp.host, account.id
            ));
        }
    }
    if let (Some(current), Some(smtp)) = (account.smtp_port, smtp) {
        if current != smtp.port {
            out.push(format!(
                "this proposal uses SMTP port {}; account {} currently uses {current}",
                smtp.port, account.id
            ));
        }
    }
    out
}

/// One `[[accounts]]` entry, serialized by the TOML writer.
#[derive(Debug, Serialize)]
struct AccountBlock {
    name: String,
    imap_server: String,
    port: u16,
    username: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    smtp_server: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    smtp_port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    password_command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    password_env: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    keychain: Option<String>,
}

#[derive(Debug, Serialize)]
struct AccountsDocument {
    accounts: Vec<AccountBlock>,
}

/// Render the `[[accounts]]` block.
///
/// Serialized by the `toml` writer rather than formatted by hand, and that is
/// a security decision, not a style one: every string in here — the hostname
/// above all — arrived from a document someone else served. Hand-formatting
/// `imap_server = "{host}"` would make a quote or a newline in a discovered
/// value an injection into the user's configuration file. The validator
/// already refuses such a hostname; this is the second lock on the same door,
/// and the one that does not depend on the validator being complete.
fn render_toml(
    address: &Address,
    imap: &ServerSettings,
    smtp: Option<&ServerSettings>,
    credential: &CredentialSource,
    source: Source,
) -> String {
    let block = AccountBlock {
        name: address.email.clone(),
        imap_server: imap.host.clone(),
        port: imap.port,
        username: imap.username.clone(),
        smtp_server: smtp.map(|s| s.host.clone()),
        smtp_port: smtp.map(|s| s.port),
        password_command: match credential {
            CredentialSource::Command(cmd) => Some(cmd.clone()),
            _ => None,
        },
        password_env: match credential {
            CredentialSource::Env(var) => Some(var.clone()),
            _ => None,
        },
        keychain: match credential {
            CredentialSource::Keychain(service) => Some(service.clone()),
            _ => None,
        },
    };
    let document = AccountsDocument {
        accounts: vec![block],
    };
    // The header is rmail's own text with only a fixed `Source` string
    // interpolated, so it carries nothing from the network.
    let mut out = format!(
        "# Discovered by `mail account add` via {}.\n\
         # Nothing was written; paste this into rmail.toml to apply it.\n",
        source.as_str()
    );
    if matches!(credential, CredentialSource::None) {
        out.push_str(
            "# Add one of `password_command`, `password_env` or `keychain` — an account \
             with no credential cannot log in.\n",
        );
    }
    if matches!(credential, CredentialSource::OAuth(_)) {
        out.push_str(
            "# This address authenticates with OAuth: create the account, then run \
             `mail account login --oauth <provider>`.\n",
        );
    }
    match toml::to_string(&document) {
        Ok(rendered) => out.push_str(&rendered),
        // Unreachable with these field types, and a diagnostic beats a lie
        // that looks pasteable.
        Err(error) => {
            tracing::warn!(%error, "could not render the autoconfig TOML block");
            out.push_str("# could not render this configuration as TOML\n");
        }
    }
    out
}
