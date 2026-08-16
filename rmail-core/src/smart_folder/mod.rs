//! Deterministic smart folders: virtual mailboxes whose membership is a
//! *predicate*, re-evaluated on every sync (prd.md, "Saved Searches & Smart
//! Folders"; task 35).
//!
//! # Membership is a view, not a copy — and never a server-side move
//!
//! prd.md is explicit: a smart folder keeps membership live "without moving
//! mail on the server." Nothing in this module writes a message into a
//! folder, and nothing in it calls [`crate::imap::mutate::ImapMutator`] at
//! all. [`SmartFolderStore::members`] answers by running
//! [`SmartFolder::predicate`] against the local database *right now*, so a
//! message that synced one millisecond ago is a member one millisecond ago,
//! with no evaluation, no reindex, and no IMAP round trip in between.
//! `smart_folder::tests::evaluating_a_smart_folder_issues_no_imap_mutation`
//! is the regression proof: an [`crate::imap::mutate::ImapMutator`] that
//! errors on every method survives a full evaluation untouched.
//!
//! The one IMAP call a smart folder can *indirectly* cause is the keyword
//! `STORE` behind an `auto_tag` whose tag is itself configured to sync (see
//! [`crate::tags::sync`]) — the tag round-trip the user asked for by
//! configuring that tag, applied to messages the folder matched. It is still
//! never a move, copy, delete, or flag replace;
//! `a_smart_folder_never_moves_or_copies_mail_on_the_server` pins that
//! distinction.
//!
//! # Why free text is rejected rather than ignored
//!
//! A deterministic predicate is compiled by [`crate::tags::query`], the same
//! hard-filter-only compiler `BulkTag`'s `query` selector uses. That
//! compiler's documented contract is to *degrade*: an operator it does not
//! back, and every free-text term, is silently dropped. For a one-shot bulk
//! tag that is fine — the caller sees the affected count before anything is
//! committed. For a smart folder it is not: the predicate is persistent and
//! unattended, and dropping half of `from:stripe invoice` leaves a folder
//! that silently contains *every* message from Stripe, re-confirmed as
//! correct on every sync, with nobody watching. So
//! [`validate_predicate`] refuses anything the compiler cannot express in
//! full. Ranked, natural-language predicates are not lost — they are task
//! 58's NL-compiled hybrid plans, and a free-text query that should stay
//! ranked belongs in a [`crate::saved_search::SavedSearch`] instead. The
//! error message says exactly that.
//!
//! # Only genuinely new matches fire actions
//!
//! prd.md: smart folders "can trigger actions (auto-tag/notify) on new
//! matches." The word doing the work is *new*. `smart_folder_matched`
//! (migration V26) is the ledger that makes it meaningful: it records which
//! members this folder's actions have already fired for, so re-evaluating an
//! unchanged folder fires nothing at all, and re-evaluating after one message
//! arrives fires exactly once, for that one message. It is **not** the
//! membership — see V26's own comment, and
//! `members_are_recomputed_not_read_from_the_ledger`.
//!
//! Three consequences worth stating outright:
//!
//! - **Creation records a baseline.** [`SmartFolderStore::create`] runs the
//!   predicate once and records every current match as already-fired, so
//!   defining a folder over an existing mailbox does not notify for the
//!   backlog. This is [`crate::hooks::HookDispatcher`]'s "the cursor starts
//!   at now, never at the beginning of retention" rule applied to a
//!   different ledger, for the identical reason: a first run that fires for
//!   all of history is thousands of duplicated side effects, not a no-op.
//!   Applying a tag to the backlog is what `mail tag-bulk --query` is for.
//! - **Actions fire before they are stamped.** A crash in between re-fires
//!   on the next evaluation rather than silently swallowing the
//!   notification. Auto-tag is idempotent by construction (the
//!   `message_tags` partial unique index), and a duplicate notification is
//!   strictly better than mail the user was never told about.
//! - **A member that leaves and returns is new again.** The ledger tracks
//!   current membership, not "everything that ever matched," so it stays
//!   bounded by the folder's size. A message that stops satisfying
//!   `is:unread` and later satisfies it again therefore fires again — which
//!   is the honest reading of "new match", and the alternative (an
//!   ever-growing has-ever-matched set) trades a real memory bound for a
//!   debatable one. The same rule applies to a member whose action *failed*
//!   and then departed before the retry: its unstamped row is deleted with
//!   every other departure, so the owed action is dropped rather than fired
//!   for a message that is no longer in the folder. Firing it anyway would
//!   auto-tag or announce a message the predicate no longer selects, which
//!   is the worse of the two — the folder is a live view, and an action is
//!   only meaningful while its subject is in it.
//!
//! # Re-evaluated on each sync
//!
//! [`SmartFolderEvaluator`] is a *consumer* of the durable event log
//! ([`crate::events`]), shaped exactly like [`crate::hooks::HookDispatcher`]
//! and `ai::dispatch::AiDispatchLoop`: it re-reads
//! [`crate::events::EventLog::since`] from its own cursor on a tick and
//! re-evaluates the smart folders of every account that saw an event. It
//! does not care *which* event — an evaluation is a fresh read of current
//! state, so an event is only ever a "something in this account changed"
//! trigger, never data. That is also why its cursor can be seeded at the
//! log's head on startup and why a retention gap is recovered by jumping to
//! the head rather than replaying: there is no history here worth replaying,
//! only current state worth re-reading, and [`SmartFolderEvaluator::spawn`]
//! does one full pass at boot to establish it.
//!
//! One consequence is worth naming, because it looks like a loop and is not:
//! a `notify` action appends an account-scoped event, which the *next* tick
//! sees and answers with one more evaluation of that account. That second
//! pass finds no new members (the ledger was stamped before the events were
//! visible to it), so it fires nothing and appends nothing, and the sequence
//! terminates after exactly one extra no-op pass. Filtering
//! [`EventKind::RuleFired`] out of the trigger set to avoid it would be
//! wrong, not merely unnecessary: another rule engine's `RULE_FIRED` can
//! record a tag application, and a `tag:` predicate's membership genuinely
//! does change when that happens.

