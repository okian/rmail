//! The read-only SQL sandbox `AskAnalytics` runs model-written queries in
//! (task 72, prd.md feature 61).
//!
//! This module is the whole safety argument for letting a language model write
//! SQL against a mailbox, and it is deliberately the only place in the build
//! where a statement's *text* comes from outside. Nothing here trusts the
//! model, the prompt, or the parser it happens to be built against.
//!
//! # Four independent layers, in the order they fire
//!
//! 1. **The connection cannot write.** Every pooled reader is opened with
//!    `PRAGMA query_only = ON` ([`crate::storage`]), so a statement that
//!    somehow got past everything below still cannot mutate a byte. This layer
//!    predates the feature and is not something this module can weaken.
//! 2. **A SQLite authorizer denies everything but a whitelisted read.**
//!    Installed for the duration of one `prepare`, it sees every table, column
//!    and function a statement touches — *including through views and
//!    subqueries, and including names the statement never spells* — and
//!    returns `Deny` for all of them except [`AuthAction::Select`], a
//!    [`AuthAction::Read`] reaching a table through one of
//!    [`ALLOWED_VIEWS`] (or referencing one of their backing tables with no
//!    column read), and a [`AuthAction::Function`] on
//!    [`ALLOWED_FUNCTIONS`]. `SQLITE_RECURSIVE` is denied outright — see
//!    `authorize` for the remote OOM that allowing it opened.
//!    This is the layer that matters: it is enforced by
//!    SQLite's own name resolution rather than by a regex over a string, so
//!    `SELECT token FROM api_tokens`, `ATTACH`, `PRAGMA`, a CTE that shadows a
//!    view name, and `INSERT ... RETURNING` all fail at prepare time with the
//!    denied name in the error.
//! 3. **The prepared statement must be read-only.** `Statement::readonly()`
//!    asks SQLite directly. Redundant with both layers above, kept because it
//!    is one line and because it is the check that survives a future SQLite
//!    version teaching the authorizer a new action code this build maps to
//!    [`AuthAction::Unknown`].
//! 4. **The work is bounded, not just the result.** [`MAX_ROWS`],
//!    [`MAX_COLUMNS`] and [`MAX_CELL_CHARS`] cap what comes back, but a
//!    statement whose *work* explodes while its output stays small
//!    (`SELECT count(*) FROM a, a, a` returns one row) is stopped by
//!    [`MAX_STEPS`] via a progress handler, and anything neither of those
//!    catches is stopped by [`MAX_DURATION`]. The wall clock is load-bearing
//!    rather than belt-and-braces: the handler's cancellation token descends
//!    from the daemon's *shutdown* token, so it does not fire when a caller
//!    disconnects or its deadline expires, and without a timer on this side a
//!    runaway statement would hold a blocking-pool thread and a pooled
//!    connection until the process ended.
//!
//! A textual denylist (`"reject any statement containing DELETE"`) is
//! deliberately *not* one of the layers. It is the layer everyone writes first
//! and it is worth nothing: it cannot see through a view, it cannot tell a
//! keyword from a column called `delete_after`, and it fails open on anything
//! its author did not think of. The one string-shaped check kept here is
//! [`single_statement`], which exists for a reason a denylist does not have —
//! `sqlite3_prepare_v2` compiles the *first* statement of its input and hands
//! back a tail pointer, so a trailing `; DROP TABLE …` would never be seen by
//! the authorizer at all, because it would never be compiled.
//!
//! # Parameters are bound, never interpolated
//!
//! The model returns SQL with `?` placeholders plus a list of values. The
//! values are bound as [`rusqlite::types::Value`], so a value never reaches
//! the parser and cannot change what the statement means — the same property
//! [`crate::query::compile`] gets by making the model write a *query string*
//! that is re-parsed rather than SQL. Here the statement really is SQL, which
//! is exactly why the parameter half must not be.

#[cfg(test)]
mod tests;

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::types::{Value, ValueRef};
use rusqlite::Connection;
use tokio_util::sync::CancellationToken;

use crate::error::Error;
use crate::retrieve::cancel::interruptible_read;
use crate::storage::Database;

