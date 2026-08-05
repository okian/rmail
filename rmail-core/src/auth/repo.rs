//! SQL access to `api_tokens` (V14).
//!
//! Private to [`crate::auth`] — unlike the shared [`crate::repo`] module,
//! nothing outside the auth domain needs a token row, so this stays
//! self-contained rather than adding to a file every task touches.

use rusqlite::{named_params, Connection, OptionalExtension, Row};

/// Fields required to create a token row (id/`created_at` are DB-assigned).
pub(super) struct NewApiToken {
    pub name: String,
    /// The argon2id PHC string, as bytes.
    pub token_hash: Vec<u8>,
    /// Comma-joined [`super::Scope::as_wire`] strings.
    pub scopes: String,
    pub expires_at: Option<i64>,
}

/// A persisted token row. Carries `token_hash` — callers outside this module
/// must never let it escape past [`super::verify`]'s constant-time compare.
pub(super) struct ApiTokenRow {
    pub id: i64,
    pub name: String,
    pub token_hash: Vec<u8>,
    pub scopes: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub expires_at: Option<i64>,
    pub revoked: bool,
}

const COLS: &str = "id, name, token_hash, scopes, created_at, last_used_at, expires_at, revoked";

fn from_row(row: &Row<'_>) -> rusqlite::Result<ApiTokenRow> {
    Ok(ApiTokenRow {
        id: row.get("id")?,
        name: row.get("name")?,
        token_hash: row.get("token_hash")?,
        scopes: row.get("scopes")?,
        created_at: row.get("created_at")?,
        last_used_at: row.get("last_used_at")?,
        expires_at: row.get("expires_at")?,
        revoked: row.get("revoked")?,
    })
}

/// Insert a token row, returning its new id.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub(super) fn insert_token(conn: &Connection, new: &NewApiToken) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO api_tokens (name, token_hash, scopes, expires_at)
         VALUES (:name, :token_hash, :scopes, :expires_at)",
        named_params! {
            ":name": new.name,
            ":token_hash": new.token_hash,
            ":scopes": new.scopes,
            ":expires_at": new.expires_at,
        },
    )?;
    Ok(conn.last_insert_rowid())
}

/// Fetch a token by id.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub(super) fn get_token(conn: &Connection, id: i64) -> rusqlite::Result<Option<ApiTokenRow>> {
    conn.query_row(
        &format!("SELECT {COLS} FROM api_tokens WHERE id = ?1"),
        [id],
        from_row,
    )
    .optional()
}

/// List all tokens, newest first.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub(super) fn list_tokens(conn: &Connection) -> rusqlite::Result<Vec<ApiTokenRow>> {
    let mut stmt = conn.prepare(&format!("SELECT {COLS} FROM api_tokens ORDER BY id DESC"))?;
    let rows = stmt.query_map([], from_row)?;
    rows.collect()
}

/// Mark a token revoked. Returns whether a row with that id existed
/// (idempotent: revoking an already-revoked token still returns `true`).
///
/// # Errors
/// Propagates any `rusqlite` error.
pub(super) fn revoke_token(conn: &Connection, id: i64) -> rusqlite::Result<bool> {
    let affected = conn.execute("UPDATE api_tokens SET revoked = 1 WHERE id = ?1", [id])?;
    Ok(affected > 0)
}

/// Record a successful verification. Best-effort from the caller's
/// perspective — see [`super::verify`] — but propagates its own errors so the
/// caller can choose to log them.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub(super) fn touch_last_used(conn: &Connection, id: i64, at: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE api_tokens SET last_used_at = ?2 WHERE id = ?1",
        rusqlite::params![id, at],
    )?;
    Ok(())
}
