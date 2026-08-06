//! The entity match retriever: prd.md's "people, orgs, amounts, tracking #s,
//! order/invoice IDs, IBANs" row, over task 19's `entities`/`entity_mentions`.
//!
//! # Patterns, not contacts
//!
//! [`QueryPlan::entities`](crate::query::QueryPlan::entities) mixes two kinds
//! of evidence: [`EntityRefKind::Pattern`](crate::query::EntityRefKind::Pattern)
//! spans task 19's regex extractors recognized in the query text itself (an
//! amount, an order number, an IBAN — real rows in `entities`/
//! `entity_mentions`), and [`EntityRefKind::Contact`](crate::query::EntityRefKind::Contact)
//! matches against the contact graph (a name or address fragment resolved to
//! someone task 19 has never extracted as an "entity" — contacts have no
//! `entities` row at all). This retriever only ever queries the former: a
//! contact match is what `from:`/`to:` soft-boosting and `sender_affinity`
//! (task 30) are for, and treating it as an entity-graph lookup here would
//! either find nothing (a contact with no message mentioning their address as
//! literal text) or double-count a hit the feature stage already credits
//! elsewhere.
//!
//! # One query, not one per entity
//!
//! [`QueryPlan::entities`] is capped at a few dozen refs
//! (`query::plan::MAX_ENTITY_REFS`), each already carrying task 26's own
//! confidence-derived `boost`. Rather than run one `SELECT` per ref — a
//! round trip per entity for what is usually one or two — every pattern ref
//! is matched in a single query via a `(kind, norm) IN (VALUES ...)` row-value
//! list, and the per-message score is the sum of `ref.boost * mention.confidence`
//! over every ref that message actually mentions: a message that matches two
//! of the query's entities is stronger evidence than one that matches only
//! one, and `MAX(confidence)` per `(message, entity)` pair means a mailing-
//! list footer repeating the same entity forty times does not inflate the
//! score beyond what one confident mention already established.

use std::collections::BTreeMap;

use rusqlite::types::Value;
use tokio_util::sync::CancellationToken;

use super::cancel::interruptible_read;
use super::filtermask::{self, FilterMask};
use super::{rank_by_score, Candidate, Source};
use crate::error::Error;
use crate::index::fts::MAX_LIMIT;
use crate::query::{EntityRefKind, QueryPlan};
use crate::storage::Database;

/// Safety cap on raw `(message, entity)` rows fetched before aggregation —
/// not the result page size (`limit` governs that, applied after scoring).
/// Bounds the pathological case of a query entity with an unusually large
/// number of mentions across the mailbox, the same way every other retriever
/// in this module bounds its own worst case rather than trusting the corpus
/// to stay small.
const MAX_RAW_MENTIONS: i64 = 5_000;

/// Exact/normalized entity-match retrieval over `entities`/`entity_mentions`.
#[derive(Debug, Clone)]
pub struct EntityRetriever {
    db: Database,
}

impl EntityRetriever {
    /// Build a retriever over `db`.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Retrieve up to `limit` messages that mention one of `plan`'s
    /// pattern-shaped entities, scored by summed boost × confidence.
    ///
    /// Returns an empty list, not an error, when `plan` resolved no
    /// pattern-shaped entities (a query with no `EntityRefKind::Pattern` ref —
    /// see the module docs — degrades this source to nothing, same as every
    /// other retriever with nothing to work with) or when a hard filter
    /// provably excludes every message.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    #[tracing::instrument(skip(self, plan, cancel), fields(refs, hits))]
    pub async fn retrieve(
        &self,
        plan: &QueryPlan,
        limit: i64,
        cancel: &CancellationToken,
    ) -> Result<Vec<Candidate>, Error> {
        let refs: Vec<(String, String, f64)> = plan
            .entities
            .iter()
            .filter_map(|e| match &e.kind {
                EntityRefKind::Pattern(kind) => {
                    Some((kind.as_str().to_owned(), e.norm.clone(), e.boost))
                }
                EntityRefKind::Contact => None,
            })
            .collect();
        tracing::Span::current().record("refs", refs.len());
        if refs.is_empty() {
            return Ok(Vec::new());
        }

        let mask = filtermask::compile(&plan.hard_filters);
        if matches!(mask, FilterMask::ExcludesEverything) {
            return Ok(Vec::new());
        }

        let values_sql = (0..refs.len())
            .map(|i| format!("(?{}, ?{})", i * 2 + 1, i * 2 + 2))
            .collect::<Vec<_>>()
            .join(", ");
        let mut sql = format!(
            "SELECT em.message_id, e.kind, e.norm, MAX(em.confidence) \
             FROM entities e JOIN entity_mentions em ON em.entity_id = e.entity_id \
             WHERE (e.kind, e.norm) IN (VALUES {values_sql})"
        );
        let mask_params = if let FilterMask::Sql(mask) = &mask {
            sql.push_str(&format!(" AND {}", mask.exists_clause("em.message_id")));
            mask.params.clone()
        } else {
            Vec::new()
        };
        // Ordered even though the caller re-sorts by score below: without an
        // `ORDER BY`, which `(message, entity)` pairs survive `LIMIT`
        // whenever the safety cap actually binds is an accident of the
        // query plan rather than a defined choice.
        sql.push_str(" GROUP BY em.message_id, e.kind, e.norm ORDER BY em.message_id LIMIT ?");

        let ref_params: Vec<Value> = refs
            .iter()
            .flat_map(|(kind, norm, _)| [Value::Text(kind.clone()), Value::Text(norm.clone())])
            .collect();
        let raw_cap = MAX_RAW_MENTIONS;
        let rows = interruptible_read(&self.db, cancel, move |conn| {
            let mut stmt = conn.prepare(&sql)?;
            let mut bound: Vec<&dyn rusqlite::ToSql> =
                Vec::with_capacity(ref_params.len() + mask_params.len() + 1);
            for value in &ref_params {
                bound.push(value);
            }
            for value in &mask_params {
                bound.push(value);
            }
            bound.push(&raw_cap);
            let rows = stmt
                .query_map(bound.as_slice(), |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, f64>(3)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<(i64, String, String, f64)>>>()?;
            Ok(rows)
        })
        .await?;

        let Some(rows) = rows else {
            tracing::debug!("scan cancelled; superseded by a newer query");
            return Ok(Vec::new());
        };

        let boosts: BTreeMap<(String, String), f64> = refs
            .into_iter()
            .map(|(kind, norm, boost)| ((kind, norm), boost))
            .collect();
        let mut scores: BTreeMap<i64, f64> = BTreeMap::new();
        for (message_id, kind, norm, confidence) in rows {
            if let Some(boost) = boosts.get(&(kind, norm)) {
                *scores.entry(message_id).or_insert(0.0) += boost * confidence;
            }
        }

        let mut scored: Vec<(i64, f64)> = scores.into_iter().collect();
        scored.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        let page = clamp_limit(limit);
        scored.truncate(usize::try_from(page).unwrap_or(usize::MAX));

        let candidates = rank_by_score(Source::Entity, scored);
        tracing::Span::current().record("hits", candidates.len());
        Ok(candidates)
    }
}

fn clamp_limit(limit: i64) -> i64 {
    if limit <= 0 {
        MAX_LIMIT
    } else {
        limit.min(MAX_LIMIT)
    }
}

#[cfg(test)]
mod tests;
