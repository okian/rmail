-- V38: the fuzzy finder's denormalized index, its change feed, and the
-- command-palette registry (task 59, prd.md III-1).
--
-- # Why a second index at all, next to FTS5 and sqlite-vec
--
-- The finder is not search. Search *ranks by relevance* over message bodies
-- and answers "what is the best answer to this question"; the finder *jumps
-- by name* over short, heterogeneous labels and answers "which of the things
-- I already know about did I mean". They share no candidate set (a mailbox,
-- a contact and a keybinding are not rows in `messages`), no scorer (a
-- subsequence aligner is not BM25), and no latency budget (< 16 ms to first
-- paint, on every keystroke, versus < 150 ms once per query). Nothing here
-- duplicates the ranking pipeline; it is a different question over a
-- different corpus.
--
-- # The table exists so folding happens once, not on every daemon start
--
-- `rmail_core::finder::FinderStore` is what a query actually scans, and it
-- lives in memory (see its own docs: the type that answers a keystroke holds
-- no `Database` at all, so it *cannot* issue a query per character typed).
-- This table is that store's durable backing: `match_blob` is the
-- Unicode-folded text the matcher runs against, computed once at write time
-- rather than re-derived from `messages`/`contacts`/... on every startup. A
-- cold daemon loads one narrow table in row order instead of running six
-- joins and folding 100k subject lines.
--
-- # The change feed, and why it is a table rather than an in-process hook
--
-- Rows land in `messages` from the sync engine, from rules, from IMAP
-- reconciliation and from tests, on several tasks' code paths. A trigger is
-- the only place that sees all of them, including the ones a future task
-- adds. `finder_dirty` is therefore written by SQLite itself, and drained on
-- a timer into the in-memory store (`finder.refresh_interval_ms`, default
-- 250 ms) in bounded batches — so a resync that rewrites an entire mailbox
-- costs the finder a few seconds of staleness rather than a stall.

-- The flattened, per-item index. One row per findable thing, whatever kind.
--
-- `kind` is a small integer rather than a text discriminator because it is
-- read on every scan: 0=message 1=mailbox 2=contact 3=saved_search 4=tag
-- 5=command, matching prd.md's own numbering. It is deliberately *not* the
-- wire enum number (`rmail.v1.ItemKind` reserves 0 for UNSPECIFIED, so its
-- MESSAGE is 1); `rmail_core::finder::ItemKind` owns both mappings
-- explicitly so neither side silently inherits the other's numbering.
CREATE TABLE finder_index (
    item_id       INTEGER PRIMARY KEY,
    kind          INTEGER NOT NULL,
    -- The row id in the source table this entry mirrors. NULL only for
    -- kinds with no source table -- there are none today, but `command`
    -- entries are seeded from a registry rather than synced from mail, so
    -- the column stays nullable rather than forcing a placeholder.
    ref_id        INTEGER,
    -- Both cascade. The finder index holds *copies* of subject lines,
    -- display names and folder paths, so deleting an account has to delete
    -- them too -- otherwise `mail account delete` would leave a searchable
    -- shadow of that account's mail behind in a table nobody thinks to look
    -- at. Cascade is also what keeps this table consistent when a mailbox
    -- is removed and its messages go with it.
    --
    -- Note the consequence, handled in `finder::index`: SQLite *documents*
    -- that foreign-key cascade actions fire triggers only when
    -- `recursive_triggers` is on (it is not; see `storage::configure_*`), so
    -- on that reading a cascade removes rows here without writing to
    -- `finder_dirty`. The SQLite this build links happens to be more
    -- generous, which is not something to rely on -- so the drain reconciles
    -- its in-memory copy against this table periodically rather than
    -- trusting the feed to be exhaustive either way.
    account_id    INTEGER REFERENCES accounts(id) ON DELETE CASCADE,
    mailbox_id    INTEGER REFERENCES mailboxes(id) ON DELETE CASCADE,
    -- What a picker row shows first: subject / folder path / display name /
    -- saved-search name / tag / command title. Original text, never folded:
    -- highlight positions are char offsets into *this* string, so it has to
    -- be the exact text the UI renders.
    primary_text  TEXT NOT NULL,
    -- The dimmer second line: sender / email address / query text.
    secondary     TEXT,
    snippet       TEXT,
    -- The text the matcher runs against: `primary_text` and `secondary`
    -- concatenated and Unicode-folded (NFKD, combining marks dropped) so
    -- `cafe` matches `café`. Note what it is *not*: prd.md describes this
    -- column as "lowercased", and it deliberately is not, because the same
    -- document also specifies smart-case ("any uppercase -> case-sensitive")
    -- and a lowercased blob has already thrown away the only information
    -- smart-case needs. Case folding happens in the matcher, per query; see
    -- `rmail_core::finder::fold`.
    match_blob    TEXT NOT NULL,
    -- Unix seconds; drives the recency term of the blended rank.
    last_activity INTEGER,
    is_unread     INTEGER NOT NULL DEFAULT 0,
    importance    REAL NOT NULL DEFAULT 0,
    -- Interaction count: messages in a mailbox, messages from a contact.
    frequency     INTEGER NOT NULL DEFAULT 0,
    updated_at    INTEGER NOT NULL DEFAULT (unixepoch())
) STRICT;

