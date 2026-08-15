//! The `FinderService` gRPC implementation (task 59, prd.md III-1).
//!
//! # Cancellation is supersession, and it has to actually stop the scan
//!
//! prd.md's finder is driven per keystroke. The client's contract is simply
//! "issue a new `Find` and drop the old stream", and this file is what makes
//! that mean something: [`Generation`] is a single "currently streaming" slot
//! — the same shape `search_service` uses, and for the same reason (this
//! daemon serves one interactive picker at a time, and an older query is by
//! construction stale) — and beginning a new `Find` cancels whichever token
//! held it.
//!
//! What the two services do *with* the token differs, because their expensive
//! work differs. Search's cost is SQLite scans, so its cancellation is
//! `retrieve::cancel::interruptible_read` turning the token into a real
//! `sqlite3_interrupt()`. The finder's cost is a CPU walk over an in-memory
//! index with no database involved at all, so its cancellation is
//! `rmail_core::finder`'s own scan polling the token every
//! `CANCEL_STRIDE` entries. Either way the abandoned work *stops*; it does
//! not merely stop being read while it keeps a thread busy.
//!
//! A superseded stream ends cleanly with a final `complete` batch flagged
//! `superseded`, rather than erroring. A picker that has already moved on
//! must not be handed a `CANCELLED` status to render, and a client that has
//! *not* moved on (a slow `mail find` racing an unrelated one) needs to be
//! able to tell a short answer from a complete one — which the flag is for.
//!
//! # `BatchAction` delegates; it does not acquire authority
//!
//! The finder is a *selector*. Every action it applies runs through the same
//! [`MailStore`] `MailService` uses, so archiving from a picker and archiving
//! from `mail archive` are one operation with one IMAP reconciliation and one
//! event — the same "reuse the real path rather than assemble a second,
//! subtly different one" argument `SavedSearchService.RunSavedSearch` makes
//! about the search pipeline.
//!
//! The action vocabulary is closed and message-only. An unknown verb is
//! `INVALID_ARGUMENT`, never a silent no-op, and a ref that no longer exists
//! is reported in `not_found` rather than failing the batch: a picker's
//! selection can outlive the mail it names, and one vanished id must not cost
//! the other nineteen.
#![allow(clippy::result_large_err)] // see mail_service.rs's note on `Result<_, Status>`

use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};

use rmail_core::config::FinderConfig;
use rmail_core::finder::index::FinderIndex;
use rmail_core::finder::{Batch, FindQuery, Finder, ItemKind, Match, Query, Scope};
use rmail_core::mail::MailStore;
use rmail_core::{repo, Database, Error as RmailError};
use rmail_proto::v1::finder_service_server::FinderService;
use rmail_proto::v1::{
    BatchActionRequest, BatchActionResponse, FindBatch, FindRequest, FindResult,
    FinderRebuildRequest, FinderRebuildResponse, FinderScope as ProtoScope, FinderStatusRequest,
    FinderStatusResponse, ItemKind as ProtoItemKind,
};
use rusqlite::OptionalExtension;
use tokio_stream::wrappers::ReceiverStream;
use tokio_util::sync::CancellationToken;
use tonic::{Request, Response, Status};
use tracing::Instrument;

/// How many batches may sit between the scan and a client before the scan
/// blocks. Small on purpose: a batch is a *snapshot*, so a client falling
/// behind gains nothing from a deep queue of stale ones — it would just
/// render four intermediate lists in a row on catch-up.
const STREAM_BUFFER: usize = 4;

/// The most messages one `BatchAction` may name.
///
/// Every id costs a full `MailStore` mutation — an IMAP round trip — and this
/// is a unary RPC, so without a cap the client decides how long the handler
/// runs and therefore how long graceful shutdown is held open. Comfortably
/// larger than `finder.max_results` (200), so acting on a whole page of
/// results is always allowed; a caller with more than this is running a bulk
/// job and should page it.
const MAX_BATCH_REFS: usize = 1_000;

