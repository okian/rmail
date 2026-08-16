//! How a smart folder's membership is computed — one deterministic arm for
//! every folder, plus the lexical and dense arms an NL-compiled hybrid plan
//! adds (task 58).
//!
//! # The three arms, and how they combine
//!
//! ```text
//!   members = <hard filters>  AND  ( <FTS match>  OR  <embedding kNN ≥ floor> )
//! ```
//!
//! Hard filters *gate*, exactly as they do in Stage 1 of the search pipeline
//! (prd.md: "Hard filters gate everything") — `from:stripe` is a `WHERE`
//! constraint, never a hint. The two text arms are a *union* because they
//! answer the same question two ways: FTS finds the message that used the
//! words, the embedding finds the one that paraphrased them. Requiring both
//! would make the dense arm pointless (it can only ever narrow a set the
//! lexical arm already found), and there is no third reading.
//!
//! A folder with no free text has no text arms at all and is exactly the
//! deterministic smart folder task 35 shipped — same SQL, same statement, no
//! behaviour change.
//!
//! # An arm that resolves to nothing still constrains
//!
//! The single most dangerous mistake available here is to *omit* a clause
//! whose input came back empty. A dense arm that matched nothing must compile
//! to `0`, not to nothing: dropping it would leave `WHERE <hard filters>`, and
//! a folder defined as "anything about the lease" would silently become "every
//! message in the account", re-confirmed as correct on every sync with nobody
//! watching — the exact failure [`super::validate_predicate`] exists to
//! prevent, arrived at from the other end. [`Membership::sql`] therefore
//! builds the arm list from the *plan*, never from what the arms returned.
//!
//! # The dense arm is bounded, and says so
//!
//! `vec_chunks` is a kNN index: it answers "the nearest `k`", not "everything
//! above a threshold". So the dense arm fetches a bounded `k`
//! ([`SEMANTIC_FETCH`]), keeps what clears the folder's stored cosine floor,
//! and contributes at most [`MAX_SEMANTIC_MEMBERS`] messages. A hybrid folder's
//! membership is therefore bounded in a way a deterministic one is not, and a
//! message can leave the folder because *other* messages moved closer to the
//! query — the honest consequence of a nearest-neighbour arm, stated here
//! rather than discovered later.
//!
//! That has a consequence for actions worth naming outright, because it is a
//! real cost and not a hypothetical. [`super`]'s ledger deletes departures
//! ("a member that leaves and returns is new again"), so a message evicted by
//! this cap and later readmitted fires `auto_tag`/`notify` a second time. For
//! task 35's deterministic folders that only happened when the *message*
//! changed — it was read, it was tagged — which a user can reason about. Here
//! it can happen because unrelated mail arrived and crowded the neighbourhood,
//! which they cannot. The mitigations are the ones already in place rather
//! than new machinery: `auto_tag` is idempotent by construction (the
//! `message_tags` partial unique index), so a re-fire re-applies a tag the
//! message already carries and creates nothing; a duplicate notification is
//! what it costs. A folder whose membership genuinely exceeds
//! [`MAX_SEMANTIC_MEMBERS`] is one whose predicate is too broad to be firing
//! actions at all, and the cap is what keeps that failure bounded instead of
//! unbounded.
//!
//! # Nothing here embeds anything
//!
//! The query vector is frozen into the folder at create time (migration V47).
//! An evaluation runs one kNN against bytes already on disk: no provider call,
//! no local embedder, nothing that can fail slowly. That is what makes
//! "re-run cheaply each sync" true rather than aspirational.

use rusqlite::types::Value;
use rusqlite::Connection;

use crate::embed::Embedding;
use crate::index::semantic::VECTOR_DIM;
use crate::query::parse::ParsedQuery;
use crate::retrieve::lexical::MatchExpr;
use crate::tags::query as filter_query;

