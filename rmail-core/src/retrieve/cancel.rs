//! Cancellable SQLite reads — the gap `storage::Database::read` itself names
//! as "a follow-up for the search path": *"`spawn_blocking` tasks cannot be
//! aborted, so dropping the returned future does not interrupt an in-flight
//! query — it runs to completion on the blocking pool."* This is that
//! follow-up. Every retriever in this task runs its scan through
//! [`interruptible_read`] rather than [`Database::read`] directly, because
//! prd.md's "query-generation token cancels superseded scans" only means
//! something if a superseded scan actually stops — not merely gets ignored
//! while it keeps consuming a blocking-pool thread and a read-pool
//! connection underneath a caller that has already moved on.
//!
//! # The mechanism: `sqlite3_interrupt`, not future-drop
//!
//! `rusqlite::Connection::get_interrupt_handle` returns an `InterruptHandle`
//! that is `Send + Sync` specifically so another thread can call `.interrupt()`
//! on a connection that is busy running a statement — SQLite's own
//! documented, signal-handler-safe cross-thread cancellation primitive. That
//! makes the shape of this function "acquire a connection, hand its interrupt
//! handle back out before running the caller's closure, then race the
//! closure against the cancellation token" rather than anything that needs to
//! reach inside `rusqlite`'s query execution itself.
//!
//! # Why the watcher is a detached task, not part of this function's own future
//!
//! The obvious first draft races `cancel.cancelled()` against the blocking
//! join *inside* [`interruptible_read`]'s own `async fn` body. That is not
//! enough: it only calls `.interrupt()` if *this function's own future* is
//! still being polled when the token fires, and the entire reason cancel-by-
//! drop exists as a pattern is that a caller stops polling a future instead
//! of awaiting it to a cancelled outcome. A superseded query's whole call
//! stack — including this function's stack frame — can be gone by the time
//! the newer query's token cancellation actually matters. So the piece that
//! calls `.interrupt()` is a `tokio::spawn`ed task, independent of whatever
//! polls (or stops polling) this function's return value: dropping the
//! future this function returns does not stop that task from eventually
//! interrupting the scan once `cancel` fires. It is `.abort()`-ed once the
//! blocking scan finishes on its own, so an ordinary, never-cancelled query
//! does not leak a task parked forever on a token nobody will cancel — but
//! `.abort()` is cleanup, not correctness; see the next section for why.
//!
//! # A stale interrupt must never reach a *recycled* connection
//!
//! `InterruptHandle` stays live for the connection's whole lifetime — it is
//! only invalidated on `close()`, never on a pooled connection simply being
//! checked back in (r2d2 recycles, it does not close). So a watcher that
//! fires `.interrupt()` *after* the connection it holds a handle for has
//! already gone back to the pool does not fail loudly: it silently aborts
//! whatever unrelated query the pool handed that same connection to next.
//! `.abort()` on the watcher is not sufficient to prevent this by itself —
//! two windows survive it: the caller can drop [`interruptible_read`]'s own
//! future (the exact case the previous section designed the detached watcher
//! for) *before* `watcher.abort()` is ever reached, and even on the ordinary
//! path there is a real gap between the blocking closure releasing the
//! connection and this function reaching its own `watcher.abort()` call.
//!
//! [`ARMED`](fn@interruptible_read)'s `armed` flag is what actually closes
//! this: the blocking closure clears it, under a lock, *before* it returns
//! and `with_read` releases the connection — and the watcher checks that
//! same flag *and* calls `.interrupt()` while still holding the lock. A
//! plain `AtomicBool` is not enough for the second half: reading "still
//! armed" and calling `.interrupt()` have to be one atomic step relative to
//! the closure's clear, or a watcher that reads `true` a moment before the
//! closure clears it could still fire after the connection is already back
//! in the pool. Holding the lock across the check makes the two mutually
//! exclusive: either the closure's clear (and therefore its connection
//! release) happens first and the watcher observes `false` and does nothing,
//! or the watcher's interrupt happens first — while the closure, blocked on
//! the same lock, has not yet returned and therefore has not yet released
//! the connection. `.interrupt()` on an idle connection (nothing running) is
//! a documented no-op, so a watcher that "wins" after `f` already finished
//! costs nothing beyond a wasted call.
//!
//! # Interruption is best-effort, and that is inherent, not a shortcut
//!
//! SQLite's own docs are explicit: `sqlite3_interrupt()` called while no
//! statement is running is a no-op with "no effect on SQL statements that are
//! started after `sqlite3_interrupt()` returns" — there is no way to "pre-arm"
//! a connection. A cancellation that lands in the narrow window before the
//! closure's first `prepare`/`step` call therefore does not stop that
//! particular scan from running to completion; it stops the *next* one, and
//! every scan long enough to matter for latency is also long enough to still
//! be executing somewhere inside that window. This is the same race every
//! cooperative-cancellation scheme has (a `CancellationToken` checked between
//! iterations has the identical gap for whatever work is already mid-iteration)
//! — `interruptible_read` does not pretend otherwise.