pub(crate) mod membership;
pub mod nl;
pub(crate) mod repo;

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, PoisonError};
use std::time::Duration;

use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

use crate::error::{Error, ErrorReason};
use crate::events::{EventKind, EventLog, NewEvent};
use crate::query::parse::Operator;
use crate::retrieve::cancel::interruptible_read;
use crate::saved_search::{validate_name, MAX_QUERY_LEN};
use crate::smart_folder::membership::DEFAULT_MIN_SIMILARITY;
use crate::storage::Database;
use crate::tags::query as filter_query;
use crate::tags::{BulkOutcome, BulkSelector, TagSource, TagStore};

/// How often [`SmartFolderEvaluator`] checks the event log for accounts that
/// need re-evaluating.
///
/// The same five seconds [`crate::hooks::DEFAULT_TICK_INTERVAL`] uses, and
/// for the same reason: nobody is waiting synchronously on a smart folder's
/// membership being *re-derived* (a read of it is always live — see the
/// module docs), so this bounds only how quickly an auto-tag/notify action
/// follows the sync that triggered it.
pub const DEFAULT_TICK_INTERVAL: Duration = Duration::from_secs(5);

/// How many events one [`SmartFolderEvaluator::tick`] reads per page.
const DRAIN_PAGE: i64 = 500;

/// A smart folder's persisted definition.
#[derive(Debug, Clone, PartialEq)]
pub struct SmartFolder {
    /// Row id.
    pub id: i64,
    /// Owning account.
    pub account_id: i64,
    /// Display name, unique per account and matched case-insensitively.
    pub name: String,
    /// The query that defines membership.
    ///
    /// Operators only for a deterministic folder ([`validate_predicate`]);
    /// operators plus free text for one Claude compiled from English
    /// ([`validate_hybrid_predicate`]), where the free text drives the lexical
    /// and dense arms [`membership`] documents.
    pub predicate: String,
    /// A tag applied to genuinely new members, if configured.
    pub auto_tag: Option<String>,
    /// Whether a new member publishes an [`EventKind::RuleFired`] event.
    pub notify: bool,
    /// Creation time (unix seconds).
    pub created_at: i64,
    /// Last definition change (unix seconds).
    pub updated_at: i64,
    /// Last evaluation (unix seconds), if any.
    pub last_evaluated_at: Option<i64>,
    /// The plain-English predicate this folder was compiled from, when it was
    /// — `None` for a hand-written one. Kept because it, not the compiled
    /// query, is what the user actually said.
    pub nl_source: Option<String>,
    /// Which embedding model produced the frozen query vector, when there is
    /// one. `None` means the folder has no dense arm.
    pub vector_model: Option<String>,
    /// The cosine floor the dense arm admits a message at.
    pub min_similarity: Option<f64>,
    /// Which Claude model compiled [`nl_source`](Self::nl_source).
    pub compiled_model: Option<String>,
    /// When it was compiled (unix seconds).
    pub compiled_at: Option<i64>,
}

/// What [`SmartFolderStore::create`] needs.
///
/// The five plan fields are all `None` for a deterministic folder, which is
/// what [`Default`] gives — task 35's call sites are unchanged by task 58.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NewSmartFolder {
    /// Owning account.
    pub account_id: i64,
    /// Display name, unique per account.
    pub name: String,
    /// The predicate. Operators only unless [`nl_source`](Self::nl_source) is
    /// set — see [`SmartFolder::predicate`].
    pub predicate: String,
    /// A tag to apply to new members, if any.
    pub auto_tag: Option<String>,
    /// Whether a new member publishes an event.
    pub notify: bool,
    /// The plain-English predicate Claude compiled, when it did. Setting this
    /// is what selects the hybrid validation path.
    pub nl_source: Option<String>,
    /// The embedded free text, frozen at compile time, with the model that
    /// produced it. Both or neither — a vector with no model cannot be joined
    /// against the index, so the store refuses the pair half-set.
    pub query_vector: Option<crate::embed::Embedding>,
    /// The embedding model behind [`query_vector`](Self::query_vector).
    pub vector_model: Option<String>,
    /// The cosine floor for the dense arm. Defaulted by the store when a
    /// vector is present and this is not.
    pub min_similarity: Option<f64>,
    /// Which Claude model compiled the predicate.
    pub compiled_model: Option<String>,
}

/// What one [`SmartFolderStore::evaluate`] found and did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Evaluation {
    /// The folder evaluated.
    pub smart_folder_id: i64,
    /// How many messages currently satisfy the predicate.
    pub members: usize,
    /// Messages that became members since the previous evaluation,
    /// ascending.
    pub entered: Vec<i64>,
    /// Messages that stopped being members, ascending.
    pub departed: Vec<i64>,
    /// New `(message, tag)` applications the `auto_tag` action created —
    /// excludes messages that already carried the tag.
    pub tagged: usize,
    /// Events published by the `notify` action.
    pub notified: usize,
}

/// A summary of one [`SmartFolderEvaluator::tick`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EvaluatorReport {
    /// How many folders were re-evaluated.
    pub folders: usize,
    /// Total new members observed across them.
    pub entered: usize,
    /// Total tag applications created.
    pub tagged: usize,
    /// Total notifications published.
    pub notified: usize,
}

