//! What the recency retriever owes a caller: newest-first ordering under
//! decay scoring, `exp(-age/half_life)` computed against an injected "now"
//! (reproducible, the same discipline `query::plan`'s own date resolution
//! tests use), a dateless message excluded rather than guessed at, and the
//! hard-filter mask honored exactly like every other retriever in this task.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use chrono::{TimeZone, Utc};
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
        let path = std::env::temp_dir().join(format!("rmail-retrieve-recency-{pid}-{n}.db"));
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

    fn retriever(&self, half_life_days: f64) -> RecencyRetriever {
        RecencyRetriever::new(self.db.clone(), half_life_days)
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

fn days_ago(now: chrono::DateTime<Utc>, days: i64) -> i64 {
    now.timestamp() - days * 86_400
}

#[tokio::test]
async fn newer_mail_ranks_above_older_mail() {
    let fx = Fixture::open().await;
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 0, 0, 0).unwrap();
    let recent = fx
        .insert(repo::NewMessage {
            date: Some(days_ago(now, 1)),
            ..Default::default()
        })
        .await;
    let old = fx
        .insert(repo::NewMessage {
            date: Some(days_ago(now, 60)),
            ..Default::default()
        })
        .await;

    let plan = fx.plan("").await;
    let hits = fx
        .retriever(30.0)
        .retrieve_at(&plan, 100, &Fixture::no_cancel(), now)
        .await
        .unwrap();

    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].message_id, recent);
    assert_eq!(hits[0].source, Source::Recency);
    assert_eq!(hits[1].message_id, old);
    assert!(hits[0].score > hits[1].score);
}

#[tokio::test]
async fn score_matches_the_exp_decay_formula() {
    let fx = Fixture::open().await;
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 0, 0, 0).unwrap();
    let msg = fx
        .insert(repo::NewMessage {
            date: Some(days_ago(now, 30)),
            ..Default::default()
        })
        .await;

    let plan = fx.plan("").await;
    let hits = fx
        .retriever(30.0)
        .retrieve_at(&plan, 100, &Fixture::no_cancel(), now)
        .await
        .unwrap();

    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].message_id, msg);
    // 30 days old at a 30-day half-life: exp(-1) ~= 0.3679.
    assert!(
        (hits[0].score - std::f64::consts::E.recip()).abs() < 1e-9,
        "got {}",
        hits[0].score
    );
}

#[tokio::test]
async fn a_message_with_no_date_is_excluded_rather_than_guessed_at() {
    let fx = Fixture::open().await;
    fx.insert(repo::NewMessage::default()).await;
    let dated = fx
        .insert(repo::NewMessage {
            date: Some(1_000),
            ..Default::default()
        })
        .await;

    let plan = fx.plan("").await;
    let hits = fx
        .retriever(30.0)
        .retrieve(&plan, 100, &Fixture::no_cancel())
        .await
        .unwrap();
    assert_eq!(
        hits.into_iter().map(|c| c.message_id).collect::<Vec<_>>(),
        vec![dated]
    );
}

#[tokio::test]
async fn a_future_dated_message_does_not_score_above_one() {
    let fx = Fixture::open().await;
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 0, 0, 0).unwrap();
    fx.insert(repo::NewMessage {
        date: Some(now.timestamp() + 30 * 86_400),
        ..Default::default()
    })
    .await;

    let plan = fx.plan("").await;
    let hits = fx
        .retriever(30.0)
        .retrieve_at(&plan, 100, &Fixture::no_cancel(), now)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert!(
        hits[0].score <= 1.0,
        "clock skew must not let a message score above the undecayed maximum, got {}",
        hits[0].score
    );
}

#[tokio::test]
async fn the_hard_filter_mask_still_applies() {
    let fx = Fixture::open().await;
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 0, 0, 0).unwrap();
    let from_alice = fx
        .insert(repo::NewMessage {
            from_addr: Some("alice@example.com".to_owned()),
            date: Some(days_ago(now, 1)),
            ..Default::default()
        })
        .await;
    fx.insert(repo::NewMessage {
        from_addr: Some("bob@example.com".to_owned()),
        date: Some(days_ago(now, 1)),
        ..Default::default()
    })
    .await;

    let plan = fx.plan("from:alice").await;
    let hits = fx
        .retriever(30.0)
        .retrieve_at(&plan, 100, &Fixture::no_cancel(), now)
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
    fx.insert(repo::NewMessage {
        date: Some(1_000),
        ..Default::default()
    })
    .await;

    let plan = fx.plan("tag:work").await;
    let hits = fx
        .retriever(30.0)
        .retrieve(&plan, 100, &Fixture::no_cancel())
        .await
        .unwrap();
    assert!(hits.is_empty());
}

#[tokio::test]
async fn an_invalid_half_life_falls_back_to_the_default_instead_of_dividing_by_zero() {
    let fx = Fixture::open().await;
    let now = Utc.with_ymd_and_hms(2024, 6, 15, 0, 0, 0).unwrap();
    fx.insert(repo::NewMessage {
        date: Some(days_ago(now, 1)),
        ..Default::default()
    })
    .await;

    let plan = fx.plan("").await;
    for bad in [0.0, -5.0, f64::NAN, f64::INFINITY] {
        let hits = fx
            .retriever(bad)
            .retrieve_at(&plan, 100, &Fixture::no_cancel(), now)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert!(
            hits[0].score.is_finite() && hits[0].score > 0.0,
            "half_life {bad} produced a non-finite or non-positive score: {}",
            hits[0].score
        );
    }
}

#[tokio::test]
async fn a_cancelled_token_degrades_to_no_candidates_without_erroring() {
    let fx = Fixture::open().await;
    fx.insert(repo::NewMessage {
        date: Some(1_000),
        ..Default::default()
    })
    .await;

    let plan = fx.plan("").await;
    let cancel = CancellationToken::new();
    cancel.cancel();
    let hits = fx
        .retriever(30.0)
        .retrieve(&plan, 100, &cancel)
        .await
        .unwrap();
    assert!(hits.is_empty());
}
