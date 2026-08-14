//! Scheduled send: the durable outbox, its state machine, and the scheduler
//! that drains it (prd.md III-5, "Send Later / Scheduled Send"; task 61).
//!
//! Everything outgoing goes through here, including "send now" — an immediate
//! send is really *schedule at `now + undo_window`*, which is what makes undo
//! a cancel rather than a recall. The row is durable, so a message survives a
//! restart, an offline window, and a laptop lid; prd.md's guiding principle is
//! that nothing depends on rmail running at the exact `send_at` instant beyond
//! a small tolerance, and that a late start still sends rather than silently
//! dropping.
//!
//! # The two failure modes this module is shaped around
//!
//! A send path has exactly two irreversible bugs: **delivering twice** and
//! **losing a message**. Both are invisible to the sender and permanent from
//! the recipient's side, so the design spends complexity on them and nowhere
//! else.
//!
//! ## At-most-once: the `smtp_message_id` fence
//!
//! Before the SMTP `DATA` command, the worker writes the message's
//! `Message-ID` into `outbox.smtp_message_id` **and commits**. Only then does
//! it transmit. Three things can happen next:
//!
//! - **Success** — the row goes to `sent`, and the fence stays as the record
//!   of what was delivered.
//! - **The peer answers with an error** — nothing was queued (SMTP is
//!   strictly request/response; a rejected `DATA` is a rejected message), so
//!   the failure path clears the fence back to `NULL` and the retry genuinely
//!   retries.
//! - **The process dies** — the fence survives, the lease lapses, and
//!   [`OutboxStore::reap_expired`] returns the row to `scheduled` *with the
//!   fence intact*. The next worker to claim it sees a committed
//!   `Message-ID`, concludes a copy may already be on the wire, and marks the
//!   row `sent` without transmitting.
//!
//! That last branch is a deliberate trade: rmail would rather leave a message
//! that *might* not have arrived than deliver a second copy of one that did.
//! prd.md states it directly ("retry treats an already-present Message-ID as
//! `sent` → at-most-once"), and the difference between the second and third
//! branches — a returned error versus a vanished process — is the entire
//! reason a retry can still exist at all.
//!
//! ## Never dropped: leases, backoff, and late tolerance
//!
//! A worker holds a row by lease, the same shape [`crate::index::IndexQueue`]
//! and [`crate::ai::AiQueue`] use: the lease *is* the liveness claim and it
//! expires on its own, so a crash returns work to the queue rather than to
//! nobody. Transient failures (4xx, offline, timeout) back off and stay
//! `scheduled` — being offline is not a reason to fail a message — until
//! `max_retries` is spent. Permanent failures (5xx, auth, invalid recipient)
//! go straight to `failed` and are never retried automatically, because
//! retrying a 550 forever is just a slower way of not telling the user.
//!
//! If rmail was not running at `send_at`, the message still goes out. Within
//! `send.late_tolerance` that is unremarkable; past it the row is flagged
//! [`OutboxEntry::sent_late`] so the user learns it was late instead of
//! assuming it was punctual. Nothing is ever dropped for being overdue.
//!
//! # Time is frozen, timezones are not recomputed
//!
//! `send_at` is an absolute unix instant, resolved once at schedule time (see
//! [`schedule`]). `tz` records the IANA zone it was scheduled *in*, for
//! display. Re-deriving the instant at send time from a wall-clock time plus a
//! zone is exactly how a message scheduled across a DST boundary goes out an
//! hour early or late, and it is a silent failure — every intermediate value
//! looks right.
//!
//! # What this module does not render
//!
//! Nothing. [`crate::compose::mime::build`] produces the RFC 5322 octets and
//! this module freezes them into `outbox.raw_mime` and hands them to `lettre`
//! unchanged. In particular the renderer emits **no `Bcc` header** — blind
//! recipients live only in the envelope — so the copy appended to IMAP `Sent`
//! needs no stripping pass and must not grow one.

pub mod followup;
pub mod policy;
pub mod schedule;
pub mod scheduler;
pub mod sent;
pub mod smtp;

#[cfg(test)]
pub(crate) mod mock;

use std::sync::Arc;
use std::time::Duration;

use rusqlite::{OptionalExtension, Row};
use tokio::sync::{broadcast, Notify};

use crate::compose::{Draft, DraftPatch, DraftStore, Mailbox};
use crate::error::Error;
use crate::storage::Database;

pub use followup::{Followup, FollowupState, FollowupStore, NewFollowup};
pub use policy::{ResolvedSchedule, SendPolicy, MIN_AI_UNDO_WINDOW};
pub use schedule::{resolve_send_at, ResolvedTime};
pub use scheduler::{SchedulerHandle, SendScheduler};
pub use sent::{ImapSentAppender, SentAppender};
pub use smtp::{classify_smtp_error, LettreSender, SendEnvelope, SendFailure, SmtpSender};

/// How many outbox changes the [`OutboxStore::watch`] fan-out buffers per
/// subscriber before that subscriber is told it lagged.
///
/// Matched to [`crate::notes`]'s channel for the same reason it chose one: a
/// slow `WatchOutbox` client must degrade to "you missed some" rather than
/// applying back-pressure to the sender, which here would be the scheduler
/// itself.
const CHANNEL_CAPACITY: usize = 256;

/// Default page size for [`OutboxStore::list`].
pub const DEFAULT_LIST_LIMIT: usize = 50;

/// Hard cap on [`OutboxStore::list`]'s page size, matching prd.md's
/// "server caps 500" pagination rule.
pub const MAX_LIST_LIMIT: usize = crate::page::MAX_PAGE_SIZE as usize;

/// One page of [`OutboxStore::list`], plus the token for the next one.
#[derive(Debug, Clone)]
pub struct OutboxPage {
    /// This page's entries, newest first.
    pub entries: Vec<OutboxEntry>,
    /// The token for the following page; `None` means this was the last.
    pub next_page_token: Option<String>,
}

/// The page-token scope for an outbox listing — see [`crate::page`].
///
/// Both filters are bound, not just the account: a token minted while looking
/// at `failed` sends would otherwise resume a `scheduled` listing at a
/// position that means nothing in it.
#[must_use]
pub fn list_scope(account_id: Option<i64>, state: Option<OutboxState>) -> crate::page::PageScope {
    crate::page::PageScope::new("rmail.v1.SendSchedulerService/ListOutbox")
        .opt_field("account_id", account_id)
        .opt_field("state", state.map(OutboxState::as_str))
}

/// Longest `body_preview` retained, in characters.
///
/// The preview exists so `mail outbox` can show what is queued without
/// loading `raw_mime`; a bound keeps a listing of 500 entries a few hundred
/// kilobytes rather than however large the messages happen to be.
const MAX_PREVIEW_CHARS: usize = 200;

// ---------------------------------------------------------------------------
// Vocabulary
// ---------------------------------------------------------------------------

/// Where an outbox row stands.
///
/// The legal transitions are exactly prd.md's:
///
/// ```text
/// scheduled -> sending -> sent
/// scheduled -> sending -> scheduled   (transient failure, retry)
/// scheduled -> sending -> failed      (permanent / retries exhausted)
/// scheduled -> canceled               (user)
/// ```
///
/// `failed` is not terminal only because [`OutboxStore::retry`] exists; the
/// scheduler itself never moves a row out of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutboxState {
    /// Waiting for its `send_at`, or backing off after a transient failure.
    Scheduled,
    /// Leased by a worker that is transmitting it.
    Sending,
    /// Delivered (or, on the recovery path, assumed delivered — see the
    /// module docs on the `smtp_message_id` fence).
    Sent,
    /// Rejected permanently, or out of retries. Only an explicit
    /// [`OutboxStore::retry`] moves it.
    Failed,
    /// Canceled by the user before it was claimed.
    Canceled,
    /// The SMTP session died without a reply, so whether the peer queued the
    /// message is unknown.
    ///
    /// Deliberately neither `sent` nor `failed`. Calling it `failed` would
    /// invite a retry that may deliver a second copy; calling it `sent` would
    /// claim a delivery that may never have happened. The row keeps its
    /// `smtp_message_id` fence and waits for a human — see the module docs.
    Uncertain,
}

