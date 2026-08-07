-- V24: tags subsystem (task 55) -- `tags`, `message_tags`, and the
-- effective-tags view search/display read through.
--
-- # A partial unique index, not a table-level UNIQUE, is what actually
-- # enforces idempotent apply
--
-- prd.md's own schema sketch writes `UNIQUE(tag_id, message_id, thread_id)`
-- directly on message_tags, but SQLite treats every NULL as distinct from
-- every other NULL for UNIQUE enforcement
-- (https://www.sqlite.org/lang_createtable.html#unique_constraints) -- two
-- message-level rows for the same tag on the same message (thread_id NULL in
-- both) would NOT collide under that literal constraint, because SQLite
-- never considers two NULLs equal for uniqueness purposes. The CHECK below
-- already forces exactly one of message_id/thread_id to be set (never both,
-- never neither), so the real, schema-enforced idempotency the task asks for
-- ("duplicate application must be idempotent via a UNIQUE constraint --
-- enforce that in the schema, not only in Rust") is two partial unique
-- indexes instead: one scoped to `WHERE message_id IS NOT NULL`, one to
-- `WHERE thread_id IS NOT NULL`. `rmail_core::tags::repo::insert_message_tag`
-- targets each with a matching `ON CONFLICT (...) WHERE ... DO NOTHING`.
CREATE TABLE tags (
    id INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    -- The full hierarchical path ("project/alpha"), not just the leaf
    -- segment -- see rmail_core::tags::hierarchy for why: it is what lets
    -- UNIQUE(account_id, name) dedupe correctly across different parents
    -- (two "alpha" tags under different roots are different names, not a
    -- collision), and what lets `tag:project/*` be a simple name-prefix
    -- match rather than a recursive parent-chain walk.
    --
    -- COLLATE NOCASE: matching (`tag:Work` finding a tag named "work") and
    -- uniqueness (`UNIQUE(account_id, name)` below) must agree on case
    -- sensitivity, or `create_tag("Work", ...)` would silently coexist with
    -- an existing "work" as two different tags while `tag:work` matched
    -- both -- confusing and, worse, exactly the kind of drift a `COLLATE`
    -- mismatch between a column and the query that reads it produces
    -- everywhere else in this schema too. Declaring it on the column once
    -- (rather than adding `COLLATE NOCASE` to every comparison) is what
    -- makes `retrieve::filtermask::tag_predicate_sql`'s plain `t.name = ?`
    -- already case-insensitive without repeating the collation there.
    name TEXT NOT NULL COLLATE NOCASE,
    parent_id INTEGER REFERENCES tags(id) ON DELETE CASCADE,
    color TEXT,
    -- 'local' never touched by sync; 'imap' always round-trips (a persistent
    -- failure is a hard error); 'auto' round-trips and downgrades itself to
    -- 'local' the first time the server refuses it. See
    -- rmail_core::tags::sync.
    sync_mode TEXT NOT NULL DEFAULT 'auto' CHECK (sync_mode IN ('local', 'imap', 'auto')),
    -- Explicit wire keyword/label override; NULL means "derive from
    -- `tags.imap.keyword_prefix` + name" (rmail_core::tags::model::Tag::wire_keyword).
    imap_keyword TEXT,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE(account_id, name)
);

CREATE INDEX idx_tags_account ON tags(account_id);
CREATE INDEX idx_tags_parent ON tags(parent_id) WHERE parent_id IS NOT NULL;

CREATE TABLE message_tags (
    id INTEGER PRIMARY KEY,
    tag_id INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    -- Exactly one of message_id/thread_id is set (CHECK below): a
    -- message-level application, or a thread-level one that covers every
    -- current *and future* member for free via `messages_tags_effective`'s
    -- join on thread_id, with no backfill needed when a message joins later.
    --
    -- prd.md says a message-level tag "follows the stable messages.id"
    -- across a move -- true of this FK *if* `messages.id` is actually
    -- stable across one. It is not, today: `MailStore::move_message`
    -- (task 39, rmail-core/src/mail/mod.rs) has no way to learn the UID a
    -- server assigns a moved message, so it deletes the local row outright
    -- (`crate::sync::remove_messages`) and lets the destination folder's
    -- next sync insert it fresh under a *new* id -- see that module's own
    -- "Move does not guess a new UID" docs. `ON DELETE CASCADE` therefore
    -- means a client-initiated `Move` genuinely loses every message-level
    -- tag application on that message today (a thread-level one survives
    -- only if the resynced message rejoins the same thread). This is a
    -- pre-existing, cross-task gap this migration does not attempt to
    -- close -- fixing it means changing task 39's move semantics to
    -- preserve `messages.id`, out of scope here -- documented rather than
    -- silently assumed away.
    message_id INTEGER REFERENCES messages(id) ON DELETE CASCADE,
    thread_id INTEGER REFERENCES threads(id) ON DELETE CASCADE,
    source TEXT NOT NULL DEFAULT 'user' CHECK (source IN ('user', 'ai', 'rule', 'imap')),
    -- 'applied' = a real, visible tag; 'pending' = an AI suggestion awaiting
    -- ResolveSuggestion; 'rejected' = a resolved-no, kept (not deleted) so a
    -- future suggestion pass can learn from it (task 57) instead of
    -- resuggesting blindly. Only 'applied' rows are ever visible to
    -- `messages_tags_effective` / `tag:` / the tag chip row.
    state TEXT NOT NULL DEFAULT 'applied' CHECK (state IN ('applied', 'pending', 'rejected')),
    -- Set for source='ai' rows: the model's confidence and a short
    -- human-readable rationale ("mentions an invoice number and a due
    -- date"), both surfaced by SuggestTags/ResolveSuggestion. NULL for
    -- source='user'/'rule'/'imap'.
    confidence REAL,
    rationale TEXT,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    CHECK ((message_id IS NULL) <> (thread_id IS NULL))
);

-- Idempotent apply -- see the module comment above for why this is two
-- partial indexes rather than one table-level UNIQUE.
CREATE UNIQUE INDEX idx_message_tags_message_uniq
    ON message_tags(tag_id, message_id) WHERE message_id IS NOT NULL;
CREATE UNIQUE INDEX idx_message_tags_thread_uniq
    ON message_tags(tag_id, thread_id) WHERE thread_id IS NOT NULL;

-- `tag:`/`has:tag` filtering is index-backed (prd.md's <50ms budget): `state`
-- leads each lookup index since almost every read only wants 'applied' rows.
CREATE INDEX idx_message_tags_message_state
    ON message_tags(message_id, state) WHERE message_id IS NOT NULL;
CREATE INDEX idx_message_tags_thread_state
    ON message_tags(thread_id, state) WHERE thread_id IS NOT NULL;
CREATE INDEX idx_message_tags_tag_state ON message_tags(tag_id, state);
CREATE INDEX idx_message_tags_pending ON message_tags(message_id, state) WHERE state = 'pending';

-- Effective tags = a message's own applied message_tags rows, unioned with
-- its thread's (prd.md: "a message's own message_tags ∪ its thread's
-- message_tags"). Only 'applied' rows count -- a 'pending' AI suggestion or
-- a 'rejected' one must never gate `tag:`/`has:tag` or render as a chip.
--
-- `DISTINCT`, not a plain join: a message that has *both* its own
-- message-level application of a tag *and* belongs to a thread with a
-- thread-level application of the same tag matches the join's `OR` twice --
-- two message_tags rows, both applied, one via each side of the `OR` -- and
-- a plain join would surface it as two output rows for the same
-- (message_id, tag_id) pair. `tag:`/`has:tag`'s `EXISTS` checks don't care,
-- but `TagStore::list_tags`'s `COUNT(message_id)` over this view would
-- double-count that message. `source`/`created_at` are deliberately not
-- projected here (unlike message_tags itself, which has exactly one row per
-- application and both are meaningful there): a message tagged both ways
-- can have two different sources/timestamps for the "same" effective tag,
-- and DISTINCT can only dedupe columns it actually selects.
CREATE VIEW messages_tags_effective AS
SELECT DISTINCT m.id AS message_id, mt.tag_id
FROM messages m
JOIN message_tags mt
    ON mt.state = 'applied'
   AND (mt.message_id = m.id OR mt.thread_id = m.thread_id);
