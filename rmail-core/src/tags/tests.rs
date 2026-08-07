//! `TagStore`-level integration tests: orchestration across
//! `repo`/`hierarchy`/`sync` against a real database.
//!
//! Wire-level correctness (the exact `STORE` syntax, coalescing, and the
//! `auto` downgrade being driven by a genuine IMAP `NO`) is already proven
//! against the real [`crate::imap::mock`] server in `imap::mutate`'s own
//! tests and in `tags::sync`'s tests (see that module for a `MockImap`-backed
//! fixture identical in spirit to this one). This file uses a lightweight
//! recording fake instead, the same "prove domain-level ordering against a
//! fake IMAP mutator" pattern `rmaild::mail_service`'s own integration tests
//! use for `MailStore` — what matters here is *whether* `TagStore` calls
//! IMAP, with what arguments, how many times, and what it does with the
//! result, not re-deriving the wire bytes a lower layer already covers.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use crate::config::{TagSyncMode, TagsConfig};
use crate::error::{Error, ErrorReason};
use crate::imap::mutate::ImapMutator;
use crate::storage::Database;

use super::*;

static COUNTER: AtomicU32 = AtomicU32::new(0);

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

struct Fixture {
    db: Database,
    path: PathBuf,
    account_id: i64,
    mailbox_id: i64,
}

impl Fixture {
    fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-tags-{pid}-{n}.db"));
        let db = Database::open(&path).expect("open temp db");
        let account_id = db
            .with_write(|conn| {
                crate::repo::insert_account(
                    conn,
                    &crate::repo::NewAccount {
                        name: format!("acct-{n}"),
                        ..Default::default()
                    },
                )
            })
            .unwrap();
        let mailbox_id = db
            .with_write(move |conn| {
                crate::repo::insert_mailbox(
                    conn,
                    &crate::repo::NewMailbox {
                        account_id,
                        name: "INBOX".to_owned(),
                        ..Default::default()
                    },
                )
            })
            .unwrap();
        Self {
            db,
            path,
            account_id,
            mailbox_id,
        }
    }

    fn seed_message(&self, uid: i64) -> i64 {
        let account_id = self.account_id;
        let mailbox_id = self.mailbox_id;
        self.db
            .with_write(move |conn| {
                crate::repo::insert_message(
                    conn,
                    &crate::repo::NewMessage {
                        account_id,
                        mailbox_id,
                        uid,
                        uidvalidity: 1,
                        ..Default::default()
                    },
                )
            })
            .unwrap()
    }

    fn seed_thread_with_messages(&self, uids: &[i64]) -> (i64, Vec<i64>) {
        let account_id = self.account_id;
        let thread_id = self
            .db
            .with_write(move |conn| {
                crate::repo::insert_thread(
                    conn,
                    &crate::repo::NewThread {
                        account_id,
                        ..Default::default()
                    },
                )
            })
            .unwrap();
        let mut ids = Vec::new();
        for uid in uids {
            let uid = *uid;
            let mailbox_id = self.mailbox_id;
            let id = self
                .db
                .with_write(move |conn| {
                    crate::repo::insert_message(
                        conn,
                        &crate::repo::NewMessage {
                            account_id,
                            mailbox_id,
                            uid,
                            uidvalidity: 1,
                            thread_id: Some(thread_id),
                            ..Default::default()
                        },
                    )
                })
                .unwrap();
            ids.push(id);
        }
        (thread_id, ids)
    }

    /// A store whose `default_sync_mode` is `local` — the common case for
    /// tests not exercising the IMAP round-trip, so `imap` never has to be
    /// touched for a tag created without an explicit `sync_mode`.
    fn store(&self, imap: Arc<dyn ImapMutator>) -> TagStore {
        let config = TagsConfig {
            default_sync_mode: TagSyncMode::Local,
            ..Default::default()
        };
        TagStore::new(self.db.clone(), imap, config)
    }

    fn store_local(&self) -> TagStore {
        self.store(Arc::new(NoImap))
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.path.display())));
        }
    }
}

