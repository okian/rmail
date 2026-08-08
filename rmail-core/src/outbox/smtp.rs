//! SMTP submission, and the one decision that matters here: whether a failure
//! is worth retrying.
//!
//! [`SmtpSender`] is a trait rather than a concrete type for the usual
//! reason — the scheduler's tests need a sender that fails on demand — but
//! also because the interesting behavior in this module is not "how do I open
//! a socket", it is the classification below.
//!
//! # 4xx versus 5xx is the whole game
//!
//! prd.md: *transient (4xx/offline) → backoff, stay `scheduled`; permanent
//! (5xx/auth/invalid recipient) → `failed`*. Getting this backwards is
//! expensive in both directions. Calling a 5xx transient means rmail retries a
//! permanently-rejected message five times over half an hour and only then
//! tells the user, who has by then walked away. Calling a 4xx — or a closed
//! laptop — permanent means a message the server explicitly asked us to try
//! again later is marked failed and silently not delivered, which is the
//! failure mode prd.md calls out by name ("not `failed` due purely to being
//! offline until `max_retries`").
//!
//! So the default is **transient**. Every unclassifiable error — a refused
//! connection, a dropped socket, a TLS handshake that did not complete, a
//! response we could not parse — retries. Only an error the peer explicitly
//! made permanent, or one caused by our own malformed request (which retrying
//! would reproduce byte for byte), becomes `failed`.
//!
//! # A returned error means nothing was queued
//!
//! Both variants of [`SendFailure`] carry that guarantee, and the outbox
//! relies on it: SMTP is strictly request/response, so an error returned from
//! `send_raw` means the peer answered and did not accept the message. That is
//! what lets [`super::OutboxStore::mark_transient_failure`] clear the
//! at-most-once fence and retry for real. The case where nothing is returned
//! at all — the process dies mid-`DATA` — is handled entirely by the fence and
//! never reaches this module.
//!
//! # Connections are pooled per account
//!
//! prd.md asks for per-account connection reuse, and lettre's own pool does it
//! — but only within one `AsyncSmtpTransport`, so the transport has to
//! outlive the send. [`LettreSender`] therefore caches one per account. The
//! cost is that the account's resolved password stays in memory for the life
//! of the daemon rather than for the life of one send; the alternative is a
//! full TCP+TLS+AUTH handshake per message, which for an outbox draining a
//! backlog is both slow and the kind of behavior submission relays rate-limit.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use lettre::address::{Address, Envelope};
use lettre::transport::smtp::authentication::Credentials;
use lettre::transport::smtp::client::Tls;
use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor};

use crate::error::Error;
use crate::storage::Database;

pub use crate::config::SmtpSecurity;

/// Ceiling on one SMTP conversation.
///
/// Generous, because it covers the whole exchange including `DATA` for a
/// multi-megabyte attachment over a poor link, and a timeout here is a
/// *transient* failure that costs a backoff — but bounded, because a wedged
/// server must not hold a worker (one of only [`super::SendPolicy::workers`])
/// forever.
pub const SMTP_DEADLINE: Duration = Duration::from_secs(120);

/// The SMTP envelope: who the message is from, and every `RCPT TO`.
///
/// Deliberately separate from the message octets. The envelope is where blind
/// recipients live — `compose::mime::build` emits no `Bcc` header — so
/// conflating the two is exactly how a `Bcc` list leaks into a delivered
/// message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendEnvelope {
    /// The `MAIL FROM` addr-spec.
    pub from: String,
    /// Every `RCPT TO` addr-spec, including blind recipients.
    pub to: Vec<String>,
}

