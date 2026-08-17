//! The destination registry and the persisted delivery queue — every
//! statement this module runs against `webhook_destinations` and
//! `webhook_deliveries` (V48).
//!
//! Shaped after [`crate::notify::repo`], which solves the same problem for
//! desktop notifications: a claim that marks a row in flight *before*
//! returning it, so an overlapping tick cannot pick the same row up twice,
//! and a lease pushed past one attempt's own timeout, so a process that dies
//! mid-attempt leaves a row that becomes claimable again rather than one that
//! is wedged. The differences from that module are the two this table has
//! that notifications do not: an idempotency key on enqueue (V48's
//! `UNIQUE (destination_id, event_key)`) and a frozen payload.

use rusqlite::OptionalExtension as _;

use crate::credential::CredentialSource;
use crate::error::Error;
use crate::storage::Database;

use super::payload::MessageFacts;
use super::{Delivery, DeliveryState, Destination, NewDestination, Template};

/// The delivery-queue states, spelled as the literals V48's `CHECK` accepts.
const STATE_PENDING: &str = "pending";
const STATE_DELIVERED: &str = "delivered";
const STATE_FAILED: &str = "failed";

/// The columns every `Destination` read selects, in the order
/// [`row_to_destination`] expects them.
const DESTINATION_COLUMNS: &str = "id, name, url, template, events, include_body, enabled, \
     secret_kind, secret_reference, max_attempts";

/// The columns every `Delivery` read selects.
const DELIVERY_COLUMNS: &str = "id, destination_id, event_key, event, message_id, payload, state, \
     attempts, max_attempts, next_attempt_at, last_status, last_error, created_at, delivered_at";

/// Register a destination.
///
/// # Errors
/// [`Error::AlreadyExists`] if the name is taken, [`Error::InvalidArgument`]
/// for a URL this daemon will not POST to (see [`super::validate_url`]), or a
/// mapped storage error.
pub async fn register(db: &Database, new: NewDestination) -> Result<Destination, Error> {
    super::validate_url(&new.url)?;
    if new.name.trim().is_empty() {
        return Err(Error::invalid_argument(
            "a webhook destination needs a name".to_owned(),
        ));
    }
    if matches!(new.secret, CredentialSource::OAuth(_)) {
        return Err(Error::invalid_argument(
            "a webhook signing key is a static shared secret; the oauth credential source has \
             nothing to sign with"
                .to_owned(),
        ));
    }
    let name = new.name.trim().to_owned();
    let url = new.url.clone();
    let template = new.template.as_str().to_owned();
    let events = super::join_events(&new.events);
    let include_body = new.include_body;
    let enabled = new.enabled;
    let secret_kind = new.secret.kind().to_owned();
    let secret_reference = new.secret.reference().map(str::to_owned);
    let max_attempts = new.max_attempts.max(1);

    db.write(move |conn| {
        // The plain INSERT, letting V48's `name TEXT NOT NULL UNIQUE` be the
        // thing that rejects a duplicate. A `SELECT ... then INSERT` would be
        // a check-then-act with a window between the two halves; here the
        // database decides, the same way `enqueue` below leans on
        // `UNIQUE (destination_id, event_key)`.
        conn.execute(
            "INSERT INTO webhook_destinations
               (name, url, template, events, include_body, enabled, secret_kind,
                secret_reference, max_attempts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            rusqlite::params![
                &name,
                &url,
                &template,
                &events,
                i64::from(include_body),
                i64::from(enabled),
                &secret_kind,
                &secret_reference,
                max_attempts,
            ],
        )?;
        let id = conn.last_insert_rowid();
        conn.query_row(
            &format!("SELECT {DESTINATION_COLUMNS} FROM webhook_destinations WHERE id = ?1"),
            [id],
            row_to_destination,
        )
    })
    .await
    .map_err(map_constraint_violation)
}

/// Every registered destination, oldest first.
///
/// # Errors
/// A mapped storage error.
pub async fn list(db: &Database) -> Result<Vec<Destination>, Error> {
    Ok(db
        .read(move |conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {DESTINATION_COLUMNS} FROM webhook_destinations ORDER BY id"
            ))?;
            let rows = stmt.query_map([], row_to_destination)?;
            rows.collect::<Result<Vec<_>, _>>()
        })
        .await?)
}

