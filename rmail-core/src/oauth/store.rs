//! Where refresh tokens live.
//!
//! The durable store is the macOS Keychain, reached through the same
//! `security-framework` generic-password API [`crate::credential`] already uses
//! for IMAP passwords — one secret store for the whole product rather than a
//! second one invented here. A refresh token is a non-expiring bearer
//! credential for the user's entire mailbox, so the only acceptable place for
//! it is the one the operating system encrypts and gates behind the user's
//! login.
//!
//! It is deliberately **not** in SQLite. The database is a plain file in the
//! user's home directory that gets copied into backups, synced by file
//! sync tools, and opened by anyone debugging an index problem.

use std::collections::HashMap;
use std::sync::Mutex;

use crate::credential::Secret;
use crate::error::Error;

use super::provider::Provider;
use super::{now, TokenStatus};

/// Which stored credential a broker call refers to: the Keychain service name
/// and the account within it.
///
/// The pair matches `security-framework`'s generic-password addressing, and
/// the account half is the login username — which is also the `user=` field of
/// the XOAUTH2 string, so a mismatch between the two is impossible by
/// construction.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct StoreKey {
    /// Keychain service name (an account's `secret_ref`).
    pub service: String,
    /// Keychain account field — the mailbox login.
    pub account: String,
}

impl StoreKey {
    /// A key for `service` and `account`.
    #[must_use]
    pub fn new(service: impl Into<String>, account: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            account: account.into(),
        }
    }

    /// How to name this credential in a message a user reads.
    ///
    /// Neither half is secret — a Keychain service name and a mailbox address
    /// are both configuration — so this is safe in an error string, and the
    /// error is much harder to act on without it.
    #[must_use]
    pub fn describe(&self) -> String {
        format!("{} ({})", self.account, self.service)
    }
}

/// The tokens held for one account.
///
/// `Debug` is derived, which is safe *because* every secret field is a
/// [`Secret`] whose own `Debug` redacts. Adding a plain `String` token field
/// here would silently start printing it; `token_material_never_survives_debug`
/// in `tests.rs` is the check that keeps that from happening quietly.
#[derive(Debug, Clone)]
pub struct StoredTokens {
    /// Which provider issued them.
    pub provider: Provider,
    /// The OAuth client id the grant belongs to. Stored with the tokens
    /// because a refresh must present the same client that was authorized, and
    /// keeping it here means refreshing needs no configuration file.
    pub client_id: String,
    /// The client secret, for providers that demand one from a native client
    /// (Google's "Desktop app" type does). Absent for a true public client.
    pub client_secret: Option<Secret>,
    /// The durable credential.
    pub refresh_token: Secret,
    /// The current short-lived credential, if one has been obtained.
    pub access_token: Option<Secret>,
    /// When the access token expires (unix seconds).
    pub expires_at: i64,
    /// The scopes the provider granted.
    pub scopes: Vec<String>,
}

impl StoredTokens {
    /// Project onto the secret-free status a caller may be told.
    #[must_use]
    pub fn status(&self, refreshed: bool) -> TokenStatus {
        TokenStatus {
            provider: self.provider,
            expires_at: self.expires_at,
            scopes: self.scopes.clone(),
            refreshed,
        }
    }

    /// Seconds until the access token expires; zero once it has.
    #[must_use]
    pub fn expires_in(&self) -> i64 {
        self.expires_at.saturating_sub(now()).max(0)
    }

    /// The JSON blob written to the store.
    fn to_json(&self) -> Result<String, Error> {
        let wire = Wire {
            provider: self.provider.as_str().to_owned(),
            client_id: self.client_id.clone(),
            client_secret: self.client_secret.as_ref().map(|s| s.expose().to_owned()),
            refresh_token: self.refresh_token.expose().to_owned(),
            access_token: self.access_token.as_ref().map(|s| s.expose().to_owned()),
            expires_at: self.expires_at,
            scopes: self.scopes.clone(),
        };
        serde_json::to_string(&wire)
            .map_err(|e| Error::internal(format!("could not encode stored OAuth tokens: {e}")))
    }

    /// Parse a blob previously written by [`StoredTokens::to_json`].
    pub(super) fn from_json(raw: &str) -> Result<Self, Error> {
        let wire: Wire = serde_json::from_str(raw).map_err(|_| {
            // The blob is the secret; a parse error must not quote it, and the
            // serde message includes the surrounding input.
            Error::failed_precondition(
                "the stored OAuth credential is not in the expected format; \
                 authorize the account again",
            )
        })?;
        Ok(Self {
            // Re-mapped rather than propagated: `Provider::parse` answers
            // `InvalidArgument` because its usual caller is a user typing a
            // name, but here the bad name came out of the *store*, which is
            // the same corruption the line above reports as
            // `FailedPrecondition`. One class of failure, one reason.
            provider: Provider::parse(&wire.provider).map_err(|_| {
                Error::failed_precondition(
                    "the stored OAuth credential names an unknown provider; \
                     authorize the account again",
                )
            })?,
            client_id: wire.client_id,
            client_secret: wire.client_secret.map(Secret::new),
            refresh_token: Secret::new(wire.refresh_token),
            access_token: wire.access_token.map(Secret::new),
            expires_at: wire.expires_at,
            scopes: wire.scopes,
        })
    }
}

/// The on-disk shape. Separate from [`StoredTokens`] so the only place raw
/// token strings exist is this one struct, at the store boundary.
#[derive(serde::Serialize, serde::Deserialize)]
struct Wire {
    provider: String,
    client_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    client_secret: Option<String>,
    refresh_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    access_token: Option<String>,
    expires_at: i64,
    #[serde(default)]
    scopes: Vec<String>,
}

