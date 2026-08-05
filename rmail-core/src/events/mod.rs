//! The durable event log and its in-process fan-out.
//!
//! Everything downstream of sync — indexing, AI enrichment, rules, the gRPC
//! event stream — is driven by events rather than by re-reading the mailbox.
//! That only works if the log is durable and gapless, because the promise a
//! subscriber depends on is not "you will get events" but *"you will not miss
//! one"*.
//!
//! # Durable first, then broadcast
//!
//! [`EventLog::append`] commits to SQLite before it publishes to the in-process
//! channel, never the other way round. A broadcast that outran the commit would
//! let a subscriber act on an event that a crash then erased — and since the
//! subscriber has already advanced its cursor past it, nothing would ever
//! replay it. Committing first makes the durable log the single source of truth
//! and the channel a pure latency optimization: drop every subscriber and the
//! state is still correct.
//!
//! # Two ways to read, one cursor
//!
//! - [`EventLog::subscribe`] is the live tail — a `tokio::sync::broadcast`
//!   receiver, bounded, and lossy under lag *by design*.
//! - [`EventLog::since`] is the durable read — everything after a cursor.
//!
//! A subscriber that falls behind the channel does not lose data; it loses its
//! *place*, and recovers it by going back to [`EventLog::since`] with the last
//! seq it actually processed. That is the whole reason `seq` is on the event
//! rather than implied by arrival order.
//!
//! # The gap a client must be told about
//!
//! Retention deletes from the bottom of the log, so a cursor can fall off the
//! end. Silently returning "no events" there would be a lie indistinguishable
//! from a quiet mailbox, and the client would carry on believing it was current
//! forever. Instead [`EventLog::since`] returns [`Error::OutOfRange`] carrying
//! [`crate::error::OLDEST_SEQ_KEY`] — the oldest position still held — so the
//! client knows both that it missed something and exactly where to resync from.
//!
//! # Why `seq` is never reused
//!
//! `events.seq` is `AUTOINCREMENT`, so its high-water mark lives in
//! `sqlite_sequence` rather than being derived from the rows present. That
//! matters because retention empties this table routinely — a mailbox quieter
//! than the age window has every row swept — and a plain rowid would restart at
//! 1, handing a subscriber at cursor 500 a "you are current" answer while 500
//! fresh events sat below it, forever.
//!
//! Combined with bottom-up pruning, the live range is always contiguous: a
//! cursor is inside it, or it is provably older than everything retained. There
//! is no third case where a cursor points into a hole.
//!
//! # Reads are atomic; publishes are commit-ordered
//!
//! Every read takes its bounds *and* its page inside one transaction. Split
//! across two, a prune landing in between would let the gap check pass against
//! a floor that no longer exists and the page skip everything just deleted.
//!
//! Publishing happens inside the write transaction's critical section, after
//! the commit. Two concurrent appends that commit as `[1,2]` then `[3,4]` must
//! not publish as `3,4,1,2`: a subscriber tracking the highest seq it has seen
//! would set its cursor to 4 and then be unable to reach 1 and 2 from either
//! path.

use std::time::Duration;

use rusqlite::{Connection, OptionalExtension};
use tokio::sync::broadcast;

use crate::error::Error;
use crate::storage::Database;

/// How many events the in-process channel buffers per subscriber before the
/// slowest one starts losing its place.
///
/// Lag is recoverable — a lagging subscriber re-reads from [`EventLog::since`]
/// — so this trades memory against how often that costs a query, not against
/// correctness. Large enough that a subscriber doing real work per event (an
/// indexer, an AI enqueue) does not thrash; small enough that a wedged one
/// cannot pin unbounded memory.
pub const DEFAULT_CHANNEL_CAPACITY: usize = 1024;

/// Default retention: rows.
pub const DEFAULT_RETENTION_ROWS: i64 = 1_000_000;

/// Default retention: age.
pub const DEFAULT_RETENTION_DAYS: i64 = 7;