// ---------------------------------------------------------------------------
// Fake IMap mutators
// ---------------------------------------------------------------------------

/// An `ImapMutator` that must never be called — used by tests whose tags are
/// all `sync_mode = local`, so any call at all is a bug.
#[derive(Debug, Default)]
struct NoImap;

#[async_trait]
impl ImapMutator for NoImap {
    async fn set_flags(&self, _: i64, _: &str, _: i64, _: i64, _: &[String]) -> Result<(), Error> {
        Err(Error::internal("NoImap: unexpected set_flags call"))
    }
    async fn move_message(&self, _: i64, _: &str, _: i64, _: i64, _: &str) -> Result<(), Error> {
        Err(Error::internal("NoImap: unexpected move_message call"))
    }
    async fn copy_message(&self, _: i64, _: &str, _: i64, _: i64, _: &str) -> Result<(), Error> {
        Err(Error::internal("NoImap: unexpected copy_message call"))
    }
    async fn delete_message(&self, _: i64, _: &str, _: i64, _: i64) -> Result<(), Error> {
        Err(Error::internal("NoImap: unexpected delete_message call"))
    }
    async fn store_keyword(
        &self,
        _: i64,
        _: &str,
        _: i64,
        _: &[i64],
        _: &str,
        _: bool,
        _: bool,
    ) -> Result<(), Error> {
        Err(Error::internal("NoImap: unexpected store_keyword call"))
    }
}

#[derive(Debug, Clone, PartialEq)]
struct StoreCall {
    mailbox: String,
    uidvalidity: i64,
    uids: Vec<i64>,
    keyword: String,
    prefer_gmail_label: bool,
    add: bool,
}

/// Records every `store_keyword` call; the other four `ImapMutator` methods
/// are never exercised by this module's tests.
#[derive(Debug, Default)]
struct RecordingImap {
    calls: Mutex<Vec<StoreCall>>,
    fail_store: bool,
}

impl RecordingImap {
    fn failing() -> Self {
        Self {
            fail_store: true,
            ..Default::default()
        }
    }

    fn calls(&self) -> Vec<StoreCall> {
        self.calls.lock().expect("not poisoned").clone()
    }
}

#[async_trait]
impl ImapMutator for RecordingImap {
    async fn set_flags(&self, _: i64, _: &str, _: i64, _: i64, _: &[String]) -> Result<(), Error> {
        Err(Error::internal("RecordingImap: unexpected set_flags call"))
    }
    async fn move_message(&self, _: i64, _: &str, _: i64, _: i64, _: &str) -> Result<(), Error> {
        Err(Error::internal(
            "RecordingImap: unexpected move_message call",
        ))
    }
    async fn copy_message(&self, _: i64, _: &str, _: i64, _: i64, _: &str) -> Result<(), Error> {
        Err(Error::internal(
            "RecordingImap: unexpected copy_message call",
        ))
    }
    async fn delete_message(&self, _: i64, _: &str, _: i64, _: i64) -> Result<(), Error> {
        Err(Error::internal(
            "RecordingImap: unexpected delete_message call",
        ))
    }
    async fn store_keyword(
        &self,
        _account_id: i64,
        mailbox: &str,
        uidvalidity: i64,
        uids: &[i64],
        keyword: &str,
        prefer_gmail_label: bool,
        add: bool,
    ) -> Result<(), Error> {
        self.calls.lock().expect("not poisoned").push(StoreCall {
            mailbox: mailbox.to_owned(),
            uidvalidity,
            uids: uids.to_vec(),
            keyword: keyword.to_owned(),
            prefer_gmail_label,
            add,
        });
        if self.fail_store {
            return Err(Error::unavailable("recording imap: store refused"));
        }
        Ok(())
    }
}

