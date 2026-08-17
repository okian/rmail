//! What task 36 owes, proved by counting work rather than by re-checking
//! answers.
//!
//! Every incrementality test here asserts on a **call count** — how many times
//! a counting [`Embedder`] was actually invoked, or how many rows exist — not
//! on whether a second call returned the right thing. A cache that hit and a
//! cache that recomputed produce identical results by definition, so a test
//! that only compares outputs passes just as happily against a cache that
//! never hits at all. The call counter is the only assertion that can tell
//! those two apart.
//!
//! Every invalidation test asserts the opposite direction, and does it by
//! changing exactly one input: mail arrives, a rank weight moves, the
//! embedding model changes. A cached entry that survives any of those is a
//! stale search result, which is the one failure this whole module exists to
//! make impossible.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;

use super::*;
use crate::config::{IndexSemanticConfig, RankWeights, SearchConfig};
use crate::embed::{hash::HashEmbedder, Embedder, Embedding};
use crate::error::Error;
use crate::index::semantic::{SemanticIndex, VECTOR_DIM};
use crate::repo;
use crate::storage::Database;

static COUNTER: AtomicU32 = AtomicU32::new(0);

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// An [`Embedder`] that counts the texts it was asked to embed and otherwise
/// delegates to the deterministic hashed backend, so equal inputs give equal
/// vectors and different inputs give different ones.
#[derive(Debug)]
struct CountingEmbedder {
    model: String,
    inner: HashEmbedder,
    /// Texts embedded, cumulative.
    texts: Arc<AtomicUsize>,
    /// `embed` calls, cumulative — distinct from `texts` because one call may
    /// carry a batch, and "one round trip" is the number a hosted provider
    /// bills for.
    calls: Arc<AtomicUsize>,
    /// `warm` calls.
    warms: Arc<AtomicUsize>,
}

impl CountingEmbedder {
    fn new(model: &str) -> Self {
        Self::with_dim(model, 16)
    }

    /// A backend of a specific width, for the one test that writes into
    /// `vec_chunks` — a `vec0` table takes its width at creation time, so the
    /// document path only accepts [`crate::index::semantic::VECTOR_DIM`].
    fn with_dim(model: &str, dim: usize) -> Self {
        Self {
            model: model.to_owned(),
            inner: HashEmbedder::new(dim),
            texts: Arc::new(AtomicUsize::new(0)),
            calls: Arc::new(AtomicUsize::new(0)),
            warms: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn texts(&self) -> usize {
        self.texts.load(Ordering::Relaxed)
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::Relaxed)
    }

    fn warms(&self) -> usize {
        self.warms.load(Ordering::Relaxed)
    }
}

#[async_trait::async_trait]
impl Embedder for CountingEmbedder {
    fn model(&self) -> &str {
        &self.model
    }

    fn dim(&self) -> usize {
        self.inner.dim()
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Embedding>, Error> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.texts.fetch_add(texts.len(), Ordering::Relaxed);
        self.inner.embed(texts).await
    }

    async fn warm(&self) -> Result<(), Error> {
        self.warms.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

/// An embedder that always fails, for the "a broken backend is not silently
/// cached" path.
#[derive(Debug)]
struct BrokenEmbedder;

#[async_trait::async_trait]
impl Embedder for BrokenEmbedder {
    fn model(&self) -> &str {
        "broken-v1"
    }

    fn dim(&self) -> usize {
        16
    }

    async fn embed(&self, _texts: &[String]) -> Result<Vec<Embedding>, Error> {
        Err(Error::internal("no model here"))
    }
}

struct Fixture {
    db: Database,
    path: PathBuf,
    account_id: i64,
    mailbox_id: i64,
    next_uid: std::cell::Cell<i64>,
}

impl Fixture {
    async fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-cache-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", path.display())));
        }
        let db = Database::open(&path).expect("open temp db");
        let (account_id, mailbox_id) = db
            .write(|conn| {
                let account_id = repo::insert_account(
                    conn,
                    &repo::NewAccount {
                        name: "Personal".to_owned(),
                        ..Default::default()
                    },
                )?;
                let mailbox_id = repo::insert_mailbox(
                    conn,
                    &repo::NewMailbox {
                        account_id,
                        name: "INBOX".to_owned(),
                        ..Default::default()
                    },
                )?;
                Ok((account_id, mailbox_id))
            })
            .await
            .expect("seed account");
        Self {
            db,
            path,
            account_id,
            mailbox_id,
            next_uid: std::cell::Cell::new(1),
        }
    }

    async fn add_message(&self) -> i64 {
        self.add_message_with(None).await
    }