/// The `\Seen` IMAP flag, spelled once.
const SEEN: &str = "\\Seen";
/// The `\Flagged` IMAP flag, spelled once.
const FLAGGED: &str = "\\Flagged";

/// The single "currently streaming" slot. See the module docs.
#[derive(Default)]
struct Generation {
    current: Arc<Mutex<Option<CancellationToken>>>,
}

impl Generation {
    /// Register a new stream as current, cancelling whichever held the slot
    /// before it. The returned token is a child of `shutdown`, so daemon
    /// shutdown stops the scan even if no later keystroke ever supersedes it.
    fn begin(&self, shutdown: &CancellationToken) -> CancellationToken {
        let token = shutdown.child_token();
        let previous = {
            let mut guard = self.current.lock().unwrap_or_else(PoisonError::into_inner);
            guard.replace(token.clone())
        };
        if let Some(previous) = previous {
            previous.cancel();
        }
        token
    }
}

/// The `FinderService` handler.
#[derive(Clone)]
pub struct FinderApi {
    finder: Finder,
    index: FinderIndex,
    mail: MailStore,
    db: Database,
    /// Where `archive` moves mail. Read from `[rules]` rather than given its
    /// own `[finder]` key: "the folder this account archives into" is one
    /// fact about a mailbox, and two settings for it would eventually
    /// disagree.
    archive_mailbox: String,
    default_scope: Scope,
    shutdown: CancellationToken,
    generation: Arc<Generation>,
}

impl FinderApi {
    /// Build the handler.
    ///
    /// `index` and `finder` are passed in rather than constructed here
    /// because the daemon's drain loop writes into the same
    /// `Arc<RwLock<FinderStore>>` this handler reads: there is exactly one
    /// store per daemon, and a constructor that made its own would give the
    /// drain and the queries two different views of the mailbox.
    #[must_use]
    pub fn new(
        finder: Finder,
        index: FinderIndex,
        mail: MailStore,
        db: Database,
        config: &FinderConfig,
        archive_mailbox: impl Into<String>,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            finder,
            index,
            mail,
            db,
            archive_mailbox: archive_mailbox.into(),
            default_scope: Scope::from(config.default_scope),
            shutdown,
            generation: Arc::new(Generation::default()),
        }
    }
}

#[tonic::async_trait]
impl FinderService for FinderApi {
    type FindStream = Pin<Box<dyn tokio_stream::Stream<Item = Result<FindBatch, Status>> + Send>>;

