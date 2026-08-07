-- V25: Compose & drafts (task 60).
--
-- Local, durable drafts (prd.md, "Compose, Schedule & Send"). A draft is the
-- editable source a full RFC 5322 message is *rendered from*; the rendered
-- octets are deliberately not stored here. Task 61's `outbox.raw_mime` is
-- where a message freezes, at schedule time, because that is the moment the
-- bytes stop being editable and start being the thing SMTP will transmit.
-- Storing a rendered copy on the draft too would mean two representations of
-- one message that can silently disagree the instant either is edited.
--
-- Recipients and attachments live in child tables rather than as delimited
-- TEXT columns (the shape `messages.to_addrs` uses for *parsed inbound*
-- mail). Inbound rows are a denormalized mirror of something already
-- serialized elsewhere; a draft is the authoritative record, and a display
-- name may legally contain a comma ("Doe, Jane" <j@x.com>), so a comma-joined
-- column cannot round-trip one without inventing a quoting scheme the
-- database itself cannot enforce.

CREATE TABLE drafts (
    id         INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,

    -- The local message this draft replies to, if any. ON DELETE SET NULL,
    -- not CASCADE: losing the parent (an expunge, a folder resync) must not
    -- silently destroy the user's unsent reply.
    in_reply_to_message_id INTEGER REFERENCES messages(id) ON DELETE SET NULL,

    -- The threading headers, resolved from the parent *once* at the moment
    -- the reply is created and frozen here as bare message-ids (no angle
    -- brackets, matching how `messages.message_id`/`references_hdr` already
    -- store them). Frozen rather than recomputed at render time precisely
    -- because `in_reply_to_message_id` can go NULL underneath us: a reply
    -- whose parent was expunged must still carry correct In-Reply-To /
    -- References, or it detaches from the conversation on every recipient's
    -- client. See `rmail-core::compose`'s module docs.
    in_reply_to    TEXT,
    references_hdr TEXT,

    -- The sending identity. Stored on the draft, not derived from the account
    -- at render time, so a draft written under one identity does not change
    -- who it is from because the account row was later edited.
    from_addr  TEXT NOT NULL,
    from_name  TEXT,

    subject    TEXT NOT NULL DEFAULT '',
    body_text  TEXT NOT NULL DEFAULT '',
    -- NULL means "no HTML alternative", which is what decides whether the
    -- rendered message is a bare text/plain or a multipart/alternative.
    body_html  TEXT,

    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch())
) STRICT;

CREATE TABLE draft_recipients (
    id       INTEGER PRIMARY KEY,
    draft_id INTEGER NOT NULL REFERENCES drafts(id) ON DELETE CASCADE,
    -- 'to' | 'cc' | 'bcc'. A CHECK rather than a bare TEXT column (the
    -- convention `notes.author`/`message_tags.source` follow) because this
    -- vocabulary is closed by the RFC, not by application policy: a fourth
    -- value would not be a new rmail feature, it would be a malformed
    -- message. `rmail-core::compose::RecipientKind` is the Rust half.
    kind     TEXT NOT NULL,
    -- The addr-spec, verbatim and never encoded — RFC 2047 encoded-words are
    -- forbidden inside an addr-spec, and a stored-encoded address could not
    -- be used as an SMTP RCPT TO without a decode step that has no business
    -- existing.
    addr     TEXT NOT NULL,
    -- The display name, decoded. Encoding it into an RFC 2047 word is the
    -- renderer's job, and only when the name is not ASCII.
    name     TEXT,
    -- Author-visible ordering. Recipients are not a set: "to: alice, bob"
    -- and "to: bob, alice" are the same delivery but not the same message,
    -- and a UI that reorders a user's addressees on every save is broken.
    position INTEGER NOT NULL,
    CHECK (kind IN ('to', 'cc', 'bcc'))
) STRICT;

CREATE TABLE draft_attachments (
    id           INTEGER PRIMARY KEY,
    draft_id     INTEGER NOT NULL REFERENCES drafts(id) ON DELETE CASCADE,
    filename     TEXT NOT NULL,
    content_type TEXT NOT NULL,
    -- The decoded bytes. Held in the database rather than as a path into the
    -- filesystem so a draft stays renderable after the file the user picked
    -- has moved or been deleted — the whole point of a durable draft.
    -- `rmail-core::compose::MAX_ATTACHMENT_BYTES` bounds the total per draft;
    -- SQLite's own limit is an order of magnitude higher and would not stop a
    -- caller from writing a gigabyte through the gRPC surface.
    content      BLOB NOT NULL,
    position     INTEGER NOT NULL
) STRICT;

-- `ListDrafts` is always account-scoped and always newest-edited first, so
-- the index carries the sort key rather than leaving it to a temp B-tree.
CREATE INDEX idx_drafts_account ON drafts(account_id, updated_at DESC);
-- Both child tables are only ever read (and deleted) by draft, in `position`
-- order; the composite index backs the filter and the ordering together.
CREATE INDEX idx_draft_recipients_draft ON draft_recipients(draft_id, position);
CREATE INDEX idx_draft_attachments_draft ON draft_attachments(draft_id, position);
