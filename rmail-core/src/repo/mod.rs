//! Typed row structs and basic repository accessors over the core schema
//! (task 6). Functions take a `&rusqlite::Connection` so they compose with
//! [`crate::Database::with_read`]/[`crate::Database::with_write`] (and their
//! async variants); they return `rusqlite::Result` for the caller to map into
//! the domain error model.
//!
//! Coverage here is deliberately basic — insert/get/list plus the upserts the
//! contact graph and sync checkpoints need. Richer queries land with the tasks
//! that consume them (sync, threading, search).

use std::collections::{BTreeMap, BTreeSet};

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

/// Repoint an account's credential reference. Returns whether a row changed.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub fn set_account_credential(
    conn: &Connection,
    id: i64,
    secret_kind: &str,
    secret_ref: Option<&str>,
) -> rusqlite::Result<bool> {
    let affected = conn.execute(
        "UPDATE accounts
            SET secret_kind = :secret_kind,
                secret_ref = :secret_ref,
                updated_at = unixepoch()
          WHERE id = :id",
        named_params! {
            ":id": id,
            ":secret_kind": secret_kind,
            ":secret_ref": secret_ref,
        },
    )?;
    Ok(affected > 0)
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

    /// The listing sort key: `Date` if the message carries one, otherwise the
    /// server's `INTERNALDATE`, otherwise `0`.
    ///
    /// The Rust mirror of [`LIST_SORT_KEY`]'s SQL, and it must stay one: a
    /// page token is built from this value and compared against that
    /// expression, so a divergence between the two would make a page boundary
    /// land somewhere the query never looks.
    #[must_use]
    pub fn sort_key(&self) -> i64 {
        self.date.or(self.internaldate).unwrap_or(0)
    }
}

/// The mailbox-listing sort expression, in SQL.
///
/// Three-argument `COALESCE` on purpose: a message with neither a `Date`
/// header nor an `INTERNALDATE` has a NULL key, and NULL compares as neither
/// less nor greater than a cursor — every such message would become
/// unreachable the moment pagination started. Pinning them at `0` puts them
/// exactly where SQLite's "NULLs last under DESC" already had them, so the
/// visible order does not change. `idx_messages_mailbox_page` (V37) indexes
/// this exact expression.
const LIST_SORT_KEY: &str = "COALESCE(date, internaldate, 0)";

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

/// Fetch multiple messages by id, in one round trip — the batched sibling of
/// [`get_message`] a result-set-shaped caller (task 33's `SearchService`,
/// presenting up to `search.default_limit` messages spanning arbitrary
/// mailboxes/threads) needs instead of one query per id. An id with no
/// matching row is simply absent from the result, the same "no row" contract
/// [`get_message`] gives a single missing id.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub fn get_messages(conn: &Connection, ids: &[i64]) -> rusqlite::Result<Vec<Message>> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
    let sql = format!("SELECT {MESSAGE_COLS} FROM messages WHERE id IN ({placeholders})");
    let mut stmt = conn.prepare(&sql)?;
    let params: Vec<&dyn rusqlite::ToSql> =
        ids.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
    let rows = stmt.query_map(params.as_slice(), Message::from_row)?;
    rows.collect()
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

/// The parsed text fields of a message, without its raw RFC822 blob.
///
/// [`get_message`] selects every column, `raw` included. A full-mailbox
/// extraction sweep that used it would read the entire mail corpus off disk to
/// look at six strings.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub fn get_message_text(conn: &Connection, id: i64) -> rusqlite::Result<Option<MessageText>> {
    conn.query_row(
        "SELECT id, subject, from_name, from_addr, to_addrs, cc_addrs, body_text, body_html
         FROM messages WHERE id = ?1",
        [id],
        |row| {
            Ok(MessageText {
                id: row.get(0)?,
                subject: row.get(1)?,
                from_name: row.get(2)?,
                from_addr: row.get(3)?,
                to_addrs: row.get(4)?,
                cc_addrs: row.get(5)?,
                body_text: row.get(6)?,
                body_html: row.get(7)?,
            })
        },
    )
    .optional()
}

/// A message's extracted body text (`index_content` where `part = 'body'`),
/// if it has been indexed.
///
/// The rest of the search pipeline (`fuse::Fuser::fetch_meta`,
/// `present::Presenter::fetch_meta`, `features::extract`) all read this same
/// table+part directly via their own inline SQL rather than a shared
/// accessor — the established convention *inside* `rmail-core`, where every
/// caller already has `rusqlite` on hand. This function exists for the one
/// case that convention does not cover: task 33's `SearchService::Explain`
/// lives in the `rmaild` crate, which has no `rusqlite` dependency of its
/// own and should not gain one just to read one row for a "why did this
/// match" snippet — see [`get_messages`]'s identical reasoning for
/// `SearchHit`'s own message rows.
///
/// `None` when the message has no `index_content` row for that part (not yet
/// indexed) or does not exist at all — the same "no row" contract every
/// other single-id lookup in this module gives.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub fn get_body_text(conn: &Connection, message_id: i64) -> rusqlite::Result<Option<String>> {
    conn.query_row(
        "SELECT text FROM index_content WHERE message_id = ?1 AND part = 'body'",
        [message_id],
        |row| row.get::<_, String>(0),
    )
    .optional()
}