    #[tracing::instrument(skip(self, request), fields(scope, limit))]
    async fn find(
        &self,
        request: Request<FindRequest>,
    ) -> Result<Response<Self::FindStream>, Status> {
        let req = request.into_inner();
        // The sigil grammar has exactly one implementation, in the core, so
        // no client can drift from it — the same argument `search_cli` makes
        // for leaving `~`/`=` to `query::parse`.
        let requested = decode_scope(req.scope).unwrap_or(self.default_scope);
        let parsed = Query::parse(&req.query, requested);
        let limit = self
            .finder
            .clamp_limit(usize::try_from(req.limit).unwrap_or(usize::MAX));
        tracing::Span::current().record("scope", parsed.scope.id());
        tracing::Span::current().record("limit", limit);

        let query = FindQuery {
            text: parsed.text,
            scope: parsed.scope,
            account_id: (req.account_id != 0).then_some(req.account_id),
            mailbox_id: (req.mailbox_id != 0).then_some(req.mailbox_id),
            limit,
            with_positions: req.with_positions,
        };

        // Registering the generation *before* the scan is spawned is what
        // makes the race favor the newer request: this call resolves, and
        // the client sees a stream, before any matching has happened.
        let cancel = self.generation.begin(&self.shutdown);
        let (tx, rx) = tokio::sync::mpsc::channel(STREAM_BUFFER);
        let (batches_tx, batches_rx) = tokio::sync::mpsc::channel::<Batch>(STREAM_BUFFER);
        let finder = self.finder.clone();

        tokio::spawn(
            async move {
                let scan = finder.find_batched(query, cancel, batches_tx);
                // `async move`, so this future *owns* `batches_rx` and drops
                // it the instant it returns. That ownership is load-bearing
                // twice over. It is what makes the client-hangup path below
                // actually stop the scan — a closed receiver is what turns
                // the scan's next `blocking_send` into an error, which is the
                // `ControlFlow::Break` `Finder::find_batched` documents. And
                // it is what keeps a full channel from becoming a deadlock:
                // borrowing the receiver would leave it alive but unpolled
                // inside the `join!`, parking a blocking-pool thread forever
                // on a send nobody will ever receive — while that thread
                // holds `FinderStore`'s read lock, which would in turn wedge
                // the drain's `store.write()` and freeze the index daemon-wide.
                let forward = async move {
                    let mut batches_rx = batches_rx;
                    let mut sent = 0u64;
                    let mut last = (0u64, false);
                    while let Some(batch) = batches_rx.recv().await {
                        last = (batch.stats.scanned, batch.stats.cancelled);
                        let message = FindBatch {
                            results: batch.items.into_iter().map(to_proto_result).collect(),
                            complete: batch.complete,
                            scanned: batch.stats.scanned,
                            superseded: batch.stats.cancelled,
                        };
                        if tx.send(Ok(message)).await.is_err() {
                            // The client hung up. Returning drops
                            // `batches_rx`, which stops the scan.
                            return (tx, sent, last);
                        }
                        sent += 1;
                    }
                    (tx, sent, last)
                };
                // Both halves run together: the scan's `blocking_send` needs
                // a live reader, and the forwarder needs the scan to be
                // producing.
                let (result, (tx, batches, (scanned, superseded))) = tokio::join!(scan, forward);
                match result {
                    Ok(stats) => tracing::debug!(
                        batches,
                        scanned = stats.scanned,
                        aligned = stats.aligned,
                        matched = stats.matched,
                        superseded = stats.cancelled,
                        "a finder scan finished"
                    ),
                    Err(error) => {
                        tracing::warn!(%error, scanned, superseded, "a finder scan failed");
                        let _ = tx.send(Err(Status::from(error))).await;
                    }
                }
            }
            .instrument(tracing::Span::current()),
        );

        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }

    #[tracing::instrument(skip(self, request), fields(action, requested))]
    async fn batch_action(
        &self,
        request: Request<BatchActionRequest>,
    ) -> Result<Response<BatchActionResponse>, Status> {
        let req = request.into_inner();
        let action = decode_action(&req.action)?;
        require_message_kind(req.kind)?;
        if req.ref_ids.len() > MAX_BATCH_REFS {
            // Every id is a full `MailStore` mutation, i.e. an IMAP round
            // trip. Without a cap the *client* sets this handler's runtime,
            // and `mail_service`'s shutdown bound ("a low multiple of
            // IMAP_DEADLINE per RPC") becomes that multiple times a
            // client-supplied integer — a single call holding graceful
            // shutdown open for hours.
            return Err(Status::from(RmailError::invalid_argument(format!(
                "a batch action may name at most {MAX_BATCH_REFS} messages; \
                 this one named {}",
                req.ref_ids.len()
            ))));
        }
        tracing::Span::current().record("action", &req.action);
        tracing::Span::current().record("requested", req.ref_ids.len());

        let mut applied = 0u32;
        let mut not_found = Vec::new();
        for message_id in req.ref_ids {
            // Checked per id, not once: the loop is unbounded in *time* even
            // though it is bounded in length, and a daemon that is stopping
            // must not keep opening IMAP conversations. Reported rather than
            // silently truncated — a caller who asked for twenty archives and
            // got nine needs to know which it was.
            if self.shutdown.is_cancelled() {
                return Err(Status::from(RmailError::unavailable(format!(
                    "the daemon is shutting down; {applied} of the batch were applied"
                ))));
            }
            match self.apply(action, message_id).await {
                Ok(()) => applied = applied.saturating_add(1),
                // prd.md: "stale ref -> action returns not_found, entry
                // pruned next drain". One vanished id must not fail the rest
                // of a selection, so NOT_FOUND is reported per id rather than
                // raised as the call's status.
                Err(RmailError::NotFound(_)) => not_found.push(message_id),
                Err(error) => return Err(Status::from(error)),
            }
        }
        tracing::debug!(
            applied,
            missing = not_found.len(),
            "applied a finder batch action"
        );
        Ok(Response::new(BatchActionResponse { applied, not_found }))
    }

