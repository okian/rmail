//! Typed row structs and basic repository accessors over the core schema
//! (task 6). Functions take a `&rusqlite::Connection` so they compose with
//! [`crate::Database::with_read`]/[`crate::Database::with_write`] (and their
//! async variants); they return `rusqlite::Result` for the caller to map into
//! the domain error model.
//!
//! Coverage here is deliberately basic — insert/get/list plus the upserts the
//! contact graph and sync checkpoints need. Richer queries land with the tasks
//! that consume them (sync, threading, search).

use rusqlite::{named_params, Connection, OptionalExtension, Row};

// ---------------------------------------------------------------------------
// Accounts
// ---------------------------------------------------------------------------

/// Fields required to create an account row (id/timestamps are DB-assigned).
#[derive(Debug, Clone, Default)]
pub struct NewAccount {
    /// Unique account name.
    pub name: String,
    /// IMAP server hostname.
    pub imap_server: Option<String>,
    /// IMAP port.
    pub imap_port: Option<u16>,
    /// Login username.
    pub username: Option<String>,
    /// SMTP server hostname.
    pub smtp_server: Option<String>,
    /// SMTP port.
    pub smtp_port: Option<u16>,
    /// Credential kind (`none`|`command`|`env`|`keychain`); defaults to `none`.
    pub secret_kind: Option<String>,
    /// Credential reference (command / env var name / keychain service).
    pub secret_ref: Option<String>,
}

/// A persisted account. Never carries the plaintext password — only the
/// `secret_kind`/`secret_ref` describing how to resolve it.
#[derive(Debug, Clone)]
pub struct Account {
    /// Stable primary key.
    pub id: i64,
    /// Unique account name.
    pub name: String,
    /// IMAP server hostname.
    pub imap_server: Option<String>,
    /// IMAP port.
    pub imap_port: Option<u16>,
    /// Login username.
    pub username: Option<String>,
    /// SMTP server hostname.
    pub smtp_server: Option<String>,
    /// SMTP port.
    pub smtp_port: Option<u16>,
    /// Credential kind (`none`|`command`|`env`|`keychain`).
    pub secret_kind: String,
    /// Credential reference (command / env var name / keychain service).
    pub secret_ref: Option<String>,
    /// Creation time (unix seconds).
    pub created_at: i64,
    /// Last-update time (unix seconds).
    pub updated_at: i64,
}

impl Account {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            name: row.get("name")?,
            imap_server: row.get("imap_server")?,
            imap_port: row.get("imap_port")?,
            username: row.get("username")?,
            smtp_server: row.get("smtp_server")?,
            smtp_port: row.get("smtp_port")?,
            secret_kind: row.get("secret_kind")?,
            secret_ref: row.get("secret_ref")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
        })
    }
}

const ACCOUNT_COLS: &str = "id, name, imap_server, imap_port, username, smtp_server, smtp_port, \
     secret_kind, secret_ref, created_at, updated_at";

/// Insert an account, returning its new id.
///
/// # Errors
/// Propagates any `rusqlite` error (e.g. a duplicate `name`).
pub fn insert_account(conn: &Connection, new: &NewAccount) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO accounts
             (name, imap_server, imap_port, username, smtp_server, smtp_port, secret_kind, secret_ref)
         VALUES
             (:name, :imap_server, :imap_port, :username, :smtp_server, :smtp_port,
              COALESCE(:secret_kind, 'none'), :secret_ref)",
        named_params! {
            ":name": new.name,
            ":imap_server": new.imap_server,
            ":imap_port": new.imap_port,
            ":username": new.username,
            ":smtp_server": new.smtp_server,
            ":smtp_port": new.smtp_port,
            ":secret_kind": new.secret_kind,
            ":secret_ref": new.secret_ref,
        },
    )?;
    Ok(conn.last_insert_rowid())
}

