//! prd.md's result cache: `(query, filter, corpus_version)` → ranked ids,
//! "invalidated when the corpus version bumps (new mail) or the active ranker
//! changes. Newly-synced mail can **bypass** the cache so fresh mail is never
//! hidden by a stale cached result."
//!
//! # Both invalidations are structural, not swept
//!
//! Nothing here deletes a row to invalidate it. The corpus version and a
//! digest of the entire effective ranking configuration are *inside the key*,
//! so new mail and a retuned ranker each move every subsequent lookup to an
//! address the old answers do not occupy. A `DELETE`-based invalidation would
//! be a line of code somebody has to write on every future path that changes
//! either input — and the one that gets forgotten does not fail a test, it
//! quietly returns yesterday's search results.
//!
//! Deletion is therefore only ever garbage collection: a TTL and an LRU bound,
//! neither of which is load-bearing for correctness.
//!
//! # Three ways a lookup can end, not two
//!
//! [`ResultCache::lookup`] returns [`Lookup::Hit`], [`Lookup::Miss`] or
//! [`Lookup::Bypass`], and the last is not a miss with a nicer name. A miss
//! hands back a [`Lease`] the caller returns to [`ResultCache::store`]; a
//! bypass hands back nothing, because a bypass means *this answer must not be
//! written down* — the corpus moved moments ago, or the version could not be
//! read at all. Collapsing the two would make "we could not establish what
//! the corpus looks like" indistinguishable from "we know exactly what it
//! looks like and it is not cached," and the store that followed would stamp
//! an answer with a version nobody verified.
//!
//! # The lease is a compare-and-set, not a token
//!
//! Between a miss and the store that follows it, a full search runs —
//! retrieval, fusion, ranking, rerank, presentation — and mail can land in the
//! middle of it. [`ResultCache::store`] re-reads the corpus version and
//! declines to write if it moved, so a page computed across a corpus change is
//! never filed under either version. The alternative (stamping it with the
//! version read at the end) would cache a result that predates mail the
//! version claims it accounts for, which is precisely the stale answer this
//! whole module is arranged to make impossible.

use rusqlite::OptionalExtension;
use sha2::{Digest, Sha256};

use crate::config::{CacheConfig, IndexSemanticConfig, SearchConfig};
use crate::storage::Database;

use super::corpus;

/// Bumped whenever the *shape* of a key changes — a field added to
/// [`ResultKeyParts`], a change to how ids are encoded. Without it, an
/// upgraded daemon would read rows written by the old shape at addresses the
/// new shape also computes.
const KEY_VERSION: u32 = 1;

/// Bumped whenever the fingerprint's inputs change. Separate from
/// [`KEY_VERSION`] so the two can move independently.
const FINGERPRINT_VERSION: u32 = 2;

/// Largest page this cache will store, in ids.
///
/// A bound on one row's size, not a policy about result sets: the caller's
/// `limit` is already server-capped well below this. A page longer than it is
/// served normally and simply not written down.
const MAX_CACHED_IDS: usize = 4_096;

/// A digest of everything about this daemon that decides *how* results are
/// ordered.
///
/// # Why it hashes the config's `Debug` rendering
///
/// The honest requirement is "changes when the active ranker changes," and the
/// active ranker is a function of the whole `[search]` table: fusion strategy
/// and `rrf_k`, BM25 field weights, per-intent fusion weights, the L1 weight
/// overrides, MMR lambda, which retrievers are on, the rerank backend and its
/// model. Hand-listing those fields here would work exactly until someone adds
/// the fourteenth — at which point a knob that changes result order would stop
/// invalidating the cache, and the symptom would be search results that ignore
/// a config change until the daemon is restarted with a different mailbox.
///
/// [`SearchConfig`] derives [`Debug`], and a derived `Debug` names every
/// field. Hashing it makes "this fingerprint covers every setting" true by
/// construction rather than by review. The cost is over-invalidation — the
/// rendering includes `learning` and `[search.feedback]`, which change no
/// ordering — and over-invalidation costs a recomputed search, which is the
/// side of that trade this cache is allowed to be wrong on.
///
/// The rendering never reaches a log or the wire; only the digest does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RankerFingerprint([u8; 32]);

