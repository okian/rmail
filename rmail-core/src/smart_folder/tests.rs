//! What task 35 owes for smart folders, proven against a real database:
//!
//! - membership is a **view** — recomputed from the predicate on every read,
//!   never served from the ledger, and never involving an IMAP mutation of
//!   any kind (no move, no copy, no delete, no flag replace);
//! - only **genuinely new** matches fire actions — a re-evaluation that
//!   finds unchanged membership fires nothing, and one that finds a single
//!   new message fires exactly once, for it;
//! - the three error paths the task names (an unparseable predicate, a
//!   duplicate name, an account that no longer exists) have defined
//!   behaviour;
//! - the evaluator follows the event log, so membership actions stay live
//!   across sync.
//!
//! The IMAP fake is the same lightweight recording double `tags::tests` uses
//! — see that file's own note for why the wire bytes are not re-derived
//! here.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::repo as ledger_repo;
use super::*;
use crate::config::{TagSyncMode, TagsConfig};
use crate::events::Retention;
use crate::imap::mutate::ImapMutator;
use crate::repo;

static COUNTER: AtomicU32 = AtomicU32::new(0);

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

struct Fixture {
    db: Database,
    path: PathBuf,
    events: EventLog,
    account_id: i64,
    mailbox_id: i64,
    next_uid: std::sync::atomic::AtomicI64,
}

impl Fixture {
    fn open() -> Self {
        let n = COUNTER.fetch_add(1, AtomicOrdering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-smart-folder-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", path.display())));
        }
        let db = Database::open(&path).expect("open temp db");
        let (account_id, mailbox_id) = db
            .with_write(move |conn| {
                let account_id = repo::insert_account(
                    conn,
                    &repo::NewAccount {
                        name: format!("acct-{n}"),
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
            .expect("seed account/mailbox");
        let events = EventLog::new(db.clone(), Retention::unlimited());
        Self {
            db,
            path,
            events,
            account_id,
            mailbox_id,
            next_uid: std::sync::atomic::AtomicI64::new(1),
        }
    }

    /// A store whose tags are `local` by default, so the IMAP double is only
    /// reached by a test that deliberately asks for a syncing tag.
    fn store(&self, imap: Arc<dyn ImapMutator>) -> SmartFolderStore {
        self.store_with_sync_mode(imap, TagSyncMode::Local)
    }

    fn store_with_sync_mode(
        &self,
        imap: Arc<dyn ImapMutator>,
        default_sync_mode: TagSyncMode,
    ) -> SmartFolderStore {
        let tags = TagStore::new(
            self.db.clone(),
            imap,
            TagsConfig {
                default_sync_mode,
                ..Default::default()
            },
        );
        SmartFolderStore::new(self.db.clone(), tags, self.events.clone())
    }

    /// The store used by every test that must observe *zero* IMAP traffic.
    fn store_no_imap(&self) -> SmartFolderStore {
        self.store(Arc::new(NoImap))
    }

    /// Insert a message from `from_addr`, optionally unread.
    fn seed_message(&self, from_addr: &str, unread: bool) -> i64 {
        let uid = self.next_uid.fetch_add(1, AtomicOrdering::Relaxed);
        let account_id = self.account_id;
        let mailbox_id = self.mailbox_id;
        let from_addr = from_addr.to_owned();
        let id = self
            .db
            .with_write(move |conn| {
                repo::insert_message(
                    conn,
                    &repo::NewMessage {
                        account_id,
                        mailbox_id,
                        uid,
                        uidvalidity: 1,
                        from_addr: Some(from_addr),
                        subject: Some(format!("message {uid}")),
                        ..Default::default()
                    },
                )
            })
            .expect("insert message");
        if !unread {
            self.set_seen(id);
        }
        id
    }

    /// Mark a message `\Seen`, i.e. remove it from an `is:unread` folder.
    fn set_seen(&self, message_id: i64) {
        self.db
            .with_write(move |conn| {
                conn.execute(
                    "INSERT INTO flags (message_id, flag) VALUES (?1, '\\Seen')",
                    [message_id],
                )?;
                Ok(())
            })
            .expect("set \\Seen");
    }

    fn clear_seen(&self, message_id: i64) {
        self.db
            .with_write(move |conn| {
                conn.execute(
                    "DELETE FROM flags WHERE message_id = ?1 AND flag = '\\Seen'",
                    [message_id],
                )?;
                Ok(())
            })
            .expect("clear \\Seen");
    }

    fn ledger(&self, smart_folder_id: i64) -> Vec<(i64, Option<i64>)> {
        self.db
            .with_read(move |conn| ledger_repo::ledger(conn, smart_folder_id))
            .expect("read ledger")
    }

    /// Just the message ids the ledger holds, ascending.
    fn ledger_ids(&self, smart_folder_id: i64) -> Vec<i64> {
        self.ledger(smart_folder_id)
            .into_iter()
            .map(|(id, _)| id)
            .collect()
    }

    /// Every `RULE_FIRED` event in the log, as `(message_id, folder name)`.
    async fn rule_fired(&self) -> Vec<(Option<i64>, String)> {
        self.events
            .since(0, 1000)
            .await
            .expect("read events")
            .events
            .into_iter()
            .filter(|e| e.kind == EventKind::RuleFired)
            .map(|e| {
                let name = e
                    .payload
                    .get("smart_folder")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_owned();
                (e.message_id, name)
            })
            .collect()
    }

    /// Which messages currently carry `tag`, ascending.
    fn tagged(&self, tag: &str) -> Vec<i64> {
        let tag = tag.to_owned();
        self.db
            .with_read(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT mt.message_id FROM message_tags mt
                     JOIN tags t ON t.id = mt.tag_id
                     WHERE t.name = ?1 AND mt.state = 'applied' AND mt.message_id IS NOT NULL
                     ORDER BY mt.message_id",
                )?;
                let rows = stmt
                    .query_map([tag], |row| row.get::<_, i64>(0))?
                    .collect::<rusqlite::Result<Vec<i64>>>()?;
                Ok(rows)
            })
            .expect("read tagged")
    }

    /// The `source` column of every application of `tag`.
    fn tag_sources(&self, tag: &str) -> Vec<String> {
        let tag = tag.to_owned();
        self.db
            .with_read(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT mt.source FROM message_tags mt
                     JOIN tags t ON t.id = mt.tag_id
                     WHERE t.name = ?1 ORDER BY mt.message_id",
                )?;
                let rows = stmt
                    .query_map([tag], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<String>>>()?;
                Ok(rows)
            })
            .expect("read tag sources")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.path.display())));
        }
    }
}

fn spec(account_id: i64, name: &str, predicate: &str) -> NewSmartFolder {
    NewSmartFolder {
        account_id,
        name: name.to_owned(),
        predicate: predicate.to_owned(),
        auto_tag: None,
        notify: false,
        // The deterministic form task 35 shipped: no NL source, so
        // `validate_predicate` runs and free text stays refused.
        ..NewSmartFolder::default()
    }
}

fn token() -> CancellationToken {
    CancellationToken::new()
}

// ---------------------------------------------------------------------------
// IMAP doubles
// ---------------------------------------------------------------------------

/// Errors on every method. Any call at all fails the test that installed it.
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

/// Refuses every keyword `STORE` — a server that will not accept the tag an
/// `auto_tag` action is trying to apply. Under `sync_mode = imap` that is a
/// hard error, which is exactly what
/// `a_failed_auto_tag_leaves_the_ledger_unstamped_so_the_action_is_retried`
/// needs to observe.
#[derive(Debug, Default)]
struct RefusingImap;

