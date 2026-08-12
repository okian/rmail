//! The mail domain service: list/get/thread reads plus the mutations that
//! must agree with the IMAP server they mirror (task 39).
//!
//! # Reads never touch IMAP
//!
//! `messages` is already a complete local mirror of what sync (tasks 9/12/15)
//! downloaded — [`MailStore::list`]/[`MailStore::get`]/[`MailStore::get_thread`]/
//! [`MailStore::attachment_bytes`] are pure local reads. Round-tripping to the
//! mailbox server on every read would make reading mail as slow and as
//! fragile as the network it should be insulated from; insulating it is the
//! whole point of syncing in the first place.
//!
//! # Ordering: IMAP first, local mirror second
//!
//! A mutation touches two systems that cannot commit together: the IMAP
//! server (the actual mailbox) and this database (a cache of it). When they
//! disagree, one of them is wrong, and the design here picks which one on
//! purpose rather than by accident of write order.
//!
//! Every mutating method calls out to IMAP *before* touching a local row. If
//! the IMAP call fails, nothing local changes and nothing is reported to the
//! caller as having happened — no divergence, full stop. If IMAP succeeds and
//! the local write then fails (a narrow window: the write is a single
//! statement against an already-open database), the caller sees an error, but
//! the mailbox has already changed on the server. That is the *safer*
//! direction to fail in: the next scheduled sync ([`crate::sync`]) re-derives
//! local state from the server regardless, so a local write that lost a race
//! against IMAP heals itself on the next pass. The alternative order — write
//! local first — would let a caller see a mutation "succeed" while the real
//! mailbox never changed, a lie that persists until an operator notices,
//! because nothing ever re-derives IMAP state *from* the local database.
//!
//! # Move does not guess a new UID
//!
//! IMAP does not hand back the destination UID a `MOVE`/`COPY` assigns unless
//! the server advertises `UIDPLUS` and the client parses `COPYUID` out of the
//! untagged response — this crate's IMAP client does not expose that (see
//! [`crate::imap::mutate`]). Without it, a moved message's local row cannot be
//! kept both *present* and *correctly identified*: leaving its
//! `(mailbox_id, uidvalidity, uid)` as-is would have the row lie about where
//! the message lives, and the destination folder's next sync would then
//! insert a second, genuine row for the same message once it discovers the
//! real one — a permanent duplicate.
//!
//! So [`MailStore::move_message`] deletes the local row once the server
//! confirms the move — via [`crate::sync::remove_messages`], *not* a bare
//! `DELETE FROM messages`, because a message can be threaded, entity-mentioned
//! and semantically indexed, and none of those cascade from a foreign key
//! (`vec_chunks`/`vec_messages` are `sqlite-vec` virtual tables, which cannot
//! carry one — see [`crate::index::semantic::drop_vectors`]) or from a plain
//! delete (a thread's `message_count`/`participants`/`root_message_id` need
//! recomputing, and a thread left with zero members needs to go). This is the
//! same removal path an ordinary IMAP-side expunge already goes through
//! (`crate::sync::engine`'s `Change::Removed`), applied here for the
//! client-initiated case. [`MailStore::move_message`] then emits an
//! [`EventKind::Moved`] event carrying the old id for anything downstream
//! that needs to drop it. The destination folder's next sync discovers the
//! message fresh, under its real UID, and re-threads it by
//! `Message-ID`/`References` like any other newly-seen mail. Until that sync
//! runs, the message is briefly invisible locally despite being on the server
//! — a gap bounded by the sync interval, not silent divergence. If the local
//! removal itself fails after a successful IMAP move, the stale row's `uid`
//! no longer exists in its (source) folder, so that folder's next sync
//! reclaims it anyway via the ordinary expunge path.
//!
//! # Copy needs no local bookkeeping at all
//!
//! A `Copy` leaves the source message untouched and creates a message this
//! database has never seen, under a UID it does not know. There is nothing
//! correct to do locally, so [`MailStore::copy_message`] does nothing beyond
//! the IMAP call: the destination folder's next sync discovers the copy as
//! ordinary new mail and emits its own `NewMail` event.
//!
//! # `UIDVALIDITY` travels with every mutation
//!
//! [`MutationTarget::uidvalidity`] is read from the local mirror alongside the
//! UID and handed to [`crate::imap::mutate::ImapMutator`], which verifies it
//! against the server's live `SELECT` response before issuing any mutating
//! command — see that module's docs. Without it, a `UIDVALIDITY` bump between
//! this message's last sync and this call would let a stale UID silently
//! address a different message on the server.

