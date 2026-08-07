//! Saved searches: a named query string, re-run through the real search
//! pipeline on demand (prd.md, "Saved Searches & Smart Folders"; task 35).
//!
//! # A saved search stores a *query*, never results
//!
//! This is the whole design, and the one thing that would be tempting to get
//! wrong. [`SavedSearch::query`] is the raw string the user typed —
//! operators, free text, `~`/`=` sigils and all — and running a saved search
//! means handing that exact string back to the same pipeline
//! `SearchService.Search` drives (`QueryPlanner` → `retrieve::Fanout` →
//! `fuse::Fuser` → `features::FeatureExtractor` → `rank::l1::L1Ranker` →
//! `present::Presenter`). Nothing here re-implements retrieval, and nothing
//! here caches a result set.
//!
//! Storing ranked ids instead would be faster and wrong in two independent
//! ways: the snapshot goes stale the moment the next message syncs (a saved
//! "unread from my manager" search would keep answering with yesterday's
//! mail), and it would become a *second* answer to "what matches this
//! query", free to disagree with the ranker whenever weights, the corpus, or
//! the operator grammar change. Re-running the string is the only form that
//! stays correct by construction — including after a future grammar addition
//! teaches the parser an operator the saved string already contained.
//!
//! Concretely, this module deliberately exposes no `run` method: it has no
//! business owning a pipeline handle. [`SavedSearchStore::resolve_for_run`]
//! hands back the query text (stamping `last_run_at` in the same statement),
//! and `rmaild::saved_search_service` feeds it straight into `SearchApi`'s
//! own streaming entry point — the identical call `Search` itself makes, so
//! there is exactly one query path in the process and a saved search cannot
//! drift from a typed one.
//!
//! # Validation: what "unparseable" means when parsing cannot fail
//!
//! [`crate::query::parse`] never returns an error by design — a search box
//! is not a compiler front end, and anything it does not recognize degrades
//! to free text. That leaves exactly one way for a query string to be
//! useless, and [`validate_query`] rejects it: a string with nothing to
//! search *for* after parsing — no operator, no term, no phrase. `""`
//! (whitespace) and `"\"\""` (an empty quoted phrase, which the parser drops)
//! are the two shapes that reach it. Rejecting them at create time rather
//! than at run time matters because a saved search is persistent: an
//! unrunnable one is a row that fails every time it is used, forever, with
//! nothing pointing back at the moment it was created.
//!
//! A *smart folder* holds its predicate to a much stricter standard — see
//! [`crate::smart_folder`] for why a persistent, unattended predicate cannot
//! afford the same "degrade to free text" latitude a one-shot search can.

pub(crate) mod repo;

use crate::error::Error;
use crate::query::parse;
use crate::storage::Database;

/// The longest query string a saved search may hold.
///
/// Not a retrieval limit — the pipeline handles long queries fine — but a
/// bound on what a client can persist. Queries are typed by humans; anything
/// near this is a client bug or an attempt to use the table as storage.
pub const MAX_QUERY_LEN: usize = 4096;

/// The longest name a saved search may carry.
pub const MAX_NAME_LEN: usize = 128;

/// One persisted named query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SavedSearch {
    /// Row id.
    pub id: i64,
    /// Owning account.
    pub account_id: i64,
    /// The name it is invoked by, unique per account and matched
    /// case-insensitively.
    pub name: String,
    /// The raw query string, re-parsed and re-run on every invocation. See
    /// the module docs.
    pub query: String,
    /// Creation time (unix seconds).
    pub created_at: i64,
    /// Last time the query text was changed (unix seconds).
    pub updated_at: i64,
    /// Last time it was run, if ever (unix seconds).
    pub last_run_at: Option<i64>,
}

/// CRUD over saved searches.
///
/// Cheap to clone: every clone shares the same database handle.
#[derive(Debug, Clone)]
pub struct SavedSearchStore {
    db: Database,
}

impl SavedSearchStore {
    /// Build a store over `db`.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Persist a new named query.
    ///
    /// # Errors
    /// [`Error::InvalidArgument`] if `name` or `query` is empty, over-long,
    /// or the query has nothing to search for (see [`validate_query`]);
    /// [`Error::AlreadyExists`] if the account already has a search by that
    /// name (case-insensitively); [`Error::NotFound`] if `account_id` names
    /// no account. Otherwise a mapped storage error.
    #[tracing::instrument(skip(self, query), fields(account_id = account_id, name = name), err)]
    pub async fn create(
        &self,
        account_id: i64,
        name: &str,
        query: &str,
    ) -> Result<SavedSearch, Error> {
        let name = validate_name(name)?;
        let query = validate_query(query)?;
        let for_error = name.clone();

        let inserted = self
            .db
            .write(move |conn| Ok(repo::insert(conn, account_id, &name, &query)))
            .await?;

        match inserted {
            Ok(saved) => Ok(saved),
            Err(err) if repo::is_unique_violation(&err) => Err(Error::already_exists(format!(
                "a saved search named {for_error:?} already exists in this account"
            ))),
            Err(err) if repo::is_missing_reference(&err) => {
                Err(Error::not_found(format!("account {account_id}")))
            }
            Err(err) => Err(Error::from(crate::StorageError::from(err))),
        }
    }

