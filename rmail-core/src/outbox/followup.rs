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
//!
//! # The waiting-on list is where that display detail stops being one
//!
//! [`FollowupStore::waiting_on`] (task 63, prd.md #21) is the aging view: "who
//! owes me an answer, and for how long". A reminder whose reply has already
//! arrived has no business on it — the list exists precisely to be read and
//! acted on — so this one query *does* evaluate reply detection, as a
//! `NOT EXISTS` correlated subquery inside the same statement rather than a
//! round trip per row.
//!
//! That is not a reversal of the decision above, but the cost is honestly
//! larger than a page: the subquery is evaluated for every *live* reminder in
//! the account, and the ordering (`COALESCE(sent_at, created_at)`) is not one
//! `idx_followups_waiting` can serve, so SQLite sorts the matches before
//! applying `LIMIT`. What makes that affordable is the denominator — a human
//! opening a waiting-on list, against a number of live reminders bounded by
//! how many things one person is actually waiting on — where the sweep pays
//! per tick, forever, whether or not anyone is looking. If a mailbox ever
//! carries enough live reminders for this to matter, the fix is a
//! materialized sort column, not moving reply detection back out of the
//! query.
//!
//! The list stays a pure read: it filters rows out of a page and never writes
//! `dismissed`, because a listing that mutated state would make "what does
//! the waiting-on list say" depend on who called it last.

use rusqlite::{OptionalExtension, Row};

use crate::error::Error;
use crate::storage::Database;

pub mod track;

/// Default page size for [`FollowupStore::list`].
pub const DEFAULT_LIST_LIMIT: usize = 50;

/// Hard cap on [`FollowupStore::list`]'s page size.
pub const MAX_LIST_LIMIT: usize = crate::page::MAX_PAGE_SIZE as usize;

/// One page of [`FollowupStore::list`], plus the token for the next one.
#[derive(Debug, Clone)]
pub struct FollowupPage {
    /// This page's reminders, soonest-due first.
    pub followups: Vec<Followup>,
    /// The token for the following page; `None` means this was the last.
    pub next_page_token: Option<String>,
}

/// The page-token scope for a reminder listing — see [`crate::page`].
#[must_use]
pub fn list_scope(account_id: Option<i64>, state: Option<FollowupState>) -> crate::page::PageScope {
    crate::page::PageScope::new("rmail.v1.SendSchedulerService/ListFollowups")
        .opt_field("account_id", account_id)
        .opt_field("state", state.map(FollowupState::as_str))
}

/// The page-token scope for a waiting-on listing — see [`crate::page`].
///
/// Distinct from [`list_scope`] on purpose: the two orderings differ (oldest
/// sent first versus soonest due first), so a token minted by one is
/// meaningless to the other and must be rejected rather than silently
/// resuming from the wrong place.
#[must_use]
pub fn waiting_on_scope(account_id: Option<i64>, overdue_only: bool) -> crate::page::PageScope {
    crate::page::PageScope::new("rmail.v1.SendSchedulerService/ListWaitingOn")
        .opt_field("account_id", account_id)
        .opt_field("overdue_only", Some(overdue_only))
}

/// Longest note retained, in octets. A reminder note is a line, not a
/// document, and this column is read into a listing.
pub const MAX_NOTE: usize = 1_000;

/// Longest extracted ask retained, in octets. Same reasoning as [`MAX_NOTE`],
/// and the same enforcement point: model prose is bounded before it reaches a
/// column a listing reads.
pub const MAX_ASK: usize = 1_000;

/// Longest subject retained on a reminder, in octets.
pub const MAX_SUBJECT: usize = 1_000;

/// Most addresses one reminder may say it is waiting on.
///
/// A bound rather than a truncation, because the column is written from an
/// RPC field: without one, a client can store an arbitrarily large blob in a
/// row that every waiting-on page then reads back. The number is generous —
/// a message with more than this many recipients has other problems, and the
/// pre-send guardian says so.
pub const MAX_WAITING_ON: usize = 200;

/// Who armed a reminder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FollowupKind {
    /// A human asked for it.
    Manual,
    /// The tracker's judge decided the message expected a reply.
    Auto,
}

impl FollowupKind {
    /// Every kind, for exhaustive iteration in tests and tooling.
    pub const ALL: [Self; 2] = [Self::Manual, Self::Auto];