/// One destination by name.
///
/// # Errors
/// [`Error::NotFound`] if no destination has that name, or a mapped storage
/// error.
pub async fn get_by_name(db: &Database, name: &str) -> Result<Destination, Error> {
    let name = name.to_owned();
    let found = db
        .read(move |conn| {
            conn.query_row(
                &format!("SELECT {DESTINATION_COLUMNS} FROM webhook_destinations WHERE name = ?1"),
                [&name],
                row_to_destination,
            )
            .optional()
        })
        .await?;
    found.ok_or_else(|| Error::not_found("no webhook destination by that name".to_owned()))
}

/// Remove a destination and, by V48's `ON DELETE CASCADE`, its delivery
/// history. Returns whether anything was removed.
///
/// # Errors
/// A mapped storage error.
pub async fn remove(db: &Database, name: &str) -> Result<bool, Error> {
    let name = name.to_owned();
    Ok(db
        .write(move |conn| {
            let changed =
                conn.execute("DELETE FROM webhook_destinations WHERE name = ?1", [&name])?;
            Ok(changed > 0)
        })
        .await?)
}

/// Enqueue one delivery, idempotently.
///
/// Returns the new delivery's id, or `None` when `(destination_id, event_key)`
/// was already enqueued — V48's UNIQUE index makes the *database*, not this
/// process, the thing that decides who was first, so two ticks racing the same
/// event cannot both queue it.
///
/// # Why the body is built by a callback rather than passed in
///
/// A payload has to name the delivery id it belongs to (it is what a receiver
/// dedupes on), and that id does not exist until the row is inserted. Doing
/// this as insert-then-update in two `Database::write` calls would leave a
/// window in which the row is already `pending` with `next_attempt_at` NULL —
/// that is, *claimable* — carrying a placeholder body, because the writer
/// mutex is released between the two calls and a concurrent
/// [`claim_due`] could take it. The receiver would then get a request that
/// says nothing.
///
/// So `build` runs *inside* the same transaction as the insert: it is handed
/// the freshly-allocated id and its result is written before the commit makes
/// the row visible to anybody. `build` is a pure formatting function over
/// facts the caller already fetched — it does no I/O and cannot fail — which
/// is what makes running it under the writer lock acceptable.
///
/// # Errors
/// A mapped storage error.
pub async fn enqueue(
    db: &Database,
    destination_id: i64,
    event_key: &str,
    event: &str,
    message_id: Option<i64>,
    max_attempts: i64,
    build: impl FnOnce(i64) -> String + Send + 'static,
) -> Result<Option<i64>, Error> {
    let event_key = event_key.to_owned();
    let event = event.to_owned();
    Ok(db
        .write(move |conn| {
            let tx = conn.transaction()?;
            let changed = tx.execute(
                "INSERT OR IGNORE INTO webhook_deliveries
                   (destination_id, event_key, event, message_id, payload, state,
                    attempts, max_attempts, next_attempt_at)
                 VALUES (?1, ?2, ?3, ?4, '', 'pending', 0, ?5, NULL)",
                rusqlite::params![
                    destination_id,
                    &event_key,
                    &event,
                    message_id,
                    max_attempts.max(1),
                ],
            )?;
            if changed == 0 {
                // Somebody else already queued this exact event for this exact
                // destination. Nothing was inserted, so nothing needs undoing.
                tx.commit()?;
                return Ok(None);
            }
            let id = tx.last_insert_rowid();
            let payload = build(id);
            tx.execute(
                "UPDATE webhook_deliveries SET payload = ?2 WHERE id = ?1",
                rusqlite::params![id, &payload],
            )?;
            tx.commit()?;
            Ok(Some(id))
        })
        .await?)
}

