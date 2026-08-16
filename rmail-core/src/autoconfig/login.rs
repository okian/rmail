//! Verifying a discovery by actually logging in.
//!
//! A discovery is a claim about someone else's server. The only thing that
//! settles it is a login, so this is the step that turns "the domain says its
//! IMAP host is X" into "X accepted these credentials" — and it is also the
//! step that catches the failure mode a validator cannot: a syntactically
//! perfect hostname that is not the user's mail server at all.
//!
//! # Why this is a trait
//!
//! The real implementation opens a TLS connection to a host the *document*
//! named. A test must never do that, so the probe is injected: the suite
//! supplies one that speaks real IMAP over a plaintext socket to the
//! in-process mock server ([`crate::imap::mock`]), exercising the same
//! `async-imap` login path against a server it controls.

use std::fmt::Debug;

use async_trait::async_trait;

use crate::credential::Secret;
use crate::error::Error;
use crate::imap::conn;

use super::{Security, ServerSettings};

/// A login attempt against a discovered server.
#[async_trait]
pub trait LoginProbe: Send + Sync + Debug {
    /// Attempt a login. `Ok(())` means the server accepted these credentials.
    ///
    /// # Errors
    ///
    /// [`Error::Unauthenticated`] if the server rejected the credentials,
    /// [`Error::Unavailable`] if it could not be reached, and
    /// [`Error::FailedPrecondition`] if this probe cannot verify the settings
    /// at all (see [`TlsLoginProbe`] on STARTTLS).
    async fn login(
        &self,
        settings: &ServerSettings,
        username: &str,
        secret: &Secret,
    ) -> Result<(), Error>;
}

/// The real probe: implicit TLS, `LOGIN`, `LOGOUT`.
#[derive(Debug, Default, Clone, Copy)]
pub struct TlsLoginProbe;

#[async_trait]
impl LoginProbe for TlsLoginProbe {
    async fn login(
        &self,
        settings: &ServerSettings,
        username: &str,
        secret: &Secret,
    ) -> Result<(), Error> {
        if settings.security != Security::Tls {
            // Not a silent pass. `Autoconfigure` reports `login_validated =
            // false` with this text, so a STARTTLS-only discovery is returned
            // as an unverified proposal rather than one that quietly skipped
            // its verification. (rmail's own IMAP client speaks implicit TLS
            // only — see `imap::conn::connect_tls` — so a STARTTLS-only
            // server would not sync either; that is worth telling the user
            // before they paste the block, not after.)
            return Err(Error::failed_precondition(format!(
                "cannot verify a {} server by login: rmail's IMAP client connects with \
                 implicit TLS only",
                settings.security.as_str()
            )));
        }
        // The whole exchange is bounded, like every other IMAP round trip in
        // this crate: the host came from a document, and a host that never
        // finishes a handshake must not hold an RPC open.
        tokio::time::timeout(crate::imap::IMAP_DEADLINE, async {
            let stream = conn::connect_tls(&settings.host, settings.port).await?;
            let mut session = conn::login(stream, username, secret.expose()).await?;
            // Best-effort: the connection is dropped either way, and a server
            // that accepted the login has already answered the question.
            let _ = session.logout().await;
            Ok::<(), Error>(())
        })
        .await
        .map_err(|_| {
            Error::deadline_exceeded(format!(
                "verifying {}:{} timed out",
                settings.host, settings.port
            ))
        })?
    }
}