    /// The stable string stored in `followups.kind`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Auto => "auto",
        }
    }

    /// Parse a stored value.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] for a string no version of this code wrote. The
    /// column carries no `CHECK` constraint (see `V42`'s own comment on why
    /// rebuilding this table was not worth it), so this is where the
    /// vocabulary is actually enforced on the way out.
    pub fn parse(value: &str) -> Result<Self, Error> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.as_str() == value)
            .ok_or_else(|| Error::internal(format!("unknown followup kind: {value}")))
    }
}

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
    /// Whether a human or the tracker's judge armed it.
    pub kind: FollowupKind,
    /// The ask this message is waiting on an answer to, when one was
    /// extracted.
    pub ask: Option<String>,
    /// Who is being waited on, as bare addr-specs.
    pub waiting_on: Vec<String>,
    /// The subject of the message being waited on, frozen at arm time — see
    /// `V42`'s comment on why it is denormalized.
    pub subject: String,
    /// When the tracked message went out (unix seconds), which is what aging
    /// is measured from. `None` for a reminder armed on a message this
    /// machine did not send.
    pub sent_at: Option<i64>,
    /// Creation time (unix seconds).
    pub created_at: i64,
}

impl Followup {
    /// How long this reminder has been waiting, in seconds, at `now`.
    ///
    /// Measured from [`Self::sent_at`] when it is known and from
    /// [`Self::created_at`] otherwise — a reminder armed by hand on somebody
    /// else's message still has an age, it is just the age of the reminder
    /// rather than of the silence.
    #[must_use]
    pub fn age_secs(&self, now: i64) -> i64 {
        now.saturating_sub(self.sent_at.unwrap_or(self.created_at))
            .max(0)
    }

    /// Whether `now` is past this reminder's `remind_at`.
    #[must_use]
    pub fn is_overdue(&self, now: i64) -> bool {
        now >= self.remind_at
    }
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
    /// Whether a human or the tracker's judge is arming this.
    pub kind: FollowupKind,
    /// The extracted ask, when there is one.
    pub ask: Option<String>,
    /// Who is being waited on, as bare addr-specs.
    pub waiting_on: Vec<String>,
    /// The tracked message's subject.
    pub subject: String,
    /// When the tracked message went out (unix seconds).
    pub sent_at: Option<i64>,
}

impl NewFollowup {
    /// A hand-armed reminder with no tracker metadata — the task 61 shape.
    ///
    /// Exists so the fields task 63 added do not have to be spelled out at
    /// every call site that predates them, and so `Default`-style struct
    /// update syntax is not the only way to construct one (which would make
    /// adding a *required* field later invisible at those call sites).
    #[must_use]
    pub fn manual(
        account_id: i64,
        message_id: impl Into<String>,
        remind_at: i64,
        tz: impl Into<String>,
        cancel_on_reply: bool,
    ) -> Self {
        Self {
            account_id,
            thread_id: None,
            message_id: message_id.into(),
            remind_at,
            tz: tz.into(),
            cancel_on_reply,
            note: None,
            kind: FollowupKind::Manual,
            ask: None,
            waiting_on: Vec::new(),
            subject: String::new(),
            sent_at: None,
        }
    }
}

/// How addresses are joined in `followups.waiting_on`.
///
/// A newline, for the reason `outbox.to_addrs` uses one: a comma can appear
/// inside a quoted local-part and would silently split one recipient into two.
const ADDRESS_SEPARATOR: char = '\n';