    #[tracing::instrument(skip(self, _request))]
    async fn rebuild_index(
        &self,
        _request: Request<FinderRebuildRequest>,
    ) -> Result<Response<FinderRebuildResponse>, Status> {
        let entries = self.index.rebuild().await.map_err(Status::from)?;
        Ok(Response::new(FinderRebuildResponse {
            entries: entries as u64,
        }))
    }

    #[tracing::instrument(skip(self, _request))]
    async fn index_status(
        &self,
        _request: Request<FinderStatusRequest>,
    ) -> Result<Response<FinderStatusResponse>, Status> {
        let status = self.index.status().await.map_err(Status::from)?;
        Ok(Response::new(FinderStatusResponse {
            entries: status.entries as u64,
            bytes: status.bytes as u64,
            pending: u64::try_from(status.pending).unwrap_or(0),
            rejected: status.rejected,
            refreshed_at: status.refreshed_at,
        }))
    }
}

/// The closed action vocabulary. See the module docs on why it is closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ItemAction {
    Archive,
    Delete,
    Read,
    Unread,
    Flag,
    Unflag,
}

impl FinderApi {
    /// Apply one action to one message.
    ///
    /// `read`/`unread`/`flag`/`unflag` are read-modify-write because
    /// `MailStore::set_flags` takes the *complete* desired set (it has to —
    /// that is what IMAP `STORE FLAGS` does). Reading first is what keeps
    /// "mark read" from also clearing `\Flagged`.
    async fn apply(&self, action: ItemAction, message_id: i64) -> Result<(), RmailError> {
        match action {
            ItemAction::Archive => {
                let destination = self.archive_destination(message_id).await?;
                self.mail.move_message(message_id, destination).await
            }
            ItemAction::Delete => self.mail.delete_message(message_id).await,
            ItemAction::Read => self.set_flag(message_id, SEEN, true).await,
            ItemAction::Unread => self.set_flag(message_id, SEEN, false).await,
            ItemAction::Flag => self.set_flag(message_id, FLAGGED, true).await,
            ItemAction::Unflag => self.set_flag(message_id, FLAGGED, false).await,
        }
    }

    /// The mailbox `archive` moves this message into, in its own account.
    async fn archive_destination(&self, message_id: i64) -> Result<i64, RmailError> {
        let name = self.archive_mailbox.clone();
        let found = self
            .db
            .read(move |conn| {
                let Some(message) = repo::get_message(conn, message_id)? else {
                    return Ok(None);
                };
                let destination = conn
                    .query_row(
                        "SELECT id FROM mailboxes WHERE account_id = ?1 AND name = ?2",
                        rusqlite::params![message.account_id, name],
                        |row| row.get::<_, i64>(0),
                    )
                    .optional()?;
                // `Some(None)` is "the message exists but the folder does
                // not", which is a different answer from "no such message" —
                // the two produce different statuses below.
                Ok(Some(destination))
            })
            .await?
            .ok_or_else(|| RmailError::not_found(format!("message {message_id}")))?;
        found.ok_or_else(|| {
            // FAILED_PRECONDITION, not NOT_FOUND: the *message* is fine, the
            // account simply has no such folder, and reporting it as a stale
            // ref would send the caller looking for the wrong problem.
            RmailError::failed_precondition(format!(
                "this account has no mailbox named {:?} to archive into",
                self.archive_mailbox
            ))
        })
    }

