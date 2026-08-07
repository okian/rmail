-- V22: AI deep-pass entity/date/amount extraction (task 49).
--
-- `prd.md`'s AI data model already specifies an `ai_entities` table for
-- exactly this -- dates, amounts, people and organizations the deep pass
-- names -- but no migration up through V21 created it: V21 (task 48)
-- deliberately scoped itself to the triage pass's own columns on
-- `ai_summaries` and left this for the deep pass that actually produces the
-- values. No task before this one reserved a migration number for it, so
-- this uses the next one available (V21 was the last applied).
--
-- One row per (message, entity), scoped to the model that extracted it --
-- the same per-model coexistence `ai_summaries` (V21) established for
-- triage/deep verdicts, applied here too: re-running the deep pass under a
-- different model must not erase what an earlier model already found for
-- the same message. `rmail-core/src/ai/deep.rs` keeps one model's set an
-- atomic replacement of that same model's prior set -- `DELETE ... WHERE
-- message_id = ? AND model = ?` immediately followed by a bulk insert in
-- the same write transaction -- without touching a different model's rows.
CREATE TABLE ai_entities (
    id          INTEGER PRIMARY KEY,
    message_id  INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    model       TEXT NOT NULL,
    -- 'date' | 'amount' | 'person' | 'organization' | 'other' -- the deep
    -- pass's own vocabulary (`rmail-core/src/ai/deep.rs`'s `ENTITY_KINDS`),
    -- open the same way `ai_summaries.pass` and `ai_queue.pass` are: a
    -- later task can add a kind without a migration to widen a CHECK
    -- constraint.
    kind        TEXT NOT NULL,
    -- As written in the message -- the un-normalized text a citation or a
    -- highlight would want, never discarded even when `iso`/`amount` below
    -- also hold a normalized reading of it.
    value       TEXT NOT NULL,
    -- Normalized date (ISO-8601), set only when `kind = 'date'`.
    iso         TEXT,
    -- Normalized amount, set only when `kind = 'amount'`.
    amount      REAL,
    -- ISO 4217 code, set only when `kind = 'amount'` and a currency was
    -- identifiable.
    currency    TEXT,
    created_at  INTEGER NOT NULL DEFAULT (unixepoch())
) STRICT;

-- Every read of this table is scoped to one message (`ai_entities.message_id
-- = messages.id` from a search hit, a summary view, or the delete-then-
-- insert rewrite above) -- never a bare scan across the whole table -- so
-- this is the one index that access pattern ever chooses, the same
-- reasoning `ai_summaries`' own `UNIQUE(message_id, pass, model)` docs (V21)
-- give for not adding further indexes speculatively.
CREATE INDEX idx_ai_entities_message ON ai_entities(message_id);