fn effective_tag_names(db: &Database, message_id: i64) -> Vec<String> {
    db.with_read(move |conn| {
        let mut stmt = conn.prepare(
            "SELECT t.name FROM messages_tags_effective mte
             JOIN tags t ON t.id = mte.tag_id
             WHERE mte.message_id = ?1 ORDER BY t.name",
        )?;
        let rows = stmt.query_map([message_id], |row| row.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<String>>>()
    })
    .unwrap()
}

// ---------------------------------------------------------------------------
// create_tag
// ---------------------------------------------------------------------------

#[tokio::test]
async fn create_tag_creates_a_new_tag() {
    let fx = Fixture::open();
    let store = fx.store_local();
    let tag = store
        .create_tag(
            fx.account_id,
            "project/alpha",
            Some("#7aa2f7".to_owned()),
            Some(TagSyncMode::Imap),
            None,
        )
        .await
        .unwrap();
    assert_eq!(tag.name, "project/alpha");
    assert_eq!(tag.color.as_deref(), Some("#7aa2f7"));
    assert_eq!(tag.sync_mode, TagSyncMode::Imap);
    assert!(
        tag.parent_id.is_some(),
        "should be auto-parented under `project`"
    );
}

#[tokio::test]
async fn create_tag_upserts_an_existing_name() {
    let fx = Fixture::open();
    let store = fx.store_local();
    let first = store
        .create_tag(fx.account_id, "work", None, Some(TagSyncMode::Local), None)
        .await
        .unwrap();
    let second = store
        .create_tag(
            fx.account_id,
            "work",
            Some("#ffffff".to_owned()),
            Some(TagSyncMode::Auto),
            None,
        )
        .await
        .unwrap();
    assert_eq!(first.id, second.id, "must update, not duplicate");
    assert_eq!(second.color.as_deref(), Some("#ffffff"));
    assert_eq!(second.sync_mode, TagSyncMode::Auto);
}

#[tokio::test]
async fn create_tag_rejects_making_a_tag_its_own_ancestor() {
    let fx = Fixture::open();
    let store = fx.store_local();
    let a = store
        .create_tag(fx.account_id, "a", None, Some(TagSyncMode::Local), None)
        .await
        .unwrap();
    let b = store
        .create_tag(
            fx.account_id,
            "b",
            None,
            Some(TagSyncMode::Local),
            Some(a.id),
        )
        .await
        .unwrap();
    assert_eq!(b.parent_id, Some(a.id));

    // Reparenting `a` under `b` (a's own child) must be rejected.
    let err = store
        .create_tag(fx.account_id, "a", None, None, Some(b.id))
        .await
        .expect_err("a cycle must be rejected");
    assert_eq!(err.reason(), ErrorReason::InvalidArgument);

    // And the parent must not have changed.
    let unchanged = store.get_or_create_tag(fx.account_id, "a").await.unwrap();
    assert_eq!(
        unchanged.parent_id, None,
        "the rejected reparent must not apply"
    );
}

#[tokio::test]
async fn create_tag_rejects_a_parent_id_that_does_not_exist() {
    let fx = Fixture::open();
    let store = fx.store_local();
    let err = store
        .create_tag(fx.account_id, "work", None, None, Some(999_999))
        .await
        .expect_err("a nonexistent parent_id must be rejected");
    assert_eq!(err.reason(), ErrorReason::InvalidArgument);
}

#[tokio::test]
async fn create_tag_rejects_a_parent_id_from_a_different_account() {
    let fx = Fixture::open();
    let store = fx.store_local();
    let other_account_id = fx
        .db
        .with_write(|conn| {
            crate::repo::insert_account(
                conn,
                &crate::repo::NewAccount {
                    name: "other-account".to_owned(),
                    ..Default::default()
                },
            )
        })
        .unwrap();
    let foreign_parent = store
        .create_tag(
            other_account_id,
            "work",
            None,
            Some(TagSyncMode::Local),
            None,
        )
        .await
        .unwrap();

    let err = store
        .create_tag(
            fx.account_id,
            "project/alpha",
            None,
            None,
            Some(foreign_parent.id),
        )
        .await
        .expect_err("a parent_id from another account must be rejected");
    assert_eq!(err.reason(), ErrorReason::InvalidArgument);
}

// ---------------------------------------------------------------------------
// add_tag / remove_tag (local)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn add_tag_creates_unknown_names_and_applies_them() {
    let fx = Fixture::open();
    let store = fx.store_local();
    let message_id = fx.seed_message(1);

    let applications = store
        .add_tag(
            Target::Message(message_id),
            &["work".to_owned(), "urgent".to_owned()],
            TagSource::User,
        )
        .await
        .unwrap();
    assert_eq!(applications.len(), 2);
    assert_eq!(
        effective_tag_names(&fx.db, message_id),
        vec!["urgent".to_owned(), "work".to_owned()]
    );
}