/// How many nearest chunks the dense arm asks `vec_chunks` for.
///
/// Chunk-granular and pre-filter, like every other consumer of this index, so
/// it is deliberately larger than the message ceiling below: several chunks of
/// one long message routinely occupy the same neighbourhood.
const SEMANTIC_FETCH: i64 = 2_000;

/// The most messages the dense arm contributes to one folder.
///
/// A ceiling rather than a page: this is membership, not a result page, so
/// there is no "next page" to reach the rest with. See the module docs on why
/// the bound is inherent to a kNN arm.
pub const MAX_SEMANTIC_MEMBERS: usize = 500;

/// The cosine floor a message must clear to enter through the dense arm,
/// when the folder does not carry its own.
///
/// Stored per folder at create time (V47's `min_similarity`) precisely so that
/// revising this constant cannot silently redefine what an existing folder
/// contains. It is a floor on *similarity to the compiled free text*, not a
/// relevance cutoff over a ranked page — a smart folder has no page to cut.
pub const DEFAULT_MIN_SIMILARITY: f64 = 0.6;

/// The dense arm's frozen inputs.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct DenseArm {
    /// The query vector, as stored.
    pub(super) vector: Embedding,
    /// The embedding model that produced it. The join filters on this, so a
    /// re-index under a different model degrades this arm to empty rather
    /// than comparing vectors from two different spaces.
    pub(super) model: String,
    /// The cosine floor.
    pub(super) min_similarity: f64,
}

/// One folder's compiled membership query.
///
/// Built purely — no I/O — from the folder's stored predicate and plan, so a
/// caller can decide what reads to run before running any.
pub(super) struct Membership {
    /// The hard-filter `WHERE` fragment, account scope included.
    where_sql: String,
    /// Its bound parameters, in placeholder order.
    params: Vec<Value>,
    /// How many operators became SQL — 0 means the fragment is the bare
    /// account scope.
    filters: usize,
    /// The FTS5 `MATCH` expression for the lexical arm, when the predicate
    /// has free text the tokenizer can use.
    match_expr: Option<String>,
    /// The dense arm, when the folder's frozen query vector could actually be
    /// loaded.
    dense: Option<DenseArm>,
    /// Whether the folder *declares* a dense arm, whether or not one loaded.
    ///
    /// The distinction is the whole safety property. `dense` is `None` both
    /// for a folder that never had a vector and for one whose stored blob
    /// would not decode — a re-index at a different width, a truncated row.
    /// Building the `0` arm from `dense` would therefore *drop* the clause in
    /// the second case, leaving `WHERE <hard filters>`: a folder defined as
    /// "anything about the lease" would silently become "every message in the
    /// account", re-confirmed as correct on every sync, and its very first
    /// evaluation would auto-tag and notify for the entire mailbox. So the arm
    /// list is built from what the folder *declared*, exactly as the module
    /// docs claim, and this flag is what makes that claim true rather than
    /// aspirational.
    dense_declared: bool,
}

impl Membership {
    /// Compile `predicate` (plus an optional dense arm) into a membership
    /// query scoped to `account_id`.
    ///
    /// Pure. Whether the predicate is *allowed* to contain free text is
    /// [`super::validate_predicate`]/[`super::validate_hybrid_predicate`]'s
    /// question, asked at create time; by the time a folder is stored, free
    /// text in it means the hybrid arms, and this builds them.
    /// `dense_declared` is whether the folder *row* claims a dense arm
    /// (`vector_model` is set), which is not the same question as whether
    /// `dense` is `Some` — see that field's own docs.
    pub(super) fn compile(
        account_id: i64,
        predicate: &str,
        dense: Option<DenseArm>,
        dense_declared: bool,
    ) -> Self {
        let compiled = filter_query::compile_detailed(account_id, predicate);
        let parsed = crate::query::parse::parse(predicate);
        Self {
            where_sql: compiled.where_sql,
            params: compiled.params,
            filters: compiled.applied,
            match_expr: match_expr_for(&parsed),
            dense_declared: dense_declared || dense.is_some(),
            dense,
        }
    }