/// The only relations a model-written statement may read.
///
/// Views, all of them created by `V50__analytics_views.sql`, none of them
/// projecting a body, a raw octet, a credential or a token. A name that is not
/// on this list cannot be read even transitively: the authorizer sees the read
/// of `messages.subject` that `analytics_messages` performs on the model's
/// behalf and admits it only because the *accessor* is a listed view.
pub const ALLOWED_VIEWS: &[&str] = &[
    "analytics_messages",
    "analytics_senders",
    "analytics_daily",
    "analytics_threads",
    "analytics_mailboxes",
    "analytics_contacts",
];

/// The base tables the views above read on the caller's behalf.
///
/// Listed separately from [`ALLOWED_VIEWS`] because they are admissible only
/// *through* a view: a read of `messages` with no accessor is a statement that
/// named the base table itself, which is denied. Keeping the two lists apart
/// is what makes that distinction expressible.
const VIEW_BACKING_TABLES: &[&str] = &[
    "messages",
    "mailboxes",
    "accounts",
    "flags",
    "threads",
    "contacts",
];

/// SQL functions a model-written statement may call.
///
/// Everything here is pure arithmetic, string handling or aggregation over
/// values already in the result set. Deliberately absent: `load_extension`,
/// `readfile`/`writefile`, `randomblob`, `zeroblob` (a memory amplifier),
/// `sqlite_version`/`sqlite_source_id` (fingerprinting), `vec_*` (the vector
/// extension — nothing an analytics question needs, and it takes blobs), and
/// every window function's `filter`-less friends that would let a query walk
/// the whole table to answer a question about one row.
///
/// Window functions *are* allowed (`row_number`, `rank`, `lag`, …): "the third
/// busiest sender each month" is an ordinary analytics question, and they
/// cannot reach anything the underlying read did not already admit.
/// Sorted, because [`authorize`] binary-searches it —
/// `the_function_allow_list_is_sorted_and_unique` fails if that ever stops
/// being true, and the failure mode it prevents is a *legitimate* function
/// being denied, which reads to a caller as "the model wrote bad SQL".
///
/// Grouped by purpose in the comments rather than by position: aggregates
/// (`avg`, `count`, `group_concat`, `max`, `min`, `sum`, `total`), scalar
/// arithmetic and null handling, string functions, date functions — which is
/// how a window is expressed — and window functions.
pub const ALLOWED_FUNCTIONS: &[&str] = &[
    "abs",
    "avg",
    "cast",
    "coalesce",
    "count",
    "date",
    "datetime",
    "dense_rank",
    "first_value",
    "group_concat",
    "ifnull",
    "iif",
    "instr",
    "julianday",
    "lag",
    "last_value",
    "lead",
    "length",
    "like",
    "lower",
    "ltrim",
    "max",
    "min",
    "ntile",
    "nullif",
    "percent_rank",
    "printf",
    "rank",
    "replace",
    "round",
    "row_number",
    "rtrim",
    "strftime",
    "substr",
    "sum",
    "time",
    "total",
    "trim",
    "unixepoch",
    "upper",
];

/// Most rows one answer may carry back.
///
/// A model asked for "every message last year" will happily write a statement
/// that returns a hundred thousand rows, and the caller is a gRPC response and
/// a prompt for the narrating call after it. The statement is *stopped* at the
/// cap rather than truncated silently — see [`QueryResult::truncated`], which
/// is reported so a narrative can say "the first 500 of them" instead of
/// implying it saw everything.
pub const MAX_ROWS: usize = 500;

/// Most columns one answer may carry back. A `SELECT *` over the widest view
/// here is well under this; anything past it is a model that has confused a
/// report with a data dump.
pub const MAX_COLUMNS: usize = 32;

/// Longest single cell, in characters. Subjects and addresses are far shorter;
/// a `group_concat` over a whole mailbox is what this bounds.
pub const MAX_CELL_CHARS: usize = 512;

/// Longest statement accepted from the model.
///
/// The authorizer bounds what a statement may *touch*; this bounds how much
/// there is to compile. `sqlite3_prepare` is not free on a pathological input.
pub const MAX_SQL_LEN: usize = 4_000;

