//! The `notifications` table: every read and write of the notification state
//! machine, in one place.
//!
//! # Every transition is conditional on the state it came from
//!
//! Not one `UPDATE` here says `WHERE id = ?`. They all say
//! `WHERE id = ? AND state = 'pending'`, and they report whether they matched.
//! That is what makes a delivery at-most-once without a lock: two ticks that
//! somehow overlap, or a tick racing a restart, both try the same transition
//! and exactly one of them changes a row. SQLite's write serialization does
//! the rest — the loser sees `0` rows changed and does not deliver.
//!
//! [`claim_due`] leans on the same property in the other direction. It marks
//! rows as *in flight* before returning them (`attempts` incremented,
//! `next_attempt_at` pushed past the attempt's own timeout) rather than
//! selecting and hoping, so a claimed row cannot be claimed again by an
//! overlapping tick even while its delivery is still running. A process that
//! dies mid-delivery leaves the row `pending` with a future
//! `next_attempt_at`, which is exactly right: it will be retried, once, after
//! the timeout it was already granted.
//!
//! # Why the score insert refuses to update
//!
//! [`record_score`] is `INSERT … ON CONFLICT DO NOTHING`. Scoring the same
//! message twice — a reaped lease, a re-enqueued pass, a daemon restarted
//! mid-call — must not re-arm a notification whose decision has already been
//! made and acted on. `UNIQUE (message_id)` plus `DO NOTHING` means the first
//! verdict for a message is the only one that can ever produce a ping.

use rusqlite::OptionalExtension;

use crate::error::Error;
use crate::storage::Database;

use super::score::{NotifyScore, Tier};

/// The `notifications.state` vocabulary. Wire strings, not an enum with a
/// derive, for the reason [`crate::events::EventKind`] spells out: these are
/// stored and matched in SQL, so they are spelled out once and referenced.
pub const STATE_PENDING: &str = "pending";
/// Delivered to the channel. Terminal.
pub const STATE_DELIVERED: &str = "delivered";
/// Deliberately not delivered. Terminal.
pub const STATE_SUPPRESSED: &str = "suppressed";
/// The channel refused it until its attempts ran out. Terminal.
pub const STATE_FAILED: &str = "failed";

/// `notifications.suppressed_reason` when the tier did not clear the
/// account's threshold — the common case, and the one prd.md #62 exists for
/// ("so newsletters never ping").
pub const SUPPRESSED_BELOW_THRESHOLD: &str = "below_threshold";
/// The account (or the whole engine) has notifications switched off.
pub const SUPPRESSED_DISABLED: &str = "notifications_disabled";

/// A pending notification, claimed for one delivery attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingNotification {
    /// `notifications.id`.
    pub id: i64,
    /// The message this is about.
    pub message_id: i64,
    /// The owning account's id.
    pub account_id: i64,
    /// The owning account's name — the key `[[accounts]] notify.threshold`
    /// is written against. Joined here rather than looked up per row, so a
    /// tick that claims twenty notifications does one query, not twenty-one.
    pub account: String,
    /// The scored tier.
    pub tier: Tier,
    /// The model's one-line reason.
    pub reason: String,
    /// The message's subject, if it has one. Read here so a delivery never
    /// has to go back to `messages` — and never retained beyond the
    /// notification body, which `notify.include_subject` governs.
    pub subject: Option<String>,
    /// The message's sender, best-effort display form.
    pub from: Option<String>,
    /// How many attempts (including this one) have been made.
    pub attempts: i64,
}

/// A delivered notification, as `StreamAlerts` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alert {
    /// `notifications.id`, which is also the stream cursor: monotonic,
    /// never reused (SQLite `INTEGER PRIMARY KEY` on a table nothing
    /// renumbers), so a client resumes with `since_id` and cannot miss a row.
    pub id: i64,
    /// The message this alert is about.
    pub message_id: i64,
    /// The owning account.
    pub account: String,
    /// The scored tier.
    pub tier: Tier,
    /// The model's one-line reason.
    pub reason: String,
    /// The message subject, if any.
    pub subject: Option<String>,
    /// The sender, best-effort display form.
    pub from: Option<String>,
    /// When it was delivered, unix seconds.
    pub delivered_at: i64,
}