    /// Add or remove one flag, preserving every other.
    async fn set_flag(&self, message_id: i64, flag: &str, present: bool) -> Result<(), RmailError> {
        let full = self.mail.get(message_id).await?;
        let mut flags = full.message.flags;
        let held = flags.iter().any(|f| f == flag);
        if held == present {
            return Ok(());
        }
        if present {
            flags.push(flag.to_owned());
        } else {
            flags.retain(|f| f != flag);
        }
        self.mail.set_flags(message_id, flags).await?;
        Ok(())
    }
}

/// Refuse a batch whose ids are not message ids.
///
/// See `BatchActionRequest.kind`: `ref_id` is a row id in whichever source
/// table the kind names, and those id spaces overlap, so an unstated kind is
/// an invitation to mutate the wrong object. `UNSPECIFIED` is refused rather
/// than defaulted for the same reason `search_service::decode_action` refuses
/// an unspecified feedback action — a client that did not say is a client
/// whose whole batch is suspect.
fn require_message_kind(kind: i32) -> Result<(), Status> {
    if kind == ProtoItemKind::Message as i32 {
        return Ok(());
    }
    let named =
        ProtoItemKind::try_from(kind).map_or("an unrecognized kind", |kind| kind.as_str_name());
    Err(Status::from(RmailError::invalid_argument(format!(
        "batch actions apply to messages only; this request named {named}. \
         Set kind = ITEM_KIND_MESSAGE and send only ref_ids from \
         ITEM_KIND_MESSAGE results"
    ))))
}

/// Translate one action verb, refusing anything outside the vocabulary.
fn decode_action(action: &str) -> Result<ItemAction, Status> {
    Ok(match action {
        "archive" => ItemAction::Archive,
        "delete" => ItemAction::Delete,
        "read" => ItemAction::Read,
        "unread" => ItemAction::Unread,
        "flag" => ItemAction::Flag,
        "unflag" => ItemAction::Unflag,
        other => {
            return Err(Status::from(RmailError::invalid_argument(format!(
                "unknown finder action {other:?}; expected one of \
                 archive, delete, read, unread, flag, unflag"
            ))))
        }
    })
}

/// The scope a request named, or `None` for `UNSPECIFIED` (which means "use
/// the server's default", not "search nothing").
fn decode_scope(scope: i32) -> Option<Scope> {
    match ProtoScope::try_from(scope).unwrap_or(ProtoScope::Unspecified) {
        ProtoScope::Unspecified => None,
        ProtoScope::All => Some(Scope::All),
        ProtoScope::Messages => Some(Scope::Only(ItemKind::Message)),
        ProtoScope::Mailboxes => Some(Scope::Only(ItemKind::Mailbox)),
        ProtoScope::Contacts => Some(Scope::Only(ItemKind::Contact)),
        ProtoScope::SavedSearches => Some(Scope::Only(ItemKind::SavedSearch)),
        ProtoScope::Tags => Some(Scope::Only(ItemKind::Tag)),
        ProtoScope::Commands => Some(Scope::Only(ItemKind::Command)),
    }
}

/// The wire number for a kind. Written out rather than derived from
/// `ItemKind::code()`: the wire enum reserves 0 for `UNSPECIFIED`, so the two
/// numberings differ by one, and a `+ 1` here would be a silent off-by-one
/// waiting for the day either side gains a variant.
fn to_proto_kind(kind: ItemKind) -> ProtoItemKind {
    match kind {
        ItemKind::Message => ProtoItemKind::Message,
        ItemKind::Mailbox => ProtoItemKind::Mailbox,
        ItemKind::Contact => ProtoItemKind::Contact,
        ItemKind::SavedSearch => ProtoItemKind::SavedSearch,
        ItemKind::Tag => ProtoItemKind::Tag,
        ItemKind::Command => ProtoItemKind::Command,
    }
}

fn to_proto_result(item: Match) -> FindResult {
    FindResult {
        item_id: item.item_id,
        kind: to_proto_kind(item.kind) as i32,
        ref_id: item.ref_id,
        score: item.score,
        primary_text: item.primary_text,
        secondary: item.secondary,
        positions: item.positions,
        account_id: item.account_id,
        mailbox_id: item.mailbox_id,
    }
}

#[cfg(test)]
mod tests;
