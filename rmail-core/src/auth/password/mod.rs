//! The client-facing password gate: set, clear, check, and verify against
//! `client_password` (V56).
//!
//! # How this relates to [`super`]'s tokens
//!
//! A capability token ([`super::mint`]) is a credential an operator hands to
//! one caller — a script, an MCP agent, another machine. This module guards
//! something upstream of that: whether a caller of `rmail`'s own API is the
//! owner at all. `rmaild`'s `ClientAuthService::LoginPassword` calls
//! [`verify_password`] and, on [`LoginOutcome::Success`], mints an ordinary
//! token via [`super::mint`] — the same function `AdminService::MintToken`
//! calls. Nothing about verification, scope enforcement, or revocation ever
//! learns which door a token came through.
//!
//! # Why this module burns Argon2 cost on paths that are not "check the
//! password"
//!
//! [`verify_password`] pays the same [`super::verify_secret_blocking`] cost
//! whether there is no password configured, the caller is locked out, or the
//! password is simply wrong. A version that returned early on the first two
//! would let a caller distinguish "nothing is set up yet" and "you are
//! locked out" from "that was a genuine, timed guess" by response latency
//! alone — a small leak, but the entire value of constant-time comparison is
//! that a system pays it everywhere or the exception becomes the oracle.

mod repo;

use super::{hash_secret_blocking, now_unix, verify_secret_blocking, DUMMY_HASH};
use crate::error::{Error, Result};
use crate::storage::Database;

/// Whether a password has been configured at all.
///
/// # Errors
///
/// A mapped storage error.
pub async fn is_configured(db: &Database) -> Result<bool> {
    Ok(db.read(repo::get).await?.is_some())
}

/// Verify `presented` against the currently configured password, touching no
/// lockout state at all.
///
/// For `ClientAuthService::SetupPassword`'s re-authentication step: proving
/// you know the *current* password before being allowed to replace it is a
/// different action from logging in, deliberately not routed through
/// [`verify_password`] and its lockout counter. `SetupPassword` requires
/// `admin` already (an existing session token, or the trusted local peer) —
/// the only callers who can reach this at all — and sharing the login
/// lockout with them would mean the caller who already holds a valid admin
/// session could lock *ordinary login* out for everyone else by mistyping
/// the current password a few times while trying to change it.
///
/// `false`, not an error, when no password is configured at all — there is
/// nothing to confirm against, which the caller (`SetupPassword`'s handler)
/// is expected to have already checked via [`is_configured`] before deciding
/// whether to require this at all; reaching here regardless still fails
/// closed rather than treating an absent row as a match.
///
/// # Errors
///
/// A mapped storage error.
pub async fn verify_current(db: &Database, presented: &str) -> Result<bool> {
    let Some(row) = db.read(repo::get).await? else {
        // Same reasoning as `verify_password`'s own `NotConfigured` branch:
        // pay the cost regardless, so "nothing to confirm" is not a faster
        // response than a genuine mismatch.
        verify_secret_blocking(presented.to_owned(), DUMMY_HASH.to_owned()).await;
        return Ok(false);
    };
    Ok(verify_secret_blocking(presented.to_owned(), row.password_hash).await)
}

/// Set (or replace) the password.
///
/// Clears any existing lockout: a lockout was earned by attempts against the
/// *old* password, and a caller who just proved they can change the password
/// at all (see `ClientAuthService::SetupPassword`'s `admin` requirement) has
/// already cleared a higher bar than the one the lockout was protecting.
///
/// Returns the `updated_at` unix timestamp actually written, so a caller
/// (e.g. `ClientAuthService::SetupPassword`) reports the moment this
/// function chose rather than reading the clock a second time and risking a
/// value that does not quite match what is in the database.
///
/// # Errors
///
/// [`Error::InvalidArgument`] for an empty password; [`Error::Internal`] if
/// hashing fails; otherwise a mapped storage error.
pub async fn set_password(db: &Database, new_password: &str) -> Result<i64> {
    if new_password.is_empty() {
        return Err(Error::invalid_argument("password must not be empty"));
    }
    let hash = hash_secret_blocking(new_password.to_owned()).await?;
    let now = now_unix();
    db.write(move |c| repo::upsert(c, &hash, now)).await?;
    Ok(now)
}

/// Remove the password gate entirely.
///
/// # Errors
///
/// A mapped storage error. Idempotent — clearing an already-absent password
/// is not an error, matching [`super::revoke`]'s idempotent-revoke contract.
pub async fn clear_password(db: &Database) -> Result<()> {
    db.write(|c| repo::delete(c)).await?;
    Ok(())
}

/// The result of a [`verify_password`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginOutcome {
    /// The password matched. The caller should mint a session token.
    Success,
    /// The password did not match.
    WrongPassword {
        /// Attempts remaining before lockout. `0` means this attempt itself
        /// was the last one before [`LoginOutcome::LockedOut`] would start
        /// being returned instead.
        remaining: u32,
    },
    /// Too many recent failures; the gate is closed for now.
    LockedOut {
        /// Seconds until [`verify_password`] will attempt a real check again.
        retry_after_secs: i64,
    },
    /// No password has ever been configured.
    NotConfigured,
}