/// The largest serialized payload an event may carry.
///
/// The broadcast channel buffers [`DEFAULT_CHANNEL_CAPACITY`] events per
/// subscriber, so an unbounded payload makes that buffer unbounded too. Event
/// payloads are summaries and identifiers, not message bodies; anything near
/// this is a bug upstream.
pub const MAX_PAYLOAD_BYTES: usize = 64 * 1024;

/// The largest page [`EventLog::since`] will return in one call.
///
/// A resuming client that was away for a week must not be answered with a
/// million rows in one allocation; it pages, using the last seq it received.
pub const MAX_PAGE: i64 = 1000;

/// What kind of thing happened.
///
/// The wire strings are a contract — they are what is stored in the log and
/// what a client branches on — so they are spelled out rather than derived.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum EventKind {
    /// A message arrived.
    NewMail,
    /// A message's flag set changed.
    FlagChanged,
    /// A message moved between folders.
    Moved,
    /// A message was expunged.
    Deleted,
    /// A folder's sync state advanced (progress, completion, failure).
    SyncState,
    /// An outbound send finished.
    SendResult,
    /// A rule matched and acted.
    RuleFired,
    /// An AI summary became available.
    AiSummary,
}

impl EventKind {
    /// Every kind, for exhaustive handling and tests.
    pub const ALL: [Self; 8] = [
        Self::NewMail,
        Self::FlagChanged,
        Self::Moved,
        Self::Deleted,
        Self::SyncState,
        Self::SendResult,
        Self::RuleFired,
        Self::AiSummary,
    ];

    /// The stable wire string stored in `events.kind`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NewMail => "NEW_MAIL",
            Self::FlagChanged => "FLAG_CHANGED",
            Self::Moved => "MOVED",
            Self::Deleted => "DELETED",
            Self::SyncState => "SYNC_STATE",
            Self::SendResult => "SEND_RESULT",
            Self::RuleFired => "RULE_FIRED",
            Self::AiSummary => "AI_SUMMARY",
        }
    }

    /// Parse a wire string back into a kind.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] for a string no version of this code ever wrote —
    /// a log written by a *newer* build, which is a deployment problem, not a
    /// client one.
    pub fn parse(value: &str) -> Result<Self, Error> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str() == value)
            .ok_or_else(|| Error::internal(format!("unknown event kind in log: {value}")))
    }
}

/// An event to append.
#[derive(Debug, Clone, Default)]
pub struct NewEvent {
    /// What happened.
    pub kind: Option<EventKind>,
    /// Account scope, if any.
    pub account_id: Option<i64>,
    /// Mailbox scope, if any.
    pub mailbox_id: Option<i64>,
    /// Message scope, if any.
    pub message_id: Option<i64>,
    /// Kind-specific detail.
    pub payload: serde_json::Value,
}

impl NewEvent {
    /// An event of `kind` with no scope and an empty payload.
    #[must_use]
    pub fn new(kind: EventKind) -> Self {
        Self {
            kind: Some(kind),
            payload: serde_json::Value::Null,
            ..Default::default()
        }
    }

    /// Scope this event to an account.
    #[must_use]
    pub fn account(mut self, account_id: i64) -> Self {
        self.account_id = Some(account_id);
        self
    }

    /// Scope this event to a mailbox.
    #[must_use]
    pub fn mailbox(mut self, mailbox_id: i64) -> Self {
        self.mailbox_id = Some(mailbox_id);
        self
    }

    /// Scope this event to a message.
    #[must_use]
    pub fn message(mut self, message_id: i64) -> Self {
        self.message_id = Some(message_id);
        self
    }

    /// Attach a payload.
    #[must_use]
    pub fn payload(mut self, payload: serde_json::Value) -> Self {
        self.payload = payload;
        self
    }
}

/// One page of a resumable read.
///
/// [`Self::next_seq`] is the cursor to pass back, and it advances even when
/// nothing in the page matched a filter. Without that, a filtered subscription
/// on a quiet account could never hold a cursor inside the retention window: it
/// would go stale, resync, receive nothing, go stale again, and loop forever
/// having missed nothing at all.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Page {
    /// Matching events, oldest first.
    pub events: Vec<Event>,
    /// The cursor to resume from — the highest position this read *scanned*,
    /// not the highest it returned.
    pub next_seq: i64,
}

