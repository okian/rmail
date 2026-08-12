//! Typed SQL over `search_log`/`search_impression`/`search_action`
//! (migration V34 -- renumbered at merge; never referenced from code).
//!
//! Nothing here consults `search.learning`. The opt-out lives one layer up in
//! [`super::FeedbackStore`], deliberately: a repository function that
//! silently did nothing depending on configuration is a function whose tests
//! prove nothing, and the guarantee this task owes ("no rows are written at
//! all") is much easier to hold when exactly one place decides whether to
//! call down here.

use rusqlite::{Connection, OptionalExtension, Transaction};

/// One `search_log` row, with the feature-vector-independent parts already
/// derived.
pub(crate) struct LogRow {
    pub(crate) query_id: i64,
    pub(crate) account_id: Option<i64>,
    pub(crate) raw_query: String,
    pub(crate) norm_hash: Vec<u8>,
    pub(crate) intent: &'static str,
    pub(crate) issued_at: i64,
    pub(crate) result_count: i64,
}

/// One `search_impression` row, with its feature vector already serialized —
/// encoding happens before the transaction opens so a serialization failure
/// cannot leave a half-written query behind.
pub(crate) struct ImpressionRow {
    pub(crate) message_id: i64,
    pub(crate) position: i64,
    pub(crate) features: Vec<u8>,
    pub(crate) l1_score: f64,
    pub(crate) l2_score: Option<f64>,
}

/// One `search_action` row.
pub(crate) struct ActionRow {
    pub(crate) message_id: i64,
    pub(crate) action: &'static str,
    pub(crate) dwell_ms: Option<i64>,
    pub(crate) at: i64,
}

/// Insert a query and its impressions atomically, returning how many
/// impressions landed.
///
/// One transaction, not two statements: a `search_log` row without its
/// impressions is a record of what the user searched for and nothing the
/// trainer can use — which is the one thing this table is not supposed to
/// become. Rolling the pair together means the log either holds a complete,
/// replayable query or holds nothing about it.
pub(crate) fn insert_query(
    conn: &mut Connection,
    log: &LogRow,
    impressions: &[ImpressionRow],
) -> rusqlite::Result<usize> {
    let tx = conn.transaction()?;
    tx.execute(
        "INSERT INTO search_log
             (query_id, account_id, raw_query, norm_hash, intent, issued_at, result_count)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            log.query_id,
            log.account_id,
            log.raw_query,
            log.norm_hash,
            log.intent,
            log.issued_at,
            log.result_count,
        ],
    )?;

    let written = insert_impressions(&tx, log.query_id, impressions)?;
    tx.commit()?;
    Ok(written)
}

