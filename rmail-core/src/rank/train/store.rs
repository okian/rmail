//! `ranker_model` (migration V54): the model history, and the transaction
//! that moves the `active` flag.
//!
//! # Why promotion and demotion are one transaction
//!
//! `ranker_model` has a UNIQUE index over `active`, so "clear the old, set the
//! new" is two statements that cannot both be skipped and must not both be
//! visible half-done. Rolled together, a crash between them leaves either the
//! old model live or the new one, never neither — and "neither" is the state
//! that would silently drop a mailbox back to cold start with no log line
//! saying so.
//!
//! # Why a rejected candidate can never be activated
//!
//! [`activate`] refuses any row whose `status` is not `accepted`, and the
//! schema carries the same rule as a `CHECK`. The guardrail is the point of
//! this task; a hand-activation path around it would make the guardrail
//! advisory. An operator who believes a refused candidate was better has one
//! honest move — collect more feedback and train again — and one dishonest
//! one this deliberately does not provide.

use rusqlite::{Connection, OptionalExtension};

/// The status vocabulary in `ranker_model.status`.
///
/// Two values, not three: "was live, then something newer replaced it" is not
/// a distinct state, it is `accepted` without the `active` flag. A third
/// `superseded` status would have to be moved back to `accepted` by a
/// rollback, which is how a rollback-then-rollback ends up oscillating
/// between two models instead of walking backwards through history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelStatus {
    /// Beat the live model on the held-out slice by the configured margin.
    /// Eligible to be live, now or after a rollback.
    Accepted,
    /// Did not. Kept for the audit trail, never runnable.
    Rejected,
}

impl ModelStatus {
    /// The stored string. Never `Debug`'s capitalization — this is on disk.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            ModelStatus::Accepted => "accepted",
            ModelStatus::Rejected => "rejected",
        }
    }

    /// Parse [`ModelStatus::as_str`]'s output back.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "accepted" => Some(ModelStatus::Accepted),
            "rejected" => Some(ModelStatus::Rejected),
            _ => None,
        }
    }
}

/// One row of the model history, as an operator surface sees it. Never
/// carries the weights: a weight table is not something a listing needs, and
/// keeping it out means `ListRankerModels` cannot become a way to exfiltrate
/// a behavioural fingerprint of the mailbox.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelRecord {
    /// `ranker_model.id`.
    pub id: i64,
    /// Unix seconds.
    pub created_at: i64,
    /// Model family; `linear` for everything this build writes.
    pub kind: String,
    /// Guardrail verdict.
    pub status: ModelStatus,
    /// Whether this row carries the `active` flag. Note this is the *stored*
    /// flag: see [`super::Trainer::models`] for the one case in which it can
    /// disagree with what is actually running.
    pub active: bool,
    /// Logged queries the run trained on.
    pub train_queries: u32,
    /// Preference pairs it fitted against.
    pub train_pairs: u32,
    /// Held-out queries it was judged on...
    pub eval_queries: u32,
    /// ...and how many of those carried any engagement at all.
    pub eval_engaged: u32,
    /// Held-out NDCG@10 of the model that was live when this was judged.
    pub baseline_ndcg: f64,
    /// Held-out NDCG@10 of this candidate.
    pub candidate_ndcg: f64,
    /// Free-text provenance.
    pub note: String,
}

/// A candidate about to be written.
pub(crate) struct NewModel {
    pub(crate) kind: &'static str,
    pub(crate) weights: Vec<u8>,
    pub(crate) status: ModelStatus,
    pub(crate) train_queries: u32,
    pub(crate) train_pairs: u32,
    pub(crate) eval_queries: u32,
    pub(crate) eval_engaged: u32,
    pub(crate) baseline_ndcg: f64,
    pub(crate) candidate_ndcg: f64,
    pub(crate) note: String,
}

/// Insert a candidate and, when `promote` is set, make it the live model in
/// the same transaction.
///
/// Returns the new row's id.
pub(crate) fn insert(
    conn: &mut Connection,
    model: &NewModel,
    promote: bool,
) -> rusqlite::Result<i64> {
    let tx = conn.transaction()?;
    if promote {
        // The old model keeps its `accepted` status and loses only the flag,
        // which is what makes it a rollback target rather than a tombstone.
        tx.execute("UPDATE ranker_model SET active = NULL WHERE active = 1", [])?;
    }
    tx.execute(
        "INSERT INTO ranker_model
             (kind, weights, status, active, train_queries, train_pairs,
              eval_queries, eval_engaged, baseline_ndcg, candidate_ndcg, note)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        rusqlite::params![
            model.kind,
            model.weights,
            model.status.as_str(),
            if promote { Some(1i64) } else { None },
            i64::from(model.train_queries),
            i64::from(model.train_pairs),
            i64::from(model.eval_queries),
            i64::from(model.eval_engaged),
            model.baseline_ndcg,
            model.candidate_ndcg,
            model.note,
        ],
    )?;
    let id = tx.last_insert_rowid();
    tx.commit()?;
    Ok(id)
}

/// The live model's id, weights blob and kind, if any row carries the flag.
pub(crate) fn active(conn: &Connection) -> rusqlite::Result<Option<(i64, Vec<u8>, String)>> {
    conn.query_row(
        "SELECT id, weights, kind FROM ranker_model WHERE active = 1",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )
    .optional()
}

