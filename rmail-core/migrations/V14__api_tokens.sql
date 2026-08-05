-- V14: capability tokens for the gRPC/MCP auth boundary (task 38).
--
-- A token's *secret* never touches disk — only `token_hash`, an argon2id PHC
-- string (self-describing: algorithm + params + salt + digest), does. The
-- bearer string handed to an operator embeds this row's id so verification
-- can look it up in O(1) rather than argon2-hashing the presented secret
-- against every stored row; the id is not itself secret, only the part after
-- it is. `scopes` is comma-joined text (mirroring `accounts.secret_kind`-style
-- flat storage elsewhere in this schema) rather than a join table, because a
-- token's scope set is small, fixed at mint time, and read as a whole on
-- every request — a join would cost a second query for no query this schema
-- ever needs (there is no "find every token with scope X").
CREATE TABLE api_tokens (
    id           INTEGER PRIMARY KEY,
    name         TEXT NOT NULL,
    -- argon2id PHC string, stored as its UTF-8 bytes.
    token_hash   BLOB NOT NULL,
    -- Comma-joined scope strings, e.g. "mail.read,mail.send,ai.invoke,admin".
    scopes       TEXT NOT NULL,
    created_at   INTEGER NOT NULL DEFAULT (unixepoch()),
    -- Updated best-effort on successful verification; a failed update must
    -- never fail the request it is merely bookkeeping for.
    last_used_at INTEGER,
    -- NULL means no expiry.
    expires_at   INTEGER,
    revoked      INTEGER NOT NULL DEFAULT 0,
    CHECK (name <> ''),
    CHECK (scopes <> ''),
    CHECK (revoked IN (0, 1))
) STRICT;

-- No secondary index: the auth interceptor's hot path is "fetch this one id"
-- (a PK lookup, already indexed by `id`), and `ListTokens` is an unfiltered
-- scan ordered by `id` — an operator mints a handful of tokens, not enough
-- for a full-table scan to matter. Add one when a query actually filters on
-- `revoked`/`expires_at` (a "list only active tokens" RPC, say), not before.
