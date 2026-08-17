-- V53: the autonomous inbox agent's run ledger, its action log, and snoozes.
--
-- prd.md #47 requires "every action logged with its reason". Two tables carry
-- that, and one carries the only action in the closed set that has no home
-- anywhere else in this schema.
--
-- # Why the action log denormalizes the message's identity
--
-- `crate::mail::MailStore::move_message` does not re-point
-- `messages.mailbox_id`; it issues the IMAP MOVE and then *deletes* the local
-- row, because the destination assigns a UID only its next sync can learn (see
-- that method's own docs). `archive` is exactly that call. So a log row keyed
-- to `messages(id) ON DELETE CASCADE` would erase itself the instant the
-- archive it records succeeded — the log would be missing precisely the
-- actions that worked, which is the failure mode "the log cannot omit
-- something that did happen" names.
--
-- Hence `ON DELETE SET NULL` plus a frozen copy of the RFC Message-ID, the
-- subject and the sender. The local id is a convenience for joining while the
-- row still exists; the frozen triple is what makes the entry readable a year
-- later, in another client, after the message has moved twice.
--
-- # Why the row is written before the mutation
--
-- `outcome` starts at 'attempted' for a live run and is updated to
-- 'applied'/'failed' once the mutation returns. The other order — act, then
-- record — leaves a crash window in which the mailbox changed and nothing says
-- so, and an unattended loop is exactly the caller that will hit it. The cost
-- is a row stuck at 'attempted' after a crash, which is an honest statement
-- ("we started this and do not know how it ended") rather than a lie in either
-- direction.
--
-- # A dry run writes nothing here
--
-- Not "writes rows marked dry-run": nothing. `agent.dry_run` is the default,
-- and the guarantee it makes is side-effect freedom, which a row in this table
-- would already violate. A dry run's plan is returned on the RPC and is gone
-- when the caller drops it. `agent_runs.dry_run` therefore only ever holds 0
-- today; the column exists so a future "record dry runs too" is a config
-- change rather than a migration, and the CHECK still admits both values.

CREATE TABLE agent_runs (
    id              INTEGER PRIMARY KEY,
    account_id      INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    -- 0 for every row this build writes; see the header.
    dry_run         INTEGER NOT NULL DEFAULT 0,
    -- The user policy the run was steering toward, frozen. A log that showed
    -- what the agent did without what it was told to do cannot be audited: the
    -- same archive is correct under one policy and wrong under another.
    policy          TEXT NOT NULL DEFAULT '',
    -- The mailbox the run walked.
    mailbox         TEXT NOT NULL DEFAULT '',
    started_at      INTEGER NOT NULL DEFAULT (unixepoch()),
    finished_at     INTEGER,
    -- Why the loop stopped. 'running' until it does, so a row left behind by a
    -- crashed daemon is distinguishable from one that finished.
    stop_reason     TEXT NOT NULL DEFAULT 'running',
    -- The three bounds, as counted. Recorded rather than derived from
    -- `agent_actions`, because an iteration that produced no action still
    -- spent a model call and still counts against the cap.
    iterations      INTEGER NOT NULL DEFAULT 0,
    model_calls     INTEGER NOT NULL DEFAULT 0,
    actions_applied INTEGER NOT NULL DEFAULT 0,
    CHECK (dry_run IN (0, 1)),
    CHECK (stop_reason IN (
        'running', 'completed', 'iteration_cap', 'action_cap', 'deadline',
        'cancelled', 'error'
    )),
    CHECK (iterations >= 0),
    CHECK (model_calls >= 0),
    CHECK (actions_applied >= 0)
) STRICT;

-- "What did the agent do lately", the read `GetAgentRunLog` makes.
CREATE INDEX idx_agent_runs_account ON agent_runs(account_id, id DESC);

CREATE TABLE agent_actions (
    id             INTEGER PRIMARY KEY,
    run_id         INTEGER NOT NULL REFERENCES agent_runs(id) ON DELETE CASCADE,
    -- NULL once the message is gone; see the header on why not CASCADE.
    message_id     INTEGER REFERENCES messages(id) ON DELETE SET NULL,
    rfc_message_id TEXT NOT NULL DEFAULT '',
    subject        TEXT NOT NULL DEFAULT '',
    sender         TEXT NOT NULL DEFAULT '',
    -- The closed vocabulary, and nothing else. A string the model wrote that
    -- is not one of these never reaches this table: it is refused at parse
    -- time and recorded as 'none' with outcome 'refused'.
    action         TEXT NOT NULL,
    -- The action's one parameter, already validated against its own allowlist
    -- (a label the operator configured, a bounded snooze). Never a mailbox
    -- name, a command, or anything else the model chose freely.
    argument       TEXT NOT NULL DEFAULT '',
    -- The model's stated reason, sanitized. Required and non-empty: an action
    -- with no reason is not auditable, and prd.md #47 asks for the reason by
    -- name.
    reason         TEXT NOT NULL,
    outcome        TEXT NOT NULL,
    detail         TEXT NOT NULL DEFAULT '',
    decided_at     INTEGER NOT NULL DEFAULT (unixepoch()),
    CHECK (action IN ('archive', 'label', 'snooze', 'draft_reply', 'escalate', 'none')),
    CHECK (outcome IN ('attempted', 'applied', 'failed', 'withheld', 'refused', 'planned'))
) STRICT;

CREATE INDEX idx_agent_actions_run ON agent_actions(run_id, id);
CREATE INDEX idx_agent_actions_message ON agent_actions(message_id);

-- The one action in the closed set with nowhere else to live.
--
-- Snooze is deliberately local and deliberately modest: it records "the agent
-- should not reconsider this until `until`" and issues no IMAP command at all.
-- `crate::agent::store::candidates` is what reads it, and the same action also
-- applies `agent.snooze_tag` so the state is visible in the tag surfaces a
-- human already uses.
--
-- It does *not* remove the message from any listing. `MailStore::list` joins no
-- snooze table, and teaching it to would change the meaning of every listing in
-- the product on behalf of one model-chosen action. Every server-side
-- alternative (move to a Snoozed folder, strip \Seen, delete-and-reappend) is a
-- mutation that a wrong verdict makes expensive to undo — and this action is
-- chosen by a model reading attacker-authored text.
CREATE TABLE message_snoozes (
    message_id INTEGER PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
    until      INTEGER NOT NULL,
    reason     TEXT NOT NULL DEFAULT '',
    snoozed_at INTEGER NOT NULL DEFAULT (unixepoch())
) STRICT;

CREATE INDEX idx_message_snoozes_until ON message_snoozes(until);