/// Most bound parameters one statement may carry.
pub const MAX_PARAMS: usize = 32;

/// Virtual-machine steps one analytics question may spend.
///
/// The step budget and [`MAX_DURATION`] catch different shapes, which is why
/// both exist. A join whose *work* explodes while its output stays small —
/// `SELECT count(*) FROM a, a, a` returns one row — never trips [`MAX_ROWS`],
/// and may never trip a wall clock either on a fast machine; it trips this.
/// SQLite calls the progress handler every `PROGRESS_OPS` opcodes, so the
/// effective granularity is that, not one step.
pub const MAX_STEPS: u64 = 50_000_000;

/// Opcodes between progress-handler calls. Small enough that the budget and
/// the interrupt are responsive, large enough that the callback is not itself
/// the cost.
const PROGRESS_OPS: std::os::raw::c_int = 10_000;

/// Wall clock one analytics question may spend.
///
/// Catches whatever the step budget's accounting does not, including a single
/// SQLite operation that allocates for a long time inside one step. It is also
/// the only bound that fires when the caller has gone away: the handler passes
/// a token derived from the daemon's *shutdown* token, which does not fire on
/// a client disconnect or an expired deadline, so without a timer on this side
/// a runaway statement would hold a blocking-pool thread and a pooled
/// connection until the process ended.
pub const MAX_DURATION: Duration = Duration::from_secs(20);

/// One value coming back from a model-written query.
///
/// Deliberately not `rusqlite::types::Value`: a blob has no place in an
/// analytics answer (nothing the views project is one) and rendering one into
/// a narrative prompt would be a way to push arbitrary bytes at the model. A
/// blob column therefore arrives as [`Cell::Unsupported`] rather than as
/// content.
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    /// SQL `NULL`.
    Null,
    /// An integer.
    Integer(i64),
    /// A float.
    Real(f64),
    /// Text, truncated at [`MAX_CELL_CHARS`] characters.
    Text(String),
    /// A value of a type this surface does not carry (today, only a blob).
    Unsupported,
}

impl Cell {
    /// Render for display and for the narrating prompt.
    #[must_use]
    pub fn render(&self) -> String {
        match self {
            Self::Null => String::new(),
            Self::Integer(v) => v.to_string(),
            Self::Real(v) => format!("{v}"),
            Self::Text(v) => v.clone(),
            Self::Unsupported => "<binary>".to_owned(),
        }
    }

    fn from_ref(value: ValueRef<'_>) -> Self {
        match value {
            ValueRef::Null => Self::Null,
            ValueRef::Integer(v) => Self::Integer(v),
            ValueRef::Real(v) => Self::Real(v),
            ValueRef::Text(bytes) => {
                let text = String::from_utf8_lossy(bytes);
                Self::Text(truncate_chars(&text, MAX_CELL_CHARS))
            }
            ValueRef::Blob(_) => Self::Unsupported,
        }
    }
}

/// Keep at most `max` characters, appending an ellipsis when anything was cut.
fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

/// The rows one model-written query produced.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct QueryResult {
    /// Column names, as SQLite reports them.
    pub columns: Vec<String>,
    /// Rows, at most [`MAX_ROWS`] of them.
    pub rows: Vec<Vec<Cell>>,
    /// Whether the statement had more rows to give when the cap stopped it.
    pub truncated: bool,
}