impl OutboxState {
    /// Every state, for exhaustive iteration in tests and tooling.
    pub const ALL: [Self; 6] = [
        Self::Scheduled,
        Self::Sending,
        Self::Sent,
        Self::Failed,
        Self::Canceled,
        Self::Uncertain,
    ];

    /// The stable string stored in `outbox.state`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Scheduled => "scheduled",
            Self::Sending => "sending",
            Self::Sent => "sent",
            Self::Failed => "failed",
            Self::Canceled => "canceled",
            Self::Uncertain => "uncertain",
        }
    }

    /// Parse a stored value.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] for a string no version of this code wrote — a
    /// database written by a newer build, which is a deployment problem, not
    /// a request problem.
    pub fn parse(value: &str) -> Result<Self, Error> {
        Self::ALL
            .into_iter()
            .find(|state| state.as_str() == value)
            .ok_or_else(|| Error::internal(format!("unknown outbox state: {value}")))
    }
}

/// Who asked for this message to be sent.
///
/// [`Origin::Ai`] is load-bearing rather than decorative: it is what forces an
/// undo window onto a send nobody typed. See [`SendPolicy`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Origin {
    /// A human, through the CLI/TUI/gRPC.
    User,
    /// Claude, through MCP. Always given an interception window.
    Ai,
    /// Raised automatically by the follow-up tracker.
    Followup,
    /// The undo-window leg of an immediate send.
    Undo,
}

impl Origin {
    /// Every origin, for exhaustive iteration in tests and tooling.
    pub const ALL: [Self; 4] = [Self::User, Self::Ai, Self::Followup, Self::Undo];

    /// The stable string stored in `outbox.origin`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Ai => "ai",
            Self::Followup => "followup",
            Self::Undo => "undo",
        }
    }

    /// Parse a wire/stored value.
    ///
    /// Only ever reads a value this code previously stored: the shipped proto
    /// uses an enum, and the request boundary is `origin_from_proto`. So an
    /// unrecognized value here is a corrupt row or a newer build's database,
    /// not a client mistake — the same class `OutboxState::parse` reports as
    /// [`Error::Internal`], and reported the same way rather than telling a
    /// caller their request was invalid when it was not.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] for anything outside the vocabulary.
    pub fn parse(value: &str) -> Result<Self, Error> {
        Self::ALL
            .into_iter()
            .find(|origin| origin.as_str() == value)
            .ok_or_else(|| Error::internal(format!("unknown outbox origin: {value}")))
    }
}

/// A row in the outbox, without its (potentially megabyte-scale) `raw_mime`.
///
/// [`OutboxStore::raw_mime`] is what returns the octets, and only the send
/// path and an explicit preview ever ask for them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxEntry {
    /// Stable id.
    pub id: i64,
    /// Owning account.
    pub account_id: i64,
    /// The draft this was rendered from, if it still exists.
    ///
    /// Deliberately not deleted when the message goes out. A scheduled send
    /// can still be cancelled, and a cancel that had already destroyed the
    /// draft would leave the user with nothing to re-edit — which is the one
    /// thing they reached for undo in order to do. Removing a sent message's
    /// draft is a client decision (`ComposeService.DeleteDraft`), made after
    /// the outcome is known.
    pub draft_id: Option<i64>,
    /// The `From` addr-spec.
    pub from_addr: String,
    /// `To` addr-specs.
    pub to: Vec<String>,
    /// `Cc` addr-specs.
    pub cc: Vec<String>,
    /// `Bcc` addr-specs. These reach `RCPT TO` and never the message.
    pub bcc: Vec<String>,
    /// Subject, decoded.
    pub subject: String,
    /// A short plain-text excerpt, for listings.
    pub body_preview: String,
    /// The parent's `Message-ID`, if this is a reply. Bare.
    pub in_reply_to: Option<String>,
    /// The local thread, if known.
    pub thread_id: Option<i64>,
    /// The absolute instant this goes out (unix seconds), frozen at schedule
    /// time.
    pub send_at: i64,
    /// The IANA zone it was scheduled in. Display only.
    pub tz: String,
    /// Where it stands.
    pub state: OutboxState,
    /// Who asked.
    pub origin: Origin,
    /// Delivery attempts made so far.
    pub attempts: i64,
    /// Attempts allowed before a transient failure becomes permanent.
    pub max_retries: i64,
    /// When a backed-off row becomes eligible again (unix seconds).
    pub next_attempt_at: Option<i64>,
    /// The last failure, verbatim, for `mail outbox --state failed`.
    pub last_error: Option<String>,
    /// The `Message-ID` committed before `DATA` — the at-most-once fence.
    pub smtp_message_id: Option<String>,
    /// When it was delivered (unix seconds).
    pub sent_at: Option<i64>,
    /// Whether it went out past `send.late_tolerance` because rmail was down.
    pub sent_late: bool,
    /// Until when an undo is offered (unix seconds); `None` for a genuine
    /// future schedule, which is cancelable right up to its lease.
    pub undo_deadline: Option<i64>,
    /// Creation time (unix seconds).
    pub created_at: i64,
    /// Last state change (unix seconds).
    pub updated_at: i64,
}

impl OutboxEntry {
    /// Every address the SMTP envelope must name in `RCPT TO`: `To`, `Cc`,
    /// **and** `Bcc`, deduplicated in that order.
    #[must_use]
    pub fn envelope_recipients(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for addr in self.to.iter().chain(&self.cc).chain(&self.bcc) {
            if !out.iter().any(|seen| seen == addr) {
                out.push(addr.clone());
            }
        }
        out
    }
}

/// A message being placed in the outbox.
///
/// `send_at`/`undo_deadline` are already resolved — see
/// [`SendPolicy::resolve`], which is where the "an AI send always gets an
/// interception window" rule lives.
#[derive(Debug, Clone)]
pub struct NewSend {
    /// Owning account.
    pub account_id: i64,
    /// The draft this was rendered from, if any. Kept so
    /// [`OutboxStore::update_body`] has something editable to re-render.
    pub draft_id: Option<i64>,
    /// The `From` addr-spec.
    pub from_addr: String,
    /// `To` addr-specs.
    pub to: Vec<String>,
    /// `Cc` addr-specs.
    pub cc: Vec<String>,
    /// `Bcc` addr-specs.
    pub bcc: Vec<String>,
    /// Subject, decoded.
    pub subject: String,
    /// The complete RFC 5322 message — exactly what SMTP will transmit.
    pub raw_mime: Vec<u8>,
    /// A plain-text excerpt for listings; truncated on write.
    pub body_preview: String,
    /// The parent's `Message-ID`, bare, if this is a reply.
    pub in_reply_to: Option<String>,
    /// The local thread, if known.
    pub thread_id: Option<i64>,
    /// The resolved absolute instant.
    pub send_at: i64,
    /// The IANA zone the request named. Display only.
    pub tz: String,
    /// Who asked.
    pub origin: Origin,
    /// Until when an undo is offered.
    pub undo_deadline: Option<i64>,
    /// Attempts allowed before a transient failure becomes permanent.
    pub max_retries: i64,
}

/// A row a worker now owns, together with everything needed to transmit it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedSend {
    /// Outbox row id.
    pub id: i64,
    /// Owning account, for credential/host resolution.
    pub account_id: i64,
    /// Who holds this lease. Carried so the completion paths can refuse to
    /// act on a row that has since been reaped and handed to someone else.
    pub worker: String,
    /// The envelope this transmission uses.
    pub envelope: SendEnvelope,
    /// The octets to transmit, verbatim.
    pub raw_mime: Vec<u8>,
    /// The `Message-ID` carried by `raw_mime`.
    pub message_id: String,
    /// The fence, if a previous attempt already committed one — the signal
    /// that a copy may already be on the wire. See the module docs.
    pub committed_message_id: Option<String>,
    /// When this was due (unix seconds); the lateness check compares it.
    pub send_at: i64,
    /// Attempts made, including this one.
    pub attempts: i64,
    /// Attempts allowed.
    pub max_retries: i64,
    /// Who asked.
    pub origin: Origin,
}

