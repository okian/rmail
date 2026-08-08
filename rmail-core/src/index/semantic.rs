//! The dense retriever: chunks, their vectors, and kNN over both.
//!
//! # Three tables, one invariant
//!
//! `chunks` says what the text is, `chunk_embeddings` says which model saw it,
//! and `vec_chunks` holds the vector. They are split because their lifetimes
//! differ: re-chunking invalidates spans a citation points at, re-embedding
//! does not; a model switch invalidates every vector while leaving every chunk
//! intact. The invariant tying them together is that a row in `vec_chunks` is
//! meaningful only if `chunk_embeddings` agrees about the model *and* the
//! content hash. [`verify`] is what checks it, because a virtual table takes no
//! foreign key and will not check it for us.
//!
//! # Work is skipped, not repeated
//!
//! Embedding is the most expensive thing in the indexer, and the queue that
//! drives it redelivers on lease expiry, so the same message arrives again as a
//! matter of course. Every stage is therefore keyed on content: a chunk whose
//! text hash is unchanged is not re-chunked, and a vector whose `(model, hash)`
//! still matches is not recomputed. Re-indexing an unchanged mailbox costs a
//! few hash comparisons.
//!
//! # A model change is a targeted rebuild
//!
//! Vectors from two models are not comparable — the cosine between them is a
//! number with no meaning, which is worse than an error because it sorts. When
//! the configured model no longer matches what a row was embedded with, that
//! row is stale and is re-embedded; nothing else is touched, and search keeps
//! working on whatever is current in the meantime.

use std::sync::Arc;

use rusqlite::OptionalExtension;
use sha2::{Digest, Sha256};

use crate::config::IndexSemanticConfig;
use crate::embed::{Embedder, Embedding};
use crate::error::Error;
use crate::index::chunk::{self, Chunk, ChunkSpec};
use crate::storage::Database;

/// Dimensionality of `vec_chunks`, fixed by the migration.
///
/// `vec0` takes its width at creation time, so a model of another size needs a
/// new table and therefore a migration. Checked before a write rather than
/// trusted: a vector of the wrong length would be rejected by SQLite with a
/// message about blob sizes, which says nothing about the actual mistake.
pub const VECTOR_DIM: usize = 384;

/// How far past `limit` the kNN reaches to survive post-filtering.
///
/// The `MATCH … k = ?` clause is evaluated by `vec0` before the joins that
/// exclude other models and moved text, so every excluded row consumes one of
/// the k slots. During a model switch — precisely when this matters — nearly
/// every row is excluded, and a search for five results measured *one*.
/// Over-fetching and widening is what makes "search keeps working on whatever
/// is current" true rather than aspirational.
const OVERFETCH: usize = 8;

/// Largest kNN fetch, however much widening would ask for.
///
/// `k` is work `vec0` does before anything can be filtered; unbounded, a switch
/// leaving one current row would scan the whole table looking for a second.
const MAX_FETCH: usize = 4096;

/// Largest number of chunks embedded in one call.
///
/// The embedder batches internally; this bounds how many chunk *texts* are held
/// in memory at once while waiting, which for a mailbox backfill is the number
/// that matters.
const EMBED_BATCH: usize = 128;

/// What one indexing pass did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SemanticReport {
    /// The message indexed.
    pub message_id: i64,
    /// Chunks the message now has.
    pub chunks: usize,
    /// Chunks whose text was unchanged, so they were left alone.
    pub unchanged: usize,
    /// Vectors computed this pass.
    pub embedded: usize,
    /// Chunks that disappeared because the text shrank.
    pub removed: usize,
    /// Whether the pass was abandoned because the text changed under it.
    ///
    /// Not an error: a concurrent writer's pass will index the newer text, and
    /// this one simply has nothing left to say.
    pub superseded: bool,
}

/// One dense hit.
#[derive(Debug, Clone, PartialEq)]
pub struct Neighbor {
    /// The chunk that matched.
    pub chunk_id: i64,
    /// Its message.
    pub message_id: i64,
    /// Which part it came from.
    pub part: String,
    /// Byte offsets into that part's text, for quoting.
    pub span_start: i64,
    pub span_end: i64,
    /// Cosine similarity, higher is better.
    pub score: f32,
}

/// What [`verify`] found.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Drift {
    /// Chunks with no row in `chunk_embeddings`.
    pub missing: i64,
    /// Chunks whose vector is gone from `vec_chunks`.
    ///
    /// The mirror of `orphaned`, and the direction that makes a chunk
    /// permanently dark: nothing joins to it, so it never appears in a result,
    /// and the skip logic — which only ever looked at `chunk_embeddings` —
    /// happily reports it as unchanged for ever.
    pub unvectored: i64,
    /// Chunks embedded by a different model or dimensionality.
    pub wrong_model: i64,
    /// Chunks whose text changed after they were embedded.
    pub stale: i64,
    /// Vectors whose chunk no longer exists.
    pub orphaned: i64,
    /// Messages whose centroid is missing, from another model, or averaged over
    /// a different number of chunks than the message now has.
    pub message_vectors: i64,
}