/// Delete an account by id. Returns whether a row was removed.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub fn delete_account(conn: &Connection, id: i64) -> rusqlite::Result<bool> {
    let affected = conn.execute("DELETE FROM accounts WHERE id = ?1", [id])?;
    Ok(affected > 0)
}

/// Fetch an account by id.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub fn get_account(conn: &Connection, id: i64) -> rusqlite::Result<Option<Account>> {
    conn.query_row(
        &format!("SELECT {ACCOUNT_COLS} FROM accounts WHERE id = ?1"),
        [id],
        Account::from_row,
    )
    .optional()
}

/// Fetch an account by its unique name.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub fn get_account_by_name(conn: &Connection, name: &str) -> rusqlite::Result<Option<Account>> {
    conn.query_row(
        &format!("SELECT {ACCOUNT_COLS} FROM accounts WHERE name = ?1"),
        [name],
        Account::from_row,
    )
    .optional()
}

/// List all accounts, ordered by name.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub fn list_accounts(conn: &Connection) -> rusqlite::Result<Vec<Account>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {ACCOUNT_COLS} FROM accounts ORDER BY name"
    ))?;
    let rows = stmt.query_map([], Account::from_row)?;
    rows.collect()
}

// ---------------------------------------------------------------------------
// Mailboxes
// ---------------------------------------------------------------------------

/// Fields required to create a mailbox row.
#[derive(Debug, Clone, Default)]
pub struct NewMailbox {
    /// Owning account id.
    pub account_id: i64,
    /// IMAP folder name (full path).
    pub name: String,
    /// Last-seen UIDVALIDITY.
    pub uidvalidity: Option<i64>,
    /// Last-seen UIDNEXT.
    pub uidnext: Option<i64>,
    /// Last-seen HIGHESTMODSEQ (CONDSTORE).
    pub highestmodseq: Option<i64>,
    /// Server folder attributes (raw).
    pub attributes: Option<String>,
}

/// A persisted mailbox/folder.
#[derive(Debug, Clone)]
pub struct Mailbox {
    /// Stable primary key.
    pub id: i64,
    /// Owning account id.
    pub account_id: i64,
    /// IMAP folder name.
    pub name: String,
    /// Last-seen UIDVALIDITY.
    pub uidvalidity: Option<i64>,
    /// Last-seen UIDNEXT.
    pub uidnext: Option<i64>,
    /// Last-seen HIGHESTMODSEQ.
    pub highestmodseq: Option<i64>,
    /// Server folder attributes.
    pub attributes: Option<String>,
    /// Creation time (unix seconds).
    pub created_at: i64,
    /// Last-update time (unix seconds).
    pub updated_at: i64,
}

impl Mailbox {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            account_id: row.get("account_id")?,
            name: row.get("name")?,
            uidvalidity: row.get("uidvalidity")?,
            uidnext: row.get("uidnext")?,
            highestmodseq: row.get("highestmodseq")?,
            attributes: row.get("attributes")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
        })
    }
}

const MAILBOX_COLS: &str = "id, account_id, name, uidvalidity, uidnext, highestmodseq, \
     attributes, created_at, updated_at";

/// Insert a mailbox, returning its new id.
///
/// # Errors
/// Propagates any `rusqlite` error (e.g. a duplicate `(account_id, name)`).
pub fn insert_mailbox(conn: &Connection, new: &NewMailbox) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO mailboxes (account_id, name, uidvalidity, uidnext, highestmodseq, attributes)
         VALUES (:account_id, :name, :uidvalidity, :uidnext, :highestmodseq, :attributes)",
        named_params! {
            ":account_id": new.account_id,
            ":name": new.name,
            ":uidvalidity": new.uidvalidity,
            ":uidnext": new.uidnext,
            ":highestmodseq": new.highestmodseq,
            ":attributes": new.attributes,
        },
    )?;
    Ok(conn.last_insert_rowid())
}