/// CRUD over smart folders, plus the evaluation that keeps their actions
/// honest.
///
/// Cheap to clone: every clone shares the database, the tag store, and the
/// event log.
#[derive(Debug, Clone)]
pub struct SmartFolderStore {
    db: Database,
    tags: TagStore,
    events: EventLog,
    /// One async lock per folder, held across a whole evaluation.
    ///
    /// Shared by every clone (that is the point) — see
    /// [`SmartFolderStore::evaluate`] for what breaks without it.
    evaluations: Arc<StdMutex<HashMap<i64, Arc<AsyncMutex<()>>>>>,
}

impl SmartFolderStore {
    /// Build a store over `db`, applying `auto_tag` actions through `tags`
    /// and publishing `notify` actions to `events`.
    ///
    /// The tag store is a hard dependency rather than an `Option` on
    /// purpose: an `auto_tag` that silently did nothing because the process
    /// was wired without one is the failure mode this type most needs to be
    /// unable to have.
    #[must_use]
    pub fn new(db: Database, tags: TagStore, events: EventLog) -> Self {
        Self {
            db,
            tags,
            events,
            evaluations: Arc::new(StdMutex::new(HashMap::new())),
        }
    }

    /// The lock serializing evaluations of one folder — see
    /// [`evaluate`](Self::evaluate).
    ///
    /// Entries nothing is currently holding are swept on every call
    /// (`strong_count == 1` means this map is the only owner), so a
    /// long-running daemon that has created and deleted many folders does not
    /// accumulate one mutex per id that ever existed.
    fn evaluation_lock(&self, id: i64) -> Arc<AsyncMutex<()>> {
        let mut guard = self
            .evaluations
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        guard.retain(|_, lock| Arc::strong_count(lock) > 1);
        Arc::clone(guard.entry(id).or_default())
    }

