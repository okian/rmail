//! The tags subsystem: colored, hierarchical labels applied to messages or
//! threads, optionally round-tripped to IMAP keywords / Gmail labels
//! (prd.md III-4, task 55).
//!
//! [`TagStore`] is the domain service every RPC in `rmaild::tag_service`
//! calls through — CRUD over `tags`, apply/remove/bulk-apply over
//! `message_tags`, and the pending-suggestion accept/reject flow. It follows
//! the same "IMAP first, local mirror second" ordering
//! [`crate::mail::MailStore`] documents: every mutation that touches a
//! `sync_mode != local` tag attempts its wire round-trip *before* the local
//! write commits, so a caller never sees a tag "applied" that the server
//! then silently disagrees with — except for an `auto` tag, whose whole
//! point is to keep working locally when the server refuses it (see
//! [`sync`]).
//!
//! # Submodules
//!
//! - [`model`] — row types (`Tag`, `MessageTag`) and the small closed enums
//!   their columns are constrained to.
//! - [`repo`] — typed SQL over `tags`/`message_tags` (migration V24).
//! - [`hierarchy`] — `/`-separated name segments, ancestor auto-vivification,
//!   cycle rejection.
//! - [`sync`] — the IMAP keyword/Gmail-label round-trip, coalesced `STORE`
//!   grouping, and the `auto` downgrade.
//! - [`query`] — a small, `tag:`/`from:`/`is:`/... hard-filter-only SQL
//!   compiler for `BulkTag`'s `query` selector.
//!
//! # The pending-suggestion state model (for task 57)
//!
//! Task 57 ("AI auto-tagging + suggestions") populates suggestions; this
//! task defines the storage and the RPC surface they land in, and does not
//! itself call a model. The contract task 57's suggestion job writes
//! against:
//!
//! - [`TagStore::record_suggestion`] writes one pending row
//!   (`target = Target::Message(id)`, `source = TagSource::Ai`,
//!   `state = TagState::Pending`, a confidence in `0.0..=1.0`, and a short
//!   rationale string) — or reach into [`repo::insert_message_tag`] directly,
//!   since task 57 lives in this same crate. The `UNIQUE` partial index
//!   (`tag_id, message_id`) makes a repeat suggestion for the same
//!   `(message, tag)` pair a no-op rather than a duplicate row — safe to
//!   call idempotently from a retried job.
//! - [`TagStore::list_pending_suggestions`] is `SuggestTags`'s backing read:
//!   it streams back whatever is currently `state = Pending` for a message,
//!   regardless of who wrote it. Task 57's suggestion job is expected to
//!   write pending rows directly (or via this module) and let a client's
//!   next `SuggestTags` call — or a live `mail suggest-tags <id>` — surface
//!   them; this task's own `SuggestTags` implementation never calls a model
//!   itself (see the RPC's own doc comment in `rmaild::tag_service`).
//! - [`TagStore::resolve_suggestion`] is `ResolveSuggestion`'s backing
//!   write: `accept = true` flips the row to `TagState::Applied` (and, if
//!   the tag's `sync_mode` allows it, pushes the tag to IMAP exactly like a
//!   direct `AddTag` would — including the `auto` downgrade); `accept =
//!   false` flips it to `TagState::Rejected`. Both transitions only ever
//!   apply to a row still `Pending` — a `rusqlite` `WHERE state = 'pending'`
//!   guard makes a duplicate resolution a no-op rather than a silent
//!   overwrite, surfaced to the caller as [`Error::FailedPrecondition`].
//! - A `Rejected` row is *kept*, not deleted — prd.md's "learns from
//!   accept/reject decisions" needs the history, and task 57's rule-learning
//!   pass (`tag_rules`, its own migration) is expected to read
//!   `message_tags` filtered to `source = 'ai'` for exactly that signal.
//!
//! Nothing here ever reads `tags.ai` config or calls
//! [`crate::ai::provider::Provider`] — that wiring, the `suggest_tags`
//! background job, and `tag_rules`' auto-apply-above-threshold logic are
//! entirely task 57's to add.
//!
//! # A known gap: tags do not survive a client-initiated move today
//!
//! prd.md says a message-level tag "follows the stable `messages.id`"
//! across a move. `message_tags.message_id` (migration V24) is built to
//! honor that — `ON DELETE CASCADE` from `messages`, nothing else — but
//! [`crate::mail::MailStore::move_message`] does not actually keep
//! `messages.id` stable across a move: with no way to learn the UID a
//! server assigns a moved message, it deletes the local row and lets the
//! destination folder's next sync insert a *new* row under a new id (see
//! that module's own "Move does not guess a new UID" docs). A message-level
//! tag is therefore lost on `MailService.Move` today; a thread-level one
//! survives only if the resynced message rejoins the same thread. Closing
//! this means changing task 39's move semantics, not this module's —
//! tracked here rather than silently assumed away.

