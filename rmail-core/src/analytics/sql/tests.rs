//! The read-only SQL sandbox: what the authorizer admits, what it refuses,
//! and the shapes a naive guard gets wrong.
//!
//! Every "refused" test here is written so it would *pass* against a broken
//! guard only if the guard were removed entirely — each one names a table the
//! views really do read (`messages`, `accounts`, `flags`) or a statement type
//! SQLite really would execute, so a test that started passing for the wrong
//! reason would be a test that stopped denying anything.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use super::*;
use crate::repo;
use crate::ErrorReason;

static COUNTER: AtomicU32 = AtomicU32::new(0);

const T0: i64 = 1_700_000_000;

struct Fx {
    db: Database,
    path: PathBuf,
    account_id: i64,
    inbox: i64,
}

impl Fx {
    fn open() -> Self {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("rmail-sqlguard-{pid}-{n}.db"));
        let db = Database::open(&path).unwrap();
        let (account_id, inbox) = db
            .with_write(|c| {
                let account_id = repo::insert_account(
                    c,
                    &repo::NewAccount {
                        name: format!("Personal-{n}"),
                        username: Some("me@example.com".to_owned()),
                        ..Default::default()
                    },
                )?;
                let inbox = repo::insert_mailbox(
                    c,
                    &repo::NewMailbox {
                        account_id,
                        name: "INBOX".to_owned(),
                        ..Default::default()
                    },
                )?;
                Ok((account_id, inbox))
            })
            .unwrap();
        Self {
            db,
            path,
            account_id,
            inbox,
        }
    }

    fn add(&self, from: &str, subject: &str, at: i64, seen: bool) -> i64 {
        let id = self
            .db
            .with_write(|c| {
                repo::insert_message(
                    c,
                    &repo::NewMessage {
                        account_id: self.account_id,
                        mailbox_id: self.inbox,
                        uid: at,
                        uidvalidity: 1,
                        message_id: Some(format!("m{at}@example.com")),
                        subject: Some(subject.to_owned()),
                        from_addr: Some(from.to_owned()),
                        date: Some(at),
                        ..Default::default()
                    },
                )
            })
            .unwrap();
        if seen {
            self.db
                .with_write(|c| {
                    c.execute(
                        "INSERT INTO flags (message_id, flag) VALUES (?1, '\\Seen')",
                        [id],
                    )
                })
                .unwrap();
        }
        id
    }

    async fn run(&self, sql: &str) -> Result<QueryResult, Error> {
        run(&self.db, &CancellationToken::new(), sql, &[]).await
    }

    async fn refused(&self, sql: &str) -> Error {
        match self.run(sql).await {
            Ok(result) => panic!("`{sql}` was allowed and returned {result:?}"),
            Err(error) => error,
        }
    }
}

impl Drop for Fx {
    fn drop(&mut self) {
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", self.path.display())));
        }
    }
}

/// The mechanism `is_allowed_read` depends on, pinned against SQLite itself.
///
/// Two claims, and the guard is only sound while both hold:
///
/// 1. A column read performed *by* a whitelisted view carries that view as the
///    accessor. This is what separates `SELECT subject FROM
///    analytics_messages` from `SELECT raw FROM messages`, which produce the
///    same `Read { table_name: "messages" }` and differ only here.
/// 2. A backing table the optimizer *drops* (because the outer query used no
///    column of it) is still notified — with an **empty column and no
///    accessor**. That shape is why clause 3 of `is_allowed_read` exists, and
///    if a future SQLite stopped emitting it, that clause would become dead
///    permission nobody had noticed granting.
///
/// A version bump that changed either would otherwise show up as an unrelated
/// query mysteriously failing, or — far worse — as a rule that admits more
/// than it was written to.
#[test]
fn the_authorizer_sees_a_view_as_the_accessor() {
    let fx = Fx::open();
    fx.add("ada@example.com", "Hi", T0, true);
    /// `(table, column, accessor)` — what the authorizer was shown.
    type Observed = (String, String, Option<String>);
    let seen: Arc<Mutex<Vec<Observed>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    fx.db
        .with_read(|conn| {
            conn.authorizer(Some(move |ctx: AuthContext<'_>| {
                if let AuthAction::Read {
                    table_name,
                    column_name,
                } = ctx.action
                {
                    if let Ok(mut v) = sink.lock() {
                        v.push((
                            table_name.to_owned(),
                            column_name.to_owned(),
                            ctx.accessor.map(str::to_owned),
                        ));
                    }
                }
                Authorization::Allow
            }));
            let prepared =
                conn.prepare("SELECT from_addr, count(*) FROM analytics_messages GROUP BY 1");
            conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
            prepared.map(|_| ())
        })
        .unwrap();
    let seen = seen.lock().unwrap().clone();

    assert!(
        seen.iter().any(|(table, column, accessor)| {
            table == "messages"
                && column == "from_addr"
                && accessor.as_deref() == Some("analytics_messages")
        }),
        "a view's own column read no longer names the view as the accessor: {seen:?}"
    );
    assert!(
        seen.iter().any(|(table, column, accessor)| {
            (table == "accounts" || table == "mailboxes") && column.is_empty() && accessor.is_none()
        }),
        "the dropped-join notification `is_allowed_read` clause 3 exists for is gone; that \
         clause is now permission granted for nothing: {seen:?}"
    );
    // And the shape that must never be admissible: a column read of a backing
    // table with no accessor at all is what a direct `SELECT raw FROM
    // messages` looks like.
    assert!(
        !is_allowed_read("messages", "raw", None),
        "a bare column read of a base table is admissible"
    );
    assert!(
        !is_allowed_read("api_tokens", "", None),
        "an empty-column read of a non-backing table is admissible"
    );
}