#[async_trait]
impl ImapMutator for RefusingImap {
    async fn set_flags(&self, _: i64, _: &str, _: i64, _: i64, _: &[String]) -> Result<(), Error> {
        Err(Error::internal("RefusingImap: unexpected set_flags call"))
    }
    async fn move_message(&self, _: i64, _: &str, _: i64, _: i64, _: &str) -> Result<(), Error> {
        Err(Error::internal(
            "RefusingImap: unexpected move_message call",
        ))
    }
    async fn copy_message(&self, _: i64, _: &str, _: i64, _: i64, _: &str) -> Result<(), Error> {
        Err(Error::internal(
            "RefusingImap: unexpected copy_message call",
        ))
    }
    async fn delete_message(&self, _: i64, _: &str, _: i64, _: i64) -> Result<(), Error> {
        Err(Error::internal(
            "RefusingImap: unexpected delete_message call",
        ))
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
        Err(Error::unavailable("server refused the keyword STORE"))
    }
}

/// Records the *name* of every `ImapMutator` method that was called, so a
/// test can assert on the shape of the traffic rather than only its absence.
#[derive(Debug, Default)]
struct RecordingImap {
    calls: Mutex<Vec<String>>,
}

impl RecordingImap {
    fn calls(&self) -> Vec<String> {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn record(&self, name: &str) {
        self.calls
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(name.to_owned());
    }
}

#[async_trait]
impl ImapMutator for RecordingImap {
    async fn set_flags(&self, _: i64, _: &str, _: i64, _: i64, _: &[String]) -> Result<(), Error> {
        self.record("set_flags");
        Ok(())
    }
    async fn move_message(&self, _: i64, _: &str, _: i64, _: i64, _: &str) -> Result<(), Error> {
        self.record("move_message");
        Ok(())
    }
    async fn copy_message(&self, _: i64, _: &str, _: i64, _: i64, _: &str) -> Result<(), Error> {
        self.record("copy_message");
        Ok(())
    }
    async fn delete_message(&self, _: i64, _: &str, _: i64, _: i64) -> Result<(), Error> {
        self.record("delete_message");
        Ok(())
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
        self.record("store_keyword");
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Membership is a live view
// ---------------------------------------------------------------------------

#[tokio::test]
async fn members_are_recomputed_not_read_from_the_ledger() {
    // The load-bearing property: a message that arrives after the last
    // evaluation is a member immediately, with no evaluation in between and
    // nothing written anywhere. If `members` ever started reading
    // `smart_folder_matched`, this fails.
    let f = Fixture::open();
    let store = f.store_no_imap();
    let first = f.seed_message("billing@stripe.com", true);

    let folder = store
        .create(&spec(f.account_id, "Stripe", "from:stripe"))
        .await
        .expect("create");
    assert_eq!(
        store
            .members(folder.id, None, &token())
            .await
            .expect("members"),
        vec![first]
    );

    let second = f.seed_message("receipts@stripe.com", true);
    assert_eq!(
        store
            .members(folder.id, None, &token())
            .await
            .expect("members"),
        vec![first, second],
        "membership must be live without an intervening evaluation"
    );
    // ...and nothing was recorded for the new arrival: the ledger is the
    // action ledger, not the membership, and reading membership never writes
    // to it.
    assert_eq!(
        f.ledger_ids(folder.id),
        vec![first],
        "reading membership must not write to the ledger"
    );
}

#[tokio::test]
async fn a_message_that_stops_matching_leaves_membership_immediately() {
    let f = Fixture::open();
    let store = f.store_no_imap();
    let a = f.seed_message("a@example.com", true);
    let b = f.seed_message("b@example.com", true);
    let folder = store
        .create(&spec(f.account_id, "Unread", "is:unread"))
        .await
        .expect("create");
    assert_eq!(
        store
            .members(folder.id, None, &token())
            .await
            .expect("members"),
        vec![a, b]
    );

    f.set_seen(a);
    assert_eq!(
        store
            .members(folder.id, None, &token())
            .await
            .expect("members"),
        vec![b],
        "no mail moved on the server; the predicate simply stopped matching"
    );
}

#[tokio::test]
async fn evaluating_a_smart_folder_issues_no_imap_mutation() {
    // prd.md: membership stays live "without moving mail on the server".
    // `NoImap` errors on *every* `ImapMutator` method, so a create, a
    // members read, and a full evaluation with both actions enabled all
    // completing successfully is the proof that none of them was called.
    let f = Fixture::open();
    let store = f.store_no_imap();
    f.seed_message("billing@stripe.com", true);

    let folder = store
        .create(&NewSmartFolder {
            auto_tag: Some("finance".to_owned()),
            notify: true,
            ..spec(f.account_id, "Stripe", "from:stripe")
        })
        .await
        .expect("create must not touch IMAP");

    store
        .members(folder.id, None, &token())
        .await
        .expect("members");

    let newcomer = f.seed_message("receipts@stripe.com", true);
    let evaluation = store.evaluate(folder.id, &token()).await.expect("evaluate");
    assert_eq!(evaluation.entered, vec![newcomer]);
    assert_eq!(evaluation.tagged, 1);
    assert_eq!(evaluation.notified, 1);
}

#[tokio::test]
async fn a_smart_folder_never_moves_or_copies_mail_on_the_server() {
    // The one IMAP call a smart folder can indirectly cause is the keyword
    // `STORE` behind an `auto_tag` the operator configured to sync — the tag
    // round-trip they asked for. It must never become a move/copy/delete/
    // flag-replace, which is what "no mail is moved on the server" rules out.
    let f = Fixture::open();
    let imap = Arc::new(RecordingImap::default());
    let store = f.store_with_sync_mode(imap.clone(), TagSyncMode::Imap);
    f.seed_message("billing@stripe.com", true);

    let folder = store
        .create(&NewSmartFolder {
            auto_tag: Some("finance".to_owned()),
            notify: true,
            ..spec(f.account_id, "Stripe", "from:stripe")
        })
        .await
        .expect("create");
    assert!(
        imap.calls().is_empty(),
        "defining a folder must not touch IMAP at all, got {:?}",
        imap.calls()
    );

    f.seed_message("receipts@stripe.com", true);
    store.evaluate(folder.id, &token()).await.expect("evaluate");

    let calls = imap.calls();
    assert_eq!(
        calls,
        vec!["store_keyword".to_owned()],
        "only the configured tag's keyword STORE may reach IMAP"
    );
    for forbidden in [
        "move_message",
        "copy_message",
        "delete_message",
        "set_flags",
    ] {
        assert!(
            !calls.iter().any(|c| c == forbidden),
            "a smart folder must never call {forbidden}"
        );
    }
}

// ---------------------------------------------------------------------------
// Only genuinely new matches fire actions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn creating_a_folder_baselines_the_backlog_and_fires_nothing() {
    // Defining "everything from Stripe" over a mailbox that already holds a
    // year of Stripe mail must not notify a hundred times. The baseline is
    // recorded as already-fired, exactly like `HookDispatcher` seeding its
    // cursor at the head rather than the start of retention.
    let f = Fixture::open();
    let store = f.store_no_imap();
    let existing: Vec<i64> = (0..3)
        .map(|_| f.seed_message("billing@stripe.com", true))
        .collect();

    let folder = store
        .create(&NewSmartFolder {
            auto_tag: Some("finance".to_owned()),
            notify: true,
            ..spec(f.account_id, "Stripe", "from:stripe")
        })
        .await
        .expect("create");

    assert_eq!(
        store
            .members(folder.id, None, &token())
            .await
            .expect("members"),
        existing,
        "the backlog is visible in the folder..."
    );
    assert!(f.tagged("finance").is_empty(), "...but was not auto-tagged");
    assert!(f.rule_fired().await.is_empty(), "...and did not notify");
    assert!(
        f.ledger(folder.id).iter().all(|(_, fired)| fired.is_some()),
        "every baselined member is recorded as already-fired"
    );
}

#[tokio::test]
async fn re_evaluating_unchanged_membership_fires_nothing() {
    // This is the test most likely to catch a real bug: drop the ledger's
    // "already fired" bookkeeping and every one of these ten evaluations
    // notifies again for the same message.
    let f = Fixture::open();
    let store = f.store_no_imap();
    f.seed_message("billing@stripe.com", true);
    let folder = store
        .create(&NewSmartFolder {
            auto_tag: Some("finance".to_owned()),
            notify: true,
            ..spec(f.account_id, "Stripe", "from:stripe")
        })
        .await
        .expect("create");

    // One genuinely new arrival, so there is something in the ledger that
    // *has* fired — an all-baseline folder would pass this test vacuously.
    let newcomer = f.seed_message("receipts@stripe.com", true);
    let first = store.evaluate(folder.id, &token()).await.expect("evaluate");
    assert_eq!(first.entered, vec![newcomer]);
    assert_eq!(first.notified, 1);
    assert_eq!(first.tagged, 1);

    for round in 0..10 {
        let again = store.evaluate(folder.id, &token()).await.expect("evaluate");
        assert!(
            again.entered.is_empty() && again.departed.is_empty(),
            "round {round}: membership did not change"
        );
        assert_eq!(again.tagged, 0, "round {round}: no new tag applications");
        assert_eq!(again.notified, 0, "round {round}: no new notifications");
    }

    assert_eq!(
        f.rule_fired().await,
        vec![(Some(newcomer), "Stripe".to_owned())],
        "exactly one notification, for the one genuinely new message"
    );
    assert_eq!(f.tagged("finance"), vec![newcomer]);
}

#[tokio::test]
async fn one_new_message_fires_exactly_once_for_exactly_that_message() {
    let f = Fixture::open();
    let store = f.store_no_imap();
    let backlog = f.seed_message("billing@stripe.com", true);
    let folder = store
        .create(&NewSmartFolder {
            auto_tag: Some("finance".to_owned()),
            notify: true,
            ..spec(f.account_id, "Stripe", "from:stripe")
        })
        .await
        .expect("create");

    let newcomer = f.seed_message("receipts@stripe.com", true);
    let evaluation = store.evaluate(folder.id, &token()).await.expect("evaluate");

    assert_eq!(evaluation.members, 2);
    assert_eq!(evaluation.entered, vec![newcomer]);
    assert!(evaluation.departed.is_empty());
    assert_eq!(evaluation.tagged, 1);
    assert_eq!(evaluation.notified, 1);
    assert_eq!(
        f.rule_fired().await,
        vec![(Some(newcomer), "Stripe".to_owned())]
    );
    assert_eq!(
        f.tagged("finance"),
        vec![newcomer],
        "the backlog message must not be retroactively tagged, only {newcomer}"
    );
    assert_ne!(backlog, newcomer);
}

#[tokio::test]
async fn a_departing_member_is_reported_and_fires_nothing() {
    let f = Fixture::open();
    let store = f.store_no_imap();
    let a = f.seed_message("a@example.com", true);
    let b = f.seed_message("b@example.com", true);
    let folder = store
        .create(&NewSmartFolder {
            notify: true,
            ..spec(f.account_id, "Unread", "is:unread")
        })
        .await
        .expect("create");

    f.set_seen(a);
    let evaluation = store.evaluate(folder.id, &token()).await.expect("evaluate");
    assert_eq!(evaluation.departed, vec![a]);
    assert!(evaluation.entered.is_empty());
    assert_eq!(evaluation.notified, 0);
    assert_eq!(evaluation.members, 1);
    assert_eq!(
        f.ledger_ids(folder.id),
        vec![b],
        "the ledger is bounded by current membership"
    );
}

#[tokio::test]
async fn a_member_that_leaves_and_returns_is_new_again() {
    // Documented consequence of a ledger bounded by current membership,
    // pinned here so a future change to that policy is a deliberate one.
    let f = Fixture::open();
    let store = f.store_no_imap();
    let a = f.seed_message("a@example.com", true);
    let folder = store
        .create(&NewSmartFolder {
            notify: true,
            ..spec(f.account_id, "Unread", "is:unread")
        })
        .await
        .expect("create");

    f.set_seen(a);
    store.evaluate(folder.id, &token()).await.expect("departs");
    f.clear_seen(a);
    let back = store.evaluate(folder.id, &token()).await.expect("returns");

    assert_eq!(back.entered, vec![a]);
    assert_eq!(back.notified, 1);
    assert_eq!(f.rule_fired().await, vec![(Some(a), "Unread".to_owned())]);
}

#[tokio::test]
async fn an_unstamped_member_left_by_an_interrupted_run_fires_on_the_next_pass() {
    // The crash window: actions run before the ledger is stamped, so a
    // process that died in between must re-fire rather than silently
    // swallow the notification. Simulated by clearing `fired_at` — the exact
    // state an interrupted run leaves behind.
    let f = Fixture::open();
    let store = f.store_no_imap();
    let a = f.seed_message("billing@stripe.com", true);
    let folder = store
        .create(&NewSmartFolder {
            notify: true,
            ..spec(f.account_id, "Stripe", "from:stripe")
        })
        .await
        .expect("create");

    let folder_id = folder.id;
    f.db.with_write(move |conn| {
        conn.execute(
            "UPDATE smart_folder_matched SET fired_at = NULL WHERE smart_folder_id = ?1",
            [folder_id],
        )?;
        Ok(())
    })
    .expect("simulate an interrupted run");

    let evaluation = store.evaluate(folder.id, &token()).await.expect("evaluate");
    assert!(
        evaluation.entered.is_empty(),
        "it was already a member; only its actions were owed"
    );
    assert_eq!(evaluation.notified, 1);
    assert_eq!(f.rule_fired().await, vec![(Some(a), "Stripe".to_owned())]);

    // ...and it is stamped now, so it does not fire a third time.
    let again = store.evaluate(folder.id, &token()).await.expect("evaluate");
    assert_eq!(again.notified, 0);
}

#[tokio::test]
async fn two_concurrent_evaluations_fire_exactly_once() {
    // The exactly-once contract has to survive `EvaluateSmartFolder` and the
    // background evaluator landing on the same folder at the same moment —
    // both go through one shared `SmartFolderStore` in the daemon, and the
    // window between "reconcile claims the row" and "stamp fired_at" spans an
    // auto-tag round trip and an event append. Drop the per-folder lock in
    // `evaluate` and this notifies twice.
    let f = Fixture::open();
    let store = f.store_no_imap();
    f.seed_message("billing@stripe.com", true);
    let folder = store
        .create(&NewSmartFolder {
            auto_tag: Some("finance".to_owned()),
            notify: true,
            ..spec(f.account_id, "Stripe", "from:stripe")
        })
        .await
        .expect("create");

    let newcomer = f.seed_message("receipts@stripe.com", true);
    let a = store.clone();
    let b = store.clone();
    let cancel = token();
    let (first, second) = tokio::join!(
        a.evaluate(folder.id, &cancel),
        b.evaluate(folder.id, &cancel)
    );
    let first = first.expect("evaluate");
    let second = second.expect("evaluate");

    assert_eq!(
        first.notified + second.notified,
        1,
        "exactly one of the two evaluations may fire"
    );
    assert_eq!(first.entered.len() + second.entered.len(), 1);
    assert_eq!(
        f.rule_fired().await,
        vec![(Some(newcomer), "Stripe".to_owned())]
    );
    assert_eq!(f.tagged("finance"), vec![newcomer]);
}

#[tokio::test]
async fn a_failed_auto_tag_leaves_the_ledger_unstamped_so_the_action_is_retried() {
    // The documented failure contract: an action that errors must not stamp
    // the ledger, or the member it was owed for is silently skipped forever.
    // The failure is a genuine one — an `imap`-mode tag whose keyword `STORE`
    // the server refuses — not a hand-edited row.
    let f = Fixture::open();
    let refusing = f.store_with_sync_mode(Arc::new(RefusingImap), TagSyncMode::Imap);
    f.seed_message("billing@stripe.com", true);
    let folder = refusing
        .create(&NewSmartFolder {
            auto_tag: Some("finance".to_owned()),
            notify: true,
            ..spec(f.account_id, "Stripe", "from:stripe")
        })
        .await
        .expect("create");

    let newcomer = f.seed_message("receipts@stripe.com", true);
    let err = refusing
        .evaluate(folder.id, &token())
        .await
        .expect_err("the refused STORE must fail the evaluation");
    assert_eq!(err.reason(), ErrorReason::Unavailable);

    // Nothing fired, and the newcomer's row is still owed its actions.
    assert!(
        f.rule_fired().await.is_empty(),
        "notification must not run after the tag action failed"
    );
    assert_eq!(
        f.ledger(folder.id)
            .into_iter()
            .filter(|(_, fired)| fired.is_none())
            .map(|(id, _)| id)
            .collect::<Vec<_>>(),
        vec![newcomer],
        "the owed action must survive as an unstamped ledger row"
    );

    // A later pass with a working server pays the debt, once.
    let working = f.store_with_sync_mode(Arc::new(RecordingImap::default()), TagSyncMode::Imap);
    let retry = working.evaluate(folder.id, &token()).await.expect("retry");
    assert!(
        retry.entered.is_empty(),
        "it was already a member; only its actions were owed"
    );
    assert_eq!(retry.notified, 1);
    assert_eq!(f.tagged("finance"), vec![newcomer]);
    assert_eq!(
        f.rule_fired().await,
        vec![(Some(newcomer), "Stripe".to_owned())]
    );
}

#[tokio::test]
async fn an_owed_action_is_dropped_if_its_member_departs_before_the_retry() {
    // The documented consequence of a ledger bounded by current membership:
    // firing for a message the predicate no longer selects would auto-tag or
    // announce mail that is not in the folder. Pinned so a future change to
    // that policy is deliberate.
    let f = Fixture::open();
    let store = f.store_no_imap();
    let a = f.seed_message("a@example.com", true);
    let folder = store
        .create(&NewSmartFolder {
            notify: true,
            ..spec(f.account_id, "Unread", "is:unread")
        })
        .await
        .expect("create");

    let folder_id = folder.id;
    f.db.with_write(move |conn| {
        conn.execute(
            "UPDATE smart_folder_matched SET fired_at = NULL WHERE smart_folder_id = ?1",
            [folder_id],
        )?;
        Ok(())
    })
    .expect("simulate an owed action");

    f.set_seen(a);
    let evaluation = store.evaluate(folder.id, &token()).await.expect("evaluate");
    assert_eq!(evaluation.departed, vec![a]);
    assert_eq!(evaluation.notified, 0);
    assert!(f.rule_fired().await.is_empty());
    assert!(f.ledger_ids(folder.id).is_empty());
}

#[tokio::test]
async fn members_honours_a_limit() {
    let f = Fixture::open();
    let store = f.store_no_imap();
    let ids: Vec<i64> = (0..5)
        .map(|_| f.seed_message("billing@stripe.com", true))
        .collect();
    let folder = store
        .create(&spec(f.account_id, "Stripe", "from:stripe"))
        .await
        .expect("create");

    assert_eq!(
        store
            .members(folder.id, Some(2), &token())
            .await
            .expect("members"),
        ids[..2].to_vec()
    );
    assert_eq!(
        store
            .members(folder.id, Some(99), &token())
            .await
            .expect("members"),
        ids
    );
    // A limited read must never be mistaken for membership by an evaluation:
    // the folder still has five members and nothing departed.
    let evaluation = store.evaluate(folder.id, &token()).await.expect("evaluate");
    assert_eq!(evaluation.members, 5);
    assert!(evaluation.departed.is_empty());
}

#[tokio::test]
async fn a_member_expunged_between_the_scan_and_the_write_does_not_fail_the_folder() {
    // Membership is resolved on a read connection and reconciled on the
    // writer, so an expunge can land in between and leave an id in `current`
    // whose `messages` row is gone. A plain `INSERT` would take the foreign
    // key and abort the whole evaluation, stranding every *other* new member
    // until the next pass. Driving `reconcile` directly is the only way to
    // hold that window open deterministically.
    let f = Fixture::open();
    let store = f.store_no_imap();
    let baselined = f.seed_message("billing@stripe.com", true);
    let folder = store
        .create(&spec(f.account_id, "Stripe", "from:stripe"))
        .await
        .expect("create");
    let survivor = f.seed_message("receipts@stripe.com", true);

    let ghost = survivor + 10_000;
    let folder_id = folder.id;
    let current = vec![baselined, survivor, ghost];
    let reconciled =
        f.db.with_write(move |conn| {
            let tx = conn.transaction()?;
            let reconciled = ledger_repo::reconcile(&tx, folder_id, &current, false)?;
            tx.commit()?;
            Ok(reconciled)
        })
        .expect("a vanished member must not fail the whole reconciliation");

    assert_eq!(
        reconciled.entered,
        vec![survivor],
        "the ghost must not be reported as a new member"
    );
    assert_eq!(reconciled.pending, vec![survivor]);
    assert_eq!(
        f.ledger_ids(folder.id),
        vec![baselined, survivor],
        "the ledger must hold only ids that name real messages"
    );
}

#[tokio::test]
async fn an_auto_tag_is_recorded_as_rule_applied_not_user_applied() {
    // prd.md's tag model distinguishes who applied a tag, and task 57's
    // rule-learning pass reads exactly that column: a rule-applied tag
    // indistinguishable from a hand-applied one is training signal that
    // never happened.
    let f = Fixture::open();
    let store = f.store_no_imap();
    f.seed_message("billing@stripe.com", true);
    let folder = store
        .create(&NewSmartFolder {
            auto_tag: Some("finance".to_owned()),
            ..spec(f.account_id, "Stripe", "from:stripe")
        })
        .await
        .expect("create");
    f.seed_message("receipts@stripe.com", true);
    store.evaluate(folder.id, &token()).await.expect("evaluate");

    assert_eq!(f.tag_sources("finance"), vec!["rule".to_owned()]);
}

#[tokio::test]
async fn a_folder_with_no_actions_still_tracks_membership_and_publishes_nothing() {
    let f = Fixture::open();
    let store = f.store_no_imap();
    f.seed_message("billing@stripe.com", true);
    let folder = store
        .create(&spec(f.account_id, "Stripe", "from:stripe"))
        .await
        .expect("create");
    let newcomer = f.seed_message("receipts@stripe.com", true);

    let evaluation = store.evaluate(folder.id, &token()).await.expect("evaluate");
    assert_eq!(evaluation.entered, vec![newcomer]);
    assert_eq!(evaluation.notified, 0);
    assert_eq!(evaluation.tagged, 0);
    assert!(f.rule_fired().await.is_empty());
    assert!(
        f.ledger(folder.id).iter().all(|(_, fired)| fired.is_some()),
        "with nothing to fire, the ledger must still settle rather than \
         re-reporting the same member as pending forever"
    );
}

// ---------------------------------------------------------------------------
// Predicate validation (error path 1)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_predicate_with_free_text_is_rejected_rather_than_silently_widened() {
    // `from:stripe invoice` compiles to `from:stripe` alone under the
    // deterministic compiler. Accepting it would create a folder that
    // silently contains *every* Stripe message, re-confirmed correct on
    // every sync with nobody watching.
    let f = Fixture::open();
    let store = f.store_no_imap();
    let err = store
        .create(&spec(f.account_id, "Broken", "from:stripe invoice"))
        .await
        .expect_err("free text must be rejected");
    assert_eq!(err.reason(), ErrorReason::InvalidArgument);
    assert!(
        err.to_string().contains("invoice"),
        "the message must name the offending text: {err}"
    );
    assert!(store.list(f.account_id).await.expect("list").is_empty());
}

#[tokio::test]
async fn a_predicate_naming_an_unsupported_operator_is_rejected() {
    let f = Fixture::open();
    let store = f.store_no_imap();
    // `body:` parses fine but the deterministic membership compiler does not
    // back it, so it would be dropped from the predicate.
    let err = store
        .create(&spec(f.account_id, "Broken", "from:stripe body:refund"))
        .await
        .expect_err("unsupported operator must be rejected");
    assert_eq!(err.reason(), ErrorReason::InvalidArgument);
    assert!(
        err.to_string().contains("body:"),
        "the message must name the operator: {err}"
    );
}

#[tokio::test]
async fn an_empty_predicate_is_rejected() {
    let f = Fixture::open();
    let store = f.store_no_imap();
    for predicate in ["", "   "] {
        let err = store
            .create(&spec(f.account_id, "Broken", predicate))
            .await
            .expect_err("empty predicate");
        assert_eq!(err.reason(), ErrorReason::InvalidArgument);
    }
}

#[test]
fn validate_predicate_accepts_the_operators_membership_is_built_from() {
    for predicate in [
        "from:stripe",
        "is:unread",
        "-in:Spam",
        "tag:finance/receipt",
        "has:attachment",
        "from:stripe is:unread -in:Spam",
        "to:me@example.com cc:boss@example.com subject:invoice",
    ] {
        assert!(
            validate_predicate(predicate).is_ok(),
            "{predicate:?} should be a valid membership predicate"
        );
    }
}

#[test]
fn validate_predicate_rejects_a_predicate_that_would_match_the_whole_account() {
    // Belt and braces on the "applied == 0" guard: whatever the rejection
    // reason, nothing that compiles to the bare `account_id = ?` scope may
    // ever be accepted.
    for predicate in ["   ", "\"\"", "hello", "date:notarange"] {
        assert!(
            validate_predicate(predicate).is_err(),
            "{predicate:?} must not be accepted as a membership predicate"
        );
    }
}

// ---------------------------------------------------------------------------
// Duplicate name and missing account (error paths 2 and 3)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_duplicate_name_is_already_exists_and_leaves_the_original_untouched() {
    let f = Fixture::open();
    let store = f.store_no_imap();
    store
        .create(&spec(f.account_id, "Stripe", "from:stripe"))
        .await
        .expect("create");

