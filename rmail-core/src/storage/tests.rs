//! Storage-layer tests: WAL/pragmas, read-pool concurrency, migration
//! idempotency, read-only enforcement, async round-trip, and the
//! fault-injection rollback path.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use rusqlite::Connection;

use super::*;

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A unique temp database path that cleans up its files (db + `-wal` + `-shm`)
/// on drop. WAL requires a real file, so `:memory:` is not usable here.
struct TempDbPath(PathBuf);

impl TempDbPath {
    fn new() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        Self(std::env::temp_dir().join(format!("rmail-storage-{pid}-{n}.db")))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDbPath {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.0.display())));
        }
    }
}

#[test]
fn wal_and_pragmas_are_configured() {
    let tmp = TempDbPath::new();
    let db = Database::open(tmp.path()).unwrap();

    let mode: String = db
        .with_read(|c| c.query_row("PRAGMA journal_mode", [], |r| r.get(0)))
        .unwrap();
    assert_eq!(
        mode.to_lowercase(),
        "wal",
        "WAL journal mode must be enabled"
    );

    let reader_fk: i64 = db
        .with_read(|c| c.query_row("PRAGMA foreign_keys", [], |r| r.get(0)))
        .unwrap();
    assert_eq!(reader_fk, 1, "foreign_keys must be ON for readers");

    let query_only: i64 = db
        .with_read(|c| c.query_row("PRAGMA query_only", [], |r| r.get(0)))
        .unwrap();
    assert_eq!(query_only, 1, "read-pool connections must be query_only");

    let writer_fk: i64 = db
        .with_write(|c| c.query_row("PRAGMA foreign_keys", [], |r| r.get(0)))
        .unwrap();
    assert_eq!(writer_fk, 1, "foreign_keys must be ON for the writer");
}

#[test]
fn migrations_apply_once_idempotently() {
    let tmp = TempDbPath::new();

    let applied_history = |db: &Database| -> i64 {
        db.with_read(|c| {
            c.query_row("SELECT count(*) FROM refinery_schema_history", [], |r| {
                r.get(0)
            })
        })
        .unwrap()
    };

    let first = {
        let db = Database::open(tmp.path()).unwrap();
        let meta_tables: i64 = db
            .with_read(|c| {
                c.query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='_rmail_meta'",
                    [],
                    |r| r.get(0),
                )
            })
            .unwrap();
        assert_eq!(meta_tables, 1, "V1 migration must create _rmail_meta");
        applied_history(&db)
    };
    assert!(first > 0, "at least one migration must have applied");

    // Re-opening applies nothing new — the history is stable (idempotent).
    let db2 = Database::open(tmp.path()).unwrap();
    assert_eq!(
        applied_history(&db2),
        first,
        "reopening must not re-apply migrations"
    );
}

#[test]
fn read_pool_connections_reject_writes() {
    let tmp = TempDbPath::new();
    let db = Database::open(tmp.path()).unwrap();

    let result =
        db.with_read(|c| c.execute("INSERT INTO _rmail_meta (key, value) VALUES ('k', 'v')", []));
    assert!(result.is_err(), "a query_only reader must reject writes");

    // The writer, by contrast, can write.
    let rows = db
        .with_write(|c| c.execute("INSERT INTO _rmail_meta (key, value) VALUES ('k', 'v')", []))
        .unwrap();
    assert_eq!(rows, 1);
}

#[test]
fn readers_run_concurrently() {
    let tmp = TempDbPath::new();
    let db = Database::open(tmp.path()).unwrap();
    db.with_write(|c| c.execute("INSERT INTO _rmail_meta (key, value) VALUES ('k', 'v')", []))
        .unwrap();

    // Eight concurrent readers all succeed against the WAL db (they neither
    // block each other nor the writer).
    let handles: Vec<_> = (0..8)
        .map(|_| {
            let db = db.clone();
            std::thread::spawn(move || {
                db.with_read(|c| {
                    c.query_row("SELECT value FROM _rmail_meta WHERE key = 'k'", [], |r| {
                        r.get::<_, String>(0)
                    })
                })
            })
        })
        .collect();

    for handle in handles {
        let value = handle.join().unwrap().unwrap();
        assert_eq!(value, "v");
    }
}

