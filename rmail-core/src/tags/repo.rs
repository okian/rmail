//! Typed SQL accessors over `tags`/`message_tags` (migration V24).
//!
//! Kept self-contained in this module rather than added to
//! [`crate::repo`] (the shared cross-cutting accessor module several other
//! in-flight tasks also touch): every function here is specific to the tags
//! domain, and nothing outside [`super`] needs to call them directly. This
//! mirrors how `ai`/`index` keep their own table's accessors inside their
//! own module tree rather than piling into `repo::mod`.

use rusqlite::{named_params, params, Connection, OptionalExtension};

use crate::config::TagSyncMode;

use super::model::{
    MessageTag, NewMessageTag, PendingSuggestion, Tag, TagState, TagWithCount, Target,
};

const TAG_COLS: &str =
    "id, account_id, name, parent_id, color, sync_mode, imap_keyword, created_at";

const MESSAGE_TAG_COLS: &str =
    "id, tag_id, message_id, thread_id, source, state, confidence, rationale, created_at";

// ---------------------------------------------------------------------------
// tags
// ---------------------------------------------------------------------------

/// Insert a tag, returning its new id.
///
/// # Errors
/// Propagates any `rusqlite` error (e.g. a duplicate `(account_id, name)`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn insert_tag(
    conn: &Connection,
    account_id: i64,
    name: &str,
    parent_id: Option<i64>,
    color: Option<&str>,
    sync_mode: TagSyncMode,
    imap_keyword: Option<&str>,
) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO tags (account_id, name, parent_id, color, sync_mode, imap_keyword)
         VALUES (:account_id, :name, :parent_id, :color, :sync_mode, :imap_keyword)",
        named_params! {
            ":account_id": account_id,
            ":name": name,
            ":parent_id": parent_id,
            ":color": color,
            ":sync_mode": sync_mode,
            ":imap_keyword": imap_keyword,
        },
    )?;
    Ok(conn.last_insert_rowid())
}

/// Fetch a tag by id.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub(crate) fn get_tag(conn: &Connection, id: i64) -> rusqlite::Result<Option<Tag>> {
    conn.query_row(
        &format!("SELECT {TAG_COLS} FROM tags WHERE id = ?1"),
        [id],
        Tag::from_row,
    )
    .optional()
}

/// Fetch a tag by its unique `(account_id, name)`.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub(crate) fn get_tag_by_name(
    conn: &Connection,
    account_id: i64,
    name: &str,
) -> rusqlite::Result<Option<Tag>> {
    conn.query_row(
        &format!("SELECT {TAG_COLS} FROM tags WHERE account_id = ?1 AND name = ?2"),
        params![account_id, name],
        Tag::from_row,
    )
    .optional()
}

/// List an account's tags with their effective message counts, alphabetical
/// by name -- the `mail tags` / `ListTags` view.
///
/// `LEFT JOIN` so a brand-new tag with zero applications still appears (with
/// `message_count = 0`) rather than being silently absent from the listing.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub(crate) fn list_tags_with_counts(
    conn: &Connection,
    account_id: i64,
) -> rusqlite::Result<Vec<TagWithCount>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {cols}, COUNT(mte.message_id) AS message_count
         FROM tags t
         LEFT JOIN messages_tags_effective mte ON mte.tag_id = t.id
         WHERE t.account_id = ?1
         GROUP BY t.id
         ORDER BY t.name",
        cols = TAG_COLS
            .split(", ")
            .map(|c| format!("t.{c}"))
            .collect::<Vec<_>>()
            .join(", "),
    ))?;
    let rows = stmt.query_map([account_id], |row| {
        Ok(TagWithCount {
            tag: Tag::from_row(row)?,
            message_count: row.get("message_count")?,
        })
    })?;
    rows.collect()
}

/// Update a tag's mutable display/sync fields (used by `CreateTag`'s
/// upsert-by-name path). `None` leaves a field unchanged.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub(crate) fn update_tag_fields(
    conn: &Connection,
    id: i64,
    color: Option<&str>,
    sync_mode: Option<TagSyncMode>,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE tags SET
             color = COALESCE(:color, color),
             sync_mode = COALESCE(:sync_mode, sync_mode)
         WHERE id = :id",
        named_params! {
            ":id": id,
            ":color": color,
            ":sync_mode": sync_mode,
        },
    )?;
    Ok(())
}

/// Re-parent a tag. Callers must run [`super::hierarchy::would_cycle`]
/// first -- this function performs no cycle check of its own.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub(crate) fn set_tag_parent(
    conn: &Connection,
    id: i64,
    parent_id: Option<i64>,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE tags SET parent_id = ?2 WHERE id = ?1",
        params![id, parent_id],
    )?;
    Ok(())
}

/// Force a tag to `sync_mode = local` -- the auto-downgrade path (see
/// [`super::sync`]).
///
/// # Errors
/// Propagates any `rusqlite` error.
pub(crate) fn downgrade_tag_to_local(conn: &Connection, id: i64) -> rusqlite::Result<()> {
    conn.execute("UPDATE tags SET sync_mode = 'local' WHERE id = ?1", [id])?;
    Ok(())
}

// ---------------------------------------------------------------------------
// message_tags
// ---------------------------------------------------------------------------