#[tokio::test]
async fn add_tag_is_idempotent() {
    let fx = Fixture::open();
    let store = fx.store_local();
    let message_id = fx.seed_message(1);

    let first = store
        .add_tag(
            Target::Message(message_id),
            &["work".to_owned()],
            TagSource::User,
        )
        .await
        .unwrap();
    assert_eq!(first.len(), 1);
    let second = store
        .add_tag(
            Target::Message(message_id),
            &["work".to_owned()],
            TagSource::User,
        )
        .await
        .unwrap();
    assert!(second.is_empty(), "a duplicate apply creates no new row");
    assert_eq!(
        effective_tag_names(&fx.db, message_id),
        vec!["work".to_owned()]
    );
}

#[tokio::test]
async fn a_thread_level_tag_covers_every_current_member() {
    let fx = Fixture::open();
    let store = fx.store_local();
    let (thread_id, messages) = fx.seed_thread_with_messages(&[1, 2, 3]);

    store
        .add_tag(
            Target::Thread(thread_id),
            &["announcement".to_owned()],
            TagSource::User,
        )
        .await
        .unwrap();

    for message_id in messages {
        assert_eq!(
            effective_tag_names(&fx.db, message_id),
            vec!["announcement".to_owned()],
            "every current thread member should see the thread-level tag"
        );
    }
}

#[tokio::test]
async fn remove_tag_removes_the_application_and_ignores_unknown_names() {
    let fx = Fixture::open();
    let store = fx.store_local();
    let message_id = fx.seed_message(1);
    store
        .add_tag(
            Target::Message(message_id),
            &["work".to_owned()],
            TagSource::User,
        )
        .await
        .unwrap();

    let removed = store
        .remove_tag(
            Target::Message(message_id),
            &["work".to_owned(), "does-not-exist".to_owned()],
        )
        .await
        .unwrap();
    assert_eq!(removed, 1);
    assert!(effective_tag_names(&fx.db, message_id).is_empty());
}

// ---------------------------------------------------------------------------
// IMAP round-trip and the `auto` downgrade
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_imap_tag_calls_store_keyword_with_the_derived_wire_keyword() {
    let fx = Fixture::open();
    let imap = Arc::new(RecordingImap::default());
    let store = fx.store(imap.clone() as Arc<dyn ImapMutator>);
    let message_id = fx.seed_message(5);
    store
        .create_tag(fx.account_id, "work", None, Some(TagSyncMode::Imap), None)
        .await
        .unwrap();

    store
        .add_tag(
            Target::Message(message_id),
            &["work".to_owned()],
            TagSource::User,
        )
        .await
        .unwrap();

    let calls = imap.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].keyword, "rmail/work");
    assert_eq!(calls[0].uids, vec![5]);
    assert!(calls[0].add);
}

#[tokio::test]
async fn a_strict_imap_tag_apply_failure_leaves_nothing_applied_locally() {
    let fx = Fixture::open();
    let imap = Arc::new(RecordingImap::failing());
    let store = fx.store(imap.clone() as Arc<dyn ImapMutator>);
    let message_id = fx.seed_message(5);
    store
        .create_tag(fx.account_id, "work", None, Some(TagSyncMode::Imap), None)
        .await
        .unwrap();

    let err = store
        .add_tag(
            Target::Message(message_id),
            &["work".to_owned()],
            TagSource::User,
        )
        .await
        .expect_err("a refused STORE under sync_mode=imap must fail the whole apply");
    assert_eq!(err.reason(), ErrorReason::Unavailable);
    assert!(
        effective_tag_names(&fx.db, message_id).is_empty(),
        "IMAP failed first, so nothing local should have changed"
    );
}

