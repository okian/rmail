//! Where `mail auth login`'s cached session token lives (client_auth).
//!
//! Mirrors `rmail_core::oauth::store`'s shape exactly: a small trait with a
//! real macOS Keychain implementation, an explicit-refusal stub everywhere
//! else, and an in-memory double for tests — see that module's own docs for
//! why a refresh token gets the real Keychain and nothing weaker. A
//! `client_auth` session token is the same class of secret (a bare bearer
//! credential good for full local access until it expires), so it gets the
//! same answer, independently arrived at here rather than by sharing code
//! with `oauth::store`: that module's `StoredTokens` is OAuth-shaped
//! (provider, client id, refresh/access pair), and forcing this crate's one
//! opaque string through that shape would be a second, unrelated feature
//! reaching into a type that does not describe it.
//!
//! Keyed by the daemon's socket path, not a fixed name: a session token for
//! one `$RMAIL_SOCKET` must never be replayed against a different daemon
//! `--socket` happens to point at next.

use std::fmt;
use std::path::Path;

/// The Keychain service name every entry this module writes uses. The
/// *account* field (see [`account_for`]) is what actually distinguishes one
/// daemon's session from another's.
///
/// Read only by the real (macOS) [`SessionStore`] impl below — on every
/// other platform the stub never reaches the Keychain at all, so this is
/// legitimately unused there in a production (non-test) build; see that
/// impl block's own `#[cfg]`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const KEYCHAIN_SERVICE: &str = "rmail-client-auth-session";

/// A cached session: the bearer secret `mail auth login` obtained, when it
/// expires (unix seconds, matching `LoginPasswordResponse.expires_at`), and
/// its token id (matching `LoginPasswordResponse.id`) — carried specifically
/// so `mail auth logout` can call `AdminService.RevokeToken` and actually end
/// the session at the daemon, not just forget it locally.
#[derive(Clone)]
pub(crate) struct CachedSession {
    pub(crate) token: String,
    pub(crate) expires_at: i64,
    pub(crate) token_id: i64,
}

impl fmt::Debug for CachedSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CachedSession")
            .field("token", &"***")
            .field("expires_at", &self.expires_at)
            .field("token_id", &self.token_id)
            .finish()
    }
}

// `to_blob`/`from_blob` are called by the real (macOS) `SessionStore` impl
// below and, on every platform, by `tests::MemoryStore` — never by the
// non-macOS stub impl, so a non-macOS *production* build sees them as
// unreachable. Same reasoning as `KEYCHAIN_SERVICE`'s own `#[cfg_attr]`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
impl CachedSession {
    /// Serialize to the blob a Keychain generic-password item stores.
    /// `token<TAB>expires_at<TAB>token_id` rather than JSON: the one field
    /// with any structure worth naming is a secret, so a full serde round
    /// trip would be ceremony over three scalars.
    fn to_blob(&self) -> String {
        format!("{}\t{}\t{}", self.token, self.expires_at, self.token_id)
    }

    /// The inverse of [`Self::to_blob`]. `None` for anything that does not
    /// have the exact shape this module ever wrote — a corrupt or
    /// hand-edited entry must be treated as absent, not panic or feed a
    /// garbage token to a request.
    fn from_blob(raw: &str) -> Option<Self> {
        let mut parts = raw.split('\t');
        let token = parts.next()?;
        let expires_at = parts.next()?;
        let token_id = parts.next()?;
        if parts.next().is_some() || token.is_empty() {
            return None;
        }
        Some(Self {
            token: token.to_owned(),
            expires_at: expires_at.parse().ok()?,
            token_id: token_id.parse().ok()?,
        })
    }
}

/// A store failure. Deliberately has no "not supported on this platform"
/// variant of its own — every call site already treats any `Err` here the
/// same way (fall back to printing the token / proceeding with none
/// cached), so a platform that cannot cache at all and a Keychain that is
/// temporarily locked are the same case as far as anything downstream needs
/// to know.
#[derive(Debug, Clone)]
pub(crate) struct SessionStoreError(pub(crate) String);

impl fmt::Display for SessionStoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SessionStoreError {}

trait SessionStore: Send + Sync {
    fn load(&self, account: &str) -> Result<Option<CachedSession>, SessionStoreError>;
    fn save(&self, account: &str, session: &CachedSession) -> Result<(), SessionStoreError>;
    fn clear(&self, account: &str) -> Result<(), SessionStoreError>;
}

struct KeychainStore;

