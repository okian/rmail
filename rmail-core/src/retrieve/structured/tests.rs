//! What the structured retriever owes a caller: a filter-only query returns
//! matching messages (which no other retriever in this build can do — see
//! the module docs), an unconstrained query contributes nothing, and
//! cancellation degrades to no candidates rather than an error.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use tokio_util::sync::CancellationToken;

use super::*;
use crate::query::QueryPlanner;
use crate::{config::ExpansionConfig, embed::hash::HashEmbedder, repo};

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Fixture {
    db: Database,
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
        let path = std::env::temp_dir().join(format!("rmail-retrieve-structured-{pid}-{n}.db"));
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
        let planner = QueryPlanner::new(
            db.clone(),
            std::sync::Arc::new(HashEmbedder::new(64)),
            ExpansionConfig::default(),
        );
        Self {
            planner,
            db,
            account_id,
            mailbox_id,
            next_uid: std::cell::Cell::new(1),
            path,
        }
    }

    async fn insert(&self, new: repo::NewMessage) -> i64 {
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
        self.db
            .write(move |c| repo::insert_message(c, &new))
            .await
            .unwrap()
    }

    async fn flag(&self, message_id: i64, flag: &str) {
        let flag = flag.to_owned();
        self.db
            .write(move |c| repo::add_flag(c, message_id, &flag))
            .await
            .unwrap();
    }

    async fn plan(&self, raw: &str) -> QueryPlan {
        self.planner.plan(raw).await.unwrap()
    }

    fn retriever(&self) -> StructuredRetriever {
        StructuredRetriever::new(self.db.clone())
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
async fn a_pure_filter_query_returns_matching_messages() {
    let fx = Fixture::open().await;
    let from_alice = fx
        .insert(repo::NewMessage {
            from_addr: Some("alice@example.com".to_owned()),
            ..Default::default()
        })
        .await;
    fx.insert(repo::NewMessage {
        from_addr: Some("bob@example.com".to_owned()),
        ..Default::default()
    })
    .await;

    let plan = fx.plan("from:alice").await;
    let hits = fx
        .retriever()
        .retrieve(&plan, 100, &Fixture::no_cancel())
        .await
        .unwrap();

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].message_id, from_alice);
    assert_eq!(hits[0].source, Source::Structured);
    assert_eq!(
        hits[0].score, 1.0,
        "pass/fail: every survivor scores the same"
    );
    assert_eq!(hits[0].rank, 1);
}

#[tokio::test]
async fn survivors_are_ordered_most_recent_first() {
    let fx = Fixture::open().await;
    let older = fx
        .insert(repo::NewMessage {
            from_addr: Some("alice@example.com".to_owned()),
            date: Some(1_000),
            ..Default::default()
        })
        .await;
    let newer = fx
        .insert(repo::NewMessage {
            from_addr: Some("alice@example.com".to_owned()),
            date: Some(2_000),
            ..Default::default()
        })
        .await;

    let plan = fx.plan("from:alice").await;
    let hits = fx
        .retriever()
        .retrieve(&plan, 100, &Fixture::no_cancel())
        .await
        .unwrap();

    assert_eq!(
        hits.into_iter().map(|c| c.message_id).collect::<Vec<_>>(),
        vec![newer, older]
    );
}

#[tokio::test]
async fn an_unconstrained_query_contributes_nothing() {
    let fx = Fixture::open().await;
    fx.insert(repo::NewMessage {
        subject: Some("budget report".to_owned()),
        ..Default::default()
    })
    .await;

    let plan = fx.plan("budget report").await;
    let hits = fx
        .retriever()
        .retrieve(&plan, 100, &Fixture::no_cancel())
        .await
        .unwrap();
    assert!(
        hits.is_empty(),
        "no operators means nothing for the structured retriever to gate on"
    );
}

#[tokio::test]
async fn a_filter_that_excludes_everything_returns_nothing() {
    let fx = Fixture::open().await;
    fx.insert(repo::NewMessage {
        from_addr: Some("alice@example.com".to_owned()),
        ..Default::default()
    })
    .await;

    let plan = fx.plan("tag:work").await;
    let hits = fx
        .retriever()
        .retrieve(&plan, 100, &Fixture::no_cancel())
        .await
        .unwrap();
    assert!(hits.is_empty());
}

#[tokio::test]
async fn multiple_filters_conjoin() {
    let fx = Fixture::open().await;
    let matches_both = fx
        .insert(repo::NewMessage {
            from_addr: Some("alice@example.com".to_owned()),
            ..Default::default()
        })
        .await;
    let read_alice = fx
        .insert(repo::NewMessage {
            from_addr: Some("alice@example.com".to_owned()),
            ..Default::default()
        })
        .await;
    fx.flag(read_alice, "\\Seen").await;
    fx.insert(repo::NewMessage {
        from_addr: Some("bob@example.com".to_owned()),
        ..Default::default()
    })
    .await;

    let plan = fx.plan("from:alice is:unread").await;
    let hits = fx
        .retriever()
        .retrieve(&plan, 100, &Fixture::no_cancel())
        .await
        .unwrap();
    assert_eq!(
        hits.into_iter().map(|c| c.message_id).collect::<Vec<_>>(),
        vec![matches_both]
    );
}

#[tokio::test]
async fn a_cancelled_token_degrades_to_no_candidates_without_erroring() {
    let fx = Fixture::open().await;
    fx.insert(repo::NewMessage {
        from_addr: Some("alice@example.com".to_owned()),
        ..Default::default()
    })
    .await;

    let plan = fx.plan("from:alice").await;
    let cancel = CancellationToken::new();
    cancel.cancel();
    let hits = fx.retriever().retrieve(&plan, 100, &cancel).await.unwrap();
    assert!(hits.is_empty());
}