impl RankerFingerprint {
    /// Hash the effective `[search]` and `[index.semantic]` configs plus the
    /// live embedding model and its width.
    ///
    /// `[index.semantic]` is in here for the same reason `[search]` is, and it
    /// is easy to miss: the dense arm's contribution to an ordering is decided
    /// as much by *how the corpus was chunked* as by how the results are
    /// weighted. `chunk_tokens`, `chunk_overlap`, `embed_threads` and
    /// `index_attachments` all change which vectors exist and therefore which
    /// messages the kNN returns — so retuning one and running
    /// `mail index rebuild --kind semantic` changes result order. Without this
    /// input the fingerprint would stand still across exactly that operation.
    ///
    /// `embedding_model`/`embedding_dim` come from the live [`Embedder`] rather
    /// than from the config because a backend can degrade at runtime (a build
    /// without the `onnx` feature falls back to hashed vectors), and it is the
    /// model that actually produced the vectors that decides whether an
    /// ordering is still meaningful.
    ///
    /// [`Embedder`]: crate::embed::Embedder
    #[must_use]
    pub fn new(
        search: &SearchConfig,
        semantic: &IndexSemanticConfig,
        embedding_model: &str,
        embedding_dim: usize,
    ) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(FINGERPRINT_VERSION.to_le_bytes());
        for rendered in [format!("{search:?}"), format!("{semantic:?}")] {
            hasher.update((rendered.len() as u64).to_le_bytes());
            hasher.update(rendered.as_bytes());
        }
        hasher.update((embedding_model.len() as u64).to_le_bytes());
        hasher.update(embedding_model.as_bytes());
        hasher.update((embedding_dim as u64).to_le_bytes());
        let mut out = [0u8; 32];
        out.copy_from_slice(&hasher.finalize());
        Self(out)
    }

    /// The raw digest, for storage and comparison.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// A short hex prefix for `tracing` fields. Never used for lookup.
    #[must_use]
    pub fn short(&self) -> String {
        hex_prefix(&self.0)
    }
}

/// Everything about a request that can change which ids come back.
///
/// A struct of typed fields rather than a caller-assembled string: a string
/// would let a query containing the delimiter impersonate another request's
/// key, and every field below is attacker-influenced text from a search box.
#[derive(Debug, Clone, Copy)]
pub struct ResultKeyParts<'a> {
    /// The raw query as the client sent it.
    pub query: &'a str,
    /// The structured filter expression accompanying it.
    pub filter: &'a str,
    /// Account scope; `0` means every configured account.
    pub account_id: i64,
    /// Execution mode (hybrid/lexical/semantic), as its wire string.
    pub mode: &'a str,
    /// How many results were asked for. Part of the key because the page is
    /// what is stored: a cached top-10 cannot answer a request for a top-50.
    pub limit: u32,
    /// Rerank policy, as its wire string.
    pub rerank: &'a str,
    /// Which search kind the rerank budget was charged against.
    pub kind: &'a str,
}

/// The content address of one cached page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResultKey([u8; 32]);

impl ResultKey {
    /// Hash the request, the corpus version, and the ranker fingerprint.
    ///
    /// Every variable-length field is length-prefixed. Without that, query
    /// `"ab"` with filter `"c"` and query `"a"` with filter `"bc"` would hash
    /// identically, and one search would serve another's results.
    #[must_use]
    pub fn new(
        parts: &ResultKeyParts<'_>,
        corpus_version: i64,
        fingerprint: &RankerFingerprint,
    ) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(KEY_VERSION.to_le_bytes());
        hasher.update(corpus_version.to_le_bytes());
        hasher.update(fingerprint.as_bytes());
        hasher.update(parts.account_id.to_le_bytes());
        hasher.update(parts.limit.to_le_bytes());
        for field in [
            parts.query,
            parts.filter,
            parts.mode,
            parts.rerank,
            parts.kind,
        ] {
            hasher.update((field.len() as u64).to_le_bytes());
            hasher.update(field.as_bytes());
        }
        let mut out = [0u8; 32];
        out.copy_from_slice(&hasher.finalize());
        Self(out)
    }

    /// A short hex prefix for `tracing` fields. Never used for lookup — a
    /// truncated digest is for reading logs, not for identity.
    #[must_use]
    pub fn short(&self) -> String {
        hex_prefix(&self.0)
    }
}

fn hex_prefix(bytes: &[u8; 32]) -> String {
    bytes.iter().take(6).map(|b| format!("{b:02x}")).collect()
}

/// Permission to write one page back, carrying the exact corpus version the
/// pipeline started from.
///
/// `#[must_use]`: a lease that is created and dropped is a search that was
/// computed and thrown away, which is the cache silently doing nothing.
#[derive(Debug, Clone, Copy)]
#[must_use = "a lease that is never stored means the search was recomputed for nothing"]
pub struct Lease {
    key: ResultKey,
    corpus_version: i64,
}

/// Why a lookup declined to use the cache at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BypassReason {
    /// `search.cache.enabled = false`.
    Disabled,
    /// The corpus changed within `search.cache.fresh_window_secs` — prd.md's
    /// "newly-synced mail bypasses the result cache."
    FreshCorpus,
    /// The corpus version could not be read, so no answer can be safely
    /// stamped or trusted.
    Unknown,
}

