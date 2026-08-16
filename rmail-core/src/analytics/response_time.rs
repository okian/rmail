//! Response-time & SLA analytics (task 71, prd.md feature 58).
//!
//! "How fast do I answer, who do I keep waiting, and is it getting worse."
//! Everything is derived from headers already in the local mirror — `From`,
//! `Date`/`INTERNALDATE`, `In-Reply-To`, `References` — plus folder names. No
//! model is called and nothing is written.
//!
//! # What a pair is
//!
//! A *pair* is one reply matched to the message it answers, joined on the
//! RFC 5322 reference graph: the reply's `In-Reply-To`, falling back to the
//! last id in its `References` chain (oldest-first, so the last entry is the
//! immediate parent). The latency is the difference between the two
//! timestamps.
//!
//! The `References` arm is a fallback for a *missing* header, not for a
//! failed lookup. When `In-Reply-To` names a message this mailbox never
//! synced, the pair is dropped rather than re-aimed at an ancestor the chain
//! does happen to contain: the ancestor is older, so the latency measured
//! against it would be longer than the real one, and a fabricated slow
//! response is worse than a missing one in a report whose whole job is to say
//! how slow you are.
//!
//! Direction is decided by who wrote each end, against the set of addresses
//! that are *you* (see [`self_addresses`]):
//!
//! | reply from | original from | counted as |
//! |---|---|---|
//! | you | them | **ours** — how long you took |
//! | them | you | **theirs** — how long they took |
//! | you | you | dropped (a note to yourself) |
//! | them | them | dropped (two other people talking on a list) |
//!
//! Both halves matter. A median of four hours is fast against a
//! correspondent who answers in ten minutes and slow against one who answers
//! in three days, and "you are the bottleneck" is only meaningful relative to
//! the other side — which is why [`ResponseGroup`] carries both.
//!
//! # The window is on the reply, not on what it answers
//!
//! A pair belongs to `[since, until)` when the **reply** was sent inside it.
//! "How fast did I respond in March" is a claim about replies written in
//! March, whatever the age of the mail they answered — so the original may
//! (and often does) predate `since`, and the original lookup is deliberately
//! unbounded in time.
//!
//! # Pairing is header-exact; "unanswered" is thread-level
//!
//! These two answer different questions and are deliberately computed
//! differently. A latency needs to know *which* message was answered, so
//! pairing is exact. "Did this get answered at all" does not: in a thread
//! where they wrote four times and you replied once at the end, a
//! message-exact rule reports three abandoned messages and a human reports
//! one answered thread. So [`ResponseGroup::awaiting_reply`] asks whether
//! *any* message of yours in the same thread is newer than theirs — a
//! question asked over all time, not only inside the window, so mail received
//! on the window's last day and answered the day after is not reported as
//! abandoned.
//!
//! It is reported at all because a median computed only over the mail you
//! answered is survivorship bias — never replying is the fastest possible way
//! to have an excellent p50 — and because it is the one bottleneck shape
//! percentiles structurally cannot see.
//!
//! # Unanswered is not the same as late
//!
//! Every conversation ends with somebody's message, and about half of them
//! end with theirs. So "they spoke last and you have not answered" describes
//! most healthy correspondence, and a bottleneck flag built on it fires
//! everywhere and means nothing. [`ResponseGroup::overdue`] is the narrower
//! claim the flag actually uses, and it needs *two* things of a message:
//!
//! 1. It has been waiting longer than [`overdue_after`] — your own p90 for
//!    that group, floored at a day. "Longer than you normally take with this
//!    person" is the only definition of late that does not need a universal
//!    constant nobody would agree on, and it is the SLA half of this
//!    feature's name.
//! 2. Its **sender** is someone you replied to at least once in the window.
//!    A newsletter is not a relationship you are blocking.
//!
//! The second test is on the sender, not on the group, and that distinction
//! is load-bearing. Under [`GroupBy::Contact`] the two coincide. Under
//! [`GroupBy::Mailbox`] they do not: one group holds mail from everybody, so
//! a group-level "you do answer these people" clause is trivially true of
//! INBOX in every mailbox that has ever been replied to once — and every
//! folder in every real account would come back flagged.
//!
//! # Percentiles are nearest-rank
//!
//! p50/p90 are the observation at 1-based rank `ceil(p/100 * n)`, not an
//! interpolation between two neighbours. Every number this module reports is
//! therefore a latency that really occurred and can be traced back to a
//! specific message — which is what makes "your p90 with Acme is 3 days"
//! something a user can check rather than a statistic they have to trust.
//!
//! # Cost, and the ceiling on it
//!
//! The scans hold one small row per message in the window in memory (ids,
//! header ids, an address and a timestamp — never a body or a `raw` blob;
//! that is why this module has its own row type instead of reusing
//! [`crate::repo::Message`]). The window is a caller-chosen number, so that
//! is not a bound on its own: [`MAX_SCAN_ROWS`] is, and a query past it fails
//! with [`Error::ResourceExhausted`] naming the knob rather than returning a
//! quarter of a mailbox labelled as all of it.
//!
//! On indexes: the two windowed scans filter on `COALESCE(date,
//! internaldate)`, which `idx_messages_date_only` (V19) indexes, so the range
//! is served from the index and `account_id` is applied per row. The original
//! lookup is `message_id IN (…)` against `idx_messages_message_id`, chunked.
//! The answered-ness query is the one that could have been an all-time table
//! scan — its address predicate (`lower(trim(from_addr))`) is not indexable —
//! which is why it is bounded to the thread ids the inbound scan actually
//! produced and driven through `idx_messages_thread` instead.

#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};

use rusqlite::types::Value;
use rusqlite::Connection;
use tokio_util::sync::CancellationToken;

use crate::error::Error;
use crate::outbox::sent::looks_like_sent;
use crate::retrieve::cancel::interruptible_read;
use crate::storage::Database;

/// One day, in seconds. The unit every default below is expressed in.
const DAY: i64 = 86_400;

/// Default report span when the caller gives no `since`: the last 90 days.
pub const DEFAULT_RANGE_SECONDS: i64 = 90 * DAY;

/// Default trend step: one point per week.
pub const DEFAULT_BUCKET_SECONDS: i64 = 7 * DAY;

/// Default rolling window each trend point summarizes: four weeks.
pub const DEFAULT_WINDOW_SECONDS: i64 = 28 * DAY;

/// Default number of groups returned.
pub const DEFAULT_LIMIT: usize = 50;

/// Hard ceiling on groups returned in one report.
pub const MAX_LIMIT: usize = 500;

/// Default evidence bar for the bottleneck rule.
pub const DEFAULT_MIN_SAMPLES: u32 = 3;

/// Default "slower than them by this much" multiplier.
pub const DEFAULT_BOTTLENECK_RATIO: f64 = 2.0;

/// Most trend points one report may contain.
///
/// A daily point over a year. Past this the series stops being a trend and
/// starts being the raw data, so an over-wide range with an over-fine bucket
/// is rejected with advice rather than silently truncated — a truncated
/// series looks exactly like a mailbox that only has recent mail.
pub const MAX_TREND_POINTS: usize = 366;

/// Floor on how long an unanswered message must sit before it is *overdue*.
///
/// The threshold is normally the group's own p90 — "longer than you usually
/// take with this person" — but a correspondent you answer within minutes
/// would otherwise make every message overdue almost immediately. A day is
/// the point past which "I have not got to it yet" stops being a description
/// of a working day.
pub const OVERDUE_FLOOR_SECONDS: i64 = DAY;

