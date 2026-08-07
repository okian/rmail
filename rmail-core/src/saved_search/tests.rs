//! What task 35 owes for saved searches: a named query is persisted
//! verbatim and re-runnable, nothing about a *result* is ever stored, and
//! the three error paths the task names — an unrunnable query, a duplicate
//! name, and an account that does not exist — have defined, tested
//! behaviour rather than a raw storage fault.
//!
//! The "re-run through the full pipeline" half is proven where the pipeline
//! actually lives: `rmaild/tests/saved_search_service.rs` asserts
//! `RunSavedSearch` and a plain `Search` of the same string return the same
//! ranked hits. This file proves the half that belongs to the store — that
//! the string handed to that pipeline is the one that was saved.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use super::*;
use crate::error::ErrorReason;
use crate::repo;

static COUNTER: AtomicU32 = AtomicU32::new(0);

struct Fixture {
    db: Database,
    path: PathBuf,
    account_id: i64,
}

impl Fixture {
    fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-saved-search-{pid}-{n}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", path.display())));
        }
        let db = Database::open(&path).expect("open temp db");
        let account_id = db
            .with_write(move |conn| {
                repo::insert_account(
                    conn,
                    &repo::NewAccount {
                        name: format!("acct-{n}"),
                        ..Default::default()
                    },
                )
            })
            .expect("seed account");
        Self {
            db,
            path,
            account_id,
        }
    }

    fn store(&self) -> SavedSearchStore {
        SavedSearchStore::new(self.db.clone())
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
// The query is stored verbatim, and is the only thing stored
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_saved_search_stores_the_raw_query_string_verbatim() {
    // Operators, a sigil, a negation, a quoted phrase and free text all
    // survive a round trip byte for byte. If any normalization crept in
    // here, re-running a saved search would stop being identical to typing
    // the same string into `mail search`.
    let f = Fixture::open();
    let raw = r#"from:stripe -in:Spam ~"quarterly report" =invoice is:unread"#;
    let created = f
        .store()
        .create(f.account_id, "Weekly", raw)
        .await
        .expect("create");
    assert_eq!(created.query, raw);

    let fetched = f.store().get(f.account_id, "Weekly").await.expect("get");
    assert_eq!(fetched.query, raw);
    assert_eq!(fetched.id, created.id);
}

#[tokio::test]
async fn nothing_about_a_result_set_is_persisted() {
    // The single most likely way to get this feature wrong is to cache ids.
    // Assert it structurally rather than by inspection: the only columns
    // `saved_searches` has are definition + timestamps, so a future
    // "results" column cannot be added without this failing.
    let f = Fixture::open();
    f.store()
        .create(f.account_id, "Weekly", "from:stripe")
        .await
        .expect("create");

    let columns: Vec<String> =
        f.db.with_read(|conn| {
            let mut stmt = conn.prepare("SELECT name FROM pragma_table_info('saved_searches')")?;
            let rows = stmt
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<String>>>()?;
            Ok(rows)
        })
        .expect("read columns");
    assert_eq!(
        columns,
        vec![
            "id".to_owned(),
            "account_id".to_owned(),
            "name".to_owned(),
            "query".to_owned(),
            "created_at".to_owned(),
            "updated_at".to_owned(),
            "last_run_at".to_owned(),
        ],
        "a saved search stores a query, never a result set"
    );
}

#[tokio::test]
async fn resolve_for_run_returns_the_saved_query_and_stamps_last_run_at() {
    let f = Fixture::open();
    let store = f.store();
    let created = store
        .create(f.account_id, "Weekly", "from:stripe is:unread")
        .await
        .expect("create");
    assert_eq!(created.last_run_at, None, "never run yet");

    let run = store
        .resolve_for_run(f.account_id, "weekly")
        .await
        .expect("resolve by a differently-cased name");
    assert_eq!(run.query, "from:stripe is:unread");
    assert!(run.last_run_at.is_some(), "the run must be stamped");

    // The stamp is durable, not just present on the returned value.
    let reread = store.get(f.account_id, "Weekly").await.expect("get");
    assert_eq!(reread.last_run_at, run.last_run_at);
}

#[tokio::test]
async fn updating_a_query_changes_what_a_later_run_resolves_to() {
    // A saved search is a live reference to a query, not a snapshot of one:
    // editing it must change what the next run executes.
    let f = Fixture::open();
    let store = f.store();
    store
        .create(f.account_id, "Weekly", "from:stripe")
        .await
        .expect("create");
    store
        .update_query(f.account_id, "Weekly", "from:aws is:flagged")
        .await
        .expect("update");

    let run = store
        .resolve_for_run(f.account_id, "Weekly")
        .await
        .expect("resolve");
    assert_eq!(run.query, "from:aws is:flagged");
}

#[tokio::test]
async fn list_is_alphabetical_and_scoped_to_its_account() {
    let f = Fixture::open();
    let other_account =
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
    let store = f.store();
    for name in ["zeta", "alpha", "mid"] {
        store
            .create(f.account_id, name, "from:stripe")
            .await
            .expect("create");
    }
    store
        .create(other_account, "elsewhere", "from:stripe")
        .await
        .expect("create in the other account");

    let names: Vec<String> = store
        .list(f.account_id)
        .await
        .expect("list")
        .into_iter()
        .map(|s| s.name)
        .collect();
    assert_eq!(names, vec!["alpha", "mid", "zeta"]);
}

#[tokio::test]
async fn delete_removes_it_and_a_second_delete_is_not_found() {
    let f = Fixture::open();
    let store = f.store();
    store
        .create(f.account_id, "Weekly", "from:stripe")
        .await
        .expect("create");
    store.delete(f.account_id, "Weekly").await.expect("delete");

    let err = store
        .delete(f.account_id, "Weekly")
        .await
        .expect_err("second delete");
    assert_eq!(err.reason(), ErrorReason::NotFound);
}

// ---------------------------------------------------------------------------
// Error paths (task 35's third acceptance requirement)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn queries_with_nothing_to_search_for_are_invalid_argument() {
    // `query::parse` cannot fail, so "unparseable" means "parses to nothing
    // searchable". Both shapes that reach it are rejected: whitespace, and a
    // non-empty string whose only token is an empty quoted phrase the parser
    // drops. Persisting either would be a row that fails on every future
    // run, with nothing pointing back at the moment it was created.
    let f = Fixture::open();
    let store = f.store();
    for raw in ["", "   ", "\t\n", "\"\""] {
        let err = store
            .create(f.account_id, "Broken", raw)
            .await
            .expect_err(&format!("{raw:?} must be rejected"));
        assert_eq!(
            err.reason(),
            ErrorReason::InvalidArgument,
            "wrong reason for {raw:?}"
        );
    }
    // ...and nothing was written.
    assert!(store.list(f.account_id).await.expect("list").is_empty());
}

