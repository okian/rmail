-- V52: which inference backend an account's AI calls use (task 78).
--
-- The daemon-wide default is `ai.provider` in the config file. A row here is
-- an *override* for one account — `AiPolicyService.SetAiProvider` /
-- `mail ai provider set <account> local` — so an operator can move one
-- account's mail on-device without restarting the daemon or editing TOML, and
-- without moving every other account with it.
--
-- This table only ever chooses between backends that are *permitted*. It is
-- read after `ai.policy` has resolved, and it cannot widen that resolution: a
-- `local_only` (or `forbidden`) folder stays local (or is refused) whatever
-- this row says. See `rmail_core::ai::local::resolve_egress`, which is the one
-- function that combines the two, and its tests.
CREATE TABLE ai_provider_overrides (
    -- 0 = the daemon-wide override (applies to every account with no row of
    -- its own); otherwise `accounts.id`. A sentinel rather than a NULL for the
    -- reason `ai_budgets.account_id` (V25) documents: this is the primary key,
    -- and SQLite treats NULLs in a unique index as distinct from each other,
    -- so a nullable column would happily hold two conflicting "global" rows.
    -- `accounts.id` is an autoincrementing INTEGER PRIMARY KEY that starts at
    -- 1 and is never written explicitly, so 0 can never collide with one.
    account_id INTEGER NOT NULL PRIMARY KEY CHECK (account_id >= 0),

    -- 'claude' — the hosted Messages API (this is the only value that can
    --            ever leave the machine).
    -- 'local'  — fully on-device inference (`ai.local`).
    --
    -- Constrained here, not only in Rust, because a value this column cannot
    -- represent is a routing decision nobody can make: an unrecognized backend
    -- read back out of the database would have to be resolved to *something*,
    -- and every choice is wrong (guessing 'claude' silently un-does an
    -- operator's local-only intent; guessing 'local' silently disables AI on a
    -- typo). Making it unstorable removes the question.
    provider TEXT NOT NULL CHECK (provider IN ('claude', 'local')),

    -- Unix seconds; when this row was last written.
    updated_at INTEGER NOT NULL
);

-- `accounts.id` is a rowid alias, so SQLite reuses a deleted account's id for
-- the next account created. Without this, an override set for account 3 would
-- silently become the routing of a completely different account 3 later —
-- which, in the direction that matters, means mail an operator never marked
-- local-only quietly inheriting someone else's on-device routing, or worse,
-- a 'claude' override outliving the account it was scoped to. There is no
-- foreign key because `account_id = 0` is the global sentinel and has no
-- `accounts` row to reference; a trigger expresses the same cleanup without
-- needing one.
CREATE TRIGGER ai_provider_overrides_follow_account_delete
AFTER DELETE ON accounts
BEGIN
    DELETE FROM ai_provider_overrides WHERE account_id = OLD.id;
END;