impl Drift {
    /// Whether the index is consistent with the configured model.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        *self == Self::default()
    }

    /// How many chunks need embedding work.
    #[must_use]
    pub fn outstanding(&self) -> i64 {
        self.missing + self.unvectored + self.wrong_model + self.stale + self.message_vectors
    }
}

/// The dense retriever.
#[derive(Clone)]
pub struct SemanticIndex {
    db: Database,
    embedder: Arc<dyn Embedder>,
    spec: ChunkSpec,
}

impl std::fmt::Debug for SemanticIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SemanticIndex")
            .field("model", &self.embedder.model())
            .field("dim", &self.embedder.dim())
            .field("spec", &self.spec)
            .finish()
    }
}

impl SemanticIndex {
    /// Build the index over a database and an embedder.
    #[must_use]
    pub fn new(db: Database, embedder: Arc<dyn Embedder>, config: &IndexSemanticConfig) -> Self {
        Self {
            db,
            embedder,
            spec: ChunkSpec::from_config(config),
        }
    }

    /// The model whose vectors this index holds.
    #[must_use]
    pub fn model(&self) -> &str {
        self.embedder.model()
    }

    /// The width of the vectors that model produces.
    ///
    /// Reported rather than assumed to be [`VECTOR_DIM`]: the two agreeing is
    /// the precondition [`SemanticIndex::index_message`] checks and
    /// [`SemanticIndex::verify`] counts violations of, so a status view that
    /// printed the schema's width would hide exactly the misconfiguration those
    /// two exist to surface.
    #[must_use]
    pub fn dim(&self) -> usize {
        self.embedder.dim()
    }

    /// Chunk and embed one message.
    ///
    /// Reads the extracted text, splits it, and computes vectors for whatever
    /// is new or stale. A chunk whose text hash and model are both unchanged
    /// costs a comparison and nothing else.
    ///
    /// A message with no extracted text is *pruned*, not rejected. It is an
    /// ordinary thing to receive — a scanned PDF with no text layer, a message
    /// whose body was only a quoted reply — and the extract stage enqueues this
    /// one unconditionally, so an error would make every such message a poison
    /// job that retries, backs off and dead-letters. Worse, bailing before
    /// `persist` used to leave the old chunks and vectors in place, still
    /// answering queries with spans into text that is no longer there.
    ///
    /// # Errors
    ///
    /// [`Error::Internal`] if the embedder produces vectors of a width
    /// `vec_chunks` cannot hold. Otherwise a mapped storage error or whatever
    /// the embedder returned.
    #[tracing::instrument(skip(self), fields(chunks, embedded))]
    pub async fn index_message(&self, message_id: i64) -> Result<SemanticReport, Error> {
        if self.embedder.dim() != VECTOR_DIM {
            // Caught before anything is written. A vector of another width is
            // rejected by SQLite with a message about blob sizes, which says
            // nothing about the model being wrong for this schema.
            return Err(Error::internal(format!(
                "model {:?} produces {} dimensions; vec_chunks holds {VECTOR_DIM}. \
                 A different width needs a migration.",
                self.embedder.model(),
                self.embedder.dim()
            )));
        }

        let parts = self.read_parts(message_id).await?;

        // Chunking is pure CPU over text that can be megabytes.
        let spec = self.spec;
        let witness = fingerprint(&parts);
        let planned = tokio::task::spawn_blocking(move || plan(&parts, spec))
            .await
            .map_err(|e| Error::internal(format!("chunking task failed: {e}")))?;

        let existing = self.read_existing(message_id).await?;
        let model = self.embedder.model().to_owned();
        let dim = i64::try_from(self.embedder.dim()).unwrap_or(i64::MAX);

        // Which chunks actually need a vector: new text, moved text, or text
        // embedded by a model that is no longer the configured one.
        let mut todo: Vec<usize> = Vec::new();
        let mut unchanged = 0usize;
        for (at, planned) in planned.iter().enumerate() {
            let key = (planned.part.clone(), planned.chunk.ordinal as i64);
            match existing.get(&key) {
                // No `dim` comparison: the width guard at the top of this
                // function refuses any embedder that is not `VECTOR_DIM`, so
                // a stored `dim` that differs is unreachable here. `verify`
                // still checks it, because a hand-edited database or a future
                // second vector table can produce one.
                Some(row)
                    if row.vectored
                        && row.content_hash == planned.hash
                        && row.model.as_deref() == Some(model.as_str()) =>
                {
                    unchanged += 1;
                }
                _ => todo.push(at),
            }
        }

        let mut vectors: Vec<(usize, Embedding)> = Vec::with_capacity(todo.len());
        for batch in todo.chunks(EMBED_BATCH) {
            let texts: Vec<String> = batch
                .iter()
                .filter_map(|at| planned.get(*at).map(|p| p.chunk.text.clone()))
                .collect();
            let embedded = self.embedder.embed(&texts).await?;
            if embedded.len() != batch.len() {
                return Err(Error::internal(format!(
                    "embedder returned {} vectors for {} chunks",
                    embedded.len(),
                    batch.len()
                )));
            }
            vectors.extend(batch.iter().copied().zip(embedded));
        }

        let report = self
            .persist(Write {
                message_id,
                planned,
                vectors,
                unchanged,
                model,
                dim,
                witness,
            })
            .await?;
        let span = tracing::Span::current();
        span.record("chunks", report.chunks);
        span.record("embedded", report.embedded);
        tracing::debug!(
            chunks = report.chunks,
            unchanged = report.unchanged,
            embedded = report.embedded,
            removed = report.removed,
            superseded = report.superseded,
            "semantic index updated"
        );
        Ok(report)
    }

