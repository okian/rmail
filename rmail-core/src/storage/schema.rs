//! Core-schema reference: the set of tables the baseline migrations establish,
//! plus tests asserting the migrations actually created them and the hot-path
//! indexes.

/// The core (non-feature) tables established by the baseline migrations.
pub const CORE_TABLES: &[&str] = &[
    "accounts",
    "mailboxes",
    "contacts",
    "threads",
    "messages",
    "flags",
    "attachments",
    "sync_state",
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
            "idx_messages_account",
            "idx_messages_thread",
            "idx_messages_message_id",
            "idx_messages_in_reply_to",
            "idx_attachments_message",
            "idx_threads_last_message",
        ] {
            assert!(
                object_exists(&tmp.db, "index", index),
                "missing hot-path index: {index}"
            );
        }
    }

    #[test]
    fn mailbox_date_listing_avoids_a_temp_sort() {
        // The composite index must back BOTH the mailbox filter and the
        // COALESCE(date, internaldate) ordering — no full temp B-tree sort.
        let tmp = TempDb::open();
        let plan: Vec<String> = tmp
            .db
            .with_read(|c| {
                let mut stmt = c.prepare(
                    "EXPLAIN QUERY PLAN
                     SELECT id FROM messages WHERE mailbox_id = 1
                     ORDER BY COALESCE(date, internaldate) DESC LIMIT 10",
                )?;
                let rows = stmt
                    .query_map([], |row| row.get::<_, String>("detail"))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .unwrap();
        let joined = plan.join(" | ");
        assert!(
            joined.contains("idx_messages_mailbox_date"),
            "listing should use the composite index, plan was: {joined}"
        );
        assert!(
            !joined.to_uppercase().contains("TEMP B-TREE"),
            "listing must not need a temp B-tree sort, plan was: {joined}"
        );
    }
}
