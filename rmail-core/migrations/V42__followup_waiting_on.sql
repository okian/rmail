-- V42: the waiting-on tracker's columns on `followups` (task 63, prd.md #21).
--
-- Task 61 shipped `followups` as a pure reminder: an id, an instant, and a
-- note. prd.md #21 asks for something a person can actually work from — "what
-- did I ask, of whom, and how long have I been waiting" — which is three
-- facts the reminder never recorded. They are added here rather than in a
-- second table because they are attributes *of the reminder*: one reminder has
-- exactly one ask, and a join would buy nothing but a nullable row.
--
-- Everything is nullable or defaulted, so every reminder armed by task 61
-- keeps working unchanged and simply reports no ask. That matters more than
-- it looks: `state` and `remind_at` are what the scheduler sweeps on, and this
-- migration must not be able to change which rows fire.

-- 'manual' (a human armed it) or 'auto' (the tracker's judge did). Kept as a
-- column rather than inferred from `ask IS NOT NULL` because the two answer
-- different questions: a hand-armed reminder can carry an ask the user typed,
-- and a judged one can decline to name one. No CHECK constraint — SQLite
-- cannot add one to an existing table without rebuilding it, and rebuilding
-- the one table whose rows are load-bearing reminders is not a trade worth
-- making for a value `FollowupKind::parse` already validates on the way in
-- and on the way out.
ALTER TABLE followups ADD COLUMN kind TEXT NOT NULL DEFAULT 'manual';

-- The extracted ask: "confirm the Q3 numbers", "send the signed SOW". Model
-- output, so it is put through `injection::sanitize_model_text` before it
-- lands here — this string is printed to a terminal.
ALTER TABLE followups ADD COLUMN ask TEXT;

-- Who is being waited on, as bare addr-specs joined by newlines. Newline for
-- the reason `outbox.to_addrs` gives: a comma can appear inside a quoted
-- local-part and would silently split one recipient into two.
ALTER TABLE followups ADD COLUMN waiting_on TEXT NOT NULL DEFAULT '';

-- The subject of the message being waited on, frozen. Denormalized on
-- purpose: the tracked message is identified by its RFC 5322 Message-ID, and
-- a message this machine sent has no local `messages` row until it syncs back
-- from IMAP — which may be minutes away, or never, on an account with no Sent
-- folder. A waiting-on list that could not name the message until then would
-- be empty exactly when it is most wanted.
ALTER TABLE followups ADD COLUMN subject TEXT NOT NULL DEFAULT '';

-- When the tracked message went out. This, not `created_at`, is what "aging"
-- is measured from: a reminder armed a week after the fact is not a week old.
-- Nullable, because a reminder armed by hand on a message the user did not
-- send has no such instant.
ALTER TABLE followups ADD COLUMN sent_at INTEGER;

-- The waiting-on list is account-scoped, state-filtered, and oldest-first,
-- which is exactly this prefix. `idx_followups_due` cannot serve it: that one
-- leads on `state` alone and orders by `remind_at`, and the waiting-on view
-- spans two states and sorts by a different column.
CREATE INDEX idx_followups_waiting ON followups(account_id, state, sent_at);
