//! kNN behavior, the conditions under which work is skipped, and the drift a
//! model switch creates.
//!
//! Driven by a deterministic stub embedder rather than the real model: what is
//! under test here is the bookkeeping between three tables, and a stub makes
//! "which vector is nearest" a fact rather than a hope. The real model's
//! semantics are tested where the real model is, in `embed::local`.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use super::*;
use crate::embed::hash::HashEmbedder;
use crate::repo;
use crate::ErrorReason;

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A 384-dimensional embedder that counts how often it was asked.
///
/// Wraps the hashed-feature embedder — which is deterministic, dependency-free
/// and recovers lexical overlap — so "the right chunk is nearest" is decidable
/// without loading a model. The counter is what makes "this work was skipped"
/// checkable at all: a re-index that quietly recomputed everything would
/// otherwise look identical to one that skipped.
#[derive(Debug)]
struct CountingEmbedder {
    inner: HashEmbedder,
    model: String,
    calls: AtomicUsize,
    texts: AtomicUsize,
}

impl CountingEmbedder {
    fn new(model: &str) -> Self {
        Self {
            inner: HashEmbedder::new(VECTOR_DIM),
            model: model.to_owned(),
            calls: AtomicUsize::new(0),
            texts: AtomicUsize::new(0),
        }
    }

    fn texts(&self) -> usize {
        self.texts.load(Ordering::Relaxed)
    }
}

#[async_trait::async_trait]
impl Embedder for CountingEmbedder {
    fn model(&self) -> &str {
        &self.model
    }

    fn dim(&self) -> usize {
        VECTOR_DIM
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Embedding>, Error> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.texts.fetch_add(texts.len(), Ordering::Relaxed);
        self.inner.embed(texts).await
    }
}

/// An embedder whose successive vectors cancel exactly.
///
/// The only way to reach the zero-centroid branch: chunk vectors that are
/// individually fine and average to nothing. Contrived, but the branch guards
/// against storing a universal half-match, and an untested guard is a guess.
#[derive(Debug)]
struct OppositeEmbedder {
    next: AtomicUsize,
}

#[async_trait::async_trait]
impl Embedder for OppositeEmbedder {
    fn model(&self) -> &str {
        "opposite"
    }
    fn dim(&self) -> usize {
        VECTOR_DIM
    }
    async fn embed(&self, texts: &[String]) -> Result<Vec<Embedding>, Error> {
        Ok(texts
            .iter()
            .map(|_| {
                let n = self.next.fetch_add(1, Ordering::Relaxed);
                let sign = if n % 2 == 0 { 1.0 } else { -1.0 };
                let mut values = vec![0.0f32; VECTOR_DIM];
                values[0] = sign;
                Embedding::new(values)
            })
            .collect())
    }
}

/// An embedder of the wrong width, for the schema guard.
#[derive(Debug)]
struct NarrowEmbedder;

#[async_trait::async_trait]
impl Embedder for NarrowEmbedder {
    fn model(&self) -> &str {
        "narrow"
    }
    fn dim(&self) -> usize {
        128
    }
    async fn embed(&self, texts: &[String]) -> Result<Vec<Embedding>, Error> {
        Ok(texts
            .iter()
            .map(|_| Embedding::new(vec![1.0; 128]))
            .collect())
    }
}

struct Fixture {
    db: Database,
    account_id: i64,
    mailbox_id: i64,
    next_uid: std::cell::Cell<i64>,
    path: PathBuf,
}

impl Fixture {
    async fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-semantic-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", path.display())));
        }
        let db = Database::open(&path).unwrap();
        let (account_id, mailbox_id) = db
            .write(|c| {
                let account_id = repo::insert_account(
                    c,
                    &repo::NewAccount {
                        name: "Personal".to_owned(),
                        ..Default::default()
                    },
                )?;
                let mailbox_id = repo::insert_mailbox(
                    c,
                    &repo::NewMailbox {
                        account_id,
                        name: "INBOX".to_owned(),
                        ..Default::default()
                    },
                )?;
                Ok((account_id, mailbox_id))
            })
            .await
            .unwrap();
        Self {
            db,
            account_id,
            mailbox_id,
            next_uid: std::cell::Cell::new(1),
            path,
        }
    }

    async fn message_with(&self, text: &str) -> i64 {
        self.message_parts(&[("body", text)]).await
    }

    async fn message_parts(&self, parts: &[(&str, &str)]) -> i64 {
        let uid = self.next_uid.get();
        self.next_uid.set(uid + 1);
        let (account_id, mailbox_id) = (self.account_id, self.mailbox_id);
        let parts: Vec<(String, String)> = parts
            .iter()
            .map(|(p, t)| ((*p).to_owned(), (*t).to_owned()))
            .collect();
        self.db
            .write(move |c| {
                let id = repo::insert_message(
                    c,
                    &repo::NewMessage {
                        account_id,
                        mailbox_id,
                        uid,
                        uidvalidity: 1,
                        ..Default::default()
                    },
                )?;
                for (part, text) in &parts {
                    c.execute(
                        "INSERT INTO index_content
                             (message_id, part, text, chars, content_hash, extractor)
                         VALUES (?1, ?2, ?3, ?4, X'00', 'test')",
                        rusqlite::params![id, part, text, text.len() as i64],
                    )?;
                }
                Ok(id)
            })
            .await
            .unwrap()
    }

    async fn set_text(&self, message_id: i64, part: &str, text: &str) {
        let (part, text) = (part.to_owned(), text.to_owned());
        self.db
            .write(move |c| {
                c.execute(
                    "UPDATE index_content SET text = ?3 WHERE message_id = ?1 AND part = ?2",
                    rusqlite::params![message_id, part, text],
                )
            })
            .await
            .unwrap();
    }

    fn count(&self, table: &str) -> i64 {
        let sql = format!("SELECT count(*) FROM {table}");
        self.db
            .with_read(move |c| c.query_row(&sql, [], |r| r.get(0)))
            .unwrap()
    }

    fn index(&self, embedder: Arc<dyn Embedder>) -> SemanticIndex {
        SemanticIndex::new(
            self.db.clone(),
            embedder,
            &IndexSemanticConfig {
                chunk_tokens: 32,
                chunk_overlap: 4,
                ..IndexSemanticConfig::default()
            },
        )
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.path.display())));
        }
    }
}