/// One model's weights blob and kind by id, and whether it is accepted.
pub(crate) fn by_id(
    conn: &Connection,
    id: i64,
) -> rusqlite::Result<Option<(Vec<u8>, String, String)>> {
    conn.query_row(
        "SELECT weights, kind, status FROM ranker_model WHERE id = ?1",
        [id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )
    .optional()
}

/// The newest accepted model strictly older than `before`.
///
/// "Strictly older" is what makes repeated rollbacks walk backwards through
/// history instead of ping-ponging between the two newest models.
///
/// There is deliberately no "newest accepted model at all" mode. That query
/// looks like the natural answer for "roll back when nothing is live", and it
/// is exactly wrong: with the deterministic scorer running it would
/// re-activate the very model the operator had just stepped off, turning a
/// rollback into a roll *forward*. [`super::Trainer::rollback`] answers that
/// case without a query at all.
pub(crate) fn rollback_target(conn: &Connection, before: i64) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT id FROM ranker_model
         WHERE status = 'accepted' AND id < ?1
         ORDER BY id DESC LIMIT 1",
        [before],
        |row| row.get(0),
    )
    .optional()
}

/// What [`activate`] found.
pub(crate) enum Activation {
    /// The flag now sits on the requested row.
    Activated,
    /// No row with that id.
    Unknown,
    /// The row exists but the guardrail refused it. See the module docs.
    Rejected,
}

/// Move the `active` flag onto `id`, atomically.
pub(crate) fn activate(conn: &mut Connection, id: i64) -> rusqlite::Result<Activation> {
    let tx = conn.transaction()?;
    let status: Option<String> = tx
        .query_row(
            "SELECT status FROM ranker_model WHERE id = ?1",
            [id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(status) = status else {
        return Ok(Activation::Unknown);
    };
    if ModelStatus::parse(&status) != Some(ModelStatus::Accepted) {
        return Ok(Activation::Rejected);
    }
    tx.execute("UPDATE ranker_model SET active = NULL WHERE active = 1", [])?;
    tx.execute("UPDATE ranker_model SET active = 1 WHERE id = ?1", [id])?;
    tx.commit()?;
    Ok(Activation::Activated)
}

/// Clear the flag, whatever holds it. The deterministic scorer takes over.
pub(crate) fn deactivate(conn: &Connection) -> rusqlite::Result<usize> {
    conn.execute("UPDATE ranker_model SET active = NULL WHERE active = 1", [])
}

/// Newest-first history, at most `limit` rows.
pub(crate) fn list(conn: &Connection, limit: i64) -> rusqlite::Result<Vec<ModelRecord>> {
    let mut stmt = conn.prepare(
        "SELECT id, created_at, kind, status, active, train_queries, train_pairs,
                eval_queries, eval_engaged, baseline_ndcg, candidate_ndcg, note
         FROM ranker_model ORDER BY id DESC LIMIT ?1",
    )?;
    let rows = stmt.query_map([limit], |row| {
        let status: String = row.get(3)?;
        let active: Option<i64> = row.get(4)?;
        Ok(ModelRecord {
            id: row.get(0)?,
            created_at: row.get(1)?,
            kind: row.get(2)?,
            // A status string outside the vocabulary cannot reach here — the
            // column has a CHECK — and reading it as `Rejected` is the
            // fail-safe direction regardless: a row this build cannot
            // classify is one it must not offer as a rollback target.
            status: ModelStatus::parse(&status).unwrap_or(ModelStatus::Rejected),
            active: active == Some(1),
            train_queries: u32::try_from(row.get::<_, i64>(5)?).unwrap_or(u32::MAX),
            train_pairs: u32::try_from(row.get::<_, i64>(6)?).unwrap_or(u32::MAX),
            eval_queries: u32::try_from(row.get::<_, i64>(7)?).unwrap_or(u32::MAX),
            eval_engaged: u32::try_from(row.get::<_, i64>(8)?).unwrap_or(u32::MAX),
            baseline_ndcg: row.get(9)?,
            candidate_ndcg: row.get(10)?,
            note: row.get(11)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Drop the oldest rows past `keep` — `keep` of *each* status, and never the
/// live model.
///
/// # Why the two statuses are counted separately
///
/// One shared budget looks simpler and quietly destroys the thing this table
/// exists for. A guardrail that is doing its job refuses most candidates, so
/// on a mailbox where relevance has plateaued the history fills with rejected
/// rows — and under a single cap those would evict the accepted models, which
/// are exactly the rollback targets prd.md's "old model kept for rollback"
/// names. The symptom would be that rollback works right up until the night
/// you need it. Counting per status makes a run of refusals unable to touch
/// the accepted history, and bounds both.
///
/// The live model is excluded by the `active IS NULL` predicate rather than
/// by being inside the kept window, so it is exempt from the count as well as
/// from the deletion: "keep 20" leaves the live model *plus* twenty accepted
/// and twenty rejected.
pub(crate) fn prune(conn: &Connection, keep: i64) -> rusqlite::Result<usize> {
    let mut removed = 0usize;
    for status in [ModelStatus::Accepted, ModelStatus::Rejected] {
        removed += conn.execute(
            "DELETE FROM ranker_model
             WHERE active IS NULL AND status = ?1
               AND id NOT IN (
                   SELECT id FROM ranker_model
                    WHERE active IS NULL AND status = ?1
                    ORDER BY id DESC LIMIT ?2
               )",
            rusqlite::params![status.as_str(), keep.max(1)],
        )?;
    }
    Ok(removed)
}
