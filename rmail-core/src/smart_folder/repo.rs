//! Typed SQL over `smart_folders` and `smart_folder_matched` (migration
//! V26).
//!
//! Nothing here runs a predicate. Membership comes from
//! [`crate::tags::query::select_message_ids`] — the same statement `BulkTag`
//! resolves its selector with — and this module only records which members
//! this folder's actions have already fired for. See V26's own comment on
//! `smart_folder_matched` for why that ledger is not the membership.

use rusqlite::{Connection, OptionalExtension, Row, Transaction};

use crate::embed::Embedding;
use crate::index::semantic::VECTOR_DIM;

use super::{NewSmartFolder, SmartFolder};

/// Every column, in the order [`from_row`] reads them.
///
/// `query_vector` is deliberately absent: it is kilobytes per folder and no
/// caller listing folders needs it. [`query_vector`] reads it on its own, on
/// the one path that does.
const COLUMNS: &str = "id, account_id, name, predicate, auto_tag, notify, \
                       created_at, updated_at, last_evaluated_at, nl_source, \
                       vector_model, min_similarity, compiled_model, compiled_at";

fn from_row(row: &Row<'_>) -> rusqlite::Result<SmartFolder> {
    Ok(SmartFolder {
        id: row.get("id")?,
        account_id: row.get("account_id")?,
        name: row.get("name")?,
        predicate: row.get("predicate")?,
        auto_tag: row.get("auto_tag")?,
        notify: row.get::<_, i64>("notify")? != 0,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
        last_evaluated_at: row.get("last_evaluated_at")?,
        nl_source: row.get("nl_source")?,
        vector_model: row.get("vector_model")?,
        min_similarity: row.get("min_similarity")?,
        compiled_model: row.get("compiled_model")?,
        compiled_at: row.get("compiled_at")?,
    })
}

/// Insert a smart folder, returning the stored row.
///
/// Constraint failures are classified by the caller — see
/// [`crate::saved_search::repo::is_unique_violation`] /
/// [`crate::saved_search::repo::is_missing_reference`], which this task's
/// two tables share rather than each carrying their own copy of the same
/// extended-result-code match.
pub(crate) fn insert(conn: &Connection, spec: &NewSmartFolder) -> rusqlite::Result<SmartFolder> {
    conn.query_row(
        &format!(
            "INSERT INTO smart_folders
                 (account_id, name, predicate, auto_tag, notify, nl_source,
                  query_vector, vector_model, min_similarity, compiled_model, compiled_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11) RETURNING {COLUMNS}"
        ),
        rusqlite::params![
            spec.account_id,
            spec.name,
            spec.predicate,
            spec.auto_tag,
            i64::from(spec.notify),
            spec.nl_source,
            spec.query_vector.as_ref().map(Embedding::to_bytes),
            spec.vector_model,
            spec.min_similarity,
            spec.compiled_model,
            // Stamped only for a folder that was actually compiled; a
            // hand-written predicate has no compile to date.
            spec.nl_source
                .as_ref()
                .map(|_| chrono::Utc::now().timestamp()),
        ],
        from_row,
    )
}

/// One folder's frozen query vector, if it has one.
///
/// A row whose blob does not deserialize at the index's width returns `None`
/// with a warning rather than an error: a corrupt or stale vector must
/// degrade the dense arm, never make the folder unreadable. That the folder
/// then has *no* enforceable arm is impossible by construction —
/// `SmartFolderStore::create` refuses to store a folder whose only arm was the
/// dense one when no vector was available — so what a `None` here can cost is
/// recall, not the account.
pub(crate) fn query_vector(conn: &Connection, id: i64) -> rusqlite::Result<Option<Embedding>> {
    let bytes: Option<Vec<u8>> = conn
        .query_row(
            "SELECT query_vector FROM smart_folders WHERE id = ?1",
            [id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    let Some(bytes) = bytes else {
        return Ok(None);
    };
    match Embedding::from_bytes(&bytes, VECTOR_DIM) {
        Ok(vector) => Ok(Some(vector)),
        Err(error) => {
            tracing::warn!(
                smart_folder_id = id,
                %error,
                "a smart folder's stored query vector could not be read; its dense arm \
                 contributes nothing"
            );
            Ok(None)
        }
    }
}

/// One account's smart folders, alphabetical by name.
pub(crate) fn list(conn: &Connection, account_id: i64) -> rusqlite::Result<Vec<SmartFolder>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM smart_folders WHERE account_id = ?1 ORDER BY name"
    ))?;
    let rows = stmt
        .query_map([account_id], from_row)?
        .collect::<rusqlite::Result<Vec<SmartFolder>>>()?;
    Ok(rows)
}