-- One entry per source row. The drain upserts through this constraint with
-- `ON CONFLICT ... DO UPDATE` rather than `INSERT OR REPLACE`, deliberately:
-- `INSERT OR REPLACE` resolves a conflict by *deleting* the old row, which
-- would fire the delete trigger below and enqueue a spurious "this entry is
-- gone" into the feed the drain is in the middle of processing.
CREATE UNIQUE INDEX idx_finder_ref ON finder_index(kind, ref_id);

-- The scan order the store is loaded in, and the order a bounded load keeps
-- when there are more entries than `finder.max_entries`: newest first, so
-- truncation drops the mail least likely to be the thing being jumped to.
CREATE INDEX idx_finder_kind_activity ON finder_index(kind, last_activity DESC);

-- The incremental change feed. `seq` is monotonic so the drain can delete
-- everything it processed with a single range delete instead of matching
-- rows back up one at a time.
--
-- `op`: 0=upsert 1=delete. Rows are coalesced by (kind, ref_id) at drain
-- time -- a message touched forty times during a resync is one re-fold, not
-- forty.
CREATE TABLE finder_dirty (
    seq        INTEGER PRIMARY KEY AUTOINCREMENT,
    kind       INTEGER NOT NULL,
    ref_id     INTEGER NOT NULL,
    op         INTEGER NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch())
) STRICT;

-- The command-palette registry. Seeded from the keymap engine's action
-- registry (`rmail_core::keymap::Action::ALL`) rather than hand-listed here,
-- so a command the palette can run is by construction an action a key can be
-- bound to and `mail keys` can print -- prd.md's "action ids shared by
-- palette/gRPC/MCP", enforced by there being only one list.
CREATE TABLE finder_commands (
    id          INTEGER PRIMARY KEY,
    -- Human-readable title, e.g. "archive".
    name        TEXT NOT NULL,
    -- Extra words that should match this command but do not appear in its
    -- name, space-separated.
    keywords    TEXT,
    -- The stable action id the palette invokes, e.g. `message.archive`.
    action      TEXT NOT NULL UNIQUE,
    -- Reserved for task 85's natural-language palette: a JSON schema for the
    -- arguments Claude fills in. NULL for the parameterless actions that
    -- exist today.
    args_schema TEXT,
    -- The keymap mode this command applies in, or NULL for anywhere.
    context     TEXT
) STRICT;

-- ---------------------------------------------------------------------------
-- The change feed's writers.
-- ---------------------------------------------------------------------------
--
-- One insert/update/delete triple per source table. `updated_at` is not
-- excluded from the UPDATE triggers: a touched row is a row whose
-- `match_blob` may have changed, and the drain's coalescing makes a false
-- positive cost one re-fold rather than anything a user can perceive.

CREATE TRIGGER finder_dirty_messages_insert AFTER INSERT ON messages BEGIN
    INSERT INTO finder_dirty (kind, ref_id, op) VALUES (0, new.id, 0);
END;

CREATE TRIGGER finder_dirty_messages_update AFTER UPDATE ON messages BEGIN
    INSERT INTO finder_dirty (kind, ref_id, op) VALUES (0, new.id, 0);
END;

CREATE TRIGGER finder_dirty_messages_delete AFTER DELETE ON messages BEGIN
    INSERT INTO finder_dirty (kind, ref_id, op) VALUES (0, old.id, 1);
END;

-- prd.md's trigger list names `messages` but not `flags`, which would leave
-- `is_unread` -- one of the five blended-ranking signals -- permanently stale:
-- reading a message writes to `flags`, never to `messages`, so nothing would
-- ever mark the entry dirty. These two close that gap.
CREATE TRIGGER finder_dirty_flags_insert AFTER INSERT ON flags BEGIN
    INSERT INTO finder_dirty (kind, ref_id, op) VALUES (0, new.message_id, 0);
