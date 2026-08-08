//! Follow-up reminders: "I sent this; nudge me if nobody replies."
//!
//! The same scheduler drains these and the outbox, because they are the same
//! shape of problem — a durable row with an absolute due time that must
//! survive a restart — and running two loops over one database to answer two
//! variants of "what is due now" would double the wake-ups for no benefit.
//!
//! # Auto-dismiss is evaluated when the reminder fires, not continuously
//!
//! prd.md asks for "follow-up auto-dismiss on detected reply". Detection means
//! asking whether any locally-synced message names this one in `In-Reply-To`
//! or `References`, which is a scan of `messages` — cheap once, expensive
//! thirty times a minute for every armed reminder in the account.
//!
//! Evaluating it at fire time gets the behavior that actually matters: a
//! reminder whose thread was answered never nudges. The only thing deferring
//! it costs is that a still-armed reminder is listed as armed until its due
//! date even though the reply has already arrived — a display detail, against
//! a whole-table scan per tick.

use rusqlite::{OptionalExtension, Row};

use crate::error::Error;
use crate::storage::Database;

/// Default page size for [`FollowupStore::list`].
pub const DEFAULT_LIST_LIMIT: usize = 50;

/// Hard cap on [`FollowupStore::list`]'s page size.
pub const MAX_LIST_LIMIT: usize = 500;

/// Longest note retained, in octets. A reminder note is a line, not a
/// document, and this column is read into a listing.
pub const MAX_NOTE: usize = 1_000;

/// Where a reminder stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FollowupState {
    /// Waiting for its `remind_at`.
    Armed,
    /// Due and raised.
    Fired,
    /// Cancelled — by the user, or automatically because a reply arrived.
    Dismissed,
}

impl FollowupState {
    /// Every state, for exhaustive iteration in tests and tooling.
    pub const ALL: [Self; 3] = [Self::Armed, Self::Fired, Self::Dismissed];

    /// The stable string stored in `followups.state`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Armed => "armed",
            Self::Fired => "fired",
            Self::Dismissed => "dismissed",
        }
    }

    /// Parse a stored value.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] for a string no version of this code wrote.
    pub fn parse(value: &str) -> Result<Self, Error> {
        Self::ALL
            .into_iter()
            .find(|state| state.as_str() == value)
            .ok_or_else(|| Error::internal(format!("unknown followup state: {value}")))
    }
}

/// An armed (or spent) reminder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Followup {
    /// Stable id.
    pub id: i64,
    /// Owning account.
    pub account_id: i64,
    /// The local thread, if one is known yet.
    pub thread_id: Option<i64>,
    /// The RFC 5322 `Message-ID` being followed up, bare.
    pub message_id: String,
    /// When to nudge (unix seconds).
    pub remind_at: i64,
    /// The IANA zone it was armed in. Display only.
    pub tz: String,
    /// Whether a detected reply dismisses it.
    pub cancel_on_reply: bool,
    /// Where it stands.
    pub state: FollowupState,
    /// What the user wanted to be reminded about.
    pub note: Option<String>,
    /// Creation time (unix seconds).
    pub created_at: i64,
}

/// A reminder being armed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewFollowup {
    /// Owning account.
    pub account_id: i64,
    /// The local thread, if known.
    pub thread_id: Option<i64>,
    /// The `Message-ID` to follow up, bare.
    pub message_id: String,
    /// When to nudge (unix seconds).
    pub remind_at: i64,
    /// The IANA zone it was armed in.
    pub tz: String,
    /// Whether a detected reply dismisses it.
    pub cancel_on_reply: bool,
    /// An optional note.
    pub note: Option<String>,
}

/// Follow-up storage.
///
/// Cheap to clone; every clone shares one database handle.
#[derive(Debug, Clone)]
pub struct FollowupStore {
    db: Database,
}