#[tokio::test]
async fn an_auto_tag_downgrades_to_local_and_still_applies() {
    let fx = Fixture::open();
    let imap = Arc::new(RecordingImap::failing());
    let store = fx.store(imap.clone() as Arc<dyn ImapMutator>);
    let message_id = fx.seed_message(5);
    let tag = store
        .create_tag(fx.account_id, "work", None, Some(TagSyncMode::Auto), None)
        .await
        .unwrap();

    let applications = store
        .add_tag(
            Target::Message(message_id),
            &["work".to_owned()],
            TagSource::User,
        )
        .await
        .unwrap();
    assert_eq!(
        applications.len(),
        1,
        "an auto tag still applies locally on refusal"
    );
    assert_eq!(
        effective_tag_names(&fx.db, message_id),
        vec!["work".to_owned()]
    );

    let reloaded = store
        .get_or_create_tag(fx.account_id, "work")
        .await
        .unwrap();
    assert_eq!(
        reloaded.sync_mode,
        TagSyncMode::Local,
        "the tag must be persisted as downgraded"
    );
    assert_eq!(reloaded.id, tag.id);
    assert_eq!(
        imap.calls().len(),
        1,
        "the wire round-trip really was attempted"
    );
}

// ---------------------------------------------------------------------------
// bulk_tag
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bulk_tag_by_message_ids_is_one_coalesced_store_call() {
    let fx = Fixture::open();
    let imap = Arc::new(RecordingImap::default());
    let store = fx.store(imap.clone() as Arc<dyn ImapMutator>);
    store
        .create_tag(fx.account_id, "urgent", None, Some(TagSyncMode::Imap), None)
        .await
        .unwrap();
    let ids = vec![fx.seed_message(1), fx.seed_message(2), fx.seed_message(3)];

    let outcome = store
        .bulk_tag(
            fx.account_id,
            BulkSelector::MessageIds(ids.clone()),
            &["urgent".to_owned()],
        )
        .await
        .unwrap();
    assert_eq!(outcome.message_count, 3);
    assert_eq!(outcome.applied, 3);

    let calls = imap.calls();
    assert_eq!(
        calls.len(),
        1,
        "three messages sharing one mailbox must coalesce into one STORE call, got: {calls:?}"
    );
    let mut uids = calls[0].uids.clone();
    uids.sort_unstable();
    assert_eq!(uids, vec![1, 2, 3]);

    for id in ids {
        assert_eq!(effective_tag_names(&fx.db, id), vec!["urgent".to_owned()]);
    }
}

