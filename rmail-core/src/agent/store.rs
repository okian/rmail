//! The run ledger and the action log — `V53`'s three tables, and the reads
//! `GetAgentRunLog` serves.
//!
//! # Write order is the audit property
//!
//! [`begin_action`] inserts the row *before* the mutation runs and
//! [`finish_action`] updates it afterwards. Nothing here offers "insert the
//! finished row", because a caller with that available would eventually use
//! it, and the crash window it opens is precisely the one an unattended loop
//! hits: mailbox changed, log silent.
//!
//! The consequence to be honest about is a row left at
//! [`Outcome::Attempted`] when the daemon dies mid-mutation. That is not a bug
//! being tolerated; it is the only truthful thing the log can say, and it is
//! why `Attempted` is a value rather than an internal placeholder.
//!
//! # Nothing here is called on a dry run
//!
//! Not "called with a flag": not called. See [`super`]'s module docs and
//! `V53`'s header — the dry-run guarantee is that no row exists afterwards,
//! and a `dry_run` column the writer sets would be a row.

use rusqlite::OptionalExtension;

use crate::error::Error;
use crate::storage::Database;

use super::action::ActionKind;

/// Why a run stopped. Every value the `agent_runs.stop_reason` CHECK admits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// Still going. The value a row carries between [`open_run`] and
    /// [`close_run`], and the one left behind by a crash.
    Running,
    /// The candidate list ran out — the loop finished its work.
    Completed,
    /// `agent.max_iterations` was reached.
    IterationCap,
    /// `agent.max_actions` was reached.
    ActionCap,
    /// `agent.max_duration` elapsed.
    Deadline,
    /// The caller went away or the daemon is shutting down.
    Cancelled,
    /// The loop could not continue.
    Error,
}

impl StopReason {
    /// Every value, for exhaustive handling and tests.
    pub const ALL: [Self; 7] = [
        Self::Running,
        Self::Completed,
        Self::IterationCap,
        Self::ActionCap,
        Self::Deadline,
        Self::Cancelled,
        Self::Error,
    ];

    /// The stored string; matches `agent_runs`' CHECK list exactly.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Completed => "completed",
            Self::IterationCap => "iteration_cap",
            Self::ActionCap => "action_cap",
            Self::Deadline => "deadline",
            Self::Cancelled => "cancelled",
            Self::Error => "error",
        }
    }

    /// Parse a stored string.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.as_str() == value)
    }

    /// Whether this is a bound firing rather than the work finishing.
    #[must_use]
    pub const fn is_bound(self) -> bool {
        matches!(self, Self::IterationCap | Self::ActionCap | Self::Deadline)
    }
}

/// What became of one decided action. Matches `agent_actions`' CHECK list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Written before the mutation; replaced by [`Outcome::Applied`] or
    /// [`Outcome::Failed`] when it returns. Surviving a run means the process
    /// died in between.
    Attempted,
    /// The mutation landed.
    Applied,
    /// The mutation was tried and did not land.
    Failed,
    /// The prompt-injection shield refused to let it run.
    Withheld,
    /// The model's answer was not in the closed vocabulary.
    Refused,
    /// A dry run: what a real run would have done. Never persisted — see the
    /// module docs — but carried in the in-memory report.
    Planned,
}

impl Outcome {
    /// Every value, for exhaustive handling and tests.
    pub const ALL: [Self; 6] = [
        Self::Attempted,
        Self::Applied,
        Self::Failed,
        Self::Withheld,
        Self::Refused,
        Self::Planned,
    ];

    /// The stored string.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Attempted => "attempted",
            Self::Applied => "applied",
            Self::Failed => "failed",
            Self::Withheld => "withheld",
            Self::Refused => "refused",
            Self::Planned => "planned",
        }
    }

    /// Parse a stored string.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|o| o.as_str() == value)
    }
}