/// Insert a mailbox by `(account_id, name)`, or update its attributes if it
/// already exists (folder discovery). Returns the mailbox id.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub fn upsert_mailbox(
    conn: &Connection,
    account_id: i64,
    name: &str,
    attributes: Option<&str>,
) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO mailboxes (account_id, name, attributes)
         VALUES (:account_id, :name, :attributes)
         ON CONFLICT(account_id, name) DO UPDATE SET
             attributes = excluded.attributes,
             updated_at = unixepoch()",
        named_params! {
            ":account_id": account_id,
            ":name": name,
            ":attributes": attributes,
        },
    )?;
    conn.query_row(
        "SELECT id FROM mailboxes WHERE account_id = ?1 AND name = ?2",
        rusqlite::params![account_id, name],
        |row| row.get(0),
    )
}

/// Fetch a mailbox by id.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub fn get_mailbox(conn: &Connection, id: i64) -> rusqlite::Result<Option<Mailbox>> {
    conn.query_row(
        &format!("SELECT {MAILBOX_COLS} FROM mailboxes WHERE id = ?1"),
        [id],
        Mailbox::from_row,
    )
    .optional()
}

/// List an account's mailboxes, ordered by name.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub fn list_mailboxes(conn: &Connection, account_id: i64) -> rusqlite::Result<Vec<Mailbox>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {MAILBOX_COLS} FROM mailboxes WHERE account_id = ?1 ORDER BY name"
    ))?;
    let rows = stmt.query_map([account_id], Mailbox::from_row)?;
    rows.collect()
}

// ---------------------------------------------------------------------------
// Threads
// ---------------------------------------------------------------------------

/// Fields required to create a thread row.
#[derive(Debug, Clone, Default)]
pub struct NewThread {
    /// Owning account id (threads are per-account).
    pub account_id: i64,
    /// Normalized subject (fallback grouping).
    pub subject_norm: Option<String>,
    /// Soft reference to the root message id.
    pub root_message_id: Option<i64>,
    /// Timestamp of the earliest message (unix seconds).
    pub first_message_at: Option<i64>,
    /// Timestamp of the most recent message (unix seconds).
    pub last_message_at: Option<i64>,
}

/// A persisted thread.
#[derive(Debug, Clone)]
pub struct Thread {
    /// Stable primary key.
    pub id: i64,
    /// Owning account id.
    pub account_id: i64,
    /// Normalized subject.
    pub subject_norm: Option<String>,
    /// Soft reference to the root message id.
    pub root_message_id: Option<i64>,
    /// Timestamp of the earliest message — the anchor for the subject-fallback
    /// window (see [`crate::thread`]).
    pub first_message_at: Option<i64>,
    /// Timestamp of the most recent message.
    pub last_message_at: Option<i64>,
    /// Number of messages in the thread.
    pub message_count: i64,
    /// Distinct participant addresses, lowercased, sorted, comma-joined.
    /// Derived by [`crate::thread::recompute_thread`].
    pub participants: Option<String>,
    /// Creation time (unix seconds).
    pub created_at: i64,
    /// Last-update time (unix seconds).
    pub updated_at: i64,
}

impl Thread {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            account_id: row.get("account_id")?,
            subject_norm: row.get("subject_norm")?,
            root_message_id: row.get("root_message_id")?,
            first_message_at: row.get("first_message_at")?,
            last_message_at: row.get("last_message_at")?,
            message_count: row.get("message_count")?,
            participants: row.get("participants")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
        })
    }

    /// The participant set as individual addresses.
    #[must_use]
    pub fn participant_list(&self) -> Vec<&str> {
        self.participants
            .as_deref()
            .unwrap_or_default()
            .split(',')
            .filter(|a| !a.is_empty())
            .collect()
    }
}

const THREAD_COLS: &str = "id, account_id, subject_norm, root_message_id, first_message_at, \
     last_message_at, message_count, participants, created_at, updated_at";

