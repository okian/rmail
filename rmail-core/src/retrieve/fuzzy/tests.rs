//! What the fuzzy retriever owes a caller: a subsequence of a subject/sender
//! field matches even without contiguous or exact spelling, smart-case
//! behaves the way prd.md's Part III spec describes, a pure filter query
//! contributes nothing, and the hard-filter mask and cancellation behave
//! like every other retriever in this task.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use tokio_util::sync::CancellationToken;

use super::*;
use crate::config::ExpansionConfig;
use crate::embed::hash::HashEmbedder;
use crate::query::QueryPlanner;
use crate::repo;

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
        let path = std::env::temp_dir().join(format!("rmail-retrieve-fuzzy-{pid}-{n}.db"));
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

    async fn plan(&self, raw: &str) -> QueryPlan {
        self.planner.plan(raw).await.unwrap()
    }

    fn retriever(&self) -> FuzzyRetriever {
        FuzzyRetriever::new(self.db.clone())
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
async fn a_non_contiguous_subsequence_matches_a_subject() {
    let fx = Fixture::open().await;
    // "ivc" is a subsequence of "invoice" (i-_-v-_-_-c-_) but not contiguous
    // and not a substring — proving this is genuinely subsequence matching,
    // not `LIKE '%ivc%'`.
    let invoice = fx
        .insert(repo::NewMessage {
            subject: Some("Invoice attached".to_owned()),
            ..Default::default()
        })
        .await;
    fx.insert(repo::NewMessage {
        subject: Some("weekly newsletter digest".to_owned()),
        ..Default::default()
    })
    .await;

    let plan = fx.plan("ivc").await;
    let hits = fx
        .retriever()
        .retrieve(&plan, 100, &Fixture::no_cancel())
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].message_id, invoice);
    assert_eq!(hits[0].source, Source::Fuzzy);
    assert_eq!(hits[0].rank, 1);
}

#[tokio::test]
async fn matching_is_case_insensitive_for_a_lowercase_query() {
    let fx = Fixture::open().await;
    let msg = fx
        .insert(repo::NewMessage {
            subject: Some("INVOICE ATTACHED".to_owned()),
            ..Default::default()
        })
        .await;

    let plan = fx.plan("invoice").await;
    let hits = fx
        .retriever()
        .retrieve(&plan, 100, &Fixture::no_cancel())
        .await
        .unwrap();
    assert_eq!(
        hits.into_iter().map(|c| c.message_id).collect::<Vec<_>>(),
        vec![msg]
    );
}

#[tokio::test]
async fn sender_fields_are_matched_as_well_as_subject() {
    let fx = Fixture::open().await;
    let msg = fx
        .insert(repo::NewMessage {
            from_name: Some("Acme Billing".to_owned()),
            from_addr: Some("billing@acme.example".to_owned()),
            ..Default::default()
        })
        .await;

    let plan = fx.plan("acme").await;
    let hits = fx
        .retriever()
        .retrieve(&plan, 100, &Fixture::no_cancel())
        .await
        .unwrap();
    assert_eq!(
        hits.into_iter().map(|c| c.message_id).collect::<Vec<_>>(),
        vec![msg]
    );
}

#[tokio::test]
async fn a_pure_filter_query_contributes_nothing() {
    let fx = Fixture::open().await;
    fx.insert(repo::NewMessage {
        subject: Some("invoice".to_owned()),
        from_addr: Some("alice@example.com".to_owned()),
        ..Default::default()
    })
    .await;

    let plan = fx.plan("from:alice").await;
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
        .insert(repo::NewMessage {
            subject: Some("invoice".to_owned()),
            from_addr: Some("alice@example.com".to_owned()),
            ..Default::default()
        })
        .await;
    fx.insert(repo::NewMessage {
        subject: Some("invoice".to_owned()),
        from_addr: Some("bob@example.com".to_owned()),
        ..Default::default()
    })
    .await;

    let plan = fx.plan("invoice from:alice").await;
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
async fn a_cancelled_token_degrades_to_no_candidates_without_erroring() {
    let fx = Fixture::open().await;
    fx.insert(repo::NewMessage {
        subject: Some("invoice".to_owned()),
        ..Default::default()
    })
    .await;

    let plan = fx.plan("invoice").await;
    let cancel = CancellationToken::new();
    cancel.cancel();
    let hits = fx.retriever().retrieve(&plan, 100, &cancel).await.unwrap();
    assert!(hits.is_empty());
}