use std::sync::Arc;

use rusqlite::OptionalExtension;

use crate::error::Error;
use crate::events::{EventKind, EventLog, NewEvent};
use crate::imap::mutate::ImapMutator;
use crate::repo;
use crate::storage::Database;

pub mod annotations;

#[cfg(test)]
mod tests;

/// Server-side default for [`MailStore::list`] when the caller asks for no
/// particular page size.
pub const DEFAULT_LIST_LIMIT: i64 = 100;

/// Hard ceiling on [`MailStore::list`], regardless of what is requested.
///
/// Full opaque-token pagination lands in task 40; this cap is what keeps a
/// pathological request from reading a whole mailbox into memory in the
/// meantime.
pub const MAX_LIST_LIMIT: i64 = 500;

/// A message with its flag set attached — `repo::Message` does not carry
/// flags inline, since they live in their own table.
#[derive(Debug, Clone)]
pub struct MessageWithFlags {
    /// The message row.
    pub message: repo::Message,
    /// Its current flags, sorted.
    pub flags: Vec<String>,
}

/// A message with its body and attachment metadata — the `Get` view.
#[derive(Debug, Clone)]
pub struct FullMessage {
    /// The message and its flags.
    pub message: MessageWithFlags,
    /// Its attachments' metadata (not their bytes — see
    /// [`MailStore::attachment_bytes`]).
    pub attachments: Vec<repo::Attachment>,
}

/// A thread and every message in it, oldest first — the `GetThread` view.
#[derive(Debug, Clone)]
pub struct ThreadView {
    /// The thread row.
    pub thread: repo::Thread,
    /// Its messages, oldest first (the order a conversation reads in).
    pub messages: Vec<MessageWithFlags>,
}

/// One attachment's bytes, re-derived from `messages.raw` on demand — see
/// [`MailStore::attachment_bytes`].
#[derive(Debug, Clone)]
pub struct AttachmentBytes {
    /// Filename, if the part carried one.
    pub filename: Option<String>,
    /// `type/subtype`, if known.
    pub content_type: Option<String>,
    /// The decoded bytes.
    pub bytes: Vec<u8>,
}

/// Where a mutation's IMAP call needs to land: the account, the message's
/// current mailbox (by name — IMAP addresses folders by name, not by this
/// database's surrogate id), and its identity within that mailbox.
struct MutationTarget {
    account_id: i64,
    mailbox_id: i64,
    mailbox_name: String,
    /// The `UIDVALIDITY` this `uid` was last known valid under — see the
    /// module docs' "`UIDVALIDITY` travels with every mutation" section.
    uidvalidity: i64,
    uid: i64,
}

/// The mail domain service: reads over the local mirror, mutations that
/// reflect to IMAP and the durable event log. See the module docs for the
/// ordering and compensation rules every mutation follows.
///
/// Cheap to clone: every clone shares the database, the event log, and the
/// IMAP mutator.
#[derive(Clone)]
pub struct MailStore {
    db: Database,
    events: EventLog,
    imap: Arc<dyn ImapMutator>,
}

impl std::fmt::Debug for MailStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MailStore").finish_non_exhaustive()
    }
}

impl MailStore {
    /// Build a store over `db`, appending to `events`, mutating IMAP through
    /// `imap`.
    #[must_use]
    pub fn new(db: Database, events: EventLog, imap: Arc<dyn ImapMutator>) -> Self {
        Self { db, events, imap }
    }

