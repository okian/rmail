//! Filing a delivered message in the account's IMAP `Sent` folder.
//!
//! # This never fails a send
//!
//! By the time this runs the message has already left the building. An
//! `APPEND` that fails means the user's own copy is missing, which is
//! annoying; turning that into a failed outbox row would make rmail *re-send*
//! a message that was already delivered, which is the one outcome this whole
//! subsystem exists to prevent. So every error here is logged and swallowed by
//! the caller (see [`super::scheduler`]), and the outbox row stays `sent`.
//!
//! # There is no Bcc to strip
//!
//! prd.md words this requirement as "`append_to_sent` strips Bcc from the
//! appended copy", which is true of a design that renders `Bcc` into the
//! message and removes it afterwards. rmail's does not: `compose::mime::build`
//! never emits a `Bcc` header at all, and blind recipients travel only in the
//! SMTP envelope. The frozen `outbox.raw_mime` is therefore already correct
//! for filing, and the right implementation of "strips Bcc" is to append those
//! octets **unmodified** — a post-hoc stripping pass here could only introduce
//! the header it was meant to remove.

use async_imap::Session;

use crate::error::Error;
use crate::imap::conn::{self, ImapStream};
use crate::imap::map_imap_err;
use crate::storage::Database;

/// Folder names checked, in order, when looking for `Sent`.
///
/// Special-use flags (RFC 6154 `\Sent`) would be better, but
/// [`crate::imap::folders::list_folders`] records only selectability today, so
/// this matches on name against the conventions the common server families
/// use. A server whose Sent folder is named something else falls
/// through to "no Sent folder", which logs and files nothing rather than
/// creating a folder the user did not ask for.
const SENT_FOLDER_NAMES: &[&str] = &[
    "sent",
    "sent items",
    "sent mail",
    "sent messages",
    "inbox.sent",
    "[gmail]/sent mail",
];

/// Whether a folder name is one of the conventional `Sent` spellings.
///
/// `pub(crate)` so [`crate::analytics::response_time`] can identify the same
/// folders when deriving which addresses are "you" from the mail you have
/// already sent. A second list there would be a list that drifts: adding a
/// server family's spelling in one place and not the other would leave the
/// outbox filing into a folder the analytics never counts as yours.
#[must_use]
pub(crate) fn looks_like_sent(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    SENT_FOLDER_NAMES.contains(&lower.as_str())
}

/// Files a delivered message in the account's `Sent` folder.
#[async_trait::async_trait]
pub trait SentAppender: Send + Sync + std::fmt::Debug {
    /// Append `raw_mime` verbatim to `account_id`'s `Sent` folder, flagged
    /// `\Seen` (the user wrote it; it is not unread mail).
    ///
    /// # Errors
    ///
    /// [`Error::FailedPrecondition`] if no `Sent` folder is known for the
    /// account; otherwise a mapped IMAP error.
    async fn append_to_sent(&self, account_id: i64, raw_mime: &[u8]) -> Result<(), Error>;
}

/// The real appender: one fresh IMAP connection per append.
///
/// Stateless per call, exactly like [`crate::imap::mutate::LiveImapMutator`],
/// and for the same reason — there is no session to lose track of between
/// calls, so every call is independently retryable.
#[derive(Debug, Clone)]
pub struct ImapSentAppender {
    db: Database,
}

impl ImapSentAppender {
    /// Create an appender that resolves account credentials from `db`.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// The account's `Sent` folder, as the local mirror knows it.
    async fn sent_folder(&self, account_id: i64) -> Result<String, Error> {
        let names: Vec<String> = self
            .db
            .read(move |conn| {
                let mut stmt =
                    conn.prepare("SELECT name FROM mailboxes WHERE account_id = ?1 ORDER BY name")?;
                let rows = stmt
                    .query_map([account_id], |row| row.get(0))?
                    .collect::<rusqlite::Result<Vec<String>>>()?;
                Ok(rows)
            })
            .await?;

        for candidate in SENT_FOLDER_NAMES {
            if let Some(found) = names
                .iter()
                .find(|name| name.to_ascii_lowercase() == *candidate)
            {
                return Ok(found.clone());
            }
        }
        Err(Error::failed_precondition(format!(
            "account {account_id} has no folder that looks like Sent; the message was \
             delivered but not filed"
        )))
    }
}

#[async_trait::async_trait]
impl SentAppender for ImapSentAppender {
    #[tracing::instrument(skip(self, raw_mime), fields(account_id, bytes = raw_mime.len()), err)]
    async fn append_to_sent(&self, account_id: i64, raw_mime: &[u8]) -> Result<(), Error> {
        let folder = self.sent_folder(account_id).await?;
        let (mut session, _caps) = conn::connect_account(&self.db, account_id).await?;
        let result = append_via(&mut session, &folder, raw_mime).await;
        // Best effort: the append already succeeded or failed on its own
        // terms, and a failed logout says nothing about it.
        let _ = session.logout().await;
        result
    }
}

/// Append to `folder` over an open session.
///
/// Split out so the mock-IMAP test can drive it without a live server, the
/// same seam `imap::mutate`'s `*_via` helpers provide.
///
/// # Errors
///
/// A mapped IMAP error.
pub(crate) async fn append_via<T: ImapStream>(
    session: &mut Session<T>,
    folder: &str,
    raw_mime: &[u8],
) -> Result<(), Error> {
    session
        .append(folder, Some("(\\Seen)"), None, raw_mime)
        .await
        .map_err(map_imap_err)
}
