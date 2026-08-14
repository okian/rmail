//! Core-schema reference: the set of tables the baseline migrations establish,
//! plus tests asserting the migrations actually created them and the hot-path
//! indexes.

/// The core (non-feature) tables established by the baseline migrations plus
/// the threading migration.
pub const CORE_TABLES: &[&str] = &[
    "accounts",
    "mailboxes",
    "contacts",
    "threads",
    "messages",
    "flags",
    "attachments",
    "sync_state",
    "thread_refs",
];

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;
    use crate::storage::Database;

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    struct TempDb {
        db: Database,
        path: PathBuf,
    }

    impl TempDb {
        fn open() -> Self {
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let pid = std::process::id();
            let path = std::env::temp_dir().join(format!("rmail-schema-{pid}-{n}.db"));
            let db = Database::open(&path).unwrap();
            Self { db, path }
        }
    }

    impl Drop for TempDb {
        fn drop(&mut self) {
            for suffix in ["", "-wal", "-shm"] {
                let _ =
                    std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.path.display())));
            }
        }
    }

    fn object_exists(db: &Database, kind: &str, name: &str) -> bool {
        db.with_read(|c| {
            c.query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = ?1 AND name = ?2",
                rusqlite::params![kind, name],
                |row| row.get::<_, i64>(0),
            )
        })
        .unwrap()
            > 0
    }

    #[test]
    fn all_core_tables_exist() {
        let tmp = TempDb::open();
        for table in CORE_TABLES {
            assert!(
                object_exists(&tmp.db, "table", table),
                "missing core table: {table}"
            );
        }
    }

    #[test]
    fn hot_path_indexes_exist() {
        let tmp = TempDb::open();
        for index in [
            "idx_messages_mailbox_date",
            // V37's keyset-pagination index. Distinct from the one above and
            // not redundant with it — see the migration for why.
            "idx_messages_mailbox_page",
            "idx_messages_account",
            "idx_messages_thread",
            "idx_messages_message_id",
            "idx_messages_in_reply_to",
            "idx_attachments_message",
            "idx_threads_last_message",
            "idx_threads_subject_norm",
            "idx_thread_refs_thread",
        ] {
            assert!(
                object_exists(&tmp.db, "index", index),
                "missing hot-path index: {index}"
            );
        }
    }

    /// The query plan for `sql`, as one string.
    fn plan_for(db: &Database, sql: &str) -> String {
        let sql = format!("EXPLAIN QUERY PLAN {sql}");
        db.with_read(|c| {
            let mut stmt = c.prepare(&sql)?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>("detail"))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .unwrap()
        .join(" | ")
    }

    #[test]
    fn the_two_argument_date_ordering_still_has_an_index() {
        // `idx_messages_mailbox_date` is no longer what `list_messages` uses
        // (see the paged test below), but a dozen `retrieve::*` queries still
        // order by this exact two-argument expression, so dropping it would
        // turn each of those into a sort.
        let tmp = TempDb::open();
        let joined = plan_for(
            &tmp.db,
            "SELECT id FROM messages WHERE mailbox_id = 1
             ORDER BY COALESCE(date, internaldate) DESC LIMIT 10",
        );
        assert!(
            joined.contains("idx_messages_mailbox_date"),
            "listing should use the composite index, plan was: {joined}"
        );
        assert!(
            !joined.to_uppercase().contains("TEMP B-TREE"),
            "listing must not need a temp B-tree sort, plan was: {joined}"
        );
    }

    #[test]
    fn the_first_page_of_a_mailbox_listing_avoids_a_temp_sort() {
        // The query `repo::list_messages` actually issues without a cursor.
        // Asserting on the shape nothing emits any more would let the index
        // regress unnoticed.
        let tmp = TempDb::open();
        let joined = plan_for(
            &tmp.db,
            "SELECT id FROM messages WHERE mailbox_id = 1
             ORDER BY COALESCE(date, internaldate, 0) DESC, id DESC LIMIT 10",
        );
        assert!(
            joined.contains("idx_messages_mailbox_page"),
            "the first page should use the pagination index, plan was: {joined}"
        );
        assert!(
            !joined.to_uppercase().contains("TEMP B-TREE"),
            "the first page must not need a temp B-tree sort, plan was: {joined}"
        );
    }

    #[test]
    fn a_paged_mailbox_listing_is_a_range_scan_not_a_sort() {
        // The point of `idx_messages_mailbox_page` (V37): a cursor turns into
        // a bound on the index prefix, so page N costs the same as page 1.
        // Without the index the planner scans the mailbox and sorts it — which
        // is invisible in a test that only checks the *rows* come back right,
        // and is precisely the regression keyset pagination exists to avoid.
        let tmp = TempDb::open();
        let joined = plan_for(
            &tmp.db,
            "SELECT id FROM messages
             WHERE mailbox_id = 1
               AND COALESCE(date, internaldate, 0) <= 100
               AND (COALESCE(date, internaldate, 0) < 100 OR id < 7)
             ORDER BY COALESCE(date, internaldate, 0) DESC, id DESC
             LIMIT 10",
        );
        assert!(
            joined.contains("idx_messages_mailbox_page"),
            "a paged listing should use the pagination index, plan was: {joined}"
        );
        assert!(
            !joined.to_uppercase().contains("TEMP B-TREE"),
            "a paged listing must not need a temp B-tree sort, plan was: {joined}"
        );
    }
}
