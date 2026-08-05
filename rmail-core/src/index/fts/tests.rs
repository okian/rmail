//! What the lexical index owes a ranker: field weighting that makes a subject
//! hit beat a body hit, phrases that mean phrases, and an index that never
//! outlives what it describes.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use super::*;
use crate::index::{extract_message, IndexQueue, QueueOptions, PRIORITY_NORMAL};
use crate::repo;
use crate::ErrorReason;

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Fixture {
    db: Database,
    fts: FtsIndex,
    queue: IndexQueue,
    account_id: i64,
    mailbox_id: i64,
    next_uid: std::cell::Cell<i64>,
    path: PathBuf,
}

impl Fixture {
    async fn open() -> Self {
        Self::with_weights(Bm25Weights::default()).await
    }

    async fn with_weights(weights: Bm25Weights) -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-fts-{pid}-{n}.db"));
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
            fts: FtsIndex::new(db.clone(), weights),
            queue: IndexQueue::new(db.clone(), QueueOptions::default()),
            db,
            account_id,
            mailbox_id,
            next_uid: std::cell::Cell::new(1),
            path,
        }
    }

    /// Store a message, extract it, and index it — the real pipeline.
    async fn index(&self, new: repo::NewMessage) -> i64 {
        let uid = self.next_uid.get();
        self.next_uid.set(uid + 1);
        let new = repo::NewMessage {
            account_id: self.account_id,
            mailbox_id: self.mailbox_id,
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

    async fn set_body(&self, message_id: i64, body: Option<&str>) {
        let body = body.map(str::to_owned);
        self.db
            .write(move |c| {
                c.execute(
                    "UPDATE messages SET body_text = ?2 WHERE id = ?1",
                    rusqlite::params![message_id, body],
                )
            })
            .await
            .unwrap();
        extract_message(&self.db, &self.queue, message_id, PRIORITY_NORMAL)
            .await
            .unwrap();
        self.fts.index_message(message_id).await.unwrap();
    }

    async fn ids(&self, query: &str) -> Vec<i64> {
        self.fts
            .search(query, 100)
            .await
            .unwrap()
            .into_iter()
            .map(|hit| hit.message_id)
            .collect()
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
async fn a_subject_hit_outranks_a_body_hit() {
    // The headline requirement, and the reason the columns exist at all. A term
    // in a subject is stronger evidence than the same term buried in a quoted
    // reply, and without field weighting BM25 cannot tell them apart.
    // Stacked *against* the subject on every other axis, so nothing but the
    // field weight can explain the order. BM25 favours short documents, so the
    // body-hit message gets a terse body and the subject-hit message carries a
    // long one — with uniform weights the body match would win.
    let fx = Fixture::open().await;
    let in_body = fx
        .index(repo::NewMessage {
            subject: Some("Unrelated".to_owned()),
            body_text: Some("Budget.".to_owned()),
            ..Default::default()
        })
        .await;
    let in_subject = fx
        .index(repo::NewMessage {
            subject: Some("Budget".to_owned()),
            body_text: Some(
                "Nothing much to add here, but this body runs on for a while so \
                 that length normalisation has every chance to prefer the other \
                 message instead of this one."
                    .to_owned(),
            ),
            ..Default::default()
        })
        .await;

    let hits = fx.fts.search("budget", 10).await.unwrap();

    assert_eq!(hits.len(), 2);
    assert_eq!(
        hits[0].message_id, in_subject,
        "the subject match ranks first despite the longer document"
    );
    assert_eq!(hits[1].message_id, in_body);
    assert!(
        hits[0].score > hits[1].score,
        "and scores higher: {} vs {}",
        hits[0].score,
        hits[1].score
    );
}

#[tokio::test]
async fn a_sender_hit_outranks_a_recipient_hit() {
    // Mail *from* someone is a stronger match than mail merely addressed to
    // them alongside forty other people — 4.0 against 2.0.
    let fx = Fixture::open().await;
    let addressed_to = fx
        .index(repo::NewMessage {
            subject: Some("Team update".to_owned()),
            from_addr: Some("someone@example.com".to_owned()),
            to_addrs: Some("ada@example.com".to_owned()),
            ..Default::default()
        })
        .await;
    let sent_by = fx
        .index(repo::NewMessage {
            subject: Some("Team update".to_owned()),
            from_addr: Some("ada@example.com".to_owned()),
            to_addrs: Some("someone@example.com".to_owned()),
            ..Default::default()
        })
        .await;

    let hits = fx.fts.search("ada", 10).await.unwrap();

    assert_eq!(hits.len(), 2);
    assert_eq!(hits[0].message_id, sent_by);
    assert_eq!(hits[1].message_id, addressed_to);
}

#[tokio::test]
async fn weights_are_configurable_and_actually_change_the_order() {
    // The PRD makes every weight TOML-tunable. A weight nothing reads is not a
    // setting, it is a comment.
    let inverted = Bm25Weights {
        subject: 1.0,
        body: 8.0,
        ..Bm25Weights::default()
    };
    let fx = Fixture::with_weights(inverted).await;
    let in_body = fx
        .index(repo::NewMessage {
            subject: Some("Unrelated".to_owned()),
            body_text: Some("We should discuss the budget at some point.".to_owned()),
            ..Default::default()
        })
        .await;
    fx.index(repo::NewMessage {
        subject: Some("Budget".to_owned()),
        body_text: Some("Nothing much to add here.".to_owned()),
        ..Default::default()
    })
    .await;

    let hits = fx.fts.search("budget", 10).await.unwrap();

    assert_eq!(
        hits[0].message_id, in_body,
        "with the weights inverted, so is the ranking"
    );
}

#[tokio::test]
async fn a_phrase_query_means_the_phrase() {
    let fx = Fixture::open().await;
    let exact = fx
        .index(repo::NewMessage {
            body_text: Some("Please review the quarterly report before Friday.".to_owned()),
            ..Default::default()
        })
        .await;
    fx.index(repo::NewMessage {
        body_text: Some("The report covers a quarterly cadence, loosely.".to_owned()),
        ..Default::default()
    })
    .await;

    assert_eq!(
        fx.ids("\"quarterly report\"").await,
        vec![exact],
        "only the message where those words are adjacent"
    );
    assert_eq!(
        fx.ids("quarterly report").await.len(),
        2,
        "the unquoted query matches both"
    );
}

#[tokio::test]
async fn diacritics_are_folded_so_an_unaccented_query_still_matches() {
    let fx = Fixture::open().await;
    let cafe = fx
        .index(repo::NewMessage {
            subject: Some("Meeting at the café".to_owned()),
            ..Default::default()
        })
        .await;

    assert_eq!(fx.ids("cafe").await, vec![cafe]);
    assert_eq!(
        fx.ids("café").await,
        vec![cafe],
        "and the accented form too"
    );
}

#[tokio::test]
async fn a_column_filter_restricts_the_match() {
    let fx = Fixture::open().await;
    fx.index(repo::NewMessage {
        body_text: Some("The word invoice appears only in this body.".to_owned()),
        ..Default::default()
    })
    .await;
    let in_subject = fx
        .index(repo::NewMessage {
            subject: Some("Invoice attached".to_owned()),
            ..Default::default()
        })
        .await;

    assert_eq!(fx.ids("subject:invoice").await, vec![in_subject]);
}

#[tokio::test]
async fn re_indexing_replaces_rather_than_accumulates() {
    // A contentless table has no update. Deleting first is what stops a term
    // that is no longer in the message from lingering in the index.
    let fx = Fixture::open().await;
    let message_id = fx
        .index(repo::NewMessage {
            body_text: Some("The original body mentions penguins.".to_owned()),
            ..Default::default()
        })
        .await;
    assert_eq!(fx.ids("penguins").await, vec![message_id]);

    fx.set_body(message_id, Some("The replacement body mentions walruses."))
        .await;

    assert!(
        fx.ids("penguins").await.is_empty(),
        "a term the message no longer contains must not still match it"
    );
    assert_eq!(fx.ids("walruses").await, vec![message_id]);
    assert_eq!(fx.fts.len().await.unwrap(), 1, "one row, not two");
}

#[tokio::test]
async fn a_part_that_disappears_stops_matching() {
    let fx = Fixture::open().await;
    let message_id = fx
        .index(repo::NewMessage {
            subject: Some("Still here".to_owned()),
            body_text: Some("Ephemeral body text.".to_owned()),
            ..Default::default()
        })
        .await;
    assert_eq!(fx.ids("ephemeral").await, vec![message_id]);

    fx.set_body(message_id, None).await;

    assert!(fx.ids("ephemeral").await.is_empty());
    assert_eq!(fx.ids("still").await, vec![message_id], "the subject stays");
}

#[tokio::test]
async fn deleting_a_message_removes_it_from_the_index() {
    // A virtual table takes no foreign key, so the cascade is a trigger. Mail
    // that stays searchable after it is gone is worse than mail that is
    // missing: every hit on it dangles.
    let fx = Fixture::open().await;
    let message_id = fx
        .index(repo::NewMessage {
            subject: Some("Doomed".to_owned()),
            ..Default::default()
        })
        .await;
    assert_eq!(fx.ids("doomed").await, vec![message_id]);

    fx.db
        .write(move |c| c.execute("DELETE FROM messages WHERE id = ?1", [message_id]))
        .await
        .unwrap();

    assert!(fx.ids("doomed").await.is_empty());
    assert!(fx.fts.is_empty().await.unwrap());
}

#[tokio::test]
async fn removing_a_message_by_hand_works_too() {
    let fx = Fixture::open().await;
    let message_id = fx
        .index(repo::NewMessage {
            subject: Some("Removable".to_owned()),
            ..Default::default()
        })
        .await;

    assert!(fx.fts.remove_message(message_id).await.unwrap());
    assert!(fx.ids("removable").await.is_empty());
    assert!(
        !fx.fts.remove_message(message_id).await.unwrap(),
        "removing it twice reports that there was nothing to remove"
    );
}

#[tokio::test]
async fn a_message_with_no_text_is_not_indexed() {
    // An empty document matches nothing and only costs the ranker a candidate
    // to discard.
    let fx = Fixture::open().await;
    let message_id = fx.index(repo::NewMessage::default()).await;

    assert!(!fx.fts.index_message(message_id).await.unwrap());
    assert!(fx.fts.is_empty().await.unwrap());
}

#[tokio::test]
async fn a_note_and_a_summary_are_searchable_in_their_own_fields() {
    // Both are written by other subsystems straight into `index_content`; the
    // lexical index has to pick them up without being told.
    let fx = Fixture::open().await;
    let message_id = fx
        .index(repo::NewMessage {
            subject: Some("Plain".to_owned()),
            ..Default::default()
        })
        .await;
    fx.db
        .write(move |c| {
            c.execute(
                "INSERT INTO index_content
                     (message_id, part, text, chars, content_hash, extractor)
                 VALUES
                     (?1, 'note', 'remember the alligator', 22, X'01', 'user'),
                     (?1, 'summary', 'a summary about zebras', 22, X'02', 'ai'),
                     (?1, 'attachment:2', 'scanned text about narwhals', 27, X'03', 'ocr')",
                [message_id],
            )
        })
        .await
        .unwrap();
    fx.fts.index_message(message_id).await.unwrap();

    assert_eq!(fx.ids("notes:alligator").await, vec![message_id]);
    assert_eq!(fx.ids("summary:zebras").await, vec![message_id]);
    assert_eq!(fx.ids("attachments:narwhals").await, vec![message_id]);
}

#[tokio::test]
async fn several_attachments_share_one_field() {
    // A ranker has no reason to care *which* attachment a term came from, only
    // that it came from one.
    let fx = Fixture::open().await;
    let message_id = fx
        .index(repo::NewMessage {
            subject: Some("Two files".to_owned()),
            ..Default::default()
        })
        .await;
    fx.db
        .write(move |c| {
            c.execute(
                "INSERT INTO index_content
                     (message_id, part, text, chars, content_hash, extractor)
                 VALUES
                     (?1, 'attachment:1', 'first file mentions okapi', 25, X'01', 'ocr'),
                     (?1, 'attachment:2', 'second file mentions tapir', 26, X'02', 'ocr')",
                [message_id],
            )
        })
        .await
        .unwrap();
    fx.fts.index_message(message_id).await.unwrap();

    assert_eq!(fx.ids("attachments:okapi").await, vec![message_id]);
    assert_eq!(fx.ids("attachments:tapir").await, vec![message_id]);
}

#[tokio::test]
async fn a_malformed_query_is_the_users_problem_not_the_servers() {
    // FTS5 reports a syntax error as a plain SQL error. Letting that surface as
    // INTERNAL would tell someone their daemon is broken when they typed an
    // unbalanced quote.
    let fx = Fixture::open().await;
    fx.index(repo::NewMessage {
        subject: Some("Anything".to_owned()),
        ..Default::default()
    })
    .await;

    for bad in ["\"unbalanced", "AND", "NEAR(", "*"] {
        let err = fx
            .fts
            .search(bad, 10)
            .await
            .expect_err("`{bad}` is not valid FTS5 syntax");
        assert_eq!(
            err.reason(),
            ErrorReason::InvalidArgument,
            "query {bad:?} reported as {:?}",
            err.reason()
        );
    }
}

#[tokio::test]
async fn an_empty_query_returns_nothing_rather_than_erroring() {
    // A blank search box is not a mistake to report.
    let fx = Fixture::open().await;
    fx.index(repo::NewMessage {
        subject: Some("Something".to_owned()),
        ..Default::default()
    })
    .await;

    assert!(fx.fts.search("", 10).await.unwrap().is_empty());
    assert!(fx.fts.search("   ", 10).await.unwrap().is_empty());
}

#[tokio::test]
async fn the_page_size_is_bounded() {
    let fx = Fixture::open().await;
    for n in 0..5 {
        fx.index(repo::NewMessage {
            subject: Some(format!("Message {n} about hedgehogs")),
            ..Default::default()
        })
        .await;
    }

    assert_eq!(fx.fts.search("hedgehogs", 2).await.unwrap().len(), 2);
    assert_eq!(
        fx.fts.search("hedgehogs", 0).await.unwrap().len(),
        5,
        "zero means the server default, as an unset field would"
    );
    assert_eq!(fx.fts.search("hedgehogs", -1).await.unwrap().len(), 5);
}

#[tokio::test]
async fn scores_are_oriented_so_higher_is_better() {
    // BM25's native sign is negative-is-better. A caller that had to remember
    // that would eventually forget.
    let fx = Fixture::open().await;
    fx.index(repo::NewMessage {
        subject: Some("Pangolin pangolin pangolin".to_owned()),
        ..Default::default()
    })
    .await;
    fx.index(repo::NewMessage {
        body_text: Some("One mention of a pangolin, in passing.".to_owned()),
        ..Default::default()
    })
    .await;

    let hits = fx.fts.search("pangolin", 10).await.unwrap();

    assert!(hits[0].score > hits[1].score);
    assert!(
        hits.iter().all(|hit| hit.score > 0.0),
        "a relevance score below zero would read as an anti-match: {hits:?}"
    );
}

#[tokio::test]
async fn an_absurd_weight_cannot_invert_the_ranking() {
    // A negative or non-finite weight from a config file would make bm25()
    // order results in a way no author intended.
    let fx = Fixture::with_weights(Bm25Weights {
        subject: -5.0,
        body: f64::NAN,
        ..Bm25Weights::default()
    })
    .await;
    fx.index(repo::NewMessage {
        subject: Some("Wombat".to_owned()),
        ..Default::default()
    })
    .await;

    let hits = fx.fts.search("wombat", 10).await.unwrap();
    assert_eq!(hits.len(), 1);
    assert!(
        hits[0].score.is_finite(),
        "a poisoned weight must not produce a poisoned score"
    );
}

#[test]
fn every_column_has_a_weight_in_the_same_order() {
    // `bm25()` takes one weight per column positionally. If these ever drift,
    // every ranking is silently wrong in a way no test of a single field would
    // catch.
    let index = FtsIndex::new(
        Database::open(std::env::temp_dir().join(format!(
            "rmail-fts-cols-{}-{}.db",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        )))
        .unwrap(),
        Bm25Weights::default(),
    );
    assert_eq!(COLUMNS.len(), index.column_weights().len());
    assert_eq!(
        index.weight_list().split(", ").count(),
        COLUMNS.len(),
        "the argument list must have exactly one weight per column"
    );
}