/// One delivery by `(destination, event_key)` — the idempotency key.
///
/// # Errors
/// A mapped storage error.
pub async fn get_by_event_key(
    db: &Database,
    destination_id: i64,
    event_key: &str,
) -> Result<Option<Delivery>, Error> {
    let event_key = event_key.to_owned();
    Ok(db
        .read(move |conn| {
            conn.query_row(
                &format!(
                    "SELECT {DELIVERY_COLUMNS} FROM webhook_deliveries
                     WHERE destination_id = ?1 AND event_key = ?2"
                ),
                rusqlite::params![destination_id, &event_key],
                row_to_delivery,
            )
            .optional()
        })
        .await?)
}

/// One claimed delivery: the frozen body plus everything the sender needs.
///
/// `Debug` is hand-written for the reason [`Delivery`]'s is, plus one more:
/// this type also carries the destination's full URL, and a Slack
/// incoming-webhook URL is itself a bearer credential. See
/// [`super::log_url`].
#[derive(Clone)]
pub struct ClaimedDelivery {
    /// The delivery row's id — also the `X-Rmail-Delivery` header a receiver
    /// dedupes on.
    pub id: i64,
    /// The destination it goes to, resolved in the same claim query so a tick
    /// claiming twenty deliveries does one query rather than twenty-one (the
    /// same reason `notify::repo::claim_due` joins `accounts`/`messages`).
    pub destination: Destination,
    /// The event wire string, for `X-Rmail-Event`.
    pub event: String,
    /// The exact bytes to POST — frozen at enqueue time, see V48.
    pub payload: String,
    /// Attempts made *including* this one.
    pub attempts: i64,
    /// This delivery's own cap, copied from the destination at enqueue.
    pub max_attempts: i64,
}

impl std::fmt::Debug for ClaimedDelivery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaimedDelivery")
            .field("id", &self.id)
            .field("destination", &self.destination.name)
            .field("url", &super::log_url(&self.destination.url))
            .field("event", &self.event)
            .field("attempts", &self.attempts)
            .field("max_attempts", &self.max_attempts)
            .field("payload", &format_args!("<{} bytes>", self.payload.len()))
            .finish_non_exhaustive()
    }
}