pub mod hierarchy;
pub mod model;
pub mod query;
pub mod repo;
pub mod sync;

pub use model::{
    MessageTag, NewMessageTag, PendingSuggestion, Tag, TagSource, TagState, TagWithCount, Target,
};

use std::sync::Arc;

use rusqlite::Connection;

use crate::config::{TagSyncMode, TagsConfig};
use crate::error::Error;
use crate::imap::mutate::ImapMutator;
use crate::storage::Database;

/// One (tag, target) application, as returned by [`TagStore::add_tag`].
#[derive(Debug, Clone, PartialEq)]
pub struct TagApplication {
    /// The new `message_tags` row's id.
    pub id: i64,
    /// The tag applied.
    pub tag: Tag,
    /// What it was applied to.
    pub target: Target,
    /// Who applied it.
    pub source: TagSource,
}

/// How [`TagStore::bulk_tag`] selects its message set.
#[derive(Debug, Clone)]
pub enum BulkSelector {
    /// A filter-only query string (see [`query`]).
    Query(String),
    /// An explicit id list.
    MessageIds(Vec<i64>),
}

/// The result of a [`TagStore::bulk_tag`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BulkOutcome {
    /// Messages the selector resolved to.
    pub message_count: usize,
    /// `(message, tag)` applications actually created — excludes any that
    /// were already applied (idempotent no-ops).
    pub applied: usize,
}

/// The tags domain service. See the module docs.
///
/// Cheap to clone: every clone shares the database and the IMAP mutator.
#[derive(Clone)]
pub struct TagStore {
    db: Database,
    imap: Arc<dyn ImapMutator>,
    config: TagsConfig,
}

impl std::fmt::Debug for TagStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TagStore").finish_non_exhaustive()
    }
}

impl TagStore {
    /// Build a store over `db`, mutating IMAP through `imap`, configured by
    /// `config` (`[tags]`, `.tags.imap`, see [`crate::config::TagsConfig`]).
    #[must_use]
    pub fn new(db: Database, imap: Arc<dyn ImapMutator>, config: TagsConfig) -> Self {
        Self { db, imap, config }
    }