/// Run one model-written statement against the analytics views.
///
/// # Errors
///
/// [`Error::InvalidArgument`] when the statement is empty, over
/// [`MAX_SQL_LEN`], not a single statement, carries too many parameters, is
/// refused by the authorizer (naming what it tried to touch), is not read-only,
/// or is not valid SQL at all. [`Error::ResourceExhausted`] when the statement
/// burns more virtual-machine steps than [`MAX_STEPS`] allows.
/// [`Error::DeadlineExceeded`] when it is still running after
/// [`MAX_DURATION`]. [`Error::Cancelled`] if `cancel` fires while it runs. A
/// mapped storage error otherwise.
#[tracing::instrument(skip(db, cancel, sql, params), fields(rows, truncated), err)]
pub async fn run(
    db: &Database,
    cancel: &CancellationToken,
    sql: &str,
    params: &[Value],
) -> Result<QueryResult, Error> {
    let sql = validate(sql)?;
    if params.len() > MAX_PARAMS {
        return Err(Error::invalid_argument(format!(
            "an analytics query may bind at most {MAX_PARAMS} parameters, not {}",
            params.len()
        )));
    }
    let params = params.to_vec();

    // Two independent stops on a statement that will not finish, because they
    // catch different shapes. The step budget catches a join whose *work*
    // explodes while its output stays small — `SELECT count(*) FROM a, a, a`
    // returns one row, so `MAX_ROWS` never fires on it. The wall clock catches
    // whatever the step budget's own accounting does not, including a single
    // SQLite operation that allocates for a long time inside one step.
    //
    // The deadline is a *child* of `cancel` rather than `cancel` itself, and
    // that matters: the handler passes a token derived from the daemon's
    // shutdown token, which does not fire when a caller disconnects. Without a
    // timer on this side, `interruptible_read`'s watcher would have nothing to
    // wake it and a runaway statement would hold a blocking-pool thread and a
    // pooled connection until the process ended.
    let budget = Arc::new(AtomicBool::new(false));
    let tripped = Arc::clone(&budget);
    let deadline = cancel.child_token();
    let timer = {
        let deadline = deadline.clone();
        tokio::spawn(async move {
            tokio::time::sleep(MAX_DURATION).await;
            deadline.cancel();
        })
    };
    let result = interruptible_read(db, &deadline, move |conn| {
        execute(conn, &sql, &params, &tripped)
    })
    .await;
    timer.abort();
    let result = result?;

    let Some(result) = result else {
        // The statement was interrupted. Which of the three reasons decides
        // the code the caller sees, and they are genuinely different advice:
        // retry later, ask a narrower question, or nothing at all.
        if budget.load(Ordering::Relaxed) {
            return Err(Error::resource_exhausted(format!(
                "the analytics query exceeded the {MAX_STEPS}-step budget one question may \
                 spend; ask for a narrower slice of the mailbox"
            )));
        }
        if cancel.is_cancelled() {
            return Err(Error::cancelled(
                "the analytics query was cancelled while it ran",
            ));
        }
        return Err(Error::deadline_exceeded(format!(
            "the analytics query was still running after {}s and was stopped",
            MAX_DURATION.as_secs()
        )));
    };
    let result = result?;
    let span = tracing::Span::current();
    span.record("rows", result.rows.len());
    span.record("truncated", result.truncated);
    Ok(result)
}

/// The string-level checks, which are only the ones a compiler cannot make.
///
/// Returns the trimmed statement. See the module docs on why there is no
/// keyword denylist here.
fn validate(sql: &str) -> Result<String, Error> {
    let sql = sql.trim();
    if sql.is_empty() {
        return Err(Error::invalid_argument(
            "the model produced no SQL for this question",
        ));
    }
    if sql.len() > MAX_SQL_LEN {
        return Err(Error::invalid_argument(format!(
            "an analytics query must be at most {MAX_SQL_LEN} bytes"
        )));
    }
    single_statement(sql)?;
    Ok(sql.to_owned())
}