/// Claim up to `limit` pending deliveries due at `now`, marking each as
/// attempted so an overlapping tick cannot claim it again.
///
/// `lease` is how far into the future a claimed row's `next_attempt_at` is
/// pushed: long enough to cover one attempt plus its timeout, so a process
/// that dies mid-attempt leaves a row that becomes claimable again rather than
/// one that is stuck.
///
/// # Errors
/// A mapped storage error.
pub async fn claim_due(
    db: &Database,
    now: i64,
    lease: i64,
    limit: i64,
) -> Result<Vec<ClaimedDelivery>, Error> {
    Ok(db
        .write(move |conn| {
            let tx = conn.transaction()?;
            let ids: Vec<i64> = {
                // `state = 'pending'` inline, not bound, and `ORDER BY id` to
                // match the index's own key — see V48's
                // `idx_webhook_deliveries_due` and the identical discipline in
                // `notify::repo::claim_due`.
                // The `enabled` join is not redundant with the enqueue-time
                // filter: a destination disabled *after* something was queued
                // for it must stop receiving, and the queued rows must stay
                // where they are rather than being sent or discarded. Written
                // as an EXISTS rather than a JOIN so the `state = 'pending'`
                // literal stays in a shape SQLite can prove
                // `idx_webhook_deliveries_due` applies to.
                let mut stmt = tx.prepare(
                    "SELECT id FROM webhook_deliveries
                     WHERE state = 'pending'
                       AND (next_attempt_at IS NULL OR next_attempt_at <= ?1)
                       AND EXISTS (
                             SELECT 1 FROM webhook_destinations w
                              WHERE w.id = webhook_deliveries.destination_id
                                AND w.enabled = 1
                           )
                     ORDER BY id
                     LIMIT ?2",
                )?;
                let rows = stmt.query_map(rusqlite::params![now, limit], |r| r.get(0))?;
                rows.collect::<Result<Vec<i64>, _>>()?
            };
            let mut claimed = Vec::with_capacity(ids.len());
            for id in ids {
                // Conditional on `pending` even though the SELECT just said
                // so: between the two, another connection may have finished
                // it. The UPDATE is the claim; the SELECT is a candidate list.
                let changed = tx.execute(
                    "UPDATE webhook_deliveries
                     SET attempts = attempts + 1, next_attempt_at = ?2
                     WHERE id = ?1 AND state = 'pending'",
                    rusqlite::params![id, now + lease],
                )?;
                if changed == 0 {
                    continue;
                }
                let row = tx
                    .query_row(
                        &format!(
                            "SELECT d.event, d.payload, d.attempts, d.max_attempts,
                                    {}
                             FROM webhook_deliveries d
                             JOIN webhook_destinations w ON w.id = d.destination_id
                             WHERE d.id = ?1",
                            DESTINATION_COLUMNS
                                .split(", ")
                                .map(|c| format!("w.{c}"))
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                        [id],
                        |r| {
                            Ok((
                                r.get::<_, String>(0)?,
                                r.get::<_, String>(1)?,
                                r.get::<_, i64>(2)?,
                                r.get::<_, i64>(3)?,
                                destination_from_offset(r, 4)?,
                            ))
                        },
                    )
                    .optional()?;
                let Some((event, payload, attempts, max_attempts, destination)) = row else {
                    continue;
                };
                claimed.push(ClaimedDelivery {
                    id,
                    destination,
                    event,
                    payload,
                    attempts,
                    max_attempts,
                });
            }
            tx.commit()?;
            Ok(claimed)
        })
        .await?)
}

/// Move a claimed delivery to `delivered`. Returns whether it was still
/// pending — `false` means somebody else finished it and nothing was recorded
/// twice.
///
/// # Errors
/// A mapped storage error.
pub async fn mark_delivered(db: &Database, id: i64, status: u16) -> Result<bool, Error> {
    Ok(db
        .write(move |conn| {
            let changed = conn.execute(
                "UPDATE webhook_deliveries
                 SET state = ?2, next_attempt_at = NULL, last_status = ?3,
                     last_error = NULL, delivered_at = unixepoch()
                 WHERE id = ?1 AND state = 'pending'",
                rusqlite::params![id, STATE_DELIVERED, i64::from(status)],
            )?;
            Ok(changed > 0)
        })
        .await?)
}

/// Move a claimed delivery to the terminal `failed` state.
///
/// # Errors
/// A mapped storage error.
pub async fn mark_failed(
    db: &Database,
    id: i64,
    status: Option<u16>,
    error: &str,
) -> Result<bool, Error> {
    let error = error.to_owned();
    Ok(db
        .write(move |conn| {
            let changed = conn.execute(
                "UPDATE webhook_deliveries
                 SET state = ?2, next_attempt_at = NULL, last_status = ?3, last_error = ?4
                 WHERE id = ?1 AND state = 'pending'",
                rusqlite::params![id, STATE_FAILED, status.map(i64::from), &error],
            )?;
            Ok(changed > 0)
        })
        .await?)
}

/// Return a claimed delivery to `pending`, due at `at` (unix seconds), with
/// the failure that caused the backoff recorded.
///
/// The attempt is never refunded: the endpoint was genuinely touched, which is
/// the difference from `notify::repo::defer`'s `refund` case (a quiet-hours
/// deferral, where the channel was not).
///
/// # Errors
/// A mapped storage error.
pub async fn defer(
    db: &Database,
    id: i64,
    at: i64,
    status: Option<u16>,
    error: &str,
) -> Result<bool, Error> {
    let error = error.to_owned();
    Ok(db
        .write(move |conn| {
            let changed = conn.execute(
                "UPDATE webhook_deliveries
                 SET next_attempt_at = ?2, last_status = ?3, last_error = ?4
                 WHERE id = ?1 AND state = 'pending'",
                rusqlite::params![id, at, status.map(i64::from), &error],
            )?;
            Ok(changed > 0)
        })
        .await?)
}

/// Enable or disable a destination. Returns the updated destination.
///
/// Disabling keeps the row and its delivery history — "stop sending here" and
/// "forget where this pointed" are different operator intents, and only
/// [`remove`] is the second one. Queued deliveries for a disabled destination
/// stay queued and stop being claimed (see [`claim_due`]), so re-enabling
/// resumes rather than replays.
///
/// # Errors
/// [`Error::NotFound`] if no destination has that name, or a mapped storage
/// error.
pub async fn set_enabled(db: &Database, name: &str, enabled: bool) -> Result<Destination, Error> {
    let name_for_update = name.to_owned();
    let changed = db
        .write(move |conn| {
            conn.execute(
                "UPDATE webhook_destinations
                 SET enabled = ?2, updated_at = unixepoch()
                 WHERE name = ?1",
                rusqlite::params![&name_for_update, i64::from(enabled)],
            )
        })
        .await?;
    if changed == 0 {
        return Err(Error::not_found(
            "no webhook destination by that name".to_owned(),
        ));
    }
    get_by_name(db, name).await
}

/// Return a claimed delivery to the queue, due at `at`, giving back the
/// attempt [`claim_due`] charged.
///
/// Only for a delivery the *daemon* abandoned — a shutdown mid-flight. An
/// endpoint that was genuinely touched keeps its attempt, which is the
/// difference from [`defer`] and the same `refund` distinction
/// `notify::repo::defer` draws. `last_error`/`last_status` are deliberately
/// left alone: the last thing that happened *to this endpoint* is still the
/// last thing that happened to it, and overwriting it with "we shut down"
/// would erase the diagnostic an operator is looking for.
///
/// # Errors
/// A mapped storage error.
pub async fn refund(db: &Database, id: i64, at: i64) -> Result<bool, Error> {
    Ok(db
        .write(move |conn| {
            let changed = conn.execute(
                "UPDATE webhook_deliveries
                 SET next_attempt_at = ?2, attempts = MAX(attempts - 1, 0)
                 WHERE id = ?1 AND state = 'pending'",
                rusqlite::params![id, at],
            )?;
            Ok(changed > 0)
        })
        .await?)
}

/// A destination's delivery history, newest first.
///
/// # Errors
/// A mapped storage error.
pub async fn list_deliveries(
    db: &Database,
    destination_id: Option<i64>,
    limit: i64,
) -> Result<Vec<Delivery>, Error> {
    let limit = limit.clamp(1, 1_000);
    Ok(db
        .read(move |conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {DELIVERY_COLUMNS} FROM webhook_deliveries
                 WHERE (?1 IS NULL OR destination_id = ?1)
                 ORDER BY id DESC
                 LIMIT ?2"
            ))?;
            let rows = stmt.query_map(rusqlite::params![destination_id, limit], row_to_delivery)?;
            rows.collect::<Result<Vec<_>, _>>()
        })
        .await?)
}

