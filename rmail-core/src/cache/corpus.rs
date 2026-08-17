//! The corpus version: one monotonic number that changes whenever anything a
//! search could match changes.
//!
//! Kept by SQL triggers (migration V51), not by Rust. See that migration's own
//! comments for which tables bump it and why a trigger rather than a `bump()`
//! call is the only version of this that stays true as new write paths land.
//! This module is only the reader.

use rusqlite::Connection;

/// The corpus version and when it last moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CorpusStamp {
    /// Monotonically increasing; never reused, never decreased.
    pub version: i64,
    /// Unix seconds at the last bump.
    pub changed_at: i64,
}

impl CorpusStamp {
    /// Whether the corpus moved within the last `window_secs` seconds — the
    /// "freshly-synced mail bypasses the result cache" condition.
    ///
    /// `window_secs == 0` is a real off switch, not a degenerate window: with
    /// it, nothing is ever fresh and the bypass never fires. That matters
    /// because a test (or an operator) that wants to observe cache *hits* at
    /// all needs a way to say "stop bypassing," and the alternative — waiting
    /// out a wall-clock window — would make the hit path untestable.
    ///
    /// A clock that has gone backwards (`now < changed_at`, which an NTP step
    /// or a restored database can produce) counts as fresh. Bypassing the
    /// cache is the conservative reading of "I do not know how old this is."
    #[must_use]
    pub fn is_fresh(&self, now: i64, window_secs: u32) -> bool {
        if window_secs == 0 {
            return false;
        }
        let age = now.saturating_sub(self.changed_at);
        age < i64::from(window_secs)
    }
}

/// Read the current stamp.
///
/// # Errors
///
/// Propagates any `rusqlite` error, including
/// [`rusqlite::Error::QueryReturnedNoRows`] if the singleton row is missing.
///
/// That last case is deliberately an error rather than a default. A missing
/// row would otherwise read as version `0` forever, which is not "unknown" —
/// it is a *constant*, and a result cache keyed on a constant is a cache that
/// never invalidates. Every caller in this module treats an error here as
/// "do not use the cache at all," so the failure mode is a slow search rather
/// than a wrong one.
pub fn read(conn: &Connection) -> rusqlite::Result<CorpusStamp> {
    conn.query_row(
        "SELECT version, changed_at FROM corpus_version WHERE id = 0",
        [],
        |row| {
            Ok(CorpusStamp {
                version: row.get(0)?,
                changed_at: row.get(1)?,
            })
        },
    )
}
