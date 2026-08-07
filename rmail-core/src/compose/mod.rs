//! Compose: local, durable drafts and the RFC 5322 renderer that turns one
//! into the octets an SMTP submission transmits (prd.md, "Compose, Schedule
//! & Send"; task 60).
//!
//! Two halves, deliberately separable:
//!
//! - [`DraftStore`] — CRUD over `drafts` / `draft_recipients` /
//!   `draft_attachments` (migration `V25`). Everything a user edits lives
//!   here, in decoded form: addresses as addr-specs and plain display names,
//!   subject and body as the text that was typed, attachments as their raw
//!   bytes. Nothing in this table is MIME-encoded, because encoding is a
//!   rendering decision that can change (a subject stops needing an
//!   encoded-word the moment its last non-ASCII character is deleted) and a
//!   stored encoded form would have to be re-derived on every edit anyway.
//! - [`mime`] — the renderer. See its own module docs; the short version is
//!   that its output is the submission payload verbatim, not a preview.
//!
//! # What this module does not do
//!
//! **It does not send.** No SMTP, no outbox, no scheduling: that is task 61,
//! which reads [`DraftStore::render`]'s output, persists it as
//! `outbox.raw_mime` together with the `Message-ID` it must be idempotent
//! against, and hands the bytes to `lettre` unchanged. Keeping the boundary
//! exactly there is what makes the rendered form testable in isolation — the
//! round-trip tests in [`mime`] parse the builder's own output back with
//! `mail_parser` and assert the headers, bodies, and attachments survived,
//! which is a far stronger statement about wire correctness than any
//! assertion against a mocked SMTP server would be.
//!
//! # Threading is frozen at reply time
//!
//! When a draft is created with an `in_reply_to_message_id`, the parent's
//! `Message-ID` and `References` are resolved **once**, then stored on the
//! draft row. They are not recomputed at render time, for a concrete reason:
//! `drafts.in_reply_to_message_id` is `ON DELETE SET NULL`, so a parent that
//! is expunged (or lost to a UIDVALIDITY re-key) takes the linkage with it.
//! A reply that silently loses `In-Reply-To`/`References` between being
//! written and being sent detaches from the conversation in every recipient's
//! client, and does so invisibly — the draft still looks like a reply in the
//! composer. Freezing the headers means the worst case is a stale chain, not
//! a missing one.
//!
//! # Bounds
//!
//! Draft content arrives over gRPC and lands in SQLite, so every unbounded
//! dimension is capped here rather than left to whichever layer notices
//! first: [`MAX_ATTACHMENT_BYTES`] (which is derived from the 16 MiB gRPC
//! frame limit — see its docs), [`MAX_FILENAME`], and [`MAX_SUBJECT`].

pub mod address;
pub mod mime;

use rusqlite::{OptionalExtension, Row, Transaction};

use crate::error::Error;
use crate::storage::Database;

pub use address::Mailbox;
pub use mime::Envelope;

/// Total attachment bytes one draft may carry.
///
/// Derived from the message size a gRPC client can actually move, which today
/// is tonic's **default** 4 MiB decode limit, not the 16 MiB
/// `grpc.limits.max_message_bytes` documents: nothing in `rmaild` applies that
/// config value to a server or a client yet, so raising this constant on the
/// strength of it would only move the failure from a clean
/// `RESOURCE_EXHAUSTED` here to an opaque codec `OUT_OF_RANGE` at the
/// transport, where no message explains it.
///
/// 4 MiB has to cover the *response* too: [`DraftStore::render`] returns the
/// whole rendered message in one unary reply, and base64 inflates attachment
/// bytes by 4/3. 2 MiB of attachments renders to ~2.8 MiB, which fits both
/// directions with room for headers; 3 MiB would not.
///
/// Raising it is a real follow-up — it needs `grpc.limits.max_message_bytes`
/// wired into both `Server`/`Channel` builders and a chunked
/// attachment-upload RPC for anything past that — but it belongs with the
/// gRPC-limits work, not here, and a bound that is honest about the transport
/// beats one that is aspirational about it.
pub const MAX_ATTACHMENT_BYTES: usize = 2 * 1024 * 1024;

/// How many attachments one draft may carry.
///
/// [`MAX_ATTACHMENT_BYTES`] counts content only, so without this a caller
/// could add unbounded zero-byte attachments: each is a row, a MIME part, and
/// a few hundred octets of headers that the byte budget never sees.
pub const MAX_ATTACHMENTS: usize = 50;