#[tokio::test]
async fn bulk_tag_by_message_ids_drops_ids_from_a_different_account() {
    // A message id belonging to another account must never create a tag
    // under this call's `account_id`, and must never reach `apply_wire`
    // (which would resolve *that* message's own account and STORE against
    // a server this call was never authorized to touch).
    let fx = Fixture::open();
    let imap = Arc::new(RecordingImap::default());
    let store = fx.store(imap.clone() as Arc<dyn ImapMutator>);
    store
        .create_tag(fx.account_id, "urgent", None, Some(TagSyncMode::Imap), None)
        .await
        .unwrap();
    let mine = fx.seed_message(1);

    let other_account_id = fx
        .db
        .with_write(|conn| {
            crate::repo::insert_account(
                conn,
                &crate::repo::NewAccount {
                    name: "other-account".to_owned(),
                    ..Default::default()
                },
            )
        })
        .unwrap();
    let other_mailbox_id = fx
        .db
        .with_write(move |conn| {
            crate::repo::insert_mailbox(
                conn,
                &crate::repo::NewMailbox {
                    account_id: other_account_id,
                    name: "INBOX".to_owned(),
                    ..Default::default()
                },
            )
        })
        .unwrap();
    let foreign = fx
        .db
        .with_write(move |conn| {
            crate::repo::insert_message(
                conn,
                &crate::repo::NewMessage {
                    account_id: other_account_id,
                    mailbox_id: other_mailbox_id,
                    uid: 1,
                    uidvalidity: 1,
                    ..Default::default()
                },
            )
        })
        .unwrap();

    let outcome = store
        .bulk_tag(
            fx.account_id,
            BulkSelector::MessageIds(vec![mine, foreign]),
            &["urgent".to_owned()],
        )
        .await
        .unwrap();
    assert_eq!(
        outcome.message_count, 1,
        "the foreign id must be dropped, not counted"
    );
    assert_eq!(effective_tag_names(&fx.db, mine), vec!["urgent".to_owned()]);
    assert!(
        effective_tag_names(&fx.db, foreign).is_empty(),
        "the other account's message must not have been tagged"
    );

    let calls = imap.calls();
    assert_eq!(calls.len(), 1, "only the caller's own mailbox is touched");
    assert_eq!(calls[0].uids, vec![1]);
}

#[tokio::test]
async fn bulk_tag_by_query_resolves_via_the_query_compiler() {
    let fx = Fixture::open();
    let store = fx.store_local();
    let account_id = fx.account_id;
    let mailbox_id = fx.mailbox_id;
    let matching = fx
        .db
        .with_write(move |conn| {
            crate::repo::insert_message(
                conn,
                &crate::repo::NewMessage {
                    account_id,
                    mailbox_id,
                    uid: 10,
                    uidvalidity: 1,
                    from_addr: Some("billing@stripe.com".to_owned()),
                    ..Default::default()
                },
            )
        })
        .unwrap();
    let _other = fx.seed_message(11);

    let outcome = store
        .bulk_tag(
            fx.account_id,
            BulkSelector::Query("from:stripe".to_owned()),
            &["finance/receipt".to_owned()],
        )
        .await
        .unwrap();
    assert_eq!(outcome.message_count, 1);
    assert_eq!(
        effective_tag_names(&fx.db, matching),
        vec!["finance/receipt".to_owned()]
    );
}

// ---------------------------------------------------------------------------
// pending suggestions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resolve_suggestion_accept_applies_and_reject_keeps_the_row() {
    let fx = Fixture::open();
    let store = fx.store_local();
    let message_id = fx.seed_message(1);
    let tag = store
        .create_tag(
            fx.account_id,
            "finance/invoice",
            None,
            Some(TagSyncMode::Local),
            None,
        )
        .await
        .unwrap();
    let row_id = fx
        .db
        .with_write(move |conn| {
            repo::insert_message_tag(
                conn,
                &NewMessageTag {
                    tag_id: tag.id,
                    target: Target::Message(message_id),
                    source: TagSource::Ai,
                    state: TagState::Pending,
                    confidence: Some(0.81),
                    rationale: Some("mentions an invoice number".to_owned()),
                },
            )
        })
        .unwrap()
        .unwrap();

    let pending = store.list_pending_suggestions(message_id).await.unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].tag.name, "finance/invoice");
    assert_eq!(pending[0].message_tag.confidence, Some(0.81));

    store.resolve_suggestion(row_id, true).await.unwrap();
    assert_eq!(
        effective_tag_names(&fx.db, message_id),
        vec!["finance/invoice".to_owned()]
    );
    assert!(
        store
            .list_pending_suggestions(message_id)
            .await
            .unwrap()
            .is_empty(),
        "an accepted suggestion is no longer pending"
    );

    // Resolving again must fail rather than silently flip state a second
    // time.
    let err = store
        .resolve_suggestion(row_id, false)
        .await
        .expect_err("already-resolved suggestions must not resolve twice");
    assert_eq!(err.reason(), ErrorReason::FailedPrecondition);
}