/// Insert a thread, returning its new id.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub fn insert_thread(conn: &Connection, new: &NewThread) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO threads
             (account_id, subject_norm, root_message_id, first_message_at, last_message_at)
         VALUES
             (:account_id, :subject_norm, :root_message_id, :first_message_at, :last_message_at)",
        named_params! {
            ":account_id": new.account_id,
            ":subject_norm": new.subject_norm,
            ":root_message_id": new.root_message_id,
            ":first_message_at": new.first_message_at,
            ":last_message_at": new.last_message_at,
        },
    )?;
    Ok(conn.last_insert_rowid())
}

/// Fetch a thread by id.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub fn get_thread(conn: &Connection, id: i64) -> rusqlite::Result<Option<Thread>> {
    conn.query_row(
        &format!("SELECT {THREAD_COLS} FROM threads WHERE id = ?1"),
        [id],
        Thread::from_row,
    )
    .optional()
}

/// List an account's threads, most-recently-active first (the conversation
/// list view). Backed by `idx_threads_account_activity`, which covers both the
/// account filter and the ordering.
///
/// A negative `limit` is clamped to 0 — in SQLite a negative LIMIT means "no
/// limit", which would turn a bad page size into a full-table read.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub fn list_threads(
    conn: &Connection,
    account_id: i64,
    limit: i64,
) -> rusqlite::Result<Vec<Thread>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {THREAD_COLS} FROM threads WHERE account_id = ?1
         ORDER BY last_message_at DESC LIMIT ?2"
    ))?;
    let rows = stmt.query_map(
        rusqlite::params![account_id, limit.max(0)],
        Thread::from_row,
    )?;
    rows.collect()
}

/// List a thread's message ids, oldest first by `COALESCE(date, internaldate)`
/// — the order a conversation reads in.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub fn list_thread_message_ids(conn: &Connection, thread_id: i64) -> rusqlite::Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT id FROM messages WHERE thread_id = ?1
         ORDER BY COALESCE(date, internaldate) ASC, id ASC",
    )?;
    let rows = stmt.query_map([thread_id], |row| row.get::<_, i64>(0))?;
    rows.collect()
}

// ---------------------------------------------------------------------------
// Contacts
// ---------------------------------------------------------------------------

/// A persisted contact.
#[derive(Debug, Clone)]
pub struct Contact {
    /// Stable primary key.
    pub id: i64,
    /// Normalized email address (unique).
    pub address: String,
    /// Display name (most recent).
    pub name: Option<String>,
    /// Messages exchanged with this contact.
    pub message_count: i64,
    /// Last time this contact was seen (unix seconds).
    pub last_seen: Option<i64>,
    /// Creation time (unix seconds).
    pub created_at: i64,
    /// Last-update time (unix seconds).
    pub updated_at: i64,
}

impl Contact {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            address: row.get("address")?,
            name: row.get("name")?,
            message_count: row.get("message_count")?,
            last_seen: row.get("last_seen")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
        })
    }
}

const CONTACT_COLS: &str = "id, address, name, message_count, last_seen, created_at, updated_at";

/// Record a contact sighting by address: insert it, or bump its message count
/// and refresh its name / `last_seen`. Returns the contact id.
///
/// `message_count` is incremented on **every** call, so callers must invoke
/// this exactly once per message (e.g. only for newly-inserted messages) to
/// keep the count accurate across resumed/re-run syncs.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub fn upsert_contact(
    conn: &Connection,
    address: &str,
    name: Option<&str>,
    seen_at: i64,
) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO contacts (address, name, message_count, last_seen)
         VALUES (:address, :name, 1, :seen_at)
         ON CONFLICT(address) DO UPDATE SET
             message_count = message_count + 1,
             name = COALESCE(excluded.name, contacts.name),
             last_seen = MAX(COALESCE(contacts.last_seen, 0), excluded.last_seen),
             updated_at = unixepoch()",
        named_params! {
            ":address": address,
            ":name": name,
            ":seen_at": seen_at,
        },
    )?;
    conn.query_row(
        "SELECT id FROM contacts WHERE address = ?1",
        [address],
        |row| row.get(0),
    )
}