/// One row of the action log, as read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoggedAction {
    /// This entry's id.
    pub id: i64,
    /// The run it belongs to.
    pub run_id: i64,
    /// The local message id, or `None` once the message is gone (an archive
    /// deletes the local row — see `V53`'s header).
    pub message_id: Option<i64>,
    /// The RFC822 `Message-ID`, frozen at decision time.
    pub rfc_message_id: String,
    /// The subject, frozen at decision time.
    pub subject: String,
    /// The sender, frozen at decision time.
    pub sender: String,
    /// What was decided.
    pub action: ActionKind,
    /// The action's validated parameter, rendered.
    pub argument: String,
    /// The model's stated reason.
    pub reason: String,
    /// What became of it.
    pub outcome: Outcome,
    /// Human-readable detail.
    pub detail: String,
    /// When it was decided.
    pub decided_at: i64,
}

/// One run, as read back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoggedRun {
    /// This run's id.
    pub id: i64,
    /// The account it walked.
    pub account_id: i64,
    /// The mailbox it walked.
    pub mailbox: String,
    /// The policy it was steering toward.
    pub policy: String,
    /// When it started.
    pub started_at: i64,
    /// When it finished, or `None` while running.
    pub finished_at: Option<i64>,
    /// Why it stopped.
    pub stop_reason: StopReason,
    /// Iterations consumed.
    pub iterations: u32,
    /// Model calls made.
    pub model_calls: u32,
    /// Actions that landed.
    pub actions_applied: u32,
    /// Its action log, oldest first.
    pub actions: Vec<LoggedAction>,
}

/// Open a run row and return its id.
///
/// # Errors
/// A mapped storage error; [`Error::NotFound`] shaped as a foreign-key
/// violation if `account_id` names no account.
pub async fn open_run(
    db: &Database,
    account_id: i64,
    mailbox: &str,
    policy: &str,
) -> Result<i64, Error> {
    let mailbox = mailbox.to_owned();
    let policy = policy.to_owned();
    Ok(db
        .write(move |conn| {
            conn.execute(
                "INSERT INTO agent_runs (account_id, dry_run, policy, mailbox, stop_reason)
                 VALUES (?1, 0, ?2, ?3, 'running')",
                rusqlite::params![account_id, policy, mailbox],
            )?;
            Ok(conn.last_insert_rowid())
        })
        .await?)
}

/// Close a run, recording why it stopped and what it consumed.
///
/// # Errors
/// A mapped storage error.
pub async fn close_run(
    db: &Database,
    run_id: i64,
    reason: StopReason,
    iterations: u32,
    model_calls: u32,
    actions_applied: u32,
) -> Result<(), Error> {
    db.write(move |conn| {
        conn.execute(
            "UPDATE agent_runs
                SET finished_at = unixepoch(), stop_reason = ?2,
                    iterations = ?3, model_calls = ?4, actions_applied = ?5
              WHERE id = ?1",
            rusqlite::params![
                run_id,
                reason.as_str(),
                i64::from(iterations),
                i64::from(model_calls),
                i64::from(actions_applied)
            ],
        )?;
        Ok(())
    })
    .await?;
    Ok(())
}

/// Everything an action log entry needs, frozen before the mutation runs.
#[derive(Debug, Clone)]
pub struct PendingAction<'a> {
    /// The run this belongs to.
    pub run_id: i64,
    /// The local message id.
    pub message_id: i64,
    /// The RFC822 `Message-ID`, if the message carries one.
    pub rfc_message_id: &'a str,
    /// The subject.
    pub subject: &'a str,
    /// The sender.
    pub sender: &'a str,
    /// What is about to be done.
    pub action: ActionKind,
    /// Its validated parameter, rendered.
    pub argument: &'a str,
    /// The model's stated reason.
    pub reason: &'a str,
    /// The outcome to record *now*. [`Outcome::Attempted`] for something about
    /// to be tried; a terminal value for something that will never be tried
    /// (refused, withheld).
    pub outcome: Outcome,
    /// Detail for the terminal case.
    pub detail: &'a str,
}