impl BypassReason {
    /// A stable string for `tracing` fields and tests.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            BypassReason::Disabled => "disabled",
            BypassReason::FreshCorpus => "fresh_corpus",
            BypassReason::Unknown => "unknown_corpus_version",
        }
    }
}

/// How a [`ResultCache::lookup`] ended. See the module docs for why a bypass
/// is not a miss.
#[derive(Debug, Clone, PartialEq)]
#[must_use]
pub enum Lookup {
    /// Serve these ids, best first. The pipeline does not run.
    Hit(Vec<i64>),
    /// Run the pipeline, then hand this lease to [`ResultCache::store`].
    Miss(Lease),
    /// Run the pipeline and store nothing.
    Bypass(BypassReason),
}

impl PartialEq for Lease {
    fn eq(&self, other: &Self) -> bool {
        self.key == other.key && self.corpus_version == other.corpus_version
    }
}

/// The `(query, filter, corpus_version)` → ranked-ids cache.
///
/// Cheap to clone: `db` shares a connection pool and everything else is
/// `Copy`.
#[derive(Debug, Clone)]
pub struct ResultCache {
    db: Database,
    config: CacheConfig,
    fingerprint: RankerFingerprint,
}

impl ResultCache {
    /// Build the cache over `db` for a daemon whose ranking is described by
    /// `fingerprint`.
    #[must_use]
    pub fn new(db: Database, config: CacheConfig, fingerprint: RankerFingerprint) -> Self {
        Self {
            db,
            config,
            fingerprint,
        }
    }

    /// The fingerprint every key this cache writes is stamped with.
    #[must_use]
    pub fn fingerprint(&self) -> &RankerFingerprint {
        &self.fingerprint
    }