    let err = store
        .create(&spec(f.account_id, "STRIPE", "from:aws"))
        .await
        .expect_err("duplicate name, case-insensitively");
    assert_eq!(err.reason(), ErrorReason::AlreadyExists);

    let existing = store.get(f.account_id, "Stripe").await.expect("get");
    assert_eq!(existing.predicate, "from:stripe");
    assert_eq!(store.list(f.account_id).await.expect("list").len(), 1);
}

#[tokio::test]
async fn creating_against_an_account_that_does_not_exist_is_not_found() {
    let f = Fixture::open();
    let err = f
        .store_no_imap()
        .create(&spec(9_999, "Stripe", "from:stripe"))
        .await
        .expect_err("no such account");
    assert_eq!(err.reason(), ErrorReason::NotFound);
    assert!(err.to_string().contains("9999"), "{err}");
}

#[tokio::test]
async fn a_folder_whose_account_was_deleted_is_gone_not_broken() {
    // `ON DELETE CASCADE` removes the folder with its account, so every
    // later call is a clean `NOT_FOUND` rather than an evaluation against a
    // dangling account id (which would resolve to the empty set and look
    // like "every member just departed").
    let f = Fixture::open();
    let store = f.store_no_imap();
    f.seed_message("billing@stripe.com", true);
    let folder = store
        .create(&NewSmartFolder {
            notify: true,
            ..spec(f.account_id, "Stripe", "from:stripe")
        })
        .await
        .expect("create");

    let account_id = f.account_id;
    f.db.with_write(move |conn| {
        conn.execute("DELETE FROM accounts WHERE id = ?1", [account_id])?;
        Ok(())
    })
    .expect("delete account");

    for reason in [
        store.evaluate(folder.id, &token()).await.err(),
        store.members(folder.id, None, &token()).await.err(),
        store.get_by_id(folder.id).await.err(),
    ] {
        assert_eq!(
            reason.map(|e| e.reason()),
            Some(ErrorReason::NotFound),
            "a folder whose account is gone must report NOT_FOUND"
        );
    }
    assert!(store.list(account_id).await.expect("list").is_empty());
    // Evaluating everything must skip it rather than fail the whole pass.
    assert!(store.evaluate_all(&token()).await.expect("pass").is_empty());
}