    /// The event log this store appends mutations to — what `MailService`'s
    /// `WatchEvents` handler subscribes to, the same way
    /// `SyncService::watch_events` subscribes to `SyncEngine::events`.
    #[must_use]
    pub fn events(&self) -> &EventLog {
        &self.events
    }

    /// List a mailbox's messages, newest first, capped at
    /// [`MAX_LIST_LIMIT`]. `requested <= 0` uses [`DEFAULT_LIST_LIMIT`].
    ///
    /// # Errors
    /// A mapped storage error.
    #[tracing::instrument(skip(self), fields(mailbox_id = mailbox_id), err)]
    pub async fn list(
        &self,
        mailbox_id: i64,
        requested: i64,
    ) -> Result<Vec<MessageWithFlags>, Error> {
        let limit = normalize_limit(requested);
        Ok(self
            .db
            .read(move |conn| {
                let messages = repo::list_messages(conn, mailbox_id, limit)?;
                let mut out = Vec::with_capacity(messages.len());
                for message in messages {
                    let flags = repo::list_flags(conn, message.id)?;
                    out.push(MessageWithFlags { message, flags });
                }
                Ok(out)
            })
            .await?)
    }

    /// Fetch one message, with its body and attachment metadata.
    ///
    /// # Errors
    /// [`Error::NotFound`] if no such message; otherwise a mapped storage
    /// error.
    #[tracing::instrument(skip(self), fields(message_id = message_id), err)]
    pub async fn get(&self, message_id: i64) -> Result<FullMessage, Error> {
        let found = self
            .db
            .read(move |conn| {
                let Some(message) = repo::get_message(conn, message_id)? else {
                    return Ok(None);
                };
                let flags = repo::list_flags(conn, message_id)?;
                let attachments = repo::list_attachments(conn, message_id)?;
                Ok(Some(FullMessage {
                    message: MessageWithFlags { message, flags },
                    attachments,
                }))
            })
            .await?;
        found.ok_or_else(|| Error::not_found(format!("message {message_id}")))
    }

    /// Fetch a thread and every message in it, oldest first.
    ///
    /// # Errors
    /// [`Error::NotFound`] if no such thread; otherwise a mapped storage
    /// error.
    #[tracing::instrument(skip(self), fields(thread_id = thread_id), err)]
    pub async fn get_thread(&self, thread_id: i64) -> Result<ThreadView, Error> {
        let found = self
            .db
            .read(move |conn| {
                let Some(thread) = repo::get_thread(conn, thread_id)? else {
                    return Ok(None);
                };
                let ids = repo::list_thread_message_ids(conn, thread_id)?;
                let mut messages = Vec::with_capacity(ids.len());
                for id in ids {
                    // A thread's message-id list and the messages table can
                    // only disagree mid-delete; skip rather than fail the
                    // whole thread over a row that vanished a moment ago.
                    if let Some(message) = repo::get_message(conn, id)? {
                        let flags = repo::list_flags(conn, id)?;
                        messages.push(MessageWithFlags { message, flags });
                    }
                }
                Ok(Some(ThreadView { thread, messages }))
            })
            .await?;
        found.ok_or_else(|| Error::not_found(format!("thread {thread_id}")))
    }

