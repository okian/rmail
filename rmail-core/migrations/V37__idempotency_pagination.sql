-- V37: the two pieces of API hardening that need storage — an idempotency
-- fence for mutating RPCs, and an index that makes keyset pagination over a
-- mailbox listing a range scan rather than a sort.

-- Replay protection for mutating RPCs (prd.md's `idempotency_keys`).
--
-- The shape is the `smtp_message_id` fence from `outbox`, generalized. That
-- fence works because the row that says "a send is in progress" is *committed
-- before* the irreversible act, so a process that dies mid-act leaves evidence
-- behind; the alternative (record afterwards) cannot distinguish "never
-- started" from "died halfway", and the difference between those two is a
-- second copy of someone's mail.
--
-- Here the same rule applies to any mutating RPC: `claim` inserts and commits
-- this row, *then* the handler runs, *then* `record` fills in the response. A
-- retry that finds a completed row replays it; a retry that finds an
-- unfinished one is refused (`ABORTED`) rather than allowed to re-apply,
-- because an unfinished row means the outcome is genuinely unknown. That is
-- the same at-most-once trade `outbox` makes and it is deliberate: the remedy
-- for an ambiguous mutation is a human (or agent) choosing a fresh key, not a
-- silent second application.
CREATE TABLE idempotency_keys (
    -- Caller-supplied, and globally single-use rather than per-method: the
    -- method is folded into `request_hash`, so reusing one key on a different
    -- RPC is a payload conflict (ALREADY_EXISTS) instead of a replay of the
    -- wrong method's response. Failing closed is the only safe direction for a
    -- value the client picks.
    key TEXT PRIMARY KEY,
    -- The gRPC method path, for operator visibility. Not part of the identity
    -- (see above) -- `request_hash` already covers it.
    method TEXT NOT NULL,
    -- SHA-256 over the method plus the encoded request. Two calls with the
    -- same key must be the same call; a differing hash is the client bug
    -- ALREADY_EXISTS exists to name.
    request_hash BLOB NOT NULL,
    -- The encoded response to replay. NULL means the claim is still held --
    -- either in flight right now, or abandoned by a process that died.
    response BLOB,
    -- The gRPC code of the recorded outcome, or -1 while the claim is
    -- unfinished. -1 is not a code any status can carry, so the CHECK below is
    -- what keeps "unfinished" from ever being confused with a cached `OK`
    -- (which really is 0).
    status_code INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    -- When the fence lapses. Until then the key cannot be reused for anything,
    -- including by the caller whose first attempt died.
    expires_at INTEGER NOT NULL,
    CHECK ((response IS NULL) = (status_code = -1))
) STRICT;

-- The reaper's scan. Claims are read by primary key; this index exists only so
-- expiring them does not walk the table.
CREATE INDEX idx_idempotency_expires ON idempotency_keys(expires_at);

-- Keyset pagination over `MailService.List`.
--
-- `idx_messages_mailbox_date` (V2) is `(mailbox_id, COALESCE(date,
-- internaldate) DESC)` and stays, because a dozen `retrieve::*` queries order
-- by that exact two-argument expression. It cannot back a *cursor*, for two
-- reasons:
--
--   * `COALESCE(date, internaldate)` is NULL for a message with neither
--     header, and a NULL sort key cannot be compared against a cursor -- every
--     such message would be unreachable after the first page. The
--     three-argument form pins those rows at 0, which is where SQLite's
--     "NULLs last under DESC" already put them for every message a mailbox
--     actually contains. The one difference: a message whose `Date` header is
--     pre-1970 now sorts *below* an undated one instead of above it. That is a
--     reordering of two pathological rows relative to each other, not a change
--     to where either sits among real mail, and it is the price of every
--     message being reachable at all.
--   * A page boundary needs a total order. Ties on the sort key are resolved
--     by `id`, and the tiebreak has to be *in the index* or the planner sorts
--     the range it just scanned.
--
-- Only `messages` gets one. `drafts`, `outbox` and `followups` paginate the
-- same way but are bounded by what one person has queued -- a few hundred rows
-- at the outside -- so their existing `(account_id, <sort> DESC)` indexes
-- narrow the scan to that, and the residual sort is over a set small enough
-- that a third index column would cost more on every insert than it saves on
-- the occasional second page. A mailbox has no such bound, which is the whole
-- reason this index exists.
--
-- `id` is the rowid, so naming it here costs nothing on disk; it is named
-- explicitly (and DESC) so the index's own order is exactly the listing's
-- `ORDER BY ... DESC, id DESC` and a page is a forward range scan.
CREATE INDEX idx_messages_mailbox_page
    ON messages(mailbox_id, COALESCE(date, internaldate, 0) DESC, id DESC);