/// How slow a median of yours must be before the "slower than them" arm will
/// fire at all.
///
/// The ratio alone is not enough when the other side is instantaneous. An
/// auto-responder has `p50 == 0`, and *any* positive median of yours beats
/// `0 * ratio`, so without a floor a two-second median would be reported as a
/// bottleneck. Nobody is blocking a conversation at that scale, whatever the
/// ratio says.
pub const MIN_SLOW_SECONDS: i64 = 15 * 60;

/// Most message rows one scan may materialize.
///
/// The scans below are bounded by the *window*, and a window is a caller-
/// chosen number: nothing stops a `mail.read` token asking for a decade of a
/// mailing-list archive, and this module holds one owned row per message
/// while it works. That is a way to take the daemon — and every other RPC
/// sharing it — down, so a scan that would exceed this fails with
/// [`Error::ResourceExhausted`] naming the knob rather than being silently
/// truncated into a report that looks complete.
///
/// Sized so an ordinary multi-year query on a personal mailbox never sees it,
/// and a run at the cap costs tens of megabytes rather than gigabytes.
pub const MAX_SCAN_ROWS: usize = 250_000;

/// Most distinct addresses treated as "you" for one report.
///
/// Sized for a real human's aliases many times over. The cap exists because
/// the Sent-folder half of [`self_addresses`] reads whatever is actually in
/// that folder, and an imported or mis-labelled archive can put a great many
/// senders there; without a bound, one bad folder would make every address in
/// it "you" and the whole report would collapse to nothing.
const MAX_SELF_ADDRESSES: usize = 256;

/// How many `message_id`s one parent-lookup statement binds at a time. Well
/// under SQLite's variable limit, and large enough that the round trips are
/// not what costs.
const ID_CHUNK: usize = 400;

/// Which dimension a report is grouped along.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GroupBy {
    /// One group per counterparty address.
    #[default]
    Contact,
    /// One group per folder the inbound side of a pair lives in.
    ///
    /// Never the Sent folder: a pair is keyed on whichever side was *waited
    /// for*, so this answers "which folders do I let rot", not "where do my
    /// replies end up".
    Mailbox,
}

/// What to report on.
#[derive(Debug, Clone, PartialEq)]
pub struct ResponseTimeQuery {
    /// Restrict to one account; `None` covers every configured account.
    pub account_id: Option<i64>,
    /// Group by contact or by folder.
    pub group_by: GroupBy,
    /// Window start, unix seconds, inclusive.
    pub since: i64,
    /// Window end, unix seconds, exclusive.
    pub until: i64,
    /// Trend step: one point every this many seconds.
    pub bucket_seconds: i64,
    /// Rolling span each trend point summarizes. Must be `>= bucket_seconds`.
    pub window_seconds: i64,
    /// Most groups to return, clamped to [`MAX_LIMIT`].
    pub limit: usize,
    /// Observations a group needs before the bottleneck rule fires on it.
    pub min_samples: u32,
    /// How many times theirs your median must be to count as slower.
    pub bottleneck_ratio: f64,
}

impl ResponseTimeQuery {
    /// A default query over the last [`DEFAULT_RANGE_SECONDS`] ending at
    /// `now`.
    #[must_use]
    pub fn ending_at(now: i64) -> Self {
        Self {
            account_id: None,
            group_by: GroupBy::Contact,
            since: now.saturating_sub(DEFAULT_RANGE_SECONDS),
            until: now,
            bucket_seconds: DEFAULT_BUCKET_SECONDS,
            window_seconds: DEFAULT_WINDOW_SECONDS,
            limit: DEFAULT_LIMIT,
            min_samples: DEFAULT_MIN_SAMPLES,
            bottleneck_ratio: DEFAULT_BOTTLENECK_RATIO,
        }
    }

    /// Reject a query that cannot produce a meaningful report, and clamp the
    /// ones where clamping is the kinder answer.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] for an empty or inverted window, a
    /// non-positive bucket, a rolling window shorter than the step it
    /// advances by, a bottleneck ratio below 1 (which would flag
    /// correspondences where the *user* is the faster side), or a
    /// bucket/range combination needing more than [`MAX_TREND_POINTS`]
    /// points.
    fn validate(&mut self) -> Result<(), Error> {
        if self.since >= self.until {
            return Err(Error::invalid_argument(format!(
                "since ({}) must be strictly before until ({})",
                self.since, self.until
            )));
        }
        if self.bucket_seconds <= 0 {
            return Err(Error::invalid_argument(
                "bucket_seconds must be positive".to_owned(),
            ));
        }
        if self.window_seconds < self.bucket_seconds {
            return Err(Error::invalid_argument(format!(
                "window_seconds ({}) must be at least bucket_seconds ({}); a rolling window \
                 shorter than the step it advances by would leave gaps between points",
                self.window_seconds, self.bucket_seconds
            )));
        }
        if !self.bottleneck_ratio.is_finite() || self.bottleneck_ratio < 1.0 {
            return Err(Error::invalid_argument(format!(
                "bottleneck_ratio ({}) must be a finite number >= 1.0",
                self.bottleneck_ratio
            )));
        }
        let points = self.trend_points();
        if points > MAX_TREND_POINTS {
            return Err(Error::invalid_argument(format!(
                "a {}s range at a {}s bucket needs {points} trend points (limit \
                 {MAX_TREND_POINTS}); widen bucket_seconds or narrow the range",
                self.until - self.since,
                self.bucket_seconds
            )));
        }
        self.limit = self.limit.clamp(1, MAX_LIMIT);
        self.min_samples = self.min_samples.max(1);
        Ok(())
    }

    /// How many trend points this window/bucket combination implies.
    ///
    /// Saturating rather than wrapping: `validate` calls this *before*
    /// rejecting an over-wide range, so it has to survive one.
    fn trend_points(&self) -> usize {
        // Unsigned throughout: `i64::div_ceil` is still unstable, and both
        // operands are known positive here (`validate` rejects an inverted
        // window and a non-positive bucket before this matters).
        let span = u64::try_from(self.until.saturating_sub(self.since).max(1)).unwrap_or(u64::MAX);
        let bucket = u64::try_from(self.bucket_seconds.max(1)).unwrap_or(1);
        usize::try_from(span.div_ceil(bucket)).unwrap_or(usize::MAX)
    }
}

/// A percentile summary of one set of latencies, in seconds.
///
/// With `samples == 0` every other field is zero and means nothing; check
/// `samples` before reading any of them.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Stats {
    /// How many latencies went into this summary.
    pub samples: u64,
    /// Median, by nearest rank.
    pub p50_seconds: i64,
    /// 90th percentile, by nearest rank.
    pub p90_seconds: i64,
    /// Arithmetic mean.
    pub mean_seconds: f64,
    /// Fastest observation.
    pub min_seconds: i64,
    /// Slowest observation.
    pub max_seconds: i64,
}

