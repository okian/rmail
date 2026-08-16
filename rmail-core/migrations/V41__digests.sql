-- V41: the AI periodic digest's durable record (task 70, prd.md feature 57).
--
-- Like `notifications` (V40) and unlike `ai_summaries` (V21), this table is
-- not a cache of what a model said about a message. It is the record of a
-- *period having been briefed on*, and the difference is what the schema is
-- shaped around: re-triaging a message overwrites its own row and nobody
-- outside the database can tell, but re-generating a digest costs a second
-- Sonnet call over the same mail and hands the reader a second, subtly
-- different briefing about a week they have already read about.
--
-- `UNIQUE (account_id, period_start, period_end)` is that guarantee. One
-- window, one stored briefing, whoever asked for it -- the scheduled job, the
-- RPC and `mail digest` all resolve to a window first and then to this key, so
-- a daemon that restarts mid-tick, ticks twice inside one period, or races a
-- manual `mail digest` for the same window ends up with one row. The only way
-- to get a second briefing for a window is to ask for one explicitly
-- (`force`), which replaces the row rather than accumulating beside it.
--
-- What this key does *not* do is deduplicate the model call. `generate` is
-- check-then-act (look the window up, call, store), so two genuinely
-- concurrent requests for the same window can both miss the lookup and both
-- pay. The constraint keeps the *record* single; it does not make the spend
-- single. That is acceptable because the only unattended caller is one
-- scheduler ticking serially in one process -- a concurrent duplicate needs
-- two operators asking for the same window in the same few seconds -- and
-- closing it properly means reserving the row before the call and reasoning
-- about how a crashed reservation is released, which is a worse trade than
-- the duplicate it prevents.
--
-- # Why `account_id` is 0 rather than NULL for "every account"
--
-- SQLite treats NULLs as distinct in a UNIQUE index, so `(NULL, start, end)`
-- would collide with nothing -- the one scope the scheduler actually uses
-- would be the one scope with no uniqueness at all. 0 is the same sentinel
-- `ai_budget`'s `GLOBAL_ACCOUNT_ID` already uses for "not one account's", so
-- this is that convention rather than a new one. It is deliberately *not* a
-- foreign key for the same reason: 0 is not a row in `accounts`.
--
-- # Why the period is a pair of columns and not one id
--
-- A digest is identified by the window it covers, and both bounds are needed
-- to say so: the "did we already brief this" lookup is on the pair, and a
-- reader rendering the briefing has to name the span.
--
-- `interval_seconds` records which cadence produced a row, 0 for an ad-hoc
-- `mail digest --since ...`, and it is *not* merely reporting: the scheduler's
-- cursor reads `MAX(period_end) WHERE interval_seconds > 0 AND period_end <=
-- now`, so an ad-hoc briefing can neither advance the timer past a period it
-- was about to brief (an ad-hoc window ends at `now`, i.e. inside the period
-- in progress) nor park it in the future (nothing stops a caller naming a
-- window years ahead). See `digest::repo::latest_period_end`, which argues
-- both halves at length.
CREATE TABLE digests (
    id               INTEGER PRIMARY KEY,
    -- 0 = every configured account. See the header.
    account_id       INTEGER NOT NULL,
    -- Half-open window in unix seconds: `period_start <= t < period_end`.
    period_start     INTEGER NOT NULL,
    period_end       INTEGER NOT NULL,
    -- The cadence this row came from, 0 for an ad-hoc request.
    interval_seconds INTEGER NOT NULL DEFAULT 0,
    generated_at     INTEGER NOT NULL DEFAULT (unixepoch()),
    -- The model that actually wrote the briefing, after any budget downgrade.
    -- Empty string when no model was called at all, which is exactly the
    -- empty-window case: a period with no mail in it gets a locally-authored
    -- "nothing arrived" briefing and is recorded, so the scheduler does not
    -- re-ask about it every tick for the rest of the retention window.
    model            TEXT NOT NULL,
    -- The rendered briefing. Engine-authored markdown: the section headings,
    -- their order and every citation marker are written by
    -- `rmail_core::digest::briefing`, and only the prose inside a bullet comes
    -- from the model. A bullet that cited no retrievable message never
    -- reaches this column.
    markdown         TEXT NOT NULL,
    -- How many messages this briefing put forward -- the window's mail after
    -- clustering has ranked it and `digest.max_messages`/`max_clusters` have
    -- cut it, i.e. before the policy gate and the token budget cut it again.
    -- Reported so a thin briefing can be told from a quiet week. Note it is
    -- therefore *not* the size of the window: a busier window than
    -- `max_messages` (or than `digest`'s own 5,000-row scan bound) reports the
    -- bound, not the truth, and the two are indistinguishable here.
    considered       INTEGER NOT NULL DEFAULT 0,
    packed           INTEGER NOT NULL DEFAULT 0,
    withheld         INTEGER NOT NULL DEFAULT 0,
    clusters         INTEGER NOT NULL DEFAULT 0,
    -- Bullets dropped because they cited nothing this daemon had retrieved.
    dropped_uncited  INTEGER NOT NULL DEFAULT 0,
    -- The audit-ledger row the briefing call was recorded under, so a digest
    -- traces back to the call that produced it -- the same linkage
    -- `ai_summaries.ledger_entry_id` and `notifications.ledger_entry_id`
    -- establish, and the same no-ON-DELETE reasoning (V18's ledger rows are
    -- never deleted). NULL for an empty window, which made no call.
    ledger_entry_id  INTEGER REFERENCES ai_ledger(id),
    UNIQUE (account_id, period_start, period_end)
) STRICT;

-- The scheduler's cursor is `SELECT MAX(period_end) ... WHERE account_id = ?`,
-- and the reuse lookup is on the UNIQUE index above. `(account_id,
-- period_end)` is what serves the first; the second already has an index.
CREATE INDEX idx_digests_cursor ON digests(account_id, period_end);

-- The sources a briefing was built from, and therefore the only messages any
-- of its lines can possibly point at.
--
-- Stored rather than re-derived because a digest is a historical document: the
-- window it covered is fixed, and re-running the selection a month later would
-- answer over a mailbox that has since been archived, expunged and re-synced.
--
-- `message_id` carries no foreign key on purpose, which is the one place this
-- table departs from `notifications`. A notification is about a message that
-- must still exist for it to mean anything; a briefing sentence about an
-- invoice that has since been deleted is still a true statement about that
-- week, and `ON DELETE CASCADE` here would silently rewrite history by
-- emptying a stored briefing's source list. `cited` records whether the
-- briefing's prose actually pointed at this source, so "packed but unused" and
-- "used" stay distinguishable.
CREATE TABLE digest_sources (
    digest_id   INTEGER NOT NULL REFERENCES digests(id) ON DELETE CASCADE,
    -- The 1-based label the prompt gave this source and the briefing cites.
    label       INTEGER NOT NULL,
    message_id  INTEGER NOT NULL,
    message_uid INTEGER NOT NULL,
    account_id  INTEGER NOT NULL,
    mailbox     TEXT NOT NULL,
    subject     TEXT NOT NULL,
    from_addr   TEXT NOT NULL,
    date        INTEGER,
    cited       INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (digest_id, label)
) STRICT;