    /// Nearest chunks to a query, best first.
    ///
    /// # Errors
    ///
    /// Whatever the embedder returned, or a mapped storage error.
    #[tracing::instrument(skip(self, query), fields(hits))]
    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<Neighbor>, Error> {
        let limit = limit.min(MAX_FETCH);
        let query = query.trim();
        if query.is_empty() || limit == 0 {
            return Ok(Vec::new());
        }
        let embedded = self.embedder.embed(&[query.to_owned()]).await?;
        let Some(vector) = embedded.into_iter().next() else {
            return Ok(Vec::new());
        };
        let hits = self.search_vector(&vector, limit).await?;
        tracing::Span::current().record("hits", hits.len());
        Ok(hits)
    }

    /// Nearest chunks to a vector that is already computed.
    ///
    /// Split from [`SemanticIndex::search`] so a caller with a message's own
    /// vector — "find me more like this one" — does not pay to embed text it
    /// has already embedded.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] if the vector is the wrong width for this
    /// index. Otherwise a mapped storage error.
    #[tracing::instrument(skip(self, vector), fields(model = %self.embedder.model(), hits))]
    pub async fn search_vector(
        &self,
        vector: &Embedding,
        limit: usize,
    ) -> Result<Vec<Neighbor>, Error> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        if vector.dim() != VECTOR_DIM {
            // Named, not silently empty. A "find more like this" caller handing
            // over a vector of the wrong width has made a mistake that no
            // amount of empty results will help it find.
            return Err(Error::invalid_argument(format!(
                "a {}-dimensional query cannot search a {VECTOR_DIM}-dimensional index",
                vector.dim()
            )));
        }
        if vector.as_slice().iter().all(|v| *v == 0.0) {
            // Nothing is near a point with no direction. Searching anyway
            // returns the k nearest to the origin, which is an arbitrary set
            // presented as an answer.
            return Ok(Vec::new());
        }
        // Widened until enough rows survive the filters, or until `vec0` has no
        // more to give. The `MATCH … k = ?` clause is evaluated before the
        // joins, so every filtered row consumes one of the k slots — and during
        // a model switch, which is exactly when this matters, nearly every row
        // is filtered. A fixed `k = limit` measured one hit for a five-hit
        // query against an index that was 90% stale.
        let mut fetch = limit.saturating_mul(2).min(MAX_FETCH);
        loop {
            // `scanned` is what `vec0` returned *before* the filters. Judging
            // exhaustion by the surviving count instead would end the loop on
            // the first pass every time, since the survivors are by definition
            // never more than `k`.
            let (rows, scanned) = self.knn(vector, fetch).await?;
            if rows.len() >= limit || scanned < fetch || fetch >= MAX_FETCH {
                let mut rows = rows;
                // Sorted here rather than left to SQL. The truncation below is
                // what makes order load-bearing — it decides which results a
                // caller never sees — and that is too much to rest on a query
                // plan preserving a `ORDER BY` through a CTE and two joins.
                rows.sort_by(|a, b| b.score.total_cmp(&a.score));
                rows.truncate(limit);
                tracing::Span::current().record("hits", rows.len());
                return Ok(rows);
            }
            fetch = fetch.saturating_mul(OVERFETCH).min(MAX_FETCH);
        }
    }

    /// One kNN pass at a given `k`: the surviving rows, and how many `vec0`
    /// returned before the filters were applied.
    async fn knn(&self, vector: &Embedding, fetch: usize) -> Result<(Vec<Neighbor>, usize), Error> {
        let bytes = vector.to_bytes();
        let k = i64::try_from(fetch).unwrap_or(i64::MAX);
        let model = self.embedder.model().to_owned();
        let dim = i64::try_from(self.embedder.dim()).unwrap_or(i64::MAX);
        let rows = self
            .db
            .read(move |conn| {
                // The kNN happens inside `vec_chunks`; the joins add the
                // provenance a caller needs and exclude rows whose model or
                // whose text no longer matches. They run *after* the kNN, which
                // is what the over-fetch above compensates for.
                // The kNN runs in the CTE and the filters run outside it, on
                // a LEFT JOIN, so a row that fails a filter is still *counted*.
                // That count is what tells the caller whether widening `k`
                // could yield more — the surviving rows never can, since they
                // are by definition no more than `k`.
                let mut stmt = conn.prepare(
                    "WITH hits AS (
                         SELECT chunk_id, distance FROM vec_chunks
                         WHERE embedding MATCH ?1 AND k = ?2
                     )
                     SELECT h.chunk_id, c.message_id, c.part, c.span_start, c.span_end,
                            h.distance,
                            (c.chunk_id IS NOT NULL
                             AND e.chunk_id IS NOT NULL
                             AND e.model = ?3
                             AND e.dim = ?4
                             AND e.content_hash = c.content_hash) AS usable
                     FROM hits h
                     LEFT JOIN chunks c ON c.chunk_id = h.chunk_id
                     LEFT JOIN chunk_embeddings e ON e.chunk_id = h.chunk_id
                     ORDER BY h.distance",
                )?;
                let mut scanned = 0usize;
                let mut kept: Vec<Neighbor> = Vec::new();
                let mut rows = stmt.query(rusqlite::params![bytes, k, model, dim])?;
                while let Some(row) = rows.next()? {
                    scanned += 1;
                    if !row.get::<_, bool>(6)? {
                        continue;
                    }
                    let distance: f64 = row.get(5)?;
                    kept.push(Neighbor {
                        chunk_id: row.get(0)?,
                        message_id: row.get(1)?,
                        part: row.get(2)?,
                        span_start: row.get(3)?,
                        span_end: row.get(4)?,
                        // `vec0`'s default metric over unit vectors is L2, and
                        // for unit vectors L2² = 2 − 2·cos. Every score this
                        // module hands out is a cosine, higher-is-better,
                        // because a caller fusing this with BM25 must not have
                        // to know which of the two orientations it holds.
                        score: (1.0 - distance * distance / 2.0) as f32,
                    });
                }
                Ok((kept, scanned))
            })
            .await?;
        Ok(rows)
    }

    /// Messages most like a given one, nearest first, excluding itself.
    ///
    /// Over message centroids rather than chunks. Ranking chunk hits and
    /// deduplicating to messages afterwards is a different and worse question:
    /// a long thread wins by having more chances to match, and a message that
    /// would have ranked well loses inside the k limit before the
    /// deduplication ever sees it.
    ///
    /// # Errors
    ///
    /// [`Error::FailedPrecondition`] if the message has no centroid yet — it
    /// has not been indexed, or it indexed to nothing. Otherwise a mapped
    /// storage error.
    #[tracing::instrument(skip(self), fields(hits))]
    pub async fn similar_messages(
        &self,
        message_id: i64,
        limit: usize,
    ) -> Result<Vec<(i64, f32)>, Error> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let model = self.embedder.model().to_owned();
        let dim = i64::try_from(self.embedder.dim()).unwrap_or(i64::MAX);
        // One more than asked for: the message is always its own nearest
        // neighbour, and excluding it inside the kNN is not possible.
        let k = i64::try_from(limit.saturating_add(1).min(MAX_FETCH)).unwrap_or(i64::MAX);
        let rows = self
            .db
            .read(move |conn| {
                let Some(mine): Option<Vec<u8>> = conn
                    .query_row(
                        "SELECT v.embedding FROM vec_messages v
                         JOIN message_embeddings m ON m.message_id = v.message_id
                         WHERE v.message_id = ?1 AND m.model = ?2 AND m.dim = ?3",
                        rusqlite::params![message_id, model, dim],
                        |row| row.get(0),
                    )
                    .optional()?
                else {
                    // Distinguished from "nothing is like it", which is a
                    // different answer to a different question. A centroid from
                    // another model exists but is not comparable to anything
                    // this index would return, so reporting no neighbours would
                    // be a lie of exactly the reassuring kind.
                    return Err(rusqlite::Error::QueryReturnedNoRows);
                };
                let mut stmt = conn.prepare(
                    "WITH hits AS (
                         SELECT message_id, distance FROM vec_messages
                         WHERE embedding MATCH ?1 AND k = ?2
                     )
                     SELECT h.message_id, h.distance FROM hits h
                     JOIN message_embeddings m ON m.message_id = h.message_id
                     WHERE h.message_id <> ?3 AND m.model = ?4 AND m.dim = ?5
                     ORDER BY h.distance",
                )?;
                let rows = stmt
                    .query_map(rusqlite::params![mine, k, message_id, model, dim], |row| {
                        let distance: f64 = row.get(1)?;
                        Ok((row.get(0)?, (1.0 - distance * distance / 2.0) as f32))
                    })?
                    .collect::<rusqlite::Result<Vec<(i64, f32)>>>()?;
                Ok(rows)
            })
            .await
            .map_err(|error| match error {
                crate::storage::StorageError::Sqlite(rusqlite::Error::QueryReturnedNoRows) => {
                    Error::failed_precondition(format!(
                        "message {message_id} has no semantic vector for the current \
                         model; index it first"
                    ))
                }
                other => Error::from(other),
            })?;
        let mut rows = rows;
        rows.sort_by(|a, b| b.1.total_cmp(&a.1));
        rows.truncate(limit);
        tracing::Span::current().record("hits", rows.len());
        Ok(rows)
    }

    /// Reconcile the three tables against the configured model.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    #[tracing::instrument(skip(self))]
    pub async fn verify(&self) -> Result<Drift, Error> {
        let model = self.embedder.model().to_owned();
        let dim = i64::try_from(self.embedder.dim()).unwrap_or(i64::MAX);
        let drift = self
            .db
            .read(move |conn| {
                let one = |sql: &str, params: &[&dyn rusqlite::ToSql]| -> rusqlite::Result<i64> {
                    conn.query_row(sql, params, |row| row.get(0))
                };
                Ok(Drift {
                    missing: one(
                        "SELECT count(*) FROM chunks c
                         WHERE NOT EXISTS (SELECT 1 FROM chunk_embeddings e
                                           WHERE e.chunk_id = c.chunk_id)",
                        &[],
                    )?,
                    unvectored: one(
                        "SELECT count(*) FROM chunks c
                         WHERE NOT EXISTS (SELECT 1 FROM vec_chunks v
                                           WHERE v.chunk_id = c.chunk_id)",
                        &[],
                    )?,
                    wrong_model: one(
                        "SELECT count(*) FROM chunk_embeddings
                         WHERE model <> ?1 OR dim <> ?2",
                        &[&model, &dim],
                    )?,
                    stale: one(
                        "SELECT count(*) FROM chunk_embeddings e
                         JOIN chunks c ON c.chunk_id = e.chunk_id
                         WHERE e.content_hash <> c.content_hash",
                        &[],
                    )?,
                    // The check a foreign key would do if a virtual table could
                    // carry one. A vector whose chunk is gone is not merely
                    // wasted space: it is returned by kNN, joined to nothing,
                    // and silently drops out of results while still consuming
                    // one of the k slots.
                    orphaned: one(
                        &format!("SELECT count(*) FROM vec_chunks v WHERE {ORPHANED}"),
                        &[],
                    )?,
                    message_vectors: one(
                        "SELECT count(*) FROM (
                             SELECT c.message_id, count(*) AS n FROM chunks c
                             GROUP BY c.message_id
                         ) t
                         LEFT JOIN message_embeddings m ON m.message_id = t.message_id
                         WHERE m.message_id IS NULL
                            OR m.model <> ?1
                            OR m.dim <> ?2
                            OR m.chunks <> t.n
                            OR NOT EXISTS (SELECT 1 FROM vec_messages v
                                           WHERE v.message_id = t.message_id)",
                        &[&model, &dim],
                    )?,
                })
            })
            .await?;
        if !drift.is_clean() {
            tracing::info!(?drift, "semantic index drift");
        }
        Ok(drift)
    }

    /// Messages with chunks that need embedding, oldest first.
    ///
    /// What a model switch turns into work: re-embedding is scheduled per
    /// message so it flows through the same queue as everything else rather
    /// than becoming one enormous transaction.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    pub async fn stale_messages(&self, limit: i64) -> Result<Vec<i64>, Error> {
        let model = self.embedder.model().to_owned();
        let dim = i64::try_from(self.embedder.dim()).unwrap_or(i64::MAX);
        Ok(self
            .db
            .read(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT DISTINCT c.message_id FROM chunks c
                     LEFT JOIN chunk_embeddings e ON e.chunk_id = c.chunk_id
                     WHERE e.chunk_id IS NULL
                        OR e.model <> ?1
                        OR e.dim <> ?2
                        OR e.content_hash <> c.content_hash
                        -- The bookkeeping row is not evidence that the vector
                        -- exists: `vec_chunks` has no foreign key, so one can
                        -- go missing while `chunk_embeddings` still claims the
                        -- chunk is embedded. Without this, such a chunk is
                        -- permanently dark and nothing ever schedules a repair.
                        OR NOT EXISTS (SELECT 1 FROM vec_chunks v
                                       WHERE v.chunk_id = c.chunk_id)
                        -- The centroid is derived from the chunk vectors, so it
                        -- can be stale while every one of them is current: a
                        -- message that gained or lost a chunk, or whose vectors
                        -- were written by a previous model.
                        OR NOT EXISTS (
                            SELECT 1 FROM message_embeddings m
                            JOIN vec_messages v ON v.message_id = m.message_id
                            WHERE m.message_id = c.message_id
                              AND m.model = ?1 AND m.dim = ?2
                              AND m.chunks = (SELECT count(*) FROM chunks x
                                              WHERE x.message_id = c.message_id)
                        )
                     ORDER BY c.message_id
                     LIMIT ?3",
                )?;
                let rows = stmt
                    .query_map(rusqlite::params![model, dim, limit.max(1)], |row| {
                        row.get(0)
                    })?
                    .collect::<rusqlite::Result<Vec<i64>>>()?;
                Ok(rows)
            })
            .await?)
    }

    /// Delete vectors whose chunk is gone.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    pub async fn collect_orphans(&self) -> Result<u64, Error> {
        let removed = self.db.write(|conn| sweep_orphan_vectors(conn)).await?;
        Ok(removed as u64)
    }

    /// The extracted parts of a message, in the order they are chunked.
    async fn read_parts(&self, message_id: i64) -> Result<Vec<(String, String)>, Error> {
        Ok(self
            .db
            .read(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT part, text FROM index_content
                     WHERE message_id = ?1 AND text <> '' ORDER BY part",
                )?;
                let rows = stmt
                    .query_map([message_id], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await?)
    }

    /// What is already stored for a message, keyed by `(part, ordinal)`.
    async fn read_existing(
        &self,
        message_id: i64,
    ) -> Result<std::collections::BTreeMap<(String, i64), ExistingChunk>, Error> {
        Ok(self
            .db
            .read(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT c.part, c.ordinal, c.content_hash, e.model, e.dim,
                            EXISTS (SELECT 1 FROM vec_chunks v WHERE v.chunk_id = c.chunk_id)
                     FROM chunks c
                     LEFT JOIN chunk_embeddings e ON e.chunk_id = c.chunk_id
                     WHERE c.message_id = ?1",
                )?;
                let rows = stmt.query_map([message_id], |row| {
                    Ok((
                        (row.get(0)?, row.get(1)?),
                        ExistingChunk {
                            content_hash: row.get(2)?,
                            model: row.get(3)?,
                            vectored: row.get(5)?,
                        },
                    ))
                })?;
                rows.collect::<rusqlite::Result<_>>()
            })
            .await?)
    }

    /// Write the plan and its new vectors in one transaction.
    ///
    /// `witness` is a hash of the text the plan was built from, re-checked
    /// inside the transaction: the plan came from a read-pool snapshot taken
    /// before an arbitrarily long embed, and two workers can hold the same
    /// message because the queue's leases expire. Last-writer-wins would leave
    /// `chunks` permanently describing older text than `index_content` holds,
    /// with both hashes self-consistent so nothing downstream could detect it.
    async fn persist(&self, write: Write) -> Result<SemanticReport, Error> {
        let Write {
            message_id,
            planned,
            vectors,
            unchanged,
            model,
            dim,
            witness,
        } = write;
        let report = self
            .db
            .write(move |conn| {
                let tx = conn.transaction()?;

                let current = {
                    let mut stmt = tx.prepare(
                        "SELECT part, text FROM index_content
                         WHERE message_id = ?1 AND text <> '' ORDER BY part",
                    )?;
                    let rows = stmt
                        .query_map([message_id], |row| Ok((row.get(0)?, row.get(1)?)))?
                        .collect::<rusqlite::Result<Vec<(String, String)>>>()?;
                    rows
                };
                if fingerprint(&current) != witness {
                    // Somebody rewrote the text while this pass was embedding.
                    // Theirs is the newer write and their own pass will index
                    // it; committing this plan on top would leave chunks that
                    // describe text nobody can see.
                    return Ok(SemanticReport {
                        message_id,
                        superseded: true,
                        ..SemanticReport::default()
                    });
                }
                let mut keep: Vec<i64> = Vec::with_capacity(planned.len());
                let mut ids: Vec<i64> = Vec::with_capacity(planned.len());

                for item in &planned {
                    // Upsert on `(message, part, ordinal)`, which is the chunk's
                    // identity. Re-running over unchanged text finds the same
                    // row and rewrites the same values, so redelivery — which
                    // the lease-based queue makes routine — is free.
                    tx.prepare_cached(
                        "INSERT INTO chunks
                             (message_id, part, ordinal, span_start, span_end, tokens,
                              content_hash)
                         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                         ON CONFLICT(message_id, part, ordinal) DO UPDATE SET
                             span_start = excluded.span_start,
                             span_end = excluded.span_end,
                             tokens = excluded.tokens,
                             content_hash = excluded.content_hash",
                    )?
                    .execute(rusqlite::params![
                        message_id,
                        item.part,
                        item.chunk.ordinal as i64,
                        item.chunk.span_start as i64,
                        item.chunk.span_end as i64,
                        item.chunk.tokens as i64,
                        item.hash,
                    ])?;
                    let id: i64 = tx.query_row(
                        "SELECT chunk_id FROM chunks
                         WHERE message_id = ?1 AND part = ?2 AND ordinal = ?3",
                        rusqlite::params![message_id, item.part, item.chunk.ordinal as i64],
                        |row| row.get(0),
                    )?;
                    keep.push(id);
                    ids.push(id);
                }

                // Chunks the message no longer has, because the text shrank.
                // Left behind, they would keep matching queries with passages
                // that are not in the message any more.
                let list = keep
                    .iter()
                    .map(i64::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                let gone: Vec<i64> = {
                    let sql = if list.is_empty() {
                        "SELECT chunk_id FROM chunks WHERE message_id = ?1".to_owned()
                    } else {
                        format!(
                            "SELECT chunk_id FROM chunks
                             WHERE message_id = ?1 AND chunk_id NOT IN ({list})"
                        )
                    };
                    let mut stmt = tx.prepare(&sql)?;
                    let rows = stmt
                        .query_map([message_id], |row| row.get(0))?
                        .collect::<rusqlite::Result<Vec<i64>>>()?;
                    rows
                };
                for id in &gone {
                    // `vec_chunks` is a virtual table: no foreign key, no
                    // cascade. Deleting the vector is this code's job, and the
                    // one place it can be forgotten.
                    tx.execute("DELETE FROM vec_chunks WHERE chunk_id = ?1", [id])?;
                    tx.execute("DELETE FROM chunks WHERE chunk_id = ?1", [id])?;
                }

                let mut embedded = 0usize;
                for (at, vector) in &vectors {
                    let Some(id) = ids.get(*at).copied() else {
                        continue;
                    };
                    let Some(item) = planned.get(*at) else {
                        continue;
                    };
                    // A zero vector has no direction. `vec0` reports its L2
                    // distance from any unit query as exactly 1.0, which the
                    // cosine conversion reads as 0.5 — so it outranks every
                    // genuinely unrelated chunk and turns up near the top of
                    // every search ever run. It is not a weak match; it is the
                    // absence of one, and storing it is worse than storing
                    // nothing.
                    if vector.as_slice().iter().all(|v| *v == 0.0) {
                        tracing::debug!(chunk_id = id, "a chunk embedded to nothing; not stored");
                        continue;
                    }
                    // Delete-then-insert: `vec0` has no upsert, and an insert
                    // onto an occupied rowid is a constraint failure rather
                    // than a replacement.
                    tx.execute("DELETE FROM vec_chunks WHERE chunk_id = ?1", [id])?;
                    tx.execute(
                        "INSERT INTO vec_chunks (chunk_id, embedding) VALUES (?1, ?2)",
                        rusqlite::params![id, vector.to_bytes()],
                    )?;
                    tx.prepare_cached(
                        "INSERT INTO chunk_embeddings (chunk_id, model, dim, content_hash)
                         VALUES (?1, ?2, ?3, ?4)
                         ON CONFLICT(chunk_id) DO UPDATE SET
                             model = excluded.model,
                             dim = excluded.dim,
                             content_hash = excluded.content_hash,
                             embedded_at = unixepoch()",
                    )?
                    .execute(rusqlite::params![id, model, dim, item.hash])?;
                    embedded += 1;
                }

                // The centroid, from the vectors that are in the table *now* —
                // not from the ones this pass happened to compute. Most passes
                // recompute nothing, and a mean over "whatever changed" would
                // describe a fragment of the message rather than the message.
                write_message_vector(&tx, message_id, &model, dim)?;

                tx.commit()?;
                Ok(SemanticReport {
                    message_id,
                    chunks: keep.len(),
                    unchanged,
                    embedded,
                    removed: gone.len(),
                    superseded: false,
                })
            })
            .await?;
        Ok(report)
    }
}

