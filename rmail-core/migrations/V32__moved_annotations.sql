-- V32: Carry message-level tags and notes across a client-initiated Move.
--
-- prd.md promises that a message-level tag "follows the stable messages.id"
-- across a move. That promise is not kept today, and V24's own `message_tags`
-- comment says so: `MailStore::move_message` cannot learn the UID the server
-- assigns a moved message, so it deletes the local row outright and lets the
-- destination folder's next sync insert it fresh under a *new* id. Every
-- `ON DELETE CASCADE` hanging off `messages` then fires -- which means a user
-- who drags a message to another folder silently loses every tag they applied
-- to it and every note they wrote on it. That is user-authored data, it is not
-- recoverable from the server, and nothing else in the system reconstructs it.
--
-- The fix cannot be "keep messages.id": learning the new UID needs UIDPLUS's
-- COPYUID response code, which the IMAP client in use does not surface. So
-- this table holds the annotations in escrow between the two halves of the
-- move, keyed by the one identity that *does* survive it -- the RFC 5322
-- `Message-ID` header, which the server has no reason to rewrite.
--
-- Escrow rows are consumed by the first matching insert and are otherwise
-- reaped by age (see `mail::annotations::EXPIRY`), so a move whose message
-- never arrives -- a server that filed it somewhere unexpected, an account
-- removed mid-move -- costs a bounded amount of dead rows rather than a leak.
CREATE TABLE moved_annotations (
    id INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    -- The move's *destination*, not its source. Matching on it as well as on
    -- the header id is what stops the source folder from reclaiming its own
    -- annotations: an IMAP move that has not yet propagated can have the
    -- source mailbox resync and re-insert the message before the destination
    -- ever sees it, and a header-id-only match would hand the escrow to that
    -- copy -- which the next expunge then deletes, losing the data for good in
    -- the exact scenario this table exists to survive.
    dest_mailbox_id INTEGER NOT NULL REFERENCES mailboxes(id) ON DELETE CASCADE,
    -- RFC 5322 `Message-ID`. Not NULL-able: a message without one cannot be
    -- re-identified after the move, so there is nothing to escrow for it and
    -- `capture` does not write a row at all.
    header_message_id TEXT NOT NULL,
    -- What was escrowed and what it must be re-attached to. Open TEXT with a
    -- CHECK, matching this schema's existing convention for small vocabularies
    -- owned by application code (`message_tags.source`, `notes.author`).
    --
    -- The `thread_*` kinds are not redundant with the message-level ones. A
    -- thread-level annotation normally needs no help: it hangs off
    -- `threads(id)`, and a moved message rejoins its conversation on resync.
    -- But `sync::remove_messages` calls `repair_threads`, which deletes a
    -- thread the move just emptied -- and single-message threads are the
    -- common case in a mailbox. Moving the only message of a thread therefore
    -- destroys its thread-level tags and notes exactly the way the
    -- message-level ones are destroyed, one level up. Those are escrowed too,
    -- and only when the thread is actually about to be reaped; a thread with
    -- other messages in it survives and must not be double-tagged.
    kind TEXT NOT NULL CHECK (
        kind IN ('tag', 'note', 'thread_tag', 'thread_note')
    ),
    -- The row to re-create, as JSON. Two annotation kinds with genuinely
    -- different shapes share one escrow table rather than getting a table
    -- each: the payload is written and read by one module
    -- (`mail::annotations`), never queried across, and never joined to. A
    -- column-per-field union table would be half NULLs and would still need
    -- the `kind` discriminator to be read correctly.
    payload TEXT NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
) STRICT;

-- The replay probe: one indexed lookup per inserted message that carries a
-- `Message-ID`. Leading with the destination mailbox keeps a full initial sync
-- of an unrelated folder off the index entirely.
CREATE INDEX idx_moved_annotations_lookup
    ON moved_annotations(dest_mailbox_id, header_message_id);

-- The reaper's scan.
CREATE INDEX idx_moved_annotations_created ON moved_annotations(created_at);
