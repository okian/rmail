-- V21: AI triage-pass output (task 48).
--
-- One row per (message_id, pass, model): a triage row and a future deep row
-- (task 49) for the same message are different rows -- different `pass` --
-- and re-analyzing a message with a different model does not clobber a
-- prior model's verdict, it adds a second row next to it. Re-running the
-- *same* pass under the *same* model is an upsert
-- (`ON CONFLICT(message_id, pass, model) DO UPDATE`, in
-- `rmail-core/src/ai/triage.rs`), not a second row -- "re-triage this
-- message" replaces its own prior triage rather than accumulating history
-- the way `ai_ledger` (V18) deliberately does for every call.
--
-- `thread_id` is a denormalized snapshot of `messages.thread_id` at write
-- time -- a plain copy, not enforced by a foreign key, the same choice
-- `ai_ledger.account_id`/`message_id` made (V18's docs) for the same reason:
-- this table is written once per AI call and must never let a
-- `messages`/`threads` foreign-key nuance turn a successful triage pass
-- into a failed write. `message_id`/`account_id` *are* enforced FKs here
-- (unlike the ledger) because those two are the row's actual identity -- an
-- orphaned triage row for a message that no longer exists is exactly the
-- staleness `ON DELETE CASCADE` exists to prevent, the same as `ai_queue`
-- (V20).
--
-- `needs_reply` is nullable INTEGER (0/1/NULL), not a plain boolean
-- default: NULL means "this row's pass never produced a needs_reply
-- verdict" (e.g. a deep-only row read by a caller expecting a triage
-- field), which `retrieve::filtermask`'s `ai:needs-reply` predicate must
-- never confuse with "produced a verdict of false" -- `needs_reply = 1` in
-- a WHERE clause already treats NULL and 0 identically (both fail the
-- predicate), so nothing downstream needs to special-case it, but the
-- column itself has to stay nullable for that to be honest rather than
-- accidental.
CREATE TABLE ai_summaries (
    id               INTEGER PRIMARY KEY,
    message_id       INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    account_id       INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    thread_id        INTEGER,
    model            TEXT NOT NULL,
    -- 'triage' | 'deep' (task 49) | ... -- open vocabulary, the same one
    -- `ai_queue.pass` / `ai_ledger.pass` already use.
    pass             TEXT NOT NULL,
    schema_version   INTEGER NOT NULL,
    tl_dr            TEXT,
    summary          TEXT,
    thread_summary   TEXT,
    key_points       TEXT,
    todos            TEXT,
    sentiment        TEXT,
    category         TEXT,
    priority         TEXT,
    needs_reply      INTEGER,
    suggested_reply  TEXT,
    suggested_tags   TEXT,
    -- The audit-ledger row this pass's provider call was recorded under --
    -- every AI artifact traces back to the call that produced it. Never a
    -- FK target for deletion: `ai_ledger` rows are never deleted (V18's
    -- append-only triggers), so a plain REFERENCES is safe with no ON
    -- DELETE clause, matching `ai_queue.ledger_entry_id`.
    ledger_entry_id  INTEGER REFERENCES ai_ledger(id),
    created_at       INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE (message_id, pass, model)
) STRICT;

-- Every `ai:` predicate (`retrieve::filtermask::ai_predicate_sql`) is a
-- correlated `EXISTS (... WHERE ai_summaries.message_id = messages.id AND
-- ...)`, driven from `messages` -- so the query planner always seeks
-- `ai_summaries` by `message_id` first, then filters the (typically one or
-- two) matching rows in memory. `UNIQUE (message_id, pass, model)` above
-- already gives that seek an index via its leading column
-- (`EXPLAIN QUERY PLAN` confirms SQLite picks `sqlite_autoindex_ai_summaries_1`
-- for a bare `message_id = ?` search with no other index present), so a
-- dedicated `message_id` index, or one on `category`/`priority`, would never
-- be chosen by this access pattern and would only add write-side cost. The
-- one index below *is* chosen (also `EXPLAIN QUERY PLAN`-verified): it is
-- small (most rows have `needs_reply` NULL or 0) and matches the predicate's
-- own condition exactly, which the `message_id`-seek plan does not replace.
CREATE INDEX idx_ai_summaries_needs_reply ON ai_summaries(needs_reply) WHERE needs_reply = 1;