#[tokio::test]
async fn evaluating_a_folder_id_that_never_existed_is_not_found() {
    let f = Fixture::open();
    let err = f
        .store_no_imap()
        .evaluate(4_242, &token())
        .await
        .expect_err("no such folder");
    assert_eq!(err.reason(), ErrorReason::NotFound);
}

#[tokio::test]
async fn a_cancelled_evaluation_errors_rather_than_reporting_an_empty_folder() {
    // The dangerous failure: treating a cancelled scan as "no members" would
    // wipe the ledger and re-fire every member on the next pass.
    let f = Fixture::open();
    let store = f.store_no_imap();
    f.seed_message("billing@stripe.com", true);
    let folder = store
        .create(&NewSmartFolder {
            notify: true,
            ..spec(f.account_id, "Stripe", "from:stripe")
        })
        .await
        .expect("create");

    let cancel = token();
    cancel.cancel();
    let err = store
        .evaluate(folder.id, &cancel)
        .await
        .expect_err("a cancelled evaluation must not succeed");
    assert_eq!(err.reason(), ErrorReason::Unavailable);
    assert_eq!(
        f.ledger(folder.id).len(),
        1,
        "the ledger must survive a cancelled evaluation untouched"
    );
}

// ---------------------------------------------------------------------------
// The evaluator follows sync
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_evaluator_re_evaluates_accounts_that_saw_an_event() {
    // "Re-evaluated on each sync": a `NEW_MAIL` event for the account is the
    // trigger; the evaluation itself re-reads current state.
    let f = Fixture::open();
    let store = f.store_no_imap();
    f.seed_message("billing@stripe.com", true);
    let folder = store
        .create(&NewSmartFolder {
            auto_tag: Some("finance".to_owned()),
            notify: true,
            ..spec(f.account_id, "Stripe", "from:stripe")
        })
        .await
        .expect("create");

    let evaluator = SmartFolderEvaluator::new(store.clone(), f.events.clone());
    // A first tick with a quiet log seeds the cursor and does nothing.
    let quiet = evaluator.tick(&token()).await.expect("tick");
    assert_eq!(quiet, EvaluatorReport::default());

    let newcomer = f.seed_message("receipts@stripe.com", true);
    f.events
        .append(
            NewEvent::new(EventKind::NewMail)
                .account(f.account_id)
                .message(newcomer),
        )
        .await
        .expect("append");

    let report = evaluator.tick(&token()).await.expect("tick");
    assert_eq!(report.folders, 1);
    assert_eq!(report.entered, 1);
    assert_eq!(report.tagged, 1);
    assert_eq!(report.notified, 1);
    assert_eq!(f.tagged("finance"), vec![newcomer]);

    // A second tick with no further events re-evaluates nothing at all.
    // The notification the tick above published is itself an account-scoped
    // event, so the next tick answers it with one more evaluation of the same
    // account (see the module docs — it terminates rather than looping). That
    // pass must find nothing new and, crucially, must fire nothing.
    let settle = evaluator.tick(&token()).await.expect("tick");
    assert_eq!(settle.entered, 0);
    assert_eq!(settle.tagged, 0);
    assert_eq!(settle.notified, 0);
    // ...and the sequence really does stop: with no new events, nothing is
    // even re-evaluated.
    let idle = evaluator.tick(&token()).await.expect("tick");
    assert_eq!(idle, EvaluatorReport::default());
    assert_eq!(
        f.rule_fired()
            .await
            .into_iter()
            .filter(|(id, _)| *id == Some(newcomer))
            .count(),
        1,
        "the trigger must not multiply notifications"
    );

    let _ = folder;
}