    /// Whether this folder needs the dense arm resolved before its membership
    /// SQL can run.
    pub(super) fn dense_arm(&self) -> Option<&DenseArm> {
        self.dense.as_ref()
    }

    /// Whether this plan constrains anything, given whether a query vector is
    /// available for the dense arm.
    ///
    /// The question [`super::SmartFolderStore::create`] asks last, and the
    /// only one that matters for the invariant: a plan for which this is false
    /// compiles to the bare `account_id = ?` scope, which is every message in
    /// the account. Takes the vector's availability as an argument rather than
    /// reading `self.dense`, because create-time is precisely when the arm may
    /// be *intended* and yet absent — a failed embedder — and answering from a
    /// `None` this build set for that reason would be answering the wrong
    /// question.
    pub(super) fn constrains(&self, has_vector: bool) -> bool {
        self.filters > 0 || self.match_expr.is_some() || has_vector
    }

    /// The membership statement and its parameters.
    ///
    /// `dense_ids` is what [`resolve_dense`] returned for this folder, and is
    /// ignored unless the plan actually has a dense arm — see the module docs
    /// on why an empty result becomes `0` rather than a dropped clause.
    fn sql(&self, dense_ids: &[i64], limit: Option<usize>) -> (String, Vec<Value>) {
        let mut params = self.params.clone();
        let mut arms: Vec<String> = Vec::new();

        if let Some(expr) = &self.match_expr {
            // An uncorrelated `IN` against the FTS table, the same shape
            // `export::select` uses: the driving scan is `messages` by
            // primary key and the match set does not depend on the row being
            // tested, so the subquery is evaluated once.
            arms.push(
                "id IN (SELECT rowid FROM fts_messages WHERE fts_messages MATCH ?)".to_owned(),
            );
            params.push(Value::Text(expr.clone()));
        }
        if self.dense_declared {
            if dense_ids.is_empty() {
                arms.push("0".to_owned());
            } else {
                // The ids are integers this crate just read out of its own
                // index, never caller text, so formatting the placeholder
                // list is not an injection surface — and they cannot be bound
                // as one parameter without `rarray`, which would disturb the
                // positional order `compile_detailed` already owns.
                let holes = vec!["?"; dense_ids.len()].join(",");
                arms.push(format!("id IN ({holes})"));
                params.extend(dense_ids.iter().map(|id| Value::Integer(*id)));
            }
        }

        if self.filters == 0 && arms.is_empty() {
            // The unconditional floor, and the last line of defence for this
            // module's entire reason to exist. Every path that reaches here is
            // supposed to have been refused at create time
            // (`SmartFolderStore::create`'s `constrains` check) — but "supposed
            // to" is exactly what `dense_declared` above also used to be, and
            // the cost of being wrong is the whole account inside a folder that
            // then auto-tags and notifies for it. A folder that constrains
            // nothing holds nothing: wrong in the safe direction, and visible
            // (an empty folder someone reports) rather than silent (a full one
            // nobody looks at).
            tracing::error!(
                "a stored smart folder predicate constrains nothing; treating it as empty \
                 rather than as the whole account — this row should have been refused when \
                 it was created"
            );
            arms.push("0".to_owned());
        }

        let mut sql = format!("SELECT id FROM messages WHERE {}", self.where_sql);
        if !arms.is_empty() {
            sql.push_str(&format!(" AND ({})", arms.join(" OR ")));
        }
        sql.push_str(" ORDER BY id");
        if let Some(limit) = limit {
            // An integer this crate computed, for the reason above.
            sql.push_str(&format!(" LIMIT {limit}"));
        }
        (sql, params)
    }