/// Serializes every [`verify_password`] call, crate-wide, to exactly one at
/// a time.
///
/// Two things this closes, together:
///
/// 1. **The lockout race.** Without it, concurrent `LoginPassword` calls all
///    read `locked_until` (or the pre-failure `failed_attempts`) before any
///    of them commits its own outcome — [`repo::record_failure`] is one
///    atomic write, so the *count* is never wrong, but the *check* against it
///    (this function's `if locked_until > now` guard, run once per call
///    before that write) is a read from before every one of the concurrent
///    writes landed. An attacker who opens N connections at once gets N
///    guesses per `client_auth.lockout` window, not `client_auth.max_attempts`
///    — the lockout would bound sequential guessing only.
/// 2. **Resource exhaustion.** Each in-flight verification holds a
///    `spawn_blocking` thread for an entire Argon2id hash (~19 MiB per the
///    parameters [`DUMMY_HASH`] was generated with). `LoginPassword` needs no
///    credential to call at all, by design (see
///    `rmaild::auth::Requirement::SelfAuthenticated`) — and this function
///    pays the same cost even when nothing is configured (the
///    [`LoginOutcome::NotConfigured`] branch below). With no serialization,
///    an unauthenticated flood of concurrent calls is a way to pin every
///    thread in tokio's blocking pool and several GB of RSS on a daemon that
///    never enabled this feature at all — and `Database::read`/`write` share
///    that same pool, so every other database access in the process stalls
///    behind the flood too.
///
/// A single permit rather than a small pool: nothing about login is
/// throughput-sensitive (a human types one password at a time), and holding
/// the gate across the whole read-check-hash-write span is what makes the
/// lockout check atomic with respect to *other calls to this function*, not
/// just each call's own write atomic with respect to itself.
static VERIFY_GATE: std::sync::LazyLock<tokio::sync::Semaphore> =
    std::sync::LazyLock::new(|| tokio::sync::Semaphore::new(1));

/// Verify a presented password, applying lockout.
///
/// `max_attempts`/`lockout_secs` come from `config.client_auth` — passed in
/// rather than read from a global, so this module has no config dependency
/// of its own and a test can exercise lockout with a tiny threshold.
///
/// # Errors
///
/// A mapped storage error. A wrong password, a lockout, or no password
/// configured are none of them errors — they are [`LoginOutcome`] variants,
/// because `ClientAuthService::LoginPassword` needs to answer each with a
/// different `tonic::Status` (`UNAUTHENTICATED` vs `RESOURCE_EXHAUSTED`) and
/// a `Result::Err` here would erase which one happened.
pub async fn verify_password(
    db: &Database,
    presented: &str,
    max_attempts: u32,
    lockout_secs: i64,
) -> Result<LoginOutcome> {
    // Held for the whole function — see `VERIFY_GATE`'s own docs for why the
    // read below has to be inside the gate too, not just the write at the
    // end. `acquire` only errs if the semaphore was `close`d, which nothing
    // in this codebase ever does; mapped rather than `expect`ed regardless,
    // since "cannot happen" is not the same guarantee as "cannot compile a
    // panic".
    let _permit = VERIFY_GATE
        .acquire()
        .await
        .map_err(|e| Error::internal(format!("client-auth verify gate closed: {e}")))?;

    let Some(row) = db.read(repo::get).await? else {
        verify_secret_blocking(presented.to_owned(), DUMMY_HASH.to_owned()).await;
        return Ok(LoginOutcome::NotConfigured);
    };

    let now = now_unix();
    if let Some(locked_until) = row.locked_until {
        if locked_until > now {
            // Same cost as a real check — see the module docs.
            verify_secret_blocking(presented.to_owned(), row.password_hash).await;
            return Ok(LoginOutcome::LockedOut {
                retry_after_secs: locked_until - now,
            });
        }
    }

    let matched = verify_secret_blocking(presented.to_owned(), row.password_hash).await;
    if matched {
        db.write(|c| repo::record_success(c)).await?;
        return Ok(LoginOutcome::Success);
    }

    let locked_until_if_tripped = now.saturating_add(lockout_secs);
    let outcome = db
        .write(move |c| repo::record_failure(c, max_attempts, now, locked_until_if_tripped))
        .await?;
    // `None`: the row was deleted (`ClearPassword`) between the read above
    // and this write — see `repo::record_failure`'s own docs on this race.
    let Some((failed_attempts, locked_until)) = outcome else {
        return Ok(LoginOutcome::NotConfigured);
    };
    Ok(match locked_until {
        Some(until) => LoginOutcome::LockedOut {
            retry_after_secs: until - now,
        },
        None => LoginOutcome::WrongPassword {
            remaining: max_attempts.saturating_sub(failed_attempts),
        },
    })
}

#[cfg(test)]
mod tests;
