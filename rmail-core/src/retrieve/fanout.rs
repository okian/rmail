//! The Stage 1 fan-out: every retriever, run concurrently against one
//! [`QueryPlan`], each individually skippable, all honoring one cancellation
//! token (prd.md, "Stage 1 — Candidate Generation").
//!
//! # `tokio::join!` is the bounded pool
//!
//! prd.md requires candidate generation to run "concurrently on a bounded
//! pool." [`Fanout::generate`] reaches for `tokio::join!` rather than a
//! bespoke worker-pool/semaphore, because the bound this task actually needs
//! already exists two layers down: every retriever's database work goes
//! through [`crate::storage::Database`]'s pooled read connections
//! ([`crate::storage::Database::DEFAULT_READ_POOL_SIZE`] of them), and every
//! blocking closure runs on tokio's own bounded blocking-thread pool. Seven
//! retrievers contending for eight read connections is already bounded
//! concurrency; a second, hand-rolled limiter on top of it would bound
//! nothing an existing pool does not already bound, at the cost of a second
//! place that pool size can drift out of sync with the real one. `join!`
//! polls all seven futures concurrently (each yields at its first `.await` —
//! the `spawn_blocking` join — so none of them blocks another from starting),
//! which is the property "concurrent, not sequential" actually asks for.
//!
//! # Individually skippable, two ways
//!
//! A source contributes nothing, and the query still succeeds, in exactly
//! two situations this module treats identically: **configured off**
//! ([`crate::config::RetrieversConfig`] leaves the corresponding retriever
//! unconstructed, so [`Fanout::generate`] never calls it at all) and
//! **failed at runtime** (the retriever ran and returned `Err` — a storage
//! fault, a malformed input one source's own parsing choked on). Neither
//! propagates past [`degrade`]: every branch of the `join!` below is `Vec<
//! Candidate>`, never `Result`, so there is no way for one source's failure
//! to fail the whole fan-out even by accident.
//!
//! [`super::lexical::LexicalRetriever`] is included even though task 27
//! built it against [`crate::query::ParsedQuery`] rather than [`QueryPlan`]:
//! [`Fanout`] re-derives the `ParsedQuery` lexical.rs expects by re-running
//! [`crate::query::parse`] on `plan.raw` — the same pure, sub-microsecond,
//! deterministic parse [`crate::query::QueryPlanner::plan_at`] itself starts
//! from (`parse`'s output is a total function of its input text, and
//! `plan.raw` is `parsed.raw` carried through unchanged — see
//! `query::plan::QueryPlanner::plan_at` — so this is not a second, possibly-
//! divergent parse of *different* text) — rather than reshaping lexical.rs's
//! already-shipped, already-tested filter compiler to a type it was never
//! built against.
//!
//! # A known gap: spell-fix and PMI expansion do not reach lexical
//!
//! The re-parse above is bit-identical to `parse::parse(raw)`, but that is
//! exactly the limitation: it recovers `ParsedQuery`, not `QueryPlan`, so
//! `QueryPlan::lexical_terms`' spell-corrected [`crate::query::TermOrigin::SpellFixed`]
//! terms and `QueryPlan::expansions`' PMI synonyms — task 26's Stage 0 steps
//! 3 and 5 — never reach the lexical retriever's `MATCH` expression, or any
//! other retriever in this module (`prefix`/`fuzzy` filter to
//! `TermOrigin::Original` deliberately; `dense` sees corrections only
//! indirectly, baked into `query_vector`). Wiring a spell-corrected/expanded
//! term in correctly is not a drop-in addition: `retrieve::lexical::MatchExpr`
//! ANDs every required term together, so a naive "add the correction as
//! another term" would *over*-constrain a query the correction was supposed
//! to broaden (`(original AND corrected)` when the intent is `(original OR
//! corrected)`, at the original's full weight and the correction's or
//! synonym's own down-weighted one) — real work belonging to a follow-up
//! task, not silently dropped without a trace here.

use tokio_util::sync::CancellationToken;

use super::{
    Candidate, DenseRetriever, EntityRetriever, FuzzyRetriever, LexicalRetriever, PrefixRetriever,
    RecencyRetriever, Source, StructuredRetriever,
};
use crate::config::RetrieversConfig;
use crate::error::Error;
use crate::index::fts::FtsIndex;
use crate::index::semantic::SemanticIndex;
use crate::query::{self, QueryPlan};
use crate::storage::Database;

/// Every retriever this task owns, wired to one database/index set and
/// configured for which sources are allowed to run at all.
#[derive(Clone)]
pub struct Fanout {
    lexical: LexicalRetriever,
    dense: Option<DenseRetriever>,
    fuzzy: Option<FuzzyRetriever>,
    entity: Option<EntityRetriever>,
    structured: Option<StructuredRetriever>,
    prefix: Option<PrefixRetriever>,
    recency: Option<RecencyRetriever>,
}

impl std::fmt::Debug for Fanout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fanout")
            .field("dense", &self.dense.is_some())
            .field("fuzzy", &self.fuzzy.is_some())
            .field("entity", &self.entity.is_some())
            .field("structured", &self.structured.is_some())
            .field("prefix", &self.prefix.is_some())
            .field("recency", &self.recency.is_some())
            .finish_non_exhaustive()
    }
}