#[tokio::test]
async fn resolve_suggestion_reject_does_not_apply_the_tag() {
    let fx = Fixture::open();
    let store = fx.store_local();
    let message_id = fx.seed_message(1);
    let tag = store
        .create_tag(
            fx.account_id,
            "newsletter",
            None,
            Some(TagSyncMode::Local),
            None,
        )
        .await
        .unwrap();
    let row_id = fx
        .db
        .with_write(move |conn| {
            repo::insert_message_tag(
                conn,
                &NewMessageTag {
                    tag_id: tag.id,
                    target: Target::Message(message_id),
                    source: TagSource::Ai,
                    state: TagState::Pending,
                    confidence: Some(0.5),
                    rationale: None,
                },
            )
        })
        .unwrap()
        .unwrap();

    store.resolve_suggestion(row_id, false).await.unwrap();
    assert!(effective_tag_names(&fx.db, message_id).is_empty());

    let row = fx
        .db
        .with_read(move |conn| repo::get_message_tag(conn, row_id))
        .unwrap()
        .unwrap();
    assert_eq!(
        row.state,
        TagState::Rejected,
        "the row is kept, not deleted"
    );
}

// ---------------------------------------------------------------------------
// import_imap_keywords
// ---------------------------------------------------------------------------

#[tokio::test]
async fn import_imap_keywords_creates_tags_with_source_imap() {
    let fx = Fixture::open();
    let store = fx.store_local();
    let message_id = fx.seed_message(1);
    for flag in ["rmail/work", "\\Flagged", "\\Seen"] {
        fx.db
            .with_write({
                let flag = flag.to_owned();
                move |conn| crate::repo::add_flag(conn, message_id, &flag)
            })
            .unwrap();
    }

    let imported = store.import_imap_keywords(message_id).await.unwrap();
    let mut names: Vec<&str> = imported.iter().map(|t| t.name.as_str()).collect();
    names.sort_unstable();
    assert_eq!(names, vec!["flagged", "work"]);

    let rows: Vec<(String, String)> = fx
        .db
        .with_read(move |conn| {
            let mut stmt = conn.prepare(
                "SELECT t.name, mt.source FROM message_tags mt
                 JOIN tags t ON t.id = mt.tag_id
                 WHERE mt.message_id = ?1 ORDER BY t.name",
            )?;
            let rows = stmt.query_map([message_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })
        .unwrap();
    assert_eq!(
        rows,
        vec![
            ("flagged".to_owned(), "imap".to_owned()),
            ("work".to_owned(), "imap".to_owned()),
        ]
    );
}

#[tokio::test]
async fn import_imap_keywords_is_idempotent() {
    let fx = Fixture::open();
    let store = fx.store_local();
    let message_id = fx.seed_message(1);
    fx.db
        .with_write(move |conn| crate::repo::add_flag(conn, message_id, "rmail/work"))
        .unwrap();

    let first = store.import_imap_keywords(message_id).await.unwrap();
    assert_eq!(first.len(), 1);
    let second = store.import_imap_keywords(message_id).await.unwrap();
    assert!(
        second.is_empty(),
        "re-importing unchanged flags applies nothing new"
    );
}

// ---------------------------------------------------------------------------
// list_tags
// ---------------------------------------------------------------------------

#[tokio::test]
async fn list_tags_reports_effective_counts() {
    let fx = Fixture::open();
    let store = fx.store_local();
    let message_id = fx.seed_message(1);
    store
        .add_tag(
            Target::Message(message_id),
            &["work".to_owned()],
            TagSource::User,
        )
        .await
        .unwrap();
    store
        .create_tag(fx.account_id, "empty", None, None, None)
        .await
        .unwrap();

    let tags = store.list_tags(fx.account_id).await.unwrap();
    assert_eq!(tags.len(), 2);
    let work = tags.iter().find(|t| t.tag.name == "work").unwrap();
    assert_eq!(work.message_count, 1);
    let empty = tags.iter().find(|t| t.tag.name == "empty").unwrap();
    assert_eq!(empty.message_count, 0);
}