END;

CREATE TRIGGER finder_dirty_flags_delete AFTER DELETE ON flags BEGIN
    INSERT INTO finder_dirty (kind, ref_id, op)
    SELECT 0, old.message_id, 0
    WHERE EXISTS (SELECT 1 FROM messages WHERE id = old.message_id);
END;

CREATE TRIGGER finder_dirty_mailboxes_insert AFTER INSERT ON mailboxes BEGIN
    INSERT INTO finder_dirty (kind, ref_id, op) VALUES (1, new.id, 0);
END;

CREATE TRIGGER finder_dirty_mailboxes_update AFTER UPDATE ON mailboxes BEGIN
    INSERT INTO finder_dirty (kind, ref_id, op) VALUES (1, new.id, 0);
END;

CREATE TRIGGER finder_dirty_mailboxes_delete AFTER DELETE ON mailboxes BEGIN
    INSERT INTO finder_dirty (kind, ref_id, op) VALUES (1, old.id, 1);
END;

CREATE TRIGGER finder_dirty_contacts_insert AFTER INSERT ON contacts BEGIN
    INSERT INTO finder_dirty (kind, ref_id, op) VALUES (2, new.id, 0);
END;

CREATE TRIGGER finder_dirty_contacts_update AFTER UPDATE ON contacts BEGIN
    INSERT INTO finder_dirty (kind, ref_id, op) VALUES (2, new.id, 0);
END;

CREATE TRIGGER finder_dirty_contacts_delete AFTER DELETE ON contacts BEGIN
    INSERT INTO finder_dirty (kind, ref_id, op) VALUES (2, old.id, 1);
END;

CREATE TRIGGER finder_dirty_saved_searches_insert AFTER INSERT ON saved_searches BEGIN
    INSERT INTO finder_dirty (kind, ref_id, op) VALUES (3, new.id, 0);
END;

CREATE TRIGGER finder_dirty_saved_searches_update AFTER UPDATE ON saved_searches BEGIN
    INSERT INTO finder_dirty (kind, ref_id, op) VALUES (3, new.id, 0);
END;

CREATE TRIGGER finder_dirty_saved_searches_delete AFTER DELETE ON saved_searches BEGIN
    INSERT INTO finder_dirty (kind, ref_id, op) VALUES (3, old.id, 1);
END;

CREATE TRIGGER finder_dirty_tags_insert AFTER INSERT ON tags BEGIN
    INSERT INTO finder_dirty (kind, ref_id, op) VALUES (4, new.id, 0);
END;

CREATE TRIGGER finder_dirty_tags_update AFTER UPDATE ON tags BEGIN
    INSERT INTO finder_dirty (kind, ref_id, op) VALUES (4, new.id, 0);
END;

CREATE TRIGGER finder_dirty_tags_delete AFTER DELETE ON tags BEGIN
    INSERT INTO finder_dirty (kind, ref_id, op) VALUES (4, old.id, 1);
END;

CREATE TRIGGER finder_dirty_commands_insert AFTER INSERT ON finder_commands BEGIN
    INSERT INTO finder_dirty (kind, ref_id, op) VALUES (5, new.id, 0);
END;

CREATE TRIGGER finder_dirty_commands_update AFTER UPDATE ON finder_commands BEGIN
    INSERT INTO finder_dirty (kind, ref_id, op) VALUES (5, new.id, 0);
END;

CREATE TRIGGER finder_dirty_commands_delete AFTER DELETE ON finder_commands BEGIN
    INSERT INTO finder_dirty (kind, ref_id, op) VALUES (5, old.id, 1);
END;

-- The removal path the source-table triggers cannot see. A direct
-- `DELETE FROM finder_index` (a cascade, or a future writer) has no
-- corresponding source-row event, so without this the in-memory store would
-- keep serving an entry whose row is gone.
--
-- The drain's *own* deletes echo through here too, and it cleans those up
-- inside the same transaction rather than letting them reach the next pass —
-- see `finder::index::apply_feed`, which explains why leaving them is not
-- merely wasteful but can silently unindex a live message.
CREATE TRIGGER finder_dirty_index_delete AFTER DELETE ON finder_index BEGIN
    INSERT INTO finder_dirty (kind, ref_id, op) VALUES (old.kind, old.ref_id, 1);
END;