/// Fetch a contact by address.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub fn get_contact_by_address(
    conn: &Connection,
    address: &str,
) -> rusqlite::Result<Option<Contact>> {
    conn.query_row(
        &format!("SELECT {CONTACT_COLS} FROM contacts WHERE address = ?1"),
        [address],
        Contact::from_row,
    )
    .optional()
}

/// List contacts, most-recently-seen first.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub fn list_contacts(conn: &Connection) -> rusqlite::Result<Vec<Contact>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {CONTACT_COLS} FROM contacts ORDER BY last_seen DESC"
    ))?;
    let rows = stmt.query_map([], Contact::from_row)?;
    rows.collect()
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

/// Fields required to create a message row.
#[derive(Debug, Clone, Default)]
pub struct NewMessage {
    /// Owning account id.
    pub account_id: i64,
    /// Owning mailbox id.
    pub mailbox_id: i64,
    /// IMAP UID within the mailbox + uidvalidity.
    pub uid: i64,
    /// IMAP UIDVALIDITY.
    pub uidvalidity: i64,
    /// RFC822 `Message-ID` header.
    pub message_id: Option<String>,
    /// Thread this message belongs to.
    pub thread_id: Option<i64>,
    /// `In-Reply-To` header.
    pub in_reply_to: Option<String>,
    /// `References` header chain.
    pub references_hdr: Option<String>,
    /// Subject.
    pub subject: Option<String>,
    /// Primary From address.
    pub from_addr: Option<String>,
    /// From display name.
    pub from_name: Option<String>,
    /// To addresses (serialized).
    pub to_addrs: Option<String>,
    /// Cc addresses (serialized).
    pub cc_addrs: Option<String>,
    /// `Date` header (unix seconds).
    pub date: Option<i64>,
    /// IMAP INTERNALDATE (unix seconds).
    pub internaldate: Option<i64>,
    /// RFC822.SIZE.
    pub size: Option<i64>,
    /// Raw RFC822 bytes.
    pub raw: Option<Vec<u8>>,
    /// Extracted plain-text body.
    pub body_text: Option<String>,
    /// HTML body.
    pub body_html: Option<String>,
    /// Whether the message has attachments.
    pub has_attachments: bool,
}

/// A persisted message.
#[derive(Debug, Clone)]
pub struct Message {
    /// Stable internal id.
    pub id: i64,
    /// Owning account id.
    pub account_id: i64,
    /// Owning mailbox id.
    pub mailbox_id: i64,
    /// IMAP UID.
    pub uid: i64,
    /// IMAP UIDVALIDITY.
    pub uidvalidity: i64,
    /// RFC822 `Message-ID` header.
    pub message_id: Option<String>,
    /// Thread id.
    pub thread_id: Option<i64>,
    /// `In-Reply-To` header.
    pub in_reply_to: Option<String>,
    /// `References` header chain.
    pub references_hdr: Option<String>,
    /// Subject.
    pub subject: Option<String>,
    /// Primary From address.
    pub from_addr: Option<String>,
    /// From display name.
    pub from_name: Option<String>,
    /// To addresses (serialized).
    pub to_addrs: Option<String>,
    /// Cc addresses (serialized).
    pub cc_addrs: Option<String>,
    /// `Date` header (unix seconds).
    pub date: Option<i64>,
    /// IMAP INTERNALDATE (unix seconds).
    pub internaldate: Option<i64>,
    /// RFC822.SIZE.
    pub size: Option<i64>,
    /// Raw RFC822 bytes.
    pub raw: Option<Vec<u8>>,
    /// Extracted plain-text body.
    pub body_text: Option<String>,
    /// HTML body.
    pub body_html: Option<String>,
    /// Whether the message has attachments.
    pub has_attachments: bool,
    /// Creation time (unix seconds).
    pub created_at: i64,
    /// Last-update time (unix seconds).
    pub updated_at: i64,
}