    /// Run the membership statement, ascending by id.
    ///
    /// `ORDER BY id` is load-bearing, not cosmetic: an evaluation diffs
    /// consecutive runs of this list against each other, and an unordered
    /// result would make that diff depend on whichever plan SQLite picked.
    pub(super) fn select(
        &self,
        conn: &Connection,
        dense_ids: &[i64],
        limit: Option<usize>,
    ) -> rusqlite::Result<Vec<i64>> {
        let (sql, params) = self.sql(dense_ids, limit);
        let mut stmt = conn.prepare(&sql)?;
        let bound: Vec<&dyn rusqlite::ToSql> = params
            .iter()
            .map(|value| value as &dyn rusqlite::ToSql)
            .collect();
        let ids = stmt
            .query_map(bound.as_slice(), |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<i64>>>()?;
        Ok(ids)
    }
}

/// The FTS5 `MATCH` expression a predicate's free text compiles to, or `None`
/// when it has none the tokenizer can use.
///
/// [`MatchExpr::build`] is the *same* builder the ranked search path uses, and
/// reusing it is the point: a folder that disagreed with `mail search` about
/// what `"office move" -draft` matches would be a second, silently divergent
/// reading of the grammar. Its proximity probe is dropped — a relevance boost
/// has no meaning for a set — exactly as [`crate::export::select`] drops it.
fn match_expr_for(parsed: &ParsedQuery) -> Option<String> {
    MatchExpr::build(parsed).map(|expr| expr.full)
}

/// Resolve the dense arm: the messages whose nearest chunk clears the floor.
///
/// Returns ids ascending and deduplicated, at most [`MAX_SEMANTIC_MEMBERS`] of
/// them, ordered by *similarity* before the cut so the bound keeps the closest
/// matches rather than the numerically smallest ids.
///
/// # Errors
/// A storage error. A zero vector, or one whose width does not match the
/// index, resolves to no messages rather than erroring — the same degradation
/// [`crate::retrieve::dense`] applies, and for the same reason: a stale or
/// malformed vector must not be able to make a folder unreadable.
pub(super) fn resolve_dense(conn: &Connection, arm: &DenseArm) -> rusqlite::Result<Vec<i64>> {
    if arm.vector.dim() != VECTOR_DIM || arm.vector.as_slice().iter().all(|v| *v == 0.0) {
        tracing::warn!(
            dim = arm.vector.dim(),
            expected = VECTOR_DIM,
            "a smart folder's stored query vector cannot search this semantic index; \
             its dense arm contributes nothing"
        );
        return Ok(Vec::new());
    }
    let bytes = arm.vector.to_bytes();
    let dim = i64::try_from(VECTOR_DIM).unwrap_or(i64::MAX);
    let mut stmt = conn.prepare(
        "WITH hits AS (
             SELECT chunk_id, distance FROM vec_chunks
              WHERE embedding MATCH ?1 AND k = ?2
         )
         SELECT c.message_id, MAX(1.0 - h.distance * h.distance / 2.0) AS similarity
           FROM hits h
           JOIN chunks c ON c.chunk_id = h.chunk_id
           JOIN chunk_embeddings e ON e.chunk_id = h.chunk_id
          WHERE e.model = ?3 AND e.dim = ?4 AND e.content_hash = c.content_hash
          GROUP BY c.message_id
         HAVING similarity >= ?5
          ORDER BY similarity DESC, c.message_id
          LIMIT ?6",
    )?;
    let limit = i64::try_from(MAX_SEMANTIC_MEMBERS).unwrap_or(i64::MAX);
    let mut ids = stmt
        .query_map(
            rusqlite::params![
                bytes,
                SEMANTIC_FETCH,
                arm.model,
                dim,
                arm.min_similarity,
                limit
            ],
            |row| row.get::<_, i64>(0),
        )?
        .collect::<rusqlite::Result<Vec<i64>>>()?;
    ids.sort_unstable();
    ids.dedup();
    Ok(ids)
}