/// What happened to a row whose transmission failed transiently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryOutcome {
    /// Backed off; still `scheduled`.
    Retrying {
        /// When it becomes eligible again (unix seconds).
        next_attempt_at: i64,
        /// How many attempts it has now had.
        attempts: i64,
    },
    /// Out of attempts; now `failed`.
    Exhausted {
        /// How many attempts it had.
        attempts: i64,
    },
}

/// A state transition, broadcast to every [`OutboxStore::watch`] subscriber.
///
/// Carries the whole post-transition entry rather than a delta: a
/// `WatchOutbox` client that reconnects mid-stream needs the current row, and
/// re-deriving one from a sequence of deltas is a second, drift-prone copy of
/// the state machine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxChange {
    /// The entry, as it now stands.
    pub entry: OutboxEntry,
}

// ---------------------------------------------------------------------------
// The store
// ---------------------------------------------------------------------------

/// The durable outbox.
///
/// Cheap to clone: every clone shares one database handle, one broadcast
/// channel, and one scheduler wake-up.
#[derive(Clone)]
pub struct OutboxStore {
    db: Database,
    drafts: DraftStore,
    changes: broadcast::Sender<OutboxChange>,
    wake: Arc<Notify>,
}

impl std::fmt::Debug for OutboxStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutboxStore")
            .field("subscribers", &self.changes.receiver_count())
            .finish_non_exhaustive()
    }
}

impl OutboxStore {
    /// Open a store over `db`.
    #[must_use]
    pub fn new(db: Database) -> Self {
        let (changes, _) = broadcast::channel(CHANNEL_CAPACITY);
        Self {
            drafts: DraftStore::new(db.clone()),
            db,
            changes,
            wake: Arc::new(Notify::new()),
        }
    }

    /// Subscribe to state transitions.
    ///
    /// In-process only, and lossy under lag — the durable record is the
    /// `outbox` table, which a lagging subscriber can always re-read.
    #[must_use]
    pub fn watch(&self) -> broadcast::Receiver<OutboxChange> {
        self.changes.subscribe()
    }

    /// A handle that wakes the scheduler sleeping on this store.
    ///
    /// Every write path here already wakes it; this exists for the callers
    /// that are not writes — a resume-from-sleep or network-up observer.
    #[must_use]
    pub fn wake_handle(&self) -> SchedulerHandle {
        SchedulerHandle::new(Arc::clone(&self.wake))
    }

    /// The draft store rendering shares with `ComposeService`.
    ///
    /// One store, not two, so an outbox row's re-render sees exactly the
    /// draft a `ComposeService` edit just wrote.
    #[must_use]
    pub fn drafts(&self) -> &DraftStore {
        &self.drafts
    }

