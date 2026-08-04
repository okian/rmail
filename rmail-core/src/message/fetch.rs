//! IMAP message fetching and idempotent persistence.

use async_imap::types::Flag;
use async_imap::Session;
use futures::StreamExt;
use tracing::Instrument;

use super::parse::{parse_message, ParsedAttachment};
use crate::error::Error;
use crate::imap::conn::ImapStream;
use crate::imap::map_imap_err;
use crate::repo;
use crate::storage::Database;
use crate::thread;

/// The IMAP attributes fetched per message.
const FETCH_QUERY: &str = "(UID FLAGS INTERNALDATE RFC822.SIZE BODY[])";

/// A message pulled from IMAP, ready to parse and persist.
#[derive(Debug, Clone)]
pub struct FetchedMessage {
    /// IMAP UID.
    pub uid: i64,
    /// UIDVALIDITY the UID belongs to.
    pub uidvalidity: i64,
    /// INTERNALDATE as a unix timestamp (seconds).
    pub internaldate: Option<i64>,
    /// RFC822 size in bytes.
    pub size: Option<i64>,
    /// IMAP flags (e.g. `\Seen`).
    pub flags: Vec<String>,
    /// Raw RFC822 bytes.
    pub raw: Vec<u8>,
}

/// The outcome of persisting one fetched message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PersistOutcome {
    /// The stable message id.
    pub message_id: i64,
    /// Whether a new row was inserted (`false` = already present, a no-op).
    pub inserted: bool,
    /// The thread the message belongs to.
    pub thread_id: Option<i64>,
    /// Threads that were absorbed into [`Self::thread_id`] because this message
    /// linked two conversations. These ids no longer exist, so anything keyed
    /// on a thread id (indexes, caches, watchers) must follow them across.
    pub merged_threads: Vec<i64>,
}

/// Parse and persist a fetched message, idempotently by IMAP identity.
///
/// If a message with the same `(mailbox, uidvalidity, uid)` already exists, this
/// is a no-op (returns `inserted = false`). Otherwise it inserts the message,
/// its attachment metadata, and its flags in one transaction.
///
/// # Errors
///
/// A mapped storage error.
#[tracing::instrument(skip(db, fetched), fields(uid = fetched.uid, uidvalidity = fetched.uidvalidity))]
pub async fn persist_fetched(
    db: &Database,
    account_id: i64,
    mailbox_id: i64,
    fetched: FetchedMessage,
) -> Result<PersistOutcome, Error> {
    let FetchedMessage {
        uid,
        uidvalidity,
        internaldate,
        size,
        flags,
        raw,
    } = fetched;

    // Parse off both the async runtime AND the writer mutex: mail-parser +
    // html2text are CPU-bound, and parsing under the single writer lock would
    // serialize every other write behind it.
    let (parsed, raw) = tokio::task::spawn_blocking(move || {
        let parsed = parse_message(&raw);
        (parsed, raw)
    })
    .await
    .map_err(|e| Error::internal(format!("message parse task failed: {e}")))?;

    let outcome = db
        .write(move |conn| {
            let tx = conn.transaction()?;

            if let Some(existing) =
                repo::get_message_by_identity(&tx, mailbox_id, uidvalidity, uid)?
            {
                // Re-fetch of an already-stored message: no-op. Note flag/state
                // reconciliation is the sync engine's job (task 12), not here.
                // The exception is a message stored before threading existed
                // (or left unthreaded by an interrupted sync) — thread it now
                // rather than leaving it out of every conversation forever.
                let (thread_id, merged_threads) = match existing.thread_id {
                    Some(thread_id) => (Some(thread_id), Vec::new()),
                    None => match thread::assign_thread(&tx, existing.id)? {
                        Some(assignment) => (Some(assignment.thread_id), assignment.merged),
                        None => (None, Vec::new()),
                    },
                };
                tx.commit()?;
                return Ok(PersistOutcome {
                    message_id: existing.id,
                    inserted: false,
                    thread_id,
                    merged_threads,
                });
            }

            let new = repo::NewMessage {
                account_id,
                mailbox_id,
                uid,
                uidvalidity,
                message_id: parsed.message_id,
                thread_id: None,
                in_reply_to: parsed.in_reply_to,
                references_hdr: parsed.references,
                subject: parsed.subject,
                from_addr: parsed.from_addr,
                from_name: parsed.from_name,
                to_addrs: parsed.to_addrs,
                cc_addrs: parsed.cc_addrs,
                date: parsed.date,
                internaldate,
                size,
                raw: Some(raw),
                body_text: parsed.body_text,
                body_html: parsed.body_html,
                has_attachments: !parsed.attachments.is_empty(),
            };
            let message_id = repo::insert_message(&tx, &new)?;

            for attachment in &parsed.attachments {
                repo::insert_attachment(&tx, &to_new_attachment(message_id, attachment))?;
            }
            for flag in &flags {
                repo::add_flag(&tx, message_id, flag)?;
            }

            // Thread in the same transaction: a message is never visible
            // without a conversation.
            let assignment = thread::assign_thread(&tx, message_id)?;
            let thread_id = assignment.as_ref().map(|a| a.thread_id);
            let merged_threads = assignment.map(|a| a.merged).unwrap_or_default();

            tx.commit()?;
            Ok(PersistOutcome {
                message_id,
                inserted: true,
                thread_id,
                merged_threads,
            })
        })
        .await?;
    Ok(outcome)
}

