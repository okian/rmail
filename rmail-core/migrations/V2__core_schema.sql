-- V2: baseline core schema.
--
-- The durable local mailbox model. `messages.id` is the stable surrogate key
-- that feature tables and FKs reference; IMAP identity is (mailbox, uidvalidity,
-- uid). A UIDVALIDITY change invalidates the UID space and requires a resync
-- that re-keys rows in place (task 12) — the surrogate id is not automatically
-- preserved across a UIDVALIDITY bump. Raw RFC822 is kept verbatim in
-- `messages.raw` alongside parsed metadata. All timestamps are unix seconds
-- (INTEGER). Tables are STRICT for column-type rigor. Parent tables precede
-- children.

-- Configured accounts (credentials are NOT stored; resolved lazily elsewhere).
CREATE TABLE accounts (
    id          INTEGER PRIMARY KEY,
    name        TEXT NOT NULL UNIQUE,
    imap_server TEXT,
    imap_port   INTEGER,
    username    TEXT,
    smtp_server TEXT,
    smtp_port   INTEGER,
    created_at  INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at  INTEGER NOT NULL DEFAULT (unixepoch())
) STRICT;

-- IMAP folders per account.
CREATE TABLE mailboxes (
    id            INTEGER PRIMARY KEY,
    account_id    INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name          TEXT NOT NULL,
    uidvalidity   INTEGER,
    uidnext       INTEGER,
    highestmodseq INTEGER,
    attributes    TEXT,
    created_at    INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at    INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE (account_id, name)
) STRICT;

-- Derived contact graph (normalized email address -> display name + counts).
CREATE TABLE contacts (
    id            INTEGER PRIMARY KEY,
    address       TEXT NOT NULL UNIQUE,
    name          TEXT,
    message_count INTEGER NOT NULL DEFAULT 0,
    last_seen     INTEGER,
    created_at    INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at    INTEGER NOT NULL DEFAULT (unixepoch())
) STRICT;

-- Conversation threads. `root_message_id` is a soft reference to messages(id)
-- (no FK, to avoid a threads<->messages cycle).
CREATE TABLE threads (
    id              INTEGER PRIMARY KEY,
    account_id      INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    subject_norm    TEXT,
    root_message_id INTEGER,
    last_message_at INTEGER,
    message_count   INTEGER NOT NULL DEFAULT 0,
    created_at      INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at      INTEGER NOT NULL DEFAULT (unixepoch())
) STRICT;

-- Messages: stable id + IMAP identity + raw RFC822 + parsed metadata.
CREATE TABLE messages (
    id              INTEGER PRIMARY KEY,
    account_id      INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    mailbox_id      INTEGER NOT NULL REFERENCES mailboxes(id) ON DELETE CASCADE,
    uid             INTEGER NOT NULL,
    uidvalidity     INTEGER NOT NULL,
    message_id      TEXT,
    thread_id       INTEGER REFERENCES threads(id) ON DELETE SET NULL,
    in_reply_to     TEXT,
    references_hdr  TEXT,
    subject         TEXT,
    from_addr       TEXT,
    from_name       TEXT,
    to_addrs        TEXT,
    cc_addrs        TEXT,
    date            INTEGER,
    internaldate    INTEGER,
    size            INTEGER,
    raw             BLOB,
    body_text       TEXT,
    body_html       TEXT,
    has_attachments INTEGER NOT NULL DEFAULT 0,
    created_at      INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at      INTEGER NOT NULL DEFAULT (unixepoch()),
    -- IMAP identity (also the idempotent upsert key). Leading with mailbox_id
    -- lets the QRESYNC/expunge UID-range scan (WHERE mailbox_id=? AND
    -- uidvalidity=? ORDER BY uid) use this index; account_id is implied by
    -- mailbox_id so it is omitted here.
    UNIQUE (mailbox_id, uidvalidity, uid)
) STRICT;

-- Per-message IMAP flags / keywords (a set).
CREATE TABLE flags (
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    flag       TEXT NOT NULL,
    PRIMARY KEY (message_id, flag)
) STRICT;

-- Attachment metadata (bytes are extracted lazily by later tasks).
CREATE TABLE attachments (
    id           INTEGER PRIMARY KEY,
    message_id   INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    part_id      TEXT,
    filename     TEXT,
    content_type TEXT,
    size         INTEGER,
    content_id   TEXT,
    is_inline    INTEGER NOT NULL DEFAULT 0,
    created_at   INTEGER NOT NULL DEFAULT (unixepoch())
) STRICT;

-- Per-folder sync checkpoints (resumable initial sync + CONDSTORE/QRESYNC).
CREATE TABLE sync_state (
    mailbox_id      INTEGER PRIMARY KEY REFERENCES mailboxes(id) ON DELETE CASCADE,
    uidvalidity     INTEGER,
    highestmodseq   INTEGER,
    last_synced_uid INTEGER,
    last_sync_at    INTEGER,
    full_sync_done  INTEGER NOT NULL DEFAULT 0,
    updated_at      INTEGER NOT NULL DEFAULT (unixepoch())
) STRICT;

-- Hot-path indexes.
-- Mailbox-scoped, date-ordered listing (the primary list view). The sort key is
-- COALESCE(date, internaldate) so mail with a missing/backdated Date header
-- still sorts by arrival; the expression matches list_messages' ORDER BY so the
-- index backs both the filter and the ordering (no temp B-tree sort).
CREATE INDEX idx_messages_mailbox_date
    ON messages(mailbox_id, COALESCE(date, internaldate) DESC);
CREATE INDEX idx_messages_account ON messages(account_id);
CREATE INDEX idx_messages_thread ON messages(thread_id);
CREATE INDEX idx_messages_message_id ON messages(message_id);
CREATE INDEX idx_messages_in_reply_to ON messages(in_reply_to);
CREATE INDEX idx_flags_flag ON flags(flag);
CREATE INDEX idx_attachments_message ON attachments(message_id);
CREATE INDEX idx_threads_last_message ON threads(last_message_at DESC);
CREATE INDEX idx_mailboxes_account ON mailboxes(account_id);
CREATE INDEX idx_contacts_last_seen ON contacts(last_seen DESC);
