//! What `interruptible_read` promises: a completed scan reports its value, an
//! already-superseded call never touches the database at all, and — the
//! claim task 28 exists to prove, not just assert — a scan cancelled while it
//! is genuinely running is genuinely stopped mid-flight, not merely
//! abandoned by a caller that walked away from the future.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use rusqlite::functions::FunctionFlags;
use tokio_util::sync::CancellationToken;

use super::*;

/// A large enough recursive scan that reaching it in full, uninterrupted,
/// takes on the order of seconds — far longer than the few milliseconds this
/// test needs to observe a partial count. The `tick` scalar function below is
/// the row counter: SQLite calls it once per row the recursive CTE produces,
/// so its value *is* how far the scan actually got, independent of whatever
/// this test later reads back as a result.
const HUGE_TARGET: i64 = 200_000_000;

/// How many rows [`tick`] must have counted before the test cancels the scan.
/// Large enough that the query is unmistakably mid-flight (not still
/// acquiring its connection), tiny next to [`HUGE_TARGET`] so the test does
/// not need to wait for any meaningful fraction of the full scan.
const CANCEL_AFTER_ROWS: u64 = 2_000;

/// Longest this test will poll for [`CANCEL_AFTER_ROWS`] before concluding
/// something is wrong rather than merely slow.
const POLL_TIMEOUT: Duration = Duration::from_secs(10);

struct Fixture {
    db: Database,
    path: std::path::PathBuf,
}

impl Fixture {
    fn open() -> Self {
        static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-retrieve-cancel-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(std::path::PathBuf::from(format!(
                "{}{suffix}",
                path.display()
            )));
        }
        let db = Database::open(&path).unwrap();
        Self { db, path }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(std::path::PathBuf::from(format!(
                "{}{suffix}",
                self.path.display()
            )));
        }
    }
}

/// Register a `tick()` SQL function that increments `counter` and returns its
/// new value — used as the recursive CTE's own counter column so the scan's
/// progress and the Rust-visible count are the same number by construction.
fn install_tick(conn: &Connection, counter: Arc<AtomicU64>) -> rusqlite::Result<()> {
    conn.create_scalar_function("tick", 0, FunctionFlags::SQLITE_UTF8, move |_| {
        Ok(counter.fetch_add(1, Ordering::Relaxed) as i64 + 1)
    })
}

const HUGE_SCAN_SQL: &str = "WITH RECURSIVE cnt(x) AS (
        SELECT tick()
        UNION ALL
        SELECT tick() FROM cnt WHERE x < ?1
    )
    SELECT count(*) FROM cnt";

#[tokio::test]
async fn a_completed_scan_returns_its_value() {
    let fx = Fixture::open();
    let cancel = CancellationToken::new();
    let result = interruptible_read(&fx.db, &cancel, |conn| {
        conn.query_row("SELECT 1 + 1", [], |row| row.get::<_, i64>(0))
    })
    .await
    .unwrap();
    assert_eq!(result, Some(2));
}

#[tokio::test]
async fn an_already_cancelled_token_never_runs_the_closure() {
    let fx = Fixture::open();
    let cancel = CancellationToken::new();
    cancel.cancel();

    let ran = Arc::new(AtomicU64::new(0));
    let ran_in_closure = Arc::clone(&ran);
    let result = interruptible_read(&fx.db, &cancel, move |_conn| {
        ran_in_closure.fetch_add(1, Ordering::Relaxed);
        Ok::<_, rusqlite::Error>(())
    })
    .await
    .unwrap();

    assert!(result.is_none());
    assert_eq!(
        ran.load(Ordering::Relaxed),
        0,
        "a query already superseded before it starts must not touch the database at all"
    );
}

#[tokio::test]
async fn cancelling_mid_scan_genuinely_stops_the_sqlite_work_not_just_the_wait() {
    let fx = Fixture::open();
    let cancel = CancellationToken::new();
    let counter = Arc::new(AtomicU64::new(0));

    let task = {
        let db = fx.db.clone();
        let cancel = cancel.clone();
        let counter = Arc::clone(&counter);
        tokio::spawn(async move {
            interruptible_read(&db, &cancel, move |conn| {
                install_tick(conn, counter)?;
                conn.query_row(HUGE_SCAN_SQL, [HUGE_TARGET], |row| row.get::<_, i64>(0))
            })
            .await
        })
    };

    // Poll instead of a fixed sleep: robust to how fast the machine running
    // this test happens to be, rather than picking a delay that is flaky
    // under CI load and needlessly slow on a fast box.
    tokio::time::timeout(POLL_TIMEOUT, async {
        while counter.load(Ordering::Relaxed) < CANCEL_AFTER_ROWS {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the scan should reach a few thousand rows well within the timeout");
    cancel.cancel();

    let outcome = task.await.unwrap().unwrap();
    assert!(
        outcome.is_none(),
        "an interrupted scan reports cancellation, not a completed row count"
    );

    let ticks = i64::try_from(counter.load(Ordering::Relaxed)).unwrap();
    assert!(
        ticks >= CANCEL_AFTER_ROWS as i64,
        "the scan must have gotten at least as far as the point this test waited for, got {ticks}"
    );
    assert!(
        ticks < HUGE_TARGET,
        "the scan must have been cut short well before the full {HUGE_TARGET} rows — it ran to \
         completion, meaning interrupt() never actually reached the running statement; got {ticks}"
    );
}

#[tokio::test]
async fn a_real_rusqlite_error_still_propagates_as_err() {
    let fx = Fixture::open();
    let cancel = CancellationToken::new();
    let err = interruptible_read(&fx.db, &cancel, |conn| {
        conn.query_row("SELECT * FROM no_such_table", [], |row| {
            row.get::<_, i64>(0)
        })
    })
    .await
    .expect_err("a genuine SQL error must not be swallowed as a cancellation");
    assert!(
        matches!(err, StorageError::Sqlite(_)),
        "expected a wrapped rusqlite error, got {err:?}"
    );
    // And it still maps to the domain error model a caller's `?` expects.
    assert_eq!(
        crate::Error::from(err).reason(),
        crate::ErrorReason::Internal
    );
}