// ---------------------------------------------------------------------------
// What is allowed
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_select_over_a_whitelisted_view_runs() {
    let fx = Fx::open();
    fx.add("ada@example.com", "Lease renewal", T0, true);
    fx.add("ada@example.com", "Lease again", T0 + 60, false);

    let result = fx
        .run("SELECT from_addr AS sender, count(*) AS n FROM analytics_messages GROUP BY 1")
        .await
        .unwrap();
    assert_eq!(result.columns, vec!["sender", "n"]);
    assert_eq!(result.rows.len(), 1);
    assert_eq!(result.rows[0][0], Cell::Text("ada@example.com".to_owned()));
    assert_eq!(result.rows[0][1], Cell::Integer(2));
}

/// The views really do reach `flags` and `accounts` under the hood. If the
/// accessor rule were wrong, this would fail — which is what makes the
/// "refused" tests below meaningful rather than vacuous.
#[tokio::test]
async fn a_view_may_read_the_base_tables_it_is_built_from() {
    let fx = Fx::open();
    fx.add("ada@example.com", "Read one", T0, true);
    fx.add("ada@example.com", "Unread one", T0 + 60, false);

    let result = fx
        .run("SELECT sum(is_read) AS seen, max(account_name) AS acct FROM analytics_messages")
        .await
        .unwrap();
    assert_eq!(result.rows[0][0], Cell::Integer(1));
    assert!(matches!(result.rows[0][1], Cell::Text(_)));
}

#[tokio::test]
async fn bound_parameters_are_values_and_never_syntax() {
    let fx = Fx::open();
    fx.add("ada@example.com", "One", T0, false);
    fx.add("bob@example.com", "Two", T0 + 60, false);

    let result = run(
        &fx.db,
        &CancellationToken::new(),
        "SELECT count(*) AS n FROM analytics_messages WHERE from_addr = ?",
        &[Value::Text("ada@example.com".to_owned())],
    )
    .await
    .unwrap();
    assert_eq!(result.rows[0][0], Cell::Integer(1));

    // A parameter holding a whole statement is a string, not a statement.
    let result = run(
        &fx.db,
        &CancellationToken::new(),
        "SELECT count(*) AS n FROM analytics_messages WHERE from_addr = ?",
        &[Value::Text("x' OR 1=1 --".to_owned())],
    )
    .await
    .unwrap();
    assert_eq!(result.rows[0][0], Cell::Integer(0));
}

#[tokio::test]
async fn every_whitelisted_view_is_readable() {
    let fx = Fx::open();
    fx.add("ada@example.com", "Hello", T0, true);
    for view in ALLOWED_VIEWS {
        let sql = format!("SELECT * FROM {view} LIMIT 1");
        fx.run(&sql)
            .await
            .unwrap_or_else(|error| panic!("{view} is whitelisted but unreadable: {error}"));
    }
}

// ---------------------------------------------------------------------------
// What is refused
// ---------------------------------------------------------------------------

/// The headline: a table the views do read, named directly, is denied — so
/// "allowed through a view" and "allowed" are genuinely different.
#[tokio::test]
async fn reading_a_base_table_directly_is_refused() {
    let fx = Fx::open();
    fx.add("ada@example.com", "Hi", T0, false);
    let error = fx.refused("SELECT raw FROM messages").await;
    assert_eq!(error.reason(), ErrorReason::InvalidArgument);
    assert!(
        error.to_string().contains("messages"),
        "the error should name what was refused: {error}"
    );
}

