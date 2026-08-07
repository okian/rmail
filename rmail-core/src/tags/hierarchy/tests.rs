use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::config::TagSyncMode;
use crate::storage::Database;

use super::super::repo;
use super::*;

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct TempDb(PathBuf, Database);

impl TempDb {
    fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-tagshier-{pid}-{n}.db"));
        let db = Database::open(&path).expect("open temp db");
        Self(path, db)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn db(&self) -> &Database {
        &self.1
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ =
                std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.path().display())));
        }
    }
}

fn seed_account(db: &Database) -> i64 {
    db.with_write(|conn| {
        crate::repo::insert_account(
            conn,
            &crate::repo::NewAccount {
                name: format!("acct-{}", COUNTER.fetch_add(1, Ordering::Relaxed)),
                ..Default::default()
            },
        )
    })
    .unwrap()
}

// ---------------------------------------------------------------------------
// ancestor_paths
// ---------------------------------------------------------------------------

#[test]
fn a_top_level_name_has_no_ancestors() {
    assert_eq!(ancestor_paths("work", "/"), Vec::<String>::new());
}

#[test]
fn ancestor_paths_are_full_path_prefixes_not_leaf_segments() {
    assert_eq!(
        ancestor_paths("project/alpha/q3", "/"),
        vec!["project".to_owned(), "project/alpha".to_owned()]
    );
}

#[test]
fn an_empty_separator_disables_hierarchy_entirely() {
    assert_eq!(ancestor_paths("project/alpha", ""), Vec::<String>::new());
}

// ---------------------------------------------------------------------------
// ensure_ancestors
// ---------------------------------------------------------------------------

#[test]
fn ensure_ancestors_auto_creates_the_parent_chain() {
    let tmp = TempDb::open();
    let account_id = seed_account(tmp.db());

    let parent_id = tmp
        .db()
        .with_write(move |conn| {
            ensure_ancestors(conn, account_id, "project/alpha/q3", "/", TagSyncMode::Auto)
        })
        .unwrap();
    let parent_id = parent_id.expect("a nested name has an immediate parent");

    let project = tmp
        .db()
        .with_read(move |conn| repo::get_tag_by_name(conn, account_id, "project"))
        .unwrap()
        .expect("`project` should have been auto-created");
    let alpha = tmp
        .db()
        .with_read(move |conn| repo::get_tag_by_name(conn, account_id, "project/alpha"))
        .unwrap()
        .expect("`project/alpha` should have been auto-created");

    assert_eq!(project.parent_id, None);
    assert_eq!(alpha.parent_id, Some(project.id));
    assert_eq!(
        parent_id, alpha.id,
        "the returned parent is the closest ancestor"
    );
}

#[test]
fn ensure_ancestors_reuses_an_existing_level_rather_than_duplicating_it() {
    let tmp = TempDb::open();
    let account_id = seed_account(tmp.db());

    tmp.db()
        .with_write(move |conn| {
            ensure_ancestors(conn, account_id, "project/alpha", "/", TagSyncMode::Auto)
        })
        .unwrap();
    tmp.db()
        .with_write(move |conn| {
            ensure_ancestors(conn, account_id, "project/beta", "/", TagSyncMode::Auto)
        })
        .unwrap();

    let count: i64 = tmp
        .db()
        .with_read(move |conn| {
            conn.query_row(
                "SELECT count(*) FROM tags WHERE account_id = ?1 AND name = 'project'",
                [account_id],
                |r| r.get(0),
            )
        })
        .unwrap();
    assert_eq!(count, 1, "the shared ancestor must not be created twice");
}

#[test]
fn a_top_level_name_gets_no_parent() {
    let tmp = TempDb::open();
    let account_id = seed_account(tmp.db());
    let parent_id = tmp
        .db()
        .with_write(move |conn| ensure_ancestors(conn, account_id, "work", "/", TagSyncMode::Auto))
        .unwrap();
    assert_eq!(parent_id, None);
}

// ---------------------------------------------------------------------------
// would_cycle
// ---------------------------------------------------------------------------

#[test]
fn a_tag_cannot_become_its_own_parent() {
    let tmp = TempDb::open();
    let account_id = seed_account(tmp.db());
    let id = tmp
        .db()
        .with_write(move |conn| {
            repo::insert_tag(
                conn,
                account_id,
                "work",
                None,
                None,
                TagSyncMode::Local,
                None,
            )
        })
        .unwrap();

    let cycle = tmp
        .db()
        .with_read(move |conn| would_cycle(conn, id, id))
        .unwrap();
    assert!(cycle, "a tag must not be settable as its own parent");
}

#[test]
fn reparenting_under_a_descendant_is_a_cycle() {
    let tmp = TempDb::open();
    let account_id = seed_account(tmp.db());
    // a -> b -> c (c's parent is b, b's parent is a).
    let a = tmp
        .db()
        .with_write(move |conn| {
            repo::insert_tag(conn, account_id, "a", None, None, TagSyncMode::Local, None)
        })
        .unwrap();
    let b = tmp
        .db()
        .with_write(move |conn| {
            repo::insert_tag(
                conn,
                account_id,
                "b",
                Some(a),
                None,
                TagSyncMode::Local,
                None,
            )
        })
        .unwrap();
    let c = tmp
        .db()
        .with_write(move |conn| {
            repo::insert_tag(
                conn,
                account_id,
                "c",
                Some(b),
                None,
                TagSyncMode::Local,
                None,
            )
        })
        .unwrap();

    // Reparenting `a` under `c` (a's own descendant) must be rejected: it
    // would make a -> c -> b -> a, a cycle with no root.
    let cycle = tmp
        .db()
        .with_read(move |conn| would_cycle(conn, a, c))
        .unwrap();
    assert!(
        cycle,
        "a must not become a descendant of its own descendant c"
    );

    // Reparenting `a` under `b` (also a's own descendant) is the same
    // failure one level up.
    let cycle = tmp
        .db()
        .with_read(move |conn| would_cycle(conn, a, b))
        .unwrap();
    assert!(
        cycle,
        "a must not become a descendant of its own descendant b"
    );
}

#[test]
fn reparenting_under_an_unrelated_tag_is_not_a_cycle() {
    let tmp = TempDb::open();
    let account_id = seed_account(tmp.db());
    let a = tmp
        .db()
        .with_write(move |conn| {
            repo::insert_tag(conn, account_id, "a", None, None, TagSyncMode::Local, None)
        })
        .unwrap();
    let unrelated = tmp
        .db()
        .with_write(move |conn| {
            repo::insert_tag(
                conn,
                account_id,
                "unrelated",
                None,
                None,
                TagSyncMode::Local,
                None,
            )
        })
        .unwrap();

    let cycle = tmp
        .db()
        .with_read(move |conn| would_cycle(conn, a, unrelated))
        .unwrap();
    assert!(!cycle, "an unrelated tag is a perfectly good new parent");
}