/// Why a transmission did not happen.
///
/// [`SendFailure::Transient`] and [`SendFailure::Permanent`] both mean the
/// same thing about delivery — **nothing was queued** — and differ only in
/// whether trying again could help. [`SendFailure::Indeterminate`] is the
/// third case, and the reason this is not a two-variant enum: the peer never
/// answered, so whether it queued the message is *unknown*.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SendFailure {
    /// Try again later: a 4xx reply, or any failure that happened before the
    /// session was established. Nothing was queued.
    #[error("temporary SMTP failure: {0}")]
    Transient(String),
    /// Do not try again: a 5xx reply, a rejected credential, an address the
    /// server will never accept, or a request this build cannot form.
    #[error("permanent SMTP failure: {0}")]
    Permanent(String),
    /// The connection was established and then died without a reply — a
    /// timeout waiting for a response, a socket closed mid-session, an
    /// unparseable answer.
    ///
    /// This is *not* a transient failure, and treating it as one is how a
    /// recipient gets two copies: if the peer had already accepted the
    /// message and only its `250` was lost, retransmitting delivers a
    /// duplicate. Nothing on this side can distinguish "it never arrived"
    /// from "the acknowledgement never came back", so the fence is kept and
    /// the row resolves through the same at-most-once path a process crash
    /// takes. See the module docs' at-most-once section.
    #[error("indeterminate SMTP failure: {0}")]
    Indeterminate(String),
}

impl SendFailure {
    /// The message, for `outbox.last_error`.
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            Self::Transient(message) | Self::Permanent(message) | Self::Indeterminate(message) => {
                message
            }
        }
    }

    /// Whether a retry could succeed.
    #[must_use]
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Transient(_))
    }
}

impl From<SendFailure> for Error {
    fn from(failure: SendFailure) -> Self {
        match failure {
            // Retryable upstream trouble is exactly what `Unavailable` means
            // in this codebase's error contract.
            SendFailure::Transient(message) => Error::unavailable(message),
            // Not `InvalidArgument`: by the time a send is transmitted the
            // request that created it is long gone, and the caller of an
            // RPC that surfaces this (`SendNow`) did nothing wrong — the
            // system is in a state that makes the send impossible.
            SendFailure::Permanent(message) => Error::failed_precondition(message),
            // The send may or may not have landed; the caller must not be
            // told it definitely failed, and must not retry on its own.
            SendFailure::Indeterminate(message) => Error::unavailable(message),
        }
    }
}

/// Transmits a rendered message.
///
/// One call is one complete submission: connect (or reuse), authenticate,
/// `MAIL FROM`/`RCPT TO`/`DATA`, done. There is no session for a caller to
/// hold, which is what makes every call independently retryable.
#[async_trait::async_trait]
pub trait SmtpSender: Send + Sync + std::fmt::Debug {
    /// Transmit `raw_mime` to `envelope`'s recipients, as `account_id`.
    ///
    /// # Errors
    ///
    /// [`SendFailure`], classified — see the module docs. Whichever variant
    /// comes back, the message was **not** queued by the peer.
    async fn send(
        &self,
        account_id: i64,
        envelope: &SendEnvelope,
        raw_mime: &[u8],
    ) -> Result<(), SendFailure>;
}

/// The real sender: `lettre` over the account's configured submission server.
#[derive(Debug)]
pub struct LettreSender {
    db: Database,
    security: SmtpSecurity,
    /// One transport per account, so lettre's pool can actually reuse a
    /// connection across sends — see the module docs.
    transports: Mutex<HashMap<i64, AsyncSmtpTransport<Tokio1Executor>>>,
}

impl LettreSender {
    /// Build a sender resolving accounts from `db`.
    #[must_use]
    pub fn new(db: Database, security: SmtpSecurity) -> Self {
        Self {
            db,
            security,
            transports: Mutex::new(HashMap::new()),
        }
    }

    /// The transport for `account_id`, building and caching one if needed.
    async fn transport(
        &self,
        account_id: i64,
    ) -> Result<AsyncSmtpTransport<Tokio1Executor>, SendFailure> {
        // The lock is never held across an await: the cached lookup and the
        // insert are each a bounded critical section, and building a
        // transport (which resolves a credential, possibly by running a shell
        // command) happens outside both. Two racing builds for the same
        // account produce two equivalent transports and one of them is
        // dropped, which is cheaper than serializing every send behind a
        // credential prompt.
        if let Some(cached) = self.lock().get(&account_id).cloned() {
            return Ok(cached);
        }
        let built = self.build_transport(account_id).await?;
        Ok(self.lock().entry(account_id).or_insert(built).clone())
    }

