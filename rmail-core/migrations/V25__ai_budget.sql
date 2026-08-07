-- Per-account and global AI spend budgets, and the ledger column that says
-- which budget a call was charged to (task 76).
--
-- There is deliberately no `ai_spend` table here. Spend is *derived* from
-- `ai_ledger` — the append-only audit trail (V18) — never accumulated in a
-- second place. A parallel counter would be one dropped write, one crash
-- between two transactions, or one forgotten call site away from disagreeing
-- with the audit trail about how much was spent, and the audit trail is the
-- one that has to be right. The cost of deriving is a `SUM` over one calendar
-- month of an indexed range scan per dispatch, which is a rounding error
-- against a network call to a model provider.

-- Which budget a ledger row is charged to.
--
-- 'interactive' is the default for two reasons. First, every row that existed
-- before this migration was written by the live dispatch path or by a forced
-- `AnalyzeMessage`/`SuggestReply`, all of which are interactive by
-- definition — so backfilling them as interactive is accurate, not merely
-- convenient. Second, a future call site that forgets to classify itself is
-- charged against the ordinary caps rather than silently escaping every cap:
-- under-attributing to `bulk` under-consumes the bulk sub-budget, which is
-- conservative, where a NULL-defaulting column would have to be treated as
-- "unclassified" and either ignored (fail-open on the global cap) or guessed.
--
-- ALTER TABLE ADD COLUMN is DDL, not an UPDATE, so V18's `ai_ledger_no_update`
-- trigger does not fire on it; the NOT NULL DEFAULT means SQLite does not
-- rewrite existing rows either.
ALTER TABLE ai_ledger ADD COLUMN work_class TEXT NOT NULL DEFAULT 'interactive';

-- The budget enforcer's only two query shapes: a per-account window scan
-- (`WHERE account_id = ? AND created_at >= ?`) and a global one, which the
-- existing `idx_ai_ledger_created_at` already serves. `idx_ai_ledger_account`
-- (V18) covers only `account_id`, so a per-account month scan would have to
-- visit every row that account ever produced; the composite lets SQLite seek
-- straight to the window.
CREATE INDEX idx_ai_ledger_account_created ON ai_ledger(account_id, created_at);

-- Explicit caps set by `AiPolicyService.SetBudget` / `mail ai budget set`.
--
-- A row here is an *override*. With no row at all the enforcer still applies
-- the global caps configured under `ai.limits` (and the bulk sub-budget it
-- derives from them), so an operator who never calls SetBudget is still
-- bounded — see `rmail_core::ai::budget`'s module docs on cap resolution.
CREATE TABLE ai_budgets (
    -- 0 = the global budget (every call counts toward it, whatever account it
    -- was made for); otherwise `accounts.id`. A sentinel rather than a NULL
    -- because this is half the primary key, and SQLite treats NULLs in a
    -- UNIQUE index as distinct from each other — a nullable column would
    -- happily hold two conflicting "global" rows. `accounts.id` is an
    -- autoincrementing INTEGER PRIMARY KEY that starts at 1 and is never
    -- written explicitly, so 0 can never collide with a real account.
    account_id INTEGER NOT NULL CHECK (account_id >= 0),

    -- 'all'  — every call in this scope counts against it.
    -- 'bulk' — only calls charged as bulk work count against it. A bulk call
    --          is checked against *both* rows: exhausting the bulk sub-budget
    --          stops bulk work without touching what interactive work may
    --          still spend under 'all'.
    class TEXT NOT NULL CHECK (class IN ('all', 'bulk')),

    -- Caps. NULL means "no cap on this dimension" — not zero. Dollars are
    -- integer micro-dollars (1e-6 USD): a cap is a number a human typed and
    -- must round-trip and compare exactly, which a float does not (`5.00`
    -- is not representable in binary floating point, so `spend >= cap` can
    -- flip on the seventeenth decimal). `ai_ledger.cost_usd` stays REAL — it
    -- is the historical record this migration must not rewrite — and is
    -- converted to micro-dollars once, at the comparison boundary.
    daily_soft_usd_micros INTEGER CHECK (daily_soft_usd_micros IS NULL OR daily_soft_usd_micros >= 0),
    daily_hard_usd_micros INTEGER CHECK (daily_hard_usd_micros IS NULL OR daily_hard_usd_micros >= 0),
    daily_soft_tokens INTEGER CHECK (daily_soft_tokens IS NULL OR daily_soft_tokens >= 0),
    daily_hard_tokens INTEGER CHECK (daily_hard_tokens IS NULL OR daily_hard_tokens >= 0),
    monthly_soft_usd_micros INTEGER CHECK (monthly_soft_usd_micros IS NULL OR monthly_soft_usd_micros >= 0),
    monthly_hard_usd_micros INTEGER CHECK (monthly_hard_usd_micros IS NULL OR monthly_hard_usd_micros >= 0),
    monthly_soft_tokens INTEGER CHECK (monthly_soft_tokens IS NULL OR monthly_soft_tokens >= 0),
    monthly_hard_tokens INTEGER CHECK (monthly_hard_tokens IS NULL OR monthly_hard_tokens >= 0),

    -- Unix seconds; when this row was last written.
    updated_at INTEGER NOT NULL,

    PRIMARY KEY (account_id, class)
);

-- `accounts.id` is a rowid alias, so SQLite reuses the id of a deleted
-- account for the next one created. Without this, a budget set for account 3
-- would silently reappear as the budget of a completely different account 3
-- later — a stale spend cap applied to mail it was never written for. There
-- is no foreign key because `account_id = 0` is the global sentinel and has
-- no `accounts` row to reference; a trigger expresses the same cleanup
-- without needing one.
CREATE TRIGGER ai_budgets_follow_account_delete
AFTER DELETE ON accounts
BEGIN
    DELETE FROM ai_budgets WHERE account_id = OLD.id;
END;