use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use tokio_util::sync::CancellationToken;

use crate::storage::{Database, StorageError};

/// Run `f` against a pooled read connection on a blocking thread, honoring
/// `cancel`.
///
/// Returns `Ok(None)` when the scan was cancelled — either `cancel` was
/// already fired before this call even acquired a connection, or the
/// in-flight statement was interrupted mid-scan. A caller should treat this
/// exactly like "no candidates from this source", never like an error: a
/// cancelled read means a newer query superseded this one, which is normal
/// operation, not a fault. Returns `Ok(Some(value))` when `f` completed,
/// including the race where completion and cancellation land at effectively
/// the same instant and completion wins. Every other `rusqlite` failure
/// still propagates as `Err`.
///
/// Returns [`StorageError`] rather than [`crate::error::Error`] — the same
/// choice [`Database::read`] itself makes — so a caller that needs a
/// domain-specific mapping (`retrieve::prefix`'s FTS5 `MATCH` syntax errors
/// becoming `InvalidArgument` rather than `Internal`, exactly like
/// `retrieve::lexical`'s own [`crate::index::fts::malformed_query`]) can
/// still apply it with `.map_err(...)`; folding every error into one fixed
/// [`crate::error::Error`] variant here would take that choice away.
///
/// # Errors
///
/// A mapped storage error for anything other than a cancelled/interrupted
/// scan (a malformed statement, a pool/connection failure, ...).
pub(crate) async fn interruptible_read<F, T>(
    db: &Database,
    cancel: &CancellationToken,
    f: F,
) -> Result<Option<T>, StorageError>
where
    F: FnOnce(&Connection) -> rusqlite::Result<T> + Send + 'static,
    T: Send + 'static,
{
    // Already superseded before this retriever even started: skip the
    // connection-pool round trip entirely rather than opening a scan only to
    // interrupt it a moment later.
    if cancel.is_cancelled() {
        return Ok(None);
    }

    let (handle_tx, handle_rx) = tokio::sync::oneshot::channel();
    // See the module docs ("A stale interrupt must never reach a *recycled*
    // connection"): cleared by the blocking closure before it releases the
    // connection, checked by the watcher under the same lock it calls
    // `.interrupt()` under, so the two can never straddle a connection
    // hand-off to a different caller.
    let armed = Arc::new(Mutex::new(true));
    let armed_in_closure = Arc::clone(&armed);
    let blocking_db = db.clone();
    // Carried onto the blocking thread, same as `Database::read` does: any
    // span `f` opens should join the request trace rather than root a new
    // one.
    let span = tracing::Span::current();
    let join = tokio::task::spawn_blocking(move || {
        let _entered = span.enter();
        blocking_db.with_read(|conn| {
            // Sent before `f` runs: `f` may run for the entire duration of
            // the scan, and the watcher below needs the handle the moment
            // there is a connection to interrupt, not after `f` returns one.
            let _ = handle_tx.send(conn.get_interrupt_handle());
            let result = f(conn);
            // Disarm before this closure returns and `with_read` releases
            // the connection — never poisoned in practice (nothing inside
            // this critical section can panic), and if it somehow were,
            // treating it as "already disarmed" is the fail-safe direction.
            if let Ok(mut armed) = armed_in_closure.lock() {
                *armed = false;
            }
            result
        })
    });

    // See the module docs: detached so cancel-by-drop of *this* function's
    // future still lands the interrupt, aborted below once the scan is over
    // either way so an uncancelled query does not leak a watcher parked on a
    // token that will never fire.
    let watcher = {
        let cancel = cancel.clone();
        let armed = Arc::clone(&armed);
        tokio::spawn(async move {
            let Ok(handle) = handle_rx.await else {
                // The blocking task ended (panicked, or the pool itself
                // failed) before it ever reached the point of sending a
                // handle — nothing to interrupt.
                return;
            };
            cancel.cancelled().await;
            // Locked across the check *and* the call: see the module docs
            // for why an `AtomicBool` read-then-call is not equivalent.
            if let Ok(armed) = armed.lock() {
                if *armed {
                    handle.interrupt();
                }
            }
        })
    };

    let outcome = match join.await {
        Ok(Ok(value)) => Ok(Some(value)),
        Ok(Err(StorageError::Sqlite(rusqlite::Error::SqliteFailure(inner, _))))
            if inner.code == rusqlite::ErrorCode::OperationInterrupted =>
        {
            Ok(None)
        }
        Ok(Err(other)) => Err(other),
        Err(join_err) => Err(StorageError::Task(join_err.to_string())),
    };
    watcher.abort();
    outcome
}

#[cfg(test)]
mod tests;