impl Message {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            account_id: row.get("account_id")?,
            mailbox_id: row.get("mailbox_id")?,
            uid: row.get("uid")?,
            uidvalidity: row.get("uidvalidity")?,
            message_id: row.get("message_id")?,
            thread_id: row.get("thread_id")?,
            in_reply_to: row.get("in_reply_to")?,
            references_hdr: row.get("references_hdr")?,
            subject: row.get("subject")?,
            from_addr: row.get("from_addr")?,
            from_name: row.get("from_name")?,
            to_addrs: row.get("to_addrs")?,
            cc_addrs: row.get("cc_addrs")?,
            date: row.get("date")?,
            internaldate: row.get("internaldate")?,
            size: row.get("size")?,
            raw: row.get("raw")?,
            body_text: row.get("body_text")?,
            body_html: row.get("body_html")?,
            has_attachments: row.get("has_attachments")?,
            created_at: row.get("created_at")?,
            updated_at: row.get("updated_at")?,
        })
    }
}

const MESSAGE_COLS: &str = "id, account_id, mailbox_id, uid, uidvalidity, message_id, thread_id, \
     in_reply_to, references_hdr, subject, from_addr, from_name, to_addrs, cc_addrs, date, \
     internaldate, size, raw, body_text, body_html, has_attachments, created_at, updated_at";

/// Insert a message, returning its new stable id.
///
/// # Errors
/// Propagates any `rusqlite` error (e.g. a duplicate
/// `(account_id, mailbox_id, uidvalidity, uid)`).
pub fn insert_message(conn: &Connection, new: &NewMessage) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO messages (
             account_id, mailbox_id, uid, uidvalidity, message_id, thread_id, in_reply_to,
             references_hdr, subject, from_addr, from_name, to_addrs, cc_addrs, date,
             internaldate, size, raw, body_text, body_html, has_attachments
         ) VALUES (
             :account_id, :mailbox_id, :uid, :uidvalidity, :message_id, :thread_id, :in_reply_to,
             :references_hdr, :subject, :from_addr, :from_name, :to_addrs, :cc_addrs, :date,
             :internaldate, :size, :raw, :body_text, :body_html, :has_attachments
         )",
        named_params! {
            ":account_id": new.account_id,
            ":mailbox_id": new.mailbox_id,
            ":uid": new.uid,
            ":uidvalidity": new.uidvalidity,
            ":message_id": new.message_id,
            ":thread_id": new.thread_id,
            ":in_reply_to": new.in_reply_to,
            ":references_hdr": new.references_hdr,
            ":subject": new.subject,
            ":from_addr": new.from_addr,
            ":from_name": new.from_name,
            ":to_addrs": new.to_addrs,
            ":cc_addrs": new.cc_addrs,
            ":date": new.date,
            ":internaldate": new.internaldate,
            ":size": new.size,
            ":raw": new.raw,
            ":body_text": new.body_text,
            ":body_html": new.body_html,
            ":has_attachments": new.has_attachments,
        },
    )?;
    Ok(conn.last_insert_rowid())
}

/// Fetch a message by its stable id.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub fn get_message(conn: &Connection, id: i64) -> rusqlite::Result<Option<Message>> {
    conn.query_row(
        &format!("SELECT {MESSAGE_COLS} FROM messages WHERE id = ?1"),
        [id],
        Message::from_row,
    )
    .optional()
}

/// Fetch a message by its IMAP identity `(mailbox, uidvalidity, uid)` — the
/// lookup task 9's idempotent persist keys on.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub fn get_message_by_identity(
    conn: &Connection,
    mailbox_id: i64,
    uidvalidity: i64,
    uid: i64,
) -> rusqlite::Result<Option<Message>> {
    conn.query_row(
        &format!(
            "SELECT {MESSAGE_COLS} FROM messages
             WHERE mailbox_id = ?1 AND uidvalidity = ?2 AND uid = ?3"
        ),
        rusqlite::params![mailbox_id, uidvalidity, uid],
        Message::from_row,
    )
    .optional()
}