#[tokio::test]
async fn async_read_write_roundtrip() {
    let tmp = TempDbPath::new();
    let db = Database::open(tmp.path()).unwrap();

    let rows = db
        .write(|c| {
            c.execute(
                "INSERT INTO _rmail_meta (key, value) VALUES (?1, ?2)",
                rusqlite::params!["a", "1"],
            )
        })
        .await
        .unwrap();
    assert_eq!(rows, 1);

    let value: String = db
        .read(|c| {
            c.query_row("SELECT value FROM _rmail_meta WHERE key = 'a'", [], |r| {
                r.get(0)
            })
        })
        .await
        .unwrap();
    assert_eq!(value, "1");
}

#[test]
fn failed_migration_rolls_back_and_reports_error() {
    let tmp = TempDbPath::new();
    let mut conn = Connection::open(tmp.path()).unwrap();

    // A migration that creates a table then runs invalid SQL. refinery runs it
    // in a transaction, so the whole thing must roll back on failure.
    let bad = refinery::Migration::unapplied(
        "V1__bad",
        "CREATE TABLE fault_table (id INTEGER);
         INSERT INTO fault_table (id) VALUES (missing_col);",
    )
    .unwrap();

    let result = refinery::Runner::new(&[bad]).run(&mut conn);
    let err = result.expect_err("a broken migration must fail");

    // Reports via the error model: refinery::Error -> StorageError -> Error.
    let storage_err = StorageError::from(err);
    let core_err: crate::Error = storage_err.into();
    assert_eq!(core_err.reason(), crate::ErrorReason::Internal);

    // Rolled back cleanly: the table the migration created must not exist.
    let table_count: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='fault_table'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(table_count, 0, "a failed migration must roll back its DDL");
}

#[test]
fn pool_size_is_honored() {
    let tmp = TempDbPath::new();
    let db = Database::open_with_pool_size(tmp.path(), 3).unwrap();
    assert_eq!(db.readers.max_size(), 3);
}

#[test]
fn reads_do_not_block_on_writer() {
    use std::sync::mpsc;

    let tmp = TempDbPath::new();
    let db = Database::open(tmp.path()).unwrap();
    db.with_write(|c| c.execute("INSERT INTO _rmail_meta (key, value) VALUES ('k', 'v')", []))
        .unwrap();

    // Occupy the single writer on another thread and keep it held.
    let held = db.clone();
    let (held_tx, held_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let writer = std::thread::spawn(move || {
        held.with_write(|_conn| {
            held_tx.send(()).unwrap(); // writer lock is now held
            release_rx.recv().unwrap(); // hold it until released
            Ok(())
        })
    });

    held_rx.recv().unwrap(); // wait until the writer is definitely held

    // A read must still succeed promptly while the writer is occupied, because
    // reads use the separate query_only pool, not the writer mutex.
    let value: String = db
        .with_read(|c| {
            c.query_row("SELECT value FROM _rmail_meta WHERE key = 'k'", [], |r| {
                r.get(0)
            })
        })
        .unwrap();
    assert_eq!(value, "v");

    release_tx.send(()).unwrap();
    writer.join().unwrap().unwrap();
}

#[test]
fn pool_exhaustion_maps_to_unavailable() {
    let tmp = TempDbPath::new();
    // Ensure the file + WAL exist.
    Database::open(tmp.path()).unwrap();

    // A size-1 pool with a short timeout, so exhaustion fails fast.
    let manager = r2d2_sqlite::SqliteConnectionManager::file(tmp.path());
    let pool = r2d2::Pool::builder()
        .max_size(1)
        .connection_timeout(std::time::Duration::from_millis(100))
        .build(manager)
        .unwrap();

    let _held = pool.get().unwrap(); // hold the only connection
    let pool_err = pool.get().expect_err("pool should be exhausted");

    let core_err: crate::Error = StorageError::Pool(pool_err).into();
    assert_eq!(
        core_err.reason(),
        crate::ErrorReason::Unavailable,
        "pool exhaustion is transient -> Unavailable"
    );
}

#[test]
fn sqlite_busy_is_transient_but_other_errors_are_internal() {
    // SQLITE_BUSY (primary code 5): transient contention -> Unavailable.
    let busy = StorageError::Sqlite(rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(5),
        Some("database is locked".to_owned()),
    ));
    assert!(busy.is_transient());
    let core: crate::Error = busy.into();
    assert_eq!(core.reason(), crate::ErrorReason::Unavailable);

    // A non-busy sqlite error is internal.
    let other = StorageError::Sqlite(rusqlite::Error::InvalidQuery);
    assert!(!other.is_transient());
    let core: crate::Error = other.into();
    assert_eq!(core.reason(), crate::ErrorReason::Internal);
}