    /// Define a smart folder and record its current membership as the
    /// action baseline (see the module docs — creating a folder never fires
    /// for the backlog).
    ///
    /// # Errors
    /// [`Error::InvalidArgument`] for an empty/over-long name, an empty
    /// `auto_tag`, a half-set vector/model pair, or a predicate
    /// [`validate_predicate`]/[`validate_hybrid_predicate`] rejects;
    /// [`Error::AlreadyExists`] if the account already has a folder by that
    /// name; [`Error::NotFound`] if `account_id` names no account.
    /// Otherwise a mapped storage error.
    #[tracing::instrument(
        skip(self, spec),
        fields(
            account_id = spec.account_id,
            name = spec.name,
            notify = spec.notify,
            hybrid = spec.nl_source.is_some(),
        ),
        err
    )]
    pub async fn create(&self, spec: &NewSmartFolder) -> Result<SmartFolder, Error> {
        let name = validate_name(&spec.name)?;
        let predicate = match &spec.nl_source {
            None => validate_predicate(&spec.predicate)?,
            Some(_) => validate_hybrid_predicate(&spec.predicate)?,
        };
        let auto_tag = match spec.auto_tag.as_deref().map(str::trim) {
            None => None,
            Some("") => {
                return Err(Error::invalid_argument(
                    "auto_tag must be a tag name, or absent entirely",
                ))
            }
            Some(tag) => Some(tag.to_owned()),
        };
        // A vector with no model cannot be joined against the semantic index
        // and a model with no vector has nothing to search with; either half
        // alone is a caller bug that would present as a folder whose dense arm
        // silently never fires.
        if spec.query_vector.is_some() != spec.vector_model.is_some() {
            return Err(Error::invalid_argument(
                "a smart folder's query vector and its embedding model must be set together",
            ));
        }
        // And it must be a vector this index can actually search. A daemon
        // configured with a wider embedder (`index.semantic.voyage.dim` is
        // 1024 by default) produces one that `vec_chunks` cannot hold, so the
        // dense arm would never fire — and if it was the plan's only
        // enforceable arm, the folder would be stored on the strength of an
        // arm that cannot exist. Refusing here is what lets `constrains`
        // below give the honest answer.
        if let Some(vector) = &spec.query_vector {
            if vector.dim() != crate::index::semantic::VECTOR_DIM {
                return Err(Error::invalid_argument(format!(
                    "a smart folder's query vector is {}-dimensional; this index holds {}. \
                     The configured embedder cannot search it, so the semantic arm could \
                     never fire",
                    vector.dim(),
                    crate::index::semantic::VECTOR_DIM
                )));
            }
        }
        // The last line of defence for the invariant this whole module is
        // built around: whatever a compiler proposed, a *stored* folder must
        // impose at least one enforceable constraint. `validate_hybrid_predicate`
        // accepts free text whose only enforceable arm is the dense one (that
        // is the point of the feature), so a plan whose embedding could not be
        // produced — a failed embedder, a hosted backend that was down — must
        // be refused here rather than stored as a folder holding the account.
        let plan = membership::Membership::compile(spec.account_id, &predicate, None, false);
        if !plan.constrains(spec.query_vector.is_some()) {
            return Err(Error::invalid_argument(
                "this predicate has no enforceable constraint: its operators compile to \
                 nothing, its free text yields no lexical match, and no query embedding \
                 was available for the semantic arm — storing it would define a folder \
                 holding every message in the account",
            ));
        }

        let account_id = spec.account_id;
        let stored = NewSmartFolder {
            account_id,
            name: name.clone(),
            predicate: predicate.clone(),
            auto_tag,
            notify: spec.notify,
            nl_source: spec.nl_source.as_ref().map(|s| s.trim().to_owned()),
            query_vector: spec.query_vector.clone(),
            vector_model: spec.vector_model.clone(),
            min_similarity: spec
                .query_vector
                .is_some()
                .then(|| spec.min_similarity.unwrap_or(DEFAULT_MIN_SIMILARITY)),
            compiled_model: spec.compiled_model.clone(),
        };
        let baseline_dense = dense_arm(
            &stored.query_vector,
            &stored.vector_model,
            stored.min_similarity,
        );

        let outcome = self
            .db
            .write(move |conn| {
                let tx = conn.transaction()?;
                let folder = match repo::insert(&tx, &stored) {
                    Ok(folder) => folder,
                    Err(err) if crate::saved_search::repo::is_unique_violation(&err) => {
                        return Ok(CreateOutcome::Duplicate)
                    }
                    Err(err) if crate::saved_search::repo::is_missing_reference(&err) => {
                        return Ok(CreateOutcome::NoAccount)
                    }
                    Err(err) => return Err(err),
                };
                // The baseline runs on the writer connection, inside the same
                // transaction that created the row: a message arriving
                // between the insert and a separate baseline pass would
                // otherwise be recorded as a "new match" and notify for mail
                // that predates nothing.
                let declared = baseline_dense.is_some();
                let plan = membership::Membership::compile(
                    account_id,
                    &predicate,
                    baseline_dense,
                    declared,
                );
                let dense_ids = match plan.dense_arm() {
                    Some(arm) => membership::resolve_dense(&tx, arm)?,
                    None => Vec::new(),
                };
                let current = plan.select(&tx, &dense_ids, None)?;
                repo::reconcile(&tx, folder.id, &current, true)?;
                // Re-read, because `reconcile` stamps `last_evaluated_at`:
                // the row `insert` returned predates that update, and handing
                // it back would report "never evaluated" for a folder a
                // `ListSmartFolders` a millisecond later shows a timestamp
                // for.
                let folder = repo::get(&tx, folder.id)?;
                tx.commit()?;
                Ok(folder.map_or(CreateOutcome::Vanished, |folder| {
                    CreateOutcome::Created(Box::new(folder))
                }))
            })
            .await?;

        match outcome {
            CreateOutcome::Created(folder) => Ok(*folder),
            CreateOutcome::Vanished => Err(Error::internal(
                "smart folder vanished between insert and read-back within its own transaction",
            )),
            CreateOutcome::Duplicate => Err(Error::already_exists(format!(
                "a smart folder named {name:?} already exists in this account"
            ))),
            CreateOutcome::NoAccount => Err(Error::not_found(format!("account {account_id}"))),
        }
    }

    /// One account's smart folders, alphabetical by name.
    ///
    /// # Errors
    /// A mapped storage error.
    #[tracing::instrument(skip(self), fields(account_id = account_id), err)]
    pub async fn list(&self, account_id: i64) -> Result<Vec<SmartFolder>, Error> {
        Ok(self
            .db
            .read(move |conn| repo::list(conn, account_id))
            .await?)
    }

    /// Look one up by name.
    ///
    /// # Errors
    /// [`Error::NotFound`] if the account has no folder by that name;
    /// otherwise a mapped storage error.
    #[tracing::instrument(skip(self), fields(account_id = account_id, name = name), err)]
    pub async fn get(&self, account_id: i64, name: &str) -> Result<SmartFolder, Error> {
        let name = name.trim().to_owned();
        let for_error = name.clone();
        self.db
            .read(move |conn| repo::get_by_name(conn, account_id, &name))
            .await?
            .ok_or_else(|| {
                Error::not_found(format!(
                    "no smart folder named {for_error:?} in account {account_id}"
                ))
            })
    }

    /// Look one up by row id.
    ///
    /// # Errors
    /// [`Error::NotFound`] if no such folder exists — which is also what a
    /// folder whose account was deleted returns, since `ON DELETE CASCADE`
    /// removes it with the account.
    #[tracing::instrument(skip(self), fields(smart_folder_id = id), err)]
    pub async fn get_by_id(&self, id: i64) -> Result<SmartFolder, Error> {
        self.db
            .read(move |conn| repo::get(conn, id))
            .await?
            .ok_or_else(|| Error::not_found(format!("smart folder {id}")))
    }

    /// Delete by name.
    ///
    /// # Errors
    /// [`Error::NotFound`] if the account has no folder by that name;
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
            Err(Error::not_found(format!(
                "no smart folder named {for_error:?} in account {account_id}"
            )))
        }
    }

    /// The folder's members *right now*, ascending by message id, at most
    /// `limit` of them.
    ///
    /// This is a fresh evaluation of the predicate, not a read of any stored
    /// membership — see the module docs. It writes nothing, fires nothing,
    /// and is safe to call on every render.
    ///
    /// `limit` is pushed into the SQL, not applied to the result: a folder
    /// whose predicate admits the whole account must not materialize it to
    /// answer a request for the first twenty.
    ///
    /// # Errors
    /// [`Error::NotFound`] if `id` names no folder; [`Error::Unavailable`]
    /// if `cancel` fired before the scan finished; otherwise a mapped
    /// storage error.
    #[tracing::instrument(
        skip(self, cancel),
        fields(smart_folder_id = id, limit = ?limit, members),
        err
    )]
    pub async fn members(
        &self,
        id: i64,
        limit: Option<usize>,
        cancel: &CancellationToken,
    ) -> Result<Vec<i64>, Error> {
        let folder = self.get_by_id(id).await?;
        let members = self.resolve_members(&folder, limit, cancel).await?;
        tracing::Span::current().record("members", members.len());
        Ok(members)
    }

    /// Re-evaluate one folder: recompute membership, reconcile the action
    /// ledger, and fire `auto_tag`/`notify` for genuinely new members only.
    ///
    /// # Concurrency: one evaluation per folder at a time
    ///
    /// Two evaluations of the same folder are serialized by a per-folder
    /// async lock held for the whole reconcile → fire → stamp sequence, and
    /// that lock is load-bearing rather than an optimization. `reconcile`'s
    /// transaction makes *entering* the ledger atomic, but the set that
    /// actually drives firing is the rows still carrying `fired_at IS NULL`,
    /// and those are only stamped in a *later* transaction — after an
    /// `auto_tag`'s IMAP round trip and the notification append. Without the
    /// lock, a second evaluation starting inside that window sees the same
    /// unstamped rows and fires for them a second time, breaking the
    /// exactly-once contract this whole design exists for. It is reachable:
    /// `rmaild` hands one store to both `SavedSearchService.EvaluateSmartFolder`
    /// (any client, any time) and the background [`SmartFolderEvaluator`].
    ///
    /// One process owns the SQLite file, so an in-process lock is sufficient;
    /// `smart_folder::tests::two_concurrent_evaluations_fire_exactly_once` is
    /// the regression proof.
    ///
    /// # Errors
    /// [`Error::NotFound`] if `id` names no folder (including one whose
    /// account was deleted out from under it); [`Error::Unavailable`] if
    /// `cancel` fired mid-scan; the tag store's error if an `auto_tag`
    /// round-trip is refused by IMAP; otherwise a mapped storage error. In
    /// every failure case the ledger is left unstamped, so the next
    /// evaluation retries the action rather than dropping it.
    #[tracing::instrument(
        skip(self, cancel),
        fields(smart_folder_id = id, members, entered, tagged, notified),
        err
    )]
    pub async fn evaluate(&self, id: i64, cancel: &CancellationToken) -> Result<Evaluation, Error> {
        let lock = self.evaluation_lock(id);
        let _serialized = lock.lock().await;

        let folder = self.get_by_id(id).await?;
        let current = self.resolve_members(&folder, None, cancel).await?;
        let members = current.len();

        let reconciled = self
            .db
            .write(move |conn| {
                let tx = conn.transaction()?;
                // Re-read inside the transaction: a folder deleted between
                // the read above and this write would otherwise have its
                // ledger rows rejected by the foreign key and surface as a
                // raw storage fault instead of the `NOT_FOUND` a deleted
                // folder deserves.
                if repo::get(&tx, id)?.is_none() {
                    return Ok(None);
                }
                let reconciled = repo::reconcile(&tx, id, &current, false)?;
                tx.commit()?;
                Ok(Some(reconciled))
            })
            .await?
            .ok_or_else(|| Error::not_found(format!("smart folder {id}")))?;

        let mut evaluation = Evaluation {
            smart_folder_id: id,
            members,
            entered: reconciled.entered,
            departed: reconciled.departed,
            tagged: 0,
            notified: 0,
        };

        if !reconciled.pending.is_empty() {
            // Actions first, stamp second — see the module docs on why the
            // crash window favours a duplicate over a lost notification.
            if let Some(tag) = folder.auto_tag.as_deref() {
                let names = [tag.to_owned()];
                let BulkOutcome { applied, .. } = self
                    .tags
                    .bulk_tag_with_source(
                        folder.account_id,
                        BulkSelector::MessageIds(reconciled.pending.clone()),
                        &names,
                        TagSource::Rule,
                    )
                    .await?;
                evaluation.tagged = applied;
            }
            if folder.notify {
                evaluation.notified = self.publish_matches(&folder, &reconciled.pending).await?;
            }
            let pending = reconciled.pending;
            self.db
                .write(move |conn| repo::mark_fired(conn, id, &pending))
                .await?;
        }

        let span = tracing::Span::current();
        span.record("members", evaluation.members);
        span.record("entered", evaluation.entered.len());
        span.record("tagged", evaluation.tagged);
        span.record("notified", evaluation.notified);
        Ok(evaluation)
    }

    /// Re-evaluate every smart folder in one account.
    ///
    /// A folder whose own evaluation fails is logged and skipped rather than
    /// failing the rest: one bad predicate must not stop an unrelated
    /// folder's notifications, the same "a failing hook does not stall the
    /// consumer" rule [`crate::hooks::HookDispatcher`] follows.
    ///
    /// # Errors
    /// A mapped storage error from listing the account's folders. Individual
    /// evaluation failures are logged, not returned.
    #[tracing::instrument(skip(self, cancel), fields(account_id = account_id), err)]
    pub async fn evaluate_account(
        &self,
        account_id: i64,
        cancel: &CancellationToken,
    ) -> Result<Vec<Evaluation>, Error> {
        let ids = self
            .db
            .read(move |conn| repo::ids_for_account(conn, account_id))
            .await?;
        Ok(self.evaluate_each(&ids, cancel).await)
    }

    /// Re-evaluate every smart folder in the database — the boot-time pass.
    ///
    /// # Errors
    /// A mapped storage error from listing the folders.
    #[tracing::instrument(skip(self, cancel), err)]
    pub async fn evaluate_all(&self, cancel: &CancellationToken) -> Result<Vec<Evaluation>, Error> {
        let ids = self.db.read(repo::all_ids).await?;
        Ok(self.evaluate_each(&ids, cancel).await)
    }

    async fn evaluate_each(&self, ids: &[i64], cancel: &CancellationToken) -> Vec<Evaluation> {
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if cancel.is_cancelled() {
                break;
            }
            match self.evaluate(*id, cancel).await {
                Ok(evaluation) => out.push(evaluation),
                Err(error) => tracing::warn!(
                    smart_folder_id = id,
                    %error,
                    "smart folder evaluation failed; leaving its ledger unstamped so the \
                     next pass retries"
                ),
            }
        }
        out
    }

    /// Run `folder`'s predicate against current state.
    ///
    /// `limit` is only ever set by a paged read; an *evaluation* must never
    /// pass one, or the ids beyond the bound would read as departures and
    /// their ledger rows would be deleted.
    async fn resolve_members(
        &self,
        folder: &SmartFolder,
        limit: Option<usize>,
        cancel: &CancellationToken,
    ) -> Result<Vec<i64>, Error> {
        let id = folder.id;
        let account_id = folder.account_id;
        let predicate = folder.predicate.clone();
        let arm_shape = folder.vector_model.clone().map(|model| {
            (
                model,
                folder.min_similarity.unwrap_or(DEFAULT_MIN_SIMILARITY),
            )
        });
        // What the *row* claims, decided before any read: a vector that fails
        // to load must still leave its clause in the query. See
        // `membership::Membership::dense_declared`.
        let declared = arm_shape.is_some();
        // `interruptible_read`, not a plain read: a superseded or
        // shutting-down evaluation must actually stop the scan, and — far
        // more importantly here — a cancelled scan must never be mistaken
        // for "this folder is now empty", which would delete the whole
        // ledger and re-fire every member on the next pass.
        //
        // The dense arm's kNN and the membership select run on the *same*
        // connection inside one closure. Two separate reads would let the
        // index move between them, which for a folder whose only enforceable
        // arm is the dense one is the difference between a member and a
        // departure.
        interruptible_read(&self.db, cancel, move |conn| {
            let dense = match arm_shape {
                // The vector is read here rather than carried on `SmartFolder`
                // on purpose: it is a few kilobytes per folder and every
                // `ListSmartFolders` would otherwise pay for it to render a
                // name.
                Some((model, min_similarity)) => {
                    repo::query_vector(conn, id)?.map(|vector| membership::DenseArm {
                        vector,
                        model,
                        min_similarity,
                    })
                }
                None => None,
            };
            let plan = membership::Membership::compile(account_id, &predicate, dense, declared);
            let dense_ids = match plan.dense_arm() {
                Some(arm) => membership::resolve_dense(conn, arm)?,
                None => Vec::new(),
            };
            plan.select(conn, &dense_ids, limit)
        })
        .await?
        .ok_or_else(|| {
            Error::unavailable(format!(
                "smart folder {} evaluation was cancelled before it completed",
                folder.id
            ))
        })
    }

    /// Publish one [`EventKind::RuleFired`] per new member.
    ///
    /// One event per message rather than one per evaluation: a consumer
    /// (`WatchEvents`, a shell hook, a future notifier) branches on
    /// `message_id`, and an event carrying a list would force every one of
    /// them to re-derive the per-message fan-out this already knows.
    ///
    /// [`EventKind::RuleFired`] rather than a new kind, because that is
    /// exactly what this is — "a rule matched and acted" — and the payload's
    /// `rule` discriminator is what lets a consumer tell a smart folder's
    /// firing from any other rule engine's without the log's kind vocabulary
    /// growing a variant per producer.
    async fn publish_matches(&self, folder: &SmartFolder, ids: &[i64]) -> Result<usize, Error> {
        let events: Vec<NewEvent> = ids
            .iter()
            .map(|message_id| {
                NewEvent::new(EventKind::RuleFired)
                    .account(folder.account_id)
                    .message(*message_id)
                    .payload(serde_json::json!({
                        "rule": "smart_folder",
                        "smart_folder_id": folder.id,
                        "smart_folder": folder.name,
                    }))
            })
            .collect();
        Ok(self.events.append_all(events).await?.len())
    }
}