/// List a mailbox's messages, newest first by `COALESCE(date, internaldate)`
/// (so mail with a missing/backdated Date header still sorts by arrival), with
/// a limit. The ordering matches `idx_messages_mailbox_date`.
///
/// A negative `limit` is clamped to 0 — in SQLite a negative LIMIT means "no
/// limit", which would turn a bad page size into a full-table read.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub fn list_messages(
    conn: &Connection,
    mailbox_id: i64,
    limit: i64,
) -> rusqlite::Result<Vec<Message>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {MESSAGE_COLS} FROM messages WHERE mailbox_id = ?1
         ORDER BY COALESCE(date, internaldate) DESC LIMIT ?2"
    ))?;
    let rows = stmt.query_map(
        rusqlite::params![mailbox_id, limit.max(0)],
        Message::from_row,
    )?;
    rows.collect()
}

// ---------------------------------------------------------------------------
// Flags
// ---------------------------------------------------------------------------

/// Add a flag to a message (idempotent).
///
/// # Errors
/// Propagates any `rusqlite` error.
pub fn add_flag(conn: &Connection, message_id: i64, flag: &str) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO flags (message_id, flag) VALUES (?1, ?2)
         ON CONFLICT(message_id, flag) DO NOTHING",
        rusqlite::params![message_id, flag],
    )?;
    Ok(())
}

/// Remove a flag from a message.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub fn remove_flag(conn: &Connection, message_id: i64, flag: &str) -> rusqlite::Result<()> {
    conn.execute(
        "DELETE FROM flags WHERE message_id = ?1 AND flag = ?2",
        rusqlite::params![message_id, flag],
    )?;
    Ok(())
}

/// List a message's flags, sorted.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub fn list_flags(conn: &Connection, message_id: i64) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT flag FROM flags WHERE message_id = ?1 ORDER BY flag")?;
    let rows = stmt.query_map([message_id], |row| row.get::<_, String>(0))?;
    rows.collect()
}

// ---------------------------------------------------------------------------
// Attachments
// ---------------------------------------------------------------------------

/// Fields required to create an attachment row.
#[derive(Debug, Clone, Default)]
pub struct NewAttachment {
    /// Owning message id.
    pub message_id: i64,
    /// MIME part id/path.
    pub part_id: Option<String>,
    /// Filename.
    pub filename: Option<String>,
    /// MIME content type.
    pub content_type: Option<String>,
    /// Size in bytes.
    pub size: Option<i64>,
    /// `Content-ID` for inline parts.
    pub content_id: Option<String>,
    /// Whether the part is inline.
    pub is_inline: bool,
}

/// A persisted attachment.
#[derive(Debug, Clone)]
pub struct Attachment {
    /// Stable primary key.
    pub id: i64,
    /// Owning message id.
    pub message_id: i64,
    /// MIME part id/path.
    pub part_id: Option<String>,
    /// Filename.
    pub filename: Option<String>,
    /// MIME content type.
    pub content_type: Option<String>,
    /// Size in bytes.
    pub size: Option<i64>,
    /// `Content-ID` for inline parts.
    pub content_id: Option<String>,
    /// Whether the part is inline.
    pub is_inline: bool,
    /// Creation time (unix seconds).
    pub created_at: i64,
}

impl Attachment {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            message_id: row.get("message_id")?,
            part_id: row.get("part_id")?,
            filename: row.get("filename")?,
            content_type: row.get("content_type")?,
            size: row.get("size")?,
            content_id: row.get("content_id")?,
            is_inline: row.get("is_inline")?,
            created_at: row.get("created_at")?,
        })
    }
}