    /// Place a message in the outbox.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] if it names no recipient or carries no
    /// `Message-ID`; [`Error::NotFound`] if `account_id` names no account;
    /// otherwise a mapped storage error.
    #[tracing::instrument(
        skip(self, new),
        fields(account_id = new.account_id, outbox_id, origin = new.origin.as_str())
    )]
    pub async fn schedule(&self, new: NewSend) -> Result<OutboxEntry, Error> {
        if new.to.is_empty() && new.cc.is_empty() && new.bcc.is_empty() {
            return Err(Error::invalid_argument(
                "a scheduled send needs at least one To/Cc/Bcc recipient",
            ));
        }
        // Refused here rather than at claim time: without a `Message-ID` the
        // at-most-once fence has nothing to write, so this row could be
        // delivered twice by a crash. That is a property of the octets, and
        // the only moment a caller can still fix it is now.
        if message_id_of(&new.raw_mime).is_none() {
            return Err(Error::invalid_argument(
                "the rendered message carries no Message-ID header",
            ));
        }

        let preview = truncate_preview(&new.body_preview);
        let account_id = new.account_id;
        let id = self
            .db
            .write(move |conn| {
                let tx = conn.transaction()?;
                // Checked explicitly rather than left to the foreign key, so
                // a bad `account_id` reports as `account N not found` rather
                // than a constraint failure this code would have to guess the
                // meaning of — `outbox` has three foreign keys.
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
                    "INSERT INTO outbox (
                         account_id, draft_id, from_addr, to_addrs, cc_addrs, bcc_addrs,
                         subject, raw_mime, body_preview, in_reply_to, thread_id,
                         send_at, tz, state, origin, max_retries, undo_deadline
                     ) VALUES (
                         ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                         'scheduled', ?14, ?15, ?16
                     )",
                    rusqlite::params![
                        account_id,
                        new.draft_id,
                        new.from_addr,
                        join_addrs(&new.to),
                        join_addrs(&new.cc),
                        join_addrs(&new.bcc),
                        new.subject,
                        new.raw_mime,
                        preview,
                        new.in_reply_to,
                        new.thread_id,
                        new.send_at,
                        new.tz,
                        new.origin.as_str(),
                        new.max_retries.max(0),
                        new.undo_deadline,
                    ],
                )?;
                let id = tx.last_insert_rowid();
                tx.commit()?;
                Ok(Ok(id))
            })
            .await??;

        tracing::Span::current().record("outbox_id", id);
        let entry = self.get(id).await?;
        tracing::info!(
            outbox_id = id,
            send_at = entry.send_at,
            origin = entry.origin.as_str(),
            "message scheduled"
        );
        self.publish(&entry);
        // After the commit, so the scheduler that wakes finds the row.
        self.wake.notify_one();
        Ok(entry)
    }

    /// Fetch one entry.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] if no row has `id`; otherwise a mapped storage
    /// error.
    #[tracing::instrument(skip(self))]
    pub async fn get(&self, id: i64) -> Result<OutboxEntry, Error> {
        let raw = self
            .db
            .read(move |conn| {
                conn.query_row(
                    &format!("SELECT {COLUMNS} FROM outbox WHERE id = ?1"),
                    [id],
                    entry_from_row,
                )
                .optional()
            })
            .await?;
        match raw {
            Some(raw) => raw.try_into(),
            None => Err(Error::not_found(format!("outbox entry {id} not found"))),
        }
    }

    /// The frozen RFC 5322 octets of one entry.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] if no row has `id`; otherwise a mapped storage
    /// error.
    pub async fn raw_mime(&self, id: i64) -> Result<Vec<u8>, Error> {
        self.db
            .read(move |conn| {
                conn.query_row("SELECT raw_mime FROM outbox WHERE id = ?1", [id], |row| {
                    row.get(0)
                })
                .optional()
            })
            .await?
            .ok_or_else(|| Error::not_found(format!("outbox entry {id} not found")))
    }

    /// List one page of an account's outbox, newest first, optionally
    /// filtered by state.
    ///
    /// `limit` of zero means [`DEFAULT_LIST_LIMIT`]; anything above
    /// [`MAX_LIST_LIMIT`] is clamped rather than rejected, matching prd.md's
    /// "server caps 500" rule. `page_token` resumes a previous page and is
    /// bound to **both** filters — see [`crate::page`]: a token minted while
    /// listing one account must not resume into another's queued mail.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] if `page_token` is malformed or belongs to a
    /// different query; otherwise a mapped storage error.
    #[tracing::instrument(skip(self, page_token))]
    pub async fn list(
        &self,
        account_id: Option<i64>,
        state: Option<OutboxState>,
        limit: usize,
        page_token: &str,
    ) -> Result<OutboxPage, Error> {
        let scope = list_scope(account_id, state);
        let after = crate::page::decode(page_token, &scope)?;
        let limit = i64::try_from(match limit {
            0 => DEFAULT_LIST_LIMIT,
            n => n.min(MAX_LIST_LIMIT),
        })
        .unwrap_or(i64::MAX);
        // The overflow probe — see `MailStore::list`.
        let probe = limit.saturating_add(1);
        let state = state.map(OutboxState::as_str);

        let mut entries: Vec<OutboxEntry> = self
            .db
            .read(move |conn| {
                let cursor_sql = match after {
                    Some(_) => "AND created_at <= ?4 AND (created_at < ?4 OR id < ?5)",
                    None => "",
                };
                let mut stmt = conn.prepare(&format!(
                    "SELECT {COLUMNS} FROM outbox
                     WHERE (?1 IS NULL OR account_id = ?1)
                       AND (?2 IS NULL OR state = ?2)
                       {cursor_sql}
                     ORDER BY created_at DESC, id DESC LIMIT ?3"
                ))?;
                let rows = match after {
                    Some(cursor) => stmt.query_map(
                        rusqlite::params![account_id, state, probe, cursor.sort, cursor.id],
                        entry_from_row,
                    )?,
                    None => {
                        stmt.query_map(rusqlite::params![account_id, state, probe], entry_from_row)?
                    }
                };
                rows.collect::<rusqlite::Result<Vec<_>>>()
            })
            .await?
            .into_iter()
            .map(TryInto::try_into)
            .collect::<Result<Vec<_>, Error>>()?;

        let overflow = i64::try_from(entries.len()).unwrap_or(i64::MAX) > limit;
        entries.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        let last = entries
            .last()
            .map(|e| crate::page::Cursor::new(e.created_at, e.id));
        Ok(OutboxPage {
            next_page_token: crate::page::next_token(&scope, last, overflow),
            entries,
        })
    }

    /// Cancel a scheduled send.
    ///
    /// Transactional against the sender: the `UPDATE` only matches a row that
    /// is still `scheduled`, and [`Self::claim_due`]'s claim is the same
    /// single-writer conditional update, so exactly one of the two wins. A
    /// cancel that lost returns [`Error::AlreadyExists`] rather than racing —
    /// prd.md's "after the deadline returns `AlreadySent`".
    ///
    /// Canceling an already-canceled row succeeds and returns it unchanged;
    /// undo is a thing users press twice.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] if no row has `id`; [`Error::AlreadyExists`] if it
    /// is already `sending` or `sent`; [`Error::FailedPrecondition`] if it is
    /// `failed`. Otherwise a mapped storage error.
    #[tracing::instrument(skip(self))]
    pub async fn cancel(&self, id: i64) -> Result<OutboxEntry, Error> {
        let changed = self
            .db
            .write(move |conn| {
                conn.execute(
                    "UPDATE outbox SET state = 'canceled', lease_expires_at = NULL,
                         leased_by = NULL, next_attempt_at = NULL, updated_at = unixepoch()
                     WHERE id = ?1 AND state = 'scheduled'",
                    [id],
                )
            })
            .await?;

        let entry = self.get(id).await?;
        if changed == 0 {
            return Err(match entry.state {
                // Idempotent: a second press of undo is not an error.
                OutboxState::Canceled => return Ok(entry),
                OutboxState::Sending | OutboxState::Sent => Error::already_exists(format!(
                    "outbox entry {id} is already being sent and can no longer be canceled"
                )),
                OutboxState::Failed => Error::failed_precondition(format!(
                    "outbox entry {id} has failed; cancel does not apply (delete or retry it)"
                )),
                // Cancelling would claim the message was stopped, and it may
                // already have been delivered. Say so rather than implying
                // either outcome.
                OutboxState::Uncertain => Error::failed_precondition(format!(
                    "outbox entry {id} may or may not have been delivered; cancel cannot \
                     un-send it. Check the recipient, then retry it or discard it."
                )),
                // Unreachable in practice: the UPDATE above matches exactly
                // this state. Reported rather than asserted, because the only
                // way to get here is another writer resurrecting the row
                // between the update and the read.
                OutboxState::Scheduled => Error::failed_precondition(format!(
                    "outbox entry {id} changed underneath the cancel; retry it"
                )),
            });
        }
        tracing::info!(outbox_id = id, "scheduled send canceled");
        self.publish(&entry);
        Ok(entry)
    }

    /// The newest still-cancelable entry, for a bare `mail undo`.
    ///
    /// Prefers the row with the latest `undo_deadline` — the send whose
    /// countdown a user is watching — and falls back to the most recently
    /// created scheduled row when no undo window is open.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] if nothing is cancelable; otherwise a mapped
    /// storage error.
    #[tracing::instrument(skip(self))]
    pub async fn newest_cancelable(&self, account_id: Option<i64>) -> Result<OutboxEntry, Error> {
        let row = self
            .db
            .read(move |conn| {
                conn.query_row(
                    &format!(
                        "SELECT {COLUMNS} FROM outbox
                         WHERE state = 'scheduled' AND (?1 IS NULL OR account_id = ?1)
                         ORDER BY undo_deadline IS NULL, undo_deadline DESC,
                                  created_at DESC, id DESC
                         LIMIT 1"
                    ),
                    [account_id],
                    entry_from_row,
                )
                .optional()
            })
            .await?
            .ok_or_else(|| Error::not_found("nothing in the outbox can be canceled"))?;
        row.try_into()
    }

    /// Move a scheduled send to a new absolute instant.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] if no row has `id`; [`Error::AlreadyExists`] if it
    /// has already been claimed or sent; [`Error::FailedPrecondition`] for a
    /// canceled or failed row. Otherwise a mapped storage error.
    #[tracing::instrument(skip(self))]
    pub async fn reschedule(
        &self,
        id: i64,
        send_at: i64,
        tz: &str,
        ai_floor_secs: i64,
    ) -> Result<OutboxEntry, Error> {
        let tz = tz.to_owned();
        let changed = self
            .db
            .write(move |conn| {
                conn.execute(
                    // MAX against the floor rather than trusting `send_at`:
                    // `RescheduleSend { send_at: 0 }` was the same one-RPC
                    // bypass `send_now` had. See that method's docs.
                    "UPDATE outbox SET
                         send_at = MAX(?2, unixepoch()
                             + CASE WHEN origin = 'ai' THEN ?4 ELSE 0 END),
                         tz = ?3,
                         next_attempt_at = NULL,
                         undo_deadline = CASE WHEN origin = 'ai' AND ?4 > 0
                             THEN MAX(?2, unixepoch() + ?4) ELSE NULL END,
                         updated_at = unixepoch()
                     WHERE id = ?1 AND state = 'scheduled'",
                    rusqlite::params![id, send_at, tz, ai_floor_secs],
                )
            })
            .await?;
        let entry = self.get(id).await?;
        if changed == 0 {
            return Err(not_scheduled(id, entry.state, "rescheduled"));
        }
        self.publish(&entry);
        self.wake.notify_one();
        Ok(entry)
    }

    /// Re-render a scheduled send's body from its draft.
    ///
    /// Only rows that came from a draft can be edited: `raw_mime` is frozen
    /// octets, not an editable document, and rebuilding a message from the
    /// outbox row's own columns would silently drop the attachments and the
    /// HTML alternative that only the draft still holds. A row scheduled from
    /// inline fields is therefore refused rather than quietly downgraded —
    /// cancel it and compose again.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] if no row has `id`; [`Error::AlreadyExists`] if it
    /// has already been claimed or sent; [`Error::FailedPrecondition`] if it
    /// has no surviving draft. Otherwise as [`DraftStore::render`].
    #[tracing::instrument(skip(self, body_text))]
    pub async fn update_body(&self, id: i64, body_text: String) -> Result<OutboxEntry, Error> {
        let entry = self.get(id).await?;
        if entry.state != OutboxState::Scheduled {
            return Err(not_scheduled(id, entry.state, "edited"));
        }
        let draft_id = entry.draft_id.ok_or_else(|| {
            Error::failed_precondition(format!(
                "outbox entry {id} was not scheduled from a draft, so its body cannot be \
                 edited in place; cancel it and compose a new message"
            ))
        })?;

        self.drafts
            .update(
                draft_id,
                DraftPatch {
                    body_text: Some(body_text),
                    ..DraftPatch::default()
                },
            )
            .await?;
        let rendered = self.drafts.render(draft_id).await?;
        let draft = self.drafts.get(draft_id).await?;
        let preview = truncate_preview(&draft.body_text);

        // Conditional on `scheduled` for the same reason `cancel` is: a claim
        // that landed between the read above and this write must not have the
        // octets swapped underneath it mid-transmission.
        let changed = self
            .db
            .write(move |conn| {
                conn.execute(
                    "UPDATE outbox SET raw_mime = ?2, body_preview = ?3, updated_at = unixepoch()
                     WHERE id = ?1 AND state = 'scheduled'",
                    rusqlite::params![id, rendered.mime, preview],
                )
            })
            .await?;
        let entry = self.get(id).await?;
        if changed == 0 {
            return Err(not_scheduled(id, entry.state, "edited"));
        }
        self.publish(&entry);
        Ok(entry)
    }

    /// Make a scheduled send due immediately — subject to the mandatory undo
    /// floor for the row's own origin.
    ///
    /// `ai_floor_secs` is [`SendPolicy::mandatory_undo_window`] for
    /// [`Origin::Ai`]. It is applied here, in SQL, against the row's stored
    /// `origin`, because that is the only way to make the decision atomic: a
    /// read-then-clamp-then-write would race the scheduler claiming the row.
    ///
    /// Without this, `SendNow` was a one-RPC bypass of the guarantee that an
    /// AI-originated send always gets a window in which a human can stop it —
    /// schedule with `origin=ai`, then immediately `SendNow`, and it went out
    /// at once. `ScheduleSend` had closed the "send_at = now" and
    /// "undo_window_secs = 0" versions of that bypass; this is the same
    /// bypass wearing a third hat.
    ///
    /// # Errors
    ///
    /// As [`Self::reschedule`].
    #[tracing::instrument(skip(self))]
    pub async fn send_now(&self, id: i64, ai_floor_secs: i64) -> Result<OutboxEntry, Error> {
        let changed = self
            .db
            .write(move |conn| {
                conn.execute(
                    "UPDATE outbox SET
                         send_at = unixepoch()
                             + CASE WHEN origin = 'ai' THEN ?2 ELSE 0 END,
                         next_attempt_at = NULL,
                         undo_deadline = CASE WHEN origin = 'ai' AND ?2 > 0
                             THEN unixepoch() + ?2 ELSE NULL END,
                         updated_at = unixepoch()
                     WHERE id = ?1 AND state = 'scheduled'",
                    rusqlite::params![id, ai_floor_secs],
                )
            })
            .await?;
        let entry = self.get(id).await?;
        if changed == 0 {
            return Err(not_scheduled(id, entry.state, "sent now"));
        }
        self.publish(&entry);
        self.wake.notify_one();
        Ok(entry)
    }

    /// Return a failed send to the queue with a fresh attempt budget.
    ///
    /// The fence is cleared as part of the same statement. It is already NULL
    /// on every path that produces a `failed` row — a returned error proves
    /// nothing was queued — but clearing it here is what makes that an
    /// invariant of the *retry* rather than a fact one has to go and check
    /// about three other functions.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] if no row has `id`;
    /// [`Error::FailedPrecondition`] if it is not `failed`. Otherwise a
    /// mapped storage error.
    #[tracing::instrument(skip(self))]
    pub async fn retry(&self, id: i64) -> Result<OutboxEntry, Error> {
        let changed = self
            .db
            .write(move |conn| {
                conn.execute(
                    "UPDATE outbox SET state = 'scheduled', attempts = 0, next_attempt_at = NULL,
                         send_at = unixepoch(), last_error = NULL, smtp_message_id = NULL,
                         lease_expires_at = NULL, leased_by = NULL, updated_at = unixepoch()
                     WHERE id = ?1 AND state = 'failed'",
                    [id],
                )
            })
            .await?;
        let entry = self.get(id).await?;
        if changed == 0 {
            return Err(Error::failed_precondition(format!(
                "outbox entry {id} is {}, not failed; only a failed send can be retried",
                entry.state.as_str()
            )));
        }
        tracing::info!(outbox_id = id, "failed send returned to the queue");
        self.publish(&entry);
        self.wake.notify_one();
        Ok(entry)
    }

    /// Claim up to `limit` due rows, leasing them to `worker`.
    ///
    /// Due means `state = 'scheduled'`, `send_at <= now`, and any backoff
    /// spent. Selection and claim share one transaction over the single
    /// writer connection, so two workers polling at the same instant cannot
    /// both take the same row — and neither can race a concurrent
    /// [`Self::cancel`].
    ///
    /// # Errors
    ///
    /// A mapped storage error, or [`Error::Internal`] if a claimed row's
    /// stored octets carry no `Message-ID` (which [`Self::schedule`] rejects,
    /// so it means the row was written by something other than this code).
    #[tracing::instrument(skip(self), fields(claimed))]
    pub async fn claim_due(
        &self,
        worker: &str,
        limit: i64,
        now: i64,
        lease: Duration,
    ) -> Result<Vec<ClaimedSend>, Error> {
        if limit <= 0 {
            return Ok(Vec::new());
        }
        let worker = worker.to_owned();
        let lease_secs = i64::try_from(lease.as_secs()).unwrap_or(i64::MAX);
        let rows = self
            .db
            .write(move |conn| {
                let tx = conn.transaction()?;
                let candidates: Vec<i64> = {
                    let mut stmt = tx.prepare(
                        "SELECT id FROM outbox
                         WHERE state = 'scheduled' AND send_at <= ?1
                           AND (next_attempt_at IS NULL OR next_attempt_at <= ?1)
                         ORDER BY send_at, id LIMIT ?2",
                    )?;
                    let rows = stmt
                        .query_map(rusqlite::params![now, limit], |row| row.get(0))?
                        .collect::<rusqlite::Result<Vec<i64>>>()?;
                    rows
                };
                let mut claimed = Vec::with_capacity(candidates.len());
                {
                    let mut claim = tx.prepare(
                        "UPDATE outbox
                         SET state = 'sending', attempts = attempts + 1,
                             lease_expires_at = ?2, leased_by = ?3, updated_at = unixepoch()
                         WHERE id = ?1 AND state = 'scheduled'
                         RETURNING id, account_id, from_addr, to_addrs, cc_addrs, bcc_addrs,
                                   raw_mime, smtp_message_id, send_at, attempts, max_retries,
                                   origin, leased_by",
                    )?;
                    for id in candidates {
                        let row = claim.query_row(
                            rusqlite::params![id, now.saturating_add(lease_secs), worker],
                            claimed_from_row,
                        )?;
                        claimed.push(row);
                    }
                }
                tx.commit()?;
                Ok(claimed)
            })
            .await?;

        let claimed: Result<Vec<ClaimedSend>, Error> =
            rows.into_iter().map(TryInto::try_into).collect();
        let claimed = claimed?;
        tracing::Span::current().record("claimed", claimed.len());
        for entry in &claimed {
            if let Ok(entry) = self.get(entry.id).await {
                self.publish(&entry);
            }
        }
        Ok(claimed)
    }

    /// Commit the at-most-once fence, **before** `DATA`.
    ///
    /// Returns whether the lease still held. This is its own committed write
    /// rather than part of the claim, because the guarantee is temporal: the
    /// fence must survive this process dying before any octet reaches the
    /// peer, and a
    /// write that shares the claim's transaction would still be correct only
    /// by accident of ordering.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    #[tracing::instrument(skip(self, claimed), fields(outbox_id = claimed.id))]
    pub async fn begin_transmit(&self, claimed: &ClaimedSend) -> Result<bool, Error> {
        let (id, worker, message_id) = (
            claimed.id,
            claimed.worker.clone(),
            claimed.message_id.clone(),
        );
        let held = self
            .db
            .write(move |conn| {
                conn.execute(
                    "UPDATE outbox SET smtp_message_id = ?3, updated_at = unixepoch()
                     WHERE id = ?1 AND state = 'sending' AND leased_by = ?2",
                    rusqlite::params![id, worker, message_id],
                )
            })
            .await?
            > 0;
        if !held {
            tracing::warn!(
                outbox_id = id,
                "the lease lapsed before DATA; another worker owns this send"
            );
        }
        Ok(held)
    }

    /// Record a successful transmission.
    ///
    /// `late` marks prd.md's "sent late (was offline)" — the message went out
    /// past `send.late_tolerance` because rmail was not running when it came
    /// due. It still went out.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    #[tracing::instrument(skip(self, claimed), fields(outbox_id = claimed.id))]
    pub async fn mark_sent(&self, claimed: &ClaimedSend, late: bool) -> Result<bool, Error> {
        let (id, worker) = (claimed.id, claimed.worker.clone());
        let held = self
            .db
            .write(move |conn| {
                conn.execute(
                    "UPDATE outbox SET state = 'sent', sent_at = unixepoch(), sent_late = ?3,
                         lease_expires_at = NULL, leased_by = NULL, last_error = NULL,
                         next_attempt_at = NULL, updated_at = unixepoch()
                     WHERE id = ?1 AND state = 'sending' AND leased_by = ?2",
                    rusqlite::params![id, worker, i64::from(late)],
                )
            })
            .await?
            > 0;
        if held {
            tracing::info!(outbox_id = id, late, "message sent");
            self.publish_id(id).await;
        }
        Ok(held)
    }

    /// Close out a row whose fence was already committed by a previous,
    /// vanished attempt.
    ///
    /// The at-most-once branch: a copy may be on the wire, so this marks the
    /// row `sent` **without transmitting**, and records why in `last_error`
    /// so the user can see that this one was recovered rather than confirmed.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    #[tracing::instrument(skip(self, claimed), fields(outbox_id = claimed.id))]
    pub async fn mark_recovered(&self, claimed: &ClaimedSend) -> Result<bool, Error> {
        let (id, worker) = (claimed.id, claimed.worker.clone());
        let held = self
            .db
            .write(move |conn| {
                conn.execute(
                    "UPDATE outbox SET state = 'sent', sent_at = unixepoch(),
                         lease_expires_at = NULL, leased_by = NULL, next_attempt_at = NULL,
                         last_error = ?3, updated_at = unixepoch()
                     WHERE id = ?1 AND state = 'sending' AND leased_by = ?2",
                    rusqlite::params![id, worker, RECOVERED_NOTE],
                )
            })
            .await?
            > 0;
        if held {
            tracing::warn!(
                outbox_id = id,
                message_id = %claimed.message_id,
                "a previous attempt committed this Message-ID before DATA; treating it as \
                 sent rather than delivering a second copy"
            );
            self.publish_id(id).await;
        }
        Ok(held)
    }

    /// Record an indeterminate outcome: the session died without a reply.
    ///
    /// **Keeps the fence.** This is the whole point of the method existing
    /// separately from [`Self::mark_transient_failure`], which clears it: if
    /// the peer had already accepted the message and only its `250` was lost,
    /// clearing the fence and rescheduling delivers a second copy to the
    /// recipient. Nothing on this side can tell that apart from "it never
    /// arrived", so the row stops here, keeps the `Message-ID` it committed,
    /// and waits for a human rather than guessing in either direction.
    ///
    /// Returns `None` if the lease no longer held.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    #[tracing::instrument(skip(self, claimed, error), fields(outbox_id = claimed.id))]
    pub async fn mark_indeterminate(
        &self,
        claimed: &ClaimedSend,
        error: &str,
        now: i64,
    ) -> Result<Option<()>, Error> {
        let (id, worker) = (claimed.id, claimed.worker.clone());
        let error = truncate_error(error);
        let logged = error.clone();
        let _ = now;
        let changed = self
            .db
            .write(move |conn| {
                conn.execute(
                    "UPDATE outbox SET state = 'uncertain',
                         lease_expires_at = NULL, leased_by = NULL, next_attempt_at = NULL,
                         last_error = ?3, updated_at = unixepoch()
                     WHERE id = ?1 AND state = 'sending' AND leased_by = ?2",
                    rusqlite::params![id, worker, error],
                )
            })
            .await?;
        if changed == 0 {
            return Ok(None);
        }
        tracing::error!(
            outbox_id = id,
            message_id = %claimed.message_id,
            error = %logged,
            "the SMTP session died without a reply; this send may or may not have been \
             delivered. Leaving it uncertain with its fence intact rather than retrying \
             (which could deliver a second copy) or marking it sent (which could claim a \
             delivery that never happened)."
        );
        self.publish_id(id).await;
        Ok(Some(()))
    }

    /// Record a transient failure: back off and stay `scheduled`, or give up
    /// once `max_retries` is spent.
    ///
    /// Clears the fence. A returned SMTP error means the peer answered and
    /// queued nothing, so re-transmitting is not a duplicate — this is the
    /// branch that keeps retries possible at all (see the module docs).
    ///
    /// Returns `None` if the lease no longer held.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    #[tracing::instrument(skip(self, claimed, error), fields(outbox_id = claimed.id))]
    pub async fn mark_transient_failure(
        &self,
        claimed: &ClaimedSend,
        error: &str,
        backoff: Duration,
        now: i64,
    ) -> Result<Option<RetryOutcome>, Error> {
        let (id, worker) = (claimed.id, claimed.worker.clone());
        let error = truncate_error(error);
        let logged = error.clone();
        let exhausted = claimed.attempts >= claimed.max_retries;
        let next_attempt_at =
            now.saturating_add(i64::try_from(backoff.as_secs()).unwrap_or(i64::MAX));
        let attempts = claimed.attempts;

        let outcome = self
            .db
            .write(move |conn| {
                let changed = if exhausted {
                    conn.execute(
                        "UPDATE outbox SET state = 'failed', smtp_message_id = NULL,
                             lease_expires_at = NULL, leased_by = NULL, next_attempt_at = NULL,
                             last_error = ?3, updated_at = unixepoch()
                         WHERE id = ?1 AND state = 'sending' AND leased_by = ?2",
                        rusqlite::params![id, worker, error],
                    )?
                } else {
                    conn.execute(
                        "UPDATE outbox SET state = 'scheduled', smtp_message_id = NULL,
                             lease_expires_at = NULL, leased_by = NULL, next_attempt_at = ?3,
                             last_error = ?4, updated_at = unixepoch()
                         WHERE id = ?1 AND state = 'sending' AND leased_by = ?2",
                        rusqlite::params![id, worker, next_attempt_at, error],
                    )?
                };
                if changed == 0 {
                    return Ok(None);
                }
                Ok(Some(if exhausted {
                    RetryOutcome::Exhausted { attempts }
                } else {
                    RetryOutcome::Retrying {
                        next_attempt_at,
                        attempts,
                    }
                }))
            })
            .await?;

        match &outcome {
            Some(RetryOutcome::Exhausted { attempts }) => tracing::warn!(
                outbox_id = id,
                attempts,
                error = %logged,
                "send failed after exhausting its retries"
            ),
            Some(RetryOutcome::Retrying {
                next_attempt_at,
                attempts,
            }) => tracing::info!(
                outbox_id = id,
                attempts,
                next_attempt_at,
                error = %logged,
                "send failed transiently and will be retried"
            ),
            None => tracing::warn!(outbox_id = id, "failed a send this worker no longer holds"),
        }
        if outcome.is_some() {
            self.publish_id(id).await;
        }
        Ok(outcome)
    }

    /// Record a permanent failure: `failed`, never retried automatically.
    ///
    /// Clears the fence for the reason [`Self::mark_transient_failure`]
    /// gives.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    #[tracing::instrument(skip(self, claimed, error), fields(outbox_id = claimed.id))]
    pub async fn mark_permanent_failure(
        &self,
        claimed: &ClaimedSend,
        error: &str,
    ) -> Result<bool, Error> {
        let (id, worker) = (claimed.id, claimed.worker.clone());
        let error = truncate_error(error);
        let logged = error.clone();
        let held = self
            .db
            .write(move |conn| {
                conn.execute(
                    "UPDATE outbox SET state = 'failed', smtp_message_id = NULL,
                         lease_expires_at = NULL, leased_by = NULL, next_attempt_at = NULL,
                         last_error = ?3, updated_at = unixepoch()
                     WHERE id = ?1 AND state = 'sending' AND leased_by = ?2",
                    rusqlite::params![id, worker, error],
                )
            })
            .await?
            > 0;
        if held {
            tracing::warn!(outbox_id = id, error = %logged, "send rejected permanently");
            self.publish_id(id).await;
        }
        Ok(held)
    }

    /// Return rows whose lease has lapsed to `scheduled`.
    ///
    /// The fence is deliberately **not** cleared: a lapsed lease means the
    /// worker vanished, which is the one case where a `DATA` may have
    /// completed with nobody left to record it. Leaving the fence is what
    /// lets the next claim recognise the situation (see [`ClaimedSend`] and
    /// the module docs) instead of delivering a second copy.
    ///
    /// The attempt count is not rolled back — a send that repeatedly kills
    /// its worker should still eventually exhaust its budget.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    #[tracing::instrument(skip(self))]
    pub async fn reap_expired(&self, now: i64) -> Result<u64, Error> {
        let ids: Vec<i64> = self
            .db
            .write(move |conn| {
                let mut stmt = conn.prepare(
                    // The CASE is what bounds this. A row whose worker dies
                    // *before* `begin_transmit` never reaches
                    // `mark_transient_failure`, which is where the attempt
                    // budget is otherwise spent -- so without it, a send that
                    // reliably kills its worker is reclaimed, re-leased, and
                    // killed again forever, one round per lease, with
                    // `attempts` climbing and nothing ever looking at it.
                    // (The post-fence crash already terminates, via
                    // `mark_recovered`.)
                    "UPDATE outbox
                     SET state = CASE WHEN attempts >= max_retries THEN 'failed'
                                      ELSE 'scheduled' END,
                         lease_expires_at = NULL, leased_by = NULL,
                         next_attempt_at = NULL,
                         last_error = CASE WHEN attempts >= max_retries
                             THEN 'the sending worker vanished repeatedly; out of attempts'
                             ELSE 'the sending worker vanished; lease expired' END,
                         updated_at = unixepoch()
                     WHERE state = 'sending' AND lease_expires_at <= ?1
                     RETURNING id",
                )?;
                let ids = stmt
                    .query_map([now], |row| row.get(0))?
                    .collect::<rusqlite::Result<Vec<i64>>>()?;
                Ok(ids)
            })
            .await?;
        if !ids.is_empty() {
            tracing::warn!(
                reclaimed = ids.len(),
                "reclaimed outbox rows from expired leases"
            );
            for id in &ids {
                self.publish_id(*id).await;
            }
        }
        Ok(ids.len() as u64)
    }

    /// The earliest `send_at` (or backoff expiry) still outstanding.
    ///
    /// This is what turns the scheduler into a sleep rather than a poll: with
    /// nothing due for six hours it sleeps for `poll_interval` and no longer,
    /// and with something due in two seconds it sleeps for two seconds.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    pub async fn next_due_at(&self) -> Result<Option<i64>, Error> {
        Ok(self
            .db
            .read(|conn| {
                conn.query_row(
                    "SELECT MIN(MAX(send_at, COALESCE(next_attempt_at, send_at)))
                     FROM outbox WHERE state = 'scheduled'",
                    [],
                    |row| row.get::<_, Option<i64>>(0),
                )
                .optional()
            })
            .await?
            .flatten())
    }

    /// Broadcast an entry to `WatchOutbox` subscribers.
    ///
    /// A send error means nobody is listening, which is the normal case.
    fn publish(&self, entry: &OutboxEntry) {
        let _ = self.changes.send(OutboxChange {
            entry: entry.clone(),
        });
    }

    /// Re-read a row and broadcast it.
    ///
    /// A read failure here loses a notification, never a transition — the row
    /// is already committed — so it is logged rather than propagated: a
    /// transient pool error must not turn a successful send into a failed
    /// one.
    async fn publish_id(&self, id: i64) {
        match self.get(id).await {
            Ok(entry) => self.publish(&entry),
            Err(error) => {
                tracing::debug!(outbox_id = id, %error, "could not broadcast an outbox change");
            }
        }
    }
}