/// Persist one scoring verdict as a `pending` notification.
///
/// Returns whether a row was actually inserted — `false` means this message
/// already had a decision, which is the dedup this whole table exists for.
///
/// `ledger_entry_id` is `Option` because the column is a real foreign key into
/// `ai_ledger`, and there is exactly one caller that legitimately has no
/// ledger row to point at: a score recorded outside the AI queue (a fixture, a
/// backfill). Every call that actually reached a provider has one — that
/// linkage is the whole reason the column exists — so `None` here is a
/// statement that no provider call happened, not a shortcut around it.
///
/// # Errors
/// A mapped storage error.
pub async fn record_score(
    db: &Database,
    message_id: i64,
    account_id: i64,
    score: &NotifyScore,
    model: &str,
    ledger_entry_id: Option<i64>,
) -> Result<bool, Error> {
    let tier = score.tier.as_str();
    let reason = score.reason.clone();
    let model = model.to_owned();
    Ok(db
        .write(move |conn| {
            let changed = conn.execute(
                "INSERT INTO notifications (
                     message_id, account_id, tier, reason, model, ledger_entry_id,
                     state, attempts, scored_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, unixepoch())
                 ON CONFLICT(message_id) DO NOTHING",
                rusqlite::params![
                    message_id,
                    account_id,
                    tier,
                    reason,
                    model,
                    ledger_entry_id,
                    STATE_PENDING,
                ],
            )?;
            Ok(changed > 0)
        })
        .await?)
}

/// Claim up to `limit` pending notifications that are due at `now`, marking
/// each as attempted so an overlapping tick cannot claim it again.
///
/// `lease` is how far into the future a claimed row's `next_attempt_at` is
/// pushed: long enough to cover one delivery attempt plus its timeout, so a
/// process that dies mid-attempt leaves a row that becomes claimable again
/// rather than one that is stuck or one that is immediately double-delivered.
///
/// # Errors
/// A mapped storage error.
pub async fn claim_due(
    db: &Database,
    now: i64,
    lease: i64,
    limit: i64,
) -> Result<Vec<PendingNotification>, Error> {
    Ok(db
        .write(move |conn| {
            let tx = conn.transaction()?;
            let ids: Vec<i64> = {
                // `state = 'pending'` is written inline, not bound: V40's
                // `idx_notifications_due` is a partial index, and whether one
                // applies is proved from the statement text, so keeping the
                // literal there means this never rests on how a given SQLite
                // version treats a parameter whose value it cannot see. The
                // `ORDER BY id` matches the index's own key, which is what
                // lets the planner satisfy the ordering from the index instead
                // of sorting — and a sort is what made it decline the index
                // (and full-scan a table that grows one permanent row per
                // message, on the single writer connection, every tick) when
                // this index was first keyed on `next_attempt_at`.
                let mut stmt = tx.prepare(
                    "SELECT id FROM notifications
                     WHERE state = 'pending'
                       AND (next_attempt_at IS NULL OR next_attempt_at <= ?1)
                     ORDER BY id
                     LIMIT ?2",
                )?;
                let rows = stmt.query_map(rusqlite::params![now, limit], |r| r.get(0))?;
                rows.collect::<Result<Vec<i64>, _>>()?
            };
            let mut claimed = Vec::with_capacity(ids.len());
            for id in ids {
                // Conditional on `pending` even though the SELECT above just
                // said so: between the two, another connection may have
                // delivered it. The UPDATE is the claim; the SELECT is only
                // a candidate list.
                let changed = tx.execute(
                    "UPDATE notifications
                     SET attempts = attempts + 1, next_attempt_at = ?2
                     WHERE id = ?1 AND state = 'pending'",
                    rusqlite::params![id, now + lease],
                )?;
                if changed == 0 {
                    continue;
                }
                let row = tx
                    .query_row(
                        "SELECT n.id, n.message_id, n.account_id, a.name, n.tier, n.reason,
                                m.subject, m.from_name, m.from_addr, n.attempts
                         FROM notifications n
                         JOIN accounts a ON a.id = n.account_id
                         JOIN messages m ON m.id = n.message_id
                         WHERE n.id = ?1",
                        [id],
                        |r| {
                            Ok((
                                r.get::<_, i64>(0)?,
                                r.get::<_, i64>(1)?,
                                r.get::<_, i64>(2)?,
                                r.get::<_, String>(3)?,
                                r.get::<_, String>(4)?,
                                r.get::<_, String>(5)?,
                                r.get::<_, Option<String>>(6)?,
                                r.get::<_, Option<String>>(7)?,
                                r.get::<_, Option<String>>(8)?,
                                r.get::<_, i64>(9)?,
                            ))
                        },
                    )
                    .optional()?;
                let Some((
                    id,
                    message_id,
                    account_id,
                    account,
                    tier,
                    reason,
                    subject,
                    name,
                    addr,
                    attempts,
                )) = row
                else {
                    continue;
                };
                // A tier this process cannot read back is a row written by a
                // future (or corrupted) version. Skipping it here would leave
                // it claimed-and-forgotten forever; it is terminated below by
                // the caller instead — see `NotifyEngine::tick`.
                let Some(tier) = Tier::parse(&tier) else {
                    tracing::warn!(
                        notification_id = id,
                        tier,
                        "notification row carries an unreadable tier; suppressing it rather than \
                         leaving it pending forever"
                    );
                    tx.execute(
                        "UPDATE notifications
                         SET state = ?2, suppressed_reason = ?3, next_attempt_at = NULL,
                             decided_at = unixepoch()
                         WHERE id = ?1 AND state = ?4",
                        rusqlite::params![id, STATE_SUPPRESSED, "unreadable_tier", STATE_PENDING],
                    )?;
                    continue;
                };
                claimed.push(PendingNotification {
                    id,
                    message_id,
                    account_id,
                    account,
                    tier,
                    reason,
                    subject,
                    from: display_from(name, addr),
                    attempts,
                });
            }
            tx.commit()?;
            Ok(claimed)
        })
        .await?)
}