#[tokio::test]
async fn the_evaluator_ignores_events_for_other_accounts() {
    let f = Fixture::open();
    let store = f.store_no_imap();
    f.seed_message("billing@stripe.com", true);
    store
        .create(&NewSmartFolder {
            notify: true,
            ..spec(f.account_id, "Stripe", "from:stripe")
        })
        .await
        .expect("create");
    let other =
        f.db.with_write(|conn| {
            repo::insert_account(
                conn,
                &repo::NewAccount {
                    name: "Other".to_owned(),
                    ..Default::default()
                },
            )
        })
        .expect("second account");

    let evaluator = SmartFolderEvaluator::new(store, f.events.clone());
    evaluator.tick(&token()).await.expect("seed");
    f.seed_message("receipts@stripe.com", true);
    f.events
        .append(NewEvent::new(EventKind::NewMail).account(other))
        .await
        .expect("append");

    let report = evaluator.tick(&token()).await.expect("tick");
    assert_eq!(report.folders, 0, "no folder in the account that changed");
    assert!(f.rule_fired().await.is_empty());
}

#[tokio::test]
async fn evaluate_account_covers_every_folder_in_it() {
    let f = Fixture::open();
    let store = f.store_no_imap();
    let stripe = f.seed_message("billing@stripe.com", false);
    let unread = f.seed_message("a@example.com", true);
    store
        .create(&spec(f.account_id, "Stripe", "from:stripe"))
        .await
        .expect("create");
    store
        .create(&spec(f.account_id, "Unread", "is:unread"))
        .await
        .expect("create");

    let evaluations = store
        .evaluate_account(f.account_id, &token())
        .await
        .expect("evaluate account");
    assert_eq!(evaluations.len(), 2);
    let members: Vec<usize> = evaluations.iter().map(|e| e.members).collect();
    assert_eq!(members, vec![1, 1]);
    assert_ne!(stripe, unread);
}