    /// Re-derive one attachment's bytes from the message's stored raw RFC822,
    /// by its positional part id (matching `attachments.part_id` — see
    /// [`crate::message::parse::parse_message`]).
    ///
    /// The bytes are not stored a second time anywhere (see
    /// [`crate::attach`]'s module docs for why); this parses `messages.raw`
    /// fresh on every call. Fine for a single attachment fetched at most a
    /// handful of times, unlike the extraction pipeline's every-attachment,
    /// every-message sweep.
    ///
    /// # Errors
    /// [`Error::NotFound`] if the message or the part does not exist;
    /// [`Error::FailedPrecondition`] if the message has no stored body;
    /// otherwise a mapped storage error.
    #[tracing::instrument(skip(self), fields(message_id = message_id, part_id = part_id), err)]
    pub async fn attachment_bytes(
        &self,
        message_id: i64,
        part_id: &str,
    ) -> Result<AttachmentBytes, Error> {
        let raw: Option<Option<Vec<u8>>> = self
            .db
            .read(move |conn| {
                conn.query_row(
                    "SELECT raw FROM messages WHERE id = ?1",
                    [message_id],
                    |row| row.get::<_, Option<Vec<u8>>>(0),
                )
                .optional()
            })
            .await?;
        let raw = match raw {
            None => return Err(Error::not_found(format!("message {message_id}"))),
            Some(None) => {
                return Err(Error::failed_precondition(format!(
                    "message {message_id} has no stored body"
                )))
            }
            Some(Some(raw)) => raw,
        };

        let part_id_owned = part_id.to_owned();
        let for_decode = part_id_owned.clone();
        let found = tokio::task::spawn_blocking(move || decode_attachment(&raw, &for_decode))
            .await
            .map_err(|e| Error::internal(format!("attachment decode task failed: {e}")))?;
        found.ok_or_else(|| {
            Error::not_found(format!(
                "attachment {part_id_owned} on message {message_id}"
            ))
        })
    }

    /// Replace a message's flag set, both on the server and locally. A full
    /// replace, matching IMAP's own `STORE FLAGS` semantics and
    /// [`crate::repo::replace_flags`] — not an add/remove delta.
    ///
    /// Returns whether the flag set actually changed (an event is only
    /// appended when it did).
    ///
    /// # Errors
    /// [`Error::InvalidArgument`] if any flag is not a safe IMAP flag atom
    /// (see [`is_safe_flag`]); [`Error::NotFound`] if no such message;
    /// otherwise the IMAP mutator's error, or a mapped storage error.
    #[tracing::instrument(skip(self, flags), fields(message_id = message_id), err)]
    pub async fn set_flags(&self, message_id: i64, flags: Vec<String>) -> Result<bool, Error> {
        for flag in &flags {
            if !is_safe_flag(flag) {
                return Err(Error::invalid_argument(format!(
                    "{flag:?} is not a valid IMAP flag"
                )));
            }
        }

        let target = self.resolve_target(message_id).await?;
        self.imap
            .set_flags(
                target.account_id,
                &target.mailbox_name,
                target.uidvalidity,
                target.uid,
                &flags,
            )
            .await?;

        // In its own transaction (rather than the bare-connection write
        // `replace_flags` also accepts) so a failure partway through its
        // delete-then-reinsert does not leave a partial flag set locally
        // against a fully-applied server state — the same reasoning
        // `sync::delta` already applies by passing it a `&Transaction`.
        let write_flags = flags.clone();
        let changed = self
            .db
            .write(move |conn| {
                let tx = conn.transaction()?;
                let changed = repo::replace_flags(&tx, message_id, &write_flags)?;
                tx.commit()?;
                Ok(changed)
            })
            .await
            .map_err(|error| {
                tracing::error!(
                    message_id,
                    %error,
                    "flags changed on the IMAP server but the local mirror failed to \
                     record it; the next sync reconciles it"
                );
                Error::from(error)
            })?;

        if changed {
            self.events
                .append(
                    NewEvent::new(EventKind::FlagChanged)
                        .account(target.account_id)
                        .mailbox(target.mailbox_id)
                        .message(message_id)
                        .payload(serde_json::json!({ "uid": target.uid, "flags": flags })),
                )
                .await?;
        }
        Ok(changed)
    }

