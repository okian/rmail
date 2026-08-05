-- V6: the durable event log.
--
-- Everything downstream of sync -- indexing, AI enrichment, rules, and the
-- gRPC event stream -- is driven by events rather than by polling the mailbox.
-- That only works if the log is *durable and gapless*: a client that
-- disconnects mid-stream resumes from its cursor, and the guarantee it needs is
-- that no event was assigned a position it never got to see.
--
-- So `seq` is the contract, and it carries AUTOINCREMENT for one specific
-- reason. A plain INTEGER PRIMARY KEY is SQLite's rowid alias, and rowid
-- assignment is `max(rowid) + 1` over the rows that *currently exist* -- so an
-- emptied table restarts at 1. Retention empties this table routinely: a
-- mailbox quieter than the age window has every row swept, and the next event
-- would be handed seq 1 again. A subscriber holding cursor 500 would then be
-- told it was current while 500 fresh events sat below its cursor, forever.
-- AUTOINCREMENT keeps the high-water mark in `sqlite_sequence` so seq is
-- monotonic across an empty table, at the cost of one extra row read per
-- insert.
--
-- Retention deletes from the *bottom* only, so the live range is always
-- contiguous: a cursor is either inside it, or it is older than `MIN(seq)` and
-- the client is told exactly that.
--
-- Payload is JSON text rather than columns per kind. The kinds are a union that
-- will keep growing (send results, rule firings, AI summaries), and a table
-- that gains a nullable column per variant becomes unreadable long before it
-- becomes wrong. The indexed columns are the ones subscriptions filter on;
-- everything else lives in the payload.
CREATE TABLE events (
    seq        INTEGER PRIMARY KEY AUTOINCREMENT,
    kind       TEXT NOT NULL,
    -- Scope, for subscription filters. All nullable: a sync-state event has no
    -- message, and an account-wide event has no mailbox.
    account_id INTEGER,
    mailbox_id INTEGER,
    message_id INTEGER,
    -- Unix seconds. Not a foreign key to anything: an event describing a
    -- deletion must outlive the row it describes.
    at         INTEGER NOT NULL DEFAULT (unixepoch()),
    payload    TEXT NOT NULL DEFAULT '{}'
) STRICT;

-- Retention prunes by age, and the cursor scan reads `seq > ?` in order, which
-- the primary key already serves. This index is for the age sweep.
CREATE INDEX idx_events_at ON events(at);
-- Subscriptions are almost always scoped to one account and resumed from a
-- cursor, so lead with the filter and follow with the ordering.
CREATE INDEX idx_events_account_seq ON events(account_id, seq);