    /// The cache, recovering from a poisoned lock rather than propagating it.
    ///
    /// A poisoned lock means another task panicked mid-insert. This is a
    /// cache, so the worst case is one stale entry; refusing to send mail for
    /// the rest of the process's life is not a better answer to that.
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<i64, AsyncSmtpTransport<Tokio1Executor>>> {
        self.transports
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    async fn build_transport(
        &self,
        account_id: i64,
    ) -> Result<AsyncSmtpTransport<Tokio1Executor>, SendFailure> {
        let account = crate::account::get(&self.db, account_id)
            .await
            .map_err(|error| classify_core_error(&error))?;
        let host = account.smtp_server.clone().ok_or_else(|| {
            SendFailure::Permanent(format!(
                "account {account_id} has no SMTP server configured"
            ))
        })?;
        let port = account.smtp_port.unwrap_or(587);

        let mut builder = match self.security.resolve(port) {
            SmtpSecurity::ImplicitTls => AsyncSmtpTransport::<Tokio1Executor>::relay(&host)
                .map_err(|error| classify_smtp_error(&error))?,
            SmtpSecurity::Starttls => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&host)
                .map_err(|error| classify_smtp_error(&error))?,
            // No TLS at all. Reachable only when an operator asked for it by
            // name (`send.smtp_security = "plaintext"`), for the local-MTA
            // case; `Auto` never resolves to it.
            SmtpSecurity::Plaintext => {
                AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(&host).tls(Tls::None)
            }
            // `resolve` never returns `Auto`; handled rather than asserted so
            // adding a variant later is a compile error here, not a runtime
            // surprise.
            SmtpSecurity::Auto => AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&host)
                .map_err(|error| classify_smtp_error(&error))?,
        };
        builder = builder.port(port).timeout(Some(SMTP_DEADLINE));

        // Resolved on a blocking thread — a `password_command` or a keychain
        // prompt can block for a long time — exactly as `imap::conn` does.
        let credential = account.credential.clone();
        let username = account.username.clone();
        let secret = tokio::task::spawn_blocking({
            let username = username.clone();
            move || credential.resolve(username.as_deref())
        })
        .await
        .map_err(|error| SendFailure::Transient(format!("credential resolution failed: {error}")))?
        .map_err(|error| classify_core_error(&error))?;

        match (username, secret) {
            (Some(username), Some(secret)) => {
                builder =
                    builder.credentials(Credentials::new(username, secret.expose().to_owned()));
            }
            // Unauthenticated submission. Legitimate for a local MTA, and a
            // misconfiguration for anything else — but the server is the
            // authority on that, and its 530/535 is a clearer message than
            // any guess made here would be.
            _ => tracing::warn!(
                account_id,
                "no SMTP credential configured; submitting unauthenticated"
            ),
        }
        Ok(builder.build())
    }
}

#[async_trait::async_trait]
impl SmtpSender for LettreSender {
    #[tracing::instrument(skip(self, raw_mime), fields(account_id, bytes = raw_mime.len()))]
    async fn send(
        &self,
        account_id: i64,
        envelope: &SendEnvelope,
        raw_mime: &[u8],
    ) -> Result<(), SendFailure> {
        let envelope = to_lettre_envelope(envelope)?;
        let transport = self.transport(account_id).await?;
        transport
            .send_raw(&envelope, raw_mime)
            .await
            .map(|response| {
                tracing::debug!(code = ?response.code(), "SMTP accepted the message");
            })
            .map_err(|error| classify_smtp_error(&error))
    }
}

/// Translate the outbox's envelope into lettre's.
///
/// An address the outbox stored but lettre cannot parse is permanent: the
/// octets are frozen, so every retry produces the identical rejection.
fn to_lettre_envelope(envelope: &SendEnvelope) -> Result<Envelope, SendFailure> {
    let from = parse_address(&envelope.from)?;
    let to = envelope
        .to
        .iter()
        .map(|addr| parse_address(addr))
        .collect::<Result<Vec<_>, _>>()?;
    if to.is_empty() {
        return Err(SendFailure::Permanent(
            "the message names no envelope recipient".to_owned(),
        ));
    }
    Envelope::new(Some(from), to)
        .map_err(|error| SendFailure::Permanent(format!("invalid SMTP envelope: {error}")))
}