/// Refuse anything after the first statement.
///
/// Not a stylistic rule. `sqlite3_prepare_v2` compiles one statement and
/// returns a pointer to whatever follows it; `rusqlite::Connection::prepare`
/// discards that tail. So a second statement is never compiled, never seen by
/// the authorizer, and — on a connection that could write — would be executed
/// by a caller that used `execute_batch` instead. Refusing it here means this
/// module never depends on which of those two a future edit reaches for.
///
/// Semicolons inside string literals, identifiers and comments do not count,
/// which is why this scans rather than splitting on `;`.
fn single_statement(sql: &str) -> Result<(), Error> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum State {
        Code,
        Single,
        Double,
        Bracket,
        Backtick,
        LineComment,
        BlockComment,
    }
    let bytes = sql.as_bytes();
    let mut state = State::Code;
    let mut index = 0usize;
    while index < bytes.len() {
        let byte = bytes[index];
        let next = bytes.get(index + 1).copied();
        match state {
            State::Code => match (byte, next) {
                (b'\'', _) => state = State::Single,
                (b'"', _) => state = State::Double,
                (b'[', _) => state = State::Bracket,
                (b'`', _) => state = State::Backtick,
                (b'-', Some(b'-')) => {
                    state = State::LineComment;
                    index += 1;
                }
                (b'/', Some(b'*')) => {
                    state = State::BlockComment;
                    index += 1;
                }
                (b';', _) => {
                    // A trailing `;` with only whitespace after it is one
                    // statement written politely, not two.
                    let rest = sql.get(index + 1..).unwrap_or("");
                    if !rest.trim().is_empty() {
                        return Err(Error::invalid_argument(
                            "an analytics query must be a single statement; everything after \
                             the first `;` would never be compiled and is refused rather than \
                             silently dropped",
                        ));
                    }
                    return Ok(());
                }
                _ => {}
            },
            // `''` inside a single-quoted literal is an escaped quote, and the
            // two-character skip below handles it: the first `'` closes, the
            // second re-opens.
            State::Single if byte == b'\'' => state = State::Code,
            State::Double if byte == b'"' => state = State::Code,
            State::Bracket if byte == b']' => state = State::Code,
            State::Backtick if byte == b'`' => state = State::Code,
            State::LineComment if byte == b'\n' => state = State::Code,
            State::BlockComment if byte == b'*' && next == Some(b'/') => {
                state = State::Code;
                index += 1;
            }
            _ => {}
        }
        index += 1;
    }
    Ok(())
}

/// Prepare under the authorizer, check read-only-ness, and read bounded rows.
///
/// Synchronous, and runs on the blocking pool through
/// [`interruptible_read`].
fn execute(
    conn: &Connection,
    sql: &str,
    params: &[Value],
    budget: &Arc<AtomicBool>,
) -> rusqlite::Result<Result<QueryResult, Error>> {
    let denied: Arc<Mutex<BTreeSet<String>>> = Arc::new(Mutex::new(BTreeSet::new()));
    let guard = SandboxGuard::install(conn, Arc::clone(&denied), Arc::clone(budget));

    let prepared = conn.prepare(sql);
    let mut stmt = match prepared {
        Ok(stmt) => stmt,
        Err(error) => {
            drop(guard);
            let refused = denied
                .lock()
                .map(|set| set.iter().cloned().collect::<Vec<_>>());
            let refused = refused.unwrap_or_default();
            if !refused.is_empty() {
                return Ok(Err(Error::invalid_argument(format!(
                    "the analytics query was refused: it used {}, which the read-only analytics \
                     sandbox does not permit. Readable views are {}",
                    refused.join(", "),
                    ALLOWED_VIEWS.join(", ")
                ))));
            }
            // Not every `prepare` failure is the model's fault, and reporting
            // one that is not as `INVALID_ARGUMENT` sends the caller to fix a
            // question that was fine. `sqlite3_prepare_v2` returns
            // `SQLITE_INTERRUPT` when the interrupt flag is already set, and
            // `SQLITE_BUSY`/`SQLITE_IOERR`/`SQLITE_NOMEM` for conditions that
            // have nothing to do with the statement — so only a genuine
            // compile error is the model's.
            if let rusqlite::Error::SqliteFailure(inner, _) = &error {
                if !matches!(
                    inner.code,
                    rusqlite::ErrorCode::Unknown
                        | rusqlite::ErrorCode::AuthorizationForStatementDenied
                ) {
                    return Err(error);
                }
            }
            return Ok(Err(Error::invalid_argument(format!(
                "the model produced SQL this database cannot compile: {error}"
            ))));
        }
    };

    if !stmt.readonly() {
        drop(stmt);
        drop(guard);
        return Ok(Err(Error::invalid_argument(
            "an analytics query must be read-only",
        )));
    }
    let column_count = stmt.column_count();
    if column_count > MAX_COLUMNS {
        drop(stmt);
        drop(guard);
        return Ok(Err(Error::invalid_argument(format!(
            "an analytics query may return at most {MAX_COLUMNS} columns, not {column_count}"
        ))));
    }
    let columns: Vec<String> = stmt
        .column_names()
        .into_iter()
        .map(std::borrow::ToOwned::to_owned)
        .collect();

    let mut rows_out: Vec<Vec<Cell>> = Vec::new();
    let mut truncated = false;
    {
        let mut rows = stmt.query(rusqlite::params_from_iter(params.iter()))?;
        while let Some(row) = rows.next()? {
            if rows_out.len() >= MAX_ROWS {
                truncated = true;
                break;
            }
            let mut cells = Vec::with_capacity(column_count);
            for index in 0..column_count {
                cells.push(Cell::from_ref(row.get_ref(index)?));
            }
            rows_out.push(cells);
        }
    }
    drop(stmt);
    drop(guard);

    Ok(Ok(QueryResult {
        columns,
        rows: rows_out,
        truncated,
    }))
}

