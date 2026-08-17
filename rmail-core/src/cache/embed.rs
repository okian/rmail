//! The query half of prd.md's embedding cache: persisted vectors for text
//! that is not a document.
//!
//! # What already existed, and what this adds
//!
//! prd.md asks for "query and document embeddings persisted; documents
//! re-embedded only on `content_hash` change." The document half is
//! [`crate::index::semantic`]: a chunk whose text hash still matches what
//! `chunk_embeddings` recorded is never re-embedded, and a model change
//! re-embeds exactly the rows that model no longer covers. This module is the
//! query half — the vector for a search box's contents, which was recomputed
//! from scratch on every re-search, and on a hosted backend was a paid network
//! round trip each time.
//!
//! # It is a decorator, so nothing had to learn about it
//!
//! [`CachingEmbedder`] implements [`Embedder`] over another [`Embedder`].
//! [`crate::query::plan::QueryPlanner`] and
//! [`crate::attach::search::AttachmentSearch`] embed queries through an
//! `Arc<dyn Embedder>` they are handed; wrapping that handle is the whole
//! integration. The alternative — a `cache.get(...)` call added at each
//! embedding site — is a line every future embedding site has to remember to
//! write, and the one that forgets is silently slower and more expensive with
//! nothing to show it.
//!
//! # Why the indexer keeps the *unwrapped* embedder
//!
//! Document vectors have their own cache, keyed on the same content hash and
//! already joined to the chunk they belong to. Routing the indexer through
//! this one as well would file a second copy of every chunk vector in a table
//! bounded by an LRU sized for queries, evicting the query vectors this table
//! exists for with chunk text that was already cached elsewhere. So the wiring
//! wraps only the query paths — see `rmaild`'s `SearchApi::new`.
//!
//! # Invalidation: none, by construction
//!
//! The key is `(model, dim, sha256(truncated text))`. There is no row this
//! cache can serve for input it was not computed from: a different model, a
//! different width, or one different character is a different key. A model
//! swap therefore needs no purge — the old rows simply stop being addressable
//! and age out of the LRU.

use std::collections::HashMap;
use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension};
use sha2::{Digest, Sha256};

use crate::embed::{truncate, Embedder, Embedding};
use crate::error::Error;
use crate::storage::Database;

/// SHA-256 of the bytes the backend would actually see.
type TextHash = [u8; 32];