/// Write an action log entry, returning its id.
///
/// # Errors
/// A mapped storage error.
pub async fn begin_action(db: &Database, pending: &PendingAction<'_>) -> Result<i64, Error> {
    let run_id = pending.run_id;
    let message_id = pending.message_id;
    let rfc_message_id = pending.rfc_message_id.to_owned();
    let subject = pending.subject.to_owned();
    let sender = pending.sender.to_owned();
    let action = pending.action.as_str();
    let argument = pending.argument.to_owned();
    let reason = pending.reason.to_owned();
    let outcome = pending.outcome.as_str();
    let detail = pending.detail.to_owned();
    Ok(db
        .write(move |conn| {
            conn.execute(
                "INSERT INTO agent_actions
                     (run_id, message_id, rfc_message_id, subject, sender,
                      action, argument, reason, outcome, detail)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                rusqlite::params![
                    run_id,
                    message_id,
                    rfc_message_id,
                    subject,
                    sender,
                    action,
                    argument,
                    reason,
                    outcome,
                    detail
                ],
            )?;
            Ok(conn.last_insert_rowid())
        })
        .await?)
}

/// Update an entry once its mutation returned.
///
/// # Errors
/// A mapped storage error.
pub async fn finish_action(
    db: &Database,
    action_id: i64,
    outcome: Outcome,
    detail: &str,
) -> Result<(), Error> {
    let detail = detail.to_owned();
    db.write(move |conn| {
        conn.execute(
            "UPDATE agent_actions SET outcome = ?2, detail = ?3 WHERE id = ?1",
            rusqlite::params![action_id, outcome.as_str(), detail],
        )?;
        Ok(())
    })
    .await?;
    Ok(())
}

/// Record a snooze. Replaces any existing one for the message: a second
/// decision about the same mail is the current answer, not a duplicate row.
///
/// # Errors
/// A mapped storage error.
pub async fn snooze(db: &Database, message_id: i64, until: i64, reason: &str) -> Result<(), Error> {
    let reason = reason.to_owned();
    db.write(move |conn| {
        conn.execute(
            "INSERT INTO message_snoozes (message_id, until, reason)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(message_id) DO UPDATE
                SET until = excluded.until,
                    reason = excluded.reason,
                    snoozed_at = unixepoch()",
            rusqlite::params![message_id, until, reason],
        )?;
        Ok(())
    })
    .await?;
    Ok(())
}

/// When a message is snoozed until, if it is.
///
/// # Errors
/// A mapped storage error.
pub async fn snoozed_until(db: &Database, message_id: i64) -> Result<Option<i64>, Error> {
    Ok(db
        .read(move |conn| {
            conn.query_row(
                "SELECT until FROM message_snoozes WHERE message_id = ?1",
                [message_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()
        })
        .await?)
}

/// The most recent runs for `account_id`, newest first, each with its actions.
///
/// `limit` is clamped by the caller; a zero or negative one returns nothing
/// rather than everything, which is the safe direction for a paging parameter
/// arriving from the wire.
///
/// # Errors
/// A mapped storage error, or [`Error::Internal`] if a row holds a string no
/// version of this code wrote.
pub async fn recent_runs(
    db: &Database,
    account_id: i64,
    limit: i64,
) -> Result<Vec<LoggedRun>, Error> {
    if limit <= 0 {
        return Ok(Vec::new());
    }
    let rows = db
        .read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT id, account_id, mailbox, policy, started_at, finished_at,
                        stop_reason, iterations, model_calls, actions_applied
                   FROM agent_runs
                  WHERE account_id = ?1
                  ORDER BY id DESC
                  LIMIT ?2",
            )?;
            let mapped = stmt
                .query_map(rusqlite::params![account_id, limit], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, Option<i64>>(5)?,
                        row.get::<_, String>(6)?,
                        row.get::<_, i64>(7)?,
                        row.get::<_, i64>(8)?,
                        row.get::<_, i64>(9)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(mapped)
        })
        .await?;

    // One query for every run's actions, not one per run. The page can be 50
    // runs, and an N+1 across the blocking pool for a read this shallow is
    // gratuitous — the actions are then handed out by run id.
    let run_ids: Vec<i64> = rows.iter().map(|row| row.0).collect();
    let mut by_run = actions_for_runs(db, &run_ids).await?;

    let mut out = Vec::with_capacity(rows.len());
    for (
        id,
        account_id,
        mailbox,
        policy,
        started_at,
        finished_at,
        stop_reason,
        iterations,
        model_calls,
        actions_applied,
    ) in rows
    {
        let stop_reason = StopReason::parse(&stop_reason).ok_or_else(|| {
            Error::internal(format!("unknown agent stop reason in log: {stop_reason}"))
        })?;
        out.push(LoggedRun {
            id,
            account_id,
            mailbox,
            policy,
            started_at,
            finished_at,
            stop_reason,
            iterations: u32::try_from(iterations).unwrap_or(u32::MAX),
            model_calls: u32::try_from(model_calls).unwrap_or(u32::MAX),
            actions_applied: u32::try_from(actions_applied).unwrap_or(u32::MAX),
            actions: by_run.remove(&id).unwrap_or_default(),
        });
    }
    Ok(out)
}

