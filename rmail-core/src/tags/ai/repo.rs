//! Typed SQL for the auto-tagging pass: `tag_rules` (migration V43), the
//! accept/reject counts [`super::Learning`] is derived from, and the
//! already-user-tagged check that keeps the classifier off mail a person has
//! already filed.
//!
//! Kept beside the pass rather than folded into [`super::super::repo`] for the
//! same reason that module gives for not living in [`crate::repo`]: every
//! function here is specific to *this* pass, and nothing outside
//! [`super`] calls them.

use std::collections::HashMap;

use rusqlite::{named_params, params, Connection, OptionalExtension};

use super::{Learning, TagRule, TagRuleMode};

/// Every enabled rule for `account_id`, keyed by the folded name of the tag it
/// names (see [`super::fold`]) — the shape [`super::AutoApplyPolicy`] looks a
/// suggestion up in.
///
/// Joined to `tags` rather than returning `tag_id`, because the classifier
/// only ever knows a tag by name: it is answering from
/// `tags.ai.taxonomy`, which is a list of names, and the tag row itself may
/// not exist yet the first time a taxonomy entry is suggested.
///
/// A `mode` outside V43's two values cannot exist (the CHECK rejects it), and
/// a row read back with one anyway degrades to [`TagRuleMode::Suggest`] rather
/// than to `Auto` — an unreadable policy grants nothing, the same fail-closed
/// choice `notify::Threshold` and `ai::deep`'s `priority_at_least` make. See
/// [`TagRuleMode::parse`].
///
/// # Errors
/// Propagates any `rusqlite` error.
pub(super) fn enabled_rules(
    conn: &Connection,
    account_id: i64,
) -> rusqlite::Result<HashMap<String, TagRule>> {
    let mut stmt = conn.prepare(
        "SELECT r.id, r.account_id, r.name, r.tag_id, t.name AS tag_name, r.mode,
                r.min_conf, r.enabled
         FROM tag_rules r
         JOIN tags t ON t.id = r.tag_id
         WHERE r.account_id = ?1 AND r.enabled = 1
         ORDER BY r.id",
    )?;
    let rows = stmt.query_map([account_id], |row| {
        Ok(TagRule {
            id: row.get("id")?,
            account_id: row.get("account_id")?,
            name: row.get("name")?,
            tag_id: row.get("tag_id")?,
            tag_name: row.get("tag_name")?,
            mode: TagRuleMode::parse(&row.get::<_, String>("mode")?),
            min_conf: row.get("min_conf")?,
            enabled: row.get::<_, i64>("enabled")? != 0,
        })
    })?;
    let mut out = HashMap::new();
    for rule in rows {
        let rule = rule?;
        out.insert(super::fold(&rule.tag_name), rule);
    }
    Ok(out)
}

/// Every rule for `account_id`, newest id last — the listing behind
/// [`crate::tags::TagStore::list_tag_rules`]. Unlike [`enabled_rules`] this
/// keeps disabled rows — an operator asking what rules exist wants to see the
/// one they switched off.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub(crate) fn all_rules(conn: &Connection, account_id: i64) -> rusqlite::Result<Vec<TagRule>> {
    let mut stmt = conn.prepare(
        "SELECT r.id, r.account_id, r.name, r.tag_id, t.name AS tag_name, r.mode,
                r.min_conf, r.enabled
         FROM tag_rules r
         JOIN tags t ON t.id = r.tag_id
         WHERE r.account_id = ?1
         ORDER BY r.id",
    )?;
    let rows = stmt.query_map([account_id], |row| {
        Ok(TagRule {
            id: row.get("id")?,
            account_id: row.get("account_id")?,
            name: row.get("name")?,
            tag_id: row.get("tag_id")?,
            tag_name: row.get("tag_name")?,
            mode: TagRuleMode::parse(&row.get::<_, String>("mode")?),
            min_conf: row.get("min_conf")?,
            enabled: row.get::<_, i64>("enabled")? != 0,
        })
    })?;
    rows.collect()
}

