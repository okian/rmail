//! Per-account backend overrides: the `ai_provider_overrides` table.
//!
//! # Three tiers, and the narrow thing this table is allowed to do
//!
//! A call's backend resolves as: the account's own row, else the daemon-wide
//! row (`account_id = 0`), else `ai.provider` from the config file. That is
//! the whole resolution — [`effective_provider`] is the only function that
//! walks it.
//!
//! What this table cannot do is widen anything. It is read *after*
//! `ai.policy`, and [`super::resolve_egress`] applies the policy first: a
//! `local_only` folder stays on-device with a `claude` row sitting right
//! there, and a `forbidden` one is refused outright. An override picks between
//! backends that are already permitted; it never grants permission. Keeping
//! that ordering in one function, rather than at each call site, is what makes
//! it a property rather than a habit.
//!
//! # Why an override lives in the database and not the config file
//!
//! `ai.provider` is a restart to change and applies to every account at once.
//! "Move this one account on-device, now" is an operational action taken in
//! response to something — a contract, an audit, a mailbox someone just
//! realized is sensitive — and it must take effect on the next job, without a
//! restart, the same way a budget does. The dispatch path reads the table on
//! each call, so it does.

use rusqlite::OptionalExtension;

use crate::config::AiProvider;
use crate::error::Error;
use crate::storage::Database;

/// The `account_id` that stands for "every account with no row of its own".
///
/// Deliberately the same sentinel [`crate::ai::budget::GLOBAL_ACCOUNT_ID`]
/// uses, for the same reason and with the same guarantee that it can never
/// collide with a real `accounts.id` — see `V52__ai_provider_override.sql`.
pub const GLOBAL_ACCOUNT_ID: i64 = crate::ai::budget::GLOBAL_ACCOUNT_ID;

/// Store (or clear, with `provider = None`) one scope's backend override.
///
/// # Errors
///
/// [`Error::InvalidArgument`] for a negative `account_id`; a mapped storage
/// error otherwise.
#[tracing::instrument(skip(db))]
pub async fn set_override(
    db: &Database,
    account_id: i64,
    provider: Option<AiProvider>,
) -> Result<(), Error> {
    if account_id < 0 {
        return Err(Error::invalid_argument(format!(
            "provider override account_id must be {GLOBAL_ACCOUNT_ID} (daemon-wide) or a \
             real account id, got {account_id}"
        )));
    }
    let now = chrono::Utc::now().timestamp();
    match provider {
        Some(provider) => {
            let wire = provider.as_str().to_owned();
            db.write(move |conn| {
                conn.execute(
                    "INSERT INTO ai_provider_overrides (account_id, provider, updated_at)
                     VALUES (?1, ?2, ?3)
                     ON CONFLICT(account_id) DO UPDATE SET
                         provider = excluded.provider,
                         updated_at = excluded.updated_at",
                    rusqlite::params![account_id, wire, now],
                )?;
                Ok(())
            })
            .await
            .map_err(Error::from)?;
            tracing::info!(
                account_id,
                provider = provider.as_str(),
                "ai provider override stored"
            );
        }
        None => {
            db.write(move |conn| {
                conn.execute(
                    "DELETE FROM ai_provider_overrides WHERE account_id = ?1",
                    rusqlite::params![account_id],
                )?;
                Ok(())
            })
            .await
            .map_err(Error::from)?;
            tracing::info!(account_id, "ai provider override cleared");
        }
    }
    Ok(())
}

/// The override stored for exactly this scope, if any — no inheritance.
///
/// Separate from [`resolve_override`] because the operator-facing status
/// surface has to be able to say "this account has its own override" as
/// distinct from "this account inherits the daemon-wide one". Rendering the
/// inherited value as though it were the account's own is how an operator
/// clears one account and is surprised that nothing changed.
///
/// # Errors
///
/// A mapped storage error.
pub async fn stored_override(db: &Database, account_id: i64) -> Result<Option<AiProvider>, Error> {
    let stored: Option<String> = db
        .read(move |conn| {
            conn.query_row(
                "SELECT provider FROM ai_provider_overrides WHERE account_id = ?1",
                rusqlite::params![account_id],
                |row| row.get(0),
            )
            .optional()
        })
        .await
        .map_err(Error::from)?;
    Ok(stored.as_deref().and_then(decode))
}

/// The override in force for `account_id`: its own row, else the daemon-wide
/// row.
///
/// # Errors
///
/// A mapped storage error.
pub async fn resolve_override(db: &Database, account_id: i64) -> Result<Option<AiProvider>, Error> {
    if let Some(own) = stored_override(db, account_id).await? {
        return Ok(Some(own));
    }
    if account_id == GLOBAL_ACCOUNT_ID {
        return Ok(None);
    }
    stored_override(db, GLOBAL_ACCOUNT_ID).await
}

/// The backend `account_id`'s calls use, ignoring policy: the resolved
/// override, else `default` (the config file's `ai.provider`).
///
/// This is *not* the final answer for a given message — policy can force a
/// call on-device regardless. [`super::resolve_egress`] combines the two, and
/// is what a dispatch path calls.
///
/// # Errors
///
/// A mapped storage error.
pub async fn effective_provider(
    db: &Database,
    account_id: i64,
    default: AiProvider,
) -> Result<AiProvider, Error> {
    Ok(resolve_override(db, account_id).await?.unwrap_or(default))
}

/// Map a stored spelling onto a backend.
///
/// An unrecognized value is dropped to `None` — "no override", the safe
/// reading — rather than guessed at, and logged loudly because the column's
/// `CHECK` constraint means it should be unreachable: seeing one means
/// something wrote to this table without going through [`set_override`].
fn decode(stored: &str) -> Option<AiProvider> {
    let parsed = AiProvider::parse(stored);
    if parsed.is_none() {
        tracing::error!(
            provider = stored,
            "ai_provider_overrides holds a backend this build does not know; \
             ignoring the override"
        );
    }
    parsed
}
