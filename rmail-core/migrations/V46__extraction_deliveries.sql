-- V46: what has already been handed to a calendar or a task tracker.
--
-- prd.md #65 requires calendar/task extraction to be "idempotent per message".
-- That cannot be a property of the extractor: the extractor is deterministic,
-- so running it twice produces the same events twice, and it is the *delivery*
-- that must not repeat. A pipe to `osascript` creates a reminder every time it
-- runs; a POST to a task webhook creates a task every time it is answered.
--
-- So the claim lives here, one row per (message, kind, uid, sink), behind a
-- UNIQUE index. `crate::extract::events::Delivery::claim` inserts with
-- `INSERT OR IGNORE` and treats "no row changed" as "somebody already
-- delivered this" — which makes the database, not the process, the thing that
-- decides who was first. Two concurrent RPCs on the same message cannot both
-- win.
--
-- The claim is taken *before* the side effect fires, and
-- `Delivery::release` deletes it when the sink fails. The other order — fire,
-- then record — leaves a window in which a crash duplicates a task in
-- somebody's tracker, and a duplicated task is the failure this table exists
-- to prevent. The cost of this order is that a claim whose process dies
-- between the insert and the sink is never retried automatically; that is a
-- missing task rather than a spurious one, and it is the right way round.
--
-- `sink` is part of the key on purpose. The same event legitimately goes to
-- both a webhook and a pipe, and a key without it would let whichever sink ran
-- first silently suppress the other.
CREATE TABLE extraction_deliveries (
    message_id   INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    -- event | task. Not a foreign key to anything: an extracted item has no
    -- row of its own, and giving it one would mean storing a copy of the
    -- message's own content next to the message.
    kind         TEXT NOT NULL,
    -- The item's iCalendar UID, or the deterministic hash
    -- `extract::events::synthesize_uid` derives when the source gave none.
    uid          TEXT NOT NULL,
    -- ics | command | webhook
    sink         TEXT NOT NULL,
    delivered_at INTEGER NOT NULL DEFAULT (unixepoch()),
    PRIMARY KEY (message_id, kind, uid, sink),
    CHECK (kind IN ('event', 'task')),
    CHECK (sink IN ('ics', 'command', 'webhook')),
    CHECK (uid <> '')
) STRICT;

-- "What has this message already delivered" — the read `ExtractEvents` makes
-- before it renders a picker, and the one an operator makes when a task did
-- not appear where they expected.
CREATE INDEX idx_extraction_deliveries_message
    ON extraction_deliveries(message_id, kind, sink);
