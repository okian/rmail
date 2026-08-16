-- V40: the priority notification engine's durable decision log (task 81,
-- prd.md #62 "AI Priority Notification Engine").
--
-- This table is not a cache of what the model said; it is the record of what
-- this machine *did about it*, and that distinction is the whole reason it
-- exists separately from `ai_summaries` (V21). A summary can be recomputed at
-- will -- re-triaging a message overwrites its own row and nothing outside
-- the database can tell. A notification cannot: once it has been delivered, a
-- human has been interrupted, and no amount of re-running the pipeline can
-- take that back. So the fact of delivery has to survive a crash, a restart,
-- a re-lease of the scoring job, and a re-enqueue of the whole pass.
--
-- `UNIQUE (message_id)` is that guarantee, and it is deliberately *not*
-- `(message_id, model)` the way `ai_summaries` is keyed. Scoring the same
-- message under a second model is a legitimate second opinion; notifying
-- about the same message twice because the operator changed
-- `ai.models.notify` is a bug the user experiences as a duplicate ping. One
-- message, one notification decision, forever.
--
-- # The state machine
--
--   pending    -- scored, not yet acted on. The only state a delivery attempt
--                 may start from.
--   delivered  -- the channel accepted it. Terminal.
--   suppressed -- deliberately not delivered (below threshold, or the
--                 account has notifications off). Terminal;
--                 `suppressed_reason` says which.
--   failed     -- the channel refused it, or kept crashing the delivery loop,
--                 until its attempts ran out. Terminal, and distinct from
--                 `suppressed` on purpose: "we chose not to" and "we could
--                 not" are different operational facts, and collapsing them
--                 would make a broken notifier look like a quiet mailbox.
--
-- There is deliberately no `stale` state. A message too old to be worth
-- interrupting anyone about never reaches this table at all: the scoring pass
-- declines it before the model call (see `notify::score`'s own docs on
-- `notify.max_message_age`), so an operator switching the feature on does not
-- pay to score a week of already-read mail, let alone get pinged about it.
--
-- Quiet hours are deliberately *not* a state. A message that arrives at 03:00
-- stays `pending` with `next_attempt_at` set to the end of the window, so it
-- is delivered when the window closes rather than dropped -- an important
-- message that arrived overnight is still important at breakfast. That is
-- also why `next_attempt_at` is shared between the quiet-hours defer and the
-- delivery-failure backoff: both mean "not before this instant", and one
-- column with one index answers both.
CREATE TABLE notifications (
    id                INTEGER PRIMARY KEY,
    message_id        INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    account_id        INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    -- The importance tier the model returned, from the fixed vocabulary in
    -- `rmail_core::notify::Tier` (`low` < `normal` < `high` < `critical` --
    -- the same ladder `ai_summaries.priority` uses, imported rather than
    -- copied). Stored as its wire string, like every other enum in this
    -- schema.
    tier              TEXT NOT NULL,
    -- The model's one-line justification. Bounded at write time by
    -- `notify::score`; this column is what a notification body and
    -- `StreamAlerts` show, so it is the one place model prose is retained.
    reason            TEXT NOT NULL,
    model             TEXT NOT NULL,
    -- The audit-ledger row the scoring call was recorded under, so a
    -- notification traces back to the call that produced it -- the same
    -- linkage `ai_summaries.ledger_entry_id` establishes, and the same
    -- no-ON-DELETE reasoning (V18's ledger rows are never deleted).
    ledger_entry_id   INTEGER REFERENCES ai_ledger(id),
    -- 'pending' | 'delivered' | 'suppressed' | 'failed' -- see the header.
    state             TEXT NOT NULL,
    -- Why a `suppressed` row was suppressed ('below_threshold',
    -- 'notifications_disabled', ...). NULL in every other state.
    suppressed_reason TEXT,
    -- Delivery attempts made so far. Only ever incremented by a delivery
    -- attempt, never by a quiet-hours defer: a message held until 07:00 has
    -- not burned a retry.
    attempts          INTEGER NOT NULL DEFAULT 0,
    -- Unix seconds before which this row must not be attempted. NULL means
    -- "ready now". See the header on why quiet hours and backoff share it.
    next_attempt_at   INTEGER,
    scored_at         INTEGER NOT NULL DEFAULT (unixepoch()),
    -- When this row reached a terminal state. NULL while pending.
    decided_at        INTEGER,
    UNIQUE (message_id)
) STRICT;

-- The delivery loop's only query: "pending rows that are due, oldest first".
--
-- Partial on `state = 'pending'` because every other state is terminal and
-- would otherwise dominate the index within a day of normal use -- a mailbox
-- that notifies on 1% of its mail leaves 99% of these rows permanently
-- `suppressed`, and an index that carried them would be almost entirely dead
-- weight the planner still has to page through.
--
-- Keyed on `id` alone, not on `(next_attempt_at, id)`. The query orders by
-- `id` (so a claim is FIFO and a lease that lapses keeps its place), and an
-- index led by `next_attempt_at` cannot serve that order -- SQLite would have
-- to materialize and sort, so it declines the index and scans the table
-- instead. Scanning this index in `id` order and filtering `next_attempt_at`
-- per row is the plan that actually runs, and it is over the pending minority
-- rather than the whole table.
--
-- A partial index only applies when the query's own WHERE clause is provably
-- implied by the index's, and SQLite performs that proof over the statement
-- text. `notify::repo` therefore spells `state = 'pending'` / `'delivered'`
-- inline rather than binding them, so applicability never rests on how a
-- particular SQLite version treats a parameter it cannot see the value of.
-- `notify::tests::the_claim_and_alert_queries_use_their_partial_indexes`
-- asserts the resulting plan directly -- this is the one property here with no
-- behavioural symptom, so nothing else would notice it regressing.
CREATE INDEX idx_notifications_due
    ON notifications(id)
    WHERE state = 'pending';

-- `StreamAlerts` resumes from a cursor and only ever reports rows that were
-- actually delivered, so it seeks by `id` within the delivered set. Partial
-- for the mirror image of the reason above -- this index carries only the
-- small delivered minority -- and read with the same inline-literal
-- discipline.
CREATE INDEX idx_notifications_delivered
    ON notifications(id)
    WHERE state = 'delivered';