/// Custody of a set of OAuth tokens.
///
/// Synchronous, because the real implementation is a Keychain call that blocks
/// (and may put a prompt in front of the user); [`super::OAuthBroker`] is what
/// keeps those calls off the runtime threads.
pub trait TokenStore: Send + Sync + std::fmt::Debug {
    /// Read the tokens stored for `key`, or `None` if there are none.
    ///
    /// # Errors
    ///
    /// A store failure, or a stored blob that cannot be parsed.
    fn load(&self, key: &StoreKey) -> Result<Option<StoredTokens>, Error>;

    /// Write `tokens` for `key`, replacing anything already there.
    ///
    /// # Errors
    ///
    /// A store failure.
    fn save(&self, key: &StoreKey, tokens: &StoredTokens) -> Result<(), Error>;

    /// Remove `key`'s tokens. Removing something absent is not an error.
    ///
    /// # Errors
    ///
    /// A store failure.
    fn delete(&self, key: &StoreKey) -> Result<(), Error>;
}

/// The macOS Keychain-backed store.
#[derive(Debug, Default, Clone, Copy)]
pub struct KeychainTokenStore;

#[cfg(target_os = "macos")]
impl TokenStore for KeychainTokenStore {
    fn load(&self, key: &StoreKey) -> Result<Option<StoredTokens>, Error> {
        match security_framework::passwords::get_generic_password(&key.service, &key.account) {
            Ok(bytes) => {
                let raw = String::from_utf8(bytes).map_err(|_| {
                    Error::failed_precondition(
                        "the stored OAuth credential is not valid UTF-8; \
                         authorize the account again",
                    )
                })?;
                StoredTokens::from_json(&raw).map(Some)
            }
            // `errSecItemNotFound` is "never authorized", which is a state the
            // caller handles, not a failure. Distinguishing it from a real
            // Keychain error by code rather than by message, since the message
            // is localized.
            Err(e) if e.code() == ITEM_NOT_FOUND => Ok(None),
            // `FailedPrecondition`, not `Unauthenticated`: a locked keychain
            // or `errSecInteractionNotAllowed` is an environment the operator
            // must fix, and telling them to authorize again does not help —
            // the *next* authorization would fail to write for the same
            // reason.
            Err(e) => Err(Error::failed_precondition(format!(
                "could not read the OAuth credential from the keychain: {e}"
            ))),
        }
    }

    fn save(&self, key: &StoreKey, tokens: &StoredTokens) -> Result<(), Error> {
        let blob = tokens.to_json()?;
        security_framework::passwords::set_generic_password(
            &key.service,
            &key.account,
            blob.as_bytes(),
        )
        .map_err(|e| {
            Error::failed_precondition(format!(
                "could not write the OAuth credential to the keychain: {e}"
            ))
        })
    }

    fn delete(&self, key: &StoreKey) -> Result<(), Error> {
        match security_framework::passwords::delete_generic_password(&key.service, &key.account) {
            Ok(()) => Ok(()),
            Err(e) if e.code() == ITEM_NOT_FOUND => Ok(()),
            Err(e) => Err(Error::failed_precondition(format!(
                "could not remove the OAuth credential from the keychain: {e}"
            ))),
        }
    }
}

/// `errSecItemNotFound`. Named rather than inlined so the two call sites above
/// cannot drift.
#[cfg(target_os = "macos")]
const ITEM_NOT_FOUND: i32 = -25300;

/// Off macOS there is no Keychain, and rmail will not invent a weaker store
/// for a credential this powerful. Every method fails the same way, with the
/// same explanation.
#[cfg(not(target_os = "macos"))]
impl TokenStore for KeychainTokenStore {
    fn load(&self, _key: &StoreKey) -> Result<Option<StoredTokens>, Error> {
        Err(unsupported())
    }

    fn save(&self, _key: &StoreKey, _tokens: &StoredTokens) -> Result<(), Error> {
        Err(unsupported())
    }

    fn delete(&self, _key: &StoreKey) -> Result<(), Error> {
        Err(unsupported())
    }
}

#[cfg(not(target_os = "macos"))]
fn unsupported() -> Error {
    Error::failed_precondition(
        "OAuth refresh tokens are stored in the macOS Keychain, which is not \
         available on this platform",
    )
}

/// An in-process store, for tests and for a flow whose tokens are never meant
/// to outlive the process.
///
/// Not a fallback for the Keychain: nothing wires this up in production,
/// because "the refresh token survives only until the daemon restarts" would
/// mean re-consenting in a browser after every restart.
#[derive(Debug, Default)]
pub struct MemoryTokenStore {
    entries: Mutex<HashMap<StoreKey, String>>,
}

impl MemoryTokenStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn entries(&self) -> std::sync::MutexGuard<'_, HashMap<StoreKey, String>> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl TokenStore for MemoryTokenStore {
    fn load(&self, key: &StoreKey) -> Result<Option<StoredTokens>, Error> {
        // Round-tripped through the same JSON the Keychain holds rather than
        // cloning a `StoredTokens`, so the encode/decode path is exercised by
        // every test that uses this store instead of only by the one that
        // tests it directly.
        match self.entries().get(key) {
            Some(raw) => StoredTokens::from_json(raw).map(Some),
            None => Ok(None),
        }
    }

    fn save(&self, key: &StoreKey, tokens: &StoredTokens) -> Result<(), Error> {
        let blob = tokens.to_json()?;
        self.entries().insert(key.clone(), blob);
        Ok(())
    }

    fn delete(&self, key: &StoreKey) -> Result<(), Error> {
        self.entries().remove(key);
        Ok(())
    }
}
