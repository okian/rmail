-- V36: the prompt-injection shield's flag ledger (task 77, prd.md #43).
--
-- prd.md, "Prompt-Injection Shield":
--   Every body is wrapped in untrusted-content delimiters and scanned for
--   injection patterns (hidden text, zero-width chars, "ignore previous
--   instructions"); detected messages are flagged and any AI action on them
--   requires confirmation, logged.
--
-- The wrapping half of that needs no schema — it is how a prompt is built
-- (see `rmail_core::ai::injection::untrusted_block`). This table is the
-- other three quarters: the flag, the record of *what* was detected so a
-- user can see what a message tried, and the confirmation that releases a
-- withheld action.
--
-- # One row per message, not one per detection
--
-- A scan is a function of a message's text, so its findings are a single
-- fact about that message that a re-scan replaces wholesale. Storing one row
-- per detection would make "is this message flagged" a GROUP BY, make a
-- re-scan a delete-then-insert, and — worse — leave a window where a message
-- has some of its old detections and some of its new ones. The detections
-- themselves live in a JSON column for the same reason `rules.toml` is a
-- document: they are a bounded, read-together, never-queried-by-field list
-- (`rmail_core::ai::injection::MAX_DETECTIONS` caps them at 32), and a
-- second table indexed by a kind nobody filters on would be cost with no
-- reader.
--
-- # Only flagged messages get a row
--
-- The overwhelming majority of mail is clean, and a row per scanned message
-- would make this table as large as `messages` to record "nothing happened".
-- Absence therefore means "not flagged", which is also what makes the action
-- gate cheap: the common path is a single indexed lookup that finds nothing.
-- The consequence — absence cannot distinguish "scanned, clean" from "never
-- scanned" — is deliberate and safe in the direction that matters: the rules
-- engine scans on the evaluation path itself rather than trusting a row to
-- already exist (see `rmail_core::rules::RuleEngine::evaluate_one`), so a
-- message no pass has ever looked at is still scanned before its actions can
-- fire.
CREATE TABLE ai_injection_flags (
    -- PRIMARY KEY, not just a foreign key: exactly one flag per message is
    -- the invariant above, and making the id the key is what lets a re-scan
    -- be an idempotent upsert rather than a read-modify-write.
    message_id   INTEGER PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
    account_id   INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    -- The highest severity among `detections`. Denormalized from the JSON
    -- deliberately: the action gate compares this on every AI-decided rule
    -- evaluation, and parsing a JSON blob to answer "is this hostile" would
    -- put a serde call on a hot path for a value that cannot disagree with
    -- itself -- `flag()` derives both from the same ScanReport in one place.
    severity     TEXT NOT NULL CHECK (severity IN ('suspicious', 'hostile')),
    -- JSON array of the distinct kind strings, for a UI that badges a
    -- message without reading every excerpt.
    kinds        TEXT NOT NULL,
    -- JSON array of {kind, excerpt, offset} -- what the message actually
    -- tried, quoted as written and bounded per excerpt. This is the
    -- "logged" half of the acceptance criterion and the whole content of
    -- `AiSafetyService.ScanInjection`'s answer.
    detections   TEXT NOT NULL,
    scanned_at   INTEGER NOT NULL DEFAULT (unixepoch()),
    -- NULL until a human explicitly confirms that AI-decided actions may act
    -- on this message anyway. Non-NULL is the *only* thing that releases a
    -- withheld rule action; nothing in the AI pipeline ever sets it, which
    -- is what makes it a confirmation rather than a retry.
    --
    -- Deliberately preserved across a re-scan whose findings are unchanged
    -- and cleared when they are not -- see `store::flag`. A confirmation is
    -- consent to *these* findings; new text on the same message is a new
    -- question.
    confirmed_at INTEGER
) STRICT;

-- "Show me every message in this account that tried something, newest
-- first" -- the only listing shape this table has. The action gate's own
-- lookup is by primary key and needs no index.
CREATE INDEX ai_injection_flags_account_scanned
    ON ai_injection_flags(account_id, scanned_at DESC);