impl Stats {
    /// Summarize `latencies`, which **must already be sorted ascending**.
    ///
    /// Taking sorted input rather than sorting internally is deliberate: the
    /// rolling trend summarizes many overlapping slices of one sorted vector,
    /// and a function that sorted its own argument would either re-sort each
    /// slice or quietly need a copy.
    #[must_use]
    pub fn from_sorted(latencies: &[i64]) -> Self {
        let Some(&min_seconds) = latencies.first() else {
            return Self::default();
        };
        // `first`/`last` on a non-empty slice; the sort contract makes these
        // the extremes.
        let max_seconds = latencies.last().copied().unwrap_or(min_seconds);
        let n = latencies.len();
        let sum: i128 = latencies.iter().map(|&v| i128::from(v)).sum();
        Self {
            samples: n as u64,
            p50_seconds: percentile(latencies, 50).unwrap_or(0),
            p90_seconds: percentile(latencies, 90).unwrap_or(0),
            // `n` is non-zero here, and an i128 sum of i64s converts to f64
            // with at most the usual float rounding — which a mean is
            // reporting anyway.
            mean_seconds: (sum as f64) / (n as f64),
            min_seconds,
            max_seconds,
        }
    }
}

/// The nearest-rank percentile of a sorted, non-empty slice.
///
/// Rank is 1-based `ceil(p/100 * n)`, clamped into range; the result is
/// always an element of `latencies`, never an interpolation between two.
/// `None` for an empty slice — a percentile of nothing is not zero, it is
/// undefined, and returning zero here is how an empty group would end up
/// looking like an instant responder.
#[must_use]
pub fn percentile(latencies: &[i64], p: u32) -> Option<i64> {
    let n = latencies.len();
    if n == 0 {
        return None;
    }
    let rank = (u128::from(p) * n as u128).div_ceil(100).max(1);
    let index = usize::try_from(rank - 1).unwrap_or(usize::MAX).min(n - 1);
    latencies.get(index).copied()
}

/// One contact, or one folder.
#[derive(Debug, Clone, PartialEq)]
pub struct ResponseGroup {
    /// Lowercased address, or folder name.
    pub key: String,
    /// Display name if one is known, else the same text as `key`.
    pub label: String,
    /// The folder id when grouping by mailbox; `None` otherwise.
    pub mailbox_id: Option<i64>,
    /// How long you took to answer them.
    pub ours: Stats,
    /// How long they took to answer you.
    pub theirs: Stats,
    /// Inbound messages attributed here inside the window.
    pub inbound: u64,
    /// How many of those have no later message of yours in the same thread.
    pub awaiting_reply: u64,
    /// The subset of `awaiting_reply` that is genuinely late: waiting longer
    /// than [`overdue_after`], *and* from a sender you replied to at least
    /// once in the window. See the module docs on why the second clause is
    /// per-sender rather than per-group.
    pub overdue: u64,
    /// Whether you are the bottleneck. Exactly
    /// `slower_than_counterpart || stalled`.
    pub bottleneck: bool,
    /// Your median is at least `bottleneck_ratio` times theirs, on at least
    /// `min_samples` observations each side.
    pub slower_than_counterpart: bool,
    /// At least `min_samples` messages are [`overdue`](Self::overdue).
    pub stalled: bool,
}

/// One point of the rolling trend, covering `[window_start, window_end)`.
#[derive(Debug, Clone, PartialEq)]
pub struct TrendPoint {
    /// Clamped to the report's `since`, so early points cover a shorter span
    /// than asked for rather than reaching for data never read.
    pub window_start: i64,
    /// Exclusive end.
    pub window_end: i64,
    /// Your own response times only.
    pub stats: Stats,
}

/// A finished report.
#[derive(Debug, Clone, PartialEq)]
pub struct ResponseTimes {
    /// The resolved window start.
    pub since: i64,
    /// The resolved window end.
    pub until: i64,
    /// How `groups` is keyed.
    pub group_by: GroupBy,
    /// Every "ours" pair in the window, ungrouped.
    pub ours: Stats,
    /// Every "theirs" pair in the window, ungrouped.
    pub theirs: Stats,
    /// Groups, most-in-need-of-attention first, truncated to the query's
    /// limit.
    pub groups: Vec<ResponseGroup>,
    /// How many groups existed before truncation.
    pub total_groups: usize,
    /// The rolling trend, oldest point first.
    pub trend: Vec<TrendPoint>,
    /// The addresses treated as "you", sorted.
    pub self_addresses: Vec<String>,
    /// Pairs matched in the window, both directions.
    pub pairs: u64,
    /// Pairs dropped because the reply predates what it answers.
    pub skipped_out_of_order: u64,
}

/// Compute a response-time report.
///
/// # Errors
///
/// [`Error::InvalidArgument`] for a query [`ResponseTimeQuery::validate`]
/// rejects, [`Error::Cancelled`] if `cancel` fires while a scan is in flight
/// (a half-scanned mailbox must not be reported as a whole one), and a mapped
/// storage error otherwise.
#[tracing::instrument(
    skip(db, cancel),
    fields(
        account_id = ?query.account_id,
        since = query.since,
        until = query.until,
        pairs = tracing::field::Empty,
        groups = tracing::field::Empty,
    ),
    err
)]
pub async fn response_times(
    db: &Database,
    cancel: &CancellationToken,
    query: ResponseTimeQuery,
) -> Result<ResponseTimes, Error> {
    let mut query = query;
    query.validate()?;

    let mailboxes = load_mailboxes(db, cancel, query.account_id).await?;
    let self_addrs = self_addresses(db, cancel, query.account_id, &mailboxes).await?;

    let replies = load_replies(db, cancel, &query, &mailboxes).await?;
    let parents = load_parents(db, cancel, query.account_id, &replies).await?;
    let inbound = load_inbound(db, cancel, &query, &self_addrs, &mailboxes).await?;
    // After `load_inbound`, because the only threads whose answered-ness
    // matters are the ones holding a message in the window — which turns an
    // all-time full-table scan into an indexed lookup over a bounded id set.
    let last_ours_in_thread =
        load_last_ours_per_thread(db, cancel, &self_addrs, &mailboxes, &inbound).await?;

    let paired = pair_up(&replies, &parents, &self_addrs);
    tracing::Span::current().record("pairs", paired.pairs.len() as u64);

    let report = assemble(
        &query,
        &self_addrs,
        &mailboxes,
        paired,
        &inbound,
        &last_ours_in_thread,
    );
    tracing::Span::current().record("groups", report.total_groups as u64);
    Ok(report)
}

// ---------------------------------------------------------------------------
// Row shapes
// ---------------------------------------------------------------------------

/// The columns this module reads off `messages`. Deliberately not
/// [`crate::repo::Message`]: that one carries `raw`, `body_text` and
/// `body_html`, and a report over ninety days of mail must not pull three
/// bodies per row into memory to compute a subtraction.
#[derive(Debug, Clone)]
struct MessageRow {
    id: i64,
    account_id: i64,
    mailbox_id: i64,
    thread_id: Option<i64>,
    message_id: Option<String>,
    in_reply_to: Option<String>,
    references_hdr: Option<String>,
    /// Lowercased and trimmed; `None` when the header was absent or blank.
    from_addr: Option<String>,
    from_name: Option<String>,
    /// `COALESCE(date, internaldate)`; `None` when the message has neither.
    at: Option<i64>,
}

const MESSAGE_SELECT: &str = "SELECT id, account_id, mailbox_id, thread_id, message_id, \
     in_reply_to, references_hdr, from_addr, from_name, \
     COALESCE(date, internaldate) AS at FROM messages";