/// The note left on a row closed out by the at-most-once recovery path.
pub const RECOVERED_NOTE: &str =
    "recovered: this message's Message-ID was committed before DATA by an attempt that did \
     not finish, so it was not sent again";

/// Longest `last_error` retained, in characters.
///
/// An SMTP rejection can carry an arbitrarily long human message, and this
/// column is displayed in a list.
const MAX_ERROR_CHARS: usize = 500;

/// The `outbox` columns [`entry_from_row`] reads, in order.
const COLUMNS: &str = "id, account_id, draft_id, from_addr, to_addrs, cc_addrs, bcc_addrs, \
     subject, body_preview, in_reply_to, thread_id, send_at, tz, state, origin, attempts, \
     max_retries, next_attempt_at, last_error, smtp_message_id, sent_at, sent_late, \
     undo_deadline, created_at, updated_at";

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// The `Message-ID` carried by a rendered message, bare (no angle brackets).
///
/// A deliberate hand-rolled scan of the header block rather than a full parse:
/// this runs on the send path for every attempt, the answer is the fence the
/// at-most-once guarantee rests on, and a full MIME parse would decode
/// megabytes of body to answer a question about the first few hundred bytes.
/// Folded continuation lines are joined, per RFC 5322 §2.2.3.
#[must_use]
pub fn message_id_of(raw: &[u8]) -> Option<String> {
    let mut value: Option<String> = None;
    for line in raw.split(|b| *b == b'\n') {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        // The header block ends at the first empty line; a Message-ID after
        // it is body text that happens to look like a header.
        if line.is_empty() {
            break;
        }
        match &mut value {
            // Already reading the value: a leading space/tab continues it,
            // anything else ends it.
            Some(collected) => {
                if line.first().is_some_and(|b| *b == b' ' || *b == b'\t') {
                    collected.push_str(&String::from_utf8_lossy(line));
                    continue;
                }
                break;
            }
            None => {
                let Some(colon) = line.iter().position(|b| *b == b':') else {
                    continue;
                };
                let (name, rest) = line.split_at(colon);
                if !name.eq_ignore_ascii_case(b"Message-ID") {
                    continue;
                }
                value = Some(String::from_utf8_lossy(&rest[1..]).into_owned());
            }
        }
    }
    let value = value?;
    let trimmed = value.trim();
    let bare = trimmed
        .strip_prefix('<')
        .and_then(|v| v.strip_suffix('>'))
        .unwrap_or(trimmed)
        .trim();
    (!bare.is_empty()).then(|| bare.to_owned())
}

