//! Typed SQL over `query_plan_cache` (migration V47).

use rusqlite::{Connection, OptionalExtension};

/// One cached compile, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CachedPlan {
    pub(super) raw: String,
    pub(super) compiled: String,
    pub(super) intent: String,
    pub(super) notes: String,
    pub(super) model: String,
    pub(super) created_at: i64,
}

/// Read the cached plan for `(account_id, hash)` and stamp the read.
///
/// A write, not a read, and that is the point: `uses`/`last_used_at` are the
/// only evidence this table is earning its keep, and a cache hit that did not
/// record itself would leave the question unanswerable. The stamp and the read
/// are one statement so a concurrent reader cannot see the row without its
/// count.
pub(super) fn touch(
    conn: &Connection,
    account_id: i64,
    hash: &str,
) -> rusqlite::Result<Option<CachedPlan>> {
    conn.query_row(
        "UPDATE query_plan_cache
            SET uses = uses + 1, last_used_at = unixepoch()
          WHERE account_id = ?1 AND query_hash = ?2
      RETURNING raw, compiled, intent, notes, model, created_at",
        rusqlite::params![account_id, hash],
        |row| {
            Ok(CachedPlan {
                raw: row.get(0)?,
                compiled: row.get(1)?,
                intent: row.get(2)?,
                notes: row.get(3)?,
                model: row.get(4)?,
                created_at: row.get(5)?,
            })
        },
    )
    .optional()
}

/// Insert or replace the cached plan for `(account_id, hash)`.
///
/// `uses` resets to 0 on an overwrite rather than being carried forward: a
/// `refresh` produces a *different* plan for the same question, and attributing
/// the old plan's hits to it would misreport both.
pub(super) fn upsert(
    conn: &Connection,
    account_id: i64,
    hash: &str,
    plan: &CachedPlan,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO query_plan_cache
             (account_id, query_hash, raw, compiled, intent, notes, model, created_at,
              last_used_at, uses)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, 0)
         ON CONFLICT(account_id, query_hash) DO UPDATE SET
             raw = excluded.raw,
             compiled = excluded.compiled,
             intent = excluded.intent,
             notes = excluded.notes,
             model = excluded.model,
             created_at = excluded.created_at,
             last_used_at = excluded.last_used_at,
             uses = 0",
        rusqlite::params![
            account_id,
            hash,
            plan.raw,
            plan.compiled,
            plan.intent,
            plan.notes,
            plan.model,
            plan.created_at,
        ],
    )?;
    Ok(())
}