const ATTACHMENT_COLS: &str =
    "id, message_id, part_id, filename, content_type, size, content_id, is_inline, created_at";

/// Insert an attachment, returning its new id.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub fn insert_attachment(conn: &Connection, new: &NewAttachment) -> rusqlite::Result<i64> {
    conn.execute(
        "INSERT INTO attachments (message_id, part_id, filename, content_type, size, content_id, is_inline)
         VALUES (:message_id, :part_id, :filename, :content_type, :size, :content_id, :is_inline)",
        named_params! {
            ":message_id": new.message_id,
            ":part_id": new.part_id,
            ":filename": new.filename,
            ":content_type": new.content_type,
            ":size": new.size,
            ":content_id": new.content_id,
            ":is_inline": new.is_inline,
        },
    )?;
    Ok(conn.last_insert_rowid())
}

/// List a message's attachments.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub fn list_attachments(conn: &Connection, message_id: i64) -> rusqlite::Result<Vec<Attachment>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {ATTACHMENT_COLS} FROM attachments WHERE message_id = ?1 ORDER BY id"
    ))?;
    let rows = stmt.query_map([message_id], Attachment::from_row)?;
    rows.collect()
}

// ---------------------------------------------------------------------------
// Sync state
// ---------------------------------------------------------------------------

/// Per-folder sync checkpoint.
#[derive(Debug, Clone, Default)]
pub struct SyncState {
    /// Owning mailbox id (primary key).
    pub mailbox_id: i64,
    /// Last-synced UIDVALIDITY.
    pub uidvalidity: Option<i64>,
    /// Last-synced HIGHESTMODSEQ.
    pub highestmodseq: Option<i64>,
    /// Highest UID synced so far (resumable checkpoint).
    pub last_synced_uid: Option<i64>,
    /// Last sync time (unix seconds).
    pub last_sync_at: Option<i64>,
    /// Whether the initial full sync has completed.
    pub full_sync_done: bool,
}

impl SyncState {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            mailbox_id: row.get("mailbox_id")?,
            uidvalidity: row.get("uidvalidity")?,
            highestmodseq: row.get("highestmodseq")?,
            last_synced_uid: row.get("last_synced_uid")?,
            last_sync_at: row.get("last_sync_at")?,
            full_sync_done: row.get("full_sync_done")?,
        })
    }
}

/// Insert or replace a mailbox's sync checkpoint.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub fn upsert_sync_state(conn: &Connection, state: &SyncState) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO sync_state
             (mailbox_id, uidvalidity, highestmodseq, last_synced_uid, last_sync_at, full_sync_done)
         VALUES
             (:mailbox_id, :uidvalidity, :highestmodseq, :last_synced_uid, :last_sync_at, :full_sync_done)
         ON CONFLICT(mailbox_id) DO UPDATE SET
             uidvalidity = excluded.uidvalidity,
             highestmodseq = excluded.highestmodseq,
             last_synced_uid = excluded.last_synced_uid,
             last_sync_at = excluded.last_sync_at,
             full_sync_done = excluded.full_sync_done,
             updated_at = unixepoch()",
        named_params! {
            ":mailbox_id": state.mailbox_id,
            ":uidvalidity": state.uidvalidity,
            ":highestmodseq": state.highestmodseq,
            ":last_synced_uid": state.last_synced_uid,
            ":last_sync_at": state.last_sync_at,
            ":full_sync_done": state.full_sync_done,
        },
    )?;
    Ok(())
}

/// Fetch a mailbox's sync checkpoint.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub fn get_sync_state(conn: &Connection, mailbox_id: i64) -> rusqlite::Result<Option<SyncState>> {
    conn.query_row(
        "SELECT mailbox_id, uidvalidity, highestmodseq, last_synced_uid, last_sync_at, full_sync_done
         FROM sync_state WHERE mailbox_id = ?1",
        [mailbox_id],
        SyncState::from_row,
    )
    .optional()
}

#[cfg(test)]
mod tests;
