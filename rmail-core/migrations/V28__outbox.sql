-- V28: the durable outbox and follow-up reminders (task 61, prd.md III-5).
--
-- The outbox is the one place in rmail where losing a row and duplicating a
-- row are both irreversible from the user's point of view, so the schema is
-- shaped around the two failure modes rather than around the happy path.
--
-- `raw_mime` is the source of truth. It is the complete RFC 5322 message
-- rendered by `compose::mime::build` at *schedule* time, and it is what SMTP
-- transmits verbatim. Freezing it here (rather than re-rendering at send
-- time from the draft) is what makes "what you scheduled is what goes out"
-- true even if the draft, the account row, or the renderer itself changes in
-- between. It also carries no `Bcc` header — the renderer omits one by
-- design — which is why the copy appended to IMAP `Sent` needs no stripping
-- step: the blind recipients live only in `bcc_addrs`, which feeds `RCPT TO`
-- and nothing else.
--
-- `smtp_message_id` is the at-most-once fence. It is written, and committed,
-- *before* the SMTP `DATA` command, so a process that dies mid-transmission
-- leaves behind proof that a copy may already be on the wire. The recovery
-- path treats that proof as "sent" rather than re-transmitting, which is the
-- deliberate choice prd.md makes: a message that silently goes out twice is
-- worse than one that needs a human to confirm it arrived. A transmission
-- that *returns* an error is a different thing entirely — the peer answered,
-- so nothing was queued — and that path clears the fence back to NULL so the
-- retry genuinely retries. See `rmail_core::outbox`'s module docs.

CREATE TABLE outbox (
    id         INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,

    -- The draft this was rendered from, when there was one. ON DELETE SET
    -- NULL, never CASCADE: deleting a draft must not delete a message that
    -- is already queued for transmission. It exists so `UpdateScheduledBody`
    -- has something editable to re-render from — `raw_mime` is frozen
    -- octets, not an editable document.
    draft_id   INTEGER REFERENCES drafts(id) ON DELETE SET NULL,

    -- Envelope participants, as bare addr-specs, newline-separated. Not
    -- comma-separated (the shape `messages.to_addrs` uses for parsed inbound
    -- mail) because these feed SMTP `RCPT TO` directly and a delimiter that
    -- can appear inside a quoted local-part would silently split one
    -- recipient into two. A newline cannot appear in an addr-spec at all.
    from_addr  TEXT NOT NULL,
    to_addrs   TEXT NOT NULL DEFAULT '',
    cc_addrs   TEXT NOT NULL DEFAULT '',
    -- Blind recipients. Present here and nowhere in `raw_mime`.
    bcc_addrs  TEXT NOT NULL DEFAULT '',

    subject       TEXT NOT NULL DEFAULT '',
    raw_mime      BLOB NOT NULL,
    body_preview  TEXT NOT NULL DEFAULT '',
    in_reply_to   TEXT,
    thread_id     INTEGER REFERENCES threads(id) ON DELETE SET NULL,

    -- The absolute instant, unix seconds, frozen when the message was
    -- scheduled. `tz` is the IANA zone it was scheduled *in*, kept for
    -- display only: re-deriving the instant from a wall-clock time and a zone
    -- at send time is exactly how a message crosses a DST boundary and goes
    -- out an hour wrong.
    send_at    INTEGER NOT NULL,
    tz         TEXT NOT NULL DEFAULT 'UTC',

    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at INTEGER NOT NULL DEFAULT (unixepoch()),

    -- scheduled -> sending -> sent
    -- scheduled -> sending -> scheduled  (transient failure, retry)
    -- scheduled -> sending -> failed     (permanent, or retries exhausted)
    -- scheduled -> canceled              (user, within the undo window)
    state      TEXT NOT NULL DEFAULT 'scheduled',
    -- user | ai | followup | undo. `ai` is load-bearing, not decoration: an
    -- AI-originated send is always given an undo window regardless of
    -- configuration, so a human can intercept it.
    origin     TEXT NOT NULL DEFAULT 'user',

    attempts        INTEGER NOT NULL DEFAULT 0,
    max_retries     INTEGER NOT NULL DEFAULT 5,
    next_attempt_at INTEGER,
    -- The lease a worker holds while transmitting. A crash leaves it in the
    -- past and the reaper returns the row to 'scheduled'; the worker that
    -- picks it up next then consults `smtp_message_id` before doing anything.
    lease_expires_at INTEGER,
    leased_by        TEXT,
    last_error       TEXT,

    smtp_message_id TEXT,
    sent_at         INTEGER,
    -- 1 when the message went out past `send.late_tolerance` because rmail
    -- was not running at `send_at`. It still went out — prd.md's rule is
    -- "never drop" — but the user is told it was late rather than being left
    -- to assume it was punctual.
    sent_late       INTEGER NOT NULL DEFAULT 0,
    -- When an undo is still possible. Set for immediate sends (which are
    -- really "schedule at now + undo_window"); NULL for a genuine future
    -- schedule, which is cancelable right up to its lease.
    undo_deadline   INTEGER,

    -- 'uncertain': the session died without a reply, so whether the peer
    -- queued the message is unknown. Deliberately its own state rather than
    -- folded into 'failed': a failed row is safe to retry, an uncertain one
    -- is not (a retry may deliver a second copy), and it is not 'sent'
    -- either, because it may never have arrived. It keeps its
    -- smtp_message_id fence and waits for a human. See the outbox module
    -- docs' at-most-once section.
    CHECK (state IN ('scheduled', 'sending', 'sent', 'failed', 'canceled', 'uncertain')),
    CHECK (origin IN ('user', 'ai', 'followup', 'undo')),
    CHECK (sent_late IN (0, 1))
) STRICT;