/// A live subscription paired with the durable backlog behind it.
///
/// See [`EventLog::catch_up`] for why the two must be taken together.
#[derive(Debug)]
pub struct Catchup {
    /// Everything durable after the cursor, oldest first.
    pub backlog: Vec<Event>,
    /// The cursor the backlog reached. Live events at or below this are
    /// duplicates of the backlog and must be discarded.
    pub next_seq: i64,
    /// The live tail.
    pub live: broadcast::Receiver<Event>,
}

/// A persisted event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// Monotonic position. The cursor a subscriber resumes from.
    pub seq: i64,
    /// What happened.
    pub kind: EventKind,
    /// Account scope, if any.
    pub account_id: Option<i64>,
    /// Mailbox scope, if any.
    pub mailbox_id: Option<i64>,
    /// Message scope, if any.
    pub message_id: Option<i64>,
    /// When it happened (unix seconds).
    pub at: i64,
    /// Kind-specific detail.
    pub payload: serde_json::Value,
}

/// A row this build cannot interpret is a corrupt log, not a bad request —
/// route it onto the same path as any other storage fault.
fn corrupt(column: &str, error: &dyn std::fmt::Display) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::other(format!(
            "corrupt event {column}: {error}"
        ))),
    )
}

impl Event {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let kind: String = row.get("kind")?;
        let payload: String = row.get("payload")?;
        Ok(Self {
            seq: row.get("seq")?,
            // A row this code cannot parse is a corrupt log, not a bad request.
            // Surfacing it as a rusqlite error keeps it on the same path as any
            // other storage fault rather than inventing a second one.
            kind: EventKind::parse(&kind).map_err(|e| corrupt("kind", &e))?,
            account_id: row.get("account_id")?,
            mailbox_id: row.get("mailbox_id")?,
            message_id: row.get("message_id")?,
            at: row.get("at")?,
            // Same reasoning as `kind` above: a payload this code cannot parse
            // is a corrupt log. Silently substituting `Null` would hand a
            // consumer an event whose detail had vanished, which is worse than
            // failing the read.
            payload: serde_json::from_str(&payload).map_err(|e| corrupt("payload", &e))?,
        })
    }
}

/// How long the log keeps events.
///
/// Both limits apply; whichever bites first wins. Rows bound disk, age bounds
/// how far back a client may resume — and a client that has been away longer
/// than this is told so rather than silently handed a truncated history.
#[derive(Debug, Clone, Copy)]
pub struct Retention {
    /// Maximum events kept. `None` for no row limit.
    pub max_rows: Option<i64>,
    /// Maximum age kept. `None` for no age limit.
    pub max_age: Option<Duration>,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            max_rows: Some(DEFAULT_RETENTION_ROWS),
            max_age: Some(Duration::from_secs(
                DEFAULT_RETENTION_DAYS as u64 * 24 * 60 * 60,
            )),
        }
    }
}

impl Retention {
    /// The largest number of rows one prune pass deletes at a time.
    ///
    /// A first prune over a million-row backlog would otherwise hold the single
    /// writer connection for the whole delete, stalling every append behind it.
    const PRUNE_CHUNK: i64 = 10_000;

    /// Keep everything. Useful for tests and short-lived processes.
    #[must_use]
    pub fn unlimited() -> Self {
        Self {
            max_rows: None,
            max_age: None,
        }
    }
}

/// The durable log plus its in-process fan-out.
///
/// Cheap to clone: every clone shares one database handle and one broadcast
/// channel, so a subsystem can hold its own without threading a reference
/// through every call.
#[derive(Debug, Clone)]
pub struct EventLog {
    db: Database,
    tx: broadcast::Sender<Event>,
    retention: Retention,
}

