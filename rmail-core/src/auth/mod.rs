//! Capability tokens: mint, list, revoke, and verify (task 38).
//!
//! A token's plaintext secret exists exactly once — in the string returned by
//! [`mint`] — and never again. Only its argon2id hash is persisted
//! (`api_tokens.token_hash`); [`verify`] recomputes the hash of a presented
//! secret and compares it to the stored one via the `argon2`/`password-hash`
//! crates, which perform that comparison in constant time. This module has no
//! opinion on *how* a caller obtained the presented string — the daemon's
//! `rmaild::auth` layer is what decides a Unix-socket peer gets [`Scope::Admin`]
//! for free while a TCP caller must present one of these.
//!
//! # Token format
//!
//! `rmail_tok_<id>_<64 hex chars>`. The id is the row's primary key, present
//! so [`verify`] can fetch the *one* candidate row by an indexed point lookup
//! rather than argon2-hashing the presented secret against every stored
//! token — the id is not secret, only the 32 random bytes after it are.

mod repo;
pub mod scope;

pub use scope::{satisfies, Scope, ScopeParseError};

use argon2::password_hash::rand_core::{OsRng, RngCore};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{Error, Result};
use crate::storage::Database;

/// Prefix on every minted bearer token; also what [`parse_presented`] strips.
const TOKEN_PREFIX: &str = "rmail_tok_";

/// Random secret length in bytes (256 bits), hex-encoded to 64 characters.
const SECRET_BYTES: usize = 32;

/// How stale `last_used_at` must be before a successful [`verify`] bothers
/// updating it.
///
/// Every verification that skips this write also skips taking the process's
/// single-writer lock ([`Database::write`]) — on the bearer-token path (the
/// only path that reaches here; the Unix-peer-uid path never touches the
/// database) that lock is shared with the sync engine and message ingest.
/// `last_used_at` is a low-precision "is this token still in use" signal, not
/// an audit trail (successes/failures should eventually get one — see
/// `rmaild::auth`), so coalescing repeat writes within this window costs
/// nothing anyone reads.
const LAST_USED_THROTTLE_SECS: i64 = 60;

/// A persisted capability token's metadata — never the secret or its hash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiToken {
    /// Stable id (also the prefix embedded in the bearer string).
    pub id: i64,
    /// Human-readable label.
    pub name: String,
    /// Granted scopes.
    pub scopes: Vec<Scope>,
    /// Mint time (unix seconds).
    pub created_at: i64,
    /// Last successful verification (unix seconds), if any.
    pub last_used_at: Option<i64>,
    /// Expiry (unix seconds); `None` means no expiry.
    pub expires_at: Option<i64>,
    /// Whether the token has been revoked.
    pub revoked: bool,
}

impl ApiToken {
    fn from_repo(row: repo::ApiTokenRow) -> Result<Self> {
        let scopes = Scope::parse_list(&row.scopes).map_err(|e| {
            // A stored scope string that fails to parse means either this
            // process's own `mint` wrote something [`Scope::parse_list`]
            // cannot read back, or the row was edited out-of-band — either
            // way it is a bug/corruption, not a bad *request*, so it maps to
            // `Internal` rather than `InvalidArgument`.
            Error::internal(format!(
                "stored token {} has unparseable scopes: {e}",
                row.id
            ))
        })?;
        Ok(Self {
            id: row.id,
            name: row.name,
            scopes,
            created_at: row.created_at,
            last_used_at: row.last_used_at,
            expires_at: row.expires_at,
            revoked: row.revoked,
        })
    }
}

/// Fields for minting a new token.
#[derive(Debug, Clone)]
pub struct NewToken {
    /// Human-readable label (required, non-empty after trimming).
    pub name: String,
    /// Scopes to grant (required, at least one).
    pub scopes: Vec<Scope>,
    /// Seconds from mint time until expiry; `None` means no expiry.
    pub ttl_secs: Option<i64>,
}

/// The result of [`mint`]: the persisted metadata plus the bearer secret.
///
/// The secret is not reconstructible once this value is dropped — only its
/// argon2id hash lives on in the database.
#[derive(Debug, Clone)]
pub struct MintedToken {
    /// The persisted token's metadata.
    pub token: ApiToken,
    /// The bearer secret. Show it to the operator now; it cannot be
    /// recovered later.
    pub secret: String,
}