/// Installs the authorizer *and* the step budget for as long as it lives, and
/// removes both on drop.
///
/// The connection is *pooled*. An authorizer left behind would silently govern
/// every unrelated query the pool hands that connection to next — which would
/// be a correctness bug of exactly the shape
/// [`crate::retrieve::cancel`]'s module docs describe for a stale interrupt
/// handle. A drop guard rather than a `conn.authorizer(None)` at the end of
/// the happy path, because there are five early returns above and an unwind is
/// possible at any of them.
struct SandboxGuard<'a> {
    conn: &'a Connection,
}

impl<'a> SandboxGuard<'a> {
    /// Install the authorizer *and* the step budget.
    ///
    /// `budget` is set when the statement is stopped for exceeding
    /// [`MAX_STEPS`], which is what lets the caller tell that reason apart
    /// from a cancellation and from the wall clock — three interruptions that
    /// are indistinguishable at the rusqlite boundary but are genuinely
    /// different advice to give a user.
    fn install(
        conn: &'a Connection,
        denied: Arc<Mutex<BTreeSet<String>>>,
        budget: Arc<AtomicBool>,
    ) -> Self {
        conn.authorizer(Some(move |context: AuthContext<'_>| {
            let verdict = authorize(&context);
            if verdict == Authorization::Deny {
                if let Ok(mut set) = denied.lock() {
                    set.insert(describe(&context.action));
                }
            }
            verdict
        }));
        let mut steps: u64 = 0;
        conn.progress_handler(
            PROGRESS_OPS,
            Some(move || {
                steps = steps.saturating_add(PROGRESS_OPS.unsigned_abs().into());
                if steps >= MAX_STEPS {
                    // Returning true aborts the statement with
                    // `SQLITE_INTERRUPT`, the same shape a cancellation
                    // produces — hence the flag.
                    budget.store(true, Ordering::Relaxed);
                    return true;
                }
                false
            }),
        );
        Self { conn }
    }
}

impl Drop for SandboxGuard<'_> {
    fn drop(&mut self) {
        self.conn
            .authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
        // The connection is pooled: a progress handler left installed would
        // count another query's opcodes against a budget that is no longer
        // anyone's, and abort it.
        self.conn.progress_handler(0, None::<fn() -> bool>);
    }
}