    async fn add_message_with(&self, text: Option<&str>) -> i64 {
        let uid = self.next_uid.get();
        self.next_uid.set(uid + 1);
        let (account_id, mailbox_id) = (self.account_id, self.mailbox_id);
        let text = text.map(str::to_owned);
        self.db
            .write(move |conn| {
                let id = repo::insert_message(
                    conn,
                    &repo::NewMessage {
                        account_id,
                        mailbox_id,
                        uid,
                        uidvalidity: 1,
                        ..Default::default()
                    },
                )?;
                if let Some(text) = text {
                    conn.execute(
                        "INSERT INTO index_content
                             (message_id, part, text, chars, content_hash, extractor)
                         VALUES (?1, 'body', ?2, ?3, X'00', 'test')",
                        rusqlite::params![id, text, text.len() as i64],
                    )?;
                }
                Ok(id)
            })
            .await
            .expect("insert message")
    }

    fn stamp(&self) -> CorpusStamp {
        self.db.with_read(corpus::read).expect("read corpus stamp")
    }

    fn version(&self) -> i64 {
        self.stamp().version
    }

    fn count(&self, table: &str) -> i64 {
        let sql = format!("SELECT count(*) FROM {table}");
        self.db
            .with_read(move |conn| conn.query_row(&sql, [], |row| row.get(0)))
            .expect("count rows")
    }

    fn stats(&self) -> CacheStats {
        self.db.with_read(stats).expect("read cache stats")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.path.display())));
        }
    }
}

/// A cache config whose bypass is off, so hits are observable at all. See
/// [`CacheConfig::fresh_window_secs`](crate::config::CacheConfig::fresh_window_secs).
fn testable_config() -> CacheConfig {
    CacheConfig {
        enabled: true,
        result_ttl_secs: 3_600,
        max_results: 64,
        max_embeddings: 64,
        fresh_window_secs: 0,
    }
}

fn fingerprint_of(search: &SearchConfig) -> RankerFingerprint {
    RankerFingerprint::new(search, &IndexSemanticConfig::default(), "test-model", 16)
}

fn parts<'a>(query: &'a str) -> ResultKeyParts<'a> {
    ResultKeyParts {
        query,
        filter: "",
        account_id: 1,
        mode: "hybrid",
        limit: 25,
        rerank: "auto",
        kind: "interactive",
    }
}