/// A message whose recipients and body were named inline, rather than by
/// pointing at a stored draft.
#[derive(Debug, Clone)]
pub struct InlineMessage {
    /// Owning account.
    pub account_id: i64,
    /// The sending identity.
    pub from: Mailbox,
    /// `To` recipients, in author order.
    pub to: Vec<Mailbox>,
    /// `Cc` recipients.
    pub cc: Vec<Mailbox>,
    /// `Bcc` recipients. Never rendered as a header.
    pub bcc: Vec<Mailbox>,
    /// Subject, decoded.
    pub subject: String,
    /// Plain-text body.
    pub body_text: String,
    /// The parent's `Message-ID`, bare, if this is a reply.
    pub in_reply_to: Option<String>,
    /// The `References` chain to carry.
    pub references: Vec<String>,
}

/// Build an in-memory [`Draft`] from an inline message, so it can go through
/// the one renderer.
///
/// Shares [`crate::compose::mime::build`] with the draft path rather than
/// having a second renderer: there is exactly one definition of "the octets
/// rmail transmits", and it lives in `compose::mime`.
///
/// # Errors
///
/// [`Error::InvalidArgument`] if no recipient is named.
pub fn inline_draft(message: InlineMessage) -> Result<Draft, Error> {
    if message.to.is_empty() && message.cc.is_empty() && message.bcc.is_empty() {
        return Err(Error::invalid_argument(
            "a scheduled send needs at least one To/Cc/Bcc recipient",
        ));
    }
    let now = chrono::Utc::now().timestamp();
    Ok(Draft {
        // Never persisted — this value exists only to be rendered, and
        // `mime::build` does not read the id.
        id: 0,
        account_id: message.account_id,
        from: message.from,
        to: message.to,
        cc: message.cc,
        bcc: message.bcc,
        subject: message.subject,
        body_text: message.body_text,
        body_html: None,
        attachments: Vec::new(),
        in_reply_to_message_id: None,
        in_reply_to: message.in_reply_to,
        references: message.references,
        created_at: now,
        updated_at: now,
    })
}