impl MessageRow {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        let from_addr: Option<String> = row.get("from_addr")?;
        Ok(Self {
            id: row.get("id")?,
            account_id: row.get("account_id")?,
            mailbox_id: row.get("mailbox_id")?,
            thread_id: row.get("thread_id")?,
            message_id: row
                .get::<_, Option<String>>("message_id")?
                .as_deref()
                .and_then(bare),
            in_reply_to: row.get("in_reply_to")?,
            references_hdr: row.get("references_hdr")?,
            from_addr: from_addr.as_deref().and_then(normalize_address),
            from_name: row.get("from_name")?,
            at: row.get("at")?,
        })
    }
}

/// A reply matched to the message it answers.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pair {
    /// Lowercased address of whichever side is not you.
    counterparty: String,
    /// That side's display name, if the message carried one.
    counterparty_name: Option<String>,
    /// The folder the *inbound* message of the pair lives in.
    mailbox_id: i64,
    /// Reply timestamp minus original timestamp; never negative.
    latency: i64,
    /// When the reply was sent — what the window and the trend key on.
    responded_at: i64,
    /// When the *inbound* side of the pair was sent. Distinct from
    /// `responded_at` for an `ours` pair, and it is the right clock for
    /// anything read off the inbound message (its display name): a stale name
    /// on a promptly-answered message must not beat a current one on a
    /// message that took a week.
    inbound_at: i64,
    /// Whether *you* wrote the reply.
    ours: bool,
}

/// The outcome of matching replies to originals.
#[derive(Debug, Default)]
struct Paired {
    pairs: Vec<Pair>,
    skipped_out_of_order: u64,
}