#[cfg(target_os = "macos")]
impl SessionStore for KeychainStore {
    fn load(&self, account: &str) -> Result<Option<CachedSession>, SessionStoreError> {
        match security_framework::passwords::get_generic_password(KEYCHAIN_SERVICE, account) {
            Ok(bytes) => {
                let raw = String::from_utf8(bytes)
                    .map_err(|_| SessionStoreError("stored session is not valid UTF-8".into()))?;
                Ok(CachedSession::from_blob(&raw))
            }
            // `errSecItemNotFound`: never logged in, not a failure.
            Err(e) if e.code() == ITEM_NOT_FOUND => Ok(None),
            Err(e) => Err(SessionStoreError(format!(
                "could not read the cached session from the keychain: {e}"
            ))),
        }
    }

    fn save(&self, account: &str, session: &CachedSession) -> Result<(), SessionStoreError> {
        security_framework::passwords::set_generic_password(
            KEYCHAIN_SERVICE,
            account,
            session.to_blob().as_bytes(),
        )
        .map_err(|e| SessionStoreError(format!("could not write the session to the keychain: {e}")))
    }

    fn clear(&self, account: &str) -> Result<(), SessionStoreError> {
        match security_framework::passwords::delete_generic_password(KEYCHAIN_SERVICE, account) {
            Ok(()) => Ok(()),
            Err(e) if e.code() == ITEM_NOT_FOUND => Ok(()),
            Err(e) => Err(SessionStoreError(format!(
                "could not remove the session from the keychain: {e}"
            ))),
        }
    }
}

#[cfg(target_os = "macos")]
const ITEM_NOT_FOUND: i32 = -25300;

/// Off macOS there is no Keychain, and — same call `oauth::store` makes for
/// a refresh token — rmail will not invent a weaker place to put a
/// credential this powerful. `mail auth login` still succeeds; it just
/// cannot remember the result, and says so.
#[cfg(not(target_os = "macos"))]
impl SessionStore for KeychainStore {
    fn load(&self, _account: &str) -> Result<Option<CachedSession>, SessionStoreError> {
        Err(unsupported())
    }

    fn save(&self, _account: &str, _session: &CachedSession) -> Result<(), SessionStoreError> {
        Err(unsupported())
    }

    fn clear(&self, _account: &str) -> Result<(), SessionStoreError> {
        Err(unsupported())
    }
}

#[cfg(not(target_os = "macos"))]
fn unsupported() -> SessionStoreError {
    SessionStoreError(
        "client-auth sessions are cached in the macOS Keychain, which is not available on this \
         platform"
            .to_owned(),
    )
}

/// The Keychain *account* for `socket` — the daemon this session belongs to.
fn account_for(socket: &Path) -> String {
    socket.display().to_string()
}

/// Load the cached session for the daemon at `socket`, if any and if it has
/// not expired.
///
/// Never surfaces an error: a caller reaching this is already deciding
/// whether to attach a bearer token to an outgoing request, and the answer
/// to "the keychain is unreadable right now" is the same as "nothing is
/// cached" — proceed with none, and let the daemon's own `UNAUTHENTICATED`
/// (if the call turns out to need one) be the message the user sees.
pub(crate) fn load(socket: &Path) -> Option<CachedSession> {
    load_from(&KeychainStore, socket)
}

/// Cache `session` for the daemon at `socket`.
///
/// # Errors
///
/// A [`SessionStoreError`] — see the type's own docs for why callers should
/// treat every variant of "could not cache it" the same way.
pub(crate) fn save(socket: &Path, session: &CachedSession) -> Result<(), SessionStoreError> {
    save_to(&KeychainStore, socket, session)
}

/// Forget the cached session for the daemon at `socket`, if any.
///
/// # Errors
///
/// As [`save`].
pub(crate) fn clear(socket: &Path) -> Result<(), SessionStoreError> {
    clear_from(&KeychainStore, socket)
}

/// [`load`] against an injected store — split out so the tests below can
/// exercise the expiry check and the round trip through an in-memory double
/// rather than the real Keychain, which Linux CI (this workspace's Docker
/// test container) cannot reach at all and a hermetic test suite should not
/// depend on regardless of platform.
fn load_from(store: &dyn SessionStore, socket: &Path) -> Option<CachedSession> {
    let session = store.load(&account_for(socket)).ok().flatten()?;
    if session.expires_at <= now_unix() {
        return None;
    }
    Some(session)
}

/// [`save`], with the store injected — see [`load_from`].
fn save_to(
    store: &dyn SessionStore,
    socket: &Path,
    session: &CachedSession,
) -> Result<(), SessionStoreError> {
    store.save(&account_for(socket), session)
}

/// [`clear`], with the store injected — see [`load_from`].
fn clear_from(store: &dyn SessionStore, socket: &Path) -> Result<(), SessionStoreError> {
    store.clear(&account_for(socket))
}

/// Current time as unix seconds, saturating rather than panicking — mirrors
/// `rmail_core::auth::now_unix`, reimplemented rather than exposed across
/// the crate boundary for one call.
fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    i64::try_from(secs).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests;