impl EventLog {
    /// Open a log over `db` with the given retention.
    #[must_use]
    pub fn new(db: Database, retention: Retention) -> Self {
        Self::with_capacity(db, retention, DEFAULT_CHANNEL_CAPACITY)
    }

    /// Open a log with an explicit channel capacity.
    #[must_use]
    pub fn with_capacity(db: Database, retention: Retention, capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity.max(1));
        Self { db, tx, retention }
    }

    /// Subscribe to the live tail.
    ///
    /// The receiver is lossy under lag by design — see the module docs. A
    /// subscriber that sees `RecvError::Lagged` has not lost data, only its
    /// place, and recovers with [`Self::since`].
    #[must_use]
    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.tx.subscribe()
    }

    /// How many subscribers are currently attached.
    #[must_use]
    pub fn subscriber_count(&self) -> usize {
        self.tx.receiver_count()
    }

    /// Append one event, then publish it.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] if the event has no kind, or a mapped storage
    /// error.
    #[tracing::instrument(skip(self, event), fields(kind, seq))]
    pub async fn append(&self, event: NewEvent) -> Result<Event, Error> {
        let mut appended = self.append_all(vec![event]).await?;
        appended
            .pop()
            .ok_or_else(|| Error::internal("append produced no event"))
    }

    /// Append several events in one transaction, then publish them in order.
    ///
    /// Batching matters where it is used: a sync window that produced fifty new
    /// messages should cost one commit, not fifty, and the events should become
    /// visible together or not at all.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] if any event has no kind or carries a payload
    /// larger than [`MAX_PAYLOAD_BYTES`], or a mapped storage error. Nothing is
    /// published if the commit fails.
    #[tracing::instrument(skip(self, events), fields(count = events.len(), first_seq, last_seq))]
    pub async fn append_all(&self, events: Vec<NewEvent>) -> Result<Vec<Event>, Error> {
        if events.is_empty() {
            return Ok(Vec::new());
        }
        let mut encoded = Vec::with_capacity(events.len());
        for event in events {
            let Some(kind) = event.kind else {
                return Err(Error::invalid_argument("event has no kind"));
            };
            // Encode before the transaction so a bad payload is rejected
            // without having taken the writer lock, and so the size cap is
            // enforced on what actually goes to disk and into the broadcast
            // channel — 1024 buffered slots of unbounded JSON is unbounded.
            let payload = serde_json::to_string(&event.payload).map_err(|e| {
                Error::invalid_argument(format!("event payload is not serializable: {e}"))
            })?;
            if payload.len() > MAX_PAYLOAD_BYTES {
                return Err(Error::invalid_argument(format!(
                    "event payload is {} bytes, over the {MAX_PAYLOAD_BYTES}-byte limit",
                    payload.len()
                )));
            }
            encoded.push((kind, event, payload));
        }

        let publish = self.tx.clone();
        let appended = self
            .db
            .write(move |conn| {
                let tx = conn.transaction()?;
                let mut out = Vec::with_capacity(encoded.len());
                {
                    let mut stmt = tx.prepare(
                        "INSERT INTO events (kind, account_id, mailbox_id, message_id, payload)
                         VALUES (?1, ?2, ?3, ?4, ?5)
                         RETURNING seq, kind, account_id, mailbox_id, message_id, at, payload",
                    )?;
                    for (kind, event, payload) in encoded {
                        let row = stmt.query_row(
                            rusqlite::params![
                                kind.as_str(),
                                event.account_id,
                                event.mailbox_id,
                                event.message_id,
                                payload,
                            ],
                            Event::from_row,
                        )?;
                        out.push(row);
                    }
                }
                tx.commit()?;

                // Publish here, still holding the writer connection, so publish
                // order is commit order. Done after the await instead, two
                // concurrent appends committing [1,2] then [3,4] could publish
                // 3,4,1,2 — and a subscriber tracking the highest seq it has
                // seen would set its cursor to 4 and never reach 1 or 2 from
                // either path. `send` is synchronous and non-blocking, so this
                // costs the writer nothing. An error means nobody is listening,
                // which is not a failure: the log is the source of truth and
                // the channel is only a shortcut to it.
                for event in &out {
                    let _ = publish.send(event.clone());
                }
                Ok(out)
            })
            .await?;

        let span = tracing::Span::current();
        if let Some(first) = appended.first() {
            span.record("first_seq", first.seq);
        }
        if let Some(last) = appended.last() {
            span.record("last_seq", last.seq);
        }
        tracing::debug!(count = appended.len(), "events appended");
        Ok(appended)
    }

    /// Read up to `limit` events after `since_seq`, oldest first.
    ///
    /// Pass `since_seq = 0` to start from the beginning of what is retained.
    /// The caller resumes by passing [`Page::next_seq`] — never the seq of the
    /// last event it *received*, if it did not finish handling it.
    ///
    /// # Errors
    ///
    /// [`Error::OutOfRange`] — carrying [`crate::error::OLDEST_SEQ_KEY`] and
    /// [`crate::error::RESUME_FROM_KEY`] — when `since_seq` is older than
    /// everything retained, so the client learns both that it missed events and
    /// exactly what cursor to resume with. Otherwise a mapped storage error.
    #[tracing::instrument(skip(self))]
    pub async fn since(&self, since_seq: i64, limit: i64) -> Result<Page, Error> {
        self.read_after(since_seq, limit, None).await
    }

    /// [`Self::since`], restricted to one account.
    ///
    /// # Errors
    ///
    /// As [`Self::since`].
    #[tracing::instrument(skip(self))]
    pub async fn since_for_account(
        &self,
        account_id: i64,
        since_seq: i64,
        limit: i64,
    ) -> Result<Page, Error> {
        self.read_after(since_seq, limit, Some(account_id)).await
    }

    /// Subscribe to the live tail *and* read the durable backlog, with no
    /// window between them where an event can be missed.
    ///
    /// The order matters and is easy to get backwards: subscribing first means
    /// anything committed from this moment on reaches the channel, and reading
    /// the backlog second means everything before it is on disk. Drain-first
    /// would drop whatever committed in between. The two can overlap, so the
    /// caller discards live events whose `seq` is at or below
    /// [`Catchup::next_seq`].
    ///
    /// # Errors
    ///
    /// As [`Self::since`].
    pub async fn catch_up(&self, since_seq: i64, limit: i64) -> Result<Catchup, Error> {
        let live = self.subscribe();
        let page = self.since(since_seq, limit).await?;
        Ok(Catchup {
            backlog: page.events,
            next_seq: page.next_seq,
            live,
        })
    }

    async fn read_after(
        &self,
        since_seq: i64,
        limit: i64,
        account_id: Option<i64>,
    ) -> Result<Page, Error> {
        if since_seq < 0 {
            return Err(Error::invalid_argument("cursor must not be negative"));
        }
        // Zero means "server default", as an unset proto field would. Clamping
        // it to 1 instead would make a paging client crawl one event per round
        // trip forever.
        let page_size = if limit <= 0 {
            MAX_PAGE
        } else {
            limit.min(MAX_PAGE)
        };

        let page = self
            .db
            .read(move |conn| {
                // Bounds and page in one transaction. Split across two, a prune
                // landing in between would let the gap check pass against a
                // floor that no longer exists while the page silently skipped
                // everything just deleted.
                let tx = conn.unchecked_transaction()?;

                let (oldest, latest): (Option<i64>, Option<i64>) =
                    tx.query_row("SELECT MIN(seq), MAX(seq) FROM events", [], |row| {
                        Ok((row.get(0)?, row.get(1)?))
                    })?;

                let gap = check_cursor(since_seq, oldest, latest);
                if let Some(gap) = gap {
                    return Ok(Err(gap));
                }

                // The scan ceiling is what makes a filtered cursor advance even
                // when nothing matched: the client learns how far the log was
                // read, not merely what came back.
                let scanned_to = latest.unwrap_or(0).min(since_seq.saturating_add(page_size));
                let events: Vec<Event> = match account_id {
                    Some(account_id) => {
                        let mut stmt = tx.prepare(
                            "SELECT seq, kind, account_id, mailbox_id, message_id, at, payload
                             FROM events WHERE seq > ?1 AND seq <= ?2 AND account_id = ?3
                             ORDER BY seq",
                        )?;
                        let rows = stmt
                            .query_map(
                                rusqlite::params![since_seq, scanned_to, account_id],
                                Event::from_row,
                            )?
                            .collect::<rusqlite::Result<Vec<Event>>>()?;
                        rows
                    }
                    None => {
                        let mut stmt = tx.prepare(
                            "SELECT seq, kind, account_id, mailbox_id, message_id, at, payload
                             FROM events WHERE seq > ?1 AND seq <= ?2 ORDER BY seq",
                        )?;
                        let rows = stmt
                            .query_map(rusqlite::params![since_seq, scanned_to], Event::from_row)?
                            .collect::<rusqlite::Result<Vec<Event>>>()?;
                        rows
                    }
                };
                Ok(Ok(Page {
                    events,
                    next_seq: scanned_to.max(since_seq),
                }))
            })
            .await?;

        match page {
            Ok(page) => {
                tracing::debug!(
                    returned = page.events.len(),
                    next_seq = page.next_seq,
                    "resume page read"
                );
                Ok(page)
            }
            Err(gap) => Err(gap),
        }
    }

    /// The oldest position still retained, or `None` if the log is empty.
    ///
    /// # Errors
    /// A mapped storage error.
    pub async fn oldest_seq(&self) -> Result<Option<i64>, Error> {
        Ok(self.db.read(|conn| bound(conn, "MIN")).await?)
    }

    /// The newest position, or `None` if the log is empty.
    ///
    /// # Errors
    /// A mapped storage error.
    pub async fn latest_seq(&self) -> Result<Option<i64>, Error> {
        Ok(self.db.read(|conn| bound(conn, "MAX")).await?)
    }

    /// How many events are retained.
    ///
    /// # Errors
    /// A mapped storage error.
    pub async fn len(&self) -> Result<i64, Error> {
        Ok(self
            .db
            .read(|conn| conn.query_row("SELECT count(*) FROM events", [], |row| row.get(0)))
            .await?)
    }

    /// Whether the log holds no events.
    ///
    /// # Errors
    /// A mapped storage error.
    pub async fn is_empty(&self) -> Result<bool, Error> {
        Ok(self.len().await? == 0)
    }

    /// Apply retention, returning how many events were dropped.
    ///
    /// Prunes from the bottom only, which is what keeps the live range
    /// contiguous and makes "your cursor is older than `oldest_seq`" a complete
    /// description of every gap a client can hit.
    ///
    /// # Errors
    /// A mapped storage error.
    #[tracing::instrument(skip(self))]
    pub async fn prune(&self) -> Result<u64, Error> {
        let retention = self.retention;
        let now = chrono::Utc::now().timestamp();
        let mut dropped = 0u64;

        // Chunked: a first prune over a million-row backlog would otherwise
        // hold the single writer connection for the whole delete, stalling
        // every append in the process behind it.
        loop {
            let removed = self
                .db
                .write(move |conn| {
                    let tx = conn.transaction()?;
                    let mut floor: Option<i64> = None;

                    if let Some(max_age) = retention.max_age {
                        let cutoff = now
                            .saturating_sub(i64::try_from(max_age.as_secs()).unwrap_or(i64::MAX));
                        // Resolve the age horizon to a *seq* before deleting.
                        // Deleting on `at` directly assumes `at` rises with
                        // `seq`, and it does not: one backwards clock step (an
                        // NTP correction after boot, a restored VM snapshot)
                        // puts an old timestamp on a new row, and the sweep
                        // punches a hole in the middle of the live range — the
                        // one thing the whole gap contract assumes cannot
                        // happen.
                        let by_age: Option<i64> = tx.query_row(
                            "SELECT MAX(seq) FROM events WHERE at < ?1",
                            [cutoff],
                            |row| row.get(0),
                        )?;
                        floor = max_option(floor, by_age);
                    }

                    match retention.max_rows {
                        // Zero is a real answer — keep nothing — not a synonym
                        // for unlimited. A config typo that silently disabled
                        // retention would grow the log without bound.
                        Some(rows) if rows <= 0 => {
                            floor = max_option(
                                floor,
                                tx.query_row("SELECT MAX(seq) FROM events", [], |row| row.get(0))?,
                            );
                        }
                        Some(rows) => {
                            let by_rows: Option<i64> = tx
                                .query_row(
                                    "SELECT seq FROM events ORDER BY seq DESC LIMIT 1 OFFSET ?1",
                                    [rows],
                                    |row| row.get(0),
                                )
                                .optional()?;
                            floor = max_option(floor, by_rows);
                        }
                        None => {}
                    }

                    let removed = match floor {
                        Some(floor) => tx.execute(
                            "DELETE FROM events WHERE seq IN
                             (SELECT seq FROM events WHERE seq <= ?1 ORDER BY seq LIMIT ?2)",
                            rusqlite::params![floor, Retention::PRUNE_CHUNK],
                        )?,
                        None => 0,
                    };
                    tx.commit()?;
                    Ok(removed)
                })
                .await?;
            dropped += removed as u64;
            if removed < usize::try_from(Retention::PRUNE_CHUNK).unwrap_or(usize::MAX) {
                break;
            }
        }

        if dropped > 0 {
            tracing::info!(dropped, "pruned events past retention");
        }
        Ok(dropped)
    }
}