/// A folder, as this module needs it.
#[derive(Debug, Clone)]
struct MailboxRow {
    id: i64,
    name: String,
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Run one scan, turning a cancellation into an error rather than an empty
/// result.
///
/// [`interruptible_read`]'s `Ok(None)` means "this scan was interrupted",
/// which for a retriever legitimately reads as "no candidates from this
/// source". For a report it does not: a half-read mailbox summarized as if it
/// were the whole one is a wrong answer presented as a right one.
async fn scan<F, T>(
    db: &Database,
    cancel: &CancellationToken,
    stage: &'static str,
    f: F,
) -> Result<T, Error>
where
    F: FnOnce(&Connection) -> rusqlite::Result<T> + Send + 'static,
    T: Send + 'static,
{
    match interruptible_read(db, cancel, f).await? {
        Some(value) => Ok(value),
        None => Err(Error::cancelled(format!(
            "response-time analytics cancelled while reading {stage}"
        ))),
    }
}

/// Refuse a scan that came back over [`MAX_SCAN_ROWS`].
///
/// Every caller asks for `MAX_SCAN_ROWS + 1` rows, so a full slice means at
/// least one more exists. Erroring rather than truncating is the whole point:
/// a report over the first quarter-million rows of a window, labelled as a
/// report over the window, is a wrong answer that looks like a right one.
fn within_cap<T>(rows: Vec<T>, stage: &str) -> Result<Vec<T>, Error> {
    if rows.len() > MAX_SCAN_ROWS {
        return Err(Error::resource_exhausted(format!(
            "this window covers more than {MAX_SCAN_ROWS} {stage} rows; narrow `since`/`until` \
             or restrict to one account"
        )));
    }
    Ok(rows)
}

/// The rows of `mailboxes` whose names match `candidates`, as ids.
fn folder_ids(mailboxes: &[MailboxRow], candidates: &[&str]) -> Vec<i64> {
    mailboxes
        .iter()
        .filter(|mailbox| folder_is(&mailbox.name, candidates))
        .map(|mailbox| mailbox.id)
        .collect()
}

/// `AND col NOT IN (?, ?, …)`, or nothing at all for an empty exclusion set.
fn not_in_clause(column: &str, ids: &[i64]) -> String {
    if ids.is_empty() {
        return String::new();
    }
    let placeholders = vec!["?"; ids.len()].join(", ");
    format!(" AND {column} NOT IN ({placeholders})")
}

/// `col IN (?, ?, …)`.
fn in_clause(column: &str, len: usize) -> String {
    let placeholders = vec!["?"; len].join(", ");
    format!("{column} IN ({placeholders})")
}

/// Every folder in scope, for the mailbox grouping's labels and for finding
/// the Sent/Drafts/Trash folders.
async fn load_mailboxes(
    db: &Database,
    cancel: &CancellationToken,
    account_id: Option<i64>,
) -> Result<Vec<MailboxRow>, Error> {
    scan(db, cancel, "mailboxes", move |conn| {
        let mut stmt = conn.prepare(
            "SELECT id, name FROM mailboxes \
             WHERE (?1 IS NULL OR account_id = ?1) ORDER BY id",
        )?;
        let rows = stmt
            .query_map([account_id], |row| {
                Ok(MailboxRow {
                    id: row.get(0)?,
                    name: row.get(1)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })
    .await
}

/// The addresses that are *you*, per account.
///
/// Two sources, unioned:
///
/// 1. `accounts.username`, when it looks like an address at all. A login of
///    `alice` says nothing about what she sends as.
/// 2. Every distinct `From` in a folder [`looks_like_sent`] recognizes. This
///    is what catches aliases and `+tags`: rmail never has to be told about
///    an identity the user has actually sent from.
///
/// The result decides direction for every pair, so getting it wrong is not a
/// small error — an empty set makes the whole report empty, which is why
/// [`ResponseTimes::self_addresses`] hands it back to the caller instead of
/// keeping it private.
async fn self_addresses(
    db: &Database,
    cancel: &CancellationToken,
    account_id: Option<i64>,
    mailboxes: &[MailboxRow],
) -> Result<HashMap<i64, HashSet<String>>, Error> {
    let mut by_account: HashMap<i64, HashSet<String>> = HashMap::new();

    let usernames: Vec<(i64, Option<String>)> = scan(db, cancel, "accounts", move |conn| {
        let mut stmt =
            conn.prepare("SELECT id, username FROM accounts WHERE (?1 IS NULL OR id = ?1)")?;
        let rows = stmt
            .query_map([account_id], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })
    .await?;
    for (id, username) in usernames {
        let entry = by_account.entry(id).or_default();
        if let Some(address) = username
            .as_deref()
            .and_then(normalize_address)
            .filter(|u| u.contains('@'))
        {
            entry.insert(address);
        }
    }

    let sent: Vec<i64> = mailboxes
        .iter()
        .filter(|mailbox| looks_like_sent(&mailbox.name))
        .map(|mailbox| mailbox.id)
        .collect();
    for mailbox_id in sent {
        let senders: Vec<(i64, Option<String>)> = scan(db, cancel, "sent folders", move |conn| {
            let mut stmt = conn.prepare(
                "SELECT DISTINCT account_id, from_addr FROM messages \
                     WHERE mailbox_id = ?1 AND from_addr IS NOT NULL \
                     ORDER BY from_addr LIMIT ?2",
            )?;
            let rows = stmt
                .query_map(
                    rusqlite::params![mailbox_id, MAX_SELF_ADDRESSES as i64],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await?;
        for (account, address) in senders {
            let entry = by_account.entry(account).or_default();
            if entry.len() >= MAX_SELF_ADDRESSES {
                tracing::warn!(
                    account_id = account,
                    cap = MAX_SELF_ADDRESSES,
                    "more distinct senders in the Sent folder than the self-address cap; \
                     response-time direction may be wrong for the excess"
                );
                break;
            }
            if let Some(address) = address.as_deref().and_then(normalize_address) {
                entry.insert(address);
            }
        }
    }

    Ok(by_account)
}

/// Every message since `since` that answers something, deduplicated across
/// folders.
///
/// The same message present in two folders (a copy, or a Gmail label) is two
/// `messages` rows with one `Message-ID`. Counting both would double every
/// latency it contributes, so rows are collapsed on
/// `(account_id, message_id)`, keeping the lowest id — the copy that was
/// synced first. A row with no `Message-ID` cannot be deduplicated and is
/// kept as-is.
/// A message in a `Drafts` folder is excluded: it carries `In-Reply-To` and
/// your address exactly like a sent reply does, and counting one would report
/// a response you have not actually made.
async fn load_replies(
    db: &Database,
    cancel: &CancellationToken,
    query: &ResponseTimeQuery,
    mailboxes: &[MailboxRow],
) -> Result<Vec<MessageRow>, Error> {
    let account_id = query.account_id;
    let (since, until) = (query.since, query.until);
    let drafts = folder_ids(mailboxes, DRAFT_FOLDER_NAMES);
    let rows = scan(db, cancel, "replies", move |conn| {
        let excluded = not_in_clause("mailbox_id", &drafts);
        let sql = format!(
            "{MESSAGE_SELECT} WHERE (? IS NULL OR account_id = ?) \
             AND COALESCE(date, internaldate) >= ? \
             AND COALESCE(date, internaldate) < ? \
             AND (in_reply_to IS NOT NULL OR references_hdr IS NOT NULL) \
             {excluded} ORDER BY id LIMIT ?"
        );
        let mut params: Vec<Value> = vec![
            account_id.map_or(Value::Null, Value::Integer),
            account_id.map_or(Value::Null, Value::Integer),
            Value::Integer(since),
            Value::Integer(until),
        ];
        params.extend(drafts.iter().map(|id| Value::Integer(*id)));
        params.push(Value::Integer(scan_limit()));
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params), MessageRow::from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })
    .await?;
    Ok(dedupe_by_message_id(within_cap(rows, "reply")?))
}

/// [`MAX_SCAN_ROWS`] plus the one extra row that reveals there are more.
fn scan_limit() -> i64 {
    i64::try_from(MAX_SCAN_ROWS.saturating_add(1)).unwrap_or(i64::MAX)
}

/// The messages `replies` answer, looked up by `Message-ID`.
///
/// Chunked `IN (...)` lookups against `idx_messages_message_id`. When one
/// `Message-ID` matches several rows — again, the same mail filed twice — the
/// earliest timestamp wins, tie-broken on the lowest id, so a latency is
/// measured from when the mail first arrived rather than from when a copy of
/// it happened to be made.
async fn load_parents(
    db: &Database,
    cancel: &CancellationToken,
    account_id: Option<i64>,
    replies: &[MessageRow],
) -> Result<HashMap<(i64, String), MessageRow>, Error> {
    let mut wanted: Vec<String> = replies
        .iter()
        .filter_map(|reply| {
            parent_ref(
                reply.in_reply_to.as_deref(),
                reply.references_hdr.as_deref(),
            )
        })
        .collect();
    wanted.sort_unstable();
    wanted.dedup();

    let mut parents: HashMap<(i64, String), MessageRow> = HashMap::new();
    for chunk in wanted.chunks(ID_CHUNK) {
        let ids: Vec<String> = chunk.to_vec();
        let rows = scan(db, cancel, "originals", move |conn| {
            let wanted = in_clause("message_id", ids.len());
            let sql = format!("{MESSAGE_SELECT} WHERE (? IS NULL OR account_id = ?) AND {wanted}");
            let mut params: Vec<Value> = vec![
                account_id.map_or(Value::Null, Value::Integer),
                account_id.map_or(Value::Null, Value::Integer),
            ];
            params.extend(ids.iter().map(|id| Value::Text(id.clone())));
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt
                .query_map(rusqlite::params_from_iter(params), MessageRow::from_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await?;
        for row in rows {
            let Some(message_id) = row.message_id.clone() else {
                continue;
            };
            let key = (row.account_id, message_id);
            match parents.get(&key) {
                Some(existing) if better_original(existing, &row) => {}
                _ => {
                    parents.insert(key, row);
                }
            }
        }
    }
    Ok(parents)
}

/// The newest message of *yours* in each of `inbound`'s threads.
///
/// The basis of [`ResponseGroup::awaiting_reply`]: their message is answered
/// when anything you wrote in the same thread is newer than it. Unbounded in
/// *time* on purpose — a reply that landed after `until` still answers the
/// mail it answers — but bounded to the threads `inbound` actually named,
/// which is what keeps it an indexed lookup (`idx_messages_thread`) instead
/// of an all-time table scan.
///
/// `Drafts` folders are excluded: a reply you have written but not sent has
/// not answered anything.
///
/// One statement per account rather than one over the union of every
/// identity: a thread belongs to exactly one account, and two accounts on one
/// machine can be two different mailboxes (a personal address and a shared
/// `support@` one). Pooling their identities would let a *colleague's* reply
/// from the shared address read as the user's own, which is the same
/// direction mistake [`pair_up`] avoids by looking each address up under the
/// account whose message it is.
async fn load_last_ours_per_thread(
    db: &Database,
    cancel: &CancellationToken,
    self_addrs: &HashMap<i64, HashSet<String>>,
    mailboxes: &[MailboxRow],
    inbound: &[MessageRow],
) -> Result<HashMap<i64, i64>, Error> {
    let drafts = folder_ids(mailboxes, DRAFT_FOLDER_NAMES);
    let mut wanted: HashMap<i64, Vec<i64>> = HashMap::new();
    for message in inbound {
        if let Some(thread_id) = message.thread_id {
            wanted
                .entry(message.account_id)
                .or_default()
                .push(thread_id);
        }
    }

    let mut last: HashMap<i64, i64> = HashMap::new();
    for (account_id, threads) in &mut wanted {
        let account_id = *account_id;
        let Some(addresses) = self_addrs.get(&account_id).filter(|set| !set.is_empty()) else {
            continue;
        };
        threads.sort_unstable();
        threads.dedup();
        let addresses: Vec<String> = addresses.iter().cloned().collect();
        for chunk in threads.chunks(ID_CHUNK) {
            let thread_ids: Vec<i64> = chunk.to_vec();
            let addresses = addresses.clone();
            let drafts = drafts.clone();
            let rows: Vec<(i64, Option<i64>)> =
                scan(db, cancel, "your sends per thread", move |conn| {
                    let threads_in = in_clause("thread_id", thread_ids.len());
                    let senders_in = in_clause("lower(trim(from_addr))", addresses.len());
                    let excluded = not_in_clause("mailbox_id", &drafts);
                    let sql = format!(
                        "SELECT thread_id, MAX(COALESCE(date, internaldate)) FROM messages \
                         WHERE account_id = ? AND {threads_in} AND {senders_in}{excluded} \
                         GROUP BY thread_id"
                    );
                    let mut params: Vec<Value> = vec![Value::Integer(account_id)];
                    params.extend(thread_ids.iter().map(|id| Value::Integer(*id)));
                    params.extend(addresses.iter().map(|a| Value::Text(a.clone())));
                    params.extend(drafts.iter().map(|id| Value::Integer(*id)));
                    let mut stmt = conn.prepare(&sql)?;
                    let rows = stmt
                        .query_map(rusqlite::params_from_iter(params), |row| {
                            Ok((row.get::<_, i64>(0)?, row.get::<_, Option<i64>>(1)?))
                        })?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    Ok(rows)
                })
                .await?;
            for (thread_id, at) in rows {
                if let Some(at) = at {
                    let entry = last.entry(thread_id).or_insert(at);
                    *entry = (*entry).max(at);
                }
            }
        }
    }
    Ok(last)
}

/// Mail from someone else that landed inside the window.
///
/// Deduplicated across folders on the same grounds as [`load_replies`]: one
/// message filed twice is one message that has or has not been answered, not
/// two.
///
/// `Trash`/`Junk` and `Drafts` folders are excluded. Deleting or junking a
/// message *is* a way of handling it, and without that exclusion a spam
/// folder is the single largest source of "overdue" mail in any mailbox.
async fn load_inbound(
    db: &Database,
    cancel: &CancellationToken,
    query: &ResponseTimeQuery,
    self_addrs: &HashMap<i64, HashSet<String>>,
    mailboxes: &[MailboxRow],
) -> Result<Vec<MessageRow>, Error> {
    let account_id = query.account_id;
    let (since, until) = (query.since, query.until);
    let mut excluded_ids = folder_ids(mailboxes, DISPOSED_FOLDER_NAMES);
    excluded_ids.extend(folder_ids(mailboxes, DRAFT_FOLDER_NAMES));
    let rows = scan(db, cancel, "inbound mail", move |conn| {
        let excluded = not_in_clause("mailbox_id", &excluded_ids);
        let sql = format!(
            "{MESSAGE_SELECT} WHERE (? IS NULL OR account_id = ?) \
             AND COALESCE(date, internaldate) >= ? \
             AND COALESCE(date, internaldate) < ? \
             AND from_addr IS NOT NULL{excluded} ORDER BY id LIMIT ?"
        );
        let mut params: Vec<Value> = vec![
            account_id.map_or(Value::Null, Value::Integer),
            account_id.map_or(Value::Null, Value::Integer),
            Value::Integer(since),
            Value::Integer(until),
        ];
        params.extend(excluded_ids.iter().map(|id| Value::Integer(*id)));
        params.push(Value::Integer(scan_limit()));
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params), MessageRow::from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    })
    .await?;
    Ok(dedupe_by_message_id(within_cap(rows, "inbound")?)
        .into_iter()
        .filter(|row| !is_self(self_addrs, row))
        .collect())
}

// ---------------------------------------------------------------------------
// Pure computation
// ---------------------------------------------------------------------------

/// Match every reply to what it answers and classify the direction.
///
/// `replies` is already restricted to the window by [`load_replies`]; the
/// direction rules and the negative-latency guard are what live here.
fn pair_up(
    replies: &[MessageRow],
    parents: &HashMap<(i64, String), MessageRow>,
    self_addrs: &HashMap<i64, HashSet<String>>,
) -> Paired {
    let no_identity = HashSet::new();
    let mut out = Paired::default();
    for reply in replies {
        let Some(responded_at) = reply.at else {
            continue;
        };
        let Some(parent_id) = parent_ref(
            reply.in_reply_to.as_deref(),
            reply.references_hdr.as_deref(),
        ) else {
            continue;
        };
        let Some(parent) = parents.get(&(reply.account_id, parent_id)) else {
            continue;
        };
        let Some(original_at) = parent.at else {
            continue;
        };
        let (Some(reply_from), Some(parent_from)) =
            (reply.from_addr.as_deref(), parent.from_addr.as_deref())
        else {
            continue;
        };
        let mine = self_addrs.get(&reply.account_id).unwrap_or(&no_identity);
        let reply_is_ours = mine.contains(reply_from);
        let parent_is_ours = mine.contains(parent_from);
        if reply_is_ours == parent_is_ours {
            // Self to self, or two other people talking in a list thread.
            // Neither is a response time of this mailbox's.
            continue;
        }
        // `checked_sub`, not `-`: `original_at` comes from a `Date:` header,
        // which is attacker-controlled and can be `i64::MIN`. Plain
        // subtraction panics on overflow in a debug build and wraps to a
        // plausible-looking latency in a release one.
        let Some(latency) = responded_at.checked_sub(original_at) else {
            out.skipped_out_of_order += 1;
            continue;
        };
        if latency < 0 {
            out.skipped_out_of_order += 1;
            continue;
        }
        let inbound = if reply_is_ours { parent } else { reply };
        // `inbound.from_addr` is `parent_from` or `reply_from`; both are
        // `Some` by the guard above.
        let Some(counterparty) = inbound.from_addr.clone() else {
            continue;
        };
        out.pairs.push(Pair {
            counterparty,
            counterparty_name: inbound.from_name.clone(),
            mailbox_id: inbound.mailbox_id,
            latency,
            responded_at,
            inbound_at: if reply_is_ours {
                original_at
            } else {
                responded_at
            },
            ours: reply_is_ours,
        });
    }
    out
}

/// Accumulator for one group while it is being built.
#[derive(Debug, Default)]
struct GroupBuild {
    /// What [`ResponseGroup::key`] will report. Distinct from the map key the
    /// build is filed under: mailbox groups are keyed on the folder *id* so
    /// two accounts' `INBOX` stay separate, while still reporting the name.
    key_text: String,
    label: Option<String>,
    /// The timestamp the current `label` came from, so the newest wins.
    label_at: i64,
    mailbox_id: Option<i64>,
    ours: Vec<i64>,
    theirs: Vec<i64>,
    inbound: u64,
    /// Inbound mail with no later message of yours in the thread, as
    /// `(timestamp, is from someone you do answer)`.
    ///
    /// Kept rather than counted for two reasons: whether one of them is
    /// *overdue* depends on `ours.p90`, which is not known until every pair
    /// has been seen, and the correspondent flag has to be carried per
    /// message rather than per group — under [`GroupBy::Mailbox`] one group
    /// holds mail from everybody, so a group-level "you do answer these
    /// people" test would be true of INBOX in every mailbox that exists.
    unanswered: Vec<(i64, bool)>,
}

/// Turn matched pairs plus inbound counts into the finished report.
fn assemble(
    query: &ResponseTimeQuery,
    self_addrs: &HashMap<i64, HashSet<String>>,
    mailboxes: &[MailboxRow],
    paired: Paired,
    inbound: &[MessageRow],
    last_ours_in_thread: &HashMap<i64, i64>,
) -> ResponseTimes {
    let names: HashMap<i64, &str> = mailboxes
        .iter()
        .map(|mailbox| (mailbox.id, mailbox.name.as_str()))
        .collect();

    let mut builds: HashMap<String, GroupBuild> = HashMap::new();
    let mut all_ours: Vec<i64> = Vec::new();
    let mut all_theirs: Vec<i64> = Vec::new();

    // The addresses you have actually answered at least once in this window.
    // The gate that keeps newsletters, CI mail and cold outreach out of
    // `overdue`, applied per *sender* rather than per group — see
    // `GroupBuild::unanswered`.
    let mut answered_senders: HashSet<&str> = HashSet::new();

    for pair in &paired.pairs {
        if pair.ours {
            all_ours.push(pair.latency);
            answered_senders.insert(pair.counterparty.as_str());
        } else {
            all_theirs.push(pair.latency);
        }
        let Some(group) = group_key(query.group_by, pair, &names) else {
            continue;
        };
        let build = builds.entry(group.map_key).or_default();
        build.key_text = group.key_text;
        build.mailbox_id = group.mailbox_id;
        if group.label.is_some() && (build.label.is_none() || pair.inbound_at >= build.label_at) {
            build.label = group.label;
            build.label_at = pair.inbound_at;
        }
        if pair.ours {
            build.ours.push(pair.latency);
        } else {
            build.theirs.push(pair.latency);
        }
    }

    // Inbound counts attach to groups that already exist; they never create
    // one. A sender you have never replied to and who has never replied to
    // you is not a correspondence, and admitting every newsletter here would
    // bury the relationships that are.
    for message in inbound {
        let Some(key) = inbound_key(query.group_by, message, &names) else {
            continue;
        };
        let Some(build) = builds.get_mut(&key) else {
            continue;
        };
        build.inbound += 1;
        if let Some(at) = message
            .at
            .filter(|_| !answered_in_thread(message, last_ours_in_thread))
        {
            let correspondent = message
                .from_addr
                .as_deref()
                .is_some_and(|from| answered_senders.contains(from));
            build.unanswered.push((at, correspondent));
        }
    }

    let mut groups: Vec<ResponseGroup> = builds
        .into_values()
        .map(|mut build| {
            build.ours.sort_unstable();
            build.theirs.sort_unstable();
            let ours = Stats::from_sorted(&build.ours);
            let theirs = Stats::from_sorted(&build.theirs);
            let cutoff = query.until.saturating_sub(overdue_after(ours));
            let overdue = build
                .unanswered
                .iter()
                .filter(|(at, correspondent)| *correspondent && *at < cutoff)
                .count() as u64;
            let (slower, stalled) = bottleneck_flags(query, ours, theirs, overdue);
            ResponseGroup {
                label: build.label.unwrap_or_else(|| build.key_text.clone()),
                key: build.key_text,
                mailbox_id: build.mailbox_id,
                ours,
                theirs,
                inbound: build.inbound,
                awaiting_reply: build.unanswered.len() as u64,
                overdue,
                bottleneck: slower || stalled,
                slower_than_counterpart: slower,
                stalled,
            }
        })
        .collect();

    // Flagged first, then slowest, then most-abandoned, then best-evidenced,
    // with the key as a final tie-break so the ordering is total and a
    // truncated report is reproducible.
    groups.sort_by(|a, b| {
        a.bottleneck
            .cmp(&b.bottleneck)
            .reverse()
            .then(a.ours.p50_seconds.cmp(&b.ours.p50_seconds).reverse())
            .then(a.overdue.cmp(&b.overdue).reverse())
            .then(a.ours.samples.cmp(&b.ours.samples).reverse())
            .then(a.key.cmp(&b.key))
            // Two accounts' `INBOX` report the same `key`, so the folder id is
            // what makes the ordering total there.
            .then(a.mailbox_id.cmp(&b.mailbox_id))
    });
    let total_groups = groups.len();
    groups.truncate(query.limit);

    all_ours.sort_unstable();
    all_theirs.sort_unstable();

    let mut ours_by_time: Vec<(i64, i64)> = paired
        .pairs
        .iter()
        .filter(|pair| pair.ours)
        .map(|pair| (pair.responded_at, pair.latency))
        .collect();
    ours_by_time.sort_unstable();

    ResponseTimes {
        since: query.since,
        until: query.until,
        group_by: query.group_by,
        ours: Stats::from_sorted(&all_ours),
        theirs: Stats::from_sorted(&all_theirs),
        groups,
        total_groups,
        trend: rolling_trend(query, &ours_by_time),
        self_addresses: flatten_self(self_addrs),
        pairs: paired.pairs.len() as u64,
        skipped_out_of_order: paired.skipped_out_of_order,
    }
}

/// How one pair is filed: the internal map key, the key reported to the
/// caller, a display label, and a folder id when there is one.
struct GroupSlot {
    /// Unique per group. The counterparty address, or the folder *id* —
    /// never the folder name, because with `account_id = None` two accounts
    /// each have an `INBOX` and merging them would report one median over two
    /// unrelated mailboxes under whichever folder id happened to be seen last.
    map_key: String,
    /// [`ResponseGroup::key`]: the address, or the folder name.
    key_text: String,
    label: Option<String>,
    mailbox_id: Option<i64>,
}

/// The slot one pair belongs in.
fn group_key(group_by: GroupBy, pair: &Pair, names: &HashMap<i64, &str>) -> Option<GroupSlot> {
    match group_by {
        GroupBy::Contact => Some(GroupSlot {
            map_key: pair.counterparty.clone(),
            key_text: pair.counterparty.clone(),
            label: pair.counterparty_name.clone(),
            mailbox_id: None,
        }),
        GroupBy::Mailbox => {
            let name = names.get(&pair.mailbox_id)?;
            Some(GroupSlot {
                map_key: pair.mailbox_id.to_string(),
                key_text: (*name).to_owned(),
                label: Some((*name).to_owned()),
                mailbox_id: Some(pair.mailbox_id),
            })
        }
    }
}

/// The internal map key an inbound message's counts attach to. Must agree
/// with [`GroupSlot::map_key`] or the counts land nowhere.
fn inbound_key(
    group_by: GroupBy,
    message: &MessageRow,
    names: &HashMap<i64, &str>,
) -> Option<String> {
    match group_by {
        GroupBy::Contact => message.from_addr.clone(),
        GroupBy::Mailbox => names
            .contains_key(&message.mailbox_id)
            .then(|| message.mailbox_id.to_string()),
    }
}

/// Whether anything of yours in the same thread is newer than `message`.
fn answered_in_thread(message: &MessageRow, last_ours_in_thread: &HashMap<i64, i64>) -> bool {
    let (Some(thread_id), Some(at)) = (message.thread_id, message.at) else {
        return false;
    };
    last_ours_in_thread
        .get(&thread_id)
        .is_some_and(|last| *last >= at)
}

/// The two bottleneck triggers, evaluated independently.
///
/// **Slower** compares medians by cross-multiplication rather than division,
/// so a counterparty who answers within the same second (`p50 == 0`, which
/// auto-responders really do produce) does not put a zero in a denominator.
/// A zero *numerator* is handled too, and it needs more than a `> 0` guard:
/// against `theirs.p50 == 0` every positive median satisfies `x >= 0 * ratio`
/// and the ratio stops discriminating entirely, so the denominator is floored
/// at one second and your own median must clear [`MIN_SLOW_SECONDS`] before
/// the arm fires at all. Two seconds is not a bottleneck however many times
/// larger than zero it is.
///
/// **Stalled** counts only *overdue* mail — already filtered to senders you
/// answer, and to messages past your own p90 — and additionally needs one
/// reply of yours in this group. Without the age filter, every conversation
/// that merely ended with the other person's message (half of all of them,
/// since somebody has to speak last) reads as a correspondence you are
/// blocking. Without the sender filter, every newsletter does. With both, the
/// flag means "you do talk to these people, and right now you are not".
fn bottleneck_flags(
    query: &ResponseTimeQuery,
    ours: Stats,
    theirs: Stats,
    overdue: u64,
) -> (bool, bool) {
    let bar = u64::from(query.min_samples);
    let slower = ours.samples >= bar
        && theirs.samples >= bar
        && ours.p50_seconds >= MIN_SLOW_SECONDS
        && (ours.p50_seconds as f64) >= (theirs.p50_seconds.max(1) as f64) * query.bottleneck_ratio;
    let stalled = ours.samples >= 1 && overdue >= bar;
    (slower, stalled)
}

/// How long an unanswered message must sit before it counts as overdue.
///
/// Your own p90 for the group, floored at [`OVERDUE_FLOOR_SECONDS`]: the bar
/// is "longer than you normally take with these people", which is the only
/// definition of late that does not need a global constant nobody agrees on.
/// A group with no replies of yours to calibrate against falls back to the
/// floor.
#[must_use]
pub fn overdue_after(ours: Stats) -> i64 {
    if ours.samples == 0 {
        OVERDUE_FLOOR_SECONDS
    } else {
        ours.p90_seconds.max(OVERDUE_FLOOR_SECONDS)
    }
}

/// The rolling trend over your own response times.
///
/// `ours` must be sorted by timestamp. Points are laid out backwards from
/// `until` so the newest point always ends exactly at the window's end — a
/// forward layout would leave the most recent, most interesting point as a
/// ragged partial bucket.
fn rolling_trend(query: &ResponseTimeQuery, ours: &[(i64, i64)]) -> Vec<TrendPoint> {
    let points = query.trend_points();
    let mut trend = Vec::with_capacity(points);
    let mut scratch: Vec<i64> = Vec::new();
    for step in (0..points).rev() {
        let offset = i64::try_from(step).unwrap_or(i64::MAX);
        let window_end = query
            .until
            .saturating_sub(offset.saturating_mul(query.bucket_seconds));
        let window_start = window_end
            .saturating_sub(query.window_seconds)
            .max(query.since);
        let lo = ours.partition_point(|(at, _)| *at < window_start);
        let hi = ours.partition_point(|(at, _)| *at < window_end);
        scratch.clear();
        scratch.extend(ours[lo..hi].iter().map(|(_, latency)| *latency));
        scratch.sort_unstable();
        trend.push(TrendPoint {
            window_start,
            window_end,
            stats: Stats::from_sorted(&scratch),
        });
    }
    trend
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// Folder names that hold mail the user is *composing*, not mail they sent.
///
/// A `Drafts` copy is a reply that was never sent. Counting one as evidence
/// that a thread was answered is the difference between "I have replied" and
/// "I have started to reply", and only the first discharges an obligation —
/// so [`load_last_ours_per_thread`] excludes these.
const DRAFT_FOLDER_NAMES: &[&str] = &["drafts", "draft", "inbox.drafts", "[gmail]/drafts"];

/// Folder names that hold mail the user has already disposed of.
///
/// Deleting or junking a message *is* a way of handling it, so mail in these
/// folders is not counted as inbound awaiting a reply. Without this a spam
/// folder is the single largest source of "overdue" mail in any mailbox.
const DISPOSED_FOLDER_NAMES: &[&str] = &[
    "trash",
    "deleted",
    "deleted items",
    "deleted messages",
    "junk",
    "junk e-mail",
    "junk email",
    "spam",
    "bulk mail",
    "inbox.trash",
    "inbox.junk",
    "[gmail]/trash",
    "[gmail]/spam",
    "[gmail]/bin",
];

/// Whether `name` is one of `candidates`, case-insensitively.
///
/// Name matching rather than RFC 6154 special-use flags for the reason
/// [`looks_like_sent`] gives: `imap::folders::list_folders` records only
/// selectability today. When that changes, all three lists should move
/// together.
fn folder_is(name: &str, candidates: &[&str]) -> bool {
    let lower = name.to_ascii_lowercase();
    candidates.contains(&lower.as_str())
}

/// The `Message-ID` a reply names as its immediate parent.
///
/// `In-Reply-To` first, then the last entry of `References`. `References` is
/// oldest-first, so its last entry *is* the immediate parent; taking the same
/// end of `In-Reply-To` keeps the two consistent for the rare header that
/// carries several ids.
fn parent_ref(in_reply_to: Option<&str>, references: Option<&str>) -> Option<String> {
    last_id(in_reply_to).or_else(|| last_id(references))
}

/// The last whitespace-separated id in a space-joined header value.
fn last_id(value: Option<&str>) -> Option<String> {
    value?.split_ascii_whitespace().next_back().and_then(bare)
}

/// Strip the angle brackets a header value may still carry.
///
/// `message::parse` stores bare ids, but `compose` and hand-written fixtures
/// have both spellings in the wild, and an id that matched on one side and
/// not the other would silently drop a pair.
fn bare(value: &str) -> Option<String> {
    let trimmed = value.trim();
    let stripped = trimmed
        .strip_prefix('<')
        .and_then(|v| v.strip_suffix('>'))
        .unwrap_or(trimmed)
        .trim();
    if stripped.is_empty() {
        None
    } else {
        Some(stripped.to_owned())
    }
}

/// Lowercase and trim an address, dropping a blank one.
///
/// **This must reproduce SQLite's `lower(trim(x))` exactly**, because
/// [`load_last_ours_per_thread`] compares values normalized here against that
/// expression evaluated inside the database. SQLite's one-argument `trim`
/// strips U+0020 only, and its `lower` is ASCII-only; Rust's `str::trim` and
/// `str::to_lowercase` are both Unicode-aware and would fold a tab, or a `Ü`,
/// that the database would leave alone. The two sides then silently stop
/// matching for exactly the addresses that need it most — an identity that
/// never matches makes every thread look unanswered and the whole group
/// falsely `stalled`.
///
/// The cost is that two Unicode-case spellings of one non-ASCII address are
/// two groups. That is a cosmetic split; a normalization that disagrees with
/// the query it feeds is a wrong answer.
fn normalize_address(value: &str) -> Option<String> {
    let trimmed = value.trim_matches(' ').to_ascii_lowercase();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

/// Whether a message was written by the user.
fn is_self(self_addrs: &HashMap<i64, HashSet<String>>, row: &MessageRow) -> bool {
    let Some(from) = row.from_addr.as_deref() else {
        return false;
    };
    self_addrs
        .get(&row.account_id)
        .is_some_and(|mine| mine.contains(from))
}

/// Every self address across every account, sorted and deduplicated.
fn flatten_self(self_addrs: &HashMap<i64, HashSet<String>>) -> Vec<String> {
    let mut all: Vec<String> = self_addrs
        .values()
        .flat_map(|set| set.iter().cloned())
        .collect();
    all.sort_unstable();
    all.dedup();
    all
}

/// Whether `existing` should be kept over `candidate` as the original of a
/// `Message-ID` present more than once.
fn better_original(existing: &MessageRow, candidate: &MessageRow) -> bool {
    match (existing.at, candidate.at) {
        (Some(a), Some(b)) if a != b => a < b,
        (Some(_), None) => true,
        (None, Some(_)) => false,
        _ => existing.id <= candidate.id,
    }
}

/// Collapse rows that are the same mail filed in more than one folder.
fn dedupe_by_message_id(rows: Vec<MessageRow>) -> Vec<MessageRow> {
    let mut seen: HashSet<(i64, String)> = HashSet::new();
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let duplicate = row
            .message_id
            .as_ref()
            .is_some_and(|message_id| !seen.insert((row.account_id, message_id.clone())));
        if !duplicate {
            out.push(row);
        }
    }
    out
}