/// Move a claimed row to `delivered`. Returns whether it was still `pending`
/// — `false` means somebody else got there first and nothing was delivered
/// twice.
///
/// # Errors
/// A mapped storage error.
pub async fn mark_delivered(db: &Database, id: i64) -> Result<bool, Error> {
    finish(db, id, STATE_DELIVERED, None).await
}

/// Move a claimed row to `suppressed` with a reason.
///
/// # Errors
/// A mapped storage error.
pub async fn mark_suppressed(db: &Database, id: i64, reason: &str) -> Result<bool, Error> {
    finish(db, id, STATE_SUPPRESSED, Some(reason.to_owned())).await
}

/// Move a claimed row to `failed` — the channel refused it and its attempts
/// are spent.
///
/// # Errors
/// A mapped storage error.
pub async fn mark_failed(db: &Database, id: i64) -> Result<bool, Error> {
    finish(db, id, STATE_FAILED, None).await
}

/// Return a claimed row to `pending`, due at `at` (unix seconds).
///
/// Used for both quiet-hours deferral and delivery backoff. `refund` gives
/// back the attempt [`claim_due`] charged, which is right for a deferral (the
/// channel was never touched) and wrong for a failure (it was).
///
/// # Errors
/// A mapped storage error.
pub async fn defer(db: &Database, id: i64, at: i64, refund: bool) -> Result<bool, Error> {
    Ok(db
        .write(move |conn| {
            let changed = conn.execute(
                "UPDATE notifications
                 SET next_attempt_at = ?2,
                     attempts = CASE WHEN ?3 THEN MAX(attempts - 1, 0) ELSE attempts END
                 WHERE id = ?1 AND state = 'pending'",
                rusqlite::params![id, at, refund],
            )?;
            Ok(changed > 0)
        })
        .await?)
}

async fn finish(
    db: &Database,
    id: i64,
    state: &'static str,
    suppressed_reason: Option<String>,
) -> Result<bool, Error> {
    Ok(db
        .write(move |conn| {
            let changed = conn.execute(
                "UPDATE notifications
                 SET state = ?2, suppressed_reason = ?3, next_attempt_at = NULL,
                     decided_at = unixepoch()
                 WHERE id = ?1 AND state = 'pending'",
                rusqlite::params![id, state, suppressed_reason],
            )?;
            Ok(changed > 0)
        })
        .await?)
}