/// Whether `since_seq` points somewhere the log can still serve, and the error
/// to return if not.
///
/// Split out because it is the whole gap contract in one place: a cursor is
/// current with the floor, inside the live range, or provably behind it.
fn check_cursor(since_seq: i64, oldest: Option<i64>, latest: Option<i64>) -> Option<Error> {
    // Cursor 0 means "everything you have" and can never be behind.
    if since_seq == 0 {
        return None;
    }
    if let Some(oldest) = oldest {
        // Strictly-after semantics: a cursor of `oldest - 1` is exactly current
        // with the floor and has missed nothing. Only `oldest - 2` and below
        // point at something that was pruned.
        if since_seq + 1 < oldest {
            return Some(resume_gap(
                format!(
                    "cursor {since_seq} is past retention; the oldest retained event is {oldest}"
                ),
                oldest,
            ));
        }
    }
    // A cursor beyond the end is a client claiming to have seen events that
    // never existed — usually a database replaced underneath it.
    if since_seq > latest.unwrap_or(0) {
        return Some(match oldest {
            Some(oldest) => resume_gap(
                format!(
                    "cursor {since_seq} is ahead of the log, which ends at {}",
                    latest.unwrap_or(0)
                ),
                oldest,
            ),
            // An empty log has no position to offer. Reporting 0 would be a
            // sentinel dressed as a cursor, and a client cannot tell those
            // apart.
            None => Error::out_of_range(format!(
                "cursor {since_seq} is ahead of the log, which is empty"
            )),
        });
    }
    None
}

/// A resume gap that tells the client both how far back the log goes and the
/// exact cursor to pass back.
///
/// Two keys rather than one because they differ by one and the difference is a
/// silently dropped event: `oldest_seq` is an event id, `resume_from` is the
/// cursor whose strictly-after read begins with it.
fn resume_gap(message: String, oldest: i64) -> Error {
    Error::resume_gap(message, oldest)
}

/// The larger of two optional positions.
fn max_option(a: Option<i64>, b: Option<i64>) -> Option<i64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

/// `MIN(seq)` or `MAX(seq)` over the log.
fn bound(conn: &Connection, aggregate: &str) -> rusqlite::Result<Option<i64>> {
    // `aggregate` is never caller-controlled — it is one of two literals from
    // this module — so there is nothing here to inject.
    conn.query_row(&format!("SELECT {aggregate}(seq) FROM events"), [], |row| {
        row.get(0)
    })
}

#[cfg(test)]
mod tests;