/// One delivery by id.
///
/// # Errors
/// [`Error::NotFound`] if there is no such delivery, or a mapped storage
/// error.
pub async fn get_delivery(db: &Database, id: i64) -> Result<Delivery, Error> {
    let found = db
        .read(move |conn| {
            conn.query_row(
                &format!("SELECT {DELIVERY_COLUMNS} FROM webhook_deliveries WHERE id = ?1"),
                [id],
                row_to_delivery,
            )
            .optional()
        })
        .await?;
    found.ok_or_else(|| Error::not_found("no such webhook delivery".to_owned()))
}

/// Re-arm a delivery for another attempt: back to `pending`, attempts reset,
/// due immediately.
///
/// This is the only way out of the terminal `failed` state, and it is
/// deliberately operator-driven — see V48's header on why an unbounded
/// automatic retry against a misconfigured URL is an outbound request
/// generator. Replaying an *already delivered* row is allowed and resends the
/// same frozen bytes under the same `X-Rmail-Delivery` id, which is exactly
/// what a receiver that lost a delivery needs and exactly what its own dedupe
/// makes safe.
///
/// # Errors
/// [`Error::NotFound`] if there is no such delivery, or a mapped storage
/// error.
pub async fn replay(db: &Database, id: i64) -> Result<Delivery, Error> {
    let changed = db
        .write(move |conn| {
            conn.execute(
                "UPDATE webhook_deliveries
                 SET state = ?2, attempts = 0, next_attempt_at = NULL,
                     last_error = NULL, delivered_at = NULL
                 WHERE id = ?1",
                rusqlite::params![id, STATE_PENDING],
            )
        })
        .await?;
    if changed == 0 {
        return Err(Error::not_found("no such webhook delivery".to_owned()));
    }
    get_delivery(db, id).await
}