/// Look one up by row id.
pub(crate) fn get(conn: &Connection, id: i64) -> rusqlite::Result<Option<SmartFolder>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM smart_folders WHERE id = ?1"),
        [id],
        from_row,
    )
    .optional()
}

/// Look one up by name within an account (case-insensitive, per the column's
/// `COLLATE NOCASE`).
pub(crate) fn get_by_name(
    conn: &Connection,
    account_id: i64,
    name: &str,
) -> rusqlite::Result<Option<SmartFolder>> {
    conn.query_row(
        &format!("SELECT {COLUMNS} FROM smart_folders WHERE account_id = ?1 AND name = ?2"),
        rusqlite::params![account_id, name],
        from_row,
    )
    .optional()
}

/// Delete by name; `true` if a row was removed.
pub(crate) fn delete(conn: &Connection, account_id: i64, name: &str) -> rusqlite::Result<bool> {
    let removed = conn.execute(
        "DELETE FROM smart_folders WHERE account_id = ?1 AND name = ?2",
        rusqlite::params![account_id, name],
    )?;
    Ok(removed > 0)
}

/// Every smart folder id in the database, oldest first — the boot-time
/// full pass [`super::SmartFolderEvaluator`] runs before it starts following
/// the event log.
pub(crate) fn all_ids(conn: &Connection) -> rusqlite::Result<Vec<i64>> {
    let mut stmt = conn.prepare("SELECT id FROM smart_folders ORDER BY id")?;
    let rows = stmt
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<i64>>>()?;
    Ok(rows)
}

/// One account's smart folder ids, oldest first.
pub(crate) fn ids_for_account(conn: &Connection, account_id: i64) -> rusqlite::Result<Vec<i64>> {
    let mut stmt =
        conn.prepare("SELECT id FROM smart_folders WHERE account_id = ?1 ORDER BY id")?;
    let rows = stmt
        .query_map([account_id], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<i64>>>()?;
    Ok(rows)
}

/// What one reconciliation of the action ledger against live membership
/// changed.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Reconciled {
    /// Messages that were not in the ledger and now are.
    pub entered: Vec<i64>,
    /// Messages that were in the ledger and no longer match.
    pub departed: Vec<i64>,
    /// Every current member whose actions have not fired yet — `entered`
    /// plus anything left unstamped by an earlier evaluation that was
    /// interrupted between firing and stamping. See V26's comment on
    /// `smart_folder_matched.fired_at`.
    pub pending: Vec<i64>,
}