    /// Move a message to another mailbox on the same account. See the module
    /// docs for why the local row is dropped rather than re-pointed.
    ///
    /// # Errors
    /// [`Error::NotFound`] if the message or destination mailbox does not
    /// exist; [`Error::InvalidArgument`] if the destination is on a different
    /// account or is the message's current mailbox; otherwise the IMAP
    /// mutator's error, or a mapped storage error.
    #[tracing::instrument(skip(self), fields(message_id = message_id, dest_mailbox_id = dest_mailbox_id), err)]
    pub async fn move_message(&self, message_id: i64, dest_mailbox_id: i64) -> Result<(), Error> {
        let target = self.resolve_target(message_id).await?;
        let dest = self.resolve_mailbox(dest_mailbox_id).await?;
        if dest.account_id != target.account_id {
            return Err(Error::invalid_argument(
                "cannot move a message to a mailbox on a different account",
            ));
        }
        if dest.id == target.mailbox_id {
            return Err(Error::invalid_argument(
                "destination mailbox is the message's current mailbox",
            ));
        }

        self.imap
            .move_message(
                target.account_id,
                &target.mailbox_name,
                target.uidvalidity,
                target.uid,
                &dest.name,
            )
            .await?;

        // `crate::sync::remove_messages`, not a bare `DELETE FROM messages`:
        // see the module docs' "Move does not guess a new UID" section for
        // why a plain delete would leave orphaned vectors, entity mentions,
        // and thread aggregates behind.
        //
        // `annotations::capture` runs first and inside the same transaction:
        // the message-level tags and notes about to cascade away are the
        // user's own work, and unlike flags or bodies the next sync cannot
        // reconstruct them. See `mail::annotations` for how they get back.
        let dest_id = dest.id;
        if let Err(error) = self
            .db
            .write(move |conn| {
                let tx = conn.transaction()?;
                annotations::capture(&tx, message_id, dest_id)?;
                crate::sync::remove_messages(&tx, &[message_id])?;
                tx.commit()?;
                Ok(())
            })
            .await
        {
            tracing::error!(
                message_id,
                %error,
                "message moved on the IMAP server but the local row could not be \
                 removed; the source folder's next sync reclaims it as an expunge"
            );
            return Err(error.into());
        }

        self.events
            .append(
                NewEvent::new(EventKind::Moved)
                    .account(target.account_id)
                    .mailbox(target.mailbox_id)
                    .message(message_id)
                    .payload(serde_json::json!({
                        "uid": target.uid,
                        "from_mailbox_id": target.mailbox_id,
                        "to_mailbox_id": dest.id,
                        "to_mailbox": dest.name,
                    })),
            )
            .await?;
        Ok(())
    }

    /// Copy a message to another mailbox on the same account, leaving the
    /// source untouched. See the module docs for why this needs no local
    /// bookkeeping.
    ///
    /// # Errors
    /// [`Error::NotFound`] if the message or destination mailbox does not
    /// exist; [`Error::InvalidArgument`] if the destination is on a different
    /// account; otherwise the IMAP mutator's error.
    #[tracing::instrument(skip(self), fields(message_id = message_id, dest_mailbox_id = dest_mailbox_id), err)]
    pub async fn copy_message(&self, message_id: i64, dest_mailbox_id: i64) -> Result<(), Error> {
        let target = self.resolve_target(message_id).await?;
        let dest = self.resolve_mailbox(dest_mailbox_id).await?;
        if dest.account_id != target.account_id {
            return Err(Error::invalid_argument(
                "cannot copy a message to a mailbox on a different account",
            ));
        }

        self.imap
            .copy_message(
                target.account_id,
                &target.mailbox_name,
                target.uidvalidity,
                target.uid,
                &dest.name,
            )
            .await?;
        Ok(())
    }

