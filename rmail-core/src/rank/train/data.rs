//! Reading the implicit-feedback log back out as training data.
//!
//! This is the only reader of `search_log`/`search_impression`/`search_action`
//! in the codebase, and it is deliberately in-process only: prd.md's "it is
//! local telemetry, never transmitted" is held by there being no RPC that
//! returns any of this, and nothing here changes that — a trainer that reads
//! the log and a surface that exposes it are different things.
//!
//! # Three statements, not one join and not one per query
//!
//! A single join across the three tables would return one row per
//! (impression x action) pair and re-send the ~0.8 kB serialized feature
//! vector on every one of them. A per-query loop would be three statements
//! per logged query, which at the default bound is six thousand round trips
//! through the connection pool. So: three statements, each restricted to the
//! same newest-`limit` window by the same subquery, all inside one
//! [`crate::storage::Database::read`] so they see one consistent snapshot.
//!
//! # Why `search_impression.l1_score` is not read
//!
//! V34's own header suggests it: the score is stored "so task 65 can verify a
//! replay reproduces it before trusting a decoded vector". That check is not
//! implementable as stated, and running it anyway would be worse than
//! skipping it. The stored score came from whichever ranker was live *at log
//! time*, and the point of this module is that the live ranker changes — so
//! after the first accepted model, every row in the log disagrees with the
//! current one by construction. A verification that fires on every row after
//! the first swap is a verification nobody will leave switched on.
//!
//! What the column is genuinely good for is a human debugging a specific
//! query ("what did the ranker actually score this at, that day"), which is
//! why it stays. The property it was meant to protect — that a decoded vector
//! is byte-identical to the one that was scored — is instead held by
//! `feedback::encode_features`/`decode_features` being an exact round trip
//! over a versioned envelope, and pinned by
//! `rank::train::tests::the_trainer_reads_back_the_exact_vector_the_ranker_scored`.
//!
//! # Why decoding happens outside the database closure
//!
//! Deserializing tens of thousands of JSON feature vectors is the expensive
//! part of a training run, and doing it inside the read closure would hold a
//! pooled connection for its duration. It also cannot be interrupted there:
//! the closure has no natural place to observe cancellation. [`decode`] runs
//! on the caller's blocking task, checks the token as it goes, and hands back
//! whichever whole queries it managed to build.

use rusqlite::Connection;
use tokio_util::sync::CancellationToken;

use crate::feedback::{decode_features, parse_intent, ActionKind};

use super::labels::{LoggedQuery, ObservedAction, ShownResult};
use super::TrainError;

/// One `search_log` row, undecoded.
pub(crate) struct RawQuery {
    pub(crate) query_id: i64,
    pub(crate) raw_query: String,
    pub(crate) group_key: Vec<u8>,
    pub(crate) intent: Option<String>,
}

/// One `search_impression` row, feature blob still serialized.
pub(crate) struct RawImpression {
    pub(crate) query_id: i64,
    pub(crate) message_id: i64,
    pub(crate) position: i64,
    pub(crate) features: Vec<u8>,
}

/// One `search_action` row.
pub(crate) struct RawAction {
    pub(crate) query_id: i64,
    pub(crate) message_id: i64,
    pub(crate) action: String,
    pub(crate) dwell_ms: Option<i64>,
}

/// Everything one training run reads, before decoding.
pub(crate) struct RawFeedback {
    pub(crate) queries: Vec<RawQuery>,
    pub(crate) impressions: Vec<RawImpression>,
    pub(crate) actions: Vec<RawAction>,
}

/// The newest `limit` logged queries, with their impressions and actions.
///
/// Newest first because relevance is a moving target: a mailbox's vocabulary,
/// its correspondents and its shape all drift, and a bound that dropped
/// *recent* data to keep old data would train the ranker the user had a year
/// ago. Retention (`[search.feedback]`) already caps how far back the log
/// goes; this caps how much of it one run holds in memory at once.
pub(crate) fn load(conn: &Connection, limit: i64) -> rusqlite::Result<RawFeedback> {
    const WINDOW: &str =
        "SELECT query_id FROM search_log ORDER BY issued_at DESC, query_id DESC LIMIT ?1";

    let mut queries = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT query_id, raw_query, norm_hash, intent FROM search_log
             ORDER BY issued_at DESC, query_id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit], |row| {
            Ok(RawQuery {
                query_id: row.get(0)?,
                raw_query: row.get(1)?,
                group_key: row.get(2)?,
                intent: row.get(3)?,
            })
        })?;
        for row in rows {
            queries.push(row?);
        }
    }

    let mut impressions = Vec::new();
    {
        let mut stmt = conn.prepare(&format!(
            "SELECT query_id, message_id, position, features FROM search_impression
             WHERE query_id IN ({WINDOW})
             ORDER BY query_id, position, message_id"
        ))?;
        let rows = stmt.query_map([limit], |row| {
            Ok(RawImpression {
                query_id: row.get(0)?,
                message_id: row.get(1)?,
                position: row.get(2)?,
                features: row.get(3)?,
            })
        })?;
        for row in rows {
            impressions.push(row?);
        }
    }

    let mut actions = Vec::new();
    {
        let mut stmt = conn.prepare(&format!(
            "SELECT query_id, message_id, action, dwell_ms FROM search_action
             WHERE query_id IN ({WINDOW})"
        ))?;
        let rows = stmt.query_map([limit], |row| {
            Ok(RawAction {
                query_id: row.get(0)?,
                message_id: row.get(1)?,
                action: row.get(2)?,
                dwell_ms: row.get(3)?,
            })
        })?;
        for row in rows {
            actions.push(row?);
        }
    }

    Ok(RawFeedback {
        queries,
        impressions,
        actions,
    })
}