#[tokio::test]
async fn a_query_that_parses_to_something_is_accepted_even_if_no_operator_is_recognized() {
    // The complement of the case above, and the reason validation is
    // "parses to nothing" rather than "has a supported operator": a saved
    // search runs through the *ranked* pipeline, where free text is a
    // first-class citizen. Rejecting it here would make the feature
    // useless for the queries people actually save.
    let f = Fixture::open();
    let created = f
        .store()
        .create(f.account_id, "Plain", "quarterly budget")
        .await
        .expect("free text is a legitimate saved search");
    assert_eq!(created.query, "quarterly budget");
}

#[tokio::test]
async fn a_duplicate_name_is_already_exists_and_leaves_the_original_untouched() {
    let f = Fixture::open();
    let store = f.store();
    store
        .create(f.account_id, "Weekly", "from:stripe")
        .await
        .expect("create");

    let err = store
        .create(f.account_id, "Weekly", "from:aws")
        .await
        .expect_err("duplicate name");
    assert_eq!(err.reason(), ErrorReason::AlreadyExists);

    // The failed create must not have clobbered the original.
    let existing = f.store().get(f.account_id, "Weekly").await.expect("get");
    assert_eq!(existing.query, "from:stripe");
}

#[tokio::test]
async fn a_duplicate_name_is_detected_case_insensitively() {
    // `COLLATE NOCASE` on the column and the lookup must agree, or "Weekly"
    // and "weekly" coexist as two rows while `get` resolves whichever the
    // planner reached first.
    let f = Fixture::open();
    let store = f.store();
    store
        .create(f.account_id, "Weekly", "from:stripe")
        .await
        .expect("create");
    let err = store
        .create(f.account_id, "WEEKLY", "from:aws")
        .await
        .expect_err("case-insensitive duplicate");
    assert_eq!(err.reason(), ErrorReason::AlreadyExists);
}