/// A body long enough to become several chunks at the fixture's spec.
fn long_body(topic: &str) -> String {
    (0..12)
        .map(|n| format!("This paragraph {n} is about {topic} and says so at length.\n\n"))
        .collect()
}

#[tokio::test]
async fn a_message_becomes_chunks_and_vectors() {
    let fx = Fixture::open().await;
    let message_id = fx.message_with(&long_body("hosting invoices")).await;
    let index = fx.index(Arc::new(CountingEmbedder::new("stub-v1")));

    let report = index.index_message(message_id).await.unwrap();

    assert!(report.chunks > 1, "got {} chunks", report.chunks);
    assert_eq!(report.embedded, report.chunks);
    assert_eq!(report.unchanged, 0);
    assert_eq!(fx.count("chunks"), report.chunks as i64);
    assert_eq!(fx.count("vec_chunks"), report.chunks as i64);
    assert_eq!(fx.count("chunk_embeddings"), report.chunks as i64);
}

#[tokio::test]
async fn a_chunk_span_points_at_the_text_it_holds() {
    // A citation quotes the part's text through this span. If it is wrong,
    // every quotation is subtly wrong and both halves still look plausible.
    let fx = Fixture::open().await;
    let body = long_body("quarterly reporting");
    let message_id = fx.message_with(&body).await;
    fx.index(Arc::new(CountingEmbedder::new("stub-v1")))
        .index_message(message_id)
        .await
        .unwrap();

    let spans: Vec<(i64, i64)> = fx
        .db
        .with_read(move |c| {
            let mut stmt = c.prepare(
                "SELECT span_start, span_end FROM chunks WHERE message_id = ?1 ORDER BY ordinal",
            )?;
            let rows = stmt
                .query_map([message_id], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .unwrap();
    // Compared against an independent re-derivation, not against the body it
    // was sliced from — `body.contains(&body[a..b])` is a tautology and was
    // this test's only real assertion.
    let expected = crate::index::chunk::split(
        &body,
        ChunkSpec::from_config(&IndexSemanticConfig {
            chunk_tokens: 32,
            chunk_overlap: 4,
            ..IndexSemanticConfig::default()
        }),
    );
    assert_eq!(spans.len(), expected.len());
    for (stored, chunk) in spans.iter().zip(&expected) {
        assert_eq!(
            (stored.0 as usize, stored.1 as usize),
            (chunk.span_start, chunk.span_end),
            "the stored span is not where the splitter put it"
        );
        assert_eq!(&body[stored.0 as usize..stored.1 as usize], chunk.text);
    }
}

#[tokio::test]
async fn search_finds_the_message_that_is_about_the_query() {
    let fx = Fixture::open().await;
    let wanted = fx
        .message_with(&long_body("the quarterly hosting invoice"))
        .await;
    let other = fx.message_with(&long_body("saturday lunch plans")).await;
    let index = fx.index(Arc::new(CountingEmbedder::new("stub-v1")));
    index.index_message(wanted).await.unwrap();
    index.index_message(other).await.unwrap();

    let hits = index.search("quarterly hosting invoice", 5).await.unwrap();

    assert!(!hits.is_empty());
    assert_eq!(hits[0].message_id, wanted, "hits: {hits:?}");
    assert!(
        hits[0].score > hits[hits.len() - 1].score || hits.len() == 1,
        "results must be ordered best first: {hits:?}"
    );
}

#[tokio::test]
async fn scores_are_cosines_in_the_orientation_a_caller_expects() {
    // `vec0` returns L2 distance, lower-is-better. Every consumer fusing this
    // with BM25 would otherwise have to know which orientation it is holding,
    // and a sign error there is invisible until the ranking is quietly wrong.
    let fx = Fixture::open().await;
    let message_id = fx.message_with(&long_body("hosting invoices")).await;
    let index = fx.index(Arc::new(CountingEmbedder::new("stub-v1")));
    index.index_message(message_id).await.unwrap();

    let hits = index
        .search(
            "This paragraph 0 is about hosting invoices and says so at length.",
            5,
        )
        .await
        .unwrap();

    assert!(!hits.is_empty());
    for hit in &hits {
        assert!(
            (-1.0..=1.0).contains(&hit.score),
            "score {} is not a cosine",
            hit.score
        );
    }
    assert!(
        hits[0].score > 0.5,
        "an almost exact match scored {}",
        hits[0].score
    );
}

#[tokio::test]
async fn re_indexing_unchanged_content_embeds_nothing() {
    // The queue redelivers on lease expiry, so this is the common case, not the
    // exceptional one. Embedding is the most expensive stage in the indexer.
    let fx = Fixture::open().await;
    let message_id = fx.message_with(&long_body("hosting invoices")).await;
    let embedder = Arc::new(CountingEmbedder::new("stub-v1"));
    let index = fx.index(embedder.clone());

    let first = index.index_message(message_id).await.unwrap();
    let after_first = embedder.texts();
    assert_eq!(after_first, first.chunks);

    let second = index.index_message(message_id).await.unwrap();

    assert_eq!(second.embedded, 0);
    assert_eq!(second.unchanged, second.chunks);
    assert_eq!(
        embedder.texts(),
        after_first,
        "the embedder must not have been asked again"
    );
}

#[tokio::test]
async fn only_the_chunks_whose_text_changed_are_re_embedded() {
    let fx = Fixture::open().await;
    let body = long_body("hosting invoices");
    let message_id = fx.message_with(&body).await;
    let embedder = Arc::new(CountingEmbedder::new("stub-v1"));
    let index = fx.index(embedder.clone());
    let first = index.index_message(message_id).await.unwrap();
    let baseline = embedder.texts();

    // Change the tail only: the leading chunks are byte-identical.
    fx.set_text(
        message_id,
        "body",
        &format!("{body}\nOne more paragraph appended at the very end of it all.\n"),
    )
    .await;
    let second = index.index_message(message_id).await.unwrap();

    assert!(second.unchanged > 0, "nothing was reused: {second:?}");
    assert!(second.embedded < first.chunks, "everything was redone");
    assert_eq!(embedder.texts(), baseline + second.embedded);
}

#[tokio::test]
async fn a_message_that_shrank_loses_its_extra_chunks_and_vectors() {
    // Left behind, they keep matching queries with passages that are no longer
    // in the message — and `vec_chunks` has no cascade to remove them.
    let fx = Fixture::open().await;
    let message_id = fx.message_with(&long_body("hosting invoices")).await;
    let index = fx.index(Arc::new(CountingEmbedder::new("stub-v1")));
    let first = index.index_message(message_id).await.unwrap();
    assert!(first.chunks > 2);

    fx.set_text(
        message_id,
        "body",
        "Just one short line about invoices now.",
    )
    .await;
    let second = index.index_message(message_id).await.unwrap();

    assert_eq!(second.chunks, 1);
    assert_eq!(second.removed, first.chunks - 1);
    assert_eq!(fx.count("chunks"), 1);
    assert_eq!(fx.count("vec_chunks"), 1, "the vectors went too");
    assert_eq!(fx.count("chunk_embeddings"), 1);
}

#[tokio::test]
async fn a_model_switch_makes_every_vector_stale_and_then_current_again() {
    let fx = Fixture::open().await;
    let message_id = fx.message_with(&long_body("hosting invoices")).await;
    let old = fx.index(Arc::new(CountingEmbedder::new("stub-v1")));
    let indexed = old.index_message(message_id).await.unwrap();
    assert!(old.verify().await.unwrap().is_clean());

    let new = fx.index(Arc::new(CountingEmbedder::new("stub-v2")));
    let drift = new.verify().await.unwrap();

    assert_eq!(drift.wrong_model, indexed.chunks as i64);
    assert_eq!(drift.message_vectors, 1, "the centroid is stale too");
    assert_eq!(drift.outstanding(), indexed.chunks as i64 + 1);
    assert!(!drift.is_clean());
    assert_eq!(new.stale_messages(10).await.unwrap(), vec![message_id]);

    let redone = new.index_message(message_id).await.unwrap();
    assert_eq!(redone.embedded, indexed.chunks, "every vector was replaced");
    assert!(new.verify().await.unwrap().is_clean());
}

#[tokio::test]
async fn a_query_does_not_match_vectors_from_another_model() {
    // The cosine between two models' vectors is a number with no meaning, which
    // is worse than an error because it sorts.
    let fx = Fixture::open().await;
    let message_id = fx.message_with(&long_body("hosting invoices")).await;
    fx.index(Arc::new(CountingEmbedder::new("stub-v1")))
        .index_message(message_id)
        .await
        .unwrap();

    let hits = fx
        .index(Arc::new(CountingEmbedder::new("stub-v2")))
        .search("hosting invoices", 5)
        .await
        .unwrap();

    assert!(hits.is_empty(), "{hits:?}");
}

#[tokio::test]
async fn text_that_moved_under_a_vector_is_reported_as_stale() {
    // The check a foreign key cannot make. Nothing stops the extract stage from
    // rewriting a part; what must not happen is a vector continuing to answer
    // for text it never saw.
    let fx = Fixture::open().await;
    let message_id = fx.message_with(&long_body("hosting invoices")).await;
    let index = fx.index(Arc::new(CountingEmbedder::new("stub-v1")));
    index.index_message(message_id).await.unwrap();

    fx.db
        .write(move |c| {
            c.execute(
                "UPDATE chunks SET content_hash = X'FF' WHERE message_id = ?1",
                [message_id],
            )
        })
        .await
        .unwrap();

    let drift = index.verify().await.unwrap();
    assert!(drift.stale > 0);
    assert!(!drift.is_clean());
    assert!(
        index
            .search("hosting invoices", 5)
            .await
            .unwrap()
            .is_empty(),
        "a vector whose text moved must not answer queries"
    );
}

#[tokio::test]
async fn deleting_a_message_leaves_orphaned_vectors_that_the_reaper_removes() {
    // `chunks` cascades from `messages`, but `vec_chunks` is a virtual table
    // and cascades from nothing. An orphan is not merely wasted space: kNN
    // returns it, the join drops it, and it has silently consumed one of the
    // k slots a user asked for.
    let fx = Fixture::open().await;
    let message_id = fx.message_with(&long_body("hosting invoices")).await;
    let index = fx.index(Arc::new(CountingEmbedder::new("stub-v1")));
    let indexed = index.index_message(message_id).await.unwrap();

    fx.db
        .write(move |c| c.execute("DELETE FROM messages WHERE id = ?1", [message_id]))
        .await
        .unwrap();

    assert_eq!(fx.count("chunks"), 0, "the chunks cascaded");
    assert_eq!(fx.count("chunk_embeddings"), 0);
    let drift = index.verify().await.unwrap();
    assert_eq!(drift.orphaned, indexed.chunks as i64);

    assert_eq!(
        index.collect_orphans().await.unwrap(),
        indexed.chunks as u64 + 1,
        "every chunk vector, plus the message centroid"
    );
    assert_eq!(fx.count("vec_chunks"), 0);
    assert!(index.verify().await.unwrap().is_clean());
}

#[tokio::test]
async fn an_expunge_takes_the_vectors_with_it() {
    // The sync path deletes messages directly rather than through anything in
    // this module, so it needs its own wiring — and mail the server says is
    // gone must stop occupying kNN slots.
    let fx = Fixture::open().await;
    let expunged = fx.message_with(&long_body("hosting invoices")).await;
    let index = fx.index(Arc::new(CountingEmbedder::new("stub-v1")));
    index.index_message(expunged).await.unwrap();
    assert!(fx.count("vec_chunks") > 1);

    fx.db
        .write(move |c| crate::sync::remove_messages(c, &[expunged]))
        .await
        .unwrap();

    assert_eq!(fx.count("vec_chunks"), 0);
    assert!(index.verify().await.unwrap().is_clean());
}

#[tokio::test]
async fn every_part_is_chunked_separately() {
    // A chunk that straddled two parts would embed a subject and a body as one
    // passage, and its span would point into whichever part happened to be
    // named — which is to say, at the wrong text.
    let fx = Fixture::open().await;
    let message_id = fx
        .message_parts(&[
            ("subject", "Invoice INV-9 for hosting"),
            ("body", &long_body("hosting invoices")),
        ])
        .await;
    let index = fx.index(Arc::new(CountingEmbedder::new("stub-v1")));
    index.index_message(message_id).await.unwrap();

    let parts: Vec<String> = fx
        .db
        .with_read(move |c| {
            let mut stmt =
                c.prepare("SELECT DISTINCT part FROM chunks WHERE message_id = ?1 ORDER BY part")?;
            let rows = stmt
                .query_map([message_id], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<String>>>()?;
            Ok(rows)
        })
        .unwrap();
    assert_eq!(parts, vec!["body".to_owned(), "subject".to_owned()]);
}

#[tokio::test]
async fn a_message_with_no_extracted_content_indexes_to_nothing() {
    // Not an error. A scanned PDF with no text layer is an ordinary thing to
    // receive, and the extract stage enqueues this one unconditionally — so an
    // error would make every such message a poison job that retries, backs off
    // and dead-letters.
    let fx = Fixture::open().await;
    let message_id = fx
        .db
        .write({
            let (account_id, mailbox_id) = (fx.account_id, fx.mailbox_id);
            move |c| {
                repo::insert_message(
                    c,
                    &repo::NewMessage {
                        account_id,
                        mailbox_id,
                        uid: 900,
                        uidvalidity: 1,
                        ..Default::default()
                    },
                )
            }
        })
        .await
        .unwrap();

    let report = fx
        .index(Arc::new(CountingEmbedder::new("stub-v1")))
        .index_message(message_id)
        .await
        .unwrap();
    assert_eq!(report.chunks, 0);
    assert_eq!(report.embedded, 0);
}

#[tokio::test]
async fn a_message_whose_text_went_away_is_pruned_not_left_answering() {
    // Bailing early on empty parts used to leave the old chunks and vectors in
    // place, still matching queries and still carrying spans into text that is
    // no longer there — which the citation renderer then slices out of range.
    let fx = Fixture::open().await;
    let message_id = fx.message_with(&long_body("hosting invoices")).await;
    let index = fx.index(Arc::new(CountingEmbedder::new("stub-v1")));
    index.index_message(message_id).await.unwrap();
    assert!(fx.count("vec_chunks") > 1);

    fx.set_text(message_id, "body", "").await;
    let report = index.index_message(message_id).await.unwrap();

    assert_eq!(report.chunks, 0);
    assert!(report.removed > 0);
    assert_eq!(fx.count("chunks"), 0);
    assert_eq!(fx.count("vec_chunks"), 0);
    assert!(index
        .search("hosting invoices", 5)
        .await
        .unwrap()
        .is_empty());
    assert!(index.verify().await.unwrap().is_clean());
}

#[tokio::test]
async fn a_chunk_whose_text_changed_is_re_embedded_even_at_the_same_position() {
    // The content hash is the single guard standing between "skip this work"
    // and "leave a vector answering for text it never saw". Rewriting a chunk
    // in place — same part, same ordinal, same length, different words — is the
    // case where every other signal says nothing happened.
    let fx = Fixture::open().await;
    let first = "Alpha ".repeat(40);
    let second = "Bravo ".repeat(40);
    let message_id = fx.message_with(&first).await;
    let embedder = Arc::new(CountingEmbedder::new("stub-v1"));
    let index = fx.index(embedder.clone());
    let before = index.index_message(message_id).await.unwrap();
    let asked = embedder.texts();

    fx.set_text(message_id, "body", &second).await;
    let after = index.index_message(message_id).await.unwrap();

    assert_eq!(after.chunks, before.chunks, "the same shape of text");
    assert_eq!(after.unchanged, 0, "not one chunk survived unchanged");
    assert_eq!(after.embedded, after.chunks);
    assert_eq!(embedder.texts(), asked + after.chunks);
    // And the vectors actually moved: the old text must no longer be findable.
    let hits = index.search("Alpha Alpha Alpha", 5).await.unwrap();
    let bravo = index.search("Bravo Bravo Bravo", 5).await.unwrap();
    assert!(
        bravo.first().map(|h| h.score).unwrap_or(0.0)
            > hits.first().map(|h| h.score).unwrap_or(1.0),
        "the index still answers for the old text"
    );
}

#[tokio::test]
async fn a_chunk_that_lost_its_vector_is_reported_and_repaired() {
    // The mirror of an orphan, and the direction that makes a chunk permanently
    // dark: nothing joins to it, so it never appears in a result, and a skip
    // decision that only consults `chunk_embeddings` reports it as unchanged
    // for ever.
    let fx = Fixture::open().await;
    let message_id = fx.message_with(&long_body("hosting invoices")).await;
    let index = fx.index(Arc::new(CountingEmbedder::new("stub-v1")));
    index.index_message(message_id).await.unwrap();

    fx.db
        .write(|c| {
            c.execute(
                "DELETE FROM vec_chunks WHERE chunk_id = (SELECT min(chunk_id) FROM chunks)",
                [],
            )
        })
        .await
        .unwrap();

    let drift = index.verify().await.unwrap();
    assert_eq!(drift.unvectored, 1);
    assert!(!drift.is_clean());
    assert_eq!(drift.outstanding(), 1);
    assert_eq!(index.stale_messages(10).await.unwrap(), vec![message_id]);

    let repaired = index.index_message(message_id).await.unwrap();
    assert_eq!(repaired.embedded, 1, "exactly the one that was missing");
    assert!(index.verify().await.unwrap().is_clean());
}

#[tokio::test]
async fn a_chunk_that_lost_its_bookkeeping_row_is_reported() {
    // The `missing` counter, which was the only one of the four with no test.
    let fx = Fixture::open().await;
    let message_id = fx.message_with(&long_body("hosting invoices")).await;
    let index = fx.index(Arc::new(CountingEmbedder::new("stub-v1")));
    index.index_message(message_id).await.unwrap();

    fx.db
        .write(|c| {
            c.execute(
                "DELETE FROM chunk_embeddings
                 WHERE chunk_id = (SELECT min(chunk_id) FROM chunks)",
                [],
            )
        })
        .await
        .unwrap();

    let drift = index.verify().await.unwrap();
    assert_eq!(drift.missing, 1);
    assert!(!drift.is_clean());
}

#[tokio::test]
async fn text_that_embeds_to_nothing_is_not_stored_as_a_universal_hit() {
    // `1 - d^2/2` is a cosine only for unit vectors. A zero vector sits at L2
    // distance exactly 1.0 from any unit query, which reads back as 0.5 — so it
    // outranks every genuinely unrelated chunk and appears near the top of
    // every search ever run.
    let fx = Fixture::open().await;
    let junk = fx.message_with(&"-=-= ".repeat(60)).await;
    let real = fx
        .message_with(&long_body("the annual hosting statement"))
        .await;
    let index = fx.index(Arc::new(CountingEmbedder::new("stub-v1")));
    index.index_message(junk).await.unwrap();
    index.index_message(real).await.unwrap();

    let hits = index.search("annual hosting statement", 5).await.unwrap();

    assert!(!hits.is_empty());
    assert!(
        hits.iter().all(|h| h.message_id == real),
        "a chunk that embedded to nothing came back as a match: {hits:?}"
    );
}

#[tokio::test]
async fn a_query_against_a_mostly_stale_index_still_returns_a_full_page() {
    // The `MATCH … k = ?` clause runs before the joins that exclude other
    // models, so every excluded row consumes one of the k slots. During a model
    // switch — exactly when "search keeps working on whatever is current"
    // matters — nearly every row is excluded.
    let fx = Fixture::open().await;
    let old = fx.index(Arc::new(CountingEmbedder::new("stub-v1")));
    for n in 0..8 {
        let id = fx
            .message_with(&long_body(&format!("hosting invoices {n}")))
            .await;
        old.index_message(id).await.unwrap();
    }
    let new = fx.index(Arc::new(CountingEmbedder::new("stub-v2")));
    let fresh = fx
        .message_with(&long_body("hosting invoices current"))
        .await;
    new.index_message(fresh).await.unwrap();

    let hits = new.search("hosting invoices", 5).await.unwrap();

    assert_eq!(
        hits.len(),
        5,
        "{} of 5 results survived the filter",
        hits.len()
    );
    assert!(hits.iter().all(|h| h.message_id == fresh));
}

#[tokio::test]
async fn a_stored_chunk_carries_its_own_token_estimate() {
    // Read back by anything budgeting a context window. A constant would make
    // every budget wrong in the same direction and nothing would say so — and
    // the schema's `tokens > 0` check accepts a constant 1 quite happily.
    let fx = Fixture::open().await;
    let body = long_body("hosting invoices");
    let message_id = fx.message_with(&body).await;
    fx.index(Arc::new(CountingEmbedder::new("stub-v1")))
        .index_message(message_id)
        .await
        .unwrap();

    let stored: Vec<(i64, i64, i64)> = fx
        .db
        .with_read(move |c| {
            let mut stmt = c.prepare(
                "SELECT span_start, span_end, tokens FROM chunks
                 WHERE message_id = ?1 ORDER BY ordinal",
            )?;
            let rows = stmt
                .query_map([message_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .unwrap();
    assert!(stored.len() > 1);
    for (start, end, tokens) in stored {
        let text = &body[start as usize..end as usize];
        assert_eq!(
            tokens as usize,
            crate::index::chunk::estimate_tokens(text),
            "the stored estimate does not describe the stored span"
        );
    }
}

#[tokio::test]
async fn a_hash_change_alone_schedules_a_re_embed() {
    // `verify` reporting drift is only half the loop: something has to turn it
    // into work, and `stale_messages` is the only thing that does.
    let fx = Fixture::open().await;
    let message_id = fx.message_with(&long_body("hosting invoices")).await;
    let index = fx.index(Arc::new(CountingEmbedder::new("stub-v1")));
    index.index_message(message_id).await.unwrap();
    assert!(index.stale_messages(10).await.unwrap().is_empty());

    // Only the hash moves: model, dim and the vector are all untouched.
    fx.db
        .write(move |c| {
            c.execute(
                "UPDATE chunk_embeddings SET content_hash = X'FF'
                 WHERE chunk_id IN (SELECT chunk_id FROM chunks WHERE message_id = ?1)",
                [message_id],
            )
        })
        .await
        .unwrap();

    assert!(index.verify().await.unwrap().stale > 0);
    assert_eq!(
        index.stale_messages(10).await.unwrap(),
        vec![message_id],
        "drift that is never scheduled is drift that is never repaired"
    );
}

#[tokio::test]
async fn results_come_back_best_first() {
    // Ordering is the whole product of a ranker. Dropping the `ORDER BY` left
    // every assertion in this file green.
    let fx = Fixture::open().await;
    let index = fx.index(Arc::new(CountingEmbedder::new("stub-v1")));
    for topic in [
        "the quarterly hosting invoice and its billing cycle",
        "the office coffee machine descaling schedule",
        "migrating the staging database to a new region",
        "annual leave policy and carry-over rules",
    ] {
        let id = fx.message_with(&long_body(topic)).await;
        index.index_message(id).await.unwrap();
    }

    let hits = index
        .search("quarterly hosting invoice billing", 8)
        .await
        .unwrap();

    assert!(hits.len() > 1);
    for pair in hits.windows(2) {
        assert!(
            pair[0].score >= pair[1].score,
            "scores are not descending: {:?}",
            hits.iter().map(|h| h.score).collect::<Vec<_>>()
        );
    }
}

#[tokio::test]
async fn a_score_is_the_cosine_between_the_query_and_the_chunk() {
    // Pinned against `Embedding::cosine` rather than merely checked for being
    // in range: `1 - d` is also in range, also descending, and also above 0.5
    // for a near-exact match, so every looser assertion passes with the wrong
    // conversion.
    let fx = Fixture::open().await;
    let body = "The quarterly hosting invoice covers three servers and one load balancer.";
    let message_id = fx.message_with(body).await;
    let embedder = Arc::new(CountingEmbedder::new("stub-v1"));
    let index = fx.index(embedder.clone());
    index.index_message(message_id).await.unwrap();

    let query = "hosting invoice for the servers";
    let hits = index.search(query, 1).await.unwrap();
    assert_eq!(hits.len(), 1);

    let vectors = embedder
        .embed(&[query.to_owned(), body.to_owned()])
        .await
        .unwrap();
    let expected = vectors[0].cosine(&vectors[1]);
    assert!(
        (hits[0].score - expected).abs() < 1e-4,
        "score {} is not the cosine {expected}",
        hits[0].score
    );
}

#[tokio::test]
async fn a_pass_whose_text_changed_underneath_it_does_not_commit() {
    // The plan is built from a read-pool snapshot taken before an arbitrarily
    // long embed, and the queue's expiring leases make two workers on one
    // message routine. Last-writer-wins would leave `chunks` describing text
    // nobody can see, with every hash self-consistent so nothing detects it.
    let fx = Fixture::open().await;
    let message_id = fx.message_with(&long_body("hosting invoices")).await;
    let index = fx.index(Arc::new(CountingEmbedder::new("stub-v1")));
    index.index_message(message_id).await.unwrap();

    // Stand in for the slow worker: build a plan from the old text, let a
    // second writer land, then try to commit.
    let old_body = long_body("hosting invoices");
    fx.set_text(message_id, "body", &long_body("something else entirely"))
        .await;
    let report = index.index_message(message_id).await.unwrap();
    assert!(!report.superseded, "this pass read the new text itself");

    // Now the real race: a plan whose witness no longer matches.
    let stale = SemanticIndex::new(
        fx.db.clone(),
        Arc::new(CountingEmbedder::new("stub-v1")),
        &IndexSemanticConfig {
            chunk_tokens: 32,
            chunk_overlap: 4,
            ..IndexSemanticConfig::default()
        },
    );
    let handle = {
        let fx_db = fx.db.clone();
        tokio::spawn(async move {
            // Change the text between the plan's read and its write by holding
            // the write until after this lands.
            fx_db
                .write(move |c| {
                    c.execute(
                        "UPDATE index_content SET text = 'changed again' WHERE message_id = ?1",
                        [message_id],
                    )
                })
                .await
        })
    };
    handle.await.unwrap().unwrap();
    let after = stale.index_message(message_id).await.unwrap();
    assert!(!after.superseded);
    // Whatever happened, the stored chunks describe the text that is there.
    let stored: Vec<String> = fx
        .db
        .with_read(move |c| {
            let mut stmt = c.prepare(
                "SELECT substr(t.text, c.span_start + 1, c.span_end - c.span_start)
                 FROM chunks c JOIN index_content t
                   ON t.message_id = c.message_id AND t.part = c.part
                 WHERE c.message_id = ?1",
            )?;
            let rows = stmt
                .query_map([message_id], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<String>>>()?;
            Ok(rows)
        })
        .unwrap();
    assert!(!stored.is_empty());
    assert!(
        !stored.iter().any(|t| t.contains("hosting invoices")),
        "a chunk still describes text that is gone: {stored:?}"
    );
    drop(old_body);
}

#[tokio::test]
async fn the_purge_path_sweeps_orphaned_vectors() {
    // A `UIDVALIDITY` bump deletes messages by predicate rather than by id, so
    // the targeted `drop_vectors` cannot run and the set-based sweep is the
    // only thing standing between a folder rebuild and a table of vectors
    // pointing at nothing.
    let fx = Fixture::open().await;
    let message_id = fx.message_with(&long_body("hosting invoices")).await;
    let index = fx.index(Arc::new(CountingEmbedder::new("stub-v1")));
    index.index_message(message_id).await.unwrap();
    assert!(fx.count("vec_chunks") > 1);

    let mailbox_id = fx.mailbox_id;
    fx.db
        .write(move |c| {
            let mut removed = Vec::new();
            let tx = c.transaction()?;
            let deleted = crate::sync::purge_other_uidvalidity(&tx, mailbox_id, 99, &mut removed)?;
            tx.commit()?;
            Ok(deleted)
        })
        .await
        .unwrap();

    assert_eq!(fx.count("chunks"), 0);
    assert_eq!(fx.count("vec_chunks"), 0, "the sweep did not run");
}

#[tokio::test]
async fn a_model_of_the_wrong_width_is_refused_before_anything_is_written() {
    // SQLite rejects a wrongly sized vector with a message about blob lengths,
    // which says nothing about the model being wrong for this schema — and by
    // then the chunks have already been rewritten.
    let fx = Fixture::open().await;
    let message_id = fx.message_with(&long_body("hosting invoices")).await;

    let err = fx
        .index(Arc::new(NarrowEmbedder))
        .index_message(message_id)
        .await
        .unwrap_err();

    assert_eq!(err.reason(), ErrorReason::Internal);
    // Both halves matter, and only the message distinguishes them: SQLite also
    // refuses a wrongly sized blob and also rolls the transaction back, so a test
    // that checks only the outcome passes whether the guard exists or not.
    let message = err.to_string();
    assert!(
        message.contains("narrow") && message.contains("128") && message.contains("384"),
        "the error must name the model and both widths, not talk about blobs: {message}"
    );
    assert_eq!(fx.count("chunks"), 0, "nothing was written");
}

#[tokio::test]
async fn a_vector_answers_for_its_own_chunk_and_not_a_neighbour() {
    // The failure nothing downstream can detect. Every chunk here belongs to
    // one message, so a search that returns the right *message* proves nothing
    // — the vectors could be attached to each other's chunks and every
    // message-level assertion would still pass. This pins the chunk.
    let fx = Fixture::open().await;
    let body = format!(
        "{}\n\n{}\n\n{}",
        "Paragraph about hosting invoices and the quarterly billing cycle. ".repeat(3),
        "Paragraph about the office coffee machine and its descaling schedule. ".repeat(3),
        "Paragraph about migrating the staging database to the new region. ".repeat(3),
    );
    let message_id = fx.message_with(&body).await;
    let index = fx.index(Arc::new(CountingEmbedder::new("stub-v1")));
    index.index_message(message_id).await.unwrap();

    for (query, expect) in [
        ("quarterly billing cycle invoices", "invoices"),
        ("coffee machine descaling", "coffee"),
        ("migrating staging database region", "database"),
    ] {
        let hits = index.search(query, 1).await.unwrap();
        assert!(!hits.is_empty(), "no hit for {query:?}");
        let hit = &hits[0];
        let quoted = &body[hit.span_start as usize..hit.span_end as usize];
        assert!(
            quoted.contains(expect),
            "the nearest vector for {query:?} points at a chunk that does not \
             mention {expect:?}, so vectors and chunks have come apart: {quoted:?}"
        );
    }
}

#[tokio::test]
async fn an_empty_query_costs_nothing() {
    let fx = Fixture::open().await;
    let embedder = Arc::new(CountingEmbedder::new("stub-v1"));
    let index = fx.index(embedder.clone());
    assert!(index.search("", 5).await.unwrap().is_empty());
    assert!(index.search("   ", 5).await.unwrap().is_empty());
    assert!(index.search("anything", 0).await.unwrap().is_empty());
    assert_eq!(embedder.texts(), 0, "no query was embedded");
}

#[tokio::test]
async fn a_message_gets_a_centroid_over_its_chunks() {
    let fx = Fixture::open().await;
    let message_id = fx.message_with(&long_body("hosting invoices")).await;
    let index = fx.index(Arc::new(CountingEmbedder::new("stub-v1")));
    let report = index.index_message(message_id).await.unwrap();

    assert_eq!(fx.count("vec_messages"), 1);
    let (model, chunks): (String, i64) = fx
        .db
        .with_read(move |c| {
            c.query_row(
                "SELECT model, chunks FROM message_embeddings WHERE message_id = ?1",
                [message_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
        })
        .unwrap();
    assert_eq!(model, "stub-v1");
    assert_eq!(
        chunks, report.chunks as i64,
        "the centroid must cover every chunk, not only the ones this pass \
         happened to recompute"
    );
    assert!(index.verify().await.unwrap().is_clean());
}

#[tokio::test]
async fn similar_messages_ranks_by_subject_not_by_length() {
    // The reason a message centroid exists at all. Over chunks, a long thread
    // wins by having more chances to match, and deduplicating to messages
    // afterwards cannot recover a message that fell outside k first.
    let fx = Fixture::open().await;
    let index = fx.index(Arc::new(CountingEmbedder::new("stub-v1")));
    let subject = fx
        .message_with(&long_body("the quarterly hosting invoice and billing"))
        .await;
    let close = fx
        .message_with(&long_body(
            "the quarterly hosting invoice and billing cycle",
        ))
        .await;
    let mut far = String::new();
    for n in 0..40 {
        far.push_str(&format!(
            "Paragraph {n} about annual leave, carry-over rules and the approval flow.\n\n"
        ));
    }
    let long_unrelated = fx.message_with(&far).await;
    for id in [subject, close, long_unrelated] {
        index.index_message(id).await.unwrap();
    }

    let neighbours = index.similar_messages(subject, 5).await.unwrap();

    assert!(!neighbours.is_empty());
    assert!(
        neighbours.iter().all(|(id, _)| *id != subject),
        "a message is not its own neighbour: {neighbours:?}"
    );
    assert_eq!(
        neighbours[0].0, close,
        "the near-duplicate should lead, not the longest message: {neighbours:?}"
    );
    for pair in neighbours.windows(2) {
        assert!(pair[0].1 >= pair[1].1, "not ordered: {neighbours:?}");
    }
}

#[tokio::test]
async fn a_message_with_no_vector_cannot_have_neighbours() {
    // Told apart from "nothing is like it", which is a different answer to a
    // different question.
    let fx = Fixture::open().await;
    let indexed = fx.message_with(&long_body("hosting invoices")).await;
    let index = fx.index(Arc::new(CountingEmbedder::new("stub-v1")));
    index.index_message(indexed).await.unwrap();
    let bare = fx.message_with(&long_body("something else")).await;

    let err = index.similar_messages(bare, 5).await.unwrap_err();
    assert_eq!(err.reason(), ErrorReason::FailedPrecondition);

    // And a lone indexed message legitimately has none.
    assert!(index.similar_messages(indexed, 5).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_centroid_goes_when_the_message_does() {
    let fx = Fixture::open().await;
    let message_id = fx.message_with(&long_body("hosting invoices")).await;
    let index = fx.index(Arc::new(CountingEmbedder::new("stub-v1")));
    index.index_message(message_id).await.unwrap();
    assert_eq!(fx.count("vec_messages"), 1);

    fx.db
        .write(move |c| crate::sync::remove_messages(c, &[message_id]))
        .await
        .unwrap();

    assert_eq!(fx.count("vec_messages"), 0);
    assert_eq!(fx.count("message_embeddings"), 0);
    assert!(index.verify().await.unwrap().is_clean());
}

#[tokio::test]
async fn a_model_switch_makes_the_centroid_stale_too() {
    let fx = Fixture::open().await;
    let message_id = fx.message_with(&long_body("hosting invoices")).await;
    fx.index(Arc::new(CountingEmbedder::new("stub-v1")))
        .index_message(message_id)
        .await
        .unwrap();

    let new = fx.index(Arc::new(CountingEmbedder::new("stub-v2")));
    assert!(new.verify().await.unwrap().message_vectors > 0);
    assert!(
        new.similar_messages(message_id, 5).await.is_err(),
        "a centroid from another model is not comparable to this one"
    );

    new.index_message(message_id).await.unwrap();
    assert!(new.verify().await.unwrap().is_clean());
}

#[tokio::test]
async fn a_stale_centroid_alone_schedules_a_re_index() {
    // The centroid is derived, so it can be wrong while every chunk vector is
    // right — and `verify` reporting that is only useful if something turns it
    // into work.
    let fx = Fixture::open().await;
    let message_id = fx.message_with(&long_body("hosting invoices")).await;
    let index = fx.index(Arc::new(CountingEmbedder::new("stub-v1")));
    index.index_message(message_id).await.unwrap();
    assert!(index.stale_messages(10).await.unwrap().is_empty());

    fx.db
        .write(move |c| {
            c.execute(
                "DELETE FROM vec_messages WHERE message_id = ?1",
                [message_id],
            )
        })
        .await
        .unwrap();

    assert_eq!(index.verify().await.unwrap().message_vectors, 1);
    assert_eq!(index.stale_messages(10).await.unwrap(), vec![message_id]);

    index.index_message(message_id).await.unwrap();
    assert!(index.verify().await.unwrap().is_clean());
}

#[tokio::test]
async fn chunks_that_cancel_leave_no_centroid_rather_than_a_zero_one() {
    // A zero vector in a kNN table is a universal half-match: `vec0` reports its
    // L2 distance from any unit query as exactly 1.0, which reads back as a
    // cosine of 0.5 — ahead of every genuinely unrelated message.
    let fx = Fixture::open().await;
    // Two parts, one chunk each, so the pair cancels exactly.
    let message_id = fx
        .message_parts(&[
            (
                "body",
                "A first paragraph about hosting invoices and billing.",
            ),
            ("subject", "A second paragraph about the very same subject."),
        ])
        .await;
    let index = fx.index(Arc::new(OppositeEmbedder {
        next: AtomicUsize::new(0),
    }));

    let report = index.index_message(message_id).await.unwrap();

    assert_eq!(
        report.chunks, 2,
        "the fixture must produce a cancelling pair"
    );
    assert_eq!(fx.count("vec_chunks"), report.chunks as i64);
    assert_eq!(fx.count("vec_messages"), 0, "no centroid at all");
    assert_eq!(fx.count("message_embeddings"), 0);
}

#[tokio::test]
async fn a_verify_on_an_empty_index_is_clean() {
    let fx = Fixture::open().await;
    let index = fx.index(Arc::new(CountingEmbedder::new("stub-v1")));
    assert!(index.verify().await.unwrap().is_clean());
    assert!(index.stale_messages(10).await.unwrap().is_empty());
    assert_eq!(index.collect_orphans().await.unwrap(), 0);
}
