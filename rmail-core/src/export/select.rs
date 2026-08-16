//! Turning a [`Selection`](super::Selection) into an ordered, bounded-memory
//! stream of message ids, plus the per-page loaders the framers need.
//!
//! # Keyset, not `OFFSET`
//!
//! [`Cursor`] walks `WHERE id > ?last ORDER BY id LIMIT ?page`. `OFFSET` would
//! be simpler and quadratic — SQLite re-scans and discards every skipped row
//! on each page, so exporting a million-message mailbox would cost half a
//! trillion discarded rows. Keying off the primary key means each page is an
//! index seek, and it means new mail arriving mid-export gets a higher `id`
//! and is either included once at the end or not at all — never duplicated
//! into an earlier page, and never used to shift a later one.
//!
//! It is **not** a snapshot. Each page is its own read transaction, so a
//! message deleted or moved out of the selection between pages simply is not
//! in the archive, and a page's flags are read at a slightly later instant
//! than its ids. Holding one transaction open for the whole export would fix
//! that and would also hold a read connection (and the WAL's reclaim point)
//! for however long a multi-gigabyte archive takes to write, which is the
//! worse trade for a daemon that is still serving mail. What is guaranteed is
//! the part that matters for an archive: nothing is written twice, and
//! nothing that existed for the whole run is skipped.
//!
//! `id` ascending is therefore both the pagination key and the archive's
//! order for a query export. It is the insertion order of the local mirror,
//! which for a synced mailbox is the order messages were first seen. A thread
//! export instead reads in conversation order, because
//! [`repo::list_thread_message_ids`] already defines that order for
//! `MailService.GetThread` and a thread is small enough to resolve in one
//! statement.
//!
//! # Rows, not distinct messages
//!
//! The unit of export is a `messages` row, which is keyed by
//! `(mailbox_id, uidvalidity, uid)`. A server that presents one RFC822
//! message in two folders — Gmail's `INBOX` plus `All Mail`, any IMAP server
//! with a copy — has two rows for it, and an unscoped export therefore
//! contains it twice, once per folder. That is what "archive this mailbox as
//! synced" means, and deduplicating by `Message-ID` would silently drop a
//! folder's copy along with the flags that differ between them. Narrow with
//! `in:` when a per-folder archive is wanted.
//!
//! # The query means what search says it means — but does not fail open
//!
//! Operators compile through [`filtermask`] — the same compiler five
//! retrievers share — and free text through
//! [`crate::retrieve::lexical`]'s own `MATCH` builder. Neither is re-derived
//! here. If `-in:Spam` or a `~`-forced-semantic term ever changes meaning,
//! it changes for export in the same commit, because there is one
//! implementation.
//!
//! Two deliberate differences from search:
//!
//! **Shape.** Search asks the lexical index for a BM25-ordered top-N; this
//! asks it for the unordered set of rowids that match at all (`id IN (SELECT
//! rowid FROM fts_messages WHERE fts_messages MATCH ?)`). An archive has no
//! cutoff.
//!
//! **Degradation.** Both compilers drop what they cannot resolve: a term that
//! produces no FTS token, a `~`-forced-semantic term (which only the dense
//! retriever can serve, and export has none), an unparseable
//! `after:lasst-week`. For search that is correct — the user still gets a
//! ranked page, weaker than they asked for. For an export it is the worst
//! possible outcome: the constraint vanishes, `SELECT id FROM messages`
//! matches *everything*, and the whole mailbox is written to disk under a
//! filename that says it is Alice's invoices. [`query_plan`] therefore
//! refuses any query whose free text or operators would only partly survive
//! compilation, with [`Error::InvalidArgument`] naming what it could not
//! enforce. Narrowing that this module cannot guarantee is not narrowing.

use std::collections::BTreeMap;

use chrono::Utc;
use rusqlite::types::Value;

use crate::error::Error;
use crate::index::fts;
use crate::query::{self, HardFilter};
use crate::repo;
use crate::retrieve::cancel::interruptible_read;
use crate::retrieve::filtermask::{self, FilterMask};
use crate::retrieve::lexical::MatchExpr;
use crate::storage::Database;

use super::{Selection, PAGE_SIZE};

use tokio_util::sync::CancellationToken;

/// A bounded-memory walk over the ids a selection resolves to.
///
/// Deliberately unaware of [`ExportOptions::limit`](super::ExportOptions::limit).
/// The limit counts messages that reached the archive, and only the framing
/// loop knows which rows did — a row whose raw was never stored is selected
/// here and written nowhere. Capping the *scan* instead would make
/// `--limit 10` silently produce eight. The cost of enforcing it upstream is
/// one over-fetched page of ids (two kilobytes), paid once at the end.
#[derive(Debug)]
pub struct Cursor {
    plan: Plan,
    /// True once the underlying source is exhausted.
    done: bool,
}

