-- V45: draft revisions -- the cyclable, revertible history a tone/length
-- rewrite produces (task 62, prd.md #19).
--
-- `RewriteDraft` is not an edit. prd.md asks for "a cyclable, revertible
-- revision", which is a statement about *history*: after three rewrites a
-- user must be able to walk back through what the draft said before each one
-- and land on any of them, including the text they typed themselves. A patch
-- applied in place can express none of that -- `UpdateDraft` already does
-- that job and deliberately keeps no history -- so the revisions live here,
-- beside the draft rather than on it.
--
-- # Why a table and not columns on `drafts`
--
-- The relationship is one-to-many and unbounded in principle (bounded in
-- practice by `compose::reply::MAX_REVISIONS`, a constant rather than a config
-- key: it is a bound on what a person can hold in their head while cycling,
-- not an operational knob), which no fixed set of columns can hold. It is also
-- strictly derived state: dropping every row here leaves a
-- perfectly good draft, which is what makes `ON DELETE CASCADE` from `drafts`
-- the right and only lifecycle -- a revision of a draft that no longer exists
-- is not a document anybody can open.
--
-- # `active` is a pointer, not a copy
--
-- Exactly one row per draft is `active`, enforced by a partial unique index
-- rather than by application code, and it names the revision whose text the
-- draft *currently* holds. The invariant `crate::compose::reply` maintains on
-- top of that is the one that makes cycling non-destructive: before switching
-- away from a revision, the draft's live body is written back into it. A
-- user who rewrites, hand-edits, then cycles back therefore finds their hand
-- edits still on the revision they made them on, rather than discovering that
-- "cycle" quietly meant "discard".
--
-- Seq 0 is always the pre-rewrite original, captured on the first rewrite.
-- Reverting is `SelectDraftRevision(seq = 0)` -- not a distinct operation,
-- because a revert that took a different code path from a cycle would be a
-- second chance to get the write-back above wrong.
CREATE TABLE draft_revisions (
    id         INTEGER PRIMARY KEY,
    draft_id   INTEGER NOT NULL REFERENCES drafts(id) ON DELETE CASCADE,
    -- 0 = the original text, 1.. = each rewrite in the order it was made.
    -- Dense and monotonic: it is what a client cycles through, and a gap
    -- would read as a lost revision.
    seq        INTEGER NOT NULL,
    -- How this revision came to be: "original", or the rewrite instruction
    -- that produced it ("formal", "warmer, shorter"). Shown in a picker, so
    -- it is stored rather than re-derived from a tone enum that may grow.
    label      TEXT NOT NULL,
    subject    TEXT NOT NULL,
    body_text  TEXT NOT NULL,
    -- The model that wrote it; NULL for seq 0, which no model wrote.
    model      TEXT,
    active     INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE (draft_id, seq)
) STRICT;

-- One active revision per draft, enforced by the schema. The alternative --
-- "the application always clears the old one first" -- is a rule that holds
-- until the first path that forgets, and the failure mode is a draft that
-- cycles to two different bodies depending on row order.
CREATE UNIQUE INDEX idx_draft_revisions_active
    ON draft_revisions(draft_id) WHERE active = 1;