/// [`SmartFolderStore::create`]'s transaction outcome — the two rejections
/// that are not storage faults need somewhere to live that
/// `rusqlite::Result` has no room for.
enum CreateOutcome {
    /// Boxed: task 58's plan columns made `SmartFolder` much the largest
    /// variant, and every rejection arm would otherwise carry its footprint.
    Created(Box<SmartFolder>),
    Duplicate,
    NoAccount,
    /// Unreachable in practice (a row this same transaction just inserted
    /// failing to read back) — a variant rather than an `.expect()` so the
    /// impossible case is still a typed error, not a panic. Same shape as
    /// `tags::Outcome::Missing`.
    Vanished,
}

/// Check a predicate is one the deterministic compiler can express in full,
/// returning it trimmed.
///
/// See the module docs' "Why free text is rejected rather than ignored".
///
/// # Errors
/// [`Error::InvalidArgument`] if the predicate is empty, longer than
/// [`crate::saved_search::MAX_QUERY_LEN`], contains free text, or names an
/// operator [`crate::tags::query`] does not compile.
pub fn validate_predicate(predicate: &str) -> Result<String, Error> {
    let predicate = predicate.trim();
    if predicate.is_empty() {
        return Err(Error::invalid_argument(
            "smart folder predicate must not be empty",
        ));
    }
    if predicate.len() > MAX_QUERY_LEN {
        return Err(Error::invalid_argument(format!(
            "smart folder predicate must be at most {MAX_QUERY_LEN} bytes"
        )));
    }
    // The account scope is irrelevant to *which* operators compile, so this
    // validates against a placeholder rather than requiring a real account
    // id to answer a question that does not depend on one.
    let compiled = filter_query::compile_detailed(0, predicate);
    if !compiled.dropped_free_text.is_empty() {
        return Err(Error::invalid_argument(format!(
            "a smart folder predicate must be operators only, but {} is free text \
             a deterministic predicate cannot gate on; it would be ignored and the \
             folder would match more than you asked for. Save it as a saved search \
             instead, or express it with an operator (subject:, body:, ...)",
            quoted_list(&compiled.dropped_free_text)
        )));
    }
    if !compiled.dropped_operators.is_empty() {
        let names: Vec<String> = compiled
            .dropped_operators
            .iter()
            .map(|op| format!("{}:", operator_key(op)))
            .collect();
        return Err(Error::invalid_argument(format!(
            "a smart folder predicate cannot use {} — that operator is not part of \
             the deterministic membership grammar, so it would be ignored and the \
             folder would match more than you asked for",
            quoted_list(&names)
        )));
    }
    if compiled.applied == 0 {
        // Reached by input that is non-empty but parses to no token at all —
        // `""` (an empty quoted phrase, which the parser drops) is the
        // canonical case. The check has to exist independently of the two
        // above because what it guards is the outcome, not the cause:
        // compiling to the bare `account_id = ?` scope means "every message
        // in this account", and no path may reach that by accident.
        return Err(Error::invalid_argument(
            "smart folder predicate resolves to no constraint at all, which would \
             match every message in the account",
        ));
    }
    Ok(predicate.to_owned())
}