/// How a cursor produces ids.
#[derive(Debug)]
enum Plan {
    /// A thread's ids, resolved in one statement and drained from the front.
    ///
    /// A thread is bounded by the conversation, not by the mailbox, so
    /// resolving it whole costs one `i64` per member and buys the
    /// conversation order `GetThread` promises.
    Fixed(std::vec::IntoIter<i64>),
    /// A keyset scan over `messages`.
    Scan {
        /// The compiled operator predicate, if any constrains the set.
        mask: Option<CompiledMask>,
        /// The FTS5 `MATCH` expression the query's free text compiled to, if
        /// it had any.
        match_expr: Option<String>,
        /// Exclusive lower bound for the next page.
        after_id: i64,
    },
    /// The selection provably matches nothing (a hard filter that excludes
    /// every message). Distinct from an exhausted scan so no statement is
    /// prepared at all.
    Empty,
}

/// A compiled hard-filter predicate: SQL plus the values it binds.
#[derive(Debug)]
struct CompiledMask {
    sql: String,
    params: Vec<Value>,
}

impl Cursor {
    /// Resolve `selection` into a cursor.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] if a thread selection names no thread — an export
    /// that quietly produced an empty archive for a typo'd id is a data-loss
    /// report waiting to happen. A mapped storage error otherwise.
    pub async fn open(
        db: &Database,
        selection: &Selection,
        cancel: &CancellationToken,
    ) -> Result<Self, Error> {
        let plan = match selection {
            Selection::Thread(thread_id) => Plan::Fixed(thread_ids(db, *thread_id, cancel).await?),
            Selection::Query(raw) => query_plan(raw)?,
        };
        Ok(Self { plan, done: false })
    }

    /// The next page of ids, or an empty vector once the selection is
    /// exhausted.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] if the free text compiled to something
    /// FTS5 rejects, [`Error::Cancelled`] if `cancel` fired, a mapped storage
    /// error otherwise.
    pub async fn next_page(
        &mut self,
        db: &Database,
        cancel: &CancellationToken,
    ) -> Result<Vec<i64>, Error> {
        if self.done {
            return Ok(Vec::new());
        }
        let budget = PAGE_SIZE as i64;

        let page = match &mut self.plan {
            Plan::Empty => Vec::new(),
            Plan::Fixed(ids) => ids.by_ref().take(PAGE_SIZE).collect(),
            Plan::Scan {
                mask,
                match_expr,
                after_id,
            } => {
                let page = scan_page(
                    db,
                    mask.as_ref(),
                    match_expr.as_deref(),
                    *after_id,
                    budget,
                    cancel,
                )
                .await?;
                if let Some(last) = page.last() {
                    *after_id = *last;
                }
                page
            }
        };

        if page.is_empty() {
            self.done = true;
        }
        Ok(page)
    }
}

/// Resolve a thread's members, oldest first.
async fn thread_ids(
    db: &Database,
    thread_id: i64,
    cancel: &CancellationToken,
) -> Result<std::vec::IntoIter<i64>, Error> {
    let found = interruptible_read(db, cancel, move |conn| {
        let thread = repo::get_thread(conn, thread_id)?;
        if thread.is_none() {
            return Ok(None);
        }
        Ok(Some(repo::list_thread_message_ids(conn, thread_id)?))
    })
    .await?
    .ok_or_else(|| Error::cancelled("export cancelled"))?;

    let ids =
        found.ok_or_else(|| Error::not_found(format!("thread {thread_id} does not exist")))?;
    Ok(ids.into_iter())
}

/// Compile a query string into a scan plan, refusing to enforce less than it
/// was asked to.
///
/// # Errors
///
/// [`Error::InvalidArgument`] if any operator or free-text term the caller
/// wrote would be dropped by compilation — see the module docs' "Degradation"
/// section for why a partially-enforced export is worse than a refused one.
fn query_plan(raw: &str) -> Result<Plan, Error> {
    let parsed = query::parse(raw);
    // Relative dates (`after:last-week`) resolve against the moment the
    // export starts, exactly as a search issued at the same moment would.
    let filters: Vec<HardFilter> = query::plan::resolve_filters(&parsed.filters, Utc::now());

    let dropped = filtermask::unenforceable(&filters);
    if !dropped.is_empty() {
        return Err(Error::invalid_argument(format!(
            "this export cannot enforce the operator(s) {}: their values did not resolve \
             (a misspelled or inverted date is the usual cause). An export has no relevance \
             cutoff to fall back on, so ignoring a gate would archive the whole mailbox — \
             fix the operator or drop it deliberately",
            dropped.join(", ")
        )));
    }

    let match_expr = MatchExpr::build(&parsed).map(|expr| expr.full);
    // `MatchExpr::build` returns `None` for a query with no lexically usable
    // free text at all — including one whose every term is `~`-forced
    // semantic (export runs no dense retriever) or is punctuation/emoji the
    // tokenizer produces nothing from. Search treats that as "nothing to
    // rank" and leans on its other retrievers; here it would mean the free
    // text the caller typed simply does not appear in the WHERE clause.
    if match_expr.is_none() && (!parsed.terms.is_empty() || !parsed.phrases.is_empty()) {
        return Err(Error::invalid_argument(
            "this export cannot enforce the query's free text: every term is either \
             `~`-forced semantic (an export has no embedding retriever) or contains no \
             character the index tokenizes. An export has no relevance cutoff to fall back \
             on, so ignoring it would archive the whole mailbox — use operators, or drop \
             the `~`",
        ));
    }

    let mask = match filtermask::compile(&filters) {
        FilterMask::ExcludesEverything => return Ok(Plan::Empty),
        // Nothing to gate on because nothing was asked for: an empty query is
        // "archive everything", which is a legitimate thing to ask an export
        // for and is why the RPC requires an explicit selection and the CLI an
        // explicit destination. The two checks above are what make this the
        // *only* way to reach an unconstrained scan.
        FilterMask::Unconstrained => None,
        FilterMask::Sql(mask) => Some(CompiledMask {
            sql: mask.sql,
            params: mask.params,
        }),
    };

    Ok(Plan::Scan {
        mask,
        match_expr,
        after_id: 0,
    })
}