/// Mint a new capability token.
///
/// # Errors
///
/// [`Error::InvalidArgument`] for an empty name, an empty scope list, or a
/// non-positive `ttl_secs`; [`Error::Internal`] if hashing the secret fails
/// (this should not happen with default argon2 parameters); otherwise a
/// mapped storage error.
pub async fn mint(db: &Database, new: NewToken) -> Result<MintedToken> {
    let name = new.name.trim().to_owned();
    if name.is_empty() {
        return Err(Error::invalid_argument("token name must not be empty"));
    }
    if new.scopes.is_empty() {
        return Err(Error::invalid_argument(
            "token must be granted at least one scope",
        ));
    }
    let expires_at = match new.ttl_secs {
        None => None,
        Some(ttl) if ttl <= 0 => {
            return Err(Error::invalid_argument("ttl_secs must be positive"));
        }
        Some(ttl) => Some(now_unix().checked_add(ttl).ok_or_else(|| {
            Error::invalid_argument("ttl_secs is too large; expiry would overflow")
        })?),
    };

    let raw_secret = generate_secret();
    let hash = hash_secret_blocking(raw_secret.clone()).await?;
    let scopes_wire = Scope::join(&new.scopes);

    let row = repo::NewApiToken {
        name: name.clone(),
        token_hash: hash.into_bytes(),
        scopes: scopes_wire,
        expires_at,
    };
    let id = db.write(move |c| repo::insert_token(c, &row)).await?;

    let token = get(db, id).await?;
    let secret = format!("{TOKEN_PREFIX}{id}_{raw_secret}");
    Ok(MintedToken { token, secret })
}

/// List all tokens, most-recently-minted first.
///
/// # Errors
///
/// A mapped storage error, or [`Error::Internal`] if a stored row's scopes
/// fail to parse (see [`ApiToken::from_repo`]).
pub async fn list(db: &Database) -> Result<Vec<ApiToken>> {
    let rows = db.read(repo::list_tokens).await?;
    rows.into_iter().map(ApiToken::from_repo).collect()
}

/// Fetch a token's metadata by id.
///
/// # Errors
///
/// [`Error::NotFound`] if no such token; otherwise a mapped storage error.
async fn get(db: &Database, id: i64) -> Result<ApiToken> {
    let row = db
        .read(move |c| repo::get_token(c, id))
        .await?
        .ok_or_else(|| Error::not_found(format!("token {id}")))?;
    ApiToken::from_repo(row)
}

/// Revoke a token by id. Idempotent: revoking an already-revoked (or
/// expired) token still succeeds — only an id that never existed errors.
///
/// # Errors
///
/// [`Error::NotFound`] if no token with that id exists; otherwise a mapped
/// storage error.
pub async fn revoke(db: &Database, id: i64) -> Result<()> {
    let existed = db.write(move |c| repo::revoke_token(c, id)).await?;
    if existed {
        Ok(())
    } else {
        Err(Error::not_found(format!("token {id}")))
    }
}

/// Verify a presented bearer token, returning its granted scopes.
///
/// Every failure path — malformed string, unknown id, wrong secret, revoked,
/// expired — returns the same [`Error::Unauthenticated`] with a generic
/// message, and every path that has a candidate row (found, regardless of
/// its revoked/expired state) pays the same argon2 cost as a genuine
/// verification. Both are deliberate: distinguishing "no such token" from
/// "wrong secret" from "revoked" via the error message or via response
/// timing would hand a caller probing for valid ids a usable oracle.
///
/// # Errors
///
/// [`Error::Unauthenticated`] on any verification failure; otherwise a
/// mapped storage error (a read that fails for reasons other than "no such
/// row" — pool exhaustion, corruption — surfaces as itself, since the
/// timing-uniformity concern above only applies to distinguishing *valid
/// inputs the caller could have chosen* from one another).
pub async fn verify(db: &Database, presented: &str) -> Result<ApiToken> {
    const GENERIC: &str = "invalid or expired token";

    let (id, secret) = parse_presented(presented).ok_or_else(|| Error::unauthenticated(GENERIC))?;

    let row = db.read(move |c| repo::get_token(c, id)).await?;

    let hash_str = row
        .as_ref()
        .and_then(|r| std::str::from_utf8(&r.token_hash).ok().map(str::to_owned));
    let matched = match hash_str {
        Some(hash) => verify_secret_blocking(secret, hash).await,
        // No such row (or a corrupt hash): still pay the argon2 cost against a
        // fixed dummy hash so the response takes the same time either way.
        None => {
            verify_secret_blocking(secret, DUMMY_HASH.to_owned()).await;
            false
        }
    };

    let Some(row) = row else {
        return Err(Error::unauthenticated(GENERIC));
    };
    if !matched || row.revoked {
        return Err(Error::unauthenticated(GENERIC));
    }
    if let Some(expires_at) = row.expires_at {
        if expires_at <= now_unix() {
            return Err(Error::unauthenticated(GENERIC));
        }
    }

    // Best-effort bookkeeping, and throttled: a failure here must not fail an
    // otherwise-valid auth decision (the caller has already proven possession
    // of the secret), and every write here takes the process's single-writer
    // lock — see [`LAST_USED_THROTTLE_SECS`].
    let now = now_unix();
    let stale = match row.last_used_at {
        Some(last) => now.saturating_sub(last) >= LAST_USED_THROTTLE_SECS,
        None => true,
    };
    if stale {
        if let Err(error) = db.write(move |c| repo::touch_last_used(c, id, now)).await {
            tracing::warn!(token_id = id, %error, "failed to record token last_used_at");
        }
    }

    ApiToken::from_repo(row)
}