/// Addresses as stored: newline-separated, empty string for none.
fn join_addrs(addrs: &[String]) -> String {
    addrs.join("\n")
}

/// The inverse of [`join_addrs`].
fn split_addrs(value: &str) -> Vec<String> {
    value
        .split('\n')
        .map(str::trim)
        .filter(|a| !a.is_empty())
        .map(str::to_owned)
        .collect()
}

fn truncate_preview(body: &str) -> String {
    truncate_chars(body.trim(), MAX_PREVIEW_CHARS)
}

fn truncate_error(error: &str) -> String {
    truncate_chars(error.trim(), MAX_ERROR_CHARS)
}

/// Truncate on a character boundary, never a byte one — a `String` sliced
/// mid-codepoint panics, and an SMTP rejection can be UTF-8.
fn truncate_chars(value: &str, max: usize) -> String {
    match value.char_indices().nth(max) {
        Some((byte, _)) => format!("{}…", &value[..byte]),
        None => value.to_owned(),
    }
}

/// The error a state-guarded mutation returns when the row moved on.
fn not_scheduled(id: i64, state: OutboxState, verb: &str) -> Error {
    match state {
        OutboxState::Sending | OutboxState::Sent => Error::already_exists(format!(
            "outbox entry {id} is already being sent and can no longer be {verb}"
        )),
        other => Error::failed_precondition(format!(
            "outbox entry {id} is {}, so it cannot be {verb}",
            other.as_str()
        )),
    }
}