/// Check a predicate compiled from natural language, returning it trimmed.
///
/// The hybrid sibling of [`validate_predicate`], and the difference is exactly
/// one rule: free text is *allowed*, because a hybrid plan has somewhere to
/// put it — the FTS and embedding arms [`membership`] documents. Everything
/// else is identical, deliberately:
///
/// - an operator the deterministic membership compiler cannot express is still
///   refused, because there is still nowhere for it to go. A folder defined as
///   `larger:10mb` would silently be "every message", on every sync, forever.
///   That is the same failure whether a human or a model wrote it, and the
///   model does not get a looser grammar than the human.
/// - a predicate that constrains nothing is still refused.
///
/// This is the checkpoint the module's contract rests on: *the model proposes,
/// it does not commit*. Nothing reaches `smart_folders.predicate` without
/// having been through the real parser and this function.
///
/// # Errors
/// [`Error::InvalidArgument`] if the predicate is empty, longer than
/// [`crate::saved_search::MAX_QUERY_LEN`], names an operator
/// [`crate::tags::query`] does not compile, or resolves to no constraint at
/// all.
pub fn validate_hybrid_predicate(predicate: &str) -> Result<String, Error> {
    let predicate = predicate.trim();
    if predicate.is_empty() {
        return Err(Error::invalid_argument(
            "smart folder predicate must not be empty",
        ));
    }
    if predicate.len() > MAX_QUERY_LEN {
        return Err(Error::invalid_argument(format!(
            "smart folder predicate must be at most {MAX_QUERY_LEN} bytes"
        )));
    }
    let compiled = filter_query::compile_detailed(0, predicate);
    if !compiled.dropped_operators.is_empty() {
        let names: Vec<String> = compiled
            .dropped_operators
            .iter()
            .map(|op| format!("{}:", operator_key(op)))
            .collect();
        return Err(Error::invalid_argument(format!(
            "a smart folder predicate cannot use {} — that operator is not part of \
             the membership grammar, so it would be ignored and the folder would \
             match more than was asked for",
            quoted_list(&names)
        )));
    }
    let parsed = crate::query::parse::parse(predicate);
    if compiled.applied == 0 && parsed.terms.is_empty() && parsed.phrases.is_empty() {
        // Non-empty input that parses to no token at all — `""`, an empty
        // quoted phrase, is the canonical case. As in [`validate_predicate`],
        // what is guarded is the outcome rather than the cause: compiling to
        // the bare `account_id = ?` scope means "every message in this
        // account", and no path may reach that by accident.
        return Err(Error::invalid_argument(
            "smart folder predicate resolves to no constraint at all, which would \
             match every message in the account",
        ));
    }
    // Free text that yields no *lexical* arm (every term `~`-forced semantic,
    // or nothing the tokenizer produces a token from) is still acceptable
    // here: the dense arm may carry it. Whether one actually exists is a fact
    // only `SmartFolderStore::create` has, and it refuses the folder if not.
    Ok(predicate.to_owned())
}

