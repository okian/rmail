-- V7: the durable index work queue and its completion record.
--
-- Indexing is four independent stages per message -- extract, lexical,
-- entities, semantic -- and they fail independently. An embeddings provider
-- being down must not stop lexical search from being built, so each stage is
-- its own row rather than a step in one job's state machine.
--
-- Two tables, because they answer different questions:
--
--   index_queue  what still needs doing, and what went wrong last time
--   index_state  what has been done, and against which content and model
--
-- The re-index decision is a comparison between them: a job is worth running
-- when the queued `content_hash` differs from the recorded one, or when the
-- recorded `model` is not the one now configured. That is what makes a re-run
-- over unchanged mail free -- the common case on every restart -- while a
-- changed body or a switched embedding model re-runs exactly what it must.
--
-- `message_id` is the stable surrogate key from `messages`, not an IMAP UID.
-- The PRD calls this column `message_uid`; the UID is folder-scoped and dies
-- with a UIDVALIDITY bump, which is precisely wrong for a record that must
-- outlive one.
CREATE TABLE index_queue (
    job_id       INTEGER PRIMARY KEY,
    message_id   INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    -- extract | lexical | entities | semantic | thread
    kind         TEXT NOT NULL,
    -- Lower runs first. Recent and INBOX mail is enqueued ahead of the archive
    -- so a user searching right after a sync finds today's mail first.
    priority     INTEGER NOT NULL DEFAULT 100,
    -- What the job is indexing. Opaque here; the extraction stage defines it.
    content_hash BLOB,
    -- pending | leased | done | dead
    state        TEXT NOT NULL DEFAULT 'pending',
    attempts     INTEGER NOT NULL DEFAULT 0,
    -- Set while leased. A worker that dies leaves this in the past, and the
    -- reaper returns the job to the queue -- which is the only reason a crash
    -- mid-index is recoverable without a coordinator.
    lease_expires_at INTEGER,
    -- Who holds the lease, for diagnosing a stuck job.
    leased_by    TEXT,
    -- Backoff: a failed job is invisible to `lease` until this passes.
    next_attempt_at  INTEGER NOT NULL DEFAULT 0,
    last_error   TEXT,
    enqueued_at  INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at   INTEGER NOT NULL DEFAULT (unixepoch()),
    -- One outstanding job per (message, stage). Re-enqueuing an already-queued
    -- stage updates it rather than queueing it twice.
    UNIQUE (message_id, kind)
) STRICT;

-- The lease query: the ready jobs, best first.
--
-- Column order matters more than it looks. `next_attempt_at` is a *range*
-- predicate, and a range column before the sort columns stops SQLite using the
-- index for ordering -- it materializes every ready row and sorts it before
-- applying LIMIT. At a million pending rows (a first index of a large mailbox
-- is roughly five jobs per message) that is tens of milliseconds per poll, per
-- worker, while holding the single writer connection that sync and the UI also
-- write through. So the sort columns come first and `next_attempt_at` is left
-- as a residual filter; `state` still lets dead and done jobs be skipped
-- without being read, which is what keeps a poison job from head-of-line
-- blocking the queue behind it.
CREATE INDEX idx_index_queue_ready
    ON index_queue(state, priority, enqueued_at, job_id);
-- The reaper's query: leased jobs whose lease has lapsed.
CREATE INDEX idx_index_queue_lease ON index_queue(state, lease_expires_at);

CREATE TABLE index_state (
    message_id   INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    kind         TEXT NOT NULL,
    -- What was indexed. NULL means "indexed, content unknown" -- a stage that
    -- has no meaningful content hash of its own.
    content_hash BLOB,
    -- Which embedding model produced it, for the stages that use one. A model
    -- switch makes every row naming the old one stale.
    model        TEXT,
    indexed_at   INTEGER NOT NULL DEFAULT (unixepoch()),
    PRIMARY KEY (message_id, kind)
) STRICT;