impl Fanout {
    /// Build a fan-out over `db`'s lexical/semantic indexes, constructing
    /// only the retrievers `config` enables.
    #[must_use]
    pub fn new(
        db: Database,
        fts: FtsIndex,
        semantic: &SemanticIndex,
        config: &RetrieversConfig,
    ) -> Self {
        Self {
            lexical: LexicalRetriever::new(fts.clone(), db.clone()),
            dense: config
                .dense
                .then(|| DenseRetriever::new(db.clone(), semantic)),
            fuzzy: config.fuzzy.then(|| FuzzyRetriever::new(db.clone())),
            entity: config.entity.then(|| EntityRetriever::new(db.clone())),
            structured: config
                .structured
                .then(|| StructuredRetriever::new(db.clone())),
            prefix: config.prefix.then(|| PrefixRetriever::new(fts, db.clone())),
            recency: config
                .recency
                .then(|| RecencyRetriever::new(db, config.recency_half_life_days)),
        }
    }

    /// Run every enabled retriever concurrently against `plan`, honoring
    /// `cancel`, and return every candidate every source produced —
    /// unsorted, undeduplicated, carrying its own source/score/rank
    /// (fusion's job, task 29, not this one's).
    #[tracing::instrument(
        skip(self, plan, cancel),
        // `limit = limit`, not a bare `limit`: a bare name in `fields(...)`
        // that also names an argument shadows the auto-recorded argument
        // with an empty placeholder instead of adding to it, so the value
        // would never actually get recorded without the `= limit` form.
        fields(limit = limit, filters = plan.hard_filters.len(), candidates)
    )]
    pub async fn generate(
        &self,
        plan: &QueryPlan,
        limit: u32,
        cancel: &CancellationToken,
    ) -> Vec<Candidate> {
        let limit = i64::from(limit);
        let (lexical, dense, fuzzy, entity, structured, prefix, recency) = tokio::join!(
            self.run_lexical(plan, limit, cancel),
            self.run_dense(plan, limit, cancel),
            self.run_fuzzy(plan, limit, cancel),
            self.run_entity(plan, limit, cancel),
            self.run_structured(plan, limit, cancel),
            self.run_prefix(plan, limit, cancel),
            self.run_recency(plan, limit, cancel),
        );
        let all: Vec<Candidate> =
            [lexical, dense, fuzzy, entity, structured, prefix, recency].concat();
        tracing::Span::current().record("candidates", all.len());
        all
    }

    async fn run_lexical(
        &self,
        plan: &QueryPlan,
        limit: i64,
        cancel: &CancellationToken,
    ) -> Vec<Candidate> {
        let parsed = query::parse(&plan.raw);
        degrade(
            Source::Lexical,
            self.lexical.retrieve(&parsed, limit, cancel).await,
        )
    }

    async fn run_dense(
        &self,
        plan: &QueryPlan,
        limit: i64,
        cancel: &CancellationToken,
    ) -> Vec<Candidate> {
        let Some(retriever) = &self.dense else {
            skipped(Source::Dense);
            return Vec::new();
        };
        degrade(Source::Dense, retriever.retrieve(plan, limit, cancel).await)
    }

    async fn run_fuzzy(
        &self,
        plan: &QueryPlan,
        limit: i64,
        cancel: &CancellationToken,
    ) -> Vec<Candidate> {
        let Some(retriever) = &self.fuzzy else {
            skipped(Source::Fuzzy);
            return Vec::new();
        };
        degrade(Source::Fuzzy, retriever.retrieve(plan, limit, cancel).await)
    }

    async fn run_entity(
        &self,
        plan: &QueryPlan,
        limit: i64,
        cancel: &CancellationToken,
    ) -> Vec<Candidate> {
        let Some(retriever) = &self.entity else {
            skipped(Source::Entity);
            return Vec::new();
        };
        degrade(
            Source::Entity,
            retriever.retrieve(plan, limit, cancel).await,
        )
    }

    async fn run_structured(
        &self,
        plan: &QueryPlan,
        limit: i64,
        cancel: &CancellationToken,
    ) -> Vec<Candidate> {
        let Some(retriever) = &self.structured else {
            skipped(Source::Structured);
            return Vec::new();
        };
        degrade(
            Source::Structured,
            retriever.retrieve(plan, limit, cancel).await,
        )
    }

    async fn run_prefix(
        &self,
        plan: &QueryPlan,
        limit: i64,
        cancel: &CancellationToken,
    ) -> Vec<Candidate> {
        let Some(retriever) = &self.prefix else {
            skipped(Source::Prefix);
            return Vec::new();
        };
        degrade(
            Source::Prefix,
            retriever.retrieve(plan, limit, cancel).await,
        )
    }

    async fn run_recency(
        &self,
        plan: &QueryPlan,
        limit: i64,
        cancel: &CancellationToken,
    ) -> Vec<Candidate> {
        let Some(retriever) = &self.recency else {
            skipped(Source::Recency);
            return Vec::new();
        };
        degrade(
            Source::Recency,
            retriever.retrieve(plan, limit, cancel).await,
        )
    }
}

/// Turn a retriever's `Result` into the infallible `Vec<Candidate>`
/// [`Fanout::generate`]'s `join!` branches all share — an `Err` degrades to
/// no candidates from that source, logged, rather than failing the query
/// (prd.md's "Graceful degradation" principle, applied at the retriever
/// boundary).
fn degrade(source: Source, result: Result<Vec<Candidate>, Error>) -> Vec<Candidate> {
    match result {
        Ok(candidates) => candidates,
        Err(error) => {
            tracing::warn!(?source, %error, "retriever failed; degrading to no candidates");
            Vec::new()
        }
    }
}

fn skipped(source: Source) {
    tracing::debug!(?source, "retriever disabled by config; skipped");
}

#[cfg(test)]
mod tests;