    /// Create a tag, or update an existing one of the same `(account_id,
    /// name)` — `CreateTag`'s backing call. `color`/`sync_mode` overwrite
    /// the existing values when given (`None` leaves them unchanged);
    /// `parent_id` re-parents an *existing* tag only when it differs from
    /// the current parent, after a cycle check (see [`hierarchy::would_cycle`]).
    ///
    /// A brand-new tag's ancestor chain is auto-vivified from `name`'s
    /// `/`-segments (`tags.hierarchy_separator`) unless `parent_id` is
    /// given explicitly, in which case the explicit value wins.
    ///
    /// # Errors
    /// [`Error::InvalidArgument`] if `name` is empty, or if `parent_id`
    /// would make the tag its own ancestor (see [`hierarchy::would_cycle`]).
    /// Otherwise a mapped storage error.
    #[tracing::instrument(
        skip(self, name, color),
        fields(account_id = account_id, parent_id = ?parent_id, sync_mode = ?sync_mode),
        err
    )]
    pub async fn create_tag(
        &self,
        account_id: i64,
        name: &str,
        color: Option<String>,
        sync_mode: Option<TagSyncMode>,
        parent_id: Option<i64>,
    ) -> Result<Tag, Error> {
        let name = name.trim().to_owned();
        if name.is_empty() {
            return Err(Error::invalid_argument("tag name must not be empty"));
        }
        let separator = self.config.hierarchy_separator.clone();
        let default_sync_mode = self.config.default_sync_mode;

        let outcome = self
            .db
            .write(move |conn| {
                let tx = conn.transaction()?;
                let derived_parent = hierarchy::ensure_ancestors(
                    &tx,
                    account_id,
                    &name,
                    &separator,
                    default_sync_mode,
                )?;

                // Validated once, before either branch touches it: an
                // explicit `parent_id` naming a tag that does not exist, or
                // that exists in a *different* account, must not reach
                // `repo::insert_tag`/`set_tag_parent` at all -- left
                // unchecked, that FK violation would surface as a raw
                // `rusqlite` error mapped to `Error::Internal` by
                // `StorageError`'s `From` impl, not the
                // `INVALID_ARGUMENT` a caller-supplied bad id deserves.
                if let Some(explicit_parent) = parent_id {
                    if !parent_belongs_to_account(&tx, account_id, explicit_parent)? {
                        return Ok(Outcome::InvalidParent);
                    }
                }

                let tag = match repo::get_tag_by_name(&tx, account_id, &name)? {
                    Some(existing) => {
                        if let Some(explicit_parent) = parent_id {
                            if Some(explicit_parent) != existing.parent_id
                                && hierarchy::would_cycle(&tx, existing.id, explicit_parent)?
                            {
                                return Ok(Outcome::WouldCycle);
                            }
                            if Some(explicit_parent) != existing.parent_id {
                                repo::set_tag_parent(&tx, existing.id, Some(explicit_parent))?;
                            }
                        }
                        repo::update_tag_fields(&tx, existing.id, color.as_deref(), sync_mode)?;
                        repo::get_tag(&tx, existing.id)?
                    }
                    None => {
                        let effective_parent = parent_id.or(derived_parent);
                        let id = repo::insert_tag(
                            &tx,
                            account_id,
                            &name,
                            effective_parent,
                            color.as_deref(),
                            sync_mode.unwrap_or(default_sync_mode),
                            None,
                        )?;
                        repo::get_tag(&tx, id)?
                    }
                };
                tx.commit()?;
                Ok(tag.map_or(Outcome::Missing, Outcome::Tag))
            })
            .await?;

        match outcome {
            Outcome::Tag(tag) => Ok(tag),
            Outcome::WouldCycle => Err(Error::invalid_argument(format!(
                "setting this tag's parent to tag {} would make it its own ancestor",
                parent_id.unwrap_or_default()
            ))),
            Outcome::InvalidParent => Err(Error::invalid_argument(format!(
                "parent_id {} does not name a tag in this account",
                parent_id.unwrap_or_default()
            ))),
            Outcome::Missing => Err(Error::internal(
                "tag vanished between insert/update and read-back within the same transaction",
            )),
        }
    }

    /// Resolve a tag by name, creating it (with ancestor auto-vivification)
    /// if it does not already exist — the "type a new name, it gets
    /// created and applied" path `AddTag`/`BulkTag`/inbound-import all
    /// share, matching prd.md's tag-palette UX ("`Enter` on a new name →
    /// create-then-apply").
    ///
    /// # Errors
    /// [`Error::InvalidArgument`] if `name` is empty (after trimming).
    /// Otherwise a mapped storage error.
    #[tracing::instrument(skip(self, name), fields(account_id = account_id), err)]
    pub async fn get_or_create_tag(&self, account_id: i64, name: &str) -> Result<Tag, Error> {
        let name = name.trim().to_owned();
        if name.is_empty() {
            return Err(Error::invalid_argument("tag name must not be empty"));
        }
        let separator = self.config.hierarchy_separator.clone();
        let default_sync_mode = self.config.default_sync_mode;

        let tag = self
            .db
            .write(move |conn| {
                let tx = conn.transaction()?;
                if let Some(existing) = repo::get_tag_by_name(&tx, account_id, &name)? {
                    tx.commit()?;
                    return Ok(Some(existing));
                }
                let parent_id = hierarchy::ensure_ancestors(
                    &tx,
                    account_id,
                    &name,
                    &separator,
                    default_sync_mode,
                )?;
                let id = repo::insert_tag(
                    &tx,
                    account_id,
                    &name,
                    parent_id,
                    None,
                    default_sync_mode,
                    None,
                )?;
                let tag = repo::get_tag(&tx, id)?;
                tx.commit()?;
                Ok(tag)
            })
            .await?;
        tag.ok_or_else(|| Error::internal("tag vanished within its own creating transaction"))
    }

    /// List an account's tags with their effective message counts,
    /// alphabetical by name — `ListTags`'s backing read (`mail tags`).
    ///
    /// # Errors
    /// A mapped storage error.
    #[tracing::instrument(skip(self), fields(account_id = account_id), err)]
    pub async fn list_tags(&self, account_id: i64) -> Result<Vec<TagWithCount>, Error> {
        Ok(self
            .db
            .read(move |conn| repo::list_tags_with_counts(conn, account_id))
            .await?)
    }

    /// Apply `names` (created on demand) to `target` — `AddTag`'s backing
    /// call. IMAP round-trips first (per tag, coalesced over `target`'s
    /// resolved message set — a single message for [`Target::Message`],
    /// every current member for [`Target::Thread`]); the local write is one
    /// transaction.
    ///
    /// # Errors
    /// [`Error::InvalidArgument`] if `names` is empty; [`Error::NotFound`]
    /// if `target` does not exist; the IMAP mutator's error if a
    /// `sync_mode = imap` tag's `STORE` is refused (an `auto` tag
    /// downgrades instead — see [`sync`]).
    #[tracing::instrument(
        skip(self, names),
        fields(target = ?target, source = ?source, names = names.len()),
        err
    )]
    pub async fn add_tag(
        &self,
        target: Target,
        names: &[String],
        source: TagSource,
    ) -> Result<Vec<TagApplication>, Error> {
        if names.is_empty() {
            return Err(Error::invalid_argument("at least one tag name is required"));
        }
        let account_id = self.resolve_account_for_target(target).await?;
        let mut tags = Vec::with_capacity(names.len());
        for name in names {
            tags.push(self.get_or_create_tag(account_id, name).await?);
        }

        let message_ids = self.target_message_ids(target).await?;
        let groups = sync::group_by_mailbox(&self.db, &message_ids).await?;

        let mut downgrade_ids = Vec::new();
        for tag in &tags {
            if let sync::WireOutcome::Downgrade =
                sync::apply_wire(self.imap.as_ref(), tag, &self.config.imap, &groups, true).await?
            {
                downgrade_ids.push(tag.id);
            }
        }

        let write_tags = tags.clone();
        let rows = self
            .db
            .write(move |conn| {
                let tx = conn.transaction()?;
                for id in &downgrade_ids {
                    repo::downgrade_tag_to_local(&tx, *id)?;
                }
                let mut out = Vec::new();
                for tag in &write_tags {
                    if let Some(id) = repo::insert_message_tag(
                        &tx,
                        &NewMessageTag {
                            tag_id: tag.id,
                            target,
                            source,
                            state: TagState::Applied,
                            confidence: None,
                            rationale: None,
                        },
                    )? {
                        out.push((id, tag.clone()));
                    }
                }
                tx.commit()?;
                Ok(out)
            })
            .await?;

        Ok(rows
            .into_iter()
            .map(|(id, tag)| TagApplication {
                id,
                tag,
                target,
                source,
            })
            .collect())
    }

    /// Remove `names` from `target` — `RemoveTag`'s backing call. A name
    /// that does not resolve to an existing tag is silently a no-op (there
    /// is nothing to remove); returns how many applications were actually
    /// removed.
    ///
    /// # Errors
    /// [`Error::InvalidArgument`] if `names` is empty; [`Error::NotFound`]
    /// if `target` does not exist; the IMAP mutator's error under
    /// `sync_mode = imap` (see [`add_tag`](Self::add_tag)'s identical note).
    #[tracing::instrument(skip(self, names), fields(target = ?target, names = names.len()), err)]
    pub async fn remove_tag(&self, target: Target, names: &[String]) -> Result<usize, Error> {
        if names.is_empty() {
            return Err(Error::invalid_argument("at least one tag name is required"));
        }
        let account_id = self.resolve_account_for_target(target).await?;
        let mut tags: Vec<Tag> = Vec::with_capacity(names.len());
        for name in names {
            let name_owned = name.trim().to_owned();
            if let Some(tag) = self
                .db
                .read(move |conn| repo::get_tag_by_name(conn, account_id, &name_owned))
                .await?
            {
                tags.push(tag);
            }
        }
        if tags.is_empty() {
            return Ok(0);
        }

        let message_ids = self.target_message_ids(target).await?;
        let groups = sync::group_by_mailbox(&self.db, &message_ids).await?;

        let mut downgrade_ids = Vec::new();
        for tag in &tags {
            if let sync::WireOutcome::Downgrade =
                sync::apply_wire(self.imap.as_ref(), tag, &self.config.imap, &groups, false).await?
            {
                downgrade_ids.push(tag.id);
            }
        }

        let tag_ids: Vec<i64> = tags.iter().map(|t| t.id).collect();
        let removed = self
            .db
            .write(move |conn| {
                let tx = conn.transaction()?;
                for id in &downgrade_ids {
                    repo::downgrade_tag_to_local(&tx, *id)?;
                }
                let mut removed = 0usize;
                for id in &tag_ids {
                    if repo::delete_message_tag(&tx, *id, target)? {
                        removed += 1;
                    }
                }
                tx.commit()?;
                Ok(removed)
            })
            .await?;
        Ok(removed)
    }

    /// Apply `names` (created on demand) to every message [`BulkSelector`]
    /// resolves to — `BulkTag`'s backing call. The whole local write is one
    /// transaction, and each tag's IMAP round-trip is coalesced into one
    /// `STORE` per mailbox its selected messages span (see [`sync`]),
    /// rather than one round trip per message — prd.md: "bulk tag = single
    /// transaction + coalesced IMAP `STORE` UID sets".
    ///
    /// # Errors
    /// [`Error::InvalidArgument`] if `names` is empty. Otherwise a mapped
    /// storage error, or the IMAP mutator's error under `sync_mode = imap`.
    #[tracing::instrument(
        skip(self, selector, names),
        fields(
            account_id = account_id,
            names = names.len(),
            by_query = matches!(selector, BulkSelector::Query(_)),
        ),
        err
    )]
    pub async fn bulk_tag(
        &self,
        account_id: i64,
        selector: BulkSelector,
        names: &[String],
    ) -> Result<BulkOutcome, Error> {
        if names.is_empty() {
            return Err(Error::invalid_argument("at least one tag name is required"));
        }
        let message_ids = match selector {
            // Scoped to `account_id` even though the caller already named
            // explicit ids: an id from a different account must not create
            // a tag under this one and STORE a keyword against a server
            // this call was never authorized to touch.
            // `get_or_create_tag`/`sync::group_by_mailbox` below derive
            // *their* account scoping from `account_id`/each message's own
            // row respectively, so an unfiltered foreign id would silently
            // resolve its own (different) account's mailbox — an id that
            // does not belong to `account_id` is simply dropped, the same
            // "absent id is not an error" contract `group_by_mailbox`
            // itself already documents.
            BulkSelector::MessageIds(ids) => self.scope_ids_to_account(account_id, &ids).await?,
            BulkSelector::Query(raw) => self.resolve_query(account_id, &raw).await?,
        };
        if message_ids.is_empty() {
            return Ok(BulkOutcome::default());
        }

        let mut tags = Vec::with_capacity(names.len());
        for name in names {
            tags.push(self.get_or_create_tag(account_id, name).await?);
        }

        let groups = sync::group_by_mailbox(&self.db, &message_ids).await?;
        let mut downgrade_ids = Vec::new();
        for tag in &tags {
            if let sync::WireOutcome::Downgrade =
                sync::apply_wire(self.imap.as_ref(), tag, &self.config.imap, &groups, true).await?
            {
                downgrade_ids.push(tag.id);
            }
        }

        let write_tags = tags.clone();
        let write_ids = message_ids.clone();
        let applied = self
            .db
            .write(move |conn| {
                let tx = conn.transaction()?;
                for id in &downgrade_ids {
                    repo::downgrade_tag_to_local(&tx, *id)?;
                }
                let mut applied = 0usize;
                for tag in &write_tags {
                    for message_id in &write_ids {
                        if repo::insert_message_tag(
                            &tx,
                            &NewMessageTag {
                                tag_id: tag.id,
                                target: Target::Message(*message_id),
                                source: TagSource::User,
                                state: TagState::Applied,
                                confidence: None,
                                rationale: None,
                            },
                        )?
                        .is_some()
                        {
                            applied += 1;
                        }
                    }
                }
                tx.commit()?;
                Ok(applied)
            })
            .await?;

        Ok(BulkOutcome {
            message_count: message_ids.len(),
            applied,
        })
    }

    /// A message's pending suggestions — `SuggestTags`'s backing read. See
    /// the module docs' "pending-suggestion state model" section.
    ///
    /// # Errors
    /// A mapped storage error.
    #[tracing::instrument(skip(self), fields(message_id = message_id), err)]
    pub async fn list_pending_suggestions(
        &self,
        message_id: i64,
    ) -> Result<Vec<PendingSuggestion>, Error> {
        Ok(self
            .db
            .read(move |conn| repo::list_pending_suggestions(conn, message_id))
            .await?)
    }

    /// Write a pending AI suggestion directly (`source = TagSource::Ai`,
    /// `state = TagState::Pending`) — the primitive task 57's suggestion job
    /// writes against (see the module docs' "pending-suggestion state
    /// model" section), and the seam a test that needs one without going
    /// through this crate's `pub(crate)` `repo` module uses instead.
    ///
    /// Idempotent via the same `UNIQUE` partial index every apply path
    /// relies on: a repeat suggestion for the same `(tag, target)` pair is a
    /// no-op, returning `Ok(None)`.
    ///
    /// # Errors
    /// A mapped storage error.
    #[tracing::instrument(
        skip(self, rationale),
        fields(tag_id = tag_id, target = ?target, confidence = confidence),
        err
    )]
    pub async fn record_suggestion(
        &self,
        tag_id: i64,
        target: Target,
        confidence: f64,
        rationale: String,
    ) -> Result<Option<i64>, Error> {
        Ok(self
            .db
            .write(move |conn| {
                repo::insert_message_tag(
                    conn,
                    &NewMessageTag {
                        tag_id,
                        target,
                        source: TagSource::Ai,
                        state: TagState::Pending,
                        confidence: Some(confidence),
                        rationale: Some(rationale),
                    },
                )
            })
            .await?)
    }

    /// Accept or reject a pending suggestion — `ResolveSuggestion`'s backing
    /// call. Accepting pushes the tag to IMAP exactly like [`add_tag`](Self::
    /// add_tag) would (including the `auto` downgrade) before flipping the
    /// row to [`TagState::Applied`]; rejecting only flips it to
    /// [`TagState::Rejected`] (kept, not deleted — see the module docs).
    ///
    /// # Errors
    /// [`Error::NotFound`] if `message_tag_id` does not exist;
    /// [`Error::FailedPrecondition`] if it is not currently
    /// [`TagState::Pending`] (already resolved, or was never a suggestion).
    /// Otherwise the IMAP mutator's error, or a mapped storage error.
    #[tracing::instrument(
        skip(self),
        fields(message_tag_id = message_tag_id, accept = accept),
        err
    )]
    pub async fn resolve_suggestion(&self, message_tag_id: i64, accept: bool) -> Result<(), Error> {
        let existing = self
            .db
            .read(move |conn| repo::get_message_tag(conn, message_tag_id))
            .await?
            .ok_or_else(|| Error::not_found(format!("suggestion {message_tag_id}")))?;
        if existing.state != TagState::Pending {
            return Err(Error::failed_precondition(format!(
                "suggestion {message_tag_id} is not pending"
            )));
        }

        if accept {
            let tag = self
                .db
                .read(move |conn| repo::get_tag(conn, existing.tag_id))
                .await?
                .ok_or_else(|| Error::not_found(format!("tag {}", existing.tag_id)))?;
            let message_ids = self.target_message_ids(existing.target).await?;
            let groups = sync::group_by_mailbox(&self.db, &message_ids).await?;
            if let sync::WireOutcome::Downgrade =
                sync::apply_wire(self.imap.as_ref(), &tag, &self.config.imap, &groups, true).await?
            {
                let tag_id = tag.id;
                self.db
                    .write(move |conn| repo::downgrade_tag_to_local(conn, tag_id))
                    .await?;
            }
        }

        let new_state = if accept {
            TagState::Applied
        } else {
            TagState::Rejected
        };
        let resolved = self
            .db
            .write(move |conn| repo::resolve_message_tag(conn, message_tag_id, new_state))
            .await?;
        if !resolved {
            return Err(Error::failed_precondition(format!(
                "suggestion {message_tag_id} was resolved concurrently"
            )));
        }
        Ok(())
    }

    /// Import a message's already-synced IMAP keywords/system flags as
    /// local tags with `source = TagSource::Imap` — prd.md: "Inbound server
    /// keywords/labels import as `source='imap'` tags."
    ///
    /// Reads `flags` (already populated by an ordinary sync — this performs
    /// no IMAP round trip of its own) and derives tag names via
    /// [`derive_imap_tag_names`]: any flag matching `tags.imap.keyword_prefix`
    /// becomes a tag named by its remainder, and — when
    /// `tags.imap.map_system` is set — `\Flagged`/`$Important` map to the
    /// reserved built-ins `"flagged"`/`"important"`. Idempotent: re-running
    /// this against a message whose flags have not changed applies nothing
    /// new (the same `UNIQUE` partial index every other apply path relies
    /// on).
    ///
    /// # Errors
    /// [`Error::NotFound`] if the message does not exist. Otherwise a mapped
    /// storage error.
    #[tracing::instrument(skip(self), fields(message_id = message_id), err)]
    pub async fn import_imap_keywords(&self, message_id: i64) -> Result<Vec<Tag>, Error> {
        let message = self
            .db
            .read(move |conn| crate::repo::get_message(conn, message_id))
            .await?
            .ok_or_else(|| Error::not_found(format!("message {message_id}")))?;
        let flags = self
            .db
            .read(move |conn| crate::repo::list_flags(conn, message_id))
            .await?;
        let names = derive_imap_tag_names(
            &flags,
            &self.config.imap.keyword_prefix,
            self.config.imap.map_system,
        );

        let mut imported = Vec::new();
        for name in names {
            let tag = self.get_or_create_tag(message.account_id, &name).await?;
            let tag_id = tag.id;
            let applied = self
                .db
                .write(move |conn| {
                    repo::insert_message_tag(
                        conn,
                        &NewMessageTag {
                            tag_id,
                            target: Target::Message(message_id),
                            source: TagSource::Imap,
                            state: TagState::Applied,
                            confidence: None,
                            rationale: None,
                        },
                    )
                })
                .await?;
            if applied.is_some() {
                imported.push(tag);
            }
        }
        Ok(imported)
    }

    /// Resolve a filter-only query string into its matching message ids,
    /// scoped to `account_id` — [`BulkSelector::Query`]'s resolution. See
    /// [`query`]'s own module docs for exactly what it understands.
    ///
    /// # Errors
    /// A mapped storage error.
    async fn resolve_query(&self, account_id: i64, raw: &str) -> Result<Vec<i64>, Error> {
        let (where_sql, params) = query::compile(account_id, raw);
        let sql = format!("SELECT id FROM messages WHERE {where_sql}");
        Ok(self
            .db
            .read(move |conn| {
                let mut stmt = conn.prepare(&sql)?;
                let bind: Vec<&dyn rusqlite::ToSql> =
                    params.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
                let rows = stmt.query_map(bind.as_slice(), |row| row.get::<_, i64>(0))?;
                rows.collect::<rusqlite::Result<Vec<i64>>>()
            })
            .await?)
    }

    /// Filter `ids` down to the ones that actually belong to `account_id`,
    /// dropping the rest -- [`BulkSelector::MessageIds`]'s account scope.
    ///
    /// # Errors
    /// A mapped storage error.
    async fn scope_ids_to_account(&self, account_id: i64, ids: &[i64]) -> Result<Vec<i64>, Error> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let ids = ids.to_vec();
        Ok(self
            .db
            .read(move |conn| {
                let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
                let sql = format!(
                    "SELECT id FROM messages WHERE account_id = ? AND id IN ({placeholders})"
                );
                let mut stmt = conn.prepare(&sql)?;
                let mut params: Vec<&dyn rusqlite::ToSql> = Vec::with_capacity(ids.len() + 1);
                params.push(&account_id);
                params.extend(ids.iter().map(|id| id as &dyn rusqlite::ToSql));
                let rows = stmt.query_map(params.as_slice(), |row| row.get::<_, i64>(0))?;
                rows.collect::<rusqlite::Result<Vec<i64>>>()
            })
            .await?)
    }

    /// The message id(s) a target's IMAP round-trip must touch: the message
    /// itself, or every current member of a thread.
    async fn target_message_ids(&self, target: Target) -> Result<Vec<i64>, Error> {
        match target {
            Target::Message(id) => Ok(vec![id]),
            Target::Thread(id) => Ok(self
                .db
                .read(move |conn| crate::repo::list_thread_message_ids(conn, id))
                .await?),
        }
    }

    /// The account a target belongs to.
    async fn resolve_account_for_target(&self, target: Target) -> Result<i64, Error> {
        match target {
            Target::Message(id) => {
                let message = self
                    .db
                    .read(move |conn| crate::repo::get_message(conn, id))
                    .await?;
                Ok(message
                    .ok_or_else(|| Error::not_found(format!("message {id}")))?
                    .account_id)
            }
            Target::Thread(id) => {
                let thread = self
                    .db
                    .read(move |conn| crate::repo::get_thread(conn, id))
                    .await?;
                Ok(thread
                    .ok_or_else(|| Error::not_found(format!("thread {id}")))?
                    .account_id)
            }
        }
    }
}