/// What decoding produced, and what it had to leave behind.
pub(crate) struct Decoded {
    pub(crate) queries: Vec<LoggedQuery>,
    /// Logged queries dropped whole. Reported rather than swallowed: a
    /// training run that silently ignored nine tenths of the corpus would
    /// still produce a model and still measure it, and the only symptom would
    /// be that personalization never seems to do much.
    pub(crate) skipped: usize,
}

/// Turn raw rows into [`LoggedQuery`]s, dropping any query that cannot be
/// replayed exactly.
///
/// A query is dropped whole — never partially — when its intent is missing or
/// unrecognized, when it showed nothing, or when any one of its impressions
/// fails to decode. The all-or-nothing rule is what protects the skip-above
/// labelling rule in [`super::labels`]: that rule reads "the results ranked
/// above the clicked one", and a page missing its third result would produce
/// preferences against documents the user never actually passed over.
///
/// # Errors
///
/// [`TrainError::Cancelled`] if `cancel` fires. Checked once per query, which
/// bounds the delay by one page's worth of JSON.
pub(crate) fn decode(raw: RawFeedback, cancel: &CancellationToken) -> Result<Decoded, TrainError> {
    use std::collections::HashMap;

    let mut impressions: HashMap<i64, Vec<&RawImpression>> = HashMap::new();
    for row in &raw.impressions {
        impressions.entry(row.query_id).or_default().push(row);
    }
    let mut actions: HashMap<i64, Vec<&RawAction>> = HashMap::new();
    for row in &raw.actions {
        actions.entry(row.query_id).or_default().push(row);
    }

    let mut queries = Vec::with_capacity(raw.queries.len());
    let mut skipped = 0usize;
    for query in &raw.queries {
        if cancel.is_cancelled() {
            return Err(TrainError::Cancelled);
        }
        let Some(intent) = query.intent.as_deref().and_then(parse_intent) else {
            tracing::debug!(
                query_id = query.query_id,
                intent = ?query.intent,
                "skipping a logged query whose intent cannot be replayed"
            );
            skipped += 1;
            continue;
        };
        let Some(rows) = impressions.get(&query.query_id) else {
            skipped += 1;
            continue;
        };

        let mut shown = Vec::with_capacity(rows.len());
        let mut broken = false;
        for row in rows {
            match decode_features(&row.features) {
                Ok(features) => shown.push(ShownResult {
                    message_id: row.message_id,
                    position: u32::try_from(row.position).unwrap_or(u32::MAX),
                    features,
                }),
                Err(error) => {
                    tracing::debug!(
                        query_id = query.query_id,
                        message_id = row.message_id,
                        %error,
                        "skipping a logged query with an undecodable feature vector"
                    );
                    broken = true;
                    break;
                }
            }
        }
        if broken || shown.is_empty() {
            skipped += 1;
            continue;
        }

        let observed = actions
            .get(&query.query_id)
            .into_iter()
            .flatten()
            .filter_map(|row| {
                // An action whose verb no version of this build writes is
                // dropped rather than guessed at. `ActionKind` is a closed
                // vocabulary enforced on every write, so this can only be a
                // row from a future build — and inventing a grade for it
                // would be inventing a label.
                ActionKind::parse(&row.action).map(|kind| ObservedAction {
                    message_id: row.message_id,
                    kind,
                    dwell_ms: row.dwell_ms,
                })
            })
            .collect();

        queries.push(LoggedQuery {
            query_id: query.query_id,
            raw_query: query.raw_query.clone(),
            group_key: query.group_key.clone(),
            intent,
            shown,
            actions: observed,
        });
    }

    Ok(Decoded { queries, skipped })
}