impl FollowupStore {
    /// Open a store over `db`.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Arm a reminder.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] if `message_id` is empty or the note is
    /// longer than [`MAX_NOTE`]; [`Error::NotFound`] if `account_id` names no
    /// account. Otherwise a mapped storage error.
    #[tracing::instrument(skip(self, new), fields(account_id = new.account_id, followup_id))]
    pub async fn create(&self, new: NewFollowup) -> Result<Followup, Error> {
        // Bare, matching how `messages.message_id` stores it — a follow-up
        // whose id keeps its angle brackets would never match the reply that
        // is supposed to dismiss it.
        let message_id = bare_message_id(&new.message_id);
        if message_id.is_empty() {
            return Err(Error::invalid_argument(
                "a follow-up needs the Message-ID it is following up",
            ));
        }
        if new.note.as_ref().is_some_and(|n| n.len() > MAX_NOTE) {
            return Err(Error::invalid_argument(format!(
                "follow-up note exceeds {MAX_NOTE} octets"
            )));
        }

        let account_id = new.account_id;
        let id = self
            .db
            .write(move |conn| {
                let tx = conn.transaction()?;
                let exists: Option<i64> = tx
                    .query_row("SELECT 1 FROM accounts WHERE id = ?1", [account_id], |r| {
                        r.get(0)
                    })
                    .optional()?;
                if exists.is_none() {
                    return Ok(Err(Error::not_found(format!(
                        "account {account_id} not found"
                    ))));
                }
                tx.execute(
                    "INSERT INTO followups
                         (account_id, thread_id, message_id, remind_at, tz, cancel_on_reply,
                          state, note)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'armed', ?7)",
                    rusqlite::params![
                        account_id,
                        new.thread_id,
                        message_id,
                        new.remind_at,
                        new.tz,
                        i64::from(new.cancel_on_reply),
                        new.note,
                    ],
                )?;
                let id = tx.last_insert_rowid();
                tx.commit()?;
                Ok(Ok(id))
            })
            .await??;
        tracing::Span::current().record("followup_id", id);
        self.get(id).await
    }

    /// Fetch one reminder.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] if no row has `id`; otherwise a mapped storage
    /// error.
    pub async fn get(&self, id: i64) -> Result<Followup, Error> {
        let raw = self
            .db
            .read(move |conn| {
                conn.query_row(
                    &format!("SELECT {COLUMNS} FROM followups WHERE id = ?1"),
                    [id],
                    row_to_raw,
                )
                .optional()
            })
            .await?;
        match raw {
            Some(raw) => raw.try_into(),
            None => Err(Error::not_found(format!("followup {id} not found"))),
        }
    }

    /// List reminders, newest first, optionally filtered by account and
    /// state.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    pub async fn list(
        &self,
        account_id: Option<i64>,
        state: Option<FollowupState>,
        limit: usize,
    ) -> Result<Vec<Followup>, Error> {
        let limit = i64::try_from(match limit {
            0 => DEFAULT_LIST_LIMIT,
            n => n.min(MAX_LIST_LIMIT),
        })
        .unwrap_or(i64::MAX);
        let state = state.map(FollowupState::as_str);

        self.db
            .read(move |conn| {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {COLUMNS} FROM followups
                     WHERE (?1 IS NULL OR account_id = ?1)
                       AND (?2 IS NULL OR state = ?2)
                     ORDER BY remind_at, id LIMIT ?3"
                ))?;
                let rows = stmt
                    .query_map(rusqlite::params![account_id, state, limit], row_to_raw)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?
            .into_iter()
            .map(TryInto::try_into)
            .collect()
    }

    /// Dismiss a reminder.
    ///
    /// Idempotent on an already-dismissed row, for the reason
    /// [`super::OutboxStore::cancel`] is: dismissing twice is a thing people
    /// do, not an error.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] if no row has `id`; otherwise a mapped storage
    /// error.
    #[tracing::instrument(skip(self))]
    pub async fn dismiss(&self, id: i64) -> Result<Followup, Error> {
        let changed = self
            .db
            .write(move |conn| {
                conn.execute(
                    "UPDATE followups SET state = 'dismissed' WHERE id = ?1 AND state <> 'dismissed'",
                    [id],
                )
            })
            .await?;
        let followup = self.get(id).await?;
        if changed > 0 {
            tracing::debug!(followup_id = id, "follow-up dismissed");
        }
        Ok(followup)
    }

    /// Fire every due reminder, dismissing the ones whose thread was already
    /// answered.
    ///
    /// Returns the reminders that actually fired, so the caller can publish
    /// them. See the module docs for why reply detection happens here rather
    /// than continuously.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    #[tracing::instrument(skip(self), fields(fired, auto_dismissed))]
    pub async fn sweep(&self, now: i64) -> Result<Vec<Followup>, Error> {
        let due: Vec<Followup> = self
            .db
            .read(move |conn| {
                let mut stmt = conn.prepare(&format!(
                    "SELECT {COLUMNS} FROM followups
                     WHERE state = 'armed' AND remind_at <= ?1
                     ORDER BY remind_at, id"
                ))?;
                let rows = stmt
                    .query_map([now], row_to_raw)?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?
            .into_iter()
            .map(TryInto::try_into)
            .collect::<Result<Vec<_>, Error>>()?;
        if due.is_empty() {
            return Ok(Vec::new());
        }

        let mut fired = Vec::new();
        let mut dismissed = 0usize;
        for followup in due {
            let answered = followup.cancel_on_reply
                && self
                    .has_reply(followup.account_id, followup.message_id.clone())
                    .await?;
            let id = followup.id;
            let next = if answered {
                FollowupState::Dismissed
            } else {
                FollowupState::Fired
            };
            // Conditional on still being armed: a `DismissFollowup` that
            // landed between the read above and this write must win, or a
            // user who just cancelled a reminder gets nudged anyway.
            let changed = self
                .db
                .write(move |conn| {
                    conn.execute(
                        "UPDATE followups SET state = ?2 WHERE id = ?1 AND state = 'armed'",
                        rusqlite::params![id, next.as_str()],
                    )
                })
                .await?;
            if changed == 0 {
                continue;
            }
            if answered {
                dismissed += 1;
                tracing::debug!(
                    followup_id = id,
                    "follow-up auto-dismissed; a reply arrived"
                );
            } else {
                fired.push(Followup {
                    state: FollowupState::Fired,
                    ..followup
                });
            }
        }
        tracing::Span::current().record("fired", fired.len());
        tracing::Span::current().record("auto_dismissed", dismissed);
        Ok(fired)
    }

    /// Whether any locally-synced message in `account_id` names `message_id`
    /// in `In-Reply-To` or `References`.
    ///
    /// `instr` over a space-padded copy of the column rather than `LIKE`:
    /// both columns hold space-joined bare ids (see `message::parse`), and a
    /// `Message-ID` may legally contain `%` and `_`, which `LIKE` would treat
    /// as wildcards — matching a reply to a *different* message.
    async fn has_reply(&self, account_id: i64, message_id: String) -> Result<bool, Error> {
        Ok(self
            .db
            .read(move |conn| {
                conn.query_row(
                    "SELECT EXISTS(
                         SELECT 1 FROM messages
                         WHERE account_id = ?1
                           AND (instr(' ' || COALESCE(in_reply_to, '') || ' ',
                                      ' ' || ?2 || ' ') > 0
                             OR instr(' ' || COALESCE(references_hdr, '') || ' ',
                                      ' ' || ?2 || ' ') > 0))",
                    rusqlite::params![account_id, message_id],
                    |row| row.get::<_, bool>(0),
                )
            })
            .await?)
    }
}

