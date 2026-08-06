//! What the prefix retriever owes a caller: a partial word matches an
//! indexed complete term, a spell-corrected or synonym term never
//! participates (only what the user actually typed), the hard-filter mask
//! still applies, and injection safety matches `retrieve::lexical`'s own.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use tokio_util::sync::CancellationToken;

use super::*;
use crate::config::Bm25Weights;
use crate::index::extract_message;
use crate::index::{IndexQueue, QueueOptions, PRIORITY_NORMAL};
use crate::query::{Intent, PlanTerm, QueryPlanner, Scope, SortSpec};
use crate::ErrorReason;
use crate::{config::ExpansionConfig, embed::hash::HashEmbedder, repo};

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Fixture {
    db: Database,
    fts: FtsIndex,
    queue: IndexQueue,
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
        let path = std::env::temp_dir().join(format!("rmail-retrieve-prefix-{pid}-{n}.db"));
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
        let fts = FtsIndex::new(db.clone(), Bm25Weights::default());
        let planner = QueryPlanner::new(
            db.clone(),
            std::sync::Arc::new(HashEmbedder::new(64)),
            ExpansionConfig {
                // Deterministic, focused tests: no spell-fix/synonym noise
                // deciding which terms end up `TermOrigin::Original`.
                synonyms: false,
                claude: false,
                spellfix: false,
            },
        );
        Self {
            fts: fts.clone(),
            queue: IndexQueue::new(db.clone(), QueueOptions::default()),
            planner,
            db,
            account_id,
            mailbox_id,
            next_uid: std::cell::Cell::new(1),
            path,
        }
    }

    async fn index(&self, new: repo::NewMessage) -> i64 {
        let uid = self.next_uid.get();
        self.next_uid.set(uid + 1);
        let (account_id, mailbox_id) = (self.account_id, self.mailbox_id);
        let new = repo::NewMessage {
            account_id,
            mailbox_id,
            uid,
            uidvalidity: 1,
            ..new
        };
        let message_id = self
            .db
            .write(move |c| repo::insert_message(c, &new))
            .await
            .unwrap();
        extract_message(&self.db, &self.queue, message_id, PRIORITY_NORMAL)
            .await
            .unwrap();
        self.fts.index_message(message_id).await.unwrap();
        message_id
    }

    async fn plan(&self, raw: &str) -> QueryPlan {
        self.planner.plan(raw).await.unwrap()
    }

    fn retriever(&self) -> PrefixRetriever {
        PrefixRetriever::new(self.fts.clone(), self.db.clone())
    }

    async fn ids(&self, raw: &str) -> Vec<i64> {
        let plan = self.plan(raw).await;
        self.retriever()
            .retrieve(&plan, 100, &Fixture::no_cancel())
            .await
            .unwrap()
            .into_iter()
            .map(|c| c.message_id)
            .collect()
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
async fn a_partial_word_matches_a_complete_indexed_term() {
    let fx = Fixture::open().await;
    let msg = fx
        .index(repo::NewMessage {
            subject: Some("Quarterly invoice".to_owned()),
            ..Default::default()
        })
        .await;
    fx.index(repo::NewMessage {
        subject: Some("unrelated update".to_owned()),
        ..Default::default()
    })
    .await;

    assert_eq!(fx.ids("inv").await, vec![msg]);
}

#[tokio::test]
async fn candidates_carry_source_and_rank() {
    let fx = Fixture::open().await;
    fx.index(repo::NewMessage {
        subject: Some("invoice".to_owned()),
        ..Default::default()
    })
    .await;

    let plan = fx.plan("inv").await;
    let hits = fx
        .retriever()
        .retrieve(&plan, 100, &Fixture::no_cancel())
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].source, Source::Prefix);
    assert_eq!(hits[0].rank, 1);
}

#[tokio::test]
async fn a_pure_filter_query_returns_nothing() {
    let fx = Fixture::open().await;
    fx.index(repo::NewMessage {
        from_addr: Some("alice@example.com".to_owned()),
        subject: Some("invoice".to_owned()),
        ..Default::default()
    })
    .await;

    assert!(fx.ids("from:alice").await.is_empty());
}

