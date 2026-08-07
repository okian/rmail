-- V23: Notes subsystem (task 56).
--
-- Freeform markdown attached to a message or a thread (prd.md, "III-4.
-- Notes & Tags"). Multiple notes per target, timestamped, editable in
-- `$EDITOR`, authored by 'user' or 'ai'.
--
-- The message-or-thread target is a XOR, and it is enforced here as a CHECK
-- constraint -- not only in `rmail-core::notes`' Rust API -- so the invariant
-- holds for *every* write path (a future migration's `INSERT`, a manual
-- `sqlite3` session, a bug in code nobody has written yet), not only calls
-- that went through `NoteStore`. `rmail-core/src/notes/tests.rs` proves the
-- database itself rejects both a row targeting neither and a row targeting
-- both, via raw SQL that never touches the Rust API.
CREATE TABLE notes (
    id         INTEGER PRIMARY KEY,
    message_id INTEGER REFERENCES messages(id) ON DELETE CASCADE,
    thread_id  INTEGER REFERENCES threads(id) ON DELETE CASCADE,
    body_md    TEXT NOT NULL,
    -- 'user' | 'ai'. Open TEXT rather than a SQL-level enum, matching this
    -- schema's existing convention for small vocabularies owned by
    -- application code (`ai_summaries.pass`, `message_tags.source`) --
    -- `rmail-core::notes::NoteAuthor` is the closed enum that actually
    -- enforces the vocabulary on every write this build makes.
    author     TEXT NOT NULL DEFAULT 'user',
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
    CHECK ((message_id IS NULL) <> (thread_id IS NULL))
) STRICT;

-- Partial indexes: a query for a message's notes never cares about the
-- (always-NULL, for that row) thread_id half of the table and vice versa, so
-- indexing only the rows where the column is actually set keeps both indexes
-- half the size of a plain `CREATE INDEX ... (message_id)` while still
-- backing `NoteStore::list`'s `WHERE message_id IS ? AND thread_id IS ?`.
CREATE INDEX idx_notes_message ON notes(message_id) WHERE message_id IS NOT NULL;
CREATE INDEX idx_notes_thread ON notes(thread_id) WHERE thread_id IS NOT NULL;

-- notes_fts: an external-content FTS5 index over note bodies
-- (`content = 'notes'`, `content_rowid = 'id'` -- the exact shape prd.md's
-- data model specifies), distinct from `fts_messages`' own `notes` column
-- (V9, task 18). The two serve different jobs: `fts_messages.notes` folds a
-- message's *effective* note text (its own notes plus its thread's, see
-- `rmail-core::notes::refresh_note_index`) into the same ranked document as
-- its subject and body, so a plain free-text search surfaces a note the way
-- it surfaces anything else. This table indexes each note *row* on its own,
-- which is what a query scoped to "notes matching X" (as opposed to
-- "messages matching X") needs, and what makes `notes`/`notes_fts` the pair
-- this task's acceptance criterion names.
--
-- External-content, not contentless: unlike `fts_messages`/`ai_fts` (which
-- store nothing and read `index_content`/`ai_summaries` back out at query
-- time), a note's text has nowhere else it needs to be re-derived from for
-- FTS5's own housekeeping, and `content = 'notes'` lets SQLite verify a
-- `rowid` against real backing rows rather than trusting the contentless
-- bookkeeping `contentless_delete=1` substitutes for it.
CREATE VIRTUAL TABLE notes_fts USING fts5(
    body_md,
    content = 'notes',
    content_rowid = 'id',
    tokenize = "unicode61 remove_diacritics 2"
);

-- Trigger-synced, per SQLite's own documented recipe for an external-content
-- FTS5 table (the engine does not maintain one automatically the way a
-- contentless table's `contentless_delete=1` partially does). The special
-- `notes_fts` "commands" row (`INSERT INTO notes_fts(notes_fts, rowid, ...)
-- VALUES ('delete', ...)`) is what lets a delete/update supply the *old* text
-- FTS5 needs to remove that row's terms from the index -- a plain
-- `DELETE FROM notes_fts WHERE rowid = ?` does not work against an
-- external-content table the way it does against `fts_messages`'
-- contentless one.
--
-- `rmail-core/src/notes/tests.rs` exercises all three directly with raw SQL
-- against `notes`/`notes_fts` -- insert, update, delete, and the
-- `DELETE FROM messages` cascade case -- so the invariant is proven for
-- every write path, not only `NoteStore`'s own.
CREATE TRIGGER notes_fts_insert AFTER INSERT ON notes BEGIN
    INSERT INTO notes_fts(rowid, body_md) VALUES (new.id, new.body_md);
END;

CREATE TRIGGER notes_fts_delete AFTER DELETE ON notes BEGIN
    INSERT INTO notes_fts(notes_fts, rowid, body_md) VALUES ('delete', old.id, old.body_md);
END;

CREATE TRIGGER notes_fts_update AFTER UPDATE ON notes BEGIN
    INSERT INTO notes_fts(notes_fts, rowid, body_md) VALUES ('delete', old.id, old.body_md);
    INSERT INTO notes_fts(rowid, body_md) VALUES (new.id, new.body_md);
END;