/// Join addresses for storage, dropping blanks.
fn join_addresses(addresses: &[String]) -> String {
    addresses
        .iter()
        .map(|a| a.trim())
        .filter(|a| !a.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Split a stored `waiting_on` column back into addresses.
fn split_addresses(stored: &str) -> Vec<String> {
    stored
        .split(ADDRESS_SEPARATOR)
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(ToOwned::to_owned)
        .collect()
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
        if new.ask.as_ref().is_some_and(|a| a.len() > MAX_ASK) {
            return Err(Error::invalid_argument(format!(
                "follow-up ask exceeds {MAX_ASK} octets"
            )));
        }
        if new.subject.len() > MAX_SUBJECT {
            return Err(Error::invalid_argument(format!(
                "follow-up subject exceeds {MAX_SUBJECT} octets"
            )));
        }
        if new.waiting_on.len() > MAX_WAITING_ON {
            return Err(Error::invalid_argument(format!(
                "a follow-up can wait on at most {MAX_WAITING_ON} addresses"
            )));
        }

        let account_id = new.account_id;
        let waiting_on = join_addresses(&new.waiting_on);
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
                          state, note, kind, ask, waiting_on, subject, sent_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'armed', ?7, ?8, ?9, ?10, ?11, ?12)",
                    rusqlite::params![
                        account_id,
                        new.thread_id,
                        message_id,
                        new.remind_at,
                        new.tz,
                        i64::from(new.cancel_on_reply),
                        new.note,
                        new.kind.as_str(),
                        new.ask,
                        waiting_on,
                        new.subject,
                        new.sent_at,
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

    /// List one page of reminders, soonest-due first, optionally filtered by
    /// account and state.
    ///
    /// `page_token` resumes a previous page and is bound to both filters — see
    /// [`crate::page`].
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] if `page_token` is malformed or belongs to a
    /// different query; otherwise a mapped storage error.
    pub async fn list(
        &self,
        account_id: Option<i64>,
        state: Option<FollowupState>,
        limit: usize,
        page_token: &str,
    ) -> Result<FollowupPage, Error> {
        let scope = list_scope(account_id, state);
        let after = crate::page::decode(page_token, &scope)?;
        let limit = i64::try_from(match limit {
            0 => DEFAULT_LIST_LIMIT,
            n => n.min(MAX_LIST_LIMIT),
        })
        .unwrap_or(i64::MAX);
        // The overflow probe — see `MailStore::list`.
        let probe = limit.saturating_add(1);
        let state = state.map(FollowupState::as_str);

        let mut followups: Vec<Followup> = self
            .db
            .read(move |conn| {
                // Ascending here, unlike every other list: a reminder queue is
                // read soonest-first. The cursor comparison flips with it.
                let cursor_sql = match after {
                    Some(_) => "AND remind_at >= ?4 AND (remind_at > ?4 OR id > ?5)",
                    None => "",
                };
                let mut stmt = conn.prepare(&format!(
                    "SELECT {COLUMNS} FROM followups
                     WHERE (?1 IS NULL OR account_id = ?1)
                       AND (?2 IS NULL OR state = ?2)
                       {cursor_sql}
                     ORDER BY remind_at, id LIMIT ?3"
                ))?;
                let rows = match after {
                    Some(cursor) => stmt.query_map(
                        rusqlite::params![account_id, state, probe, cursor.sort, cursor.id],
                        row_to_raw,
                    )?,
                    None => {
                        stmt.query_map(rusqlite::params![account_id, state, probe], row_to_raw)?
                    }
                };
                rows.collect::<rusqlite::Result<Vec<_>>>()
            })
            .await?
            .into_iter()
            .map(TryInto::try_into)
            .collect::<Result<Vec<_>, Error>>()?;

        let overflow = i64::try_from(followups.len()).unwrap_or(i64::MAX) > limit;
        followups.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        let last = followups
            .last()
            .map(|f| crate::page::Cursor::new(f.remind_at, f.id));
        Ok(FollowupPage {
            next_page_token: crate::page::next_token(&scope, last, overflow),
            followups,
        })
    }

    /// The oldest still-live reminder for `message_id` in `account_id`, if
    /// any.
    ///
    /// "Live" is `armed` or `fired` — a dismissed reminder is a decision the
    /// user made, and it must not make a later [`track`](track::FollowupTracker::track)
    /// silently reuse it.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    pub async fn live_for_message(
        &self,
        account_id: i64,
        message_id: &str,
    ) -> Result<Option<Followup>, Error> {
        let message_id = bare_message_id(message_id);
        if message_id.is_empty() {
            return Ok(None);
        }
        let raw = self
            .db
            .read(move |conn| {
                conn.query_row(
                    &format!(
                        "SELECT {COLUMNS} FROM followups
                         WHERE account_id = ?1 AND message_id = ?2
                           AND state IN ('armed', 'fired')
                         ORDER BY id LIMIT 1"
                    ),
                    rusqlite::params![account_id, message_id],
                    row_to_raw,
                )
                .optional()
            })
            .await?;
        raw.map(TryInto::try_into).transpose()
    }

    /// One page of the aging waiting-on list, longest-waiting first.
    ///
    /// Only live reminders (`armed` or `fired`) appear, and a reminder whose
    /// thread has already been answered is filtered out in SQL — see the
    /// module docs for why *this* query evaluates reply detection when
    /// [`Self::sweep`] deliberately defers it, and why it stays a pure read.
    ///
    /// `overdue_only` narrows the page to reminders already past their
    /// `remind_at`: "who is late", as distinct from "who owes me anything".
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] if `page_token` is malformed or belongs to a
    /// different query; otherwise a mapped storage error.
    #[tracing::instrument(skip(self, page_token), fields(returned))]
    pub async fn waiting_on(
        &self,
        account_id: Option<i64>,
        overdue_only: bool,
        now: i64,
        limit: usize,
        page_token: &str,
    ) -> Result<FollowupPage, Error> {
        let scope = waiting_on_scope(account_id, overdue_only);
        let after = crate::page::decode(page_token, &scope)?;
        let limit = i64::try_from(match limit {
            0 => DEFAULT_LIST_LIMIT,
            n => n.min(MAX_LIST_LIMIT),
        })
        .unwrap_or(i64::MAX);
        let probe = limit.saturating_add(1);

        // Oldest-waiting first, which is `COALESCE(sent_at, created_at)`
        // *ascending* — the earliest instant is the longest wait. That is
        // already the direction `crate::page`'s cursor comparison is written
        // for, so no negation is needed and none is used: the one this query
        // originally carried inverted the whole listing and put the newest
        // row at the top of a list whose entire purpose is aging.
        let mut followups: Vec<Followup> = self
            .db
            .read(move |conn| {
                let cursor_sql = match after {
                    Some(_) => {
                        "AND COALESCE(f.sent_at, f.created_at) >= ?4 \
                         AND (COALESCE(f.sent_at, f.created_at) > ?4 OR f.id > ?5)"
                    }
                    None => "",
                };
                // `?2` is referenced either way, so the bound parameter count
                // does not depend on the filter. A statement whose parameter
                // count changes with a `format!` branch is one place away from
                // a "wrong number of parameters" at runtime.
                let overdue_sql = if overdue_only {
                    "AND f.remind_at <= ?2"
                } else {
                    "AND (?2 IS NOT NULL)"
                };
                let mut stmt = conn.prepare(&format!(
                    "SELECT {COLUMNS} FROM followups f
                     WHERE (?1 IS NULL OR f.account_id = ?1)
                       AND f.state IN ('armed', 'fired')
                       {overdue_sql}
                       {cursor_sql}
                       AND (f.cancel_on_reply = 0 OR NOT EXISTS (
                             SELECT 1 FROM messages m
                             WHERE m.account_id = f.account_id
                               AND (instr(' ' || COALESCE(m.in_reply_to, '') || ' ',
                                          ' ' || f.message_id || ' ') > 0
                                 OR instr(' ' || COALESCE(m.references_hdr, '') || ' ',
                                          ' ' || f.message_id || ' ') > 0)))
                     ORDER BY COALESCE(f.sent_at, f.created_at), f.id
                     LIMIT ?3"
                ))?;
                let rows = match after {
                    Some(cursor) => stmt.query_map(
                        rusqlite::params![account_id, now, probe, cursor.sort, cursor.id],
                        row_to_raw,
                    )?,
                    None => {
                        stmt.query_map(rusqlite::params![account_id, now, probe], row_to_raw)?
                    }
                };
                rows.collect::<rusqlite::Result<Vec<_>>>()
            })
            .await?
            .into_iter()
            .map(TryInto::try_into)
            .collect::<Result<Vec<_>, Error>>()?;

        let overflow = i64::try_from(followups.len()).unwrap_or(i64::MAX) > limit;
        followups.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        tracing::Span::current().record("returned", followups.len());
        let last = followups
            .last()
            .map(|f| crate::page::Cursor::new(f.sent_at.unwrap_or(f.created_at), f.id));
        Ok(FollowupPage {
            next_page_token: crate::page::next_token(&scope, last, overflow),
            followups,
        })
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
     state, note, created_at, kind, ask, waiting_on, subject, sent_at";

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
    kind: String,
    ask: Option<String>,
    waiting_on: String,
    subject: String,
    sent_at: Option<i64>,
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
            kind: FollowupKind::parse(&raw.kind)?,
            ask: raw.ask,
            waiting_on: split_addresses(&raw.waiting_on),
            subject: raw.subject,
            sent_at: raw.sent_at,
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
        kind: row.get(10)?,
        ask: row.get(11)?,
        waiting_on: row.get(12)?,
        subject: row.get(13)?,
        sent_at: row.get(14)?,
    })
}

#[cfg(test)]
mod tests;
