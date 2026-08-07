-- V26: saved searches and deterministic smart folders (task 35).
--
-- prd.md, "Saved Searches & Smart Folders":
--   * Saved search  -- a named query string; re-run through the full
--     pipeline on demand.
--   * Smart folder  -- a saved query re-evaluated on every sync so
--     membership stays live. No mail is moved on the server. Smart folders
--     can trigger actions (auto-tag/notify) on new matches.
--
-- # What is deliberately NOT stored here: results
--
-- Neither table has a "results" column, a `saved_search_hits` table, or any
-- other place to put the message ids a query resolved to *last* time. That
-- is the whole point of both features and the single most likely way to get
-- this schema wrong. A saved search stores its raw query text and re-runs
-- it through the real ranking pipeline; a smart folder stores its predicate
-- and recomputes membership from it. A snapshot of ids would go stale the
-- moment the next message synced -- exactly the failure "membership stays
-- live" names -- and would additionally be a second, divergent answer to
-- "what matches this query" alongside the retrieval pipeline's own.
--
-- `smart_folder_matched` below looks like such a snapshot and is not; see
-- its own comment.

CREATE TABLE saved_searches (
    id INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    -- COLLATE NOCASE, matching `tags.name`'s reasoning verbatim: lookup by
    -- name (`mail search --saved Weekly`) and uniqueness
    -- (`UNIQUE(account_id, name)` below) must agree on case sensitivity, or
    -- creating "Weekly" beside an existing "weekly" silently succeeds while
    -- a lookup matches whichever row the query planner reached first.
    name TEXT NOT NULL COLLATE NOCASE,
    -- The *raw* query string exactly as typed -- operators, free text,
    -- sigils and all. Not a parsed/normalized form: `query::parse` is
    -- lossless-by-design over `raw` (see `ParsedQuery::raw`'s own docs) and
    -- re-parsing on each run is what guarantees a saved search behaves
    -- identically to typing the same string into `mail search`, including
    -- after a future grammar addition teaches the parser a new operator the
    -- string already contained.
    query TEXT NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
    -- When this search was last re-run. Purely informational (recency
    -- ordering in a picker); nothing about correctness reads it.
    last_run_at INTEGER,
    UNIQUE(account_id, name)
) STRICT;

-- No separate `(account_id)` index on either table: `UNIQUE(account_id,
-- name)` already builds one with account_id leftmost, which fully serves
-- both `WHERE account_id = ? ORDER BY name` reads (the only shape either
-- table is scanned by). A second index would be pure write amplification.

CREATE TABLE smart_folders (
    id INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    name TEXT NOT NULL COLLATE NOCASE,
    -- An operator-DSL predicate (`from:stripe is:unread -in:Spam`), compiled
    -- to a SQL `WHERE` fragment by `rmail_core::tags::query` -- the same
    -- compiler `BulkTag`'s `query` selector uses, so "which messages does
    -- this name" has exactly one answer across both features.
    --
    -- Free text is rejected at create time rather than ignored (see
    -- `rmail_core::smart_folder`'s docs): a predicate whose ranked half is
    -- silently dropped resolves to a *strictly larger* set than the user
    -- described, and a smart folder that quietly contains every message in
    -- the account is worse than one that refused to be created. NL-defined
    -- predicates compiled into a hybrid plan are task 58's, not this
    -- column's.
    predicate TEXT NOT NULL,
    -- Actions fired for genuinely new members (see `smart_folder_matched`).
    -- NULL / 0 mean "no action"; a folder with neither is a pure view.
    auto_tag TEXT,
    notify INTEGER NOT NULL DEFAULT 0 CHECK (notify IN (0, 1)),
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
    last_evaluated_at INTEGER,
    UNIQUE(account_id, name)
) STRICT;

-- The action ledger -- NOT the membership.
--
-- Membership is always recomputed from `smart_folders.predicate`; nothing in
-- this build ever reads this table to answer "what is in this folder"
-- (`SmartFolderStore::members` runs the predicate, and its tests assert a
-- message inserted after the last evaluation shows up without any write to
-- this table having happened). What this table exists for is the one
-- question the predicate cannot answer on its own: *which members have
-- already had this folder's auto-tag/notify actions fired for them*, so a
-- re-evaluation that finds the same membership fires nothing at all and a
-- re-evaluation that finds one new message fires exactly once, for it.
--
-- `fired_at` is nullable on purpose. A row is inserted when a message first
-- enters the folder, and stamped only after the actions for it have
-- actually run. Ordering the two that way (reconcile-then-fire-then-stamp
-- rather than reconcile-and-stamp-then-fire) makes a crash in between
-- re-fire on the next evaluation instead of silently swallowing the
-- notification: auto-tag is idempotent by construction (the `message_tags`
-- partial unique index), and a duplicate notification is strictly better
-- than mail the user was never told about.
--
-- Rows for departed members are deleted, so this table is bounded by
-- current membership rather than by everything that ever matched. The
-- consequence is deliberate and documented in `rmail_core::smart_folder`: a
-- message that leaves the predicate and later satisfies it again is a new
-- match again, and fires again.
CREATE TABLE smart_folder_matched (
    smart_folder_id INTEGER NOT NULL REFERENCES smart_folders(id) ON DELETE CASCADE,
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    matched_at INTEGER NOT NULL DEFAULT (unixepoch()),
    -- NULL = "entered the folder, actions not yet fired". See above.
    fired_at INTEGER,
    PRIMARY KEY (smart_folder_id, message_id)
) STRICT, WITHOUT ROWID;

-- The pending-action scan (`WHERE smart_folder_id = ? AND fired_at IS NULL`)
-- runs on every evaluation and is almost always empty; a partial index keeps
-- it that cheap without carrying the fired majority of the table.
CREATE INDEX idx_smart_folder_matched_pending
    ON smart_folder_matched(smart_folder_id) WHERE fired_at IS NULL;

-- `ON DELETE CASCADE` from `messages` handles the row itself, but SQLite
-- needs an index on the referencing column to resolve the cascade without a
-- full table scan per deleted message -- and message deletion is a bulk
-- operation (an expunge sweeps a whole UID range).
CREATE INDEX idx_smart_folder_matched_message ON smart_folder_matched(message_id);