/// Longest attachment content type accepted, in octets.
///
/// A `Content-Type` value is a single unfoldable token, so unlike a subject
/// or an address list it cannot be wrapped: its length lands directly on one
/// line against RFC 5322's 998-octet limit. 100 octets is well past the
/// longest type IANA has registered (~90) and leaves the rendered line an
/// order of magnitude of headroom.
pub const MAX_CONTENT_TYPE: usize = 100;

/// Longest attachment filename accepted.
///
/// A filename becomes an RFC 2231 percent-encoded parameter, which can triple
/// its length; 200 octets therefore renders to at most ~600, comfortably
/// inside RFC 5322's 998-octet line limit without needing RFC 2231's
/// parameter-continuation machinery.
pub const MAX_FILENAME: usize = 200;

/// Longest subject accepted, in octets.
///
/// Not an RFC limit — RFC 5322 bounds the rendered *line*, which folding
/// already handles — but a bound on how much of a message's header a single
/// field may become. Chosen as 998 so it reads as "one line's worth of
/// subject", which is already far past what any client displays.
pub const MAX_SUBJECT: usize = 998;

/// Default number of drafts [`DraftStore::list`] returns.
pub const DEFAULT_LIST_LIMIT: usize = 50;

/// Hard cap on [`DraftStore::list`]'s page size, matching prd.md's
/// "server caps 500" pagination rule.
pub const MAX_LIST_LIMIT: usize = 500;

/// Which recipient header an address belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RecipientKind {
    /// Primary recipient.
    To,
    /// Carbon copy.
    Cc,
    /// Blind carbon copy — reaches the server as a `RCPT TO` and never as a
    /// header (see [`mime`]'s module docs).
    Bcc,
}

impl RecipientKind {
    /// The stable string stored in `draft_recipients.kind`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::To => "to",
            Self::Cc => "cc",
            Self::Bcc => "bcc",
        }
    }

    /// Parse a stored value, or `None` for anything this build never wrote.
    #[must_use]
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "to" => Some(Self::To),
            "cc" => Some(Self::Cc),
            "bcc" => Some(Self::Bcc),
            _ => None,
        }
    }
}

/// An attachment on a persisted draft.
///
/// `content` is empty in the drafts [`DraftStore::list`] returns — loading
/// every attachment of every listed draft would make a list of fifty drafts
/// a several-hundred-megabyte read. `size` is authoritative in both cases;
/// [`DraftStore::get`] is what returns the bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftAttachment {
    /// Stable id.
    pub id: i64,
    /// Suggested filename, as it will appear in `Content-Disposition`.
    pub filename: String,
    /// `type/subtype`.
    pub content_type: String,
    /// Decoded length in bytes, always populated.
    pub size: i64,
    /// The bytes — empty in a `list` result, populated by `get`.
    pub content: Vec<u8>,
}

/// An attachment being added to a draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAttachment {
    /// Suggested filename.
    pub filename: String,
    /// `type/subtype`; anything unparseable renders as
    /// `application/octet-stream`.
    pub content_type: String,
    /// The decoded bytes.
    pub content: Vec<u8>,
}

/// A persisted draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    /// Stable id.
    pub id: i64,
    /// Owning account.
    pub account_id: i64,
    /// The sending identity.
    pub from: Mailbox,
    /// `To` recipients, in author order.
    pub to: Vec<Mailbox>,
    /// `Cc` recipients, in author order.
    pub cc: Vec<Mailbox>,
    /// `Bcc` recipients, in author order. Never rendered as a header.
    pub bcc: Vec<Mailbox>,
    /// Subject, decoded.
    pub subject: String,
    /// Plain-text body.
    pub body_text: String,
    /// HTML alternative, if any. Its presence is what makes the rendered
    /// message a `multipart/alternative`.
    pub body_html: Option<String>,
    /// Attachments in author order (see [`DraftAttachment`] on `content`).
    pub attachments: Vec<DraftAttachment>,
    /// The local message this replies to, if it still exists.
    pub in_reply_to_message_id: Option<i64>,
    /// The parent's `Message-ID`, frozen at reply time (see the module docs).
    pub in_reply_to: Option<String>,
    /// The `References` chain this reply will carry, frozen at reply time.
    pub references: Vec<String>,
    /// Creation time (unix seconds).
    pub created_at: i64,
    /// Last-edit time (unix seconds).
    pub updated_at: i64,
}

