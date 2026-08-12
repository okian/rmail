//! The listwise-rerank cache prd.md asks for: "Structured output, cached by
//! `(query_hash, candidate_id_set)`."
//!
//! # Why the *set*, not the list
//!
//! The key hashes the candidate ids **sorted and deduplicated**, which makes
//! two requests that retrieved the same messages in a different L1 order one
//! cache entry rather than two. That is the right trade for what this cache
//! protects: a Claude listwise rerank is a paid network round trip whose
//! whole job is to *replace* the incoming order, so the incoming order is an
//! input the answer is deliberately insensitive to. Keying on the ordered
//! list would miss on exactly the case the cache exists for — a user
//! re-running a query after a background reindex nudged two L1 scores past
//! each other — and would pay for a second call to be told the same thing.
//!
//! # What else is in the key, and why
//!
//! The model id and a prompt-shape version are hashed alongside the query and
//! the ids. Neither is cosmetic: a cached ordering is only valid for the
//! prompt and model that produced it, and without them, changing
//! `search.reranker.claude_model` (or editing the prompt) would keep serving
//! verdicts from the old one until the process restarted. Nothing else is
//! keyed — in particular not the *documents*, since a message's text is
//! immutable once synced.
//!
//! # Bounded, in memory, and process-local
//!
//! Entries are content-addressed, so nothing ever invalidates one; without a
//! bound the map would grow with distinct queries for the life of the daemon.
//! [`RerankCache`] therefore evicts least-recently-used entries past
//! `search.reranker.claude_cache_entries`. It is deliberately *not*
//! persisted: a SQLite-backed cache would need a migration, an eviction job,
//! and an invalidation story for prompt changes, to save a repeat call that
//! only matters inside one interactive session — the session this process is
//! already serving.

use std::collections::{HashMap, VecDeque};
use std::sync::{Mutex, PoisonError};

use sha2::{Digest, Sha256};

use super::RerankVerdict;

/// Bumped whenever the listwise prompt or its response schema changes, so a
/// verdict cached under the old shape is never replayed under the new one.
const PROMPT_VERSION: u32 = 1;

/// A content-addressed cache key. Opaque by construction — the only way to
/// build one is [`CacheKey::new`], which is what guarantees every caller
/// hashes the same fields in the same order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CacheKey([u8; 32]);

impl CacheKey {
    /// Hash `(prompt version, model, query, candidate id set)`.
    ///
    /// `candidate_ids` is sorted and deduplicated before hashing, so the
    /// caller may pass them in any order — see the module docs for why the
    /// set rather than the list is the right key.
    #[must_use]
    pub fn new(model: &str, query: &str, candidate_ids: &[i64]) -> Self {
        let mut ids = candidate_ids.to_vec();
        ids.sort_unstable();
        ids.dedup();

        let mut hasher = Sha256::new();
        hasher.update(PROMPT_VERSION.to_le_bytes());
        // Length-prefixed rather than concatenated: without it, model
        // `"ab"` + query `"c"` and model `"a"` + query `"bc"` would hash
        // identically and one query could serve another's cached order.
        hasher.update((model.len() as u64).to_le_bytes());
        hasher.update(model.as_bytes());
        hasher.update((query.len() as u64).to_le_bytes());
        hasher.update(query.as_bytes());
        hasher.update((ids.len() as u64).to_le_bytes());
        for id in ids {
            hasher.update(id.to_le_bytes());
        }
        let digest = hasher.finalize();
        let mut out = [0u8; 32];
        out.copy_from_slice(&digest);
        Self(out)
    }

    /// A short hex prefix, for `tracing` fields. Never used for lookup — a
    /// truncated digest is for reading logs, not for identity.
    #[must_use]
    pub fn short(&self) -> String {
        self.0
            .iter()
            .take(6)
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

/// A bounded, in-memory LRU of listwise verdicts.
#[derive(Debug)]
pub struct RerankCache {
    capacity: usize,
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    entries: HashMap<CacheKey, Vec<RerankVerdict>>,
    /// Least-recently-used first. Small (`capacity` is a few hundred), so a
    /// linear `retain` on hit is cheaper than the allocation an intrusive
    /// list would need.
    recency: VecDeque<CacheKey>,
}

impl RerankCache {
    /// A cache holding at most `capacity` entries. A capacity of zero
    /// disables caching entirely — every lookup misses and every store is
    /// dropped — which is what makes `claude_cache_entries = 0` a real
    /// off switch rather than a degenerate map that keeps one entry.
    #[must_use]
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            inner: Mutex::new(Inner::default()),
        }
    }

    /// The cached verdicts for `key`, if any, marking it most-recently-used.
    #[must_use]
    pub fn get(&self, key: &CacheKey) -> Option<Vec<RerankVerdict>> {
        if self.capacity == 0 {
            return None;
        }
        // Recovered rather than propagated: a poisoned cache mutex means some
        // other task panicked while holding it, which says nothing about the
        // validity of the entries themselves, and refusing every subsequent
        // lookup would silently turn every search into a paid provider call.
        let mut guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let hit = guard.entries.get(key).cloned()?;
        guard.recency.retain(|existing| existing != key);
        guard.recency.push_back(*key);
        Some(hit)
    }

    /// Store `verdicts` under `key`, evicting the least-recently-used entry
    /// if that puts the cache over capacity.
    pub fn insert(&self, key: CacheKey, verdicts: Vec<RerankVerdict>) {
        if self.capacity == 0 {
            return;
        }
        let mut guard = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        if guard.entries.insert(key, verdicts).is_none() {
            guard.recency.push_back(key);
        } else {
            guard.recency.retain(|existing| existing != &key);
            guard.recency.push_back(key);
        }
        while guard.recency.len() > self.capacity {
            let Some(evicted) = guard.recency.pop_front() else {
                break;
            };
            guard.entries.remove(&evicted);
        }
    }

    /// How many entries are held. Test/observability only.
    #[must_use]
    pub fn len(&self) -> usize {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entries
            .len()
    }

    /// Whether the cache holds nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
