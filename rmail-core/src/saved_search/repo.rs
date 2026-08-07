//! Typed SQL over `saved_searches` (migration V26).
//!
//! Nothing here interprets a query string — that is
//! [`super::validate_query`]'s job, deliberately kept out of the repository
//! layer so the *same* validation runs whether a row arrives through
//! [`super::SavedSearchStore`] or (later) through a bulk import.

use rusqlite::{Connection, OptionalExtension, Row};

use super::SavedSearch;

/// Every column, in the order [`from_row`] reads them.
const COLUMNS: &str = "id, account_id, name, query, created_at, updated_at, last_run_at";

fn from_row(row: &Row<'_>) -> rusqlite::Result<SavedSearch> {
    Ok(SavedSearch {
        id: row.get("id")?,
        account_id: row.get("account_id")?,
        name: row.get("name")?,
        query: row.get("query")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        last_run_at: row.get("last_run_at")?,
    })
}

/// Insert a saved search, returning the stored row.
///
/// A duplicate `(account_id, name)` surfaces as a `SQLITE_CONSTRAINT_UNIQUE`
/// failure and an `account_id` naming no account as a
/// `SQLITE_CONSTRAINT_FOREIGNKEY` one — both classified by
/// [`is_unique_violation`]/[`is_missing_reference`] rather than pre-checked
/// with a `SELECT`, which would be a TOCTOU race against a concurrent
/// writer and would still have to handle the failure anyway.
pub(crate) fn insert(
    conn: &Connection,
    account_id: i64,
    name: &str,
    query: &str,
) -> rusqlite::Result<SavedSearch> {
    conn.query_row(
        &format!(
            "INSERT INTO saved_searches (account_id, name, query) VALUES (?1, ?2, ?3)
             RETURNING {COLUMNS}"
        ),
        rusqlite::params![account_id, name, query],
        from_row,
    )
}

/// Replace an existing saved search's query text, returning the updated row
/// or `None` if no such name exists in the account.
pub(crate) fn update_query(
    conn: &Connection,
    account_id: i64,
    name: &str,
    query: &str,
) -> rusqlite::Result<Option<SavedSearch>> {
    conn.query_row(
        &format!(
            "UPDATE saved_searches SET query = ?3, updated_at = unixepoch()
             WHERE account_id = ?1 AND name = ?2 RETURNING {COLUMNS}"
        ),
        rusqlite::params![account_id, name, query],
        from_row,
    )
    .optional()
}

/// One account's saved searches, alphabetical by name.
pub(crate) fn list(conn: &Connection, account_id: i64) -> rusqlite::Result<Vec<SavedSearch>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM saved_searches WHERE account_id = ?1 ORDER BY name"
    ))?;
    let rows = stmt
        .query_map([account_id], from_row)?
        .collect::<rusqlite::Result<Vec<SavedSearch>>>()?;
    Ok(rows)
}

/// Look one up by name within an account (case-insensitive, per the column's
/// `COLLATE NOCASE`).
pub(crate) fn get_by_name(
    conn: &Connection,
    account_id: i64,
    name: &str,
) -> rusqlite::Result<Option<SavedSearch>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM saved_searches WHERE account_id = ?1 AND name = ?2"),
        rusqlite::params![account_id, name],
        from_row,
    )
    .optional()
}

/// Delete by name; `true` if a row was removed.
pub(crate) fn delete(conn: &Connection, account_id: i64, name: &str) -> rusqlite::Result<bool> {
    let removed = conn.execute(
        "DELETE FROM saved_searches WHERE account_id = ?1 AND name = ?2",
        rusqlite::params![account_id, name],
    )?;
    Ok(removed > 0)
}

/// Stamp `last_run_at` and return the row as it now stands, or `None` if the
/// name does not exist.
///
/// Returning the row from the same statement that stamps it is what makes
/// "resolve the query to re-run" and "record that it was run" one atomic
/// step: a `SELECT` followed by an `UPDATE` could hand back a query string
/// that a concurrent `update_query` had already replaced.
pub(crate) fn touch_run(
    conn: &Connection,
    account_id: i64,
    name: &str,
) -> rusqlite::Result<Option<SavedSearch>> {
    conn.query_row(
        &format!(
            "UPDATE saved_searches SET last_run_at = unixepoch()
             WHERE account_id = ?1 AND name = ?2 RETURNING {COLUMNS}"
        ),
        rusqlite::params![account_id, name],
        from_row,
    )
    .optional()
}

/// Whether `err` is a `UNIQUE` constraint failure.
///
/// Matched on the extended result code, never on the message text: the
/// wording is not a contract and differs across SQLite builds, while
/// `SQLITE_CONSTRAINT_UNIQUE` is. (`crate::notes::is_missing_target` makes
/// the identical argument for the foreign-key code.)
pub(crate) fn is_unique_violation(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(inner, _)
            if inner.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
                || inner.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY
    )
}

/// Whether `err` is a foreign-key violation — for this task's two tables,
/// always "the `account_id` names no account".
pub(crate) fn is_missing_reference(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(inner, _)
            if inner.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_FOREIGNKEY
    )
}
