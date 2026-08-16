//! The durable side of a digest: what was briefed, over which window, from
//! which messages.
//!
//! Every write here goes through one transaction, and the one that matters is
//! [`store`]'s: the `digests` row and its `digest_sources` rows land together
//! or not at all. A briefing whose source list was lost would still render —
//! its markdown already carries `[msg:<id>]` inline — but nothing could then
//! answer "which messages was this built from", which is the question the
//! whole citation discipline exists to make answerable.

use rusqlite::{OptionalExtension, Transaction};

use crate::error::Error;
use crate::storage::Database;

/// A stored briefing, as [`load_window`] returns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredDigest {
    /// `digests.id`.
    pub id: i64,
    /// 0 for "every configured account".
    pub account_id: i64,
    /// Half-open window in unix seconds.
    pub period_start: i64,
    /// Half-open window in unix seconds.
    pub period_end: i64,
    /// When the briefing was written.
    pub generated_at: i64,
    /// The model that wrote it; empty when the window was empty and no call
    /// was made.
    pub model: String,
    /// The rendered markdown.
    pub markdown: String,
    /// Messages this briefing put forward, before the policy gate and the
    /// token budget cut them further. See `V41__digests.sql` on why this is
    /// not the size of the window.
    pub considered: i64,
    /// Messages that entered the prompt.
    pub packed: i64,
    /// Messages the AI policy withheld.
    pub withheld: i64,
    /// Clusters the packed messages were grouped into.
    pub clusters: i64,
    /// Bullets dropped for citing nothing this daemon retrieved.
    pub dropped_uncited: i64,
    /// The sources it was built from, by ascending label.
    pub sources: Vec<StoredSource>,
}

/// One source a stored briefing was built from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredSource {
    /// The 1-based label the prompt used.
    pub label: u32,
    /// `messages.id`.
    pub message_id: i64,
    /// `messages.uid`.
    pub message_uid: i64,
    /// Owning account.
    pub account_id: i64,
    /// Owning folder.
    pub mailbox: String,
    /// Subject, empty when the message has none.
    pub subject: String,
    /// From address, empty when the message has none.
    pub from_addr: String,
    /// `messages.date`, when the row has one.
    pub date: Option<i64>,
    /// Whether a surviving line of the briefing pointed at this source.
    pub cited: bool,
}

/// Everything [`store`] needs to write one briefing.
#[derive(Debug, Clone)]
pub struct NewDigest {
    /// 0 for "every configured account".
    pub account_id: i64,
    /// Half-open window in unix seconds.
    pub period_start: i64,
    /// Half-open window in unix seconds.
    pub period_end: i64,
    /// The cadence that produced it, 0 for an ad-hoc request.
    pub interval_seconds: i64,
    /// The model that wrote it, empty for an empty window.
    pub model: String,
    /// The rendered markdown.
    pub markdown: String,
    /// Messages this briefing put forward, before the policy gate and the
    /// token budget cut them further.
    pub considered: i64,
    /// Messages that entered the prompt.
    pub packed: i64,
    /// Messages the AI policy withheld.
    pub withheld: i64,
    /// Clusters the packed messages were grouped into.
    pub clusters: i64,
    /// Bullets dropped for citing nothing.
    pub dropped_uncited: i64,
    /// The audit-ledger row the call was recorded under, when one was made.
    pub ledger_entry_id: Option<i64>,
    /// The sources, by ascending label.
    pub sources: Vec<StoredSource>,
}

/// The stored briefing for exactly this window, if there is one.
///
/// The lookup the reuse path and the scheduler's "did we already brief this
/// period" check both make. Keyed on the whole window rather than on its
/// start, so a wider ad-hoc briefing that happens to begin at a period
/// boundary is not mistaken for that period's own.
///
/// # Errors
/// A mapped storage error.
pub async fn load_window(
    db: &Database,
    account_id: i64,
    period_start: i64,
    period_end: i64,
) -> Result<Option<StoredDigest>, Error> {
    let row = db
        .read(move |conn| {
            let mut digest = conn
                .query_row(
                    "SELECT id, account_id, period_start, period_end, generated_at, model, \
                            markdown, considered, packed, withheld, clusters, dropped_uncited \
                     FROM digests \
                     WHERE account_id = ?1 AND period_start = ?2 AND period_end = ?3",
                    rusqlite::params![account_id, period_start, period_end],
                    |row| {
                        Ok(StoredDigest {
                            id: row.get(0)?,
                            account_id: row.get(1)?,
                            period_start: row.get(2)?,
                            period_end: row.get(3)?,
                            generated_at: row.get(4)?,
                            model: row.get(5)?,
                            markdown: row.get(6)?,
                            considered: row.get(7)?,
                            packed: row.get(8)?,
                            withheld: row.get(9)?,
                            clusters: row.get(10)?,
                            dropped_uncited: row.get(11)?,
                            sources: Vec::new(),
                        })
                    },
                )
                .optional()?;
            if let Some(digest) = digest.as_mut() {
                digest.sources = read_sources(conn, digest.id)?;
            }
            Ok(digest)
        })
        .await?;
    Ok(row)
}

