//! SQLite storage foundation.
//!
//! [`Database`] wraps a WAL-mode SQLite database with a **single writer** (a
//! `Mutex`-guarded connection) and a **pool of read connections**, so search
//! and other reads never block on writes. Every connection is configured with
//! a busy timeout, foreign-key enforcement, and WAL-appropriate pragmas;
//! read-pool connections are additionally `query_only`. Pending [`refinery`]
//! migrations run idempotently on [`Database::open`].
//!
//! Blocking SQLite calls are exposed both synchronously
//! ([`Database::with_read`]/[`Database::with_write`]) and asynchronously
//! ([`Database::read`]/[`Database::write`], which offload to a blocking thread
//! so the async runtime is never blocked).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use rusqlite::Connection;

pub mod schema;

/// Migrations embedded from `rmail-core/migrations` at compile time.
mod embedded {
    refinery::embed_migrations!("./migrations");
}

/// Default number of pooled read connections.
pub const DEFAULT_READ_POOL_SIZE: u32 = 8;

/// Busy timeout applied to every connection.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

/// Errors from the storage layer.
#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    /// A `rusqlite` operation failed.
    #[error("database error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// Acquiring a pooled connection failed (pool exhausted / timed out).
    #[error("connection pool error: {0}")]
    Pool(#[from] r2d2::Error),

    /// A migration failed (and was rolled back).
    #[error("migration failed: {0}")]
    Migration(#[from] refinery::Error),

    /// A filesystem operation around the database file failed.
    #[error("storage i/o error: {0}")]
    Io(#[from] std::io::Error),

    /// A blocking database task failed to join (panicked or was cancelled).
    #[error("blocking database task failed: {0}")]
    Task(String),

    /// The writer lock was poisoned by a panic in another thread.
    #[error("writer lock poisoned")]
    Poisoned,

    /// WAL journal mode could not be established (reads-never-block-writes
    /// relies on it), leaving the database in `{0}` mode.
    #[error("WAL journal mode unavailable (got {0:?})")]
    WalUnavailable(String),
}

impl StorageError {
    /// Whether this error is transient and worth retrying — pool
    /// exhaustion/timeout, or a `SQLITE_BUSY`/`SQLITE_LOCKED` after the busy
    /// timeout elapsed (e.g. contention with a checkpoint or another writer).
    #[must_use]
    pub fn is_transient(&self) -> bool {
        match self {
            StorageError::Pool(_) => true,
            StorageError::Sqlite(rusqlite::Error::SqliteFailure(e, _)) => matches!(
                e.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            ),
            _ => false,
        }
    }
}

impl From<StorageError> for crate::Error {
    fn from(err: StorageError) -> Self {
        if err.is_transient() {
            // Transient contention — clients may retry.
            Self::unavailable(err.to_string())
        } else {
            // Internal condition; detail is redacted at the gRPC boundary but
            // preserved here for logging.
            Self::internal(err.to_string())
        }
    }
}

/// A WAL-mode SQLite database: one writer, many pooled readers.
///
/// Cloning is cheap (shared handles) and safe to share across tasks/threads.
#[derive(Clone)]
pub struct Database {
    writer: Arc<Mutex<Connection>>,
    readers: Pool<SqliteConnectionManager>,
    path: PathBuf,
}

impl std::fmt::Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Database")
            .field("path", &self.path)
            .field("read_pool_size", &self.readers.max_size())
            .finish_non_exhaustive()
    }
}