impl Draft {
    /// Every address the SMTP envelope must name in `RCPT TO`: `To`, `Cc`,
    /// **and** `Bcc`.
    ///
    /// This is the only place blind recipients are surfaced — [`mime::build`]
    /// omits them from the message entirely — so the submission path (task 61)
    /// reads them from here rather than re-parsing the rendered headers, which
    /// by design do not mention them.
    #[must_use]
    pub fn envelope_recipients(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for mailbox in self.to.iter().chain(&self.cc).chain(&self.bcc) {
            if !out.iter().any(|seen| seen == mailbox.address()) {
                out.push(mailbox.address().to_owned());
            }
        }
        out
    }
}

/// A new draft.
#[derive(Debug, Clone)]
pub struct NewDraft {
    /// Owning account.
    pub account_id: i64,
    /// Sending identity.
    pub from: Mailbox,
    /// `To` recipients.
    pub to: Vec<Mailbox>,
    /// `Cc` recipients.
    pub cc: Vec<Mailbox>,
    /// `Bcc` recipients.
    pub bcc: Vec<Mailbox>,
    /// Subject.
    pub subject: String,
    /// Plain-text body.
    pub body_text: String,
    /// HTML alternative.
    pub body_html: Option<String>,
    /// Attachments.
    pub attachments: Vec<NewAttachment>,
    /// The local message this replies to. Its threading headers are resolved
    /// once, here, and frozen onto the draft (see the module docs).
    pub in_reply_to_message_id: Option<i64>,
}

/// A partial edit. `None` leaves a field alone.
///
/// A recipient list set to `Some(vec![])` genuinely clears that header —
/// which is why the post-edit draft, not the patch, is what has to still name
/// at least one recipient.
#[derive(Debug, Clone, Default)]
pub struct DraftPatch {
    /// Replace the sending identity.
    pub from: Option<Mailbox>,
    /// Replace the `To` list wholesale.
    pub to: Option<Vec<Mailbox>>,
    /// Replace the `Cc` list wholesale.
    pub cc: Option<Vec<Mailbox>>,
    /// Replace the `Bcc` list wholesale.
    pub bcc: Option<Vec<Mailbox>>,
    /// Replace the subject.
    pub subject: Option<String>,
    /// Replace the plain-text body.
    pub body_text: Option<String>,
    /// Replace the HTML alternative. `Some("")` **removes** it — that is the
    /// only way to say "no HTML" through a patch whose absent fields already
    /// mean "leave alone".
    pub body_html: Option<String>,
    /// Replace the attachment list wholesale.
    pub attachments: Option<Vec<NewAttachment>>,
}

/// A rendered draft: exactly what a submission would transmit, plus the two
/// pieces of out-of-band information the transmission needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rendered {
    /// The complete RFC 5322 message (the SMTP `DATA` payload).
    pub mime: Vec<u8>,
    /// The `Message-ID` in the rendered headers. Task 61 persists this
    /// *before* `DATA` so a crashed send can be recognised on retry instead
    /// of delivering a second copy.
    pub message_id: String,
    /// Every `RCPT TO`, including `Bcc` — see
    /// [`Draft::envelope_recipients`].
    pub envelope_recipients: Vec<String>,
}

/// Draft storage: CRUD plus rendering.
///
/// Cheap to clone; every clone shares one database handle.
#[derive(Debug, Clone)]
pub struct DraftStore {
    db: Database,
}

