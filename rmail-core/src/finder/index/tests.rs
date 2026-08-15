//! `FinderIndex` against a real database: the triggers actually fire, the
//! drain actually coalesces, and the store actually ends up describing what
//! is in the mailbox.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, RwLock};

use super::{FinderIndex, RECONCILE_EVERY_PASSES};
use crate::config::FinderConfig;
use crate::finder::store::FinderStore;
use crate::finder::ItemKind;
use crate::keymap::Action;
use crate::repo;
use crate::storage::Database;

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Fixture {
    db: Database,
    path: PathBuf,
    account_id: i64,
    mailbox_id: i64,
    store: Arc<RwLock<FinderStore>>,
}

impl Fixture {
    fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-finder-{pid}-{n}.db"));
        let db = Database::open(&path).expect("open temp db");
        let account_id = db
            .with_write(|conn| {
                repo::insert_account(
                    conn,
                    &repo::NewAccount {
                        name: format!("acct-{n}"),
                        ..Default::default()
                    },
                )
            })
            .expect("insert account");
        let mailbox_id = db
            .with_write(move |conn| {
                repo::insert_mailbox(
                    conn,
                    &repo::NewMailbox {
                        account_id,
                        name: "INBOX".to_owned(),
                        ..Default::default()
                    },
                )
            })
            .expect("insert mailbox");
        Self {
            db,
            path,
            account_id,
            mailbox_id,
            store: Arc::new(RwLock::new(FinderStore::new())),
        }
    }

    fn index(&self) -> FinderIndex {
        FinderIndex::new(
            self.db.clone(),
            Arc::clone(&self.store),
            &FinderConfig::default(),
        )
    }

    fn index_with(&self, config: FinderConfig) -> FinderIndex {
        FinderIndex::new(self.db.clone(), Arc::clone(&self.store), &config)
    }

    fn seed_message(&self, uid: i64, subject: &str) -> i64 {
        let account_id = self.account_id;
        let mailbox_id = self.mailbox_id;
        let subject = subject.to_owned();
        self.db
            .with_write(move |conn| {
                repo::insert_message(
                    conn,
                    &repo::NewMessage {
                        account_id,
                        mailbox_id,
                        uid,
                        uidvalidity: 1,
                        subject: Some(subject),
                        from_name: Some("Dana Whitfield".to_owned()),
                        from_addr: Some("dana@example.com".to_owned()),
                        date: Some(1_800_000_000),
                        body_text: Some("the body of the message, at some length".to_owned()),
                        ..Default::default()
                    },
                )
            })
            .expect("insert message")
    }

    fn dirty_rows(&self) -> i64 {
        self.db
            .with_read(|conn| {
                conn.query_row("SELECT COUNT(*) FROM finder_dirty", [], |row| row.get(0))
            })
            .expect("count dirty")
    }

    fn index_rows(&self) -> i64 {
        self.db
            .with_read(|conn| {
                conn.query_row("SELECT COUNT(*) FROM finder_index", [], |row| row.get(0))
            })
            .expect("count index")
    }

    fn primary_texts(&self, kind: ItemKind) -> Vec<String> {
        let guard = self.store.read().expect("store lock");
        guard
            .entries(kind)
            .iter()
            .map(|e| e.primary_text().to_owned())
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

// ---------------------------------------------------------------------------
// triggers
// ---------------------------------------------------------------------------

/// The change feed is written by SQLite itself, not by a Rust call site — the
/// property that makes it see every writer, including ones a later task adds.
#[tokio::test]
async fn inserting_a_message_writes_the_dirty_feed() {
    let fx = Fixture::open();
    let before = fx.dirty_rows();
    fx.seed_message(1, "Acme invoice 338");
    assert!(
        fx.dirty_rows() > before,
        "the messages insert trigger did not fire"
    );
}

/// prd.md's trigger list omits `flags`, which would leave `is_unread` — a
/// blended-ranking input — permanently stale.
#[tokio::test]
async fn flagging_a_message_marks_it_dirty() {
    let fx = Fixture::open();
    let message_id = fx.seed_message(1, "Acme invoice 338");
    let index = fx.index();
    index.rebuild().await.expect("rebuild");
    assert_eq!(fx.dirty_rows(), 0, "rebuild truncates the feed");

    fx.db
        .with_write(move |conn| repo::add_flag(conn, message_id, "\\Seen"))
        .expect("flag");
    assert!(fx.dirty_rows() > 0, "the flags insert trigger did not fire");

    index.drain(1).await.expect("drain");
    let guard = fx.store.read().expect("store lock");
    let entry = &guard.entries(ItemKind::Message)[0];
    assert!(!entry.unread, "the drain did not pick up the read flag");
}

#[tokio::test]
async fn deleting_a_message_removes_it_from_the_store() {
    let fx = Fixture::open();
    let message_id = fx.seed_message(1, "Acme invoice 338");
    let index = fx.index();
    index.rebuild().await.expect("rebuild");
    assert_eq!(fx.primary_texts(ItemKind::Message).len(), 1);

    fx.db
        .with_write(move |conn| {
            conn.execute("DELETE FROM messages WHERE id = ?1", [message_id])?;
            Ok(())
        })
        .expect("delete");
    index.drain(1).await.expect("drain");
    assert!(fx.primary_texts(ItemKind::Message).is_empty());
    assert_eq!(fx.index_rows(), fx.expected_rows_without_messages());
}

impl Fixture {
    /// Everything a rebuild writes that is not a message: one mailbox, plus
    /// one command per keymap action.
    fn expected_rows_without_messages(&self) -> i64 {
        1 + i64::try_from(Action::ALL.len()).expect("small")
    }
}

// ---------------------------------------------------------------------------
// rebuild
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_rebuild_indexes_every_kind() {
    let fx = Fixture::open();
    fx.seed_message(1, "Acme invoice 338");
    let account_id = fx.account_id;
    fx.db
        .with_write(move |conn| {
            conn.execute(
                "INSERT INTO contacts (address, name, message_count) VALUES (?1, ?2, 12)",
                rusqlite::params!["dana@example.com", "Dana Whitfield"],
            )?;
            conn.execute(
                "INSERT INTO saved_searches (account_id, name, query) VALUES (?1, ?2, ?3)",
                rusqlite::params![account_id, "Weekly", "is:unread newer_than:7d"],
            )?;
            conn.execute(
                "INSERT INTO tags (account_id, name) VALUES (?1, ?2)",
                rusqlite::params![account_id, "project/alpha"],
            )?;
            Ok(())
        })
        .expect("seed the other kinds");

    let index = fx.index();
    let loaded = index.rebuild().await.expect("rebuild");
    assert!(loaded > 0);

    assert_eq!(
        fx.primary_texts(ItemKind::Message),
        vec!["Acme invoice 338"]
    );
    assert_eq!(fx.primary_texts(ItemKind::Mailbox), vec!["INBOX"]);
    assert_eq!(fx.primary_texts(ItemKind::Contact), vec!["Dana Whitfield"]);
    assert_eq!(fx.primary_texts(ItemKind::SavedSearch), vec!["Weekly"]);
    assert_eq!(fx.primary_texts(ItemKind::Tag), vec!["project/alpha"]);
    assert_eq!(
        fx.primary_texts(ItemKind::Command).len(),
        Action::ALL.len(),
        "every keymap action must be a palette command"
    );
}

/// A rebuild has to truncate the feed in the same transaction, or clearing
/// `finder_index` hands the drain a backlog exactly as large as the index it
/// just rebuilt — describing changes that are already applied.
#[tokio::test]
async fn a_rebuild_leaves_no_backlog() {
    let fx = Fixture::open();
    for uid in 1..=20 {
        fx.seed_message(uid, &format!("subject {uid}"));
    }
    let index = fx.index();
    index.rebuild().await.expect("first rebuild");
    assert_eq!(fx.dirty_rows(), 0);
    index.rebuild().await.expect("second rebuild");
    assert_eq!(fx.dirty_rows(), 0);
    assert_eq!(fx.primary_texts(ItemKind::Message).len(), 20);
}

/// The command registry is the keymap's action list, not a second list that
/// can drift from it.
#[tokio::test]
async fn commands_come_from_the_keymap_action_registry() {
    let fx = Fixture::open();
    let index = fx.index();
    index.seed_commands().await.expect("seed");
    let actions: Vec<String> = fx
        .db
        .with_read(|conn| {
            let mut stmt = conn.prepare("SELECT action FROM finder_commands ORDER BY action")?;
            let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<String>>>()
        })
        .expect("read commands");
    let mut expected: Vec<String> = Action::ALL
        .iter()
        .map(|action| action.id().to_owned())
        .collect();
    expected.sort();
    assert_eq!(actions, expected);
}

/// Seeding twice must not duplicate, and an action that no longer exists must
/// not survive as an unrunnable palette entry.
#[tokio::test]
async fn reseeding_commands_is_idempotent_and_prunes() {
    let fx = Fixture::open();
    let index = fx.index();
    index.seed_commands().await.expect("first seed");
    fx.db
        .with_write(|conn| {
            conn.execute(
                "INSERT INTO finder_commands (name, action) VALUES ('gone', 'legacy.action')",
                [],
            )?;
            Ok(())
        })
        .expect("insert a stale command");
    index.seed_commands().await.expect("second seed");
    let count: i64 = fx
        .db
        .with_read(|conn| {
            conn.query_row("SELECT COUNT(*) FROM finder_commands", [], |row| row.get(0))
        })
        .expect("count");
    assert_eq!(count, i64::try_from(Action::ALL.len()).expect("small"));
}

// ---------------------------------------------------------------------------
// draining
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_drain_picks_up_a_new_message() {
    let fx = Fixture::open();
    let index = fx.index();
    index.rebuild().await.expect("rebuild");
    assert!(fx.primary_texts(ItemKind::Message).is_empty());

    fx.seed_message(1, "Acme invoice 338");
    let report = index.drain(1).await.expect("drain");
    assert!(report.rows > 0);
    assert_eq!(report.upserted, 1);
    assert_eq!(
        fx.primary_texts(ItemKind::Message),
        vec!["Acme invoice 338"]
    );
}

/// A row touched forty times inside one drain window must cost one re-fold,
/// not forty — the property that keeps a resync from starving the writer.
#[tokio::test]
async fn a_drain_coalesces_repeated_touches() {
    let fx = Fixture::open();
    let index = fx.index();
    index.rebuild().await.expect("rebuild");
    let message_id = fx.seed_message(1, "first");
    for n in 0..40 {
        let subject = format!("touch {n}");
        fx.db
            .with_write(move |conn| {
                conn.execute(
                    "UPDATE messages SET subject = ?1 WHERE id = ?2",
                    rusqlite::params![subject, message_id],
                )?;
                Ok(())
            })
            .expect("update");
    }
    assert!(fx.dirty_rows() >= 40);
    let report = index.drain(1).await.expect("drain");
    assert!(report.rows >= 40, "all the feed rows were read");
    assert_eq!(report.upserted, 1, "but only one entry was rebuilt");
    assert_eq!(fx.primary_texts(ItemKind::Message), vec!["touch 39"]);
}

/// prd.md: "Large dirty backlog -> capped batched drain."
#[tokio::test]
async fn a_drain_is_capped_and_makes_progress_across_passes() {
    let fx = Fixture::open();
    let index = fx.index_with(FinderConfig {
        max_drain_batch: 5,
        ..FinderConfig::default()
    });
    index.rebuild().await.expect("rebuild");
    for uid in 1..=20 {
        fx.seed_message(uid, &format!("subject {uid}"));
    }
    let first = index.drain(1).await.expect("drain");
    assert_eq!(first.rows, 5, "the cap is a real cap");
    assert!(fx.dirty_rows() > 0, "there is a backlog left");

    for pass in 2..=10 {
        index.drain(pass).await.expect("drain");
    }
    assert_eq!(fx.primary_texts(ItemKind::Message).len(), 20);
}

/// Applying the same feed batch twice must be harmless: a crash between
/// "apply" and "delete the feed rows" has to leave the index correct.
#[tokio::test]
async fn applying_a_batch_twice_is_idempotent() {
    let fx = Fixture::open();
    let index = fx.index();
    index.rebuild().await.expect("rebuild");
    fx.seed_message(1, "Acme invoice 338");
    index.drain(1).await.expect("first drain");
    let after_first = fx.index_rows();
    // Re-enqueue the same change by touching the row again.
    fx.db
        .with_write(|conn| {
            conn.execute("UPDATE messages SET subject = subject", [])?;
            Ok(())
        })
        .expect("touch");
    index.drain(2).await.expect("second drain");
    assert_eq!(fx.index_rows(), after_first);
    assert_eq!(fx.primary_texts(ItemKind::Message).len(), 1);
}

/// prd.md: "Stale ref -> ... entry pruned next drain." An upsert for a row
/// that has since disappeared has to prune, not resurrect.
#[tokio::test]
async fn an_upsert_for_a_vanished_row_prunes_the_entry() {
    let fx = Fixture::open();
    let index = fx.index();
    fx.seed_message(1, "Acme invoice 338");
    index.rebuild().await.expect("rebuild");
    assert_eq!(fx.primary_texts(ItemKind::Message).len(), 1);

    // Enqueue an upsert by hand for a ref that does not exist.
    fx.db
        .with_write(|conn| {
            conn.execute(
                "INSERT INTO finder_dirty (kind, ref_id, op, created_at) VALUES (0, 99999, 0, 0)",
                [],
            )?;
            Ok(())
        })
        .expect("enqueue");
    index.drain(1).await.expect("drain");
    assert_eq!(fx.primary_texts(ItemKind::Message).len(), 1, "unaffected");

    // ...and now the real row, deleted behind the trigger's back is covered
    // by the delete trigger itself; this asserts the vanished-ref path leaves
    // no phantom entry.
    let count: i64 = fx
        .db
        .with_read(|conn| {
            conn.query_row(
                "SELECT COUNT(*) FROM finder_index WHERE kind = 0 AND ref_id = 99999",
                [],
                |row| row.get(0),
            )
        })
        .expect("count");
    assert_eq!(count, 0);
}

/// Deleting an account must leave nothing of its mail findable — the finder
/// index holds *copies* of subject lines, so a shadow surviving there would
/// be an account that is deleted everywhere except in the picker.
///
/// The mechanism is deliberately not asserted. `finder_index.account_id`
/// cascades, so the rows go either way; whether the cascade *also* fires the
/// delete triggers that feed the in-memory store is a SQLite build detail
/// (documented to require `recursive_triggers`, which this database does not
/// set). This asserts the outcome after a reconcile pass, which holds under
/// both readings — and `the_reconcile_pass_repairs_drift` covers the case
/// where the feed genuinely says nothing.
#[tokio::test]
async fn deleting_an_account_leaves_none_of_its_mail_findable() {
    let fx = Fixture::open();
    let index = fx.index();
    for uid in 1..=3 {
        fx.seed_message(uid, &format!("subject {uid}"));
    }
    index.rebuild().await.expect("rebuild");
    assert_eq!(fx.primary_texts(ItemKind::Message).len(), 3);

    let account_id = fx.account_id;
    fx.db
        .with_write(move |conn| {
            conn.execute("DELETE FROM accounts WHERE id = ?1", [account_id])?;
            Ok(())
        })
        .expect("delete the account");
    // The cascade emptied the table; only the account-less commands remain.
    assert_eq!(
        fx.index_rows(),
        i64::try_from(Action::ALL.len()).expect("small")
    );

    index.drain(1).await.expect("ordinary drain");
    index
        .drain(RECONCILE_EVERY_PASSES)
        .await
        .expect("reconcile drain");
    assert!(fx.primary_texts(ItemKind::Message).is_empty());
    assert!(fx.primary_texts(ItemKind::Mailbox).is_empty());
}

/// The safety net itself: rows can leave `finder_index` without the feed ever
/// describing it (a cascade on a build that does not fire triggers for one, a
/// future task writing the table directly, a bug in this file). The reconcile
/// pass exists so none of those leave the picker serving mail that is gone,
/// and this drives it with the feed deliberately emptied so nothing *but*
/// reconciliation could repair it.
#[tokio::test]
async fn the_reconcile_pass_repairs_drift_the_feed_never_described() {
    let fx = Fixture::open();
    let index = fx.index();
    for uid in 1..=3 {
        fx.seed_message(uid, &format!("subject {uid}"));
    }
    index.rebuild().await.expect("rebuild");
    assert_eq!(fx.primary_texts(ItemKind::Message).len(), 3);

    fx.db
        .with_write(|conn| {
            conn.execute("DELETE FROM finder_index WHERE kind = 0", [])?;
            // ...and swallow every event that delete produced, so the feed is
            // silent about it.
            conn.execute("DELETE FROM finder_dirty", [])?;
            Ok(())
        })
        .expect("drift the table behind the store's back");

    // An ordinary pass has nothing to go on.
    index.drain(1).await.expect("ordinary drain");
    assert_eq!(
        fx.primary_texts(ItemKind::Message).len(),
        3,
        "an empty feed cannot describe a change"
    );

    // The reconcile pass notices the counts disagree and reloads.
    let report = index
        .drain(RECONCILE_EVERY_PASSES)
        .await
        .expect("reconcile drain");
    assert!(report.reloaded, "the reconcile pass did not fire");
    assert!(fx.primary_texts(ItemKind::Message).is_empty());
}

/// The reconcile net must survive a *capped* store, which is exactly the
/// large mailbox it was written for.
///
/// `rejected` is cleared only by a full load, and a full load is what the
/// reconcile pass triggers — so a check that gave up whenever anything had
/// ever been turned away would disable itself permanently the first time a
/// cap bound, for the life of the process.
#[tokio::test]
async fn the_reconcile_pass_still_fires_on_a_capped_store() {
    let fx = Fixture::open();
    for uid in 1..=8 {
        fx.seed_message(uid, &format!("subject {uid}"));
    }
    // Small enough that the store is genuinely truncated.
    let index = fx.index_with(FinderConfig {
        max_entries: 4,
        ..FinderConfig::default()
    });
    index.rebuild().await.expect("rebuild");
    {
        let guard = fx.store.read().expect("store lock");
        assert_eq!(guard.len(), 4);
        assert!(guard.rejected() > 0, "the cap must have bound");
    }

    // Drift the table out from under the store, feed silent.
    fx.db
        .with_write(|conn| {
            conn.execute("DELETE FROM finder_index WHERE kind = 0", [])?;
            conn.execute("DELETE FROM finder_dirty", [])?;
            Ok(())
        })
        .expect("drift");

    let report = index
        .drain(RECONCILE_EVERY_PASSES)
        .await
        .expect("reconcile drain");
    assert!(
        report.reloaded,
        "the reconcile pass switched itself off on a capped store"
    );
    assert!(fx.primary_texts(ItemKind::Message).is_empty());
}

/// The drain's own deletes echo back through `finder_dirty_index_delete`, and
/// leaving them for the next pass is not merely wasteful: they carry a higher
/// `seq` than any row the batch cap left unread, and the next pass coalesces
/// "last seq wins" — so an echo can override a pending upsert for the same
/// `(kind, ref_id)`, which row-id reuse makes reachable. The echo is deleted
/// inside the transaction that produced it.
#[tokio::test]
async fn a_drain_does_not_leave_its_own_delete_echoes_behind() {
    let fx = Fixture::open();
    let index = fx.index();
    let message_id = fx.seed_message(1, "Acme invoice 338");
    index.rebuild().await.expect("rebuild");
    assert_eq!(fx.dirty_rows(), 0);

    fx.db
        .with_write(move |conn| {
            conn.execute("DELETE FROM messages WHERE id = ?1", [message_id])?;
            Ok(())
        })
        .expect("delete");
    index.drain(1).await.expect("drain");

    assert_eq!(
        fx.dirty_rows(),
        0,
        "the drain left its own delete echo in the feed"
    );
    assert!(fx.primary_texts(ItemKind::Message).is_empty());
}

// ---------------------------------------------------------------------------
// loading and status
// ---------------------------------------------------------------------------

/// A migration establishes the tables but cannot populate them, so the first
/// start after V38 has a full mailbox and an empty index.
#[tokio::test]
async fn ensure_built_populates_a_never_built_index() {
    let fx = Fixture::open();
    fx.seed_message(1, "Acme invoice 338");
    let index = fx.index();
    assert_eq!(fx.index_rows(), 0);
    let loaded = index.ensure_built().await.expect("ensure_built");
    assert!(loaded > 0);
    assert_eq!(
        fx.primary_texts(ItemKind::Message),
        vec!["Acme invoice 338"]
    );
}

/// The store's cap has to bind on load, not just on incremental upserts —
/// otherwise a cold start over a large mailbox allocates without limit.
#[tokio::test]
async fn a_load_honors_the_entry_cap() {
    let fx = Fixture::open();
    for uid in 1..=20 {
        fx.seed_message(uid, &format!("subject {uid}"));
    }
    let index = fx.index_with(FinderConfig {
        max_entries: 5,
        ..FinderConfig::default()
    });
    index.rebuild().await.expect("rebuild");
    let guard = fx.store.read().expect("store lock");
    assert_eq!(guard.len(), 5);
    assert!(guard.rejected() > 0);
}

#[tokio::test]
async fn status_reports_the_backlog_and_the_footprint() {
    let fx = Fixture::open();
    let index = fx.index();
    fx.seed_message(1, "Acme invoice 338");
    index.rebuild().await.expect("rebuild");

    let status = index.status().await.expect("status");
    assert!(status.entries > 0);
    assert!(status.bytes > 0);
    assert_eq!(status.pending, 0);
    assert!(status.refreshed_at > 0);

    fx.seed_message(2, "another");
    let status = index.status().await.expect("status");
    assert!(status.pending > 0, "a pending change must be visible");
}

/// The snippet column is capped in bytes, and a byte cut through a multi-byte
/// character would store text every renderer downstream has to defend against.
#[tokio::test]
async fn the_snippet_cap_snaps_to_a_char_boundary() {
    let fx = Fixture::open();
    let account_id = fx.account_id;
    let mailbox_id = fx.mailbox_id;
    fx.db
        .with_write(move |conn| {
            repo::insert_message(
                conn,
                &repo::NewMessage {
                    account_id,
                    mailbox_id,
                    uid: 1,
                    uidvalidity: 1,
                    subject: Some("café".to_owned()),
                    // "café résumé": c a f then a two-byte 'é' at bytes 3-4.
                    // A cap of 4 therefore lands *inside* that character.
                    body_text: Some("café résumé".to_owned()),
                    ..Default::default()
                },
            )
        })
        .expect("insert");
    let index = fx.index_with(FinderConfig {
        snippet_max_bytes: 4,
        ..FinderConfig::default()
    });
    index.rebuild().await.expect("rebuild");

    let snippet: String = fx
        .db
        .with_read(|conn| {
            conn.query_row(
                "SELECT snippet FROM finder_index WHERE kind = 0",
                [],
                |row| row.get(0),
            )
        })
        .expect("read snippet");
    assert_eq!(snippet, "caf", "a byte cap must snap down to a boundary");
    assert!(snippet.len() < 4, "and it really is shorter than the cap");
}

/// ...and a cap that already lands on a boundary must not lose a character.
#[tokio::test]
async fn the_snippet_cap_keeps_a_whole_character_when_it_fits() {
    let fx = Fixture::open();
    let account_id = fx.account_id;
    let mailbox_id = fx.mailbox_id;
    fx.db
        .with_write(move |conn| {
            repo::insert_message(
                conn,
                &repo::NewMessage {
                    account_id,
                    mailbox_id,
                    uid: 1,
                    uidvalidity: 1,
                    body_text: Some("café résumé".to_owned()),
                    ..Default::default()
                },
            )
        })
        .expect("insert");
    let index = fx.index_with(FinderConfig {
        snippet_max_bytes: 5,
        ..FinderConfig::default()
    });
    index.rebuild().await.expect("rebuild");

    let snippet: String = fx
        .db
        .with_read(|conn| {
            conn.query_row(
                "SELECT snippet FROM finder_index WHERE kind = 0",
                [],
                |row| row.get(0),
            )
        })
        .expect("read snippet");
    assert_eq!(snippet, "café");
}

/// The folded blob on disk has to be what the matcher would produce, or a
/// consumer reading `match_blob` directly would get a different answer from
/// the in-memory index.
#[tokio::test]
async fn the_stored_blob_is_folded() {
    let fx = Fixture::open();
    let account_id = fx.account_id;
    let mailbox_id = fx.mailbox_id;
    fx.db
        .with_write(move |conn| {
            repo::insert_message(
                conn,
                &repo::NewMessage {
                    account_id,
                    mailbox_id,
                    uid: 1,
                    uidvalidity: 1,
                    subject: Some("Café meeting".to_owned()),
                    ..Default::default()
                },
            )
        })
        .expect("insert");
    fx.index().rebuild().await.expect("rebuild");
    let blob: String = fx
        .db
        .with_read(|conn| {
            conn.query_row(
                "SELECT match_blob FROM finder_index WHERE kind = 0",
                [],
                |row| row.get(0),
            )
        })
        .expect("read blob");
    assert!(blob.starts_with("Cafe meeting"), "got {blob:?}");
    let guard = fx.store.read().expect("store lock");
    assert!(guard.entries(ItemKind::Message)[0]
        .blob()
        .starts_with("Cafe meeting"));
}