fn parse_address(addr: &str) -> Result<Address, SendFailure> {
    addr.parse::<Address>()
        .map_err(|error| SendFailure::Permanent(format!("invalid address {addr:?}: {error}")))
}

/// Decide whether a *local* failure on the way to SMTP — resolving the
/// account, running a `password_command` — should cost the message its
/// attempt budget.
///
/// The direction is the same one [`classify_smtp_error`] runs in and for the
/// same reason: only a condition that will still be true on the next attempt
/// is permanent. A missing account or a rejected keychain item is; a busy
/// database, a timeout, or an internal hiccup is not, and calling one of those
/// permanent turns a five-second blip into a message that never goes out.
///
/// [`crate::Error::Internal`]'s detail deliberately stays server-side (it is
/// replaced with a generic message on its way to a `Status`), and
/// `outbox.last_error` is read by any `mail.read` client, so that one variant
/// is summarized rather than quoted.
#[must_use]
fn classify_core_error(error: &Error) -> SendFailure {
    use crate::ErrorReason::{
        AlreadyExists, DeadlineExceeded, FailedPrecondition, Internal, InvalidArgument, NotFound,
        OutOfRange, PermissionDenied, ResourceExhausted, Unauthenticated, Unavailable,
    };
    match error.reason() {
        NotFound | FailedPrecondition | InvalidArgument | PermissionDenied | Unauthenticated
        | AlreadyExists | OutOfRange => SendFailure::Permanent(error.to_string()),
        Unavailable | DeadlineExceeded | ResourceExhausted => {
            SendFailure::Transient(error.to_string())
        }
        Internal => {
            tracing::error!(%error, "an internal error blocked a send");
            SendFailure::Transient("an internal error blocked this send".to_owned())
        }
    }
}

/// Decide whether an SMTP error is worth retrying.
///
/// The ordering is deliberate: an explicit reply code from the peer outranks
/// everything else, because the peer is the authority on whether it will ever
/// accept this message. Below that, an error caused by our own request is
/// permanent (a retry reproduces it exactly), and **everything else is
/// transient** — see the module docs on why that default runs in this
/// direction.
#[must_use]
pub fn classify_smtp_error(error: &lettre::transport::smtp::Error) -> SendFailure {
    let detail = describe(error);
    if error.is_permanent() {
        return SendFailure::Permanent(detail);
    }
    if error.is_transient() {
        return SendFailure::Transient(detail);
    }
    // We waited and the peer never answered. If that wait was for the reply
    // to `DATA`, the message may already be queued on the far side, and a
    // retry would be a second copy. Connection-refused and friends fall
    // through to the transient default below instead, because those provably
    // never got as far as a session -- calling *those* indeterminate would
    // trade duplicates for silently undelivered mail, which is worse.
    if error.is_timeout() {
        return SendFailure::Indeterminate(detail);
    }
    // Our own request was malformed (an unencodable address, a relay name
    // that is not a hostname). Retrying re-sends the same bytes.
    if error.is_client() {
        return SendFailure::Permanent(detail);
    }
    // Connection refused, socket closed, TLS handshake failed, unparseable
    // reply: every one of these is what "the laptop was closed" looks like
    // from here.
    SendFailure::Transient(detail)
}

/// A one-line description of an SMTP error, with its reply code when it has
/// one.
///
/// The code is spelled out because `last_error` is what the user reads in
/// `mail outbox --state failed`, and "550" versus "451" is the difference
/// between "fix the address" and "wait".
fn describe(error: &lettre::transport::smtp::Error) -> String {
    match error.status() {
        Some(code) => format!("{code} {error}"),
        None => error.to_string(),
    }
}

#[cfg(test)]
mod tests;