    /// Look `parts` up against the current corpus version.
    #[tracing::instrument(
        level = "debug",
        skip_all,
        fields(
            cache_key = tracing::field::Empty,
            corpus_version = tracing::field::Empty,
        )
    )]
    pub async fn lookup(&self, parts: &ResultKeyParts<'_>) -> Lookup {
        if !self.config.enabled || self.config.max_results == 0 {
            return Lookup::Bypass(BypassReason::Disabled);
        }
        let stamp = match self.db.read(corpus::read).await {
            Ok(stamp) => stamp,
            Err(error) => {
                tracing::warn!(%error, "corpus version unreadable; not using the result cache");
                return Lookup::Bypass(BypassReason::Unknown);
            }
        };
        let now = chrono::Utc::now().timestamp();
        if stamp.is_fresh(now, self.config.fresh_window_secs) {
            tracing::debug!(
                corpus_version = stamp.version,
                "corpus is freshly synced; bypassing the result cache"
            );
            return Lookup::Bypass(BypassReason::FreshCorpus);
        }

        let key = ResultKey::new(parts, stamp.version, &self.fingerprint);
        tracing::Span::current().record("cache_key", key.short());
        tracing::Span::current().record("corpus_version", stamp.version);

        let ttl = i64::from(self.config.result_ttl_secs);
        let fingerprint = *self.fingerprint.as_bytes();
        // A pure read, on the read pool. `Database` is built so that reads
        // never block on writes, and an `UPDATE ... RETURNING` here would have
        // taken the process-wide writer mutex on a lookup — making a cache hit
        // slower than the pipeline it replaces whenever a sync happened to
        // hold that mutex. The `uses`/`last_used_at` stamp follows only on a
        // hit (below), where it is paid against a search that is not going to
        // run at all.
        let read = self
            .db
            .read(move |conn| {
                conn.query_row(
                    // The `corpus_version`/`ranker_fingerprint` predicates are
                    // redundant with the key that already hashed both — and
                    // are checked anyway. "Redundant given no bug" is exactly
                    // what a stale search result would be hiding behind, and
                    // the cost of being sure is two integer comparisons.
                    "SELECT message_ids FROM search_result_cache
                      WHERE cache_key = ?1
                        AND corpus_version = ?2
                        AND ranker_fingerprint = ?3
                        AND unixepoch() - created_at < ?4",
                    rusqlite::params![key.0.as_slice(), stamp.version, fingerprint.as_slice(), ttl],
                    |row| row.get::<_, Vec<u8>>(0),
                )
                .optional()
            })
            .await;

        match read {
            Ok(Some(bytes)) => match decode_ids(&bytes) {
                Some(ids) => {
                    tracing::debug!(hits = ids.len(), "result cache hit");
                    self.stamp_hit(key).await;
                    Lookup::Hit(ids)
                }
                None => {
                    // A blob that is not a whole number of i64s is corruption.
                    // Treated as a miss so the search still answers, and the
                    // lease lets the next store overwrite it.
                    tracing::warn!("corrupt result-cache row; recomputing");
                    Lookup::Miss(Lease {
                        key,
                        corpus_version: stamp.version,
                    })
                }
            },
            Ok(None) => Lookup::Miss(Lease {
                key,
                corpus_version: stamp.version,
            }),
            Err(error) => {
                tracing::warn!(%error, "result cache read failed; recomputing");
                Lookup::Bypass(BypassReason::Unknown)
            }
        }
    }

    /// Record a hit against `key`: `uses` is the only evidence this table
    /// earns its keep, and `last_used_at` is what keeps a hot page from being
    /// evicted ahead of a cold one.
    ///
    /// Deliberately *after* the answer has been decided rather than folded
    /// into the lookup: the read stays on the read pool (see
    /// [`Self::lookup`]), and this write is charged against a whole search
    /// pipeline that is not going to run. Failures are logged and dropped —
    /// losing a use count must never lose a search.
    async fn stamp_hit(&self, key: ResultKey) {
        let write = self
            .db
            .write(move |conn| {
                conn.execute(
                    "UPDATE search_result_cache
                        SET uses = uses + 1, last_used_at = unixepoch()
                      WHERE cache_key = ?1",
                    rusqlite::params![key.0.as_slice()],
                )
            })
            .await;
        if let Err(error) = write {
            tracing::debug!(%error, "could not record a result-cache hit");
        }
    }

    /// Write `ids` back under `lease`, unless the corpus moved while the
    /// pipeline ran.
    ///
    /// Never returns an error: a cache that can fail a search it was only
    /// supposed to speed up is worse than no cache. Failures are logged.
    #[tracing::instrument(level = "debug", skip_all, fields(cache_key = %lease.key.short()))]
    pub async fn store(&self, lease: Lease, ids: &[i64]) {
        if !self.config.enabled || self.config.max_results == 0 {
            return;
        }
        if ids.len() > MAX_CACHED_IDS {
            tracing::debug!(len = ids.len(), "page too large to cache");
            return;
        }
        let blob = encode_ids(ids);
        let fingerprint = *self.fingerprint.as_bytes();
        let capacity = i64::from(self.config.max_results);
        let expected = lease.corpus_version;
        let key = lease.key;

        let outcome = self
            .db
            .write(move |conn| {
                let tx = conn.transaction()?;
                let current = corpus::read(&tx)?;
                if current.version != expected {
                    // Not an error and not a retry: the answer in hand
                    // describes a corpus that no longer exists. Filing it
                    // under either version is how a cache starts lying.
                    return Ok(false);
                }
                tx.execute(
                    "INSERT INTO search_result_cache
                         (cache_key, corpus_version, ranker_fingerprint, message_ids)
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(cache_key) DO UPDATE SET
                         corpus_version = excluded.corpus_version,
                         ranker_fingerprint = excluded.ranker_fingerprint,
                         message_ids = excluded.message_ids,
                         created_at = unixepoch(),
                         last_used_at = unixepoch(),
                         uses = 0",
                    rusqlite::params![
                        key.0.as_slice(),
                        expected,
                        fingerprint.as_slice(),
                        blob.as_slice()
                    ],
                )?;
                tx.execute(
                    "DELETE FROM search_result_cache
                      WHERE cache_key IN (
                          SELECT cache_key FROM search_result_cache
                           ORDER BY last_used_at DESC
                           LIMIT -1 OFFSET ?1
                      )",
                    rusqlite::params![capacity],
                )?;
                tx.commit()?;
                Ok(true)
            })
            .await;

        match outcome {
            Ok(true) => tracing::debug!(stored = ids.len(), "result cached"),
            Ok(false) => tracing::debug!("corpus moved while searching; result not cached"),
            Err(error) => tracing::warn!(%error, "result cache write failed"),
        }
    }
}

/// Ranked ids as little-endian `i64`s, in order.
fn encode_ids(ids: &[i64]) -> Vec<u8> {
    ids.iter().flat_map(|id| id.to_le_bytes()).collect()
}

/// Read back what [`encode_ids`] wrote. `None` on a blob that is not a whole
/// number of `i64`s — a truncated page read as a shorter one would silently
/// drop results.
fn decode_ids(bytes: &[u8]) -> Option<Vec<i64>> {
    if bytes.len() % 8 != 0 {
        return None;
    }
    Some(
        bytes
            .chunks_exact(8)
            .map(|c| {
                let mut buf = [0u8; 8];
                buf.copy_from_slice(c);
                i64::from_le_bytes(buf)
            })
            .collect(),
    )
}