/// The latest `period_end` any *scheduled* briefing for `account_id` covers
/// that has already elapsed at `now`, or `None` when none has ever been
/// written.
///
/// This is the scheduler's cursor, and both filters are load-bearing.
///
/// **`interval_seconds > 0` — scheduled rows only.** An ad-hoc `mail digest
/// --since 7d` stores a window ending at *now*, which is inside the period in
/// progress. Letting that advance the cursor makes `ceil_to_grid` round past
/// the current period, so the period the timer was about to brief is skipped
/// and never briefed by anything — one CLI invocation silently costing the
/// reader a day. Ad-hoc briefings answer a question the operator asked; they
/// are not the timer's record of its own work.
///
/// **`period_end <= now` — no cursor from the future.** Nothing stops a caller
/// naming a window years ahead (a client sending milliseconds, most likely).
/// Without this bound one such call parks the cursor past every boundary the
/// grid will produce for years, and the scheduled digest stops for good with
/// no error anywhere. Bounding the read means such a row is inert: it exists,
/// it is returned to whoever asked for it, and the timer carries on.
///
/// # Errors
/// A mapped storage error.
pub async fn latest_period_end(
    db: &Database,
    account_id: i64,
    now: i64,
) -> Result<Option<i64>, Error> {
    Ok(db
        .read(move |conn| {
            conn.query_row(
                "SELECT MAX(period_end) FROM digests \
                 WHERE account_id = ?1 AND interval_seconds > 0 AND period_end <= ?2",
                rusqlite::params![account_id, now],
                |row| row.get::<_, Option<i64>>(0),
            )
        })
        .await?)
}

/// Write one briefing and its sources in a single transaction, replacing any
/// briefing already stored for the same window.
///
/// Replacement rather than a second row: `UNIQUE (account_id, period_start,
/// period_end)` is the "one window, one briefing" guarantee (see the
/// migration), and the only caller that reaches this with a window already
/// stored is an explicit `force`. Deleting the old row first — rather than
/// upserting — is what keeps `digest_sources` consistent, since the sources of
/// a regenerated briefing are not necessarily the sources of the old one and
/// `ON DELETE CASCADE` clears them for free.
///
/// # Errors
/// A mapped storage error.
pub async fn store(db: &Database, digest: NewDigest) -> Result<i64, Error> {
    Ok(db
        .write(move |conn| {
            let tx = conn.transaction()?;
            tx.execute(
                "DELETE FROM digests \
                 WHERE account_id = ?1 AND period_start = ?2 AND period_end = ?3",
                rusqlite::params![digest.account_id, digest.period_start, digest.period_end],
            )?;
            tx.execute(
                "INSERT INTO digests (
                     account_id, period_start, period_end, interval_seconds, generated_at,
                     model, markdown, considered, packed, withheld, clusters, dropped_uncited,
                     ledger_entry_id
                 ) VALUES (?1, ?2, ?3, ?4, unixepoch(), ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                rusqlite::params![
                    digest.account_id,
                    digest.period_start,
                    digest.period_end,
                    digest.interval_seconds,
                    digest.model,
                    digest.markdown,
                    digest.considered,
                    digest.packed,
                    digest.withheld,
                    digest.clusters,
                    digest.dropped_uncited,
                    digest.ledger_entry_id,
                ],
            )?;
            let id = tx.last_insert_rowid();
            write_sources(&tx, id, &digest.sources)?;
            tx.commit()?;
            Ok(id)
        })
        .await?)
}

fn write_sources(tx: &Transaction<'_>, id: i64, sources: &[StoredSource]) -> rusqlite::Result<()> {
    let mut stmt = tx.prepare(
        "INSERT INTO digest_sources (
             digest_id, label, message_id, message_uid, account_id, mailbox, subject,
             from_addr, date, cited
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
    )?;
    for source in sources {
        stmt.execute(rusqlite::params![
            id,
            source.label,
            source.message_id,
            source.message_uid,
            source.account_id,
            source.mailbox,
            source.subject,
            source.from_addr,
            source.date,
            i64::from(source.cited),
        ])?;
    }
    Ok(())
}

fn read_sources(conn: &rusqlite::Connection, id: i64) -> rusqlite::Result<Vec<StoredSource>> {
    let mut stmt = conn.prepare(
        "SELECT label, message_id, message_uid, account_id, mailbox, subject, from_addr, \
                date, cited \
         FROM digest_sources WHERE digest_id = ?1 ORDER BY label",
    )?;
    let rows = stmt.query_map([id], |row| {
        Ok(StoredSource {
            label: row.get::<_, i64>(0)?.try_into().unwrap_or(u32::MAX),
            message_id: row.get(1)?,
            message_uid: row.get(2)?,
            account_id: row.get(3)?,
            mailbox: row.get(4)?,
            subject: row.get(5)?,
            from_addr: row.get(6)?,
            date: row.get(7)?,
            cited: row.get::<_, i64>(8)? != 0,
        })
    })?;
    rows.collect()
}