// ---------------------------------------------------------------------------
// Row mapping
// ---------------------------------------------------------------------------

/// An `outbox` row before its wire strings are parsed.
struct RawEntry {
    id: i64,
    account_id: i64,
    draft_id: Option<i64>,
    from_addr: String,
    to_addrs: String,
    cc_addrs: String,
    bcc_addrs: String,
    subject: String,
    body_preview: String,
    in_reply_to: Option<String>,
    thread_id: Option<i64>,
    send_at: i64,
    tz: String,
    state: String,
    origin: String,
    attempts: i64,
    max_retries: i64,
    next_attempt_at: Option<i64>,
    last_error: Option<String>,
    smtp_message_id: Option<String>,
    sent_at: Option<i64>,
    sent_late: i64,
    undo_deadline: Option<i64>,
    created_at: i64,
    updated_at: i64,
}

impl TryFrom<RawEntry> for OutboxEntry {
    type Error = Error;

    fn try_from(raw: RawEntry) -> Result<Self, Error> {
        Ok(Self {
            id: raw.id,
            account_id: raw.account_id,
            draft_id: raw.draft_id,
            from_addr: raw.from_addr,
            to: split_addrs(&raw.to_addrs),
            cc: split_addrs(&raw.cc_addrs),
            bcc: split_addrs(&raw.bcc_addrs),
            subject: raw.subject,
            body_preview: raw.body_preview,
            in_reply_to: raw.in_reply_to,
            thread_id: raw.thread_id,
            send_at: raw.send_at,
            tz: raw.tz,
            state: OutboxState::parse(&raw.state)?,
            origin: Origin::parse(&raw.origin)?,
            attempts: raw.attempts,
            max_retries: raw.max_retries,
            next_attempt_at: raw.next_attempt_at,
            last_error: raw.last_error,
            smtp_message_id: raw.smtp_message_id,
            sent_at: raw.sent_at,
            sent_late: raw.sent_late != 0,
            undo_deadline: raw.undo_deadline,
            created_at: raw.created_at,
            updated_at: raw.updated_at,
        })
    }
}

fn entry_from_row(row: &Row<'_>) -> rusqlite::Result<RawEntry> {
    Ok(RawEntry {
        id: row.get(0)?,
        account_id: row.get(1)?,
        draft_id: row.get(2)?,
        from_addr: row.get(3)?,
        to_addrs: row.get(4)?,
        cc_addrs: row.get(5)?,
        bcc_addrs: row.get(6)?,
        subject: row.get(7)?,
        body_preview: row.get(8)?,
        in_reply_to: row.get(9)?,
        thread_id: row.get(10)?,
        send_at: row.get(11)?,
        tz: row.get(12)?,
        state: row.get(13)?,
        origin: row.get(14)?,
        attempts: row.get(15)?,
        max_retries: row.get(16)?,
        next_attempt_at: row.get(17)?,
        last_error: row.get(18)?,
        smtp_message_id: row.get(19)?,
        sent_at: row.get(20)?,
        sent_late: row.get(21)?,
        undo_deadline: row.get(22)?,
        created_at: row.get(23)?,
        updated_at: row.get(24)?,
    })
}

/// A claimed row before its wire strings are parsed.
struct RawClaim {
    id: i64,
    account_id: i64,
    from_addr: String,
    to_addrs: String,
    cc_addrs: String,
    bcc_addrs: String,
    raw_mime: Vec<u8>,
    smtp_message_id: Option<String>,
    send_at: i64,
    attempts: i64,
    max_retries: i64,
    origin: String,
    worker: String,
}

impl TryFrom<RawClaim> for ClaimedSend {
    type Error = Error;

    fn try_from(raw: RawClaim) -> Result<Self, Error> {
        let mut recipients: Vec<String> = Vec::new();
        for addr in split_addrs(&raw.to_addrs)
            .into_iter()
            .chain(split_addrs(&raw.cc_addrs))
            .chain(split_addrs(&raw.bcc_addrs))
        {
            if !recipients.contains(&addr) {
                recipients.push(addr);
            }
        }
        // `schedule` refuses a message without one, so this can only fail for
        // a row written by something other than this code — a corrupt
        // database, not a bad request.
        let message_id = message_id_of(&raw.raw_mime).ok_or_else(|| {
            Error::internal(format!(
                "outbox entry {} has no Message-ID in its stored octets",
                raw.id
            ))
        })?;
        Ok(Self {
            id: raw.id,
            account_id: raw.account_id,
            worker: raw.worker,
            envelope: SendEnvelope {
                from: raw.from_addr,
                to: recipients,
            },
            raw_mime: raw.raw_mime,
            message_id,
            committed_message_id: raw.smtp_message_id,
            send_at: raw.send_at,
            attempts: raw.attempts,
            max_retries: raw.max_retries,
            origin: Origin::parse(&raw.origin)?,
        })
    }
}

fn claimed_from_row(row: &Row<'_>) -> rusqlite::Result<RawClaim> {
    Ok(RawClaim {
        id: row.get(0)?,
        account_id: row.get(1)?,
        from_addr: row.get(2)?,
        to_addrs: row.get(3)?,
        cc_addrs: row.get(4)?,
        bcc_addrs: row.get(5)?,
        raw_mime: row.get(6)?,
        smtp_message_id: row.get(7)?,
        send_at: row.get(8)?,
        attempts: row.get(9)?,
        max_retries: row.get(10)?,
        origin: row.get(11)?,
        worker: row.get(12)?,
    })
}

#[cfg(test)]
mod tests;