/// Split a presented bearer string into `(id, secret)`. Returns `None` for
/// anything that does not have the [`TOKEN_PREFIX`]-`<id>`-`_`-`<secret>`
/// shape, without distinguishing *how* it was malformed — see [`verify`].
fn parse_presented(s: &str) -> Option<(i64, String)> {
    let rest = s.strip_prefix(TOKEN_PREFIX)?;
    let (id_str, secret) = rest.split_once('_')?;
    let id: i64 = id_str.parse().ok()?;
    if secret.is_empty() {
        return None;
    }
    Some((id, secret.to_owned()))
}

/// Generate a fresh random secret: 32 bytes from the OS RNG, hex-encoded.
fn generate_secret() -> String {
    let mut bytes = [0u8; SECRET_BYTES];
    OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Hash a secret with argon2id and a fresh random salt, returning the PHC
/// string (self-describing: algorithm + params + salt + digest).
///
/// Synchronous and CPU-bound by design (argon2's whole point is to cost real
/// time and memory) — callers on the async path must run this via
/// [`hash_secret_blocking`], never inline in an `async fn`.
fn hash_secret(secret: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(secret.as_bytes(), &salt)
        .map(|hash| hash.to_string())
        .map_err(|e| Error::internal(format!("argon2 hash failed: {e}")))
}

/// [`hash_secret`], run on the blocking-task pool.
///
/// `Database::read`/`write` already do this for SQLite access; argon2's
/// default params (~19 MiB, 2 iterations — deliberately expensive) are
/// exactly the same class of work, and running them inline on a Tokio worker
/// would stall every other task scheduled on that worker for the hash's
/// whole duration on every mint.
///
/// # Errors
///
/// As [`hash_secret`], or [`Error::Internal`] if the blocking task panics or
/// is cancelled.
async fn hash_secret_blocking(secret: String) -> Result<String> {
    match tokio::task::spawn_blocking(move || hash_secret(&secret)).await {
        Ok(result) => result,
        Err(join_error) => Err(Error::internal(format!(
            "hashing task failed: {join_error}"
        ))),
    }
}

/// Verify `secret` against a stored PHC hash string. `false` for any
/// mismatch *or* an unparseable `phc` — the latter should not happen for a
/// hash this module wrote, but a corrupt row must fail closed, not panic.
fn verify_secret(secret: &str, phc: &str) -> bool {
    match PasswordHash::new(phc) {
        Ok(parsed) => Argon2::default()
            .verify_password(secret.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

/// [`verify_secret`], run on the blocking-task pool — see
/// [`hash_secret_blocking`] for why. A panicked/cancelled blocking task fails
/// closed (`false`), not open: a caller must never be granted access because
/// the verification itself broke.
async fn verify_secret_blocking(secret: String, phc: String) -> bool {
    tokio::task::spawn_blocking(move || verify_secret(&secret, &phc))
        .await
        .unwrap_or(false)
}

/// A fixed, known-valid argon2id PHC hash verified against on every "no such
/// token id" path, so that path costs the same as a real verification.
///
/// Hardcoded rather than computed at first use: a lazily-computed dummy that
/// failed to hash (see [`hash_secret`]'s error path) would fall back to an
/// empty string, whose `PasswordHash::new` fails *instantly* — reopening
/// exactly the timing oracle this constant exists to close, silently. This
/// literal was generated once via `Argon2::default()` over the fixed input
/// `"rmail-dummy-verification-secret-never-stored"`; it protects nothing and
/// never needs to change — its only job is to burn the same CPU a real
/// verification would.
const DUMMY_HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$DFaru382lYMgIot9qGJ15w$\
     fXiUth5w7z7RpssPoc+oUhJYNp86JW57d+iQJpB20hc";

/// Current time as unix seconds, saturating rather than panicking on a clock
/// before the epoch or a value past `i64::MAX`.
fn now_unix() -> i64 {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    i64::try_from(secs).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests;