/// Delete the vectors of messages about to be removed.
///
/// Called from the expunge path, in the same transaction, *before* the delete:
/// once `chunks` has cascaded away there is no longer anything linking a vector
/// to the message it belonged to, and the only remaining recourse is the
/// full-table sweep below.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub(crate) fn drop_vectors(conn: &rusqlite::Connection, ids: &[i64]) -> rusqlite::Result<usize> {
    if ids.is_empty() {
        return Ok(0);
    }
    let list = ids
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    conn.execute(
        &format!("DELETE FROM vec_messages WHERE message_id IN ({list})"),
        [],
    )?;
    conn.execute(
        &format!(
            "DELETE FROM vec_chunks WHERE chunk_id IN (
                 SELECT chunk_id FROM chunks WHERE message_id IN ({list})
             )"
        ),
        [],
    )
}

/// Delete every vector whose chunk is gone.
///
/// The set-based twin of [`drop_vectors`], for the cases where the messages are
/// already deleted and the link is lost — a `UIDVALIDITY` bump invalidating a
/// six-figure folder, or a repair.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub(crate) fn sweep_orphan_vectors(conn: &rusqlite::Connection) -> rusqlite::Result<usize> {
    let messages = conn.execute(
        "DELETE FROM vec_messages WHERE message_id IN (
             SELECT v.message_id FROM vec_messages v
             WHERE NOT EXISTS (SELECT 1 FROM messages m WHERE m.id = v.message_id)
         )",
        [],
    )?;
    Ok(messages
        + conn.execute(
            &format!(
                "DELETE FROM vec_chunks WHERE chunk_id IN (
                 SELECT v.chunk_id FROM vec_chunks v WHERE {ORPHANED}
             )"
            ),
            [],
        )?)
}