/// A message's indexable text, without the raw blob.
#[derive(Debug, Clone, Default)]
pub struct MessageText {
    /// Stable message id.
    pub id: i64,
    /// Subject.
    pub subject: Option<String>,
    /// From display name.
    pub from_name: Option<String>,
    /// Primary From address.
    pub from_addr: Option<String>,
    /// To addresses (serialized).
    pub to_addrs: Option<String>,
    /// Cc addresses (serialized).
    pub cc_addrs: Option<String>,
    /// Extracted plain-text body.
    pub body_text: Option<String>,
    /// HTML body.
    pub body_html: Option<String>,
}

/// List a mailbox's messages, newest first by [`LIST_SORT_KEY`] (so mail with
/// a missing/backdated Date header still sorts by arrival), starting strictly
/// after `after`.
///
/// `id DESC` is the tiebreak, in the `ORDER BY` **and** in the cursor, because
/// timestamps tie: a bulk import gives a hundred messages one `INTERNALDATE`,
/// and a page boundary landing inside that group would repeat or drop the rest
/// of it depending on how SQLite happened to break the tie that time. Both
/// halves are in `idx_messages_mailbox_page` (V37), so a page is a range scan
/// with no sort.
///
/// The cursor predicate is written as a range (`key <= ?`) plus a filter
/// rather than the equivalent `(key < ?) OR (key = ? AND id < ?)`, so the
/// planner gets a single bound on the index prefix instead of a disjunction it
/// has to turn into two scans.
///
/// A negative `limit` is clamped to 0 — in SQLite a negative LIMIT means "no
/// limit", which would turn a bad page size into a full-table read.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub fn list_messages(
    conn: &Connection,
    mailbox_id: i64,
    after: Option<crate::page::Cursor>,
    limit: i64,
) -> rusqlite::Result<Vec<Message>> {
    // The cursor's two parameters are appended rather than bound to NULL on
    // the first page: `?3 IS NULL OR key <= ?3` would be uniform, but it also
    // stops the planner from turning the comparison into a bound on the
    // index prefix, which is the entire performance argument for keyset
    // pagination.
    let mut params: Vec<i64> = vec![mailbox_id, limit.max(0)];
    let cursor_sql = match after {
        Some(cursor) => {
            params.push(cursor.sort);
            params.push(cursor.id);
            format!("AND {LIST_SORT_KEY} <= ?3 AND ({LIST_SORT_KEY} < ?3 OR id < ?4)")
        }
        None => String::new(),
    };
    let mut stmt = conn.prepare(&format!(
        "SELECT {MESSAGE_COLS} FROM messages WHERE mailbox_id = ?1 {cursor_sql}
         ORDER BY {LIST_SORT_KEY} DESC, id DESC LIMIT ?2"
    ))?;
    let rows = stmt.query_map(rusqlite::params_from_iter(params), Message::from_row)?;
    rows.collect()
}

/// The UIDs already stored for a mailbox at a given UIDVALIDITY within
/// `low..=high`, ascending. A sync uses this to drop UIDs it already has from
/// the window it is about to request; it is deliberately range-scoped so a
/// million-message folder is never materialized at once.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub fn list_message_uids(
    conn: &Connection,
    mailbox_id: i64,
    uidvalidity: i64,
    low: i64,
    high: i64,
) -> rusqlite::Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT uid FROM messages
         WHERE mailbox_id = ?1 AND uidvalidity = ?2 AND uid BETWEEN ?3 AND ?4
         ORDER BY uid",
    )?;
    let rows = stmt.query_map(
        rusqlite::params![mailbox_id, uidvalidity, low, high],
        |row| row.get(0),
    )?;
    rows.collect()
}

/// The `(uid, message id)` pairs stored for a mailbox at a UIDVALIDITY within
/// `low..=high`, ascending by UID.
///
/// The delta sync needs the surrogate id to reconcile flags and to expunge, but
/// not the row — [`get_message_by_identity`] would drag every raw RFC822 blob in
/// the range through memory to answer the same question.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub fn list_message_uid_ids(
    conn: &Connection,
    mailbox_id: i64,
    uidvalidity: i64,
    low: i64,
    high: i64,
) -> rusqlite::Result<Vec<(i64, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT uid, id FROM messages
         WHERE mailbox_id = ?1 AND uidvalidity = ?2 AND uid BETWEEN ?3 AND ?4
         ORDER BY uid",
    )?;
    let rows = stmt.query_map(
        rusqlite::params![mailbox_id, uidvalidity, low, high],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    rows.collect()
}

