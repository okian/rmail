//! What the entity retriever owes a caller: a message that mentions the same
//! entity the query text names is found, a contact match never feeds this
//! source (see the module docs), several agreeing entities outscore one, and
//! the hard-filter mask and cancellation behave like every other retriever
//! in this task.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use tokio_util::sync::CancellationToken;

use super::*;
use crate::index::extract_entities;
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
        let path = std::env::temp_dir().join(format!("rmail-retrieve-entity-{pid}-{n}.db"));
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

    /// Insert a message, give it `body` as its only extracted part, and run
    /// the real entity extractor over it — the same "insert `index_content`
    /// directly, skip MIME parsing" shortcut `index::semantic`'s own tests
    /// use, since what this module needs is a populated `entities`/
    /// `entity_mentions`, not a realistic message.
    async fn message_mentioning(&self, body: &str) -> i64 {
        let uid = self.next_uid.get();
        self.next_uid.set(uid + 1);
        let (account_id, mailbox_id) = (self.account_id, self.mailbox_id);
        let body = body.to_owned();
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
        extract_entities(&self.db, message_id).await.unwrap();
        message_id
    }

    async fn plan(&self, raw: &str) -> QueryPlan {
        self.planner.plan(raw).await.unwrap()
    }

    fn retriever(&self) -> EntityRetriever {
        EntityRetriever::new(self.db.clone())
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
async fn a_message_mentioning_the_same_entity_as_the_query_is_found() {
    let fx = Fixture::open().await;
    let matches = fx
        .message_mentioning("Please pay billing@acme.com by Friday")
        .await;
    fx.message_mentioning("nothing relevant in this one").await;

    let plan = fx.plan("billing@acme.com").await;
    assert!(
        !plan.entities.is_empty(),
        "the query text itself must have been recognized as an entity span"
    );

    let hits = fx
        .retriever()
        .retrieve(&plan, 100, &Fixture::no_cancel())
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].message_id, matches);
    assert_eq!(hits[0].source, Source::Entity);
    assert_eq!(hits[0].rank, 1);
    assert!(hits[0].score > 0.0);
}

#[tokio::test]
async fn a_query_with_no_recognizable_entity_contributes_nothing() {
    let fx = Fixture::open().await;
    fx.message_mentioning("Please pay billing@acme.com by Friday")
        .await;

    let plan = fx.plan("budget report").await;
    assert!(plan.entities.is_empty());
    let hits = fx
        .retriever()
        .retrieve(&plan, 100, &Fixture::no_cancel())
        .await
        .unwrap();
    assert!(hits.is_empty());
}

#[tokio::test]
async fn a_message_matching_two_query_entities_outscores_one_matching_a_single_entity() {
    let fx = Fixture::open().await;
    let both = fx
        .message_mentioning("billing@acme.com re: order REF-99001 attached")
        .await;
    let one = fx
        .message_mentioning("billing@acme.com only, nothing else")
        .await;

    let plan = fx.plan("billing@acme.com order REF-99001").await;
    let hits = fx
        .retriever()
        .retrieve(&plan, 100, &Fixture::no_cancel())
        .await
        .unwrap();

    let both_score = hits.iter().find(|c| c.message_id == both).unwrap().score;
    let one_score = hits.iter().find(|c| c.message_id == one).unwrap().score;
    assert!(
        both_score > one_score,
        "agreeing on two entities ({both_score}) should outscore agreeing on one ({one_score})"
    );
}

#[tokio::test]
async fn the_hard_filter_mask_still_applies() {
    let fx = Fixture::open().await;
    let uid = fx.next_uid.get();
    let (account_id, mailbox_id) = (fx.account_id, fx.mailbox_id);
    let body = "billing@acme.com attached".to_owned();
    let from_alice = fx
        .db
        .write(move |c| {
            let id = repo::insert_message(
                c,
                &repo::NewMessage {
                    account_id,
                    mailbox_id,
                    uid,
                    uidvalidity: 1,
                    from_addr: Some("alice@example.com".to_owned()),
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
    fx.next_uid.set(uid + 1);
    extract_entities(&fx.db, from_alice).await.unwrap();
    fx.message_mentioning("billing@acme.com from someone else")
        .await;

    let plan = fx.plan("billing@acme.com from:alice").await;
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
    fx.message_mentioning("billing@acme.com attached").await;

    let plan = fx.plan("billing@acme.com").await;
    let cancel = CancellationToken::new();
    cancel.cancel();
    let hits = fx.retriever().retrieve(&plan, 100, &cancel).await.unwrap();
    assert!(hits.is_empty());
}