impl DraftStore {
    /// Open a store over `db`.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Create a draft, resolving and freezing its threading headers if it is
    /// a reply.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] if the draft names no recipient, or
    /// breaches [`MAX_SUBJECT`], [`MAX_FILENAME`], or [`MAX_CONTENT_TYPE`].
    /// [`Error::ResourceExhausted`] if the attachments exceed
    /// [`MAX_ATTACHMENT_BYTES`] or [`MAX_ATTACHMENTS`]. [`Error::NotFound`]
    /// if `account_id` names no account, or `in_reply_to_message_id` names no
    /// message **in that account** (see [`resolve_threading`]). Otherwise a
    /// mapped storage error.
    #[tracing::instrument(skip(self, new), fields(account_id = new.account_id, draft_id))]
    pub async fn create(&self, new: NewDraft) -> Result<Draft, Error> {
        validate_recipients(&new.to, &new.cc, &new.bcc)?;
        let subject = validate_subject(&new.subject)?;
        let attachments = validate_attachments(new.attachments)?;

        let account_id = new.account_id;
        let parent_id = new.in_reply_to_message_id;
        let from = new.from.clone();
        let to = new.to.clone();
        let cc = new.cc.clone();
        let bcc = new.bcc.clone();
        let body_text = new.body_text;
        let body_html = normalize_html(new.body_html);

        let id = self
            .db
            .write(move |conn| {
                let tx = conn.transaction()?;
                // Checked explicitly rather than left to the foreign key, so
                // a bad `account_id` reports as `account N not found` instead
                // of a constraint failure this code would then have to guess
                // the meaning of — `drafts` has two foreign keys, and the
                // other one (`in_reply_to_message_id`) needs a different
                // message. Inside the transaction, so there is no window
                // between the check and the insert.
                let account_exists: Option<i64> = tx
                    .query_row(
                        "SELECT 1 FROM accounts WHERE id = ?1",
                        [account_id],
                        |row| row.get(0),
                    )
                    .optional()?;
                if account_exists.is_none() {
                    return Ok(Err(Error::not_found(format!(
                        "account {account_id} not found"
                    ))));
                }
                let threading = match parent_id {
                    None => Threading::default(),
                    Some(parent_id) => match resolve_threading(&tx, account_id, parent_id)? {
                        Some(threading) => threading,
                        None => {
                            return Ok(Err(Error::not_found(format!(
                                "message {parent_id} not found"
                            ))))
                        }
                    },
                };

                tx.execute(
                    "INSERT INTO drafts (
                         account_id, in_reply_to_message_id, in_reply_to, references_hdr,
                         from_addr, from_name, subject, body_text, body_html
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    rusqlite::params![
                        account_id,
                        parent_id,
                        threading.in_reply_to,
                        join_ids(&threading.references),
                        from.address(),
                        from.display_name(),
                        subject,
                        body_text,
                        body_html,
                    ],
                )?;
                let id = tx.last_insert_rowid();
                write_recipients(&tx, id, &to, &cc, &bcc)?;
                write_attachments(&tx, id, &attachments)?;
                tx.commit()?;
                Ok(Ok(id))
            })
            .await??;

        tracing::Span::current().record("draft_id", id);
        // Read back after the commit rather than assembling the response from
        // the request, so the caller sees exactly what was stored (server
        // defaults, normalized HTML, the frozen threading chain) rather than
        // a hopeful reconstruction of it. A `DeleteDraft` landing in the gap
        // would turn this into `NOT_FOUND` — accurate, if surprising, and not
        // worth holding the writer lock across two operations to avoid.
        self.get(id).await
    }

    /// Fetch a draft, attachment bytes included.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] if no draft has `draft_id`; otherwise a mapped
    /// storage error.
    #[tracing::instrument(skip(self))]
    pub async fn get(&self, draft_id: i64) -> Result<Draft, Error> {
        self.db
            .read(move |conn| load_draft(conn, draft_id, true))
            .await?
            .ok_or_else(|| Error::not_found(format!("draft {draft_id} not found")))
    }

    /// List an account's drafts, most recently edited first, **without**
    /// attachment bytes (see [`DraftAttachment`]).
    ///
    /// `limit` of zero means [`DEFAULT_LIST_LIMIT`]; anything above
    /// [`MAX_LIST_LIMIT`] is clamped to it rather than rejected, matching
    /// prd.md's "server caps 500" rule.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    #[tracing::instrument(skip(self))]
    pub async fn list(&self, account_id: i64, limit: usize) -> Result<Vec<Draft>, Error> {
        let limit = match limit {
            0 => DEFAULT_LIST_LIMIT,
            n => n.min(MAX_LIST_LIMIT),
        };
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);