/// The facts one message contributes to a payload.
///
/// `include_body` gates the *query*, not a later filter: with it off, the
/// body is never read out of the database at all, so no edit to the payload
/// builder downstream can put one in a request that was not entitled to it.
///
/// # Errors
/// [`Error::NotFound`] if the message does not exist, or a mapped storage
/// error.
pub async fn facts_for(
    db: &Database,
    message_id: i64,
    include_body: bool,
) -> Result<MessageFacts, Error> {
    let found = db
        .read(move |conn| {
            let base = conn
                .query_row(
                    "SELECT m.subject, m.from_addr, m.from_name, m.message_id, m.date,
                            m.internaldate, a.name, b.name
                     FROM messages m
                     JOIN accounts a ON a.id = m.account_id
                     JOIN mailboxes b ON b.id = m.mailbox_id
                     WHERE m.id = ?1",
                    [message_id],
                    |r| {
                        Ok((
                            r.get::<_, Option<String>>(0)?,
                            r.get::<_, Option<String>>(1)?,
                            r.get::<_, Option<String>>(2)?,
                            r.get::<_, Option<String>>(3)?,
                            r.get::<_, Option<i64>>(4)?,
                            r.get::<_, Option<i64>>(5)?,
                            r.get::<_, String>(6)?,
                            r.get::<_, String>(7)?,
                        ))
                    },
                )
                .optional()?;
            let Some((subject, from_addr, from_name, rfc_id, date, internaldate, account, mailbox)) =
                base
            else {
                return Ok(None);
            };
            let body = if include_body {
                conn.query_row(
                    "SELECT body_text FROM messages WHERE id = ?1",
                    [message_id],
                    |r| r.get::<_, Option<String>>(0),
                )
                .optional()?
                .flatten()
            } else {
                None
            };
            // The most recent stored summary for this message, whichever pass
            // wrote it. No provider call happens here — see `payload`'s own
            // module docs on why enrichment is read, never computed.
            let ai: Option<(Option<String>, Option<String>, Option<String>)> = conn
                .query_row(
                    "SELECT tl_dr, summary, todos FROM ai_summaries
                     WHERE message_id = ?1
                     ORDER BY id DESC
                     LIMIT 1",
                    [message_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            let (tl_dr, summary, todos) = ai.unwrap_or((None, None, None));
            Ok(Some((
                subject,
                from_addr,
                from_name,
                rfc_id,
                date.or(internaldate),
                account,
                mailbox,
                body,
                tl_dr,
                summary,
                todos,
            )))
        })
        .await?;

    let Some((
        subject,
        from_addr,
        from_name,
        rfc_id,
        date,
        account,
        mailbox,
        body,
        tl_dr,
        summary,
        todos,
    )) = found
    else {
        return Err(Error::not_found(format!("no message with id {message_id}")));
    };

    Ok(MessageFacts {
        message_id,
        account,
        mailbox,
        rfc_message_id: rfc_id.filter(|s| !s.is_empty()),
        from: display_from(from_name, from_addr),
        subject: subject.unwrap_or_default(),
        date,
        body,
        // `tl_dr` before `summary`: the two-sentence field prd.md #64 asks
        // for is what triage already writes, and falling through to the
        // longer `summary` only when there is no tl_dr means the trim in
        // `payload::two_sentences` is a bound rather than the usual path.
        summary: tl_dr.or(summary).filter(|s| !s.trim().is_empty()),
        action_items: parse_todos(todos.as_deref()),
    })
}

/// `Name <addr>`, or whichever half exists.
fn display_from(name: Option<String>, addr: Option<String>) -> String {
    match (
        name.filter(|s| !s.trim().is_empty()),
        addr.filter(|s| !s.trim().is_empty()),
    ) {
        (Some(name), Some(addr)) => format!("{name} <{addr}>"),
        (Some(name), None) => name,
        (None, Some(addr)) => addr,
        (None, None) => String::new(),
    }
}

/// `ai_summaries.todos` is written as a JSON array of strings by the AI
/// passes; anything else is treated as no action items rather than as an
/// error, because a payload is not the place to discover that a summary row
/// is malformed.
fn parse_todos(todos: Option<&str>) -> Vec<String> {
    let Some(todos) = todos else {
        return Vec::new();
    };
    serde_json::from_str::<Vec<String>>(todos).unwrap_or_default()
}

/// Map SQLite's own constraint violation onto the domain error a caller
/// should see. Every other storage failure keeps its default mapping.
///
/// The only constraint [`register`] can trip is `name`'s UNIQUE index — every
/// other `CHECK` on the table is already enforced above it in Rust (the URL
/// policy, the non-empty name, the credential kind, the attempt floor), so a
/// violation reaching here means the name is taken.
fn map_constraint_violation(error: crate::storage::StorageError) -> Error {
    if let crate::storage::StorageError::Sqlite(rusqlite::Error::SqliteFailure(e, _)) = &error {
        if e.code == rusqlite::ErrorCode::ConstraintViolation {
            return Error::already_exists(
                "a webhook destination with that name already exists".to_owned(),
            );
        }
    }
    Error::from(error)
}

/// Read a `Destination` from a row whose first column is `id`.
fn row_to_destination(row: &rusqlite::Row<'_>) -> rusqlite::Result<Destination> {
    destination_from_offset(row, 0)
}

/// Read a `Destination` from `offset` columns in, in `DESTINATION_COLUMNS`
/// order — used both on its own and joined behind a delivery's own columns.
fn destination_from_offset(
    row: &rusqlite::Row<'_>,
    offset: usize,
) -> rusqlite::Result<Destination> {
    let secret_kind: String = row.get(offset + 7)?;
    let secret_reference: Option<String> = row.get(offset + 8)?;
    Ok(Destination {
        id: row.get(offset)?,
        name: row.get(offset + 1)?,
        url: row.get(offset + 2)?,
        template: Template::parse(&row.get::<_, String>(offset + 3)?).unwrap_or_default(),
        events: super::split_events(&row.get::<_, String>(offset + 4)?),
        include_body: row.get::<_, i64>(offset + 5)? != 0,
        enabled: row.get::<_, i64>(offset + 6)? != 0,
        // V48's CHECK already restricts `secret_kind` to this module's
        // vocabulary, so a row this cannot parse is a row written by a future
        // version. It degrades to "unsigned" rather than making the whole
        // destination unreadable — the receiver rejects an unsigned request,
        // which is a visible, diagnosable failure, where an unreadable row
        // would silently vanish from `mail webhook list`.
        secret: CredentialSource::from_stored(&secret_kind, secret_reference.as_deref())
            .unwrap_or(CredentialSource::None),
        max_attempts: row.get(offset + 9)?,
    })
}

/// Read a `Delivery` from a row selecting `DELIVERY_COLUMNS`.
fn row_to_delivery(row: &rusqlite::Row<'_>) -> rusqlite::Result<Delivery> {
    let state: String = row.get(6)?;
    Ok(Delivery {
        id: row.get(0)?,
        destination_id: row.get(1)?,
        event_key: row.get(2)?,
        event: row.get(3)?,
        message_id: row.get(4)?,
        payload: row.get(5)?,
        state: DeliveryState::parse(&state).unwrap_or(DeliveryState::Pending),
        attempts: row.get(7)?,
        max_attempts: row.get(8)?,
        next_attempt_at: row.get(9)?,
        last_status: row.get::<_, Option<i64>>(10)?,
        last_error: row.get(11)?,
        created_at: row.get(12)?,
        delivered_at: row.get(13)?,
    })
}