-- The scheduler's only hot query is "what is due now, and when is the next
-- thing due" — both are (state, send_at) prefix scans, which is what keeps
-- the loop a single indexed lookup per wake instead of a table scan.
CREATE INDEX idx_outbox_due ON outbox(state, send_at);
-- The outbox view is account-scoped and newest-first.
CREATE INDEX idx_outbox_account ON outbox(account_id, created_at DESC);
-- One row per Message-ID, enforced by the database rather than by the code
-- that writes it. Two outbox rows claiming the same Message-ID would mean
-- the at-most-once fence protects neither of them.
CREATE UNIQUE INDEX idx_outbox_smtp_message_id
    ON outbox(smtp_message_id) WHERE smtp_message_id IS NOT NULL;

-- Follow-up reminders: "I sent this, nudge me if nobody replies."
CREATE TABLE followups (
    id         INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,

    -- Nullable, unlike prd.md's sketch. A follow-up armed on a message rmail
    -- just sent has no local thread row yet (the sent copy is only threaded
    -- once it syncs back from IMAP), and a NOT NULL column would force the
    -- caller to invent an id or forbid the single most obvious moment to arm
    -- a follow-up.
    thread_id  INTEGER REFERENCES threads(id) ON DELETE SET NULL,
    -- The RFC 5322 Message-ID being followed up, bare (no angle brackets) —
    -- the same form `messages.message_id` stores. This is the join key for
    -- reply detection, which is why it is the message *identity* rather than
    -- a local row id: the reply that dismisses this follow-up names it in
    -- `In-Reply-To`/`References`, not by rowid.
    message_id TEXT NOT NULL,

    remind_at  INTEGER NOT NULL,
    tz         TEXT NOT NULL DEFAULT 'UTC',
    cancel_on_reply INTEGER NOT NULL DEFAULT 1,
    -- armed | fired | dismissed
    state      TEXT NOT NULL DEFAULT 'armed',
    note       TEXT,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),

    CHECK (state IN ('armed', 'fired', 'dismissed')),
    CHECK (cancel_on_reply IN (0, 1))
) STRICT;

-- The sweep asks the same shape of question the outbox one does.
CREATE INDEX idx_followups_due ON followups(state, remind_at);
CREATE INDEX idx_followups_account ON followups(account_id, created_at DESC);
-- Reply detection looks a follow-up up by the id its reply names.
CREATE INDEX idx_followups_message ON followups(message_id);