#[tokio::test]
async fn reading_a_secret_table_is_refused() {
    let fx = Fx::open();
    for sql in [
        "SELECT * FROM api_tokens",
        "SELECT * FROM account_credentials",
        "SELECT * FROM ai_ledger",
        "SELECT name FROM sqlite_master",
        // The shape clause 3 of `is_allowed_read` admits for a backing table:
        // a reference with no column read. It must stay denied for everything
        // else, or the clause would be a hole rather than a narrow allowance.
        "SELECT count(*) AS n FROM api_tokens",
        "SELECT 1 AS n FROM account_credentials",
    ] {
        let error = fx.refused(sql).await;
        assert_eq!(error.reason(), ErrorReason::InvalidArgument, "{sql}");
    }
}

/// A write is refused *and* is named as a write, not as "no such table".
#[tokio::test]
async fn writes_are_refused() {
    let fx = Fx::open();
    let id = fx.add("ada@example.com", "Hi", T0, false);
    for sql in [
        "DELETE FROM messages",
        "UPDATE messages SET subject = 'x'",
        "INSERT INTO flags (message_id, flag) VALUES (1, '\\Seen')",
        "DROP TABLE messages",
        "DROP VIEW analytics_messages",
        "CREATE TABLE evil (x INTEGER)",
        "ALTER TABLE messages ADD COLUMN evil TEXT",
    ] {
        let error = fx.refused(sql).await;
        assert_eq!(error.reason(), ErrorReason::InvalidArgument, "{sql}");
    }
    // Nothing actually happened.
    let survived: i64 = fx
        .db
        .with_read(|c| {
            c.query_row("SELECT count(*) FROM messages WHERE id = ?1", [id], |r| {
                r.get(0)
            })
        })
        .unwrap();
    assert_eq!(
        survived, 1,
        "a refused statement still changed the database"
    );
}

/// `CREATE VIEW`/`CREATE TEMP TABLE` are the escape a whitelist keyed only on
/// names would miss: define your own view over `api_tokens`, then select from
/// it. Both halves are denied.
#[tokio::test]
async fn defining_a_new_relation_is_refused() {
    let fx = Fx::open();
    for sql in [
        "CREATE VIEW leak AS SELECT * FROM api_tokens",
        "CREATE TEMP VIEW leak AS SELECT * FROM api_tokens",
        "CREATE TEMP TABLE leak AS SELECT * FROM messages",
    ] {
        let error = fx.refused(sql).await;
        assert_eq!(error.reason(), ErrorReason::InvalidArgument, "{sql}");
    }
}

#[tokio::test]
async fn attach_and_pragma_are_refused() {
    let fx = Fx::open();
    for sql in [
        "ATTACH DATABASE '/tmp/evil.db' AS evil",
        "PRAGMA table_list",
        "PRAGMA query_only = OFF",
    ] {
        let error = fx.refused(sql).await;
        assert_eq!(error.reason(), ErrorReason::InvalidArgument, "{sql}");
    }
}

/// A CTE that *names* a whitelisted view does not launder a read of something
/// else: the authorizer sees the underlying table, not the alias.
#[tokio::test]
async fn a_cte_cannot_launder_a_forbidden_table() {
    let fx = Fx::open();
    let error = fx
        .refused(
            "WITH analytics_messages AS (SELECT * FROM api_tokens) \
             SELECT * FROM analytics_messages",
        )
        .await;
    assert_eq!(error.reason(), ErrorReason::InvalidArgument);
}