/// Replace a message's flag set with `flags`, returning whether it changed.
///
/// IMAP flags are a set the server owns outright, so a delta sync replaces
/// rather than merges: a flag the server no longer reports has been cleared.
/// The `false` return lets a caller distinguish a real flag change from a
/// message the server merely re-reported.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub fn replace_flags(
    conn: &Connection,
    message_id: i64,
    flags: &[String],
) -> rusqlite::Result<bool> {
    let current: BTreeSet<String> = list_flags(conn, message_id)?.into_iter().collect();
    let desired: BTreeSet<&str> = flags.iter().map(String::as_str).collect();
    if current
        .iter()
        .map(String::as_str)
        .eq(desired.iter().copied())
    {
        return Ok(false);
    }
    conn.execute("DELETE FROM flags WHERE message_id = ?1", [message_id])?;
    for flag in &desired {
        add_flag(conn, message_id, flag)?;
    }
    conn.execute(
        "UPDATE messages SET updated_at = unixepoch() WHERE id = ?1",
        [message_id],
    )?;
    Ok(true)
}

/// Update a mailbox's server-reported UID state after a `SELECT`, so the
/// `mailboxes` row and the sync checkpoint do not drift apart.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub fn update_mailbox_uid_state(
    conn: &Connection,
    mailbox_id: i64,
    uidvalidity: i64,
    uidnext: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE mailboxes SET uidvalidity = ?2, uidnext = ?3, updated_at = unixepoch()
         WHERE id = ?1",
        rusqlite::params![mailbox_id, uidvalidity, uidnext],
    )?;
    Ok(())
}

/// Record the `HIGHESTMODSEQ` a `SELECT` reported, mirroring what the server
/// said. The authoritative *checkpoint* — how far a delta sync has actually
/// applied — is [`SyncState::highestmodseq`]; this column is the server-state
/// mirror alongside `uidvalidity`/`uidnext`, and a server that reports no
/// modseq clears it rather than leaving a stale value behind.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub fn update_mailbox_highestmodseq(
    conn: &Connection,
    mailbox_id: i64,
    highestmodseq: Option<i64>,
) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE mailboxes SET highestmodseq = ?2, updated_at = unixepoch() WHERE id = ?1",
        rusqlite::params![mailbox_id, highestmodseq],
    )?;
    Ok(())
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

/// Every flag on every message in `ids`, sorted within each message, one
/// round trip — the batched sibling of [`list_flags`] a result-set-shaped
/// caller needs (see [`get_messages`]'s identical reasoning). A message with
/// no flags at all is simply absent from the map; `.get(id)` and `.get(id)`
/// on a message that has flags but wasn't in `ids` both read the same to a
/// caller doing `.cloned().unwrap_or_default()`.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub fn list_flags_by_message(
    conn: &Connection,
    ids: &[i64],
) -> rusqlite::Result<BTreeMap<i64, Vec<String>>> {
    if ids.is_empty() {
        return Ok(BTreeMap::new());
    }
    let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
    let sql = format!(
        "SELECT message_id, flag FROM flags WHERE message_id IN ({placeholders}) \
         ORDER BY message_id, flag"
    );
    let mut stmt = conn.prepare(&sql)?;
    let params: Vec<&dyn rusqlite::ToSql> =
        ids.iter().map(|id| id as &dyn rusqlite::ToSql).collect();
    let rows = stmt.query_map(params.as_slice(), |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut out: BTreeMap<i64, Vec<String>> = BTreeMap::new();
    for row in rows {
        let (id, flag) = row?;
        out.entry(id).or_default().push(flag);
    }
    Ok(out)
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
    /// **High** water mark: the highest UID the walk has covered. Everything
    /// above it is new mail a later run must fetch.
    pub last_synced_uid: Option<i64>,
    /// **Low** water mark: the lowest UID the walk has reached. The backlog
    /// resumes just below it; `None` means the walk has not started. See
    /// [`crate::sync::full`] for why the UID set alone cannot supply this.
    pub walked_down_to: Option<i64>,
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
            walked_down_to: row.get("walked_down_to")?,
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
             (mailbox_id, uidvalidity, highestmodseq, last_synced_uid, walked_down_to,
              last_sync_at, full_sync_done)
         VALUES
             (:mailbox_id, :uidvalidity, :highestmodseq, :last_synced_uid, :walked_down_to,
              :last_sync_at, :full_sync_done)
         ON CONFLICT(mailbox_id) DO UPDATE SET
             uidvalidity = excluded.uidvalidity,
             highestmodseq = excluded.highestmodseq,
             last_synced_uid = excluded.last_synced_uid,
             walked_down_to = excluded.walked_down_to,
             last_sync_at = excluded.last_sync_at,
             full_sync_done = excluded.full_sync_done,
             updated_at = unixepoch()",
        named_params! {
            ":mailbox_id": state.mailbox_id,
            ":uidvalidity": state.uidvalidity,
            ":highestmodseq": state.highestmodseq,
            ":last_synced_uid": state.last_synced_uid,
            ":walked_down_to": state.walked_down_to,
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
        "SELECT mailbox_id, uidvalidity, highestmodseq, last_synced_uid, walked_down_to,
                last_sync_at, full_sync_done
         FROM sync_state WHERE mailbox_id = ?1",
        [mailbox_id],
        SyncState::from_row,
    )
    .optional()
}

#[cfg(test)]
mod tests;