/// Apply a tag to a target, idempotently: a second call for the same
/// `(tag_id, target)` is a no-op (see migration V24's partial unique
/// indexes). Returns the new row's id, or `None` if it already existed.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub(crate) fn insert_message_tag(
    conn: &Connection,
    new: &NewMessageTag,
) -> rusqlite::Result<Option<i64>> {
    let affected = match new.target {
        Target::Message(message_id) => conn.execute(
            "INSERT INTO message_tags (tag_id, message_id, thread_id, source, state, confidence, rationale)
             VALUES (?1, ?2, NULL, ?3, ?4, ?5, ?6)
             ON CONFLICT (tag_id, message_id) WHERE message_id IS NOT NULL DO NOTHING",
            params![
                new.tag_id,
                message_id,
                new.source,
                new.state,
                new.confidence,
                new.rationale
            ],
        )?,
        Target::Thread(thread_id) => conn.execute(
            "INSERT INTO message_tags (tag_id, message_id, thread_id, source, state, confidence, rationale)
             VALUES (?1, NULL, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (tag_id, thread_id) WHERE thread_id IS NOT NULL DO NOTHING",
            params![
                new.tag_id,
                thread_id,
                new.source,
                new.state,
                new.confidence,
                new.rationale
            ],
        )?,
    };
    if affected == 0 {
        return Ok(None);
    }
    Ok(Some(conn.last_insert_rowid()))
}

/// Remove a tag from a target. Returns whether a row was removed.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub(crate) fn delete_message_tag(
    conn: &Connection,
    tag_id: i64,
    target: Target,
) -> rusqlite::Result<bool> {
    let affected = match target {
        Target::Message(message_id) => conn.execute(
            "DELETE FROM message_tags WHERE tag_id = ?1 AND message_id = ?2",
            params![tag_id, message_id],
        )?,
        Target::Thread(thread_id) => conn.execute(
            "DELETE FROM message_tags WHERE tag_id = ?1 AND thread_id = ?2",
            params![tag_id, thread_id],
        )?,
    };
    Ok(affected > 0)
}

/// Fetch a `message_tags` row by id.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub(crate) fn get_message_tag(conn: &Connection, id: i64) -> rusqlite::Result<Option<MessageTag>> {
    conn.query_row(
        &format!("SELECT {MESSAGE_TAG_COLS} FROM message_tags WHERE id = ?1"),
        [id],
        MessageTag::from_row,
    )
    .optional()
}

/// Move a `message_tags` row from `pending` to `state`. Returns whether a
/// row was updated (`false` if it did not exist, or was not `pending`).
///
/// # Errors
/// Propagates any `rusqlite` error.
pub(crate) fn resolve_message_tag(
    conn: &Connection,
    id: i64,
    state: TagState,
) -> rusqlite::Result<bool> {
    let affected = conn.execute(
        "UPDATE message_tags SET state = ?2 WHERE id = ?1 AND state = 'pending'",
        params![id, state],
    )?;
    Ok(affected > 0)
}

/// A message's pending suggestions, joined with the tag each names, newest
/// first -- `SuggestTags`'s backing read (see the module docs on why task 55
/// streams *existing* pending rows rather than calling a model itself).
///
/// # Errors
/// Propagates any `rusqlite` error.
pub(crate) fn list_pending_suggestions(
    conn: &Connection,
    message_id: i64,
) -> rusqlite::Result<Vec<PendingSuggestion>> {
    // `tags` and `message_tags` both have an `id` and a `created_at` column,
    // so the two sides cannot share this query's bare column names the way
    // `Tag::from_row`/`MessageTag::from_row` (both built for a single-table
    // `SELECT`) expect -- `row.get("id")` would silently resolve to
    // whichever `id` came first in the result set for *both* calls. The
    // `message_tags` side is left unaliased (so `MessageTag::from_row` keeps
    // working unmodified) and only the `tags` side gets unique aliases,
    // assembled into a `Tag` by hand below rather than through
    // `Tag::from_row`.
    let mut stmt = conn.prepare(
        "SELECT mt.id, mt.tag_id, mt.message_id, mt.thread_id, mt.source, mt.state,
                mt.confidence, mt.rationale, mt.created_at,
                t.id AS tag_pk, t.account_id AS tag_account_id, t.name AS tag_name,
                t.parent_id AS tag_parent_id, t.color AS tag_color,
                t.sync_mode AS tag_sync_mode, t.imap_keyword AS tag_imap_keyword,
                t.created_at AS tag_created_at
         FROM message_tags mt
         JOIN tags t ON t.id = mt.tag_id
         WHERE mt.message_id = ?1 AND mt.state = 'pending'
         ORDER BY mt.created_at DESC, mt.id DESC",
    )?;
    let rows = stmt.query_map([message_id], |row| {
        let message_tag = MessageTag::from_row(row)?;
        let tag = Tag {
            id: row.get("tag_pk")?,
            account_id: row.get("tag_account_id")?,
            name: row.get("tag_name")?,
            parent_id: row.get("tag_parent_id")?,
            color: row.get("tag_color")?,
            sync_mode: row.get("tag_sync_mode")?,
            imap_keyword: row.get("tag_imap_keyword")?,
            created_at: row.get("tag_created_at")?,
        };
        Ok(PendingSuggestion { message_tag, tag })
    })?;
    rows.collect()
}

#[cfg(test)]
mod tests;