/// A **recursive** CTE named after a whitelisted relation cannot launder
/// itself either — this is the one the sibling test above does not catch.
///
/// # The bug this pins
///
/// `is_allowed_read` keys on the name SQLite resolved, and for a CTE that is
/// the CTE's own name rather than a real relation. With `SQLITE_RECURSIVE`
/// allowed, a recursive CTE *named after* an allowed view (or one of their
/// backing tables) passed every layer: it prepared, reported
/// `readonly() == true`, and returned a single `count(*)` row so `MAX_ROWS`
/// never fired. `UNION` then grew its dedup table without bound, and read
/// connections set `PRAGMA temp_store = MEMORY`, so it grew in RAM —
/// a remote OOM reachable by any `mail.read` + `ai.invoke` caller.
///
/// `a_cte_cannot_launder_a_forbidden_table` misses it because that CTE's
/// *body* reads a forbidden table; here the body reads only the CTE itself,
/// so there is no forbidden name for the authorizer to see. The refusal has
/// to come from denying the recursion, not from the read.
#[tokio::test]
async fn a_recursive_cte_named_after_an_allowed_relation_is_refused() {
    let fx = Fx::open();
    for sql in [
        // Shadowing a whitelisted view.
        "WITH RECURSIVE analytics_messages(x) AS \
         (SELECT 1 UNION SELECT x + 1 FROM analytics_messages) \
         SELECT count(*) FROM analytics_messages",
        // Shadowing a backing table.
        "WITH RECURSIVE messages(x) AS \
         (SELECT 1 UNION SELECT x + 1 FROM messages) \
         SELECT count(*) FROM messages",
        // `UNION ALL` is the CPU-spin variant rather than the memory one.
        "WITH RECURSIVE analytics_messages(x) AS \
         (SELECT 1 UNION ALL SELECT x + 1 FROM analytics_messages) \
         SELECT count(*) FROM analytics_messages",
        // A name nothing whitelists, to show the denial is of the recursion
        // itself rather than of the shadowing.
        "WITH RECURSIVE counter(x) AS \
         (SELECT 1 UNION SELECT x + 1 FROM counter) \
         SELECT count(*) FROM counter",
    ] {
        let error = fx.refused(sql).await;
        assert_eq!(error.reason(), ErrorReason::InvalidArgument, "{sql}");
    }
}

/// A subquery is not a blind spot either.
#[tokio::test]
async fn a_subquery_over_a_forbidden_table_is_refused() {
    let fx = Fx::open();
    let error = fx
        .refused("SELECT (SELECT count(*) FROM api_tokens) AS n FROM analytics_messages LIMIT 1")
        .await;
    assert_eq!(error.reason(), ErrorReason::InvalidArgument);
}

#[tokio::test]
async fn a_function_outside_the_allow_list_is_refused() {
    let fx = Fx::open();
    // `load_extension` is the one that matters; `randomblob` is the memory
    // amplifier. Both are real SQLite functions, so a passing test means the
    // authorizer denied them rather than SQLite failing to find them.
    for sql in [
        "SELECT randomblob(1000000) AS b FROM analytics_messages LIMIT 1",
        "SELECT sqlite_version() AS v",
    ] {
        let error = fx.refused(sql).await;
        assert_eq!(error.reason(), ErrorReason::InvalidArgument, "{sql}");
    }
}

#[tokio::test]
async fn a_second_statement_is_refused_rather_than_silently_dropped() {
    let fx = Fx::open();
    let id = fx.add("ada@example.com", "Hi", T0, false);
    let error = fx
        .refused("SELECT 1 AS n FROM analytics_messages; DELETE FROM messages")
        .await;
    assert_eq!(error.reason(), ErrorReason::InvalidArgument);
    assert!(error.to_string().contains("single statement"), "{error}");
    let survived: i64 = fx
        .db
        .with_read(|c| {
            c.query_row("SELECT count(*) FROM messages WHERE id = ?1", [id], |r| {
                r.get(0)
            })
        })
        .unwrap();
    assert_eq!(survived, 1);
}

/// A trailing semicolon is politeness, not a second statement.
#[tokio::test]
async fn a_trailing_semicolon_is_accepted() {
    let fx = Fx::open();
    fx.add("ada@example.com", "Hi", T0, false);
    let result = fx
        .run("SELECT count(*) AS n FROM analytics_messages;  \n ")
        .await
        .unwrap();
    assert_eq!(result.rows[0][0], Cell::Integer(1));
}

/// A semicolon inside a literal is not a statement separator, and refusing it
/// would make a legitimate query unwritable.
#[test]
fn a_semicolon_inside_a_literal_or_comment_is_not_a_separator() {
    assert!(single_statement("SELECT ';' AS x FROM analytics_messages").is_ok());
    assert!(single_statement("SELECT 1 -- a; comment\nFROM analytics_messages").is_ok());
    assert!(single_statement("SELECT 1 /* a; comment */ FROM analytics_messages").is_ok());
    assert!(single_statement("SELECT \"a;b\" FROM analytics_messages").is_ok());
    assert!(single_statement("SELECT [a;b] FROM analytics_messages").is_ok());
    assert!(single_statement("SELECT 1; SELECT 2").is_err());
}

// ---------------------------------------------------------------------------
// Bounds
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_empty_or_over_long_statement_is_refused() {
    let fx = Fx::open();
    assert_eq!(
        fx.refused("   ").await.reason(),
        ErrorReason::InvalidArgument
    );
    let long = format!(
        "SELECT count(*) AS n FROM analytics_messages WHERE subject = '{}'",
        "x".repeat(MAX_SQL_LEN)
    );
    let error = fx.refused(&long).await;
    assert!(error.to_string().contains("at most"), "{error}");
}