        Ok(self
            .db
            .read(move |conn| {
                let ids: Vec<i64> = {
                    let mut stmt = conn.prepare(
                        "SELECT id FROM drafts WHERE account_id = ?1
                         ORDER BY updated_at DESC, id DESC LIMIT ?2",
                    )?;
                    let rows = stmt
                        .query_map(rusqlite::params![account_id, limit], |row| row.get(0))?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    rows
                };
                // Three indexed lookups per draft rather than one join with
                // a fan-out to un-pivot afterwards. Bounded by
                // `MAX_LIST_LIMIT`, all on one already-open connection, all
                // covered by `idx_draft_recipients_draft`/
                // `idx_draft_attachments_draft`: a full page is ~1.5k
                // statement executions against a local file, which is
                // sub-millisecond and a good trade for sharing exactly one
                // row-assembly path with `get` — the alternative is a second
                // one that can drift from it.
                let mut drafts = Vec::with_capacity(ids.len());
                for id in ids {
                    if let Some(draft) = load_draft(conn, id, false)? {
                        drafts.push(draft);
                    }
                }
                Ok(drafts)
            })
            .await?)
    }

    /// Apply a partial edit.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] if no draft has `draft_id`.
    /// [`Error::InvalidArgument`] if the edit would leave the draft with no
    /// recipients, or breaches [`MAX_SUBJECT`], [`MAX_FILENAME`], or
    /// [`MAX_CONTENT_TYPE`]. [`Error::ResourceExhausted`] if the new
    /// attachments exceed [`MAX_ATTACHMENT_BYTES`] or [`MAX_ATTACHMENTS`].
    /// Otherwise a mapped storage error.
    #[tracing::instrument(skip(self, patch))]
    pub async fn update(&self, draft_id: i64, patch: DraftPatch) -> Result<Draft, Error> {
        let subject = patch.subject.as_deref().map(validate_subject).transpose()?;
        let attachments = patch.attachments.map(validate_attachments).transpose()?;
        let from = patch.from;
        let (to, cc, bcc) = (patch.to, patch.cc, patch.bcc);
        let body_text = patch.body_text;
        let body_html = patch.body_html;

        self.db
            .write(move |conn| {
                let tx = conn.transaction()?;
                let Some(current) = load_draft(&tx, draft_id, false)? else {
                    return Ok(Err(Error::not_found(format!("draft {draft_id} not found"))));
                };

                // Validated against the *post-edit* draft: a patch that only
                // clears `to` is legal so long as `cc`/`bcc` still carry the
                // message, and a patch that clears all three is not, however
                // few of the three it names.
                let next_to = to.as_ref().unwrap_or(&current.to);
                let next_cc = cc.as_ref().unwrap_or(&current.cc);
                let next_bcc = bcc.as_ref().unwrap_or(&current.bcc);
                if let Err(err) = validate_recipients(next_to, next_cc, next_bcc) {
                    return Ok(Err(err));
                }

                if let Some(from) = &from {
                    tx.execute(
                        "UPDATE drafts SET from_addr = ?1, from_name = ?2 WHERE id = ?3",
                        rusqlite::params![from.address(), from.display_name(), draft_id],
                    )?;
                }
                if let Some(subject) = &subject {
                    tx.execute(
                        "UPDATE drafts SET subject = ?1 WHERE id = ?2",
                        rusqlite::params![subject, draft_id],
                    )?;
                }
                if let Some(body_text) = &body_text {
                    tx.execute(
                        "UPDATE drafts SET body_text = ?1 WHERE id = ?2",
                        rusqlite::params![body_text, draft_id],
                    )?;
                }
                if let Some(body_html) = &body_html {
                    tx.execute(
                        "UPDATE drafts SET body_html = ?1 WHERE id = ?2",
                        rusqlite::params![normalize_html(Some(body_html.clone())), draft_id],
                    )?;
                }
                if to.is_some() || cc.is_some() || bcc.is_some() {
                    tx.execute(
                        "DELETE FROM draft_recipients WHERE draft_id = ?1",
                        [draft_id],
                    )?;
                    write_recipients(&tx, draft_id, next_to, next_cc, next_bcc)?;
                }
                if let Some(attachments) = &attachments {
                    tx.execute(
                        "DELETE FROM draft_attachments WHERE draft_id = ?1",
                        [draft_id],
                    )?;
                    write_attachments(&tx, draft_id, attachments)?;
                }
                // Unconditional, even when the patch turned out to be a no-op:
                // `updated_at` is `ListDrafts`' sort key, and a save the user
                // performed is a save whether or not it changed anything.
                tx.execute(
                    "UPDATE drafts SET updated_at = unixepoch() WHERE id = ?1",
                    [draft_id],
                )?;
                tx.commit()?;
                Ok(Ok(()))
            })
            .await??;

        // Re-read for the reason `create` documents, with the same race.
        self.get(draft_id).await
    }

    /// Delete a draft (cascading to its recipients and attachments).
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] if no draft has `draft_id`; otherwise a mapped
    /// storage error.
    #[tracing::instrument(skip(self))]
    pub async fn delete(&self, draft_id: i64) -> Result<(), Error> {
        let deleted = self
            .db
            .write(move |conn| conn.execute("DELETE FROM drafts WHERE id = ?1", [draft_id]))
            .await?;
        if deleted == 0 {
            return Err(Error::not_found(format!("draft {draft_id} not found")));
        }
        Ok(())
    }

    /// Render a draft into the exact octets an SMTP submission would send.
    ///
    /// Each call mints a fresh `Message-ID` and stamps the current time, so
    /// two calls never produce identical bytes — the identity of a *sent*
    /// message is minted once, by the send path, not by every preview.
    ///
    /// # Errors
    ///
    /// [`Error::NotFound`] if no draft has `draft_id`;
    /// [`Error::InvalidArgument`] if it names no recipient; otherwise a
    /// mapped storage error.
    #[tracing::instrument(skip(self), fields(message_id))]
    pub async fn render(&self, draft_id: i64) -> Result<Rendered, Error> {
        let draft = self.get(draft_id).await?;
        let envelope = Envelope::now(&draft);
        let envelope_recipients = draft.envelope_recipients();
        tracing::Span::current().record("message_id", envelope.message_id());

        // Off the runtime: rendering is a base64 pass over every attachment,
        // a SHA-256 over the assembled parts, a substring scan for the
        // boundary, and several full copies of the result — megabytes of
        // CPU with no await point in it. Left inline it would stall every
        // other RPC sharing the worker thread.
        let message_id = envelope.message_id().to_owned();
        let span = tracing::Span::current();
        let mime = tokio::task::spawn_blocking(move || {
            let _entered = span.enter();
            mime::build(&draft, &envelope)
        })
        .await
        .map_err(|error| Error::internal(format!("render task failed: {error}")))?
        // Logged here because the boundary cannot: `Error::Internal`'s detail
        // is deliberately replaced with a generic message on its way to a
        // `Status` (see `error::Error`'s own contract, which asks the caller
        // to log first), and for a line-length violation that detail is the
        // only evidence an operator would ever get.
        .inspect_err(|error| {
            if matches!(error.reason(), crate::ErrorReason::Internal) {
                tracing::error!(%error, draft_id, "rendering a draft violated an RFC invariant");
            }
        })?;

        Ok(Rendered {
            mime,
            message_id,
            envelope_recipients,
        })
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

fn validate_recipients(to: &[Mailbox], cc: &[Mailbox], bcc: &[Mailbox]) -> Result<(), Error> {
    if to.is_empty() && cc.is_empty() && bcc.is_empty() {
        return Err(Error::invalid_argument(
            "a draft needs at least one To/Cc/Bcc recipient",
        ));
    }
    Ok(())
}

fn validate_subject(subject: &str) -> Result<String, Error> {
    if subject.len() > MAX_SUBJECT {
        return Err(Error::invalid_argument(format!(
            "subject exceeds {MAX_SUBJECT} octets"
        )));
    }
    // Rejected rather than stripped, for the reason `address::validate_display_name`
    // gives: a CR/LF in a subject is either a caller bug or a header-injection
    // attempt, and quietly repairing it hides both.
    if let Some(bad) = subject.chars().find(|c| c.is_control()) {
        return Err(Error::invalid_argument(format!(
            "subject contains a control character {bad:?}"
        )));
    }
    Ok(subject.to_owned())
}

fn validate_attachments(attachments: Vec<NewAttachment>) -> Result<Vec<NewAttachment>, Error> {
    if attachments.len() > MAX_ATTACHMENTS {
        return Err(Error::resource_exhausted(format!(
            "a draft may carry at most {MAX_ATTACHMENTS} attachments"
        )));
    }
    let mut total = 0usize;
    let mut out = Vec::with_capacity(attachments.len());
    for attachment in attachments {
        total = total.saturating_add(attachment.content.len());
        if total > MAX_ATTACHMENT_BYTES {
            return Err(Error::resource_exhausted(format!(
                "draft attachments exceed {MAX_ATTACHMENT_BYTES} bytes"
            )));
        }
        out.push(NewAttachment {
            filename: validate_filename(&attachment.filename)?,
            content_type: validate_content_type(&attachment.content_type)?,
            content: attachment.content,
        });
    }
    Ok(out)
}

/// A `Content-Type` value is one unfoldable header token, so its length lands
/// straight on a line against RFC 5322's limit — bounded here, at the request
/// boundary, where the caller can be told what was wrong. (The renderer
/// enforces the same bound independently; see
/// `mime::sanitize_content_type`.)
fn validate_content_type(content_type: &str) -> Result<String, Error> {
    if content_type.len() > MAX_CONTENT_TYPE {
        return Err(Error::invalid_argument(format!(
            "attachment content type exceeds {MAX_CONTENT_TYPE} octets"
        )));
    }
    Ok(content_type.to_owned())
}

/// Reduce a filename to its basename and check it is renderable.
///
/// The directory prefix is dropped rather than rejected because a client
/// sensibly passes the path the user picked; what must not survive is the
/// path *itself*, since a receiving client that honours `filename` verbatim
/// would otherwise be handed `../../.ssh/authorized_keys` to save.
fn validate_filename(filename: &str) -> Result<String, Error> {
    let base = filename
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(filename)
        .trim();
    if base.is_empty() || base == "." || base == ".." {
        return Err(Error::invalid_argument(
            "attachment filename must not be empty",
        ));
    }
    if base.len() > MAX_FILENAME {
        return Err(Error::invalid_argument(format!(
            "attachment filename exceeds {MAX_FILENAME} octets"
        )));
    }
    if let Some(bad) = base.chars().find(|c| c.is_control()) {
        return Err(Error::invalid_argument(format!(
            "attachment filename contains a control character {bad:?}"
        )));
    }
    Ok(base.to_owned())
}

/// An HTML alternative that is empty (or whitespace) is no alternative at
/// all — storing `Some("")` would render a `multipart/alternative` whose
/// second branch is blank, which is worse than having no HTML part.
fn normalize_html(html: Option<String>) -> Option<String> {
    html.filter(|h| !h.trim().is_empty())
}

// ---------------------------------------------------------------------------
// SQL
// ---------------------------------------------------------------------------

/// The parent-derived headers a reply freezes onto its draft row.
#[derive(Debug, Default)]
struct Threading {
    in_reply_to: Option<String>,
    references: Vec<String>,
}

/// Read `parent_id`'s threading headers and derive the reply's, or `None` if
/// no such message exists **in `account_id`**.
///
/// The account scoping is not belt-and-braces: without it, a draft on one
/// account could freeze another account's `Message-ID` into its `References`,
/// which both cross-links two mailboxes that were meant to stay separate and
/// leaks the other account's message id to every recipient of this one.
fn resolve_threading(
    tx: &Transaction<'_>,
    account_id: i64,
    parent_id: i64,
) -> rusqlite::Result<Option<Threading>> {
    let parent: Option<(Option<String>, Option<String>, Option<String>)> = tx
        .query_row(
            "SELECT message_id, in_reply_to, references_hdr FROM messages
             WHERE id = ?1 AND account_id = ?2",
            [parent_id, account_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((message_id, in_reply_to, references)) = parent else {
        return Ok(None);
    };
    // `messages.in_reply_to`/`references_hdr` are space-joined bare ids —
    // see `message::parse::join_ids`, which writes them.
    let parent_refs = split_ids(references.as_deref());
    let parent_in_reply_to = split_ids(in_reply_to.as_deref());
    Ok(Some(Threading {
        in_reply_to: message_id.clone().filter(|id| !id.trim().is_empty()),
        references: mime::reply_references(
            &parent_refs,
            &parent_in_reply_to,
            message_id.as_deref(),
        ),
    }))
}

fn split_ids(value: Option<&str>) -> Vec<String> {
    value
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_owned)
        .collect()
}

fn join_ids(ids: &[String]) -> Option<String> {
    if ids.is_empty() {
        None
    } else {
        Some(ids.join(" "))
    }
}

fn write_recipients(
    tx: &Transaction<'_>,
    draft_id: i64,
    to: &[Mailbox],
    cc: &[Mailbox],
    bcc: &[Mailbox],
) -> rusqlite::Result<()> {
    let mut stmt = tx.prepare(
        "INSERT INTO draft_recipients (draft_id, kind, addr, name, position)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    let mut position = 0i64;
    for (kind, list) in [
        (RecipientKind::To, to),
        (RecipientKind::Cc, cc),
        (RecipientKind::Bcc, bcc),
    ] {
        for mailbox in list {
            stmt.execute(rusqlite::params![
                draft_id,
                kind.as_str(),
                mailbox.address(),
                mailbox.display_name(),
                position,
            ])?;
            position += 1;
        }
    }
    Ok(())
}

fn write_attachments(
    tx: &Transaction<'_>,
    draft_id: i64,
    attachments: &[NewAttachment],
) -> rusqlite::Result<()> {
    let mut stmt = tx.prepare(
        "INSERT INTO draft_attachments (draft_id, filename, content_type, content, position)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    for (position, attachment) in attachments.iter().enumerate() {
        stmt.execute(rusqlite::params![
            draft_id,
            attachment.filename,
            attachment.content_type,
            attachment.content,
            i64::try_from(position).unwrap_or(i64::MAX),
        ])?;
    }
    Ok(())
}

/// Load one draft with its children, or `None` if it does not exist.
///
/// `with_content` selects whether attachment bytes come along — see
/// [`DraftAttachment`].
fn load_draft(
    conn: &rusqlite::Connection,
    draft_id: i64,
    with_content: bool,
) -> rusqlite::Result<Option<Draft>> {
    let row: Option<DraftRow> = conn
        .query_row(
            "SELECT id, account_id, in_reply_to_message_id, in_reply_to, references_hdr,
                    from_addr, from_name, subject, body_text, body_html, created_at, updated_at
             FROM drafts WHERE id = ?1",
            [draft_id],
            DraftRow::from_row,
        )
        .optional()?;
    let Some(row) = row else { return Ok(None) };

    let (mut to, mut cc, mut bcc) = (Vec::new(), Vec::new(), Vec::new());
    {
        let mut stmt = conn.prepare(
            "SELECT kind, addr, name FROM draft_recipients
             WHERE draft_id = ?1 ORDER BY position, id",
        )?;
        let rows = stmt.query_map([draft_id], |row| {
            let kind: String = row.get(0)?;
            let addr: String = row.get(1)?;
            let name: Option<String> = row.get(2)?;
            Ok((kind, addr, name))
        })?;
        for entry in rows {
            let (kind, addr, name) = entry?;
            let kind = RecipientKind::parse(&kind)
                .ok_or_else(|| corrupt(draft_id, "draft_recipients.kind", &kind))?;
            // A stored address that no longer parses is corrupt data, not a
            // client mistake: every write path goes through `Mailbox`, so the
            // only way to get one is a raw SQL edit or a schema change.
            let mailbox = Mailbox::new(&addr, name.as_deref())
                .map_err(|e| corrupt(draft_id, "draft_recipients.addr", &e.to_string()))?;
            match kind {
                RecipientKind::To => to.push(mailbox),
                RecipientKind::Cc => cc.push(mailbox),
                RecipientKind::Bcc => bcc.push(mailbox),
            }
        }
    }

    let attachments = {
        let sql = if with_content {
            "SELECT id, filename, content_type, length(content), content FROM draft_attachments
             WHERE draft_id = ?1 ORDER BY position, id"
        } else {
            "SELECT id, filename, content_type, length(content), NULL FROM draft_attachments
             WHERE draft_id = ?1 ORDER BY position, id"
        };
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt
            .query_map([draft_id], |row| {
                Ok(DraftAttachment {
                    id: row.get(0)?,
                    filename: row.get(1)?,
                    content_type: row.get(2)?,
                    size: row.get(3)?,
                    content: row.get::<_, Option<Vec<u8>>>(4)?.unwrap_or_default(),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };

    let from = Mailbox::new(&row.from_addr, row.from_name.as_deref())
        .map_err(|e| corrupt(draft_id, "drafts.from_addr", &e.to_string()))?;

    Ok(Some(Draft {
        id: row.id,
        account_id: row.account_id,
        from,
        to,
        cc,
        bcc,
        subject: row.subject,
        body_text: row.body_text,
        body_html: row.body_html,
        attachments,
        in_reply_to_message_id: row.in_reply_to_message_id,
        in_reply_to: row.in_reply_to,
        references: split_ids(row.references_hdr.as_deref()),
        created_at: row.created_at,
        updated_at: row.updated_at,
    }))
}

/// The `drafts` row, before its children are attached.
struct DraftRow {
    id: i64,
    account_id: i64,
    in_reply_to_message_id: Option<i64>,
    in_reply_to: Option<String>,
    references_hdr: Option<String>,
    from_addr: String,
    from_name: Option<String>,
    subject: String,
    body_text: String,
    body_html: Option<String>,
    created_at: i64,
    updated_at: i64,
}

impl DraftRow {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get(0)?,
            account_id: row.get(1)?,
            in_reply_to_message_id: row.get(2)?,
            in_reply_to: row.get(3)?,
            references_hdr: row.get(4)?,
            from_addr: row.get(5)?,
            from_name: row.get(6)?,
            subject: row.get(7)?,
            body_text: row.get(8)?,
            body_html: row.get(9)?,
            created_at: row.get(10)?,
            updated_at: row.get(11)?,
        })
    }
}

/// A stored row this build cannot interpret is corrupt data, not a bad
/// request — mirrors `crate::notes::corrupt` exactly.
fn corrupt(id: i64, column: &str, value: &str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        0,
        rusqlite::types::Type::Text,
        Box::new(std::io::Error::other(format!(
            "corrupt draft {id}, column {column}: {value}"
        ))),
    )
}

#[cfg(test)]
mod tests;