/// The rule. Default-deny, and every `Allow` arm is exhaustively enumerated.
///
/// `Deny` rather than `Ignore` for a refused read: `Ignore` makes SQLite
/// substitute `NULL` for the column and *carry on*, which would turn "you may
/// not read `api_tokens`" into a query that succeeds and returns a column of
/// nulls. A caller cannot tell that from an empty table, and a narrative
/// written over it would be a confident answer to a question that was never
/// allowed to be asked.
fn authorize(context: &AuthContext<'_>) -> Authorization {
    match context.action {
        // The statement is a SELECT. Says nothing about what it reads; the
        // `Read` arm below is what governs that.
        //
        // `AuthAction::Recursive` is deliberately **not** allowed here, and
        // falls through to the catch-all `Deny`. Allowing it was a remote OOM:
        // `is_allowed_read` keys on the name SQLite resolved, which for a CTE
        // is the CTE's own name rather than a real relation, so a recursive
        // CTE *named after* a whitelisted view laundered itself past every
        // other layer —
        //
        //     WITH RECURSIVE analytics_messages(x) AS
        //       (SELECT 1 UNION SELECT x+1 FROM analytics_messages)
        //     SELECT count(*) FROM analytics_messages
        //
        // prepares, reports `readonly() == true`, and returns a single row, so
        // `MAX_ROWS` never fires either. `UNION` grows its dedup table without
        // bound and read connections set `PRAGMA temp_store = MEMORY`, so it
        // grows in RAM; `UNION ALL` spins a blocking-pool thread instead.
        // Reachable by any `mail.read` + `ai.invoke` caller.
        //
        // Denying the action outright costs nothing this feature needs: no
        // analytics question the compiler emits requires a recursive CTE, and
        // a query that wants one now fails at prepare with `Recursive` named.
        AuthAction::Select => Authorization::Allow,
        AuthAction::Read {
            table_name,
            column_name,
        } => {
            if is_allowed_read(table_name, column_name, context.accessor) {
                Authorization::Allow
            } else {
                Authorization::Deny
            }
        }
        AuthAction::Function { function_name } => {
            let name = function_name.to_ascii_lowercase();
            if ALLOWED_FUNCTIONS.binary_search(&name.as_str()).is_ok() {
                Authorization::Allow
            } else {
                Authorization::Deny
            }
        }
        _ => Authorization::Deny,
    }
}

/// Whether one table read is admissible.
///
/// Three shapes are allowed and no others:
///
/// 1. **The statement named a whitelisted view.** `analytics_messages` and its
///    five siblings.
/// 2. **A whitelisted view is reading one of its own backing tables** —
///    `accessor` is the view. This is why the base tables are not simply added
///    to [`ALLOWED_VIEWS`]: `SELECT raw FROM messages` and `SELECT subject
///    FROM analytics_messages` both produce a read of `messages`, and only the
///    second carries an accessor.
/// 3. **A backing table is *referenced* with no column read at all** — an
///    empty `column_name`. This is not a hypothetical: SQLite flattens a view
///    into its caller, and when the optimizer then drops a join whose columns
///    the outer query never used (`SELECT from_addr, count(*) FROM
///    analytics_messages GROUP BY 1` needs nothing from `accounts` or
///    `mailboxes`), it still notifies the authorizer that the table was
///    referenced — with an empty column *and no accessor*, because by then the
///    view context is gone. `the_authorizer_sees_a_view_as_the_accessor` pins
///    both halves of that mechanism, because clause 3 is only safe while
///    clause 2 carries the real column reads.
///
/// Clause 3 is restricted to [`VIEW_BACKING_TABLES`], so what it grants is
/// exactly "`messages` exists and has N rows" — which
/// `SELECT count(*) FROM analytics_messages` already grants, over the same
/// rows. `SELECT count(*) FROM api_tokens` needs the same shape of read and is
/// still denied, because `api_tokens` is on neither list.
fn is_allowed_read(table_name: &str, column_name: &str, accessor: Option<&str>) -> bool {
    let table = table_name.to_ascii_lowercase();
    if ALLOWED_VIEWS.contains(&table.as_str()) {
        return true;
    }
    if !VIEW_BACKING_TABLES.contains(&table.as_str()) {
        return false;
    }
    match accessor {
        Some(view) => ALLOWED_VIEWS.contains(&view.to_ascii_lowercase().as_str()),
        None => column_name.is_empty(),
    }
}