#[tokio::test]
async fn the_spawned_evaluator_brings_folders_current_at_boot_and_then_follows_the_log() {
    let f = Fixture::open();
    let store = f.store_no_imap();
    f.seed_message("billing@stripe.com", true);
    let folder = store
        .create(&NewSmartFolder {
            notify: true,
            ..spec(f.account_id, "Stripe", "from:stripe")
        })
        .await
        .expect("create");

    // Mail that arrived while the daemon was down: no event will ever be
    // published for it, so only the boot pass can catch it.
    let while_down = f.seed_message("receipts@stripe.com", true);

    let cancel = token();
    let handle = SmartFolderEvaluator::new(store.clone(), f.events.clone())
        .with_tick_interval(std::time::Duration::from_millis(20))
        .spawn(cancel.clone())
        .await;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        if f.rule_fired()
            .await
            .iter()
            .any(|(id, _)| *id == Some(while_down))
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the boot pass never fired for mail that arrived while the daemon was down"
        );
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }

    cancel.cancel();
    handle.await.expect("evaluator task joins");
    let _ = folder;
}

#[tokio::test]
async fn a_retention_gap_recovers_by_re_reading_state_not_replaying_history() {
    // The evaluator's cursor can fall off the end of a pruned log. Recovery
    // is a full pass over current state, not a replay: an event is only ever
    // a "something changed" trigger here, so there is no history to lose.
    let f = Fixture::open();
    let store = f.store_no_imap();
    f.seed_message("billing@stripe.com", true);
    let folder = store
        .create(&NewSmartFolder {
            notify: true,
            ..spec(f.account_id, "Stripe", "from:stripe")
        })
        .await
        .expect("create");

    // Seed the evaluator's cursor at the log's current head...
    f.events
        .append(NewEvent::new(EventKind::SyncState).account(f.account_id))
        .await
        .expect("append");
    let evaluator = SmartFolderEvaluator::new(store.clone(), f.events.clone());
    evaluator.tick(&token()).await.expect("seed the cursor");

    // ...append several more, prune the whole log out from under it, and only
    // then push the head forward again. That ordering is what actually
    // produces a gap: `EventLog::since` treats a cursor of `oldest - 1` as
    // exactly current with the floor, so the pruned span has to be strictly
    // more than one event wide before the cursor is provably behind it.
    for _ in 0..3 {
        f.events
            .append(NewEvent::new(EventKind::SyncState).account(f.account_id))
            .await
            .expect("append");
    }
    EventLog::new(
        f.db.clone(),
        Retention {
            max_rows: Some(0),
            max_age: None,
        },
    )
    .prune()
    .await
    .expect("prune");
    let newcomer = f.seed_message("receipts@stripe.com", true);
    for _ in 0..2 {
        f.events
            .append(NewEvent::new(EventKind::SyncState).account(f.account_id))
            .await
            .expect("append");
    }

    let report = evaluator
        .tick(&token())
        .await
        .expect("a retention gap is recovered from, not returned");
    assert_eq!(report.folders, 1, "recovery re-evaluates every folder");
    assert_eq!(report.entered, 1);
    assert_eq!(report.notified, 1);
    assert_eq!(
        f.rule_fired().await,
        vec![(Some(newcomer), "Stripe".to_owned())]
    );

    // And the cursor is current again: the next tick reads forward from the
    // head it recovered to (finding only the notification it just published,
    // which settles to a no-op pass), and the one after that is silent.
    let settle = evaluator.tick(&token()).await.expect("tick");
    assert_eq!(settle.entered, 0);
    assert_eq!(settle.notified, 0);
    assert_eq!(
        evaluator.tick(&token()).await.expect("tick"),
        EvaluatorReport::default()
    );
    let _ = folder;
}

// ---------------------------------------------------------------------------
// Task 58: NL-compiled hybrid plans
// ---------------------------------------------------------------------------
//
// What these owe, on top of task 35's deterministic guarantees above:
//
// - free text is *accepted* on the hybrid path and becomes a real constraint
//   (the FTS arm), rather than being silently dropped the way the operator
//   compiler would drop it;
// - an operator the membership compiler cannot enforce is still refused, so
//   the model gets no looser grammar than a human;
// - a plan whose only enforceable arm would have been an embedding that could
//   not be produced is refused rather than stored as a folder holding the
//   account — the invariant this whole module exists for, reached from the
//   direction only task 58 opens up;
// - the two text arms *union*, and the dense arm gates rather than merely
//   ranking;
// - a stale query vector degrades the dense arm to nothing without making the
//   folder unreadable.

use crate::embed::hash::HashEmbedder;
use crate::embed::{Embedder, Embedding};
use crate::index::fts::FtsIndex;
use crate::index::semantic::{SemanticIndex, VECTOR_DIM};

impl Fixture {
    /// Give a message indexable text and put it in the lexical index — what
    /// the FTS arm of a hybrid plan reads.
    async fn index_text(&self, message_id: i64, body: &str) {
        let body = body.to_owned();
        let chars = i64::try_from(body.chars().count()).unwrap_or(i64::MAX);
        self.db
            .write(move |conn| {
                conn.execute(
                    "INSERT INTO index_content
                         (message_id, part, text, chars, content_hash, extractor)
                     VALUES (?1, 'body', ?2, ?3, X'00', 'test')",
                    rusqlite::params![message_id, body, chars],
                )?;
                Ok(())
            })
            .await
            .expect("seed index_content");
        let fts = FtsIndex::new(self.db.clone(), crate::config::Bm25Weights::default());
        fts.index_message(message_id).await.expect("index message");
    }

    /// Chunk and embed a message with `embedder`, so the dense arm has
    /// something to find.
    async fn index_semantic(&self, message_id: i64, embedder: &Arc<dyn Embedder>) {
        let index = SemanticIndex::new(
            self.db.clone(),
            Arc::clone(embedder),
            &crate::config::IndexSemanticConfig::default(),
        );
        index
            .index_message(message_id)
            .await
            .expect("semantic index");
    }
}

fn test_embedder() -> Arc<dyn Embedder> {
    // Deterministic and offline: the same text always produces the same
    // vector, which is what makes a similarity floor assertable at all.
    Arc::new(HashEmbedder::new(VECTOR_DIM))
}

/// A spec for a folder compiled from English, with `predicate` standing in
/// for whatever the compiler produced.
fn nl_spec(account_id: i64, name: &str, predicate: &str) -> NewSmartFolder {
    NewSmartFolder {
        account_id,
        name: name.to_owned(),
        predicate: predicate.to_owned(),
        nl_source: Some("what the user actually said".to_owned()),
        compiled_model: Some("test-compile-model".to_owned()),
        ..NewSmartFolder::default()
    }
}