#[tokio::test]
async fn too_many_parameters_are_refused() {
    let fx = Fx::open();
    let params: Vec<Value> = (0..=MAX_PARAMS).map(|i| Value::Integer(i as i64)).collect();
    let error = run(
        &fx.db,
        &CancellationToken::new(),
        "SELECT 1 AS n FROM analytics_messages",
        &params,
    )
    .await
    .unwrap_err();
    assert_eq!(error.reason(), ErrorReason::InvalidArgument);
}

/// The row cap stops the statement and *says* it did. A silently truncated
/// result labelled complete is the failure mode this guards.
#[tokio::test]
async fn the_row_cap_stops_the_statement_and_reports_it() {
    let fx = Fx::open();
    for i in 0..(MAX_ROWS as i64 + 5) {
        fx.add("ada@example.com", "Hi", T0 + i, false);
    }
    let result = fx
        .run("SELECT message_id AS id FROM analytics_messages ORDER BY id")
        .await
        .unwrap();
    assert_eq!(result.rows.len(), MAX_ROWS);
    assert!(result.truncated, "the cap fired without saying so");
}

#[tokio::test]
async fn too_many_columns_are_refused() {
    let fx = Fx::open();
    fx.add("ada@example.com", "Hi", T0, false);
    let columns: Vec<String> = (0..=MAX_COLUMNS).map(|i| format!("{i} AS c{i}")).collect();
    let sql = format!(
        "SELECT {} FROM analytics_messages LIMIT 1",
        columns.join(", ")
    );
    let error = fx.refused(&sql).await;
    assert!(error.to_string().contains("columns"), "{error}");
}

#[tokio::test]
async fn an_over_long_cell_is_truncated_rather_than_carried_whole() {
    let fx = Fx::open();
    fx.add(
        "ada@example.com",
        &"x".repeat(MAX_CELL_CHARS * 2),
        T0,
        false,
    );
    let result = fx
        .run("SELECT subject AS s FROM analytics_messages LIMIT 1")
        .await
        .unwrap();
    match &result.rows[0][0] {
        Cell::Text(text) => assert_eq!(text.chars().count(), MAX_CELL_CHARS + 1),
        other => panic!("expected text, got {other:?}"),
    }
}

/// A cancelled query is an error, never an empty result set: a report of "no
/// rows" from a scan that was interrupted is a wrong answer wearing a right
/// one's clothes.
#[tokio::test]
async fn a_cancelled_query_errors_rather_than_returning_no_rows() {
    let fx = Fx::open();
    fx.add("ada@example.com", "Hi", T0, false);
    let cancel = CancellationToken::new();
    cancel.cancel();
    let error = run(
        &fx.db,
        &cancel,
        "SELECT count(*) AS n FROM analytics_messages",
        &[],
    )
    .await
    .unwrap_err();
    assert_eq!(error.reason(), ErrorReason::Cancelled);
}

/// A pooled connection must not keep the authorizer after the guarded query
/// finishes — an authorizer left installed would deny every unrelated read the
/// pool hands that connection to next.
#[tokio::test]
async fn the_authorizer_does_not_outlive_the_query_on_a_pooled_connection() {
    let fx = Fx::open();
    fx.add("ada@example.com", "Hi", T0, false);
    // Run enough guarded queries that every connection in the read pool has
    // certainly been used at least once.
    for _ in 0..16 {
        let _ = fx.run("SELECT count(*) AS n FROM analytics_messages").await;
        let _ = fx.run("SELECT * FROM api_tokens").await;
    }
    // An ordinary read, through the same pool, over a table the sandbox denies.
    let count: i64 = fx
        .db
        .read(|conn| conn.query_row("SELECT count(*) FROM messages", [], |row| row.get(0)))
        .await
        .unwrap();
    assert_eq!(count, 1, "the authorizer leaked onto a pooled connection");
}

/// The message a refused query comes back with has to name what it tried to
/// reach; a bare "invalid argument" is unactionable for a caller whose model
/// wrote the SQL.
#[tokio::test]
async fn a_refusal_names_the_allowed_views() {
    let fx = Fx::open();
    let error = fx.refused("SELECT * FROM api_tokens").await;
    let text = error.to_string();
    assert!(text.contains("api_tokens"), "{text}");
    assert!(text.contains("analytics_messages"), "{text}");
}