/// [`TagStore::create_tag`]'s transaction outcome: a plain
/// `rusqlite::Result<Tag>` has no room for "rejected, not a storage error"
/// (the cycle and invalid-parent cases), so the transaction closure returns
/// this instead and `create_tag` converts the rejection variants into a
/// proper [`Error::InvalidArgument`] once it is back in `async` context.
enum Outcome {
    Tag(Tag),
    WouldCycle,
    /// An explicit `parent_id` that does not name any tag, or names one in
    /// a different account.
    InvalidParent,
    /// Unreachable in practice (a row this same transaction just
    /// inserted/updated failing to read back) — kept as a variant rather
    /// than an `.expect()` so the impossible case is still a typed `Error`,
    /// not a panic.
    Missing,
}

/// Whether `parent_id` names a tag that exists and belongs to `account_id`
/// — [`TagStore::create_tag`]'s guard against an explicit `parent_id` that
/// would otherwise reach `repo::insert_tag`/`repo::set_tag_parent` as a
/// foreign-key violation (mapped to `Error::Internal`, not the
/// `INVALID_ARGUMENT` a bad caller-supplied id deserves) or, worse, a
/// parent in an account this call has no business touching.
fn parent_belongs_to_account(
    conn: &Connection,
    account_id: i64,
    parent_id: i64,
) -> rusqlite::Result<bool> {
    Ok(repo::get_tag(conn, parent_id)?.is_some_and(|tag| tag.account_id == account_id))
}

/// Derive local tag names from a message's already-synced IMAP flags —
/// see [`TagStore::import_imap_keywords`].
fn derive_imap_tag_names(flags: &[String], keyword_prefix: &str, map_system: bool) -> Vec<String> {
    let mut names = Vec::new();
    for flag in flags {
        if !keyword_prefix.is_empty() {
            if let Some(name) = flag.strip_prefix(keyword_prefix) {
                if !name.is_empty() {
                    names.push(name.to_owned());
                    continue;
                }
            }
        }
        if map_system {
            match flag.as_str() {
                "\\Flagged" => names.push("flagged".to_owned()),
                "$Important" => names.push("important".to_owned()),
                _ => {}
            }
        }
    }
    names
}

#[cfg(test)]
mod tests;