#[tokio::test]
async fn the_same_name_in_a_different_account_is_not_a_duplicate() {
    let f = Fixture::open();
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
    let store = f.store();
    store
        .create(f.account_id, "Weekly", "from:stripe")
        .await
        .expect("create");
    store
        .create(other, "Weekly", "from:aws")
        .await
        .expect("uniqueness is per account");
}

#[tokio::test]
async fn creating_against_an_account_that_does_not_exist_is_not_found() {
    // The foreign key is what actually catches this; without the explicit
    // classification it would surface as `INTERNAL` ("database error: ...")
    // and tell a client nothing actionable.
    let f = Fixture::open();
    let err = f
        .store()
        .create(9_999, "Weekly", "from:stripe")
        .await
        .expect_err("no such account");
    assert_eq!(err.reason(), ErrorReason::NotFound);
    assert!(
        err.to_string().contains("9999"),
        "the message should name the account: {err}"
    );
}

#[tokio::test]
async fn deleting_an_account_takes_its_saved_searches_with_it() {
    let f = Fixture::open();
    let store = f.store();
    store
        .create(f.account_id, "Weekly", "from:stripe")
        .await
        .expect("create");

    let account_id = f.account_id;
    f.db.with_write(move |conn| {
        conn.execute("DELETE FROM accounts WHERE id = ?1", [account_id])?;
        Ok(())
    })
    .expect("delete account");

    let err = store
        .get(f.account_id, "Weekly")
        .await
        .expect_err("cascaded away");
    assert_eq!(err.reason(), ErrorReason::NotFound);
}

#[tokio::test]
async fn an_empty_or_overlong_name_is_invalid_argument() {
    let f = Fixture::open();
    let store = f.store();
    for name in ["", "   "] {
        let err = store
            .create(f.account_id, name, "from:stripe")
            .await
            .expect_err("empty name");
        assert_eq!(err.reason(), ErrorReason::InvalidArgument);
    }
    let long = "x".repeat(MAX_NAME_LEN + 1);
    let err = store
        .create(f.account_id, &long, "from:stripe")
        .await
        .expect_err("overlong name");
    assert_eq!(err.reason(), ErrorReason::InvalidArgument);
}

#[tokio::test]
async fn an_overlong_query_is_invalid_argument() {
    let f = Fixture::open();
    let long = "a".repeat(MAX_QUERY_LEN + 1);
    let err = f
        .store()
        .create(f.account_id, "Big", &long)
        .await
        .expect_err("overlong query");
    assert_eq!(err.reason(), ErrorReason::InvalidArgument);
}

#[tokio::test]
async fn running_or_updating_a_missing_name_is_not_found() {
    let f = Fixture::open();
    let store = f.store();
    assert_eq!(
        store
            .resolve_for_run(f.account_id, "nope")
            .await
            .expect_err("no such search")
            .reason(),
        ErrorReason::NotFound
    );
    assert_eq!(
        store
            .update_query(f.account_id, "nope", "from:stripe")
            .await
            .expect_err("no such search")
            .reason(),
        ErrorReason::NotFound
    );
}
