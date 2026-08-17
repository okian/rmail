//! SQL access to `client_password` (V56).
//!
//! Private to [`crate::auth::password`], mirroring `auth::repo`'s own
//! "nothing outside the auth domain needs this row" scoping.

use rusqlite::{Connection, OptionalExtension};

/// The one row this table ever holds.
pub(super) struct PasswordRow {
    /// Argon2id PHC string.
    pub password_hash: String,
    pub locked_until: Option<i64>,
}

/// Fetch the row, if a password has ever been set.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub(super) fn get(conn: &Connection) -> rusqlite::Result<Option<PasswordRow>> {
    conn.query_row(
        "SELECT password_hash, locked_until FROM client_password WHERE id = 1",
        [],
        |row| {
            Ok(PasswordRow {
                password_hash: row.get("password_hash")?,
                locked_until: row.get("locked_until")?,
            })
        },
    )
    .optional()
}

/// Create the row, or replace an existing one: hash, `updated_at`, and the
/// lockout state (cleared) all move together — see [`super::set_password`]
/// for why a changed password clears a lockout earned by the old one.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub(super) fn upsert(conn: &Connection, hash: &str, now: i64) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO client_password (id, password_hash, created_at, updated_at, failed_attempts, locked_until)
         VALUES (1, ?1, ?2, ?2, 0, NULL)
         ON CONFLICT (id) DO UPDATE SET
             password_hash = excluded.password_hash,
             updated_at = excluded.updated_at,
             failed_attempts = 0,
             locked_until = NULL",
        rusqlite::params![hash, now],
    )?;
    Ok(())
}

/// Remove the row. Idempotent: deleting an absent row is not an error.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub(super) fn delete(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM client_password WHERE id = 1", [])?;
    Ok(())
}

/// Reset the failure/lockout state after a successful login.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub(super) fn record_success(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE client_password SET failed_attempts = 0, locked_until = NULL WHERE id = 1",
        [],
    )?;
    Ok(())
}

/// Record a failed login attempt: reset `failed_attempts` to 1 if the row's
/// own past lockout has already expired, otherwise increment it, and — if
/// that reaches `max_attempts` — set `locked_until` to
/// `locked_until_if_tripped`.
///
/// # Why reset rather than only ever increment
///
/// Without the reset, a `failed_attempts` left at (or above) `max_attempts`
/// by a lockout that has since passed never comes back down on its own —
/// `failed_attempts + 1` only grows — so the very next wrong guess re-trips
/// a fresh lockout immediately, no matter how long ago the first one expired.
/// The caller-visible effect would be "one grace attempt after every
/// lockout, forever", which is not what `client_auth.max_attempts` promises.
///
/// # Race freedom
///
/// Two statements, but still race-free: this function only ever runs inside
/// [`crate::storage::Database::write`]'s single-writer lock, so nothing else
/// can touch this row between them — see [`super::VERIFY_GATE`]'s own docs
/// for the *other* race this alone would not close (concurrent callers all
/// passing the lockout check before any of them writes), which is why
/// [`super::verify_password`] additionally holds a permit around this call
/// and the read/hash that precedes it.
///
/// # TOCTOU with `ClearPassword`
///
/// Returns `None`, not an error, if the row is gone by the time this runs —
/// `verify_password` read it, but a concurrent `SetupPassword`/`ClearPassword`
/// deleted or replaced it before this write landed. The caller reports that
/// the same way a `get` that found nothing would ([`LoginOutcome::NotConfigured`]),
/// rather than as an internal storage error — the row being gone is exactly
/// as truthful an answer as "no password is configured" is, and a real
/// concurrent-admin-action is not a storage failure.
///
/// # Errors
/// Propagates any `rusqlite` error other than the row having disappeared.
pub(super) fn record_failure(
    conn: &Connection,
    max_attempts: u32,
    now: i64,
    locked_until_if_tripped: i64,
) -> rusqlite::Result<Option<(u32, Option<i64>)>> {
    let failed_attempts: Option<u32> = conn
        .query_row(
            "UPDATE client_password
             SET failed_attempts = CASE
                     WHEN locked_until IS NOT NULL AND locked_until <= ?1 THEN 1
                     ELSE failed_attempts + 1
                 END
             WHERE id = 1
             RETURNING failed_attempts",
            rusqlite::params![now],
            |row| row.get(0),
        )
        .optional()?;
    let Some(failed_attempts) = failed_attempts else {
        return Ok(None);
    };

    let locked_until: Option<i64> = conn.query_row(
        "UPDATE client_password
         SET locked_until = CASE WHEN failed_attempts >= ?1 THEN ?2 ELSE locked_until END
         WHERE id = 1
         RETURNING locked_until",
        rusqlite::params![max_attempts, locked_until_if_tripped],
        |row| row.get(0),
    )?;

    Ok(Some((failed_attempts, locked_until)))
}