/// Create the rule named `name` for `account_id`, or update the existing one
/// of that name in place — the write behind
/// [`crate::tags::TagStore::set_tag_rule`]. Returns its id.
///
/// # Errors
/// Propagates any `rusqlite` error, including V43's `CHECK` on `mode`/
/// `min_conf` (a caller passing an out-of-range floor is a bad request, not a
/// silently clamped one).
pub(crate) fn upsert_rule(
    conn: &Connection,
    account_id: i64,
    name: &str,
    tag_id: i64,
    mode: TagRuleMode,
    min_conf: f64,
    enabled: bool,
) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO tag_rules (account_id, name, tag_id, mode, min_conf, enabled)
         VALUES (:account_id, :name, :tag_id, :mode, :min_conf, :enabled)
         ON CONFLICT(account_id, name) DO UPDATE SET
             tag_id = excluded.tag_id,
             mode = excluded.mode,
             min_conf = excluded.min_conf,
             enabled = excluded.enabled",
        named_params! {
            ":account_id": account_id,
            ":name": name,
            ":tag_id": tag_id,
            ":mode": mode.as_str(),
            ":min_conf": min_conf,
            ":enabled": i64::from(enabled),
        },
    )?;
    conn.query_row(
        "SELECT id FROM tag_rules WHERE account_id = ?1 AND name = ?2",
        params![account_id, name],
        |row| row.get(0),
    )
}

/// Whether `message_id` already carries a tag a *person* applied — the
/// "skip already-user-tagged mail" cost control (prd.md III-4, "Claude
/// Integration").
///
/// Effective, not just direct: a tag applied to the message's thread covers
/// this message too (that is what `messages_tags_effective` means), and a
/// person who filed a whole thread has filed this message with it. Only
/// `source = 'user'` counts — an `'imap'` keyword the server happened to
/// carry, a `'rule'` auto-application, and this pass's own `'ai'` rows are
/// not somebody having made a decision about this message.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub(super) fn has_user_applied_tag(conn: &Connection, message_id: i64) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT EXISTS (
             SELECT 1 FROM message_tags mt
             WHERE mt.source = 'user' AND mt.state = 'applied'
               AND (mt.message_id = ?1
                    OR mt.thread_id = (SELECT thread_id FROM messages WHERE id = ?1))
         )",
        [message_id],
        |row| row.get::<_, i64>(0),
    )
    .map(|n| n != 0)
}

/// `messages.account_id`/`thread_id` for a message, or `None` if it no longer
/// exists.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub(super) fn message_scope(
    conn: &Connection,
    message_id: i64,
) -> rusqlite::Result<Option<(i64, Option<i64>)>> {
    conn.query_row(
        "SELECT account_id, thread_id FROM messages WHERE id = ?1",
        [message_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()
}

/// How the recipient has ruled on this account's past AI tag suggestions, per
/// tag, keyed by folded tag name — the whole of what
/// "learns from accept/reject decisions" (prd.md #12) reads.
///
/// Only `source = 'ai'` rows are counted, and only in a terminal state:
/// `applied` is a suggestion somebody accepted through
/// [`crate::tags::TagStore::resolve_suggestion`], `rejected` is one they
/// turned down. A row still `pending` is a decision that has not been made yet
/// and must not tilt anything either way, and an auto-applied row is
/// `source = 'rule'` precisely so it cannot be miscounted as an acceptance —
/// see V43's own header on why that separation matters.
///
/// `window_secs` bounds how far back a decision still counts, and it is what
/// keeps suppression from being an absorbing state — see
/// [`super::LEARNING_WINDOW_SECS`]. A row's `created_at` is when the
/// *suggestion* was written, not when it was answered (V24 has no
/// `resolved_at`); over a window measured in months the difference is noise,
/// and using the earlier of the two timestamps makes a decision age out
/// slightly sooner, which is the direction that errs toward re-offering a tag
/// rather than banning it for longer.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub(super) fn learning(
    conn: &Connection,
    account_id: i64,
    window_secs: i64,
) -> rusqlite::Result<HashMap<String, Learning>> {
    let mut stmt = conn.prepare(
        "SELECT t.name AS tag_name, mt.state, COUNT(*) AS n
         FROM message_tags mt
         JOIN tags t ON t.id = mt.tag_id
         WHERE t.account_id = ?1 AND mt.source = 'ai'
           AND mt.state IN ('applied', 'rejected')
           AND mt.created_at >= unixepoch() - ?2
         GROUP BY t.name, mt.state",
    )?;
    let rows = stmt.query_map(params![account_id, window_secs], |row| {
        Ok((
            row.get::<_, String>("tag_name")?,
            row.get::<_, String>("state")?,
            row.get::<_, i64>("n")?,
        ))
    })?;
    let mut out: HashMap<String, Learning> = HashMap::new();
    for row in rows {
        let (tag_name, state, n) = row?;
        let entry = out.entry(super::fold(&tag_name)).or_default();
        match state.as_str() {
            "applied" => entry.accepted += n,
            "rejected" => entry.rejected += n,
            // Unreachable: the `IN` above already restricts the two states,
            // and V24's CHECK restricts the column. Ignored rather than
            // asserted, since a panic in non-test code is not an option here.
            _ => {}
        }
    }
    Ok(out)
}
