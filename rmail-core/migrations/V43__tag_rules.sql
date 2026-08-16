-- V43: `tag_rules` -- the auto-apply policy AI tag suggestions are judged
-- against (task 57, prd.md III-4's data model and its "Claude Integration"
-- section).
--
-- Task 55 shipped the *suggestion* half of auto-tagging: `message_tags` rows
-- with `source = 'ai'`, `state = 'pending'`, a confidence and a rationale, and
-- the accept/reject transitions over them. What it deliberately left out was
-- the question of when a suggestion should skip the pending state entirely and
-- just be applied. prd.md answers that with this table: a rule names a tag, a
-- mode (`suggest` keeps everything pending; `auto` lets a confident suggestion
-- apply itself), and the confidence floor that applies to it.
--
-- # Why a table and not three more `[tags.ai]` config keys
--
-- The threshold is per *tag*, not per mailbox: "auto-apply `newsletter` at
-- 0.7 but never auto-apply `finance/invoice` below 0.95" is the whole point,
-- and it is a decision a person makes and revises as they see what the
-- classifier does, one tag at a time -- not something they want to restart a
-- daemon to change. `tag_id` is a real foreign key with `ON DELETE CASCADE`
-- so deleting a tag cannot leave a rule pointing at nothing.
--
-- # `query` and `ai_prompt` are columns this task writes but does not consume
--
-- prd.md's sketch gives a rule two ways to select mail: a deterministic
-- `query` (apply immediately, no model involved) and an `ai_prompt` (a
-- per-rule instruction scored by the model). Task 57 implements neither
-- selector -- its classifier scores a message against the whole
-- `tags.ai.taxonomy` in one call, and `rmail_core::tags::ai` reads only
-- `tag_id`/`mode`/`min_conf`/`enabled` off these rows. Both columns are
-- declared here anyway, nullable, because they are part of the row's
-- documented shape and adding a column to a table an operator already has
-- rules in is a migration; leaving room for the selector half costs nothing
-- now. Nothing reads them yet: a rule with a `query` set today behaves
-- exactly like one without.
--
-- # `min_conf` has a CHECK, `mode` has a CHECK, `enabled` does not
--
-- The two that constrain behaviour are constrained in the schema, the same
-- discipline V24 applies to `sync_mode`/`source`/`state`: a `min_conf` outside
-- `0.0..=1.0` or a `mode` outside the closed vocabulary is an operator
-- mistake that must fail at the write, not silently make a rule that can
-- never fire (or one that fires on everything). `enabled` is a plain
-- INTEGER-as-boolean like every other flag in this schema.
CREATE TABLE tag_rules (
    id INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    -- Human label, for `mail rules list`. Unique per account so a rule can be
    -- named in a CLI verb without an id.
    name TEXT NOT NULL,
    -- Reserved for the deterministic selector -- see the header. NULL today
    -- for every rule this codebase writes.
    query TEXT,
    -- Reserved for the per-rule AI selector -- see the header.
    ai_prompt TEXT,
    tag_id INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
    -- 'suggest': every suggestion for this tag stays pending, whatever its
    -- confidence. 'auto': a suggestion at or above `min_conf` (and above the
    -- global `tags.ai.auto_apply_min_confidence` ceiling -- see
    -- `rmail_core::tags::ai::AutoApplyPolicy`) is applied outright, with
    -- `source = 'ai'` so it stays distinguishable from a hand-applied tag and
    -- reversible with an ordinary `mail untag`.
    mode TEXT NOT NULL DEFAULT 'suggest' CHECK (mode IN ('suggest', 'auto')),
    min_conf REAL NOT NULL DEFAULT 0.75 CHECK (min_conf >= 0.0 AND min_conf <= 1.0),
    enabled INTEGER NOT NULL DEFAULT 1,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE(account_id, name)
);

-- The one read path: "every enabled rule for this account", resolved once per
-- suggestion batch and matched against the tags the model named.
CREATE INDEX idx_tag_rules_account ON tag_rules(account_id) WHERE enabled = 1;

-- # One documented widening of V24's `confidence`/`rationale` comment
--
-- V24 says those two columns are "NULL for source='user'/'rule'/'imap'",
-- written when `'rule'` could only mean a deterministic query match with no
-- number behind it. An auto-applied AI suggestion is a `tag_rules` match too
-- -- so it is written `source = 'rule'` -- but it *does* have a confidence and
-- a rationale, and they are the two facts a person needs when they ask why a
-- tag appeared on their mail unprompted. Both are therefore populated on that
-- one kind of row. Nothing ever constrained them (no CHECK, no NOT NULL), so
-- this widens a comment, not a constraint, and V24 itself is left untouched --
-- editing an applied migration would change its refinery checksum and break
-- every existing database.
--
-- One consequence worth stating plainly: `source = 'rule'` now covers two
-- different appliers -- a deterministic match (`rules::actions`,
-- `smart_folder`, confidence NULL) and a confident AI suggestion promoted by a
-- rule in this table (confidence set). `source` alone therefore no longer
-- distinguishes them; `confidence IS NOT NULL` does. That is a deliberate
-- trade: both really are "a rule applied this", the distinction a person cares
-- about is user-versus-automatic (which `source` still answers), and splitting
-- the vocabulary would mean a new `message_tags.source` value and a CHECK
-- change in V24, which cannot be edited without breaking every existing
-- database's refinery checksum.
--
-- Auto-applied rows are `source = 'rule'` rather than `source = 'ai'` for a
-- second, sharper reason: it is what keeps the learning signal below honest.
-- `source = 'ai'` rows in a terminal state are exactly the suggestions a
-- *person* ruled on (`applied` = accepted, `rejected` = rejected). If an
-- auto-application were recorded as `source = 'ai', state = 'applied'` it
-- would be counted as an acceptance nobody made, and every auto-apply would
-- raise that tag's own accept rate -- a classifier grading its own homework.
--
-- The accept/reject learning signal (`rmail_core::tags::ai::Learning`) counts
-- `message_tags` rows by `(tag_id, state)` restricted to `source = 'ai'`.
-- Without this, that count is a scan of every application in the mailbox on a
-- path that runs once per newly synced message. Partial, because every row it
-- must ever count has `source = 'ai'`.
CREATE INDEX idx_message_tags_ai_learning
    ON message_tags(tag_id, state) WHERE source = 'ai';