    /// Replace an existing saved search's query text.
    ///
    /// # Errors
    /// [`Error::InvalidArgument`] as [`create`](Self::create);
    /// [`Error::NotFound`] if the account has no search by that name.
    /// Otherwise a mapped storage error.
    #[tracing::instrument(skip(self, query), fields(account_id = account_id, name = name), err)]
    pub async fn update_query(
        &self,
        account_id: i64,
        name: &str,
        query: &str,
    ) -> Result<SavedSearch, Error> {
        let name = validate_name(name)?;
        let query = validate_query(query)?;
        let for_error = name.clone();
        self.db
            .write(move |conn| repo::update_query(conn, account_id, &name, &query))
            .await?
            .ok_or_else(|| not_found(account_id, &for_error))
    }

    /// One account's saved searches, alphabetical by name.
    ///
    /// # Errors
    /// A mapped storage error.
    #[tracing::instrument(skip(self), fields(account_id = account_id), err)]
    pub async fn list(&self, account_id: i64) -> Result<Vec<SavedSearch>, Error> {
        Ok(self
            .db
            .read(move |conn| repo::list(conn, account_id))
            .await?)
    }

    /// Look one up by name.
    ///
    /// # Errors
    /// [`Error::NotFound`] if the account has no search by that name;
    /// otherwise a mapped storage error.
    #[tracing::instrument(skip(self), fields(account_id = account_id, name = name), err)]
    pub async fn get(&self, account_id: i64, name: &str) -> Result<SavedSearch, Error> {
        let name = name.trim().to_owned();
        let for_error = name.clone();
        self.db
            .read(move |conn| repo::get_by_name(conn, account_id, &name))
            .await?
            .ok_or_else(|| not_found(account_id, &for_error))
    }

    /// Delete by name.
    ///
    /// # Errors
    /// [`Error::NotFound`] if the account has no search by that name;
    /// otherwise a mapped storage error.
    #[tracing::instrument(skip(self), fields(account_id = account_id, name = name), err)]
    pub async fn delete(&self, account_id: i64, name: &str) -> Result<(), Error> {
        let name = name.trim().to_owned();
        let for_error = name.clone();
        let removed = self
            .db
            .write(move |conn| repo::delete(conn, account_id, &name))
            .await?;
        if removed {
            Ok(())
        } else {
            Err(not_found(account_id, &for_error))
        }
    }

    /// Resolve `name` to the query string to re-run, stamping `last_run_at`
    /// in the same statement.
    ///
    /// This is the entire "run a saved search" surface this module offers —
    /// see the module docs for why the pipeline call itself lives at the
    /// gRPC boundary instead of here.
    ///
    /// # Errors
    /// [`Error::NotFound`] if the account has no search by that name;
    /// otherwise a mapped storage error.
    #[tracing::instrument(skip(self), fields(account_id = account_id, name = name), err)]
    pub async fn resolve_for_run(&self, account_id: i64, name: &str) -> Result<SavedSearch, Error> {
        let name = name.trim().to_owned();
        let for_error = name.clone();
        self.db
            .write(move |conn| repo::touch_run(conn, account_id, &name))
            .await?
            .ok_or_else(|| not_found(account_id, &for_error))
    }
}

/// The one `NOT_FOUND` message shape this module produces, so a client sees
/// the same text from `get`, `delete`, and `resolve_for_run`.
fn not_found(account_id: i64, name: &str) -> Error {
    Error::not_found(format!(
        "no saved search named {name:?} in account {account_id}"
    ))
}

/// Trim and bound a name.
///
/// # Errors
/// [`Error::InvalidArgument`] if empty after trimming or longer than
/// [`MAX_NAME_LEN`].
pub fn validate_name(name: &str) -> Result<String, Error> {
    let name = name.trim();
    if name.is_empty() {
        return Err(Error::invalid_argument("name must not be empty"));
    }
    if name.chars().count() > MAX_NAME_LEN {
        return Err(Error::invalid_argument(format!(
            "name must be at most {MAX_NAME_LEN} characters"
        )));
    }
    Ok(name.to_owned())
}

/// Check that `query` is something the pipeline can actually run, returning
/// it trimmed.
///
/// See the module docs' "Validation" section: [`crate::query::parse`] cannot
/// fail, so the only rejectable query is one that parses to *nothing* — no
/// operator, no term, no phrase. The whitespace-only case and the
/// empty-quoted-phrase case (`""`, which the parser drops) both land here.
///
/// # Errors
/// [`Error::InvalidArgument`] if the query is over [`MAX_QUERY_LEN`] or
/// parses to nothing searchable.
pub fn validate_query(query: &str) -> Result<String, Error> {
    let query = query.trim();
    if query.len() > MAX_QUERY_LEN {
        return Err(Error::invalid_argument(format!(
            "query must be at most {MAX_QUERY_LEN} bytes"
        )));
    }
    let parsed = parse::parse(query);
    if parsed.filters.is_empty() && parsed.terms.is_empty() && parsed.phrases.is_empty() {
        return Err(Error::invalid_argument(
            "query has nothing to search for: no operator, term, or phrase",
        ));
    }
    Ok(query.to_owned())
}

#[cfg(test)]
mod tests;