/// One keyset page of the scan.
async fn scan_page(
    db: &Database,
    mask: Option<&CompiledMask>,
    match_expr: Option<&str>,
    after_id: i64,
    limit: i64,
    cancel: &CancellationToken,
) -> Result<Vec<i64>, Error> {
    let mut sql = String::from("SELECT id FROM messages WHERE id > ?");
    if let Some(mask) = mask {
        sql.push_str(" AND ");
        sql.push_str(&mask.sql);
    }
    if match_expr.is_some() {
        // A rowid `IN` against the FTS table, not a correlated `EXISTS`: the
        // subquery is uncorrelated and evaluated once per page, which is the
        // cheap direction here (the driving scan is `messages` by primary
        // key, and the match set does not depend on the row being tested).
        sql.push_str(" AND id IN (SELECT rowid FROM fts_messages WHERE fts_messages MATCH ?)");
    }
    sql.push_str(" ORDER BY id LIMIT ?");

    let params: Vec<Value> = mask.map(|m| m.params.clone()).unwrap_or_default();
    let match_expr = match_expr.map(str::to_owned);

    let page = interruptible_read(db, cancel, move |conn| {
        let mut stmt = conn.prepare(&sql)?;
        let mut bound: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(params.len() + 3);
        bound.push(&after_id);
        for param in &params {
            bound.push(param);
        }
        if let Some(expr) = &match_expr {
            bound.push(expr);
        }
        bound.push(&limit);
        let ids = stmt
            .query_map(bound.as_slice(), |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<i64>>>()?;
        Ok(ids)
    })
    .await
    // An FTS5 syntax error surfaces as a generic SQLite failure; this is the
    // same mapping `retrieve::lexical` applies so a malformed query is
    // INVALID_ARGUMENT rather than INTERNAL on both surfaces.
    .map_err(fts::malformed_query)?;

    page.ok_or_else(|| Error::cancelled("export cancelled"))
}

/// Load one message row, raw blob included.
///
/// # Errors
///
/// [`Error::Cancelled`] if `cancel` fired, a mapped storage error otherwise.
pub async fn load_message(
    db: &Database,
    id: i64,
    cancel: &CancellationToken,
) -> Result<Option<repo::Message>, Error> {
    interruptible_read(db, cancel, move |conn| repo::get_message(conn, id))
        .await?
        .ok_or_else(|| Error::cancelled("export cancelled"))
}

/// Load flags for a whole page in one statement.
///
/// # Errors
///
/// [`Error::Cancelled`] if `cancel` fired, a mapped storage error otherwise.
pub async fn flags_for(
    db: &Database,
    ids: &[i64],
    cancel: &CancellationToken,
) -> Result<BTreeMap<i64, Vec<String>>, Error> {
    let ids = ids.to_vec();
    interruptible_read(db, cancel, move |conn| {
        repo::list_flags_by_message(conn, &ids)
    })
    .await?
    .ok_or_else(|| Error::cancelled("export cancelled"))
}

/// Load one message's attachment metadata.
///
/// # Errors
///
/// [`Error::Cancelled`] if `cancel` fired, a mapped storage error otherwise.
pub async fn attachments_for(
    db: &Database,
    id: i64,
    cancel: &CancellationToken,
) -> Result<Vec<repo::Attachment>, Error> {
    interruptible_read(db, cancel, move |conn| repo::list_attachments(conn, id))
        .await?
        .ok_or_else(|| Error::cancelled("export cancelled"))
}

/// Every mailbox id → name, for the JSON record's `mailbox` field.
///
/// Loaded once per page rather than per message: a mailbox table is tens of
/// rows even for a heavily foldered account, and joining it into the message
/// query would drag the (large) `raw` blob through a join for no gain.
///
/// # Errors
///
/// [`Error::Cancelled`] if `cancel` fired, a mapped storage error otherwise.
pub async fn mailbox_names(
    db: &Database,
    cancel: &CancellationToken,
) -> Result<BTreeMap<i64, String>, Error> {
    interruptible_read(db, cancel, move |conn| {
        let mut stmt = conn.prepare("SELECT id, name FROM mailboxes")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        rows.collect::<rusqlite::Result<BTreeMap<i64, String>>>()
    })
    .await?
    .ok_or_else(|| Error::cancelled("export cancelled"))
}