/// Every action belonging to any of `run_ids`, grouped by run, oldest first
/// within each.
///
/// # Errors
/// A mapped storage error, or [`Error::Internal`] for an unknown stored enum.
async fn actions_for_runs(
    db: &Database,
    run_ids: &[i64],
) -> Result<std::collections::HashMap<i64, Vec<LoggedAction>>, Error> {
    let mut out: std::collections::HashMap<i64, Vec<LoggedAction>> =
        std::collections::HashMap::new();
    if run_ids.is_empty() {
        return Ok(out);
    }
    // `rusqlite` has no list binding, so the placeholders are generated from
    // the *count* and every id is still bound as a parameter — no id is ever
    // formatted into the SQL text.
    let placeholders = vec!["?"; run_ids.len()].join(", ");
    let sql = format!(
        "SELECT id, run_id, message_id, rfc_message_id, subject, sender,
                action, argument, reason, outcome, detail, decided_at
           FROM agent_actions
          WHERE run_id IN ({placeholders})
          ORDER BY run_id, id"
    );
    let owned: Vec<i64> = run_ids.to_vec();
    let rows = db
        .read(move |conn| {
            let mut stmt = conn.prepare(&sql)?;
            let mapped = stmt
                .query_map(rusqlite::params_from_iter(owned.iter()), row_to_tuple)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(mapped)
        })
        .await?;
    for row in rows {
        let action = tuple_to_action(row)?;
        out.entry(action.run_id).or_default().push(action);
    }
    Ok(out)
}

/// One run's action log, oldest first.
///
/// # Errors
/// A mapped storage error, or [`Error::Internal`] for an unknown stored enum.
pub async fn actions_for(db: &Database, run_id: i64) -> Result<Vec<LoggedAction>, Error> {
    Ok(actions_for_runs(db, &[run_id])
        .await?
        .remove(&run_id)
        .unwrap_or_default())
}

/// The column tuple every action query selects. One definition, so the
/// single-run and multi-run reads cannot drift in column order — a kind of
/// drift the compiler cannot see, since every column is a `String` or an
/// `i64`.
type ActionRow = (
    i64,
    i64,
    Option<i64>,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    i64,
);

fn row_to_tuple(row: &rusqlite::Row<'_>) -> rusqlite::Result<ActionRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
    ))
}

/// # Errors
/// [`Error::Internal`] for an action or outcome string no version of this code
/// wrote — a log written by a *newer* build, which is a deployment problem
/// rather than a caller's.
fn tuple_to_action(row: ActionRow) -> Result<LoggedAction, Error> {
    let (
        id,
        run_id,
        message_id,
        rfc_message_id,
        subject,
        sender,
        action,
        argument,
        reason,
        outcome,
        detail,
        decided_at,
    ) = row;
    Ok(LoggedAction {
        id,
        run_id,
        message_id,
        rfc_message_id,
        subject,
        sender,
        action: ActionKind::parse(&action)
            .ok_or_else(|| Error::internal(format!("unknown agent action in log: {action}")))?,
        argument,
        reason,
        outcome: Outcome::parse(&outcome).ok_or_else(|| {
            Error::internal(format!("unknown agent action outcome in log: {outcome}"))
        })?,
        detail,
        decided_at,
    })
}