/// Assemble the dense arm from a folder's stored plan columns, when it has
/// one. `None` whenever either half is missing — see
/// [`NewSmartFolder::query_vector`].
fn dense_arm(
    vector: &Option<crate::embed::Embedding>,
    model: &Option<String>,
    min_similarity: Option<f64>,
) -> Option<membership::DenseArm> {
    match (vector, model) {
        (Some(vector), Some(model)) => Some(membership::DenseArm {
            vector: vector.clone(),
            model: model.clone(),
            min_similarity: min_similarity.unwrap_or(DEFAULT_MIN_SIMILARITY),
        }),
        _ => None,
    }
}

/// `"a", "b"` — a human-readable list for a rejection message.
fn quoted_list(items: &[String]) -> String {
    items
        .iter()
        .map(|item| format!("{item:?}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The DSL key an [`Operator`] was written as, for error messages.
///
/// Delegates to [`crate::query::parse::render_operator`], which is the one
/// place this crate spells an operator back the way a user wrote it — a second
/// copy here could disagree with the confirmable plan `mail search --nl`
/// prints, about the very operator the message is complaining about.
fn operator_key(op: &Operator) -> &'static str {
    crate::query::parse::render_operator(op).0
}

/// The background consumer that keeps smart folder actions following sync.
///
/// See the module docs' "Re-evaluated on each sync" section.
#[derive(Debug)]
pub struct SmartFolderEvaluator {
    store: SmartFolderStore,
    events: EventLog,
    cursor: AtomicI64,
    tick_interval: Duration,
}

impl SmartFolderEvaluator {
    /// Sentinel for "no cursor yet", distinct from position 0 ("everything
    /// the log has"), which is a real and different instruction.
    const UNSEEDED_CURSOR: i64 = -1;

    /// Build an evaluator over `store`, following `events`.
    #[must_use]
    pub fn new(store: SmartFolderStore, events: EventLog) -> Self {
        Self {
            store,
            events,
            cursor: AtomicI64::new(Self::UNSEEDED_CURSOR),
            tick_interval: DEFAULT_TICK_INTERVAL,
        }
    }

    /// Override the tick interval (tests, and an operator who wants tighter
    /// action latency).
    #[must_use]
    pub fn with_tick_interval(mut self, interval: Duration) -> Self {
        self.tick_interval = interval;
        self
    }

    /// Read the log forward from this evaluator's cursor and re-evaluate the
    /// smart folders of every account that saw an event.
    ///
    /// # Errors
    /// A mapped storage error from reading the event log. A retention gap is
    /// recovered from by jumping to the head (see the module docs), not
    /// returned.
    #[tracing::instrument(skip(self, cancel), err)]
    pub async fn tick(&self, cancel: &CancellationToken) -> Result<EvaluatorReport, Error> {
        let mut cursor = self.cursor.load(Ordering::SeqCst);
        if cursor == Self::UNSEEDED_CURSOR {
            cursor = self.events.latest_seq().await?.unwrap_or(0);
        }

        let mut accounts: BTreeSet<i64> = BTreeSet::new();
        loop {
            let page = match self.events.since(cursor, DRAIN_PAGE).await {
                Ok(page) => page,
                Err(error) if error.reason() == ErrorReason::OutOfRange => {
                    let head = self.events.latest_seq().await?.unwrap_or(0);
                    tracing::warn!(
                        cursor,
                        head,
                        %error,
                        "smart folder evaluation cursor fell behind the event log's \
                         retention window; jumping to the head and re-evaluating every \
                         folder rather than replaying history"
                    );
                    self.cursor.store(head, Ordering::SeqCst);
                    // Current state is the only thing an evaluation reads, so
                    // the gap is closed by re-reading it once, not by
                    // replaying the events that were pruned.
                    return Ok(summarize(self.store.evaluate_all(cancel).await?));
                }
                Err(error) => return Err(error),
            };
            let got = page.events.len();
            for event in &page.events {
                if let Some(account_id) = event.account_id {
                    accounts.insert(account_id);
                }
            }
            cursor = page.next_seq;
            if i64::try_from(got).unwrap_or(i64::MAX) < DRAIN_PAGE {
                break;
            }
        }
        self.cursor.store(cursor, Ordering::SeqCst);

        let mut report = EvaluatorReport::default();
        for account_id in accounts {
            if cancel.is_cancelled() {
                break;
            }
            let evaluations = self.store.evaluate_account(account_id, cancel).await?;
            let account_report = summarize(evaluations);
            report.folders += account_report.folders;
            report.entered += account_report.entered;
            report.tagged += account_report.tagged;
            report.notified += account_report.notified;
        }
        Ok(report)
    }

    /// Seed the cursor at the log's head, run one full evaluation pass, then
    /// tick until `cancel` fires.
    ///
    /// Seeding *before* the task is spawned (rather than lazily on the first
    /// tick) is what makes "the head" mean boot, so events that land during
    /// startup are not skipped by a cursor seeded after them — the same
    /// correction [`crate::hooks::HookDispatcher::spawn`] documents.
    pub async fn spawn(self, cancel: CancellationToken) -> tokio::task::JoinHandle<()> {
        let head = match self.events.latest_seq().await {
            Ok(head) => head.unwrap_or(0),
            Err(error) => {
                tracing::warn!(
                    %error,
                    "could not read the event log head; the smart folder evaluator will \
                     seed its cursor on its first tick instead"
                );
                Self::UNSEEDED_CURSOR
            }
        };
        if head != Self::UNSEEDED_CURSOR {
            self.cursor.store(head, Ordering::SeqCst);
        }
        tokio::spawn(async move {
            // One full pass at boot: the cursor deliberately skips history,
            // so nothing else would bring a folder whose account changed
            // while the daemon was down back up to date.
            match self.store.evaluate_all(&cancel).await {
                Ok(evaluations) => {
                    tracing::debug!(folders = evaluations.len(), "smart folder boot pass");
                }
                Err(error) => tracing::warn!(%error, "smart folder boot pass failed"),
            }
            loop {
                tokio::select! {
                    () = cancel.cancelled() => return,
                    () = tokio::time::sleep(self.tick_interval) => {}
                }
                match self.tick(&cancel).await {
                    Ok(report) => tracing::debug!(?report, "smart folder evaluation tick"),
                    Err(error) => {
                        tracing::warn!(%error, "smart folder evaluation tick failed");
                    }
                }
            }
        })
    }
}

/// Fold a batch of evaluations into one report.
fn summarize(evaluations: Vec<Evaluation>) -> EvaluatorReport {
    let mut report = EvaluatorReport {
        folders: evaluations.len(),
        ..EvaluatorReport::default()
    };
    for evaluation in evaluations {
        report.entered += evaluation.entered.len();
        report.tagged += evaluation.tagged;
        report.notified += evaluation.notified;
    }
    report
}

#[cfg(test)]
mod tests;