/// Delivered alerts with `id > since_id`, oldest first, at most `limit`.
///
/// # Errors
/// A mapped storage error.
pub async fn alerts_since(db: &Database, since_id: i64, limit: i64) -> Result<Vec<Alert>, Error> {
    Ok(db
        .read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT n.id, n.message_id, a.name, n.tier, n.reason, m.subject,
                        m.from_name, m.from_addr, n.decided_at
                 FROM notifications n
                 JOIN accounts a ON a.id = n.account_id
                 JOIN messages m ON m.id = n.message_id
                 -- Inline, not bound, and ordered by the index's own key: see
                 -- `claim_due` and V40 on why both matter to whether the
                 -- partial index this seek depends on is used at all.
                 WHERE n.state = 'delivered' AND n.id > ?1
                 ORDER BY n.id
                 LIMIT ?2",
            )?;
            let rows = stmt.query_map(rusqlite::params![since_id, limit], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, i64>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, Option<String>>(5)?,
                    r.get::<_, Option<String>>(6)?,
                    r.get::<_, Option<String>>(7)?,
                    r.get::<_, Option<i64>>(8)?,
                ))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (id, message_id, account, tier, reason, subject, name, addr, delivered_at) =
                    row?;
                // An unreadable tier cannot happen on a delivered row (the
                // claim path suppresses those before they can be delivered),
                // but reading it back defensively costs nothing and keeps
                // this query total.
                let Some(tier) = Tier::parse(&tier) else {
                    continue;
                };
                out.push(Alert {
                    id,
                    message_id,
                    account,
                    tier,
                    reason,
                    subject,
                    from: display_from(name, addr),
                    delivered_at: delivered_at.unwrap_or_default(),
                });
            }
            Ok(out)
        })
        .await?)
}

/// The highest `notifications.id` currently stored, or `0` if the table is
/// empty — the cursor a `StreamAlerts` subscriber that asked for "from now
/// on" starts at.
///
/// # Errors
/// A mapped storage error.
pub async fn latest_id(db: &Database) -> Result<i64, Error> {
    Ok(db
        .read(|conn| {
            conn.query_row("SELECT COALESCE(MAX(id), 0) FROM notifications", [], |r| {
                r.get::<_, i64>(0)
            })
        })
        .await?)
}

/// What this daemon decided about one message, as `ScoreMessage` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    /// The scored tier.
    pub tier: Tier,
    /// The model's one-line reason.
    pub reason: String,
    /// Why it was suppressed, when it was.
    pub suppressed_reason: Option<String>,
}

/// The decision row `id`, if it is still there and its tier is readable.
///
/// # Errors
/// A mapped storage error.
pub async fn decision(db: &Database, id: i64) -> Result<Option<Decision>, Error> {
    let row: Option<(String, String, Option<String>)> = db
        .read(move |conn| {
            conn.query_row(
                "SELECT tier, reason, suppressed_reason FROM notifications WHERE id = ?1",
                [id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()
        })
        .await?;
    Ok(row.and_then(|(tier, reason, suppressed_reason)| {
        Some(Decision {
            tier: Tier::parse(&tier)?,
            reason,
            suppressed_reason,
        })
    }))
}

/// One message's notification decision, for `ScoreMessage`'s "what would
/// happen / what did happen" answer.
///
/// # Errors
/// A mapped storage error.
pub async fn state_of(db: &Database, message_id: i64) -> Result<Option<(String, i64)>, Error> {
    Ok(db
        .read(move |conn| {
            conn.query_row(
                "SELECT state, id FROM notifications WHERE message_id = ?1",
                [message_id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()
        })
        .await?)
}

/// `"Ada Lovelace <ada@example.com>"`, or whichever half exists.
fn display_from(name: Option<String>, addr: Option<String>) -> Option<String> {
    match (name, addr) {
        (Some(name), Some(addr)) => Some(format!("{name} <{addr}>")),
        (Some(one), None) | (None, Some(one)) => Some(one),
        (None, None) => None,
    }
}
