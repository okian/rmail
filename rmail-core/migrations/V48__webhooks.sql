-- V48: outbound webhooks — the durable record of what this machine sends to
-- somebody else's server (task 68, prd.md #49 and #64).
--
-- This is the first table in the schema whose whole purpose is egress of mail
-- content. `hooks` (task 67) runs an operator's command locally and
-- `notifications` (V40) pings the local desktop; both are deliberately
-- confined to this machine. A webhook is not, so the schema is shaped around
-- the two questions an operator will ask after the fact — *what left, and
-- where did it go* — rather than around the happy path of a 200 response.
--
-- # Nothing is sent to a destination that is not in this table
--
-- There is no default destination, no implicit one derived from other
-- configuration, and no wildcard. A row here exists only because an operator
-- ran `mail webhook add` (or `WebhookService/Register`). `enabled = 0` keeps
-- the row and stops the sending, so turning a destination off is not the same
-- act as forgetting where it pointed.
--
-- # `include_body` is per destination and defaults to 0
--
-- The default payload is the *notification*, not the mail: sender, subject,
-- a message id and a deep link. That is enough for a Slack channel to say
-- "something arrived from X about Y, click here", and it is the least content
-- that makes the notification useful. A body is a separate, explicit decision
-- per destination — a team's alert channel and a personal ticketing webhook
-- are not owed the same amount of a private message — so it is a column here
-- rather than a global switch. `rmail_core::webhooks::payload` is where the
-- two shapes are actually built, and it documents field by field what each
-- one carries.
--
-- # The signing key's *source* is stored, never the key
--
-- `secret_kind`/`secret_reference` are exactly `accounts`' (V3) columns and
-- are read back through the same `crate::credential::CredentialSource`. A
-- key inline in this table would be a key in every backup, every `sqlite3`
-- session and every support bundle of this database; a *reference* to the
-- operator's keychain or password command is not. `secret_kind = 'none'` is a
-- destination that receives unsigned requests, which is honest about what the
-- receiver can verify rather than pretending a constant is a signature.
--
-- # One delivery row per (destination, event) — the idempotency fence
--
-- `UNIQUE (destination_id, event_key)` is what makes delivery idempotent per
-- event, in the same way `extraction_deliveries` (V46) makes a calendar sink
-- idempotent per item: the enqueue is an `INSERT OR IGNORE` and the database,
-- not the process, decides who was first. A redelivered sync, an overlapping
-- tick and two daemons racing the same event log all collapse to one row and
-- therefore one POST.
--
-- The fence is on *enqueue*, not on the POST, which is the deliberate
-- difference from V46. There the claim is taken before the side effect and a
-- failure releases it, because the side effect (a task in somebody's tracker)
-- is unbounded and duplicating it is the worst outcome. Here the row *is* the
-- queue: it survives the crash, carries its own attempt count, and is retried
-- from where it stopped. A crash between the POST and the state update
-- therefore costs at most one duplicate delivery to a receiver that already
-- has `X-Rmail-Delivery` to dedupe on — at-least-once with a stable id, which
-- is what every webhook receiver in the world is already built for, rather
-- than the at-most-once V46 needs.
CREATE TABLE webhook_destinations (
    id           INTEGER PRIMARY KEY,

    -- The operator's handle for this destination — what `mail forward <id>
    -- --to slack:eng-alerts` names and what `mail webhook rm` addresses. A
    -- name rather than the URL, so nothing that references a destination has
    -- to carry the URL around (a URL is frequently itself the secret: a Slack
    -- incoming-webhook URL is a bearer credential in the path).
    name         TEXT NOT NULL UNIQUE,

    -- Where the POST goes. `https://` is required except on loopback, which
    -- is allowed plaintext because it cannot leave the machine (and is what
    -- the tests drive). Enforced in `webhooks::validate_url` at the moment a
    -- destination is registered, not only at send time, so an unusable
    -- destination is refused while somebody is still looking at it.
    url          TEXT NOT NULL,

    -- generic | slack. How the payload is rendered — see
    -- `rmail_core::webhooks::Template`. `slack` produces the `text` field
    -- Slack incoming webhooks require; `generic` produces the JSON document
    -- with the same facts as named fields.
    template     TEXT NOT NULL DEFAULT 'generic',

    -- Which events this destination subscribes to: newline-separated
    -- `crate::config::HookEvent` wire strings (`on_new_message`,
    -- `on_rule_match`, ...). Newline-separated rather than comma-separated
    -- for the reason `outbox.to_addrs` gives — a delimiter that can occur
    -- inside a value silently splits one entry into two — even though this
    -- particular vocabulary is closed today.
    --
    -- Empty means "subscribes to nothing", and a destination that subscribes
    -- to nothing still receives an explicit `mail forward`. That split is the
    -- point: an operator who wants a channel they push to by hand, with no
    -- automatic firehose, writes exactly that.
    events       TEXT NOT NULL DEFAULT '',

    -- 0: sender, subject, message id and deep link only. 1: the operator
    -- explicitly turned body inclusion on for this destination. See the
    -- header.
    include_body INTEGER NOT NULL DEFAULT 0,

    enabled      INTEGER NOT NULL DEFAULT 1,

    -- none | command | env | keychain — `crate::credential::CredentialSource`'s
    -- own vocabulary, minus `oauth` (there is nothing to refresh here; a
    -- webhook signing key is a static shared secret). See the header.
    secret_kind      TEXT NOT NULL DEFAULT 'none',
    secret_reference TEXT,

    -- The attempt cap for this destination's deliveries, copied onto each
    -- delivery row at enqueue time so that lowering it later cannot strand an
    -- in-flight delivery mid-retry with a cap it has already passed.
    max_attempts INTEGER NOT NULL DEFAULT 5,

    created_at   INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at   INTEGER NOT NULL DEFAULT (unixepoch()),

    CHECK (name <> ''),
    CHECK (url <> ''),
    CHECK (template IN ('generic', 'slack')),
    CHECK (include_body IN (0, 1)),
    CHECK (enabled IN (0, 1)),
    CHECK (secret_kind IN ('none', 'command', 'env', 'keychain')),
    -- Every source but `none` is a reference to somewhere else; a row that
    -- names a source with nowhere to look is a destination whose signature
    -- can never be produced.
    CHECK (secret_kind = 'none' OR (secret_reference IS NOT NULL AND secret_reference <> '')),
    CHECK (max_attempts >= 1)
) STRICT;

-- The persisted delivery queue.
CREATE TABLE webhook_deliveries (
    id             INTEGER PRIMARY KEY,
    destination_id INTEGER NOT NULL REFERENCES webhook_destinations(id) ON DELETE CASCADE,

    -- The idempotency key: what happened, from the sender's point of view.
    -- For a dispatched event it is the durable event log's `seq`
    -- (`event:<seq>`), which is globally unique and monotonic. For a manual
    -- forward it is `forward:<message_id>:<nonce>`, because forwarding the
    -- same message to the same channel twice on purpose is a legitimate act a
    -- human just performed — unlike an event, which is one fact that happened
    -- once. See `webhooks::EventKey`.
    event_key      TEXT NOT NULL,

    -- The `crate::config::HookEvent` wire string, or `forward`. Denormalized
    -- from `event_key` so `ListDeliveries` can say what a row was without
    -- parsing a key whose shape is this module's private business.
    event          TEXT NOT NULL,

    -- The message this is about, when there is one. ON DELETE SET NULL rather
    -- than CASCADE: the record that something left this machine must outlive
    -- the local copy of what it was about — a delivery log that a later
    -- expunge can erase is not a log.
    message_id     INTEGER REFERENCES messages(id) ON DELETE SET NULL,

    -- The exact JSON body that is (or was) POSTed, frozen at enqueue time.
    --
    -- Frozen, not re-rendered per attempt, for the reason `outbox.raw_mime`
    -- gives: a retry must transmit what the first attempt transmitted, or the
    -- signature the receiver dedupes on stops identifying one thing. It also
    -- makes `ReplayDelivery` honest — a replay resends the bytes that were
    -- sent, not a fresh render of a mailbox that has since changed.
    --
    -- Already redacted (`crate::ai::redact`) and already minimized by
    -- `include_body` when it lands here. Nothing further is done to it on the
    -- way out, so this column is exactly what left the machine.
    payload        TEXT NOT NULL,

    -- pending -> delivered            (2xx)
    -- pending -> pending              (transient failure, backoff)
    -- pending -> failed               (attempts spent, or a permanent refusal)
    --
    -- `failed` is terminal on purpose: a destination that has refused five
    -- times is not helped by a sixth, and an unbounded retry against a
    -- misconfigured URL is an outbound request generator. `ReplayDelivery` is
    -- the explicit, operator-driven way back out of it.
    state          TEXT NOT NULL DEFAULT 'pending',

    attempts       INTEGER NOT NULL DEFAULT 0,
    -- Copied from the destination at enqueue time — see that column's docs.
    max_attempts   INTEGER NOT NULL DEFAULT 5,
    -- Unix seconds before which this row must not be attempted. NULL means
    -- "ready now". Carries both the backoff after a failure and the lease a
    -- worker holds while an attempt is in flight, exactly as
    -- `notifications.next_attempt_at` does and for the same reason: both mean
    -- "not before this instant", and one column with one index answers both.
    next_attempt_at INTEGER,

    -- The last HTTP status seen, when the peer answered at all. NULL for a
    -- connection that never got a response (DNS, refused, timed out) — which
    -- is a different operational fact from a 500 and must not look like one.
    last_status    INTEGER,
    -- The last failure, as a short operator-facing string. Never the response
    -- body verbatim and never anything from the request: see
    -- `webhooks::Delivery`'s docs.
    last_error     TEXT,

    created_at     INTEGER NOT NULL DEFAULT (unixepoch()),
    delivered_at   INTEGER,

    CHECK (event_key <> ''),
    CHECK (state IN ('pending', 'delivered', 'failed')),
    CHECK (attempts >= 0),
    CHECK (max_attempts >= 1),

    -- The idempotency fence — see the header.
    UNIQUE (destination_id, event_key)
) STRICT;

-- The delivery loop's only query: "pending rows that are due, oldest first".
--
-- Partial on `state = 'pending'`, keyed on `id` alone, for exactly the
-- reasons V40's `idx_notifications_due` spells out: the terminal states
-- dominate the table within a day of normal use, and an index led by
-- `next_attempt_at` cannot serve the `ORDER BY id` the claim needs, so SQLite
-- declines it and scans the table instead. `webhooks::store::claim_due`
-- likewise spells `state = 'pending'` inline rather than binding it, so
-- whether the partial index applies never rests on how a given SQLite version
-- treats a parameter whose value it cannot see.
CREATE INDEX idx_webhook_deliveries_due
    ON webhook_deliveries(id)
    WHERE state = 'pending';

-- `ListDeliveries` is per destination, newest first — the operator's "what
-- did this endpoint actually receive" view.
CREATE INDEX idx_webhook_deliveries_destination
    ON webhook_deliveries(destination_id, id DESC);