/// What a denied action tried to touch, for the error message.
///
/// Names are attacker-adjacent (the model wrote them), so this renders a
/// bounded description rather than echoing arbitrary text: a table name is cut
/// to 64 characters and everything else is a fixed string.
fn describe(action: &AuthAction<'_>) -> String {
    let short = |name: &str| truncate_chars(name, 64);
    match action {
        AuthAction::Read {
            table_name,
            column_name,
        } => format!("{}.{}", short(table_name), short(column_name)),
        AuthAction::Insert { table_name } => format!("INSERT into {}", short(table_name)),
        AuthAction::Update { table_name, .. } => format!("UPDATE of {}", short(table_name)),
        AuthAction::Delete { table_name } => format!("DELETE from {}", short(table_name)),
        AuthAction::DropTable { table_name } => format!("DROP TABLE {}", short(table_name)),
        AuthAction::DropView { view_name } => format!("DROP VIEW {}", short(view_name)),
        AuthAction::CreateTable { table_name } => format!("CREATE TABLE {}", short(table_name)),
        AuthAction::CreateView { view_name } => format!("CREATE VIEW {}", short(view_name)),
        AuthAction::AlterTable { table_name, .. } => format!("ALTER TABLE {}", short(table_name)),
        AuthAction::Attach { .. } => "ATTACH".to_owned(),
        AuthAction::Detach { .. } => "DETACH".to_owned(),
        AuthAction::Pragma { pragma_name, .. } => format!("PRAGMA {}", short(pragma_name)),
        AuthAction::Transaction { .. } => "a transaction control statement".to_owned(),
        AuthAction::Savepoint { .. } => "a savepoint".to_owned(),
        AuthAction::Function { function_name } => {
            format!("the function {}()", short(function_name))
        }
        AuthAction::CreateVtable { module_name, .. }
        | AuthAction::DropVtable { module_name, .. } => {
            format!("the virtual-table module {}", short(module_name))
        }
        _ => "a statement this surface does not permit".to_owned(),
    }
}

/// The schema description shown to the model, built from the same list the
/// authorizer enforces.
///
/// Hand-written column notes rather than `PRAGMA table_info`: the model needs
/// to be told that `sent_at` is unix seconds and that `direction` is a
/// heuristic, and a generated listing would carry types and no meaning. It is
/// a `&'static str` so the system prompt is byte-stable across calls, which is
/// what lets the provider's prompt cache serve it — the same discipline
/// [`crate::query::compile`]'s prompt keeps.
pub const SCHEMA_DOC: &str = "\
analytics_messages(message_id, account_id, account_name, mailbox_id, mailbox, \
thread_id, from_addr, from_name, to_addrs, cc_addrs, subject, sent_at, \
sent_day, size_bytes, has_attachments, is_read, is_flagged, is_answered, \
direction)
  One row per message. sent_at is unix seconds; sent_day is 'YYYY-MM-DD'.
  is_read/is_flagged/is_answered/has_attachments are 0 or 1.
  direction is 'inbound' or 'outbound'; outbound is decided by the folder \
looking like Sent or the From matching the account's configured username, so \
an account with aliases can under-count outbound.
  to_addrs and cc_addrs are comma-joined address lists, not tables.

analytics_senders(account_id, from_addr, from_name, messages, read_messages, \
read_rate, first_seen, last_seen, threads)
  One row per inbound sender. read_rate is between 0 and 1.

analytics_daily(account_id, sent_day, direction, messages, read_messages)
  Volume per day per direction.

analytics_threads(thread_id, account_id, subject, messages, last_message_at, \
outbound_messages, inbound_messages)

analytics_mailboxes(mailbox_id, account_id, mailbox, messages, unread_messages)

analytics_contacts(address, name, messages, last_seen)";

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod invariants {
    use super::{ALLOWED_FUNCTIONS, ALLOWED_VIEWS};

    /// `authorize` binary-searches the function list, so it has to be sorted.
    /// A test rather than a runtime sort: an unsorted list would silently
    /// deny a legitimate function, which reads as "the model wrote bad SQL".
    #[test]
    fn the_function_allow_list_is_sorted_and_unique() {
        let mut sorted = ALLOWED_FUNCTIONS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.as_slice(), ALLOWED_FUNCTIONS);
    }

    /// Every view named in the prompt is a view the authorizer admits, and
    /// vice versa. A view described to the model but denied at prepare time
    /// produces a confident query that always fails.
    #[test]
    fn every_documented_view_is_allowed() {
        for view in ALLOWED_VIEWS {
            assert!(
                super::SCHEMA_DOC.contains(view),
                "{view} is allowed but not described to the model"
            );
        }
        for line in super::SCHEMA_DOC.lines() {
            let Some(name) = line
                .split('(')
                .next()
                .filter(|n| n.starts_with("analytics_"))
            else {
                continue;
            };
            assert!(
                ALLOWED_VIEWS.contains(&name),
                "{name} is described to the model but not allowed"
            );
        }
    }
}
