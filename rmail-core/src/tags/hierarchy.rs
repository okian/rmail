//! Tag hierarchy: `/`-separated name segments, ancestor auto-vivification,
//! and cycle rejection (prd.md, III-4: "Hierarchy cycles rejected").
//!
//! # `tags.name` stores the full path, not a leaf segment
//!
//! A tag created as `"project/alpha"` is stored with exactly that string in
//! `name`, not `"alpha"` with an implicit lookup through `parent_id` to
//! reconstruct the path. Two things fall out of that choice: `UNIQUE
//! (account_id, name)` dedupes correctly across different parents (a
//! `"personal/alpha"` and a `"project/alpha"` are different names, not a
//! collision a leaf-only scheme would have to break some other way), and
//! `tag:project/*` (task 55's grammar) is a plain name-prefix match rather
//! than a recursive parent-chain walk per candidate row.
//!
//! [`ensure_ancestors`] is what keeps `parent_id` populated anyway, Gmail
//! nested-label style: creating `"project/alpha/q3"` auto-creates `"project"`
//! and `"project/alpha"` if they do not already exist, each pointing at the
//! level above it, so `parent_id` is always a real, useful edge for anything
//! that wants to walk the tree top-down (a future tag-palette hierarchy
//! browser) without re-parsing every tag's name.
//!
//! # Why a cycle is even possible when parents are name-derived
//!
//! Every parent [`ensure_ancestors`] assigns comes from splitting a *new*
//! tag's own name, so that path alone can never produce a cycle: a parent's
//! path is always strictly shorter than its child's. The cycle
//! [`would_cycle`] guards against is a different one -- [`super::TagStore::
//! create_tag`] also accepts an explicit `parent_id` (independent of the
//! `/`-name convention, the way a tag-picker UI reparenting an existing tag
//! would), and *that* can name any existing tag, including one of the target
//! tag's own descendants.

use rusqlite::Connection;

use crate::config::TagSyncMode;

use super::repo;

/// Bound on how many `parent_id` hops [`would_cycle`] follows before giving
/// up. A legitimate tag tree is a handful of levels deep
/// (`project/alpha/q3` is already unusual); this exists so a chain
/// corrupted by something outside this module's control (a hand-edited
/// database) makes cycle detection fail loudly rather than loop forever.
const MAX_ANCESTOR_DEPTH: u32 = 64;

/// Split `name` on `separator` into the full-path form of each of its
/// strict ancestors, e.g. `"project/alpha/q3"` with separator `"/"` ->
/// `["project", "project/alpha"]` (not `"project/alpha/q3"` itself -- that
/// is the tag being created, not one of its ancestors).
///
/// Returns an empty vec for a top-level name (no separator present) or an
/// empty separator (nothing to split on -- `hierarchy_separator = ""` is a
/// valid, if unusual, config value that simply turns hierarchy off).
fn ancestor_paths(name: &str, separator: &str) -> Vec<String> {
    if separator.is_empty() {
        return Vec::new();
    }
    let segments: Vec<&str> = name.split(separator).filter(|s| !s.is_empty()).collect();
    if segments.len() <= 1 {
        return Vec::new();
    }
    (1..segments.len())
        .map(|i| segments[..i].join(separator))
        .collect()
}

/// Ensure every ancestor path of `name` exists as a tag, creating any that
/// do not (with `default_sync_mode`, no color, no explicit parent beyond
/// the one this walk itself assigns) and return the immediate parent's id
/// -- `None` for a top-level name.
///
/// Each level is looked up before being created, so calling this twice for
/// sibling tags (`"project/alpha"` then `"project/beta"`) creates `"project"`
/// only once.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub(crate) fn ensure_ancestors(
    conn: &Connection,
    account_id: i64,
    name: &str,
    separator: &str,
    default_sync_mode: TagSyncMode,
) -> rusqlite::Result<Option<i64>> {
    let mut parent_id: Option<i64> = None;
    for path in ancestor_paths(name, separator) {
        parent_id = Some(match repo::get_tag_by_name(conn, account_id, &path)? {
            Some(tag) => tag.id,
            None => repo::insert_tag(
                conn,
                account_id,
                &path,
                parent_id,
                None,
                default_sync_mode,
                None,
            )?,
        });
    }
    Ok(parent_id)
}

/// Whether re-parenting `tag_id` under `candidate_parent_id` would create a
/// cycle: either `candidate_parent_id` *is* `tag_id` (a tag made its own
/// parent), or `tag_id` already appears somewhere in
/// `candidate_parent_id`'s own ancestor chain -- which would make
/// `candidate_parent_id` a descendant of `tag_id`, and therefore `tag_id` a
/// descendant of its own child once reparented under it.
///
/// Returns plain `rusqlite::Result` (not [`crate::Error`]) so callers can
/// run this alongside [`repo`]'s own functions inside a single
/// [`crate::storage::Database::write`] transaction closure without a error-type
/// seam in the middle. If walking `candidate_parent_id`'s existing
/// `parent_id` chain exceeds [`MAX_ANCESTOR_DEPTH`] -- which [`ensure_ancestors`]'s
/// own name-derived parents can never produce, only a chain corrupted by
/// something outside this module's control -- this fails *closed*: `Ok(true)`
/// (reject the reparent as a possible cycle) rather than surfacing an
/// internal error for a database problem unrelated to the caller's request.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub(crate) fn would_cycle(
    conn: &Connection,
    tag_id: i64,
    candidate_parent_id: i64,
) -> rusqlite::Result<bool> {
    let mut current = candidate_parent_id;
    for _ in 0..MAX_ANCESTOR_DEPTH {
        if current == tag_id {
            return Ok(true);
        }
        match repo::get_tag(conn, current)? {
            Some(tag) => match tag.parent_id {
                Some(parent) => current = parent,
                None => return Ok(false),
            },
            // The candidate parent (or one of its own ancestors) does not
            // exist -- nothing left to walk, and therefore no cycle found.
            None => return Ok(false),
        }
    }
    tracing::warn!(
        tag_id,
        candidate_parent_id,
        "tag ancestor chain exceeds {MAX_ANCESTOR_DEPTH} levels while checking for a cycle; \
         refusing the reparent rather than risk one"
    );
    Ok(true)
}

#[cfg(test)]
mod tests;