/// Hash `text` the way this cache keys it: after [`truncate`], so the digest
/// covers exactly the bytes a backend would be given.
fn hash_text(text: &str) -> TextHash {
    let digest = Sha256::digest(truncate(text).as_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// Typed access to `embedding_cache` (migration V51).
///
/// Cheap to clone: `db` shares a connection pool, like every other
/// storage-backed type in this crate.
#[derive(Debug, Clone)]
pub struct EmbeddingCache {
    db: Database,
    /// Hard ceiling on stored rows; the coldest are evicted past it. `0`
    /// disables the cache outright — every lookup misses and every store is
    /// dropped — so `max_embeddings = 0` is a real off switch rather than a
    /// map that keeps one entry.
    capacity: u32,
}

impl EmbeddingCache {
    /// Build the cache over `db`, holding at most `capacity` vectors.
    #[must_use]
    pub fn new(db: Database, capacity: u32) -> Self {
        Self { db, capacity }
    }

    /// Look up every hash in `hashes`.
    ///
    /// # A pure read, on purpose
    ///
    /// This runs on the interactive search path —
    /// [`crate::query::plan::QueryPlanner`] embeds on every plan, which is
    /// every keystroke-driven `Search` — and [`Database`]'s whole shape exists
    /// so that "search and other reads never block on writes." An earlier
    /// version of this method stamped `uses`/`last_used_at` here with an
    /// `UPDATE ... RETURNING`, which took the process-wide writer mutex on
    /// that path: while a sync held it for a bulk insert, a cache *hit* would
    /// have been slower than the uncached embed it replaced. A cache that can
    /// make search slower than not having it is a liability. So the read pool
    /// serves this, and the bookkeeping moved to [`Self::put_many`] — see
    /// there for why that loses nothing that matters.
    ///
    /// A row whose blob does not decode to a `dim`-wide vector is treated as a
    /// miss, so the caller re-embeds it and [`Self::put_many`]'s upsert
    /// *overwrites* the bad row on the way back. A corrupt row that were only
    /// skipped would be re-read and re-skipped on every query for ever, and
    /// the only symptom would be a cache that never hits.
    ///
    /// # Errors
    ///
    /// [`Error`] if the database read fails.
    #[tracing::instrument(level = "debug", skip_all, fields(model, hits = tracing::field::Empty))]
    pub async fn get_many(
        &self,
        model: &str,
        dim: usize,
        hashes: Vec<TextHash>,
    ) -> Result<HashMap<TextHash, Embedding>, Error> {
        if self.capacity == 0 || hashes.is_empty() {
            return Ok(HashMap::new());
        }
        let model = model.to_owned();
        let dim_i64 = i64::try_from(dim).unwrap_or(i64::MAX);
        self.db
            .read(move |conn| {
                let mut found = HashMap::with_capacity(hashes.len());
                // One statement, prepared once and stepped per hash: the batch
                // is at most `embed::MAX_BATCH`, and a single `IN (?, ?, ...)`
                // would need a statement per distinct batch width, defeating
                // the prepared-statement cache this connection keeps.
                let mut stmt = conn.prepare_cached(
                    "SELECT vector FROM embedding_cache
                      WHERE model = ?1 AND dim = ?2 AND text_hash = ?3",
                )?;
                for hash in hashes {
                    let bytes: Option<Vec<u8>> = stmt
                        .query_row(rusqlite::params![model, dim_i64, hash.as_slice()], |row| {
                            row.get(0)
                        })
                        .optional()?;
                    let Some(bytes) = bytes else { continue };
                    match Embedding::from_bytes(&bytes, dim) {
                        Ok(embedding) => {
                            found.insert(hash, embedding);
                        }
                        Err(error) => tracing::warn!(
                            %error,
                            model = %model,
                            "corrupt embedding-cache row; re-embedding and overwriting it"
                        ),
                    }
                }
                Ok(found)
            })
            .await
            .inspect(|found| {
                tracing::Span::current().record("hits", found.len());
            })
            .map_err(Error::from)
    }

    /// Store `rows`, stamp `touched` as freshly used, then evict back to
    /// `capacity`.
    ///
    /// # Why the LRU stamp lives here rather than in the lookup
    ///
    /// Refreshing a hit's `last_used_at` matters for exactly one reason:
    /// keeping a hot entry from being evicted. Eviction only ever happens
    /// *here*, when something new is being inserted — so a hit that shares a
    /// call with a miss is stamped in this same transaction, and a run of pure
    /// hits, which creates no eviction pressure at all, correctly costs
    /// nothing. That is what lets [`Self::get_many`] stay off the writer lock
    /// without weakening the policy where it bites.
    ///
    /// `uses` is therefore a *lower bound* on reads served, not a total: it
    /// counts the hits that happened to coincide with a miss. It is still the
    /// number that answers "is this table earning its keep," which is all it
    /// was ever for.
    ///
    /// # Errors
    ///
    /// [`Error`] if the write fails.
    #[tracing::instrument(level = "debug", skip_all, fields(model, stored = rows.len()))]
    pub async fn put_many(
        &self,
        model: &str,
        dim: usize,
        rows: Vec<(TextHash, Embedding)>,
        touched: Vec<TextHash>,
    ) -> Result<(), Error> {
        if self.capacity == 0 || rows.is_empty() {
            return Ok(());
        }
        let model = model.to_owned();
        let dim_i64 = i64::try_from(dim).unwrap_or(i64::MAX);
        let capacity = i64::from(self.capacity);
        self.db
            .write(move |conn| {
                let tx = conn.transaction()?;
                {
                    let mut stmt = tx.prepare_cached(
                        "INSERT INTO embedding_cache (model, dim, text_hash, vector)
                         VALUES (?1, ?2, ?3, ?4)
                         ON CONFLICT(model, dim, text_hash) DO UPDATE SET
                             vector = excluded.vector,
                             last_used_at = unixepoch()",
                    )?;
                    for (hash, embedding) in &rows {
                        stmt.execute(rusqlite::params![
                            model,
                            dim_i64,
                            hash.as_slice(),
                            embedding.to_bytes()
                        ])?;
                    }
                }
                {
                    let mut stmt = tx.prepare_cached(
                        "UPDATE embedding_cache
                            SET uses = uses + 1, last_used_at = unixepoch()
                          WHERE model = ?1 AND dim = ?2 AND text_hash = ?3",
                    )?;
                    for hash in &touched {
                        stmt.execute(rusqlite::params![model, dim_i64, hash.as_slice()])?;
                    }
                }
                evict(&tx, capacity)?;
                tx.commit()
            })
            .await
            .map_err(Error::from)
    }
}

/// Drop the coldest rows until at most `capacity` remain.
///
/// Ties on `last_used_at` are broken by `rowid`… which a `WITHOUT ROWID`
/// table does not have, so the ordering falls back to the primary key. Either
/// way the bound holds, which is the only property that matters: the choice
/// between two equally-cold entries is not one worth a column.
fn evict(conn: &Connection, capacity: i64) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM embedding_cache
          WHERE (model, dim, text_hash) IN (
              SELECT model, dim, text_hash FROM embedding_cache
               ORDER BY last_used_at DESC
               LIMIT -1 OFFSET ?1
          )",
        rusqlite::params![capacity],
    )?;
    Ok(())
}