fn insert_impressions(
    tx: &Transaction<'_>,
    query_id: i64,
    impressions: &[ImpressionRow],
) -> rusqlite::Result<usize> {
    // Prepared once and reused across the batch: a page's worth of
    // impressions is up to `search.default_limit` rows, and re-preparing per
    // row is the difference between one parse and twenty-five.
    let mut stmt = tx.prepare(
        "INSERT INTO search_impression
             (query_id, message_id, position, features, l1_score, l2_score)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    let mut written = 0usize;
    for row in impressions {
        written += stmt.execute(rusqlite::params![
            query_id,
            row.message_id,
            row.position,
            row.features,
            row.l1_score,
            row.l2_score,
        ])?;
    }
    Ok(written)
}

/// What [`insert_actions`] found, so the caller can map each case to the
/// right domain error instead of inferring it from a row count.
pub(crate) enum ActionOutcome {
    /// The batch landed; this many rows were written.
    Written(usize),
    /// No `search_log` row with that id — never minted, or already dropped by
    /// retention.
    UnknownQuery,
    /// An action named a message this query never showed, carrying the
    /// offending id.
    NotShown(i64),
}

/// Insert actions for an existing query.
///
/// The whole batch is one transaction so a client reporting "opened result 3,
/// scrolled past 1 and 2" either contributes all three observations or none —
/// a partial batch would produce a pairwise label claiming the user skipped a
/// result they in fact opened.
///
/// # Why membership is checked, and checked *here*
///
/// An action is only meaningful relative to an impression: the pairwise
/// label task 65 derives is "this result, at this position, beat the ones
/// above it", and a `message_id` that was never on the page has no position
/// and no vector to attribute anything to. Accepting one would write a
/// training label out of thin air.
///
/// That also makes it a capability question, not just a hygiene one.
/// `LogFeedback` sits at `mail.read` (see `rmaild::auth::methods`) precisely
/// because a caller can only talk about a page this daemon already served it;
/// without this check, a read-scoped token could attach arbitrary actions to
/// arbitrary message ids under one of its own real `query_id`s. The check
/// lives inside the same transaction as the inserts so there is no window
/// between "verified" and "written" — a `SELECT` in the caller followed by an
/// `INSERT` here would be exactly the TOCTOU gap the constraint exists to
/// close.
pub(crate) fn insert_actions(
    conn: &mut Connection,
    query_id: i64,
    actions: &[ActionRow],
) -> rusqlite::Result<ActionOutcome> {
    let tx = conn.transaction()?;

    // Checked explicitly rather than inferred from an empty impression set:
    // the two are equivalent today (a `search_log` row is only ever written
    // alongside at least one impression), but "your query id is stale" and
    // "you named a message that query never showed" are different things to
    // tell a client, and coupling them would make the message wrong the first
    // time that invariant changed.
    let known = tx
        .query_row(
            "SELECT 1 FROM search_log WHERE query_id = ?1",
            [query_id],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if !known {
        return Ok(ActionOutcome::UnknownQuery);
    }

    let mut written = 0usize;
    {
        let mut shown =
            tx.prepare("SELECT 1 FROM search_impression WHERE query_id = ?1 AND message_id = ?2")?;
        let mut insert = tx.prepare(
            "INSERT INTO search_action (query_id, message_id, action, dwell_ms, at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        for row in actions {
            let impressed = shown
                .query_row(rusqlite::params![query_id, row.message_id], |_| Ok(()))
                .optional()?
                .is_some();
            if !impressed {
                // Dropping the transaction rolls back whatever this batch had
                // already inserted, which is the atomicity the doc comment
                // above promises.
                return Ok(ActionOutcome::NotShown(row.message_id));
            }
            written += insert.execute(rusqlite::params![
                query_id,
                row.message_id,
                row.action,
                row.dwell_ms,
                row.at,
            ])?;
        }
    }
    tx.commit()?;
    Ok(ActionOutcome::Written(written))
}

/// Delete up to `chunk` `search_log` rows that fall outside either retention
/// bound, returning how many went. Impressions and actions follow via
/// `ON DELETE CASCADE`.
///
/// The caller loops until a short chunk comes back. That converges because
/// both bounds are recomputed against the *surviving* rows on every pass: the
/// doomed set shrinks by exactly what each pass removed.
///
/// # Why the count bound is an `OFFSET`, not a subtraction
///
/// "Everything except the `max_queries` newest" rather than "delete
/// `count - max_queries` rows": the subtraction form has to read a count and
/// then act on it, and a write landing between the two makes it delete the
/// wrong number. The offset form is one statement with no intermediate
/// value to go stale.
///
/// # Why the newest-first ordering breaks ties on `query_id`
///
/// `issued_at` is unix *seconds*, so a burst of searches — which is exactly
/// what an interactive, keystroke-driven search box produces — shares a
/// timestamp. Ordering on `issued_at` alone would leave the tie broken
/// arbitrarily, and a comparison against the boundary row's timestamp would
/// then sweep every row in that second rather than the ones actually past
/// the bound. Carrying `query_id` into the ordering makes the cut a total
/// order over rows, and selecting the doomed ids directly (rather than
/// comparing against the boundary's timestamp) is what keeps a tie from
/// over-deleting.
pub(crate) fn prune_chunk(
    conn: &mut Connection,
    cutoff: i64,
    max_queries: i64,
    chunk: i64,
) -> rusqlite::Result<usize> {
    conn.execute(
        "DELETE FROM search_log WHERE query_id IN (
             SELECT query_id FROM (
                 SELECT query_id FROM search_log WHERE issued_at < ?1
                 UNION
                 SELECT query_id FROM (
                     SELECT query_id FROM search_log
                     ORDER BY issued_at DESC, query_id DESC
                     LIMIT -1 OFFSET ?2
                 )
             )
             LIMIT ?3)",
        rusqlite::params![cutoff, max_queries, chunk],
    )
}

/// How many queries the log holds.
pub(crate) fn count_queries(conn: &Connection) -> rusqlite::Result<i64> {
    conn.query_row("SELECT count(*) FROM search_log", [], |row| row.get(0))
}

/// Whether `err` is a `UNIQUE`/`PRIMARY KEY` constraint failure — here,
/// always "a minted `query_id` collided".
///
/// Matched on the extended result code, never on the message text: the
/// wording is not a contract and differs across SQLite builds, while
/// `SQLITE_CONSTRAINT_PRIMARYKEY` is. (`saved_search::repo`'s own helper
/// makes the identical argument.)
pub(crate) fn is_unique_violation(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(inner, _)
            if inner.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
                || inner.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY
    )
}

// There is deliberately no `is_missing_reference` helper here, unlike
// `saved_search::repo`. `search_action`'s only foreign key is to
// `search_log`, and [`insert_actions`] resolves that case explicitly, inside
// its own transaction, as [`ActionOutcome::UnknownQuery`] — which reaches the
// client as a `NOT_FOUND` naming the stale id rather than as a classified
// constraint code. Keeping a second, unreachable path to the same answer
// would be a branch no test could ever cover.
