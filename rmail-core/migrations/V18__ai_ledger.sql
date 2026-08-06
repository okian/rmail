-- The append-only AI call audit ledger and its day rollups (task 45).
--
-- `ai_ledger` is enforced immutable at the database layer, not merely by
-- convention: the three triggers below reject any UPDATE, DELETE, or
-- id-colliding INSERT (SQLite's `INSERT OR REPLACE` deletes-then-inserts, and
-- that delete is invisible to a plain `BEFORE DELETE` trigger unless
-- `PRAGMA recursive_triggers` is on — which this codebase does not set, so
-- the third trigger below is load-bearing, not redundant) regardless of which
-- code path issues it. An audit trail whose only protection is "the
-- application never edits it" is one bad migration, one debugging session, or
-- one future contributor away from silently no longer being one. There is
-- deliberately no CASCADE from `accounts`/`messages` onto this table either —
-- `account_id`/`message_id` are plain, unenforced references, so deleting an
-- account can never touch a ledger row (an `ON DELETE SET NULL` would issue
-- an UPDATE against this very table, which the trigger below would then
-- reject, breaking the unrelated delete).
CREATE TABLE ai_ledger (
    id INTEGER PRIMARY KEY,
    -- Unix seconds; when this call was recorded.
    created_at INTEGER NOT NULL,
    -- Context ids. Deliberately not foreign keys — see above.
    account_id INTEGER,
    message_id INTEGER,
    -- The provider's id for the response, when it produced one.
    request_id TEXT,
    model TEXT NOT NULL,
    -- Caller-supplied context tag, e.g. 'triage' | 'deep'. Not every AI call
    -- belongs to the two-pass summarization pipeline, so this is optional.
    pass TEXT,
    input_tokens INTEGER NOT NULL,
    output_tokens INTEGER NOT NULL,
    cache_creation_input_tokens INTEGER NOT NULL,
    cache_read_input_tokens INTEGER NOT NULL,
    cost_usd REAL NOT NULL,
    -- What the redaction pass did to this payload before it left the
    -- machine, e.g. 'none' | 'redacted'. Recorded as whatever label the
    -- caller supplies; this table does not define the vocabulary (task 44
    -- does) so it can compose with that module without depending on it.
    redaction_level TEXT NOT NULL,
    latency_ms INTEGER NOT NULL,
    -- SHA-256 of the exact bytes transmitted to the provider, post-redaction.
    -- Proof of what left the machine, not what the caller intended to send.
    payload_sha256 BLOB NOT NULL,
    -- 'ok' | 'error'.
    status TEXT NOT NULL CHECK (status IN ('ok', 'error')),
    error TEXT
);

-- Every query this ledger's readers actually run: a time-range scan
-- (QueryAiCalls/ExportLedger's default order), a narrow-by-context scan for a
-- specific account or message, and a narrow-by-model scan.
CREATE INDEX idx_ai_ledger_created_at ON ai_ledger(created_at);
CREATE INDEX idx_ai_ledger_account ON ai_ledger(account_id);
CREATE INDEX idx_ai_ledger_message ON ai_ledger(message_id);
CREATE INDEX idx_ai_ledger_model ON ai_ledger(model);

CREATE TRIGGER ai_ledger_no_update
BEFORE UPDATE ON ai_ledger
BEGIN
    SELECT RAISE(ABORT, 'ai_ledger is append-only: UPDATE is not permitted');
END;

CREATE TRIGGER ai_ledger_no_delete
BEFORE DELETE ON ai_ledger
BEGIN
    SELECT RAISE(ABORT, 'ai_ledger is append-only: DELETE is not permitted');
END;

-- `INSERT OR REPLACE INTO ai_ledger (id, ...) VALUES (<existing id>, ...)` is
-- itself an insert, not an update or delete, so neither trigger above sees
-- it — SQLite resolves the id collision by silently deleting the old row
-- first, and that internal delete does not fire `BEFORE DELETE` triggers
-- unless `recursive_triggers` is on. Block the collision directly instead:
-- a caller naming an id that already exists is either a bug (id reuse) or an
-- attempt to overwrite history, and both should fail loudly.
-- This guard covers a rowid/primary-key collision only, because that is the
-- only UNIQUE constraint this table has today. If a future migration adds
-- another UNIQUE index (e.g. on `request_id`), `INSERT OR REPLACE` gets a
-- second conflict target this trigger does not watch, and would need a
-- matching `WHEN` clause (or its own trigger) added alongside it.
CREATE TRIGGER ai_ledger_no_id_reuse
BEFORE INSERT ON ai_ledger
WHEN NEW.id IS NOT NULL AND EXISTS (SELECT 1 FROM ai_ledger WHERE id = NEW.id)
BEGIN
    SELECT RAISE(ABORT, 'ai_ledger is append-only: INSERT OR REPLACE over an existing id is not permitted');
END;

-- Day rollups, keyed by UTC calendar day ('YYYY-MM-DD'). Unlike the ledger
-- itself this table is mutated in place: it is a materialized aggregate, not
-- the historical record, and is rebuilt-by-increment on every ledger write.
CREATE TABLE ai_usage (
    day TEXT PRIMARY KEY,
    requests INTEGER NOT NULL DEFAULT 0,
    input_tokens INTEGER NOT NULL DEFAULT 0,
    output_tokens INTEGER NOT NULL DEFAULT 0,
    cache_creation_input_tokens INTEGER NOT NULL DEFAULT 0,
    cache_read_input_tokens INTEGER NOT NULL DEFAULT 0,
    cost_usd REAL NOT NULL DEFAULT 0
);