impl Database {
    /// Open (creating if needed) the database at `path`, configure WAL + pragmas
    /// on every connection, and run any pending migrations. Uses
    /// [`DEFAULT_READ_POOL_SIZE`] read connections.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] if the parent directory cannot be created, a
    /// connection cannot be opened/configured, or a migration fails.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StorageError> {
        Self::open_with_pool_size(path, DEFAULT_READ_POOL_SIZE)
    }

    /// Like [`Database::open`] with an explicit read-pool size (a size of 0 is
    /// clamped to 1, since a pool needs at least one connection).
    ///
    /// # Errors
    ///
    /// As [`Database::open`].
    pub fn open_with_pool_size(
        path: impl AsRef<Path>,
        read_pool_size: u32,
    ) -> Result<Self, StorageError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }

        // Writer first: it establishes WAL mode (persistent on the file) and
        // runs migrations before any reader opens.
        let mut writer = Connection::open(&path)?;
        configure_writer(&writer)?;
        run_migrations(&mut writer)?;

        // Readers: a pool of query-only connections; WAL is already set on the
        // file, so readers proceed concurrently without blocking the writer.
        let manager = SqliteConnectionManager::file(&path).with_init(configure_reader);
        let readers = Pool::builder()
            .max_size(read_pool_size.max(1))
            .connection_timeout(BUSY_TIMEOUT)
            .build(manager)?;

        tracing::info!(path = %path.display(), read_pool_size = readers.max_size(), "opened database");

        Ok(Self {
            writer: Arc::new(Mutex::new(writer)),
            readers,
            path,
        })
    }

    /// The database file path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Run a closure with a pooled **read** connection (synchronous).
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] if a connection cannot be acquired or the
    /// closure returns an error.
    pub fn with_read<F, T>(&self, f: F) -> Result<T, StorageError>
    where
        F: FnOnce(&Connection) -> rusqlite::Result<T>,
    {
        let conn = self.readers.get()?;
        f(&conn).map_err(StorageError::from)
    }

    /// Run a closure with the single **writer** connection (synchronous).
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] if the writer lock is poisoned or the closure
    /// returns an error.
    pub fn with_write<F, T>(&self, f: F) -> Result<T, StorageError>
    where
        F: FnOnce(&mut Connection) -> rusqlite::Result<T>,
    {
        let mut guard = self.writer.lock().map_err(|_| StorageError::Poisoned)?;
        f(&mut guard).map_err(StorageError::from)
    }

    /// Run a **read** closure on a blocking thread so the async runtime is not
    /// blocked.
    ///
    /// Cancellation note: `spawn_blocking` tasks cannot be aborted, so dropping
    /// the returned future does not interrupt an in-flight query — it runs to
    /// completion on the blocking pool. Queries here are bounded; interrupting
    /// long scans (via SQLite `interrupt()`) is a follow-up for the search path.
    ///
    /// The calling span is carried onto the blocking thread, so spans the
    /// closure opens stay attached to the request trace instead of rooting a
    /// new one.
    ///
    /// # Errors
    ///
    /// As [`Database::with_read`], plus [`StorageError::Task`] if the blocking
    /// task fails to join.
    pub async fn read<F, T>(&self, f: F) -> Result<T, StorageError>
    where
        F: FnOnce(&Connection) -> rusqlite::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let readers = self.readers.clone();
        let span = tracing::Span::current();
        let join = tokio::task::spawn_blocking(move || -> Result<T, StorageError> {
            let _entered = span.enter();
            let conn = readers.get()?;
            f(&conn).map_err(StorageError::from)
        })
        .await;
        match join {
            Ok(result) => result,
            Err(e) => Err(StorageError::Task(e.to_string())),
        }
    }

    /// Run a **write** closure on a blocking thread so the async runtime is not
    /// blocked.
    ///
    /// As with [`Database::read`], the calling span is carried onto the
    /// blocking thread so the closure's spans join the request trace.
    ///
    /// # Errors
    ///
    /// As [`Database::with_write`], plus [`StorageError::Task`] if the blocking
    /// task fails to join.
    pub async fn write<F, T>(&self, f: F) -> Result<T, StorageError>
    where
        F: FnOnce(&mut Connection) -> rusqlite::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let writer = Arc::clone(&self.writer);
        let span = tracing::Span::current();
        let join = tokio::task::spawn_blocking(move || -> Result<T, StorageError> {
            let _entered = span.enter();
            let mut guard = writer.lock().map_err(|_| StorageError::Poisoned)?;
            f(&mut guard).map_err(StorageError::from)
        })
        .await;
        match join {
            Ok(result) => result,
            Err(e) => Err(StorageError::Task(e.to_string())),
        }
    }
}

/// Configure the writer connection: WAL mode + enforcement/durability pragmas.
///
/// WAL is the load-bearing property of this design (reads never block writes),
/// so its establishment is verified rather than assumed — some filesystems
/// silently refuse it.
fn configure_writer(conn: &Connection) -> Result<(), StorageError> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.execute_batch(
        "PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;
         PRAGMA temp_store = MEMORY;",
    )?;
    // Setting journal_mode returns the resulting mode; confirm it actually took.
    let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
    if !mode.eq_ignore_ascii_case("wal") {
        return Err(StorageError::WalUnavailable(mode));
    }
    Ok(())
}

/// Configure a read-pool connection: read-only, with matching pragmas. Runs for
/// every connection the pool creates (the `with_init` hook).
fn configure_reader(conn: &mut Connection) -> rusqlite::Result<()> {
    conn.busy_timeout(BUSY_TIMEOUT)?;
    conn.execute_batch(
        "PRAGMA foreign_keys = ON;
         PRAGMA temp_store = MEMORY;
         PRAGMA query_only = ON;",
    )
}

/// Apply all pending embedded migrations. A failing migration is rolled back by
/// refinery (each migration runs in its own transaction).
fn run_migrations(conn: &mut Connection) -> Result<(), StorageError> {
    let report = embedded::migrations::runner().run(conn)?;
    let applied = report.applied_migrations().len();
    if applied > 0 {
        tracing::info!(applied, "applied database migrations");
    } else {
        tracing::debug!("database schema up to date");
    }
    Ok(())
}

#[cfg(test)]
mod tests;
