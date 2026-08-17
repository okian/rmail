-- V56: the client-facing password gate.
--
-- Backs `rmail_core::auth::password`. Distinct from `api_tokens` (V14):
-- a token is a credential an operator mints for one caller; this is the
-- password a human types to prove they are the rmail owner at all, before
-- any token comes into it -- see `ClientAuthService.LoginPassword`, which
-- verifies a presented password against this row and, on success, mints an
-- ordinary `api_tokens` row the same way `AdminService.MintToken` does. This
-- table never grants access by itself; it only ever leads to a mint.
--
-- # Why a singleton row, not a per-user table
--
-- rmail has one operator per daemon -- the Unix socket already has exactly
-- one trusted owning uid (see `rmaild::auth`'s "Two principals"). A table
-- keyed by user id would imply a multi-tenant model this daemon does not
-- have; `CHECK (id = 1)` says in the schema what would otherwise only be
-- true by convention, and makes a second row a constraint violation instead
-- of a silent ambiguity the next reader has to notice by hand.
--
-- # Why lockout state lives on this row instead of a separate table
--
-- `failed_attempts`/`locked_until` are `LoginPassword`'s only defense: the
-- password itself is the one secret in this table, so there is nothing to
-- rate-limit *per caller* the way, say, a per-IP table would -- the daemon
-- has no notion of "caller" before a login succeeds. A single counter next
-- to the single password is the whole state machine; a second table would
-- only be a second place the two could disagree about which attempt they
-- describe.
CREATE TABLE client_password (
    id INTEGER PRIMARY KEY CHECK (id = 1),

    -- Argon2id PHC string (self-describing: algorithm + params + salt +
    -- digest), the same encoding `api_tokens.token_hash` stores.
    password_hash TEXT NOT NULL,

    created_at INTEGER NOT NULL,
    -- Bumped on every `SetupPassword` call, including the first: read as
    -- "created_at" until it first differs from `created_at`.
    updated_at INTEGER NOT NULL,

    -- Consecutive failed `LoginPassword` attempts since the last success (or
    -- since the password was last set). Reset to 0 on a successful login and
    -- on `SetupPassword` -- a changed password should not inherit a lockout
    -- earned by attempts against the old one.
    failed_attempts INTEGER NOT NULL DEFAULT 0,

    -- Set once `failed_attempts` reaches `client_auth.max_attempts`; NULL
    -- when there is no active lockout. `LoginPassword` refuses to even
    -- attempt an Argon2 verification while this is in the future -- see
    -- `auth::password::verify_password` for why that check comes first, not
    -- as a fast path but as the only thing that makes a lockout mean
    -- anything (Argon2id succeeding or failing costs the same either way,
    -- but skipping it entirely is what stops an attacker with a large
    -- request budget from just waiting the hash cost out).
    locked_until INTEGER
) STRICT;
