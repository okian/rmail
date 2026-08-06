//! What the dense retriever owes a caller: the message whose chunks are
//! actually near the query vector ranks first, `score` is the max chunk
//! similarity while `mean_score` carries the mean alongside it, no query
//! vector degrades to no candidates rather than an error, and the
//! hard-filter mask and cancellation behave like every other retriever in
//! this task.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use tokio_util::sync::CancellationToken;

use super::*;
use crate::config::{ExpansionConfig, IndexSemanticConfig};
use crate::embed::hash::HashEmbedder;
use crate::query::QueryPlanner;
use crate::repo;

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Fixture {
    db: Database,
    semantic: SemanticIndex,
    planner: QueryPlanner,
    account_id: i64,
    mailbox_id: i64,
    next_uid: std::cell::Cell<i64>,
    path: PathBuf,
}

impl Fixture {
    async fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-retrieve-dense-{pid}-{n}.db"));
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
        // The *same* embedder instance backs both the index and the query
        // planner, exactly as a real deployment would (one configured
        // embedder): comparing vectors from two different embedders is
        // comparing noise (`index::semantic`'s own "a model change is a
        // targeted rebuild" doc).
        let embedder = std::sync::Arc::new(HashEmbedder::new(VECTOR_DIM));
        let semantic = SemanticIndex::new(
            db.clone(),
            embedder.clone(),
            &IndexSemanticConfig {
                chunk_tokens: 32,
                chunk_overlap: 4,
                ..IndexSemanticConfig::default()
            },
        );
        let planner = QueryPlanner::new(db.clone(), embedder, ExpansionConfig::default());
        Self {
            semantic,
            planner,
            db,
            account_id,
            mailbox_id,
            next_uid: std::cell::Cell::new(1),
            path,
        }
    }

    /// Insert a message, give it `body` as its only extracted part, and embed
    /// it into the semantic index.
    async fn message_with(&self, body: &str, from_addr: Option<&str>) -> i64 {
        let uid = self.next_uid.get();
        self.next_uid.set(uid + 1);
        let (account_id, mailbox_id) = (self.account_id, self.mailbox_id);
        let body = body.to_owned();
        let from_addr = from_addr.map(str::to_owned);
        let message_id = self
            .db
            .write(move |c| {
                let id = repo::insert_message(
                    c,
                    &repo::NewMessage {
                        account_id,
                        mailbox_id,
                        uid,
                        uidvalidity: 1,
                        from_addr,
                        ..Default::default()
                    },
                )?;
                c.execute(
                    "INSERT INTO index_content
                         (message_id, part, text, chars, content_hash, extractor)
                     VALUES (?1, 'body', ?2, ?3, X'00', 'test')",
                    rusqlite::params![id, body, body.len() as i64],
                )?;
                Ok(id)
            })
            .await
            .unwrap();
        self.semantic.index_message(message_id).await.unwrap();
        message_id
    }

    async fn plan(&self, raw: &str) -> QueryPlan {
        self.planner.plan(raw).await.unwrap()
    }

    fn retriever(&self) -> DenseRetriever {
        DenseRetriever::new(self.db.clone(), &self.semantic)
    }

    fn no_cancel() -> CancellationToken {
        CancellationToken::new()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.path.display())));
        }
    }
}

#[tokio::test]
async fn the_nearest_message_ranks_first() {
    let fx = Fixture::open().await;
    let relevant = fx
        .message_with(
            "the quarterly budget report covers spending across every team",
            None,
        )
        .await;
    fx.message_with("centrifuge maintenance schedule for the lab", None)
        .await;

    let plan = fx.plan("quarterly budget report").await;
    assert!(plan.query_vector.is_some());

    let hits = fx
        .retriever()
        .retrieve(&plan, 100, &Fixture::no_cancel())
        .await
        .unwrap();
    assert!(!hits.is_empty());
    assert_eq!(hits[0].message_id, relevant);
    assert_eq!(hits[0].source, Source::Dense);
    assert_eq!(hits[0].rank, 1);
}

#[tokio::test]
async fn score_is_max_and_differs_from_the_mean_across_chunks() {
    let fx = Fixture::open().await;
    // Two chunks' worth of very different vocabulary (fixture spec is
    // `chunk_tokens: 32`), so the message's chunks have genuinely different
    // similarity to the query: a single-chunk body would make max and mean
    // identical regardless of whether the implementation actually computes
    // both, which is exactly the gap this test exists to close.
    let matching = "alpha beta gamma ".repeat(15);
    let unrelated =
        "zzzyx penguino toastera umbrellon galaxen violinum cactusar marbleth ".repeat(8);
    let body = format!("{matching}{unrelated}");
    fx.message_with(&body, None).await;

    let plan = fx.plan("alpha beta gamma").await;
    let hits = fx
        .retriever()
        .retrieve(&plan, 100, &Fixture::no_cancel())
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    let mean = hits[0]
        .mean_score
        .expect("dense candidates always carry a mean score");
    assert!(
        hits[0].score > mean,
        "the query-matching chunk's similarity ({}) should exceed the mean across all of the \
         message's chunks ({mean}) once there is more than one chunk with different content",
        hits[0].score
    );
}

#[tokio::test]
async fn no_query_vector_degrades_to_no_candidates() {
    let fx = Fixture::open().await;
    fx.message_with("anything at all", None).await;

    // An empty query has no free text to embed at all — `QueryPlanner`
    // itself never calls the embedder, and `query_vector` is `None`,
    // covering the same branch a broken/unconfigured embedder would.
    let plan = fx.plan("").await;
    assert!(plan.query_vector.is_none());

    let hits = fx
        .retriever()
        .retrieve(&plan, 100, &Fixture::no_cancel())
        .await
        .unwrap();
    assert!(hits.is_empty());
}

#[tokio::test]
async fn the_hard_filter_mask_still_applies() {
    let fx = Fixture::open().await;
    let from_alice = fx
        .message_with("budget report notes", Some("alice@example.com"))
        .await;
    fx.message_with("budget report notes", Some("bob@example.com"))
        .await;

    let plan = fx.plan("budget report from:alice").await;
    let hits = fx
        .retriever()
        .retrieve(&plan, 100, &Fixture::no_cancel())
        .await
        .unwrap();
    assert_eq!(
        hits.into_iter().map(|c| c.message_id).collect::<Vec<_>>(),
        vec![from_alice]
    );
}

#[tokio::test]
async fn a_filter_that_excludes_everything_returns_nothing() {
    let fx = Fixture::open().await;
    fx.message_with("budget report notes", None).await;

    let plan = fx.plan("budget report tag:work").await;
    let hits = fx
        .retriever()
        .retrieve(&plan, 100, &Fixture::no_cancel())
        .await
        .unwrap();
    assert!(hits.is_empty());
}

#[tokio::test]
async fn a_cancelled_token_degrades_to_no_candidates_without_erroring() {
    let fx = Fixture::open().await;
    fx.message_with("budget report notes", None).await;

    let plan = fx.plan("budget report").await;
    let cancel = CancellationToken::new();
    cancel.cancel();
    let hits = fx.retriever().retrieve(&plan, 100, &cancel).await.unwrap();
    assert!(hits.is_empty());
}