-- ai_fts: the AI-enrichment text index the acceptance criterion asks for.
-- Contentless, the same reasoning as `fts_messages` (V9): the text already
-- lives in `ai_summaries`, and `contentless_delete=1` is what lets a plain
-- `DELETE ... WHERE rowid = ?` work without SQLite demanding the deleted
-- row's original column values back.
--
-- One `ai_fts` row per *message*, not per `ai_summaries` row: a message can
-- carry a triage row and a deep row (task 49) side by side, and a search
-- over "what has the AI said about this message" should see both folded
-- together rather than have to know which pass produced which field. The
-- triggers below re-aggregate every surviving `ai_summaries` row for a
-- message on every insert/update/delete -- delete-then-insert, the same
-- "no update on a contentless table" discipline `fts_messages`'s own sync
-- uses -- so `ai_fts.rowid` always reflects the union of whatever passes
-- have run for that message.
CREATE VIRTUAL TABLE ai_fts USING fts5(
    tl_dr,
    summary,
    thread_summary,
    key_points,
    tags,
    content = '',
    contentless_delete = 1,
    tokenize = "unicode61 remove_diacritics 2"
);

-- `group_concat` skips NULL inputs (a deep-only column on a triage row,
-- say), so folding every row for a message together never inserts a literal
-- "NULL" token; `COALESCE(..., '')` only guards the case where *every* row
-- for the message left a given field NULL, matching `fts_messages`'s own
-- "default empty string, never a SQL NULL, into a contentless FTS column"
-- rule.
CREATE TRIGGER ai_summaries_fts_insert AFTER INSERT ON ai_summaries BEGIN
    DELETE FROM ai_fts WHERE rowid = new.message_id;
    INSERT INTO ai_fts (rowid, tl_dr, summary, thread_summary, key_points, tags)
    SELECT
        new.message_id,
        COALESCE(group_concat(tl_dr, ' '), ''),
        COALESCE(group_concat(summary, ' '), ''),
        COALESCE(group_concat(thread_summary, ' '), ''),
        COALESCE(group_concat(key_points, ' '), ''),
        COALESCE(group_concat(suggested_tags, ' '), '')
    FROM ai_summaries WHERE message_id = new.message_id;
END;

-- `message_id` is part of the row's conflict/identity key and application
-- code never rewrites it on an existing row -- this trigger recomputes
-- `new.message_id`'s aggregate and only cleans up `old.message_id`, which is
-- correct as long as that assumption holds.
CREATE TRIGGER ai_summaries_fts_update AFTER UPDATE ON ai_summaries BEGIN
    DELETE FROM ai_fts WHERE rowid = old.message_id;
    INSERT INTO ai_fts (rowid, tl_dr, summary, thread_summary, key_points, tags)
    SELECT
        new.message_id,
        COALESCE(group_concat(tl_dr, ' '), ''),
        COALESCE(group_concat(summary, ' '), ''),
        COALESCE(group_concat(thread_summary, ' '), ''),
        COALESCE(group_concat(key_points, ' '), ''),
        COALESCE(group_concat(suggested_tags, ' '), '')
    FROM ai_summaries WHERE message_id = new.message_id;
END;

-- After the delete, zero or more rows for `old.message_id` may remain (the
-- deep row survives a triage row's deletion, or vice versa); the `HAVING`
-- guard is what tells "recompute the aggregate" apart from "nothing is left
-- to index" -- inserting an all-empty rowid would leave a phantom, always-
-- matching-nothing-but-present row in the index for a message with no AI
-- enrichment left at all.
CREATE TRIGGER ai_summaries_fts_delete AFTER DELETE ON ai_summaries BEGIN
    DELETE FROM ai_fts WHERE rowid = old.message_id;
    INSERT INTO ai_fts (rowid, tl_dr, summary, thread_summary, key_points, tags)
    SELECT
        old.message_id,
        COALESCE(group_concat(tl_dr, ' '), ''),
        COALESCE(group_concat(summary, ' '), ''),
        COALESCE(group_concat(thread_summary, ' '), ''),
        COALESCE(group_concat(key_points, ' '), ''),
        COALESCE(group_concat(suggested_tags, ' '), '')
    FROM ai_summaries WHERE message_id = old.message_id
    HAVING count(*) > 0;
END;