#[test]
fn a_hybrid_predicate_accepts_free_text_and_a_deterministic_one_still_does_not() {
    // The one rule that differs between the two paths. Everything free text
    // could mean is enforceable on the hybrid path and nothing of it is on the
    // deterministic one, which is exactly why they are two functions.
    assert!(validate_hybrid_predicate("from:stripe invoice").is_ok());
    assert!(validate_hybrid_predicate("lease renewal").is_ok());
    assert_eq!(
        validate_predicate("from:stripe invoice")
            .expect_err("free text is not a deterministic predicate")
            .reason(),
        ErrorReason::InvalidArgument
    );
}

#[test]
fn a_hybrid_predicate_still_refuses_an_unenforceable_operator() {
    // `larger:` is a perfectly good *search* operator that the membership
    // compiler cannot express. Accepting it because a model wrote it would
    // define a folder that silently ignores the constraint and holds
    // everything else the predicate names.
    let error = validate_hybrid_predicate("larger:10mb lease")
        .expect_err("an unenforceable operator must be refused on both paths");
    assert_eq!(error.reason(), ErrorReason::InvalidArgument);
    assert!(
        error.to_string().contains("larger:"),
        "the rejection must name the operator: {error}"
    );
}

#[test]
fn a_hybrid_predicate_that_parses_to_nothing_is_refused() {
    for predicate in ["", "   ", "\"\""] {
        let error = validate_hybrid_predicate(predicate)
            .expect_err("a predicate that constrains nothing must be refused");
        assert_eq!(error.reason(), ErrorReason::InvalidArgument);
    }
    let long = "x ".repeat(MAX_QUERY_LEN);
    assert_eq!(
        validate_hybrid_predicate(&long)
            .expect_err("an over-long predicate must be refused")
            .reason(),
        ErrorReason::InvalidArgument
    );
}

#[tokio::test]
async fn free_text_in_a_hybrid_folder_actually_gates_membership() {
    // The whole point of the hybrid path: `from:stripe invoice` must mean
    // "from Stripe AND about invoices", not "from Stripe" with the rest
    // quietly dropped. Without the lexical arm this folder would hold both
    // messages.
    let f = Fixture::open();
    let store = f.store_no_imap();
    let about_invoice = f.seed_message("billing@stripe.com", true);
    let about_outage = f.seed_message("status@stripe.com", true);
    f.index_text(about_invoice, "your invoice for June is attached")
        .await;
    f.index_text(about_outage, "we had a brief outage this morning")
        .await;

    let folder = store
        .create(&nl_spec(
            f.account_id,
            "stripe-invoices",
            "from:stripe invoice",
        ))
        .await
        .expect("create");

    let members = store
        .members(folder.id, None, &token())
        .await
        .expect("members");
    assert_eq!(members, vec![about_invoice]);
}

#[tokio::test]
async fn a_hybrid_folder_with_no_enforceable_arm_is_refused() {
    // The dangerous case task 58 introduces and nothing else can reach: a
    // compiled plan whose only constraint was going to be an embedding, on a
    // daemon whose embedder was unavailable. Storing it would define a folder
    // holding every message in the account, re-confirmed on every sync.
    let f = Fixture::open();
    let store = f.store_no_imap();
    let seen = f.seed_message("anyone@example.com", true);

    // `~lease` forces the term semantic, so it yields no lexical arm; with no
    // vector there is nothing left.
    let error = store
        .create(&nl_spec(f.account_id, "unconstrained", "~lease"))
        .await
        .expect_err("a plan with no enforceable arm must be refused");
    assert_eq!(error.reason(), ErrorReason::InvalidArgument);
    assert!(
        error.to_string().contains("every message"),
        "the rejection must say what it is preventing: {error}"
    );

    // Nothing was stored, so nothing is holding the account.
    assert!(store.list(f.account_id).await.expect("list").is_empty());
    let _ = seen;
}

#[tokio::test]
async fn a_query_vector_without_its_model_is_refused() {
    // Either half alone is a caller bug that would present as a folder whose
    // dense arm silently never fires — the failure this pair check exists to
    // make impossible.
    let f = Fixture::open();
    let store = f.store_no_imap();
    let mut spec = nl_spec(f.account_id, "half-set", "from:stripe lease");
    spec.query_vector = Some(Embedding::new(vec![0.5; VECTOR_DIM]));

    let error = store
        .create(&spec)
        .await
        .expect_err("a vector with no model must be refused");
    assert_eq!(error.reason(), ErrorReason::InvalidArgument);
}

#[tokio::test]
async fn the_dense_arm_admits_a_paraphrase_the_lexical_arm_misses() {
    // "hybrid" earning its name: the folder's free text appears verbatim in
    // one message and not at all in the other, and the second is a member
    // anyway because its embedding is what the query was built from.
    let f = Fixture::open();
    let store = f.store_no_imap();
    let embedder = test_embedder();

    let verbatim = f.seed_message("landlord@example.com", true);
    let paraphrase = f.seed_message("landlord@example.com", true);
    let unrelated = f.seed_message("landlord@example.com", true);
    f.index_text(verbatim, "lease renewal terms").await;
    f.index_text(paraphrase, "tenancy agreement extension")
        .await;
    f.index_text(unrelated, "the parking barrier is broken again")
        .await;
    for id in [verbatim, paraphrase, unrelated] {
        f.index_semantic(id, &embedder).await;
    }

    let vector = embedder
        .embed(&["tenancy agreement extension".to_owned()])
        .await
        .expect("embed")
        .remove(0);
    let mut spec = nl_spec(f.account_id, "lease", "from:landlord lease renewal");
    spec.query_vector = Some(vector);
    spec.vector_model = Some(embedder.model().to_owned());
    // A floor just under an exact hit, so the identical text clears it and
    // unrelated text does not.
    spec.min_similarity = Some(0.99);

    let folder = store.create(&spec).await.expect("create");
    let members = store
        .members(folder.id, None, &token())
        .await
        .expect("members");

    assert!(
        members.contains(&verbatim),
        "the lexical arm must still admit the verbatim match: {members:?}"
    );
    assert!(
        members.contains(&paraphrase),
        "the dense arm must admit a message the lexical arm cannot: {members:?}"
    );
    assert!(
        !members.contains(&unrelated),
        "the dense arm must gate, not merely rank: {members:?}"
    );
}

#[tokio::test]
async fn a_stale_query_vector_degrades_the_dense_arm_without_breaking_the_folder() {
    // A re-index under a different model leaves every stored vector pointing
    // at a space the index no longer holds. The join on `vector_model` is what
    // turns that into "this arm finds nothing" rather than "these cosines mean
    // nothing and still sort".
    let f = Fixture::open();
    let store = f.store_no_imap();
    let embedder = test_embedder();
    let message = f.seed_message("landlord@example.com", true);
    f.index_text(message, "lease renewal terms").await;
    f.index_semantic(message, &embedder).await;

    let vector = embedder
        .embed(&["lease renewal terms".to_owned()])
        .await
        .expect("embed")
        .remove(0);
    let mut spec = nl_spec(f.account_id, "lease", "from:landlord lease");
    spec.query_vector = Some(vector);
    spec.vector_model = Some("a-model-this-index-was-never-built-with".to_owned());
    spec.min_similarity = Some(0.0);

    let folder = store.create(&spec).await.expect("create");
    let members = store
        .members(folder.id, None, &token())
        .await
        .expect("members");

    // The lexical arm still answers, so the folder is readable; what the stale
    // vector cost is recall, which is the honest degradation.
    assert_eq!(members, vec![message]);
}