#[tokio::test]
async fn a_term_shorter_than_the_minimum_is_not_prefix_matched() {
    let fx = Fixture::open().await;
    fx.index(repo::NewMessage {
        subject: Some("invoice".to_owned()),
        ..Default::default()
    })
    .await;

    assert!(
        fx.ids("i").await.is_empty(),
        "a single character would match nearly the whole vocabulary"
    );
}

#[tokio::test]
async fn a_negated_term_is_not_prefix_matched() {
    let fx = Fixture::open().await;
    fx.index(repo::NewMessage {
        subject: Some("invoice".to_owned()),
        ..Default::default()
    })
    .await;

    assert!(fx.ids("-inv").await.is_empty());
}

#[tokio::test]
async fn the_hard_filter_mask_still_applies() {
    let fx = Fixture::open().await;
    let from_alice = fx
        .index(repo::NewMessage {
            from_addr: Some("alice@example.com".to_owned()),
            subject: Some("invoice".to_owned()),
            ..Default::default()
        })
        .await;
    fx.index(repo::NewMessage {
        from_addr: Some("bob@example.com".to_owned()),
        subject: Some("invoice".to_owned()),
        ..Default::default()
    })
    .await;

    assert_eq!(fx.ids("inv from:alice").await, vec![from_alice]);
}

#[tokio::test]
async fn fts5_metacharacters_in_a_term_cannot_change_the_query_shape() {
    let fx = Fixture::open().await;
    let noise = fx
        .index(repo::NewMessage {
            subject: Some("completely unrelated filler content".to_owned()),
            ..Default::default()
        })
        .await;

    for text in ["OR", "AND", "NOT", "NEAR(x,1)", "*", "\""] {
        let ids = fx.ids(text).await;
        assert!(
            !ids.contains(&noise),
            "term {text:?} must not turn into an operator matching everything"
        );
    }
}

#[tokio::test]
async fn a_cancelled_token_degrades_to_no_candidates_without_erroring() {
    let fx = Fixture::open().await;
    fx.index(repo::NewMessage {
        subject: Some("invoice".to_owned()),
        ..Default::default()
    })
    .await;

    let plan = fx.plan("inv").await;
    let cancel = CancellationToken::new();
    cancel.cancel();
    let hits = fx.retriever().retrieve(&plan, 100, &cancel).await.unwrap();
    assert!(hits.is_empty());
}

#[tokio::test]
async fn an_embedded_nul_byte_is_invalid_argument_not_internal() {
    // A hand-built plan: `query::parse`/`QueryPlanner` never produce a NUL
    // byte from ordinary input, so this exercises the error-mapping path
    // directly, the same way `retrieve::lexical`'s own NUL-byte test does
    // for the sibling retriever that shares `fts::malformed_query`.
    let fx = Fixture::open().await;
    fx.index(repo::NewMessage {
        subject: Some("invoice".to_owned()),
        ..Default::default()
    })
    .await;

    let plan = QueryPlan {
        raw: "foo\0bar".to_owned(),
        hard_filters: Vec::new(),
        lexical_terms: vec![PlanTerm {
            text: "foo\0bar".to_owned(),
            negated: false,
            mode: Mode::Auto,
            weight: 1.0,
            origin: TermOrigin::Original,
        }],
        phrases: Vec::new(),
        expansions: Vec::new(),
        query_vector: None,
        entities: Vec::new(),
        intent: Intent::Navigational,
        sort: SortSpec::Relevance,
        scope: Scope::default(),
        needs_nl_compile: false,
    };

    let err = fx
        .retriever()
        .retrieve(&plan, 100, &Fixture::no_cancel())
        .await
        .expect_err("an embedded NUL must not silently succeed or degrade to empty");
    assert_eq!(
        err.reason(),
        ErrorReason::InvalidArgument,
        "a malformed query is the caller's mistake, not a server fault"
    );
}