/// What makes a `vec_chunks` row an orphan.
///
/// One definition with three readers — the counter, the sweep and the reaper —
/// which previously carried byte-identical copies. Three places for them to
/// stop agreeing about what the reaper is allowed to delete.
const ORPHANED: &str = "NOT EXISTS (SELECT 1 FROM chunks c WHERE c.chunk_id = v.chunk_id)";

/// A chunk already in the database.
#[derive(Debug)]
struct ExistingChunk {
    content_hash: Vec<u8>,
    model: Option<String>,
    /// Whether `vec_chunks` actually holds the vector. Bookkeeping alone is not
    /// evidence: the virtual table has no foreign key, so a row can go missing
    /// from it while `chunk_embeddings` still claims the chunk is embedded.
    vectored: bool,
}

/// Recompute a message's centroid from the chunk vectors currently stored.
///
/// The normalized mean. Cheap — no model call — and it does not truncate, which
/// embedding the whole message as one string would: past the model's input
/// limit the vector would describe only the message's opening.
fn write_message_vector(
    conn: &rusqlite::Connection,
    message_id: i64,
    model: &str,
    dim: i64,
) -> rusqlite::Result<()> {
    let vectors: Vec<Vec<u8>> = {
        let mut stmt = conn.prepare_cached(
            "SELECT v.embedding FROM vec_chunks v
             JOIN chunks c ON c.chunk_id = v.chunk_id
             WHERE c.message_id = ?1",
        )?;
        let rows = stmt
            .query_map([message_id], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<Vec<u8>>>>()?;
        rows
    };

    conn.execute(
        "DELETE FROM vec_messages WHERE message_id = ?1",
        [message_id],
    )?;
    if vectors.is_empty() {
        // A message with no chunk vectors has no centroid. Leaving the old one
        // would make a deleted body still answer "what is like this".
        conn.execute(
            "DELETE FROM message_embeddings WHERE message_id = ?1",
            [message_id],
        )?;
        return Ok(());
    }

    let mut mean = vec![0.0f64; VECTOR_DIM];
    for bytes in &vectors {
        for (slot, chunk) in mean.iter_mut().zip(bytes.chunks_exact(4)) {
            *slot += f64::from(f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
        }
    }
    let count = vectors.len();
    for slot in &mut mean {
        *slot /= count as f64;
    }
    let centroid = Embedding::new(mean.into_iter().map(|v| v as f32).collect());
    if centroid.as_slice().iter().all(|v| *v == 0.0) {
        // Chunks that cancelled each other out exactly. Vanishingly unlikely,
        // and a zero vector in a kNN table is a universal half-match, so it is
        // not stored for the same reason a chunk's is not.
        conn.execute(
            "DELETE FROM message_embeddings WHERE message_id = ?1",
            [message_id],
        )?;
        return Ok(());
    }

    conn.execute(
        "INSERT INTO vec_messages (message_id, embedding) VALUES (?1, ?2)",
        rusqlite::params![message_id, centroid.to_bytes()],
    )?;
    conn.execute(
        "INSERT INTO message_embeddings (message_id, model, dim, chunks)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(message_id) DO UPDATE SET
             model = excluded.model,
             dim = excluded.dim,
             chunks = excluded.chunks,
             embedded_at = unixepoch()",
        rusqlite::params![message_id, model, dim, count as i64],
    )?;
    Ok(())
}

/// Everything one write needs.
///
/// A struct rather than eight parameters: the two `String`s and the two `Vec`s
/// were positional and adjacent, which is one transposed pair away from writing
/// a plan's hashes under another plan's model.
struct Write {
    message_id: i64,
    planned: Vec<Planned>,
    vectors: Vec<(usize, Embedding)>,
    unchanged: usize,
    model: String,
    dim: i64,
    witness: Vec<u8>,
}

/// A hash of the exact text a plan was built from.
///
/// Cheap enough to take on every pass and specific enough that any edit to any
/// part changes it. Part names are hashed as well as bodies, so a part
/// appearing or disappearing is a change even when the remaining text is not.
fn fingerprint(parts: &[(String, String)]) -> Vec<u8> {
    let mut hasher = Sha256::new();
    for (part, text) in parts {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
        hasher.update((text.len() as u64).to_le_bytes());
        hasher.update(text.as_bytes());
    }
    hasher.finalize().to_vec()
}

/// A chunk and where it came from.
#[derive(Debug)]
struct Planned {
    part: String,
    chunk: Chunk,
    hash: Vec<u8>,
}

/// Split every part and hash each chunk's text.
fn plan(parts: &[(String, String)], spec: ChunkSpec) -> Vec<Planned> {
    let mut planned = Vec::new();
    for (part, text) in parts {
        for chunk in chunk::split(text, spec) {
            let hash = Sha256::digest(chunk.text.as_bytes()).to_vec();
            planned.push(Planned {
                part: part.clone(),
                chunk,
                hash,
            });
        }
    }
    planned
}

#[cfg(test)]
mod tests;
