-- V20: the durable AI work queue (task 47).
--
-- Sibling of `index_queue` (V7), same discipline: what still needs doing and
-- what went wrong last time, a lease that survives the worker holding it, and
-- a poison job that backs off and eventually quarantines rather than blocking
-- the queue behind it. See `rmail-core/src/ai/queue.rs`'s module docs for why
-- an AI job additionally needs a fifth state (`error`) that `index_queue`
-- does not: `dead` means "retries exhausted", but a policy-forbidden folder,
-- an empty-after-redaction body, or a model refusal are never going to
-- succeed on retry no matter how many attempts remain, and conflating those
-- with a job that is still worth reattempting would make `mail ai retry
-- --failed` — which targets `dead` — either miss them or wrongly resurrect
-- them.
--
-- `UNIQUE(message_id, pass)` is the acceptance criterion's dedup key
-- verbatim: a message queued twice for the same pass (triage re-enqueued by
-- a sync sweep while already pending, say) is not queued twice.
CREATE TABLE ai_queue (
    job_id           INTEGER PRIMARY KEY,
    message_id       INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    account_id       INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    -- Caller-supplied context tag, e.g. 'triage' | 'deep' — the same
    -- vocabulary `ai_ledger.pass` uses (task 45), not a fixed enum here
    -- either: a later pass (auto-tagging, task 57) is a new string, not a
    -- schema change.
    pass             TEXT NOT NULL,
    -- Lower runs first. A message the user just opened outranks the backlog
    -- walk, same convention as `index_queue.priority`.
    priority         INTEGER NOT NULL DEFAULT 100,
    -- pending | leased | done | error | dead
    state            TEXT NOT NULL DEFAULT 'pending',
    attempts         INTEGER NOT NULL DEFAULT 0,
    -- Set while leased. A worker that dies leaves this in the past, and the
    -- reaper returns the job to the queue. Batch-mode leases (see below) use
    -- a much longer TTL than a live per-request lease, because a Message
    -- Batches submission can legitimately take up to 24 hours to resolve.
    lease_expires_at INTEGER,
    leased_by        TEXT,
    -- Backoff: a failed job is invisible to `lease` until this passes.
    next_attempt_at  INTEGER NOT NULL DEFAULT 0,
    last_error       TEXT,
    -- Set once this job has been folded into a Message Batches API
    -- submission (`custom_id` = `message_id`). NULL means "not part of a
    -- batch" — the live per-request path. A batch's in-flight bookkeeping
    -- (the redacted payload and token map needed to audit and rehydrate its
    -- eventual result) lives in the coordinator's process memory, not here —
    -- see the module docs on why that is a deliberate consequence of the
    -- redaction firewall's "never persist a token map" rule, and what
    -- happens to a batch job whose coordinator process restarts before the
    -- batch ends.
    batch_id         TEXT,
    -- The audit ledger row this job's provider call was recorded under, once
    -- known. Never a FK target for deletion — `ai_ledger` rows are never
    -- deleted (V18's append-only triggers), so a plain REFERENCES is safe
    -- without an ON DELETE clause.
    ledger_entry_id  INTEGER REFERENCES ai_ledger(id),
    enqueued_at      INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at       INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE (message_id, pass)
) STRICT;

-- The lease query: ready jobs, best first. Column order mirrors
-- `idx_index_queue_ready`'s reasoning — the range predicate on
-- `next_attempt_at` goes last so the sort columns can still use the index.
CREATE INDEX idx_ai_queue_ready ON ai_queue(state, priority, enqueued_at, job_id);
-- The reaper's query: leased jobs whose lease has lapsed.
CREATE INDEX idx_ai_queue_lease ON ai_queue(state, lease_expires_at);
-- Looking up every job a batch submission covers, for the poll path.
CREATE INDEX idx_ai_queue_batch ON ai_queue(batch_id) WHERE batch_id IS NOT NULL;