    /// Delete a message: mark it `\Deleted` and expunge it on the server,
    /// then remove the local row.
    ///
    /// # Errors
    /// [`Error::NotFound`] if no such message; otherwise the IMAP mutator's
    /// error, or a mapped storage error.
    #[tracing::instrument(skip(self), fields(message_id = message_id), err)]
    pub async fn delete_message(&self, message_id: i64) -> Result<(), Error> {
        let target = self.resolve_target(message_id).await?;
        self.imap
            .delete_message(
                target.account_id,
                &target.mailbox_name,
                target.uidvalidity,
                target.uid,
            )
            .await?;

        // `crate::sync::remove_messages` — see `move_message`'s comment on
        // the same call for why a bare delete is not enough.
        if let Err(error) = self
            .db
            .write(move |conn| {
                let tx = conn.transaction()?;
                crate::sync::remove_messages(&tx, &[message_id])?;
                tx.commit()?;
                Ok(())
            })
            .await
        {
            tracing::error!(
                message_id,
                %error,
                "message deleted on the IMAP server but the local row could not be \
                 removed; the next sync reclaims it as an expunge"
            );
            return Err(error.into());
        }

        self.events
            .append(
                NewEvent::new(EventKind::Deleted)
                    .account(target.account_id)
                    .mailbox(target.mailbox_id)
                    .message(message_id)
                    .payload(serde_json::json!({ "uid": target.uid })),
            )
            .await?;
        Ok(())
    }

    /// Resolve a message to the account/mailbox/UID an IMAP mutation needs.
    async fn resolve_target(&self, message_id: i64) -> Result<MutationTarget, Error> {
        let found = self
            .db
            .read(move |conn| {
                let Some(message) = repo::get_message(conn, message_id)? else {
                    return Ok(None);
                };
                let Some(mailbox) = repo::get_mailbox(conn, message.mailbox_id)? else {
                    return Ok(None);
                };
                Ok(Some(MutationTarget {
                    account_id: message.account_id,
                    mailbox_id: message.mailbox_id,
                    mailbox_name: mailbox.name,
                    uidvalidity: message.uidvalidity,
                    uid: message.uid,
                }))
            })
            .await?;
        found.ok_or_else(|| Error::not_found(format!("message {message_id}")))
    }

    /// Resolve a mailbox id to its row.
    async fn resolve_mailbox(&self, mailbox_id: i64) -> Result<repo::Mailbox, Error> {
        let found = self
            .db
            .read(move |conn| repo::get_mailbox(conn, mailbox_id))
            .await?;
        found.ok_or_else(|| Error::not_found(format!("mailbox {mailbox_id}")))
    }
}

/// `requested <= 0` becomes [`DEFAULT_LIST_LIMIT`]; anything larger than
/// [`MAX_LIST_LIMIT`] is clamped down to it.
fn normalize_limit(requested: i64) -> i64 {
    if requested <= 0 {
        DEFAULT_LIST_LIMIT
    } else {
        requested.min(MAX_LIST_LIMIT)
    }
}

/// Whether `flag` is safe to interpolate into a hand-built `STORE` command
/// line.
///
/// This is not the full RFC 3501 `flag`/`atom` grammar — it is a boundary
/// check against command injection: [`crate::imap::mutate`] builds its
/// `FLAGS (...)` argument by joining these strings with spaces and dropping
/// the result straight into an IMAP command line, so a flag containing a
/// space, a parenthesis, or a control character could smuggle extra IMAP
/// syntax rather than naming a flag. Restricting flags to an optional leading
/// backslash followed by ASCII alphanumerics/`-`/`_`/`.` closes that off while
/// still admitting every predefined flag (`\Seen`, `\Flagged`, …) and any
/// reasonable keyword.
fn is_safe_flag(flag: &str) -> bool {
    let body = flag.strip_prefix('\\').unwrap_or(flag);
    !body.is_empty()
        && body
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// Pull one attachment's bytes out of a raw message by its positional part
/// id. `None` if the raw does not parse, or no attachment has that part id.
fn decode_attachment(raw: &[u8], part_id: &str) -> Option<AttachmentBytes> {
    use mail_parser::{MessageParser, MimeHeaders};

    let message = MessageParser::default().parse(raw)?;
    message.attachments().enumerate().find_map(|(index, part)| {
        if index.to_string() != part_id {
            return None;
        }
        Some(AttachmentBytes {
            filename: part.attachment_name().map(str::to_owned),
            content_type: part
                .content_type()
                .map(crate::message::parse::format_content_type),
            bytes: part.contents().to_vec(),
        })
    })
}