/// The messages this run should consider, newest first.
///
/// Deterministic and daemon-chosen — see [`super`]'s module docs on why the
/// model picks *what to do*, never *what to look at*. The exclusions are:
///
/// - another account's mail (scoped, like every query in [`crate::rules`]),
/// - anything already snoozed past `now`,
/// - anything this account's agent has already decided on, in any earlier run.
///
/// That last one is what stops the loop re-deciding the same message forever
/// and what makes two runs in a row idempotent. It is keyed by message rather
/// than by (run, message) precisely so it survives across runs.
///
/// # The one exception, and why it has to exist
///
/// A `withheld` entry whose message a human has *since confirmed* does not
/// count. Without that carve-out the shield would be a dead end rather than a
/// gate: the agent withholds, the user reviews and confirms with
/// `AiSafetyService.ConfirmInjection` — and the next run never looks at the
/// message again, because it already has an entry. The confirmation surface
/// task 77 built would do nothing here, and the withhold's own advice ("review
/// it and, if it is safe, confirm") would be a lie.
///
/// It is *conditioned* on the confirmation rather than simply excluding every
/// withheld entry, because an unconfirmed one must stay excluded: re-deciding
/// hostile mail on every run means an attacker who can get one message into
/// the mailbox can make its owner pay for a model call on every run, forever.
///
/// # And the second one, for the same structural reason
///
/// An applied `snooze` whose `until` has passed likewise does not count.
/// Without that clause the snooze exclusion in this very query would be dead
/// logic — a snoozed message already carries a logged action, so the
/// "already decided" clause would exclude it forever and the expiry would
/// never arrive. "Defer until T" has to mean the agent looks again at T, or
/// `snooze` is behaviourally identical to `none` while costing one unit of the
/// `max_actions` budget.
///
/// # Errors
/// A mapped storage error.
pub async fn candidates(
    db: &Database,
    account_id: i64,
    mailbox: &str,
    now: i64,
    limit: i64,
) -> Result<Vec<i64>, Error> {
    if limit <= 0 {
        return Ok(Vec::new());
    }
    let mailbox = mailbox.to_owned();
    Ok(db
        .read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT m.id
                   FROM messages m
                   JOIN mailboxes b ON b.id = m.mailbox_id
                  WHERE m.account_id = ?1
                    AND b.name = ?2
                    AND NOT EXISTS (
                        SELECT 1 FROM message_snoozes s
                         WHERE s.message_id = m.id AND s.until > ?3
                    )
                    AND NOT EXISTS (
                        SELECT 1 FROM agent_actions a
                          JOIN agent_runs r ON r.id = a.run_id
                         WHERE a.message_id = m.id AND r.account_id = ?1
                           -- A withheld entry a human has since confirmed does
                           -- not count; see this function's docs.
                           AND NOT (
                               a.outcome = 'withheld'
                               AND EXISTS (
                                   SELECT 1 FROM ai_injection_flags f
                                    WHERE f.message_id = m.id
                                      AND f.confirmed_at IS NOT NULL
                               )
                           )
                           -- Nor does an applied snooze whose time has come.
                           -- Without this the snooze exclusion above would be
                           -- dead logic: a snoozed message already has a
                           -- logged action, so it would be excluded by *that*
                           -- clause forever and the expiry would never
                           -- arrive. Deferring until T has to mean the agent
                           -- looks again at T.
                           AND NOT (
                               a.action = 'snooze'
                               AND a.outcome = 'applied'
                               AND NOT EXISTS (
                                   SELECT 1 FROM message_snoozes s2
                                    WHERE s2.message_id = m.id AND s2.until > ?3
                               )
                           )
                    )
                  ORDER BY COALESCE(m.date, m.internaldate, 0) DESC, m.id DESC
                  LIMIT ?4",
            )?;
            let ids = stmt
                .query_map(rusqlite::params![account_id, mailbox, now, limit], |row| {
                    row.get::<_, i64>(0)
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(ids)
        })
        .await?)
}
