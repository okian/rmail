-- V35: the rules engine (task 66, prd.md #45/#46/#50).
--
-- prd.md, "Automation, Rules & Hooks":
--   TOML predicates mixing deterministic matchers (from/subject/header/
--   flags/size regex) with a `claude_is` NL predicate; actions
--   move/label/flag/archive/notify/run-hook/draft-reply; classification
--   cached by `message-id + prompt-hash`; evaluated on each new message. NL
--   rule synthesis + dry-run backtest with per-decision Claude explanations.
--
-- # Why the rule body is stored as TOML text, not as columns
--
-- A rule is a *document*: a predicate set of open-ended shape (any header
-- name, any number of regexes) plus an action block. Normalizing that into
-- columns would mean a table per predicate kind and a migration every time
-- the grammar grows one. It is also the exact artifact the user authors and
-- reads back -- `mail rule add` writes TOML, `ListRules` shows TOML, and
-- `SynthesizeRule` proposes TOML -- so any other storage form would be a
-- lossy round trip through a second representation nobody asked for. The
-- text is validated (and its regexes compiled, bounded, and rejected if they
-- do not compile) *before* it is ever written, so a row here is always a
-- document this build can parse; see `rmail_core::rules::model`.

CREATE TABLE rules (
    id INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    -- COLLATE NOCASE for the reason `saved_searches.name`/`tags.name` are:
    -- lookup by name and the uniqueness constraint below must agree on case,
    -- or creating "Newsletters" beside an existing "newsletters" silently
    -- succeeds while a lookup matches whichever row the planner reached first.
    name TEXT NOT NULL COLLATE NOCASE,
    -- The rule document, verbatim, as a single `[[rules]]` array-of-tables
    -- element. See this file's header for why it is text.
    toml TEXT NOT NULL,
    -- A disabled rule is still listed and still backtestable (that is the
    -- point of writing one before turning it on); the evaluator skips it.
    enabled INTEGER NOT NULL DEFAULT 1,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE(account_id, name)
) STRICT;

-- No separate `(account_id)` index: `UNIQUE(account_id, name)` already builds
-- one with account_id leftmost, which fully serves the only read shape this
-- table has (`WHERE account_id = ? ORDER BY name`).

-- The `claude_is` classification cache, keyed exactly as the acceptance
-- criterion words it: message-id + prompt-hash.
--
-- # Why the key is a hash of the *whole* prompt, not the predicate text
--
-- `prompt_hash` covers the natural-language predicate, the system prompt
-- version, the model id, AND the digest of the few-shot examples currently
-- recorded for that predicate (see `rule_examples` below). That last term is
-- load-bearing: a user correction only means anything if it changes future
-- classifications, and a cache keyed on the predicate text alone would keep
-- serving the pre-correction verdict forever. Folding the examples into the
-- hash makes a correction produce a *different* key -- so the next
-- evaluation misses, re-asks with the new few-shot context, and caches the
-- answer under the new key. The old row is left alone rather than deleted:
-- it is the honest record of what the model said under the old prompt, and
-- rolling a correction back re-uses it for free.
CREATE TABLE rule_classifications (
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    prompt_hash TEXT NOT NULL,
    -- 0/1. STRICT tables have no BOOLEAN, so this is the same INTEGER
    -- convention `messages.has_attachments` already uses.
    verdict INTEGER NOT NULL,
    -- The model's one-line justification, which `BacktestRule` reports per
    -- `claude_is` decision. Cached with the verdict rather than re-derived:
    -- re-asking for an explanation of a cached verdict would be a second
    -- paid call for a decision that has already been made.
    explanation TEXT NOT NULL,
    -- The model that produced it, denormalized from the prompt hash so an
    -- operator reading this table can see it without recomputing anything.
    model TEXT NOT NULL,
    -- The `ai_ledger` row this call was audited under, when it came from a
    -- real provider call. NULL for a verdict this build did not pay for.
    -- Deliberately NOT a foreign key: `ai_ledger` is append-only and pruned
    -- on its own schedule, and a cache entry outliving its ledger row is
    -- fine -- what is not fine is a ledger prune failing because a cache
    -- still points at it.
    ledger_entry_id INTEGER,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    PRIMARY KEY (message_id, prompt_hash)
) STRICT;

-- User corrections, which become few-shot examples on the next classification
-- of the same predicate (prd.md #50's "corrections become few-shot
-- examples").
CREATE TABLE rule_examples (
    id INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    -- The natural-language predicate this example teaches, stored verbatim
    -- rather than hashed: the hash includes the examples, so keying the
    -- examples by the hash would be circular.
    prompt TEXT NOT NULL,
    -- ON DELETE SET NULL, not CASCADE: the lesson ("mail that looks like
    -- *this* is not a cold pitch") outlives the message it was learned from,
    -- and `rendered` below is what the few-shot turn actually replays.
    message_id INTEGER REFERENCES messages(id) ON DELETE SET NULL,
    -- The message as it was rendered to the model, frozen at correction
    -- time. Frozen rather than re-rendered on each use so an example keeps
    -- teaching the same lesson after the message is deleted, moved, or has
    -- its body re-extracted by a later indexing change.
    rendered TEXT NOT NULL,
    -- What the user says the answer should have been. 0/1, as above.
    expected INTEGER NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    -- One correction per (account, predicate, message): correcting the same
    -- message twice replaces the earlier verdict rather than teaching the
    -- model both answers at once. NULL `message_id`s are distinct under
    -- SQLite's UNIQUE semantics, which is intended -- a hand-written example
    -- with no message behind it is not a duplicate of another.
    UNIQUE(account_id, prompt, message_id)
) STRICT;

-- The at-most-once ledger for rule actions.
--
-- A rule's actions are side effects with no natural idempotency key: a
-- draft-reply creates a *new* draft each time, a run-hook spawns a process,
-- a notify appends an event. `rule_actions_fired` is what makes "this rule
-- has already acted on this message" a fact rather than a guess, so the
-- background evaluator, a `EvaluateRules` RPC, and a second daemon tick
-- cannot each fire the same rule for the same message. The row is claimed
-- (INSERT OR IGNORE, then check whether it landed) *before* any action runs
-- -- see `rmail_core::rules`'s module docs for why at-most-once is the right
-- trade here and what it costs.
CREATE TABLE rule_actions_fired (
    rule_id INTEGER NOT NULL REFERENCES rules(id) ON DELETE CASCADE,
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    fired_at INTEGER NOT NULL DEFAULT (unixepoch()),
    PRIMARY KEY (rule_id, message_id)
) STRICT;

-- Deleting a message must not leave its claim rows behind; the FK above
-- handles that. The reverse direction (finding every claim for one message)
-- is not a read this engine performs, so no second index is warranted.