/// Strip the angle brackets a caller may have copied out of a header.
fn bare_message_id(value: &str) -> String {
    let trimmed = value.trim();
    trimmed
        .strip_prefix('<')
        .and_then(|v| v.strip_suffix('>'))
        .unwrap_or(trimmed)
        .trim()
        .to_owned()
}

const COLUMNS: &str = "id, account_id, thread_id, message_id, remind_at, tz, cancel_on_reply, \
     state, note, created_at";

struct RawFollowup {
    id: i64,
    account_id: i64,
    thread_id: Option<i64>,
    message_id: String,
    remind_at: i64,
    tz: String,
    cancel_on_reply: i64,
    state: String,
    note: Option<String>,
    created_at: i64,
}

impl TryFrom<RawFollowup> for Followup {
    type Error = Error;

    fn try_from(raw: RawFollowup) -> Result<Self, Error> {
        Ok(Self {
            id: raw.id,
            account_id: raw.account_id,
            thread_id: raw.thread_id,
            message_id: raw.message_id,
            remind_at: raw.remind_at,
            tz: raw.tz,
            cancel_on_reply: raw.cancel_on_reply != 0,
            state: FollowupState::parse(&raw.state)?,
            note: raw.note,
            created_at: raw.created_at,
        })
    }
}

fn row_to_raw(row: &Row<'_>) -> rusqlite::Result<RawFollowup> {
    Ok(RawFollowup {
        id: row.get(0)?,
        account_id: row.get(1)?,
        thread_id: row.get(2)?,
        message_id: row.get(3)?,
        remind_at: row.get(4)?,
        tz: row.get(5)?,
        cancel_on_reply: row.get(6)?,
        state: row.get(7)?,
        note: row.get(8)?,
        created_at: row.get(9)?,
    })
}

#[cfg(test)]
mod tests;