/// An [`Embedder`] that answers from [`EmbeddingCache`] first.
///
/// Every method except [`Embedder::embed`] delegates unchanged, so a wrapped
/// embedder is indistinguishable from the one it wraps everywhere the model
/// id or width is what matters — which includes the `chunk_embeddings` join
/// the dense retriever runs, where a decorator that reported its own model id
/// would silently exclude every document vector from every search.
#[derive(Debug)]
pub struct CachingEmbedder {
    inner: Arc<dyn Embedder>,
    cache: EmbeddingCache,
}

impl CachingEmbedder {
    /// Wrap `inner`, caching its vectors in `db` up to `capacity` rows.
    #[must_use]
    pub fn new(db: Database, inner: Arc<dyn Embedder>, capacity: u32) -> Self {
        Self {
            cache: EmbeddingCache::new(db, capacity),
            inner,
        }
    }
}

#[async_trait::async_trait]
impl Embedder for CachingEmbedder {
    fn model(&self) -> &str {
        self.inner.model()
    }

    fn dim(&self) -> usize {
        self.inner.dim()
    }

    #[tracing::instrument(
        level = "debug",
        skip_all,
        fields(inputs = texts.len(), embedded = tracing::field::Empty)
    )]
    async fn embed(&self, texts: &[String]) -> Result<Vec<Embedding>, Error> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let model = self.inner.model().to_owned();
        let dim = self.inner.dim();
        let hashes: Vec<TextHash> = texts.iter().map(|t| hash_text(t)).collect();

        // A read failure degrades to a plain embed rather than failing the
        // search: a cache that can take search down with it is a liability,
        // not an optimization.
        let mut cached = match self.cache.get_many(&model, dim, hashes.clone()).await {
            Ok(cached) => cached,
            Err(error) => {
                tracing::warn!(%error, "embedding cache read failed; embedding directly");
                HashMap::new()
            }
        };

        // Distinct misses only. A batch that repeats the same text — a
        // re-search of one query across several arms — must not pay for it
        // twice in the same call either.
        let mut missing_texts: Vec<String> = Vec::new();
        let mut missing_hashes: Vec<TextHash> = Vec::new();
        for (text, hash) in texts.iter().zip(&hashes) {
            if cached.contains_key(hash) || missing_hashes.contains(hash) {
                continue;
            }
            missing_hashes.push(*hash);
            missing_texts.push(text.clone());
        }

        tracing::Span::current().record("embedded", missing_texts.len());
        if !missing_texts.is_empty() {
            let fresh = self.inner.embed(&missing_texts).await?;
            if fresh.len() != missing_texts.len() {
                return Err(Error::internal(format!(
                    "embedder returned {} vectors for {} inputs",
                    fresh.len(),
                    missing_texts.len()
                )));
            }
            // The hits go back in the same transaction as the misses, which is
            // the only moment eviction can threaten them — see
            // `EmbeddingCache::put_many`.
            let touched: Vec<TextHash> = cached.keys().copied().collect();
            let rows: Vec<(TextHash, Embedding)> =
                missing_hashes.iter().copied().zip(fresh).collect();
            for (hash, embedding) in &rows {
                cached.insert(*hash, embedding.clone());
            }
            if let Err(error) = self.cache.put_many(&model, dim, rows, touched).await {
                tracing::warn!(%error, "embedding cache write failed; result still served");
            }
        }

        // Rebuilt in input order, which is the contract `Embedder::embed`
        // states and every caller zips against.
        hashes
            .iter()
            .map(|hash| {
                cached.get(hash).cloned().ok_or_else(|| {
                    Error::internal("embedding cache lost a vector it had just computed")
                })
            })
            .collect()
    }

    /// Warm the *inner* backend, bypassing the cache entirely.
    ///
    /// The default [`Embedder::warm`] embeds one fixed string. Through this
    /// decorator that string is a cache hit from the second daemon start
    /// onward, so warming would return instantly having loaded nothing, and
    /// the several-hundred-megabyte model load it exists to front-run would
    /// land on the first user query instead — the exact cost warming was
    /// added to avoid, reintroduced by an optimization.
    async fn warm(&self) -> Result<(), Error> {
        self.inner.warm().await
    }
}