#[tokio::test]
async fn a_dense_arm_that_matches_nothing_does_not_widen_the_folder() {
    // The single most dangerous mistake available in this design: dropping an
    // arm whose input came back empty, leaving `WHERE <hard filters>` — which
    // for a folder defined by meaning is "every message from this sender".
    let f = Fixture::open();
    let store = f.store_no_imap();
    let embedder = test_embedder();
    let indexed = f.seed_message("landlord@example.com", true);
    let never_indexed = f.seed_message("landlord@example.com", true);
    f.index_text(indexed, "lease renewal terms").await;
    f.index_semantic(indexed, &embedder).await;

    let vector = embedder
        .embed(&["something no message resembles".to_owned()])
        .await
        .expect("embed")
        .remove(0);
    // `~lease` yields no lexical arm, so the dense arm is the only text arm —
    // and its floor is set high enough that it matches nothing at all.
    let mut spec = nl_spec(f.account_id, "meaning-only", "from:landlord ~lease");
    spec.query_vector = Some(vector);
    spec.vector_model = Some(embedder.model().to_owned());
    spec.min_similarity = Some(0.999);

    let folder = store.create(&spec).await.expect("create");
    let members = store
        .members(folder.id, None, &token())
        .await
        .expect("members");

    assert!(
        members.is_empty(),
        "an empty dense arm must compile to `0`, not to a dropped clause — this \
         folder would otherwise hold {members:?}"
    );
    let _ = never_indexed;
}

#[tokio::test]
async fn a_hybrid_folder_fires_only_for_genuinely_new_members() {
    // Task 35's exactly-once ledger contract, re-proven over the hybrid path:
    // the arms changed, the firing rule did not.
    let f = Fixture::open();
    let store = f.store_no_imap();
    let backlog = f.seed_message("billing@stripe.com", true);
    f.index_text(backlog, "your invoice for June").await;

    let mut spec = nl_spec(f.account_id, "invoices", "from:stripe invoice");
    spec.notify = true;
    let folder = store.create(&spec).await.expect("create");

    // Creation records a baseline, so the backlog notifies for nothing.
    let first = store.evaluate(folder.id, &token()).await.expect("evaluate");
    assert_eq!(first.members, 1);
    assert_eq!(first.entered, Vec::<i64>::new());
    assert_eq!(first.notified, 0);

    let newcomer = f.seed_message("billing@stripe.com", true);
    f.index_text(newcomer, "invoice for July is ready").await;
    let second = store.evaluate(folder.id, &token()).await.expect("evaluate");
    assert_eq!(second.entered, vec![newcomer]);
    assert_eq!(second.notified, 1);

    // And a third pass over unchanged membership fires nothing.
    let third = store.evaluate(folder.id, &token()).await.expect("evaluate");
    assert_eq!(third.entered, Vec::<i64>::new());
    assert_eq!(third.notified, 0);
    let _ = backlog;
}

#[tokio::test]
async fn a_hybrid_folder_reports_its_provenance() {
    // The compiled query is not what the user said; both are kept, and a
    // client needs the sentence to explain the folder.
    let f = Fixture::open();
    let store = f.store_no_imap();
    let folder = store
        .create(&nl_spec(f.account_id, "invoices", "from:stripe invoice"))
        .await
        .expect("create");

    assert_eq!(
        folder.nl_source.as_deref(),
        Some("what the user actually said")
    );
    assert_eq!(folder.compiled_model.as_deref(), Some("test-compile-model"));
    assert!(folder.compiled_at.is_some_and(|at| at > 0));
    // No vector was supplied, so nothing claims a semantic arm exists.
    assert_eq!(folder.vector_model, None);

    // And a deterministic folder claims none of it.
    let plain = store
        .create(&spec(f.account_id, "plain", "from:stripe"))
        .await
        .expect("create");
    assert_eq!(plain.nl_source, None);
    assert_eq!(plain.compiled_model, None);
    assert_eq!(plain.compiled_at, None);
}

#[tokio::test]
async fn a_declared_dense_arm_whose_vector_will_not_load_empties_the_folder() {
    // The regression this test exists for: `Membership` used to build the `0`
    // arm from whether the vector *resolved* rather than whether the folder
    // *declared* one, so a stored blob that would not decode dropped the
    // clause entirely — and a folder defined as "anything about the lease"
    // became `WHERE account_id = ?`, i.e. every message in the account, whose
    // very first evaluation auto-tags and notifies for the whole mailbox.
    //
    // A wrong-width blob is the reachable case, not a corrupt one: a daemon
    // configured with a wider embedder (`index.semantic.voyage.dim` is 1024)
    // writes one, and `Embedding::from_bytes` refuses it at read time.
    let f = Fixture::open();
    let store = f.store_no_imap();
    let embedder = test_embedder();
    let message = f.seed_message("landlord@example.com", true);
    f.index_text(message, "lease renewal terms").await;
    f.index_semantic(message, &embedder).await;

    let vector = embedder
        .embed(&["lease renewal terms".to_owned()])
        .await
        .expect("embed")
        .remove(0);
    // `~lease` yields no lexical arm, so the dense arm is the only text arm —
    // exactly the shape where dropping it is catastrophic rather than merely
    // wrong.
    let mut spec = nl_spec(f.account_id, "meaning-only", "from:landlord ~lease");
    spec.query_vector = Some(vector);
    spec.vector_model = Some(embedder.model().to_owned());
    let folder = store.create(&spec).await.expect("create");

    // Rewrite the stored vector at a width this index cannot hold — what a
    // re-index under a different model leaves behind.
    let id = folder.id;
    f.db.write(move |conn| {
        conn.execute(
            "UPDATE smart_folders SET query_vector = ?2 WHERE id = ?1",
            rusqlite::params![id, vec![0u8; 64]],
        )?;
        Ok(())
    })
    .await
    .expect("corrupt the stored vector");

    let members = store
        .members(folder.id, None, &token())
        .await
        .expect("members");
    assert!(
        members.is_empty(),
        "a declared dense arm that could not be loaded must still constrain — this \
         folder would otherwise hold {members:?}, which is every message from that \
         sender"
    );

    // And an evaluation of it fires for nobody, rather than for the account.
    let evaluation = store.evaluate(folder.id, &token()).await.expect("evaluate");
    assert_eq!(evaluation.members, 0);
    assert_eq!(evaluation.entered, Vec::<i64>::new());
}

#[tokio::test]
async fn a_query_vector_this_index_cannot_search_is_refused_at_create() {
    // Refusing at the source is what lets `constrains` give the honest answer:
    // a folder stored on the strength of an arm that can never fire is a
    // folder with one fewer constraint than it claims.
    let f = Fixture::open();
    let store = f.store_no_imap();
    let mut spec = nl_spec(f.account_id, "too-wide", "from:landlord lease");
    spec.query_vector = Some(Embedding::new(vec![0.5; VECTOR_DIM * 2]));
    spec.vector_model = Some("a-wider-model".to_owned());

    let error = store
        .create(&spec)
        .await
        .expect_err("a vector this index cannot hold must be refused");
    assert_eq!(error.reason(), ErrorReason::InvalidArgument);
    assert!(
        error.to_string().contains("dimensional"),
        "the rejection must say what is wrong with it: {error}"
    );
}

#[tokio::test]
async fn a_predicate_that_constrains_nothing_holds_nothing_even_if_it_is_stored() {
    // The unconditional floor in `Membership::sql`. Every path here is
    // supposed to be refused at create time, so this reaches past `create` and
    // writes the row directly — the shape a future bug, a hand edit, or an
    // older build's row would take. Wrong in the safe direction: an empty
    // folder someone reports, not a full one nobody looks at.
    let f = Fixture::open();
    let store = f.store_no_imap();
    let message = f.seed_message("anyone@example.com", true);
    let folder = store
        .create(&nl_spec(f.account_id, "ok", "from:anyone"))
        .await
        .expect("create");

    let id = folder.id;
    f.db.write(move |conn| {
        conn.execute(
            // `~lease` compiles to no filter and no lexical arm, and there is
            // no vector.
            "UPDATE smart_folders SET predicate = '~lease' WHERE id = ?1",
            [id],
        )?;
        Ok(())
    })
    .await
    .expect("plant an unconstrained predicate");

    let members = store
        .members(folder.id, None, &token())
        .await
        .expect("members");
    assert!(
        members.is_empty(),
        "an unconstrained stored predicate must hold nothing, not {members:?}"
    );
    let _ = message;
}