fn expect_lease(lookup: Lookup) -> Lease {
    match lookup {
        Lookup::Miss(lease) => lease,
        other => unreachable!("expected a miss, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Corpus version
// ---------------------------------------------------------------------------

#[tokio::test]
async fn new_mail_bumps_the_corpus_version() {
    let fx = Fixture::open().await;
    let before = fx.version();
    fx.add_message().await;
    assert!(
        fx.version() > before,
        "an inserted message must move the corpus version"
    );
}

#[tokio::test]
async fn a_flag_change_bumps_the_corpus_version() {
    let fx = Fixture::open().await;
    let id = fx.add_message().await;
    let before = fx.version();
    fx.db
        .write(move |conn| {
            conn.execute(
                "INSERT INTO flags (message_id, flag) VALUES (?1, '\\Seen')",
                rusqlite::params![id],
            )
        })
        .await
        .expect("set flag");
    assert!(
        fx.version() > before,
        "`is:unread` changes what a search matches, so a flag write must \
         invalidate cached results even though `messages` was untouched"
    );
}

#[tokio::test]
async fn a_tag_change_bumps_the_corpus_version() {
    let fx = Fixture::open().await;
    let id = fx.add_message().await;
    let account_id = fx.account_id;
    let before = fx.version();
    fx.db
        .write(move |conn| {
            conn.execute(
                "INSERT INTO tags (account_id, name) VALUES (?1, 'work')",
                rusqlite::params![account_id],
            )?;
            let tag_id = conn.last_insert_rowid();
            conn.execute(
                "INSERT INTO message_tags (tag_id, message_id) VALUES (?1, ?2)",
                rusqlite::params![tag_id, id],
            )
        })
        .await
        .expect("tag message");
    assert!(fx.version() > before, "`tag:` matching must invalidate");
}

#[tokio::test]
async fn re_extracted_text_bumps_the_corpus_version() {
    let fx = Fixture::open().await;
    let id = fx.add_message_with(Some("original text")).await;
    let before = fx.version();
    fx.db
        .write(move |conn| {
            conn.execute(
                "UPDATE index_content SET text = 'replaced' WHERE message_id = ?1",
                rusqlite::params![id],
            )
        })
        .await
        .expect("re-extract");
    assert!(
        fx.version() > before,
        "a reindex that changes the matched text must invalidate results"
    );
}

/// The hole a first pass at this left open, and the reason `index_state` is in
/// the trigger set.
///
/// The lexical, entity and semantic stages write `fts_messages`, `entities`
/// and `chunks`/`vec_chunks` — none of which the other triggers watch, and two
/// of which are virtual tables SQLite will not put a trigger on at all. So
/// mail could land, extraction could run, a search could be cached, and then
/// the semantic stage could drain and bring that message into the dense arm
/// with the version standing still — leaving the identical query serving its
/// pre-embedding answer for a whole TTL. Every stage records its completion in
/// `index_state`, so watching that one table closes all three.
#[tokio::test]
async fn a_finished_index_stage_bumps_the_corpus_version() {
    let fx = Fixture::open().await;
    let id = fx.add_message().await;
    let before = fx.version();
    fx.db
        .write(move |conn| {
            conn.execute(
                "INSERT INTO index_state (message_id, kind, content_hash, model)
                 VALUES (?1, 'semantic', X'11', 'stub-v1')",
                rusqlite::params![id],
            )
        })
        .await
        .expect("record a finished stage");
    assert!(
        fx.version() > before,
        "a stage that just brought a message into the dense arm must \
         invalidate results computed before it ran"
    );
}

/// The destructive direction: `IndexAdmin::wipe_stage` deletes `index_state`
/// rows for the stage it is rebuilding and touches none of the other watched
/// tables, so without this `mail index rebuild --kind semantic` would leave
/// every cached page readable while the index it was computed from was gone.
#[tokio::test]
async fn wiping_an_index_stage_bumps_the_corpus_version() {
    let fx = Fixture::open().await;
    let id = fx.add_message().await;
    fx.db
        .write(move |conn| {
            conn.execute(
                "INSERT INTO index_state (message_id, kind, content_hash, model)
                 VALUES (?1, 'lexical', X'11', NULL)",
                rusqlite::params![id],
            )
        })
        .await
        .expect("record a finished stage");
    let before = fx.version();
    fx.db
        .write(|conn| conn.execute("DELETE FROM index_state WHERE kind = 'lexical'", []))
        .await
        .expect("wipe the stage");
    assert!(fx.version() > before, "a rebuild must invalidate");
}

#[tokio::test]
async fn deleting_mail_bumps_the_corpus_version() {
    let fx = Fixture::open().await;
    let id = fx.add_message().await;
    let before = fx.version();
    fx.db
        .write(move |conn| conn.execute("DELETE FROM messages WHERE id = ?1", [id]))
        .await
        .expect("delete message");
    assert!(fx.version() > before, "a deletion must invalidate");
}

#[test]
fn a_zero_window_switches_the_freshness_bypass_off() {
    let stamp = CorpusStamp {
        version: 7,
        changed_at: 1_000,
    };
    assert!(stamp.is_fresh(1_000, 30), "just changed");
    assert!(stamp.is_fresh(1_029, 30), "inside the window");
    assert!(!stamp.is_fresh(1_030, 30), "the window is half-open");
    assert!(!stamp.is_fresh(1_000, 0), "zero is off, not zero-width");
    assert!(
        stamp.is_fresh(900, 30),
        "a backwards clock is 'I do not know', which must bypass"
    );
}

// ---------------------------------------------------------------------------
// Embedding cache — the query half
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_repeated_query_is_embedded_once() {
    let fx = Fixture::open().await;
    let inner = Arc::new(CountingEmbedder::new("stub-v1"));
    let embedder = CachingEmbedder::new(fx.db.clone(), Arc::clone(&inner) as Arc<dyn Embedder>, 64);

    let query = vec!["who owes me money".to_owned()];
    let first = embedder.embed(&query).await.expect("first embed");
    assert_eq!(inner.calls(), 1, "the first call must reach the backend");

    let second = embedder.embed(&query).await.expect("second embed");
    assert_eq!(
        inner.calls(),
        1,
        "the second call must be served from the cache — this is the whole \
         claim, and only the call count can prove it"
    );
    assert_eq!(first, second, "and the cached vector must be the same one");
    assert_eq!(fx.count("embedding_cache"), 1);
}

#[tokio::test]
async fn a_batch_pays_only_for_its_misses() {
    let fx = Fixture::open().await;
    let inner = Arc::new(CountingEmbedder::new("stub-v1"));
    let embedder = CachingEmbedder::new(fx.db.clone(), Arc::clone(&inner) as Arc<dyn Embedder>, 64);

    embedder
        .embed(&["alpha".to_owned()])
        .await
        .expect("warm the cache");
    assert_eq!(inner.texts(), 1);

    let batch = vec!["alpha".to_owned(), "beta".to_owned(), "alpha".to_owned()];
    let out = embedder.embed(&batch).await.expect("mixed batch");
    assert_eq!(out.len(), 3, "one vector per input, in input order");
    assert_eq!(out[0], out[2], "the repeat resolves to the same vector");
    assert_eq!(
        inner.texts(),
        2,
        "only 'beta' was new: the cached hit and the within-batch duplicate \
         must both be free"
    );
}

#[tokio::test]
async fn a_different_model_never_serves_another_models_vector() {
    let fx = Fixture::open().await;
    let first_backend = Arc::new(CountingEmbedder::new("model-a"));
    let second_backend = Arc::new(CountingEmbedder::new("model-b"));
    let query = vec!["quarterly report".to_owned()];

    let a = CachingEmbedder::new(
        fx.db.clone(),
        Arc::clone(&first_backend) as Arc<dyn Embedder>,
        64,
    );
    a.embed(&query).await.expect("embed under model-a");

    let b = CachingEmbedder::new(
        fx.db.clone(),
        Arc::clone(&second_backend) as Arc<dyn Embedder>,
        64,
    );
    b.embed(&query).await.expect("embed under model-b");

    assert_eq!(
        second_backend.calls(),
        1,
        "a model switch must miss: vectors from two models are not comparable, \
         and serving one for the other is a silently wrong ranking"
    );
    assert_eq!(fx.count("embedding_cache"), 2, "one row per model");
}

#[tokio::test]
async fn the_embedding_cache_stays_within_its_bound() {
    let fx = Fixture::open().await;
    let inner = Arc::new(CountingEmbedder::new("stub-v1"));
    let embedder = CachingEmbedder::new(fx.db.clone(), Arc::clone(&inner) as Arc<dyn Embedder>, 2);

    for text in ["one", "two", "three", "four"] {
        embedder
            .embed(&[text.to_owned()])
            .await
            .expect("embed distinct text");
    }
    assert_eq!(
        fx.count("embedding_cache"),
        2,
        "the LRU bound is what keeps this table from growing with distinct \
         queries for the life of the daemon"
    );
}

#[tokio::test]
async fn a_zero_capacity_cache_stores_nothing() {
    let fx = Fixture::open().await;
    let inner = Arc::new(CountingEmbedder::new("stub-v1"));
    let embedder = CachingEmbedder::new(fx.db.clone(), Arc::clone(&inner) as Arc<dyn Embedder>, 0);

    let query = vec!["anything".to_owned()];
    embedder.embed(&query).await.expect("first");
    embedder.embed(&query).await.expect("second");

    assert_eq!(inner.calls(), 2, "capacity 0 is a real off switch");
    assert_eq!(fx.count("embedding_cache"), 0);
}

#[tokio::test]
async fn a_corrupt_row_is_dropped_and_recomputed() {
    let fx = Fixture::open().await;
    let inner = Arc::new(CountingEmbedder::new("stub-v1"));
    let embedder = CachingEmbedder::new(fx.db.clone(), Arc::clone(&inner) as Arc<dyn Embedder>, 64);

    let query = vec!["corrupted".to_owned()];
    embedder.embed(&query).await.expect("populate");
    fx.db
        .write(|conn| conn.execute("UPDATE embedding_cache SET vector = X'0102'", []))
        .await
        .expect("corrupt the row");

    let out = embedder.embed(&query).await.expect("recompute");
    assert_eq!(out.len(), 1);
    assert_eq!(inner.calls(), 2, "a truncated blob must not be read short");
    assert_eq!(
        fx.count("embedding_cache"),
        1,
        "and the bad row must be replaced, not re-skipped on every query"
    );
    let out2 = embedder.embed(&query).await.expect("now cached again");
    assert_eq!(inner.calls(), 2, "the replacement row must serve");
    assert_eq!(out, out2);
}

#[tokio::test]
async fn a_failing_backend_caches_nothing() {
    let fx = Fixture::open().await;
    let embedder = CachingEmbedder::new(fx.db.clone(), Arc::new(BrokenEmbedder), 64);
    let error = embedder
        .embed(&["anything".to_owned()])
        .await
        .expect_err("a broken backend must surface its error");
    assert!(matches!(error, Error::Internal(_)));
    assert_eq!(
        fx.count("embedding_cache"),
        0,
        "a failure must not be cached as an answer"
    );
}

#[tokio::test]
async fn warming_loads_the_model_instead_of_hitting_the_cache() {
    let fx = Fixture::open().await;
    let inner = Arc::new(CountingEmbedder::new("stub-v1"));
    let embedder = CachingEmbedder::new(fx.db.clone(), Arc::clone(&inner) as Arc<dyn Embedder>, 64);

    embedder.warm().await.expect("first warm");
    embedder.warm().await.expect("second warm");

    assert_eq!(
        inner.warms(),
        2,
        "warming exists to force a model load before the first user query; \
         served from the cache it would return instantly having loaded nothing"
    );
    assert_eq!(
        fx.count("embedding_cache"),
        0,
        "and it must not fill the cache with its probe text"
    );
}

// ---------------------------------------------------------------------------
// Embedding cache — the document half (already `content_hash`-keyed)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn unchanged_documents_are_not_re_embedded() {
    let fx = Fixture::open().await;
    let body: String = (0..12)
        .map(|n| format!("Paragraph {n} is about renewals and says so at length.\n\n"))
        .collect();
    let message_id = fx.add_message_with(Some(&body)).await;
    let embedder = Arc::new(CountingEmbedder::with_dim("stub-v1", VECTOR_DIM));
    let index = SemanticIndex::new(
        fx.db.clone(),
        Arc::clone(&embedder) as Arc<dyn Embedder>,
        &IndexSemanticConfig {
            chunk_tokens: 32,
            chunk_overlap: 4,
            ..IndexSemanticConfig::default()
        },
    );

    let first = index.index_message(message_id).await.expect("first pass");
    assert!(first.embedded > 0, "the first pass must actually embed");
    let after_first = embedder.texts();

    let second = index.index_message(message_id).await.expect("second pass");
    assert_eq!(
        second.embedded, 0,
        "prd.md: documents are re-embedded only on a content_hash change"
    );
    assert_eq!(
        embedder.texts(),
        after_first,
        "and the backend must not be called at all — the report saying zero \
         proves only what the report says"
    );
}

// ---------------------------------------------------------------------------
// Result cache
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_stored_page_is_served_back() {
    let fx = Fixture::open().await;
    let search = SearchConfig::default();
    let cache = ResultCache::new(fx.db.clone(), testable_config(), fingerprint_of(&search));

    let lease = expect_lease(cache.lookup(&parts("invoices")).await);
    cache.store(lease, &[7, 3, 9]).await;

    match cache.lookup(&parts("invoices")).await {
        Lookup::Hit(ids) => assert_eq!(ids, vec![7, 3, 9], "order is the answer"),
        other => unreachable!("expected a hit, got {other:?}"),
    }
    assert_eq!(fx.stats().result_uses, 1, "a hit must record itself");
}

#[tokio::test]
async fn a_different_query_does_not_share_a_page() {
    let fx = Fixture::open().await;
    let search = SearchConfig::default();
    let cache = ResultCache::new(fx.db.clone(), testable_config(), fingerprint_of(&search));

    let lease = expect_lease(cache.lookup(&parts("invoices")).await);
    cache.store(lease, &[1, 2]).await;

    assert!(
        matches!(cache.lookup(&parts("receipts")).await, Lookup::Miss(_)),
        "two questions are two entries"
    );
}

#[tokio::test]
async fn new_mail_invalidates_a_cached_page() {
    let fx = Fixture::open().await;
    let search = SearchConfig::default();
    let cache = ResultCache::new(fx.db.clone(), testable_config(), fingerprint_of(&search));

    let lease = expect_lease(cache.lookup(&parts("invoices")).await);
    cache.store(lease, &[1, 2, 3]).await;
    assert!(matches!(
        cache.lookup(&parts("invoices")).await,
        Lookup::Hit(_)
    ));

    fx.add_message().await;

    assert!(
        matches!(cache.lookup(&parts("invoices")).await, Lookup::Miss(_)),
        "prd.md: the result cache is invalidated when the corpus version \
         bumps. A hit here would be mail the user cannot find."
    );
}

#[tokio::test]
async fn a_retuned_ranker_invalidates_a_cached_page() {
    let fx = Fixture::open().await;
    let before = SearchConfig::default();
    let cache = ResultCache::new(fx.db.clone(), testable_config(), fingerprint_of(&before));
    let lease = expect_lease(cache.lookup(&parts("invoices")).await);
    cache.store(lease, &[1, 2, 3]).await;

    let after = SearchConfig {
        rank_weights: RankWeights(BTreeMap::from([("bm25_subject".to_owned(), 9.5)])),
        ..SearchConfig::default()
    };
    assert_ne!(
        fingerprint_of(&before),
        fingerprint_of(&after),
        "a rank weight change must change the fingerprint"
    );
    let retuned = ResultCache::new(fx.db.clone(), testable_config(), fingerprint_of(&after));

    assert!(
        matches!(retuned.lookup(&parts("invoices")).await, Lookup::Miss(_)),
        "prd.md: invalidated when the active ranker changes"
    );
}

#[tokio::test]
async fn a_changed_embedding_model_invalidates_a_cached_page() {
    let fx = Fixture::open().await;
    let search = SearchConfig::default();
    let cache = ResultCache::new(
        fx.db.clone(),
        testable_config(),
        RankerFingerprint::new(&search, &IndexSemanticConfig::default(), "model-a", 16),
    );
    let lease = expect_lease(cache.lookup(&parts("invoices")).await);
    cache.store(lease, &[1, 2, 3]).await;

    let swapped = ResultCache::new(
        fx.db.clone(),
        testable_config(),
        RankerFingerprint::new(&search, &IndexSemanticConfig::default(), "model-b", 16),
    );
    assert!(
        matches!(swapped.lookup(&parts("invoices")).await, Lookup::Miss(_)),
        "the dense arm's contribution to an ordering is a property of the \
         model that produced the vectors"
    );
}

/// `[index.semantic]` is not part of `[search]`, and it decides how the corpus
/// was chunked — which decides what the dense arm returns. Retuning
/// `chunk_tokens` and running `mail index rebuild --kind semantic` changes
/// result order, so it has to move the fingerprint too.
#[tokio::test]
async fn a_retuned_chunker_invalidates_a_cached_page() {
    let fx = Fixture::open().await;
    let search = SearchConfig::default();
    let before = RankerFingerprint::new(&search, &IndexSemanticConfig::default(), "m", 16);
    let cache = ResultCache::new(fx.db.clone(), testable_config(), before);
    let lease = expect_lease(cache.lookup(&parts("invoices")).await);
    cache.store(lease, &[1, 2, 3]).await;

    let rechunked = IndexSemanticConfig {
        chunk_tokens: 128,
        ..IndexSemanticConfig::default()
    };
    let after = RankerFingerprint::new(&search, &rechunked, "m", 16);
    assert_ne!(before, after, "chunking changes which vectors exist");
    let retuned = ResultCache::new(fx.db.clone(), testable_config(), after);
    assert!(matches!(
        retuned.lookup(&parts("invoices")).await,
        Lookup::Miss(_)
    ));
}

#[tokio::test]
async fn freshly_synced_mail_bypasses_the_result_cache() {
    let fx = Fixture::open().await;
    let search = SearchConfig::default();
    let config = CacheConfig {
        fresh_window_secs: 0,
        ..testable_config()
    };
    let cache = ResultCache::new(fx.db.clone(), config, fingerprint_of(&search));
    let lease = expect_lease(cache.lookup(&parts("invoices")).await);
    cache.store(lease, &[1, 2, 3]).await;
    assert!(
        matches!(cache.lookup(&parts("invoices")).await, Lookup::Hit(_)),
        "the entry is there with the bypass off"
    );

    // Same entry, same corpus version — only the freshness window changes.
    let guarded = ResultCache::new(
        fx.db.clone(),
        CacheConfig {
            fresh_window_secs: 3_600,
            ..testable_config()
        },
        fingerprint_of(&search),
    );
    assert_eq!(
        guarded.lookup(&parts("invoices")).await,
        Lookup::Bypass(BypassReason::FreshCorpus),
        "prd.md: newly-synced mail bypasses the cache, so fresh mail is never \
         hidden by a stale cached result"
    );
}

#[tokio::test]
async fn a_bypass_writes_nothing() {
    let fx = Fixture::open().await;
    let search = SearchConfig::default();
    let cache = ResultCache::new(
        fx.db.clone(),
        CacheConfig {
            fresh_window_secs: 3_600,
            ..testable_config()
        },
        fingerprint_of(&search),
    );
    assert!(matches!(
        cache.lookup(&parts("invoices")).await,
        Lookup::Bypass(_)
    ));
    assert_eq!(
        fx.count("search_result_cache"),
        0,
        "a bypass hands back no lease, so there is nothing to store with"
    );
}

#[tokio::test]
async fn a_disabled_cache_neither_reads_nor_writes() {
    let fx = Fixture::open().await;
    let search = SearchConfig::default();
    let config = CacheConfig {
        enabled: false,
        ..testable_config()
    };
    let cache = ResultCache::new(fx.db.clone(), config, fingerprint_of(&search));

    assert_eq!(
        cache.lookup(&parts("invoices")).await,
        Lookup::Bypass(BypassReason::Disabled)
    );
    // A lease minted by an enabled cache must still be refused by a disabled
    // one: the switch is about what is on disk, not about what a caller asks.
    let enabled = ResultCache::new(fx.db.clone(), testable_config(), fingerprint_of(&search));
    let lease = expect_lease(enabled.lookup(&parts("invoices")).await);
    cache.store(lease, &[1, 2, 3]).await;
    assert_eq!(fx.count("search_result_cache"), 0);
}

#[tokio::test]
async fn mail_landing_mid_search_prevents_the_store() {
    let fx = Fixture::open().await;
    let search = SearchConfig::default();
    let cache = ResultCache::new(fx.db.clone(), testable_config(), fingerprint_of(&search));

    // The lease is taken before the pipeline runs...
    let lease = expect_lease(cache.lookup(&parts("invoices")).await);
    // ...and mail lands while it is running.
    fx.add_message().await;
    cache.store(lease, &[1, 2, 3]).await;

    assert_eq!(
        fx.count("search_result_cache"),
        0,
        "a page computed across a corpus change describes a corpus that no \
         longer exists; filing it under either version is how a cache lies"
    );
}

#[tokio::test]
async fn an_expired_page_is_not_served() {
    let fx = Fixture::open().await;
    let search = SearchConfig::default();
    let cache = ResultCache::new(fx.db.clone(), testable_config(), fingerprint_of(&search));
    let lease = expect_lease(cache.lookup(&parts("invoices")).await);
    cache.store(lease, &[1, 2, 3]).await;

    // Backdate the row rather than sleeping: the TTL is the thing under test,
    // not the clock.
    fx.db
        .write(|conn| {
            conn.execute(
                "UPDATE search_result_cache SET created_at = created_at - 10000",
                [],
            )
        })
        .await
        .expect("backdate");

    assert!(matches!(
        cache.lookup(&parts("invoices")).await,
        Lookup::Miss(_)
    ));
}

#[tokio::test]
async fn the_result_cache_stays_within_its_bound() {
    let fx = Fixture::open().await;
    let search = SearchConfig::default();
    let cache = ResultCache::new(
        fx.db.clone(),
        CacheConfig {
            max_results: 2,
            ..testable_config()
        },
        fingerprint_of(&search),
    );
    for query in ["one", "two", "three", "four"] {
        let lease = expect_lease(cache.lookup(&parts(query)).await);
        cache.store(lease, &[1]).await;
    }
    assert_eq!(fx.count("search_result_cache"), 2);
}

#[test]
fn key_fields_cannot_bleed_into_each_other() {
    let search = SearchConfig::default();
    let fingerprint = fingerprint_of(&search);
    let left = ResultKey::new(
        &ResultKeyParts {
            query: "ab",
            filter: "c",
            ..parts("")
        },
        1,
        &fingerprint,
    );
    let right = ResultKey::new(
        &ResultKeyParts {
            query: "a",
            filter: "bc",
            ..parts("")
        },
        1,
        &fingerprint,
    );
    assert_ne!(
        left, right,
        "without length prefixes one search would serve another's results"
    );
}

#[test]
fn every_request_field_is_part_of_the_key() {
    let search = SearchConfig::default();
    let fp = fingerprint_of(&search);
    let base = parts("invoices");
    let key = ResultKey::new(&base, 1, &fp);

    let variants = [
        ResultKeyParts {
            query: "receipts",
            ..base
        },
        ResultKeyParts {
            filter: "is:unread",
            ..base
        },
        ResultKeyParts {
            account_id: 2,
            ..base
        },
        ResultKeyParts {
            mode: "semantic",
            ..base
        },
        ResultKeyParts { limit: 50, ..base },
        ResultKeyParts {
            rerank: "off",
            ..base
        },
        ResultKeyParts {
            kind: "deep",
            ..base
        },
    ];
    for variant in variants {
        assert_ne!(
            key,
            ResultKey::new(&variant, 1, &fp),
            "changing {variant:?} must change the key"
        );
    }
    assert_ne!(
        key,
        ResultKey::new(&base, 2, &fp),
        "the corpus version is in the key"
    );
}

// ---------------------------------------------------------------------------
// Operator surface
// ---------------------------------------------------------------------------

#[tokio::test]
async fn stats_report_every_cache() {
    let fx = Fixture::open().await;
    let search = SearchConfig::default();
    let cache = ResultCache::new(fx.db.clone(), testable_config(), fingerprint_of(&search));
    let inner = Arc::new(CountingEmbedder::new("stub-v1"));
    let embedder = CachingEmbedder::new(fx.db.clone(), Arc::clone(&inner) as Arc<dyn Embedder>, 64);

    embedder
        .embed(&["a query".to_owned()])
        .await
        .expect("embed");
    // A batch mixing the cached text with a new one. The hit is what gets
    // counted — `uses` is stamped in the write transaction the miss opens, so
    // that the lookup itself never takes the writer lock (see
    // `EmbeddingCache::put_many`).
    embedder
        .embed(&["a query".to_owned(), "another".to_owned()])
        .await
        .expect("embed a mixed batch");
    let lease = expect_lease(cache.lookup(&parts("invoices")).await);
    cache.store(lease, &[1, 2]).await;
    let _ = cache.lookup(&parts("invoices")).await;
    seed_query_plan(&fx).await;

    let stats = fx.stats();
    assert_eq!(stats.corpus_version, fx.version());
    assert_eq!(stats.embeddings, 2);
    assert_eq!(stats.embedding_uses, 1, "one read served without a backend");
    assert_eq!(stats.results, 1);
    assert_eq!(stats.result_uses, 1);
    assert_eq!(stats.query_plans, 1, "task 58's cache is reported too");
    assert_eq!(stats.stale_results, 0);
}

/// The lookups on the interactive search path must not take the process-wide
/// writer mutex — `Database` exists so reads never block on writes, and a
/// cache hit that queued behind a sync's bulk insert would be slower than the
/// work it replaced.
///
/// Proved by holding the writer and then doing a lookup: if either lookup
/// wrote, this test would deadlock against the held guard rather than fail.
#[tokio::test]
async fn a_cache_hit_does_not_wait_on_the_writer_lock() {
    let fx = Fixture::open().await;
    let inner = Arc::new(CountingEmbedder::new("stub-v1"));
    let embedder = CachingEmbedder::new(fx.db.clone(), Arc::clone(&inner) as Arc<dyn Embedder>, 64);
    let query = vec!["held".to_owned()];
    embedder.embed(&query).await.expect("populate");

    let db = fx.db.clone();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
    let (held_tx, held_rx) = tokio::sync::oneshot::channel::<()>();
    // A writer held for as long as the lookup below takes, standing in for the
    // bulk-insert transaction a sync holds.
    let holder = tokio::task::spawn_blocking(move || {
        db.with_write(|_conn| {
            let _ = held_tx.send(());
            // Blocking on purpose: this is the writer being unavailable.
            let _ = release_rx.blocking_recv();
            Ok(())
        })
    });
    held_rx.await.expect("writer taken");

    let hit = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        embedder.embed(&query.clone()),
    )
    .await
    .expect("a cache lookup must not wait on the writer")
    .expect("embed");
    assert_eq!(hit.len(), 1);
    assert_eq!(inner.calls(), 1, "and it was served from the cache");

    let _ = release_tx.send(());
    holder.await.expect("join holder").expect("writer closure");
}

#[tokio::test]
async fn stats_expose_pages_a_corpus_bump_stranded() {
    let fx = Fixture::open().await;
    let search = SearchConfig::default();
    let cache = ResultCache::new(fx.db.clone(), testable_config(), fingerprint_of(&search));
    let lease = expect_lease(cache.lookup(&parts("invoices")).await);
    cache.store(lease, &[1]).await;
    fx.add_message().await;

    assert_eq!(
        fx.stats().stale_results,
        1,
        "an entry no lookup can address again is visible to an operator, so \
         a sweep that is not running has a symptom"
    );
}

#[tokio::test]
async fn a_sweep_drops_stranded_and_expired_pages_only() {
    let fx = Fixture::open().await;
    let search = SearchConfig::default();
    let cache = ResultCache::new(fx.db.clone(), testable_config(), fingerprint_of(&search));
    let lease = expect_lease(cache.lookup(&parts("invoices")).await);
    cache.store(lease, &[1]).await;
    let inner = Arc::new(CountingEmbedder::new("stub-v1"));
    let embedder = CachingEmbedder::new(fx.db.clone(), Arc::clone(&inner) as Arc<dyn Embedder>, 64);
    embedder.embed(&["kept".to_owned()]).await.expect("embed");

    fx.add_message().await;
    let now = chrono::Utc::now().timestamp();
    let config = testable_config();
    let report = fx
        .db
        .write(move |conn| sweep(conn, &config, now))
        .await
        .expect("sweep");

    assert_eq!(report.results, 1, "the stranded page goes");
    assert_eq!(
        report.embeddings, 0,
        "the query vector is still addressable"
    );
    assert_eq!(fx.count("search_result_cache"), 0);
    assert_eq!(fx.count("embedding_cache"), 1);
}

#[tokio::test]
async fn a_purge_clears_every_cache() {
    let fx = Fixture::open().await;
    let search = SearchConfig::default();
    let cache = ResultCache::new(fx.db.clone(), testable_config(), fingerprint_of(&search));
    let lease = expect_lease(cache.lookup(&parts("invoices")).await);
    cache.store(lease, &[1]).await;
    let inner = Arc::new(CountingEmbedder::new("stub-v1"));
    let embedder = CachingEmbedder::new(fx.db.clone(), Arc::clone(&inner) as Arc<dyn Embedder>, 64);
    embedder.embed(&["gone".to_owned()]).await.expect("embed");
    seed_query_plan(&fx).await;

    let report = fx.db.write(purge).await.expect("purge");
    assert_eq!(report.results, 1);
    assert_eq!(report.embeddings, 1);
    assert_eq!(report.query_plans, 1);
    assert_eq!(report.total(), 3);
    assert_eq!(fx.count("search_result_cache"), 0);
    assert_eq!(fx.count("embedding_cache"), 0);
    assert_eq!(fx.count("query_plan_cache"), 0);
}

/// One row in task 58's cache, written with raw SQL — this module reports and
/// purges that table but does not own it.
async fn seed_query_plan(fx: &Fixture) {
    let account_id = fx.account_id;
    fx.db
        .write(move |conn| {
            conn.execute(
                "INSERT INTO query_plan_cache
                     (account_id, query_hash, raw, compiled, intent, notes, model)
                 VALUES (?1, 'deadbeef', 'who owes me money', 'is:unread invoice',
                         'lookup', 'note', 'test-model')",
                rusqlite::params![account_id],
            )
        })
        .await
        .expect("seed a query plan");
}