/// Bring the ledger in line with `current` (which must be ascending), and
/// report the difference.
///
/// `stamp_entered` writes `fired_at` on the newly inserted rows immediately
/// — used for the baseline recorded at create time, where by definition
/// nothing is "new" and nothing should fire.
///
/// Runs inside the caller's transaction, so *this* function's read-diff-write
/// is atomic. That alone is **not** enough to make firing exactly-once —
/// `pending` includes rows an earlier, still-in-flight evaluation has claimed
/// but not yet stamped, and stamping happens in a later transaction. What
/// closes that gap is [`super::SmartFolderStore::evaluate`]'s per-folder
/// lock, held across the whole reconcile → fire → stamp sequence; see its own
/// docs.
pub(crate) fn reconcile(
    tx: &Transaction<'_>,
    smart_folder_id: i64,
    current: &[i64],
    stamp_entered: bool,
) -> rusqlite::Result<Reconciled> {
    let known: Vec<i64> = {
        let mut stmt = tx.prepare(
            "SELECT message_id FROM smart_folder_matched
             WHERE smart_folder_id = ?1 ORDER BY message_id",
        )?;
        let rows = stmt
            .query_map([smart_folder_id], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<i64>>>()?;
        rows
    };

    // Both sides are ascending (`select_message_ids` orders by id, the read
    // above orders by message_id), so the difference is a linear merge
    // rather than two hash sets — and, more usefully, `entered`/`departed`
    // come out ordered, which is what makes an evaluation's reported delta
    // stable enough to assert on.
    let (mut i, mut j) = (0usize, 0usize);
    let mut entered = Vec::new();
    let mut departed = Vec::new();
    while i < current.len() && j < known.len() {
        match current[i].cmp(&known[j]) {
            std::cmp::Ordering::Less => {
                entered.push(current[i]);
                i += 1;
            }
            std::cmp::Ordering::Greater => {
                departed.push(known[j]);
                j += 1;
            }
            std::cmp::Ordering::Equal => {
                i += 1;
                j += 1;
            }
        }
    }
    entered.extend_from_slice(&current[i..]);
    departed.extend_from_slice(&known[j..]);

    if !departed.is_empty() {
        let mut stmt = tx.prepare(
            "DELETE FROM smart_folder_matched WHERE smart_folder_id = ?1 AND message_id = ?2",
        )?;
        for id in &departed {
            stmt.execute(rusqlite::params![smart_folder_id, id])?;
        }
    }
    if !entered.is_empty() {
        // `SELECT ... WHERE EXISTS`, not a plain `VALUES`: membership was
        // resolved on a read connection a moment ago (see
        // `SmartFolderStore::evaluate`, which deliberately does not hold the
        // writer lock across a full scan), so an expunge landing in between
        // leaves an id here whose `messages` row is gone. A plain insert
        // would take the foreign key, fail the *whole* evaluation, and make
        // every other genuinely new member in this folder wait for the next
        // pass because one unrelated message was deleted.
        let fired_at: Option<i64> = if stamp_entered { Some(now(tx)?) } else { None };
        let mut stmt = tx.prepare(
            "INSERT INTO smart_folder_matched (smart_folder_id, message_id, fired_at)
             SELECT ?1, ?2, ?3 WHERE EXISTS (SELECT 1 FROM messages WHERE id = ?2)",
        )?;
        let mut inserted = Vec::with_capacity(entered.len());
        for id in &entered {
            if stmt.execute(rusqlite::params![smart_folder_id, id, fired_at])? > 0 {
                inserted.push(*id);
            }
        }
        // A vanished message never entered the folder as far as anyone can
        // observe: it is not in the ledger, it is not in membership, and
        // reporting it as `entered` would name an id a caller cannot look up.
        entered = inserted;
    }

    let pending: Vec<i64> = {
        let mut stmt = tx.prepare(
            "SELECT message_id FROM smart_folder_matched
             WHERE smart_folder_id = ?1 AND fired_at IS NULL ORDER BY message_id",
        )?;
        let rows = stmt
            .query_map([smart_folder_id], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<i64>>>()?;
        rows
    };

    tx.execute(
        "UPDATE smart_folders SET last_evaluated_at = unixepoch() WHERE id = ?1",
        [smart_folder_id],
    )?;

    Ok(Reconciled {
        entered,
        departed,
        pending,
    })
}

/// Stamp `fired_at` on rows whose actions have now run.
///
/// One transaction, not N autocommits: a folder whose predicate just admitted
/// a thousand messages would otherwise pay a thousand WAL syncs while holding
/// the single writer connection.
pub(crate) fn mark_fired(
    conn: &mut Connection,
    smart_folder_id: i64,
    message_ids: &[i64],
) -> rusqlite::Result<usize> {
    if message_ids.is_empty() {
        return Ok(0);
    }
    let tx = conn.transaction()?;
    let mut stamped = 0usize;
    {
        let mut stmt = tx.prepare(
            "UPDATE smart_folder_matched SET fired_at = unixepoch()
             WHERE smart_folder_id = ?1 AND message_id = ?2 AND fired_at IS NULL",
        )?;
        for id in message_ids {
            stamped += stmt.execute(rusqlite::params![smart_folder_id, id])?;
        }
    }
    tx.commit()?;
    Ok(stamped)
}

/// The ledger as it stands — for tests and diagnostics only; membership is
/// never read from here (see the module docs).
#[cfg(test)]
pub(crate) fn ledger(
    conn: &Connection,
    smart_folder_id: i64,
) -> rusqlite::Result<Vec<(i64, Option<i64>)>> {
    let mut stmt = conn.prepare(
        "SELECT message_id, fired_at FROM smart_folder_matched
         WHERE smart_folder_id = ?1 ORDER BY message_id",
    )?;
    let rows = stmt
        .query_map([smart_folder_id], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<rusqlite::Result<Vec<(i64, Option<i64>)>>>()?;
    Ok(rows)
}

/// `unixepoch()` as SQLite itself computes it, so a stamped `fired_at` uses
/// the same clock as the column's own `DEFAULT (unixepoch())` rather than
/// the host's `chrono` view of it.
fn now(tx: &Transaction<'_>) -> rusqlite::Result<i64> {
    tx.query_row("SELECT unixepoch()", [], |row| row.get(0))
}