/// Convert a `Fetch` item into a [`FetchedMessage`], or `None` if it lacks a
/// UID or body. Prefers the server-declared `RFC822.SIZE`, falling back to the
/// fetched body length.
fn fetched_from(fetch: &async_imap::types::Fetch, uidvalidity: i64) -> Option<FetchedMessage> {
    let uid = fetch.uid?;
    let body = fetch.body()?;
    let flags: Vec<String> = fetch.flags().map(|f| flag_to_string(&f)).collect();
    let internaldate = fetch.internal_date().map(|d| d.timestamp());
    let size = fetch
        .size
        .map(i64::from)
        .or_else(|| i64::try_from(body.len()).ok());
    Some(FetchedMessage {
        uid: i64::from(uid),
        uidvalidity,
        internaldate,
        size,
        flags,
        raw: body.to_vec(),
    })
}

/// Fetch messages by UID set from the current (already-selected) mailbox,
/// collecting them into a `Vec`.
///
/// Note: this buffers every message's raw bytes in memory; prefer
/// [`fetch_and_persist`] for large sets, which streams one message at a time.
///
/// # Errors
///
/// A mapped IMAP error.
pub async fn fetch_uids<T: ImapStream>(
    session: &mut Session<T>,
    uidvalidity: i64,
    uid_set: &str,
) -> Result<Vec<FetchedMessage>, Error> {
    let mut stream = session
        .uid_fetch(uid_set, FETCH_QUERY)
        .await
        .map_err(map_imap_err)?;

    let mut messages = Vec::new();
    while let Some(item) = stream.next().await {
        let fetch = item.map_err(map_imap_err)?;
        match fetched_from(&fetch, uidvalidity) {
            Some(message) => messages.push(message),
            None => tracing::warn!("skipping FETCH item without a UID or body"),
        }
    }
    Ok(messages)
}

/// How many fetched messages may sit between the socket and the database.
///
/// This is the concurrency bound of the fetch pipeline: without it, persisting
/// message N (parse + a write transaction) happens inline while the server's
/// FETCH response stream sits idle, so a window of 200 messages serializes 200
/// write transactions against a stalled socket. With it, downloading and
/// persisting overlap and at most this many raw messages are ever in memory.
const PIPELINE_DEPTH: usize = 8;

/// Fetch a UID set and persist each message idempotently.
///
/// Downloading and persisting run concurrently across a bounded channel: the
/// FETCH response is drained as fast as the socket delivers it while a separate
/// task parses and writes, with never more than [`PIPELINE_DEPTH`] messages in
/// flight. Nothing materializes the whole mailbox.
///
/// Callers driving a large or long-running fetch (the sync engine) should bound
/// this with a deadline appropriate to the window size. Note a fetch abandoned
/// mid-stream (dropped future or elapsed timeout) leaves the IMAP session with
/// an unfinished command: the session must be dropped, not reused.
///
/// # Errors
///
/// A mapped IMAP or storage error.
#[tracing::instrument(skip(session, db), fields(uidvalidity))]
pub async fn fetch_and_persist<T: ImapStream>(
    session: &mut Session<T>,
    db: &Database,
    account_id: i64,
    mailbox_id: i64,
    uidvalidity: i64,
    uid_set: &str,
) -> Result<Vec<PersistOutcome>, Error> {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<FetchedMessage>(PIPELINE_DEPTH);
    let persist_db = db.clone();
    let persister = tokio::spawn(
        async move {
            let mut outcomes = Vec::new();
            while let Some(message) = rx.recv().await {
                outcomes.push(persist_fetched(&persist_db, account_id, mailbox_id, message).await?);
            }
            Ok::<_, Error>(outcomes)
        }
        .instrument(tracing::Span::current()),
    );

    let mut stream = session
        .uid_fetch(uid_set, FETCH_QUERY)
        .await
        .map_err(map_imap_err)?;

    while let Some(item) = stream.next().await {
        let fetch = item.map_err(map_imap_err)?;
        let Some(message) = fetched_from(&fetch, uidvalidity) else {
            tracing::warn!("skipping FETCH item without a UID or body");
            continue;
        };
        drop(fetch); // release the borrow of the stream buffer before handing off
        if tx.send(message).await.is_err() {
            // The persister stopped early; its error is the real one, so stop
            // reading and let the join below report it.
            break;
        }
    }
    drop(stream);
    drop(tx);

    let outcomes = persister
        .await
        .map_err(|e| Error::internal(format!("message persist task failed: {e}")))??;
    tracing::debug!(persisted = outcomes.len(), "fetch_and_persist complete");
    Ok(outcomes)
}

fn to_new_attachment(message_id: i64, attachment: &ParsedAttachment) -> repo::NewAttachment {
    repo::NewAttachment {
        message_id,
        part_id: attachment.part_id.clone(),
        filename: attachment.filename.clone(),
        content_type: attachment.content_type.clone(),
        size: attachment.size,
        content_id: attachment.content_id.clone(),
        is_inline: attachment.is_inline,
    }
}

/// Render an IMAP flag to its wire string (`\Seen`, or the custom keyword).
fn flag_to_string(flag: &Flag<'_>) -> String {
    match flag {
        Flag::Seen => "\\Seen".to_owned(),
        Flag::Answered => "\\Answered".to_owned(),
        Flag::Flagged => "\\Flagged".to_owned(),
        Flag::Deleted => "\\Deleted".to_owned(),
        Flag::Draft => "\\Draft".to_owned(),
        Flag::Recent => "\\Recent".to_owned(),
        Flag::MayCreate => "\\*".to_owned(),
        Flag::Custom(name) => name.clone().into_owned(),
    }
}

#[cfg(test)]
mod tests;
