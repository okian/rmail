-- V55: discovered OpenPGP public keys, and the auto-encrypt decision cache.
--
-- Backs `rmail_core::crypto`. When a recipient address is set on a draft, the
-- daemon looks for that address's public key in the background; this is where
-- the answer is kept so the next draft to the same person is an instant local
-- decision instead of a second network round trip.
--
-- # Absence is an answer, and it is cached too
--
-- Most addresses have no OpenPGP key and never will. A cache that only stored
-- successes would re-query every keyserver, for every one of those addresses,
-- on every draft -- which is slow, rude to the servers, and (because the query
-- carries the address) leaks the user's contact graph over and over for no
-- benefit. So a miss is a stored row, with `outcome = 'absent'`, and it
-- suppresses further lookups for `crypto.negative_ttl`.
--
-- That is why `outcome` exists rather than the absence of a row meaning
-- absence: "we have never looked" and "we looked and there is nothing" are
-- different states with different next actions, and a schema that cannot tell
-- them apart forces the code to guess.
--
-- # Every row expires, and the expiry is not a policy decision
--
-- `revalidate_after` is `min(fetched_at + crypto.key_ttl, key_expires_at)` --
-- the shorter of "our cache is a month old" and "the key itself is dead".
-- Taking the minimum is what stops the cache from ever handing back a key the
-- world already considers invalid: a 30-day TTL on a key that expires next
-- Tuesday would keep encrypting to it for three weeks after nobody could read
-- the result. The daemon computes the minimum on write (see
-- `crypto::cache::put`) rather than the reader comparing both columns,
-- because a reader that forgets one of the two comparisons fails *open* --
-- it encrypts to a dead key and the mail is unreadable.
--
-- # Why the key bytes are stored and not just the fingerprint
--
-- A fingerprint alone would mean re-fetching the key to encrypt anything,
-- which defeats the cache on exactly the path that matters. The stored blob is
-- the transferable public key as received, unparsed -- re-parsed on read by
-- `crypto::key::parse`. Storing the parsed form instead would bake this
-- release's understanding of the format into the database.

-- ---------------------------------------------------------------------------
-- The discovery cache
-- ---------------------------------------------------------------------------

CREATE TABLE pgp_keys (
    -- The recipient address, normalized: lowercased, no display name, no
    -- angle brackets. `crypto::normalize_address` is the only writer of this
    -- column and the only thing that builds a lookup key, so the two cannot
    -- disagree about what "the same address" means -- a cache keyed on
    -- unnormalized addresses would miss on `Alice@Example.com` after storing
    -- `alice@example.com` and re-query the network for a row it already had.
    address TEXT PRIMARY KEY,

    -- 'found' or 'absent'. See the header: absence is a cached answer.
    outcome TEXT NOT NULL CHECK (outcome IN ('found', 'absent')),

    -- Uppercase hex, no spaces. NULL exactly when `outcome = 'absent'`.
    fingerprint TEXT,

    -- The transferable public key, as fetched. NULL exactly when absent.
    key_data BLOB,

    -- The key's own self-reported creation time. This is the tiebreak when a
    -- lookup turns up several usable keys for one address: newest wins, which
    -- is the "if found multiple get the latest one" rule. Stored rather than
    -- recomputed so the ordering is inspectable in SQL.
    key_created_at INTEGER,

    -- The key's expiry, or NULL for a key that does not expire. Folded into
    -- `revalidate_after` on write; kept as its own column so an operator can
    -- see *why* a row is short-lived.
    key_expires_at INTEGER,

    -- Where it came from: 'autocrypt', 'wkd', or the configured keyserver's
    -- name. Not decoration -- it is what makes the privacy posture auditable.
    -- An operator who believes they never touch public keyservers can check
    -- that belief against this column instead of against the config that was
    -- supposed to produce it.
    source TEXT,

    -- When the lookup ran.
    fetched_at INTEGER NOT NULL,

    -- When this row stops being authoritative: min(TTL, key expiry). Applies
    -- to both outcomes -- for 'absent' it is the negative TTL, which is what
    -- "if found none, don't search for a month" means mechanically.
    revalidate_after INTEGER NOT NULL,

    -- How many consecutive lookups have failed for this address, for backoff.
    -- Distinct from 'absent': a keyserver timing out is not evidence that the
    -- recipient has no key, and treating it as such would suppress discovery
    -- for a month over one bad network minute. Failures back off from a short
    -- retry instead of writing an 'absent' row.
    failures INTEGER NOT NULL DEFAULT 0
) STRICT;

-- The background refresher's only query: "what is due?". Without this it
-- scans every address the user has ever composed to.
CREATE INDEX pgp_keys_revalidate ON pgp_keys (revalidate_after);

-- ---------------------------------------------------------------------------
-- Trust on first use
-- ---------------------------------------------------------------------------

-- Every fingerprint ever seen for an address, and when.
--
-- # Why a key changing is worth recording
--
-- Automatic key discovery has one serious failure mode, and it is not "no key
-- found" -- it is "the wrong key found". An attacker who can publish to a
-- keyserver, or answer for the recipient's domain, can offer a key they hold
-- the private half of; the user's client then silently encrypts to it, and the
-- mail is both unreadable by the recipient and readable by the attacker. The
-- encryption indicator would show a confident padlock the entire time.
--
-- rmail cannot prevent that -- no unauthenticated discovery mechanism can --
-- but it can refuse to let it happen *quietly*. This table makes a key change
-- a visible event: the first key for an address is trusted on sight (there is
-- nothing to compare it to), and a later, different one is surfaced to the
-- user rather than swapped in behind them. That is the whole of TOFU, and it
-- is the difference between "we were fooled" and "we were fooled and nobody
-- could tell".
CREATE TABLE pgp_key_history (
    address TEXT NOT NULL,
    fingerprint TEXT NOT NULL,
    first_seen_at INTEGER NOT NULL,
    last_seen_at INTEGER NOT NULL,
    source TEXT,
    -- Set once the user has been shown this fingerprint and kept it. An
    -- unaccepted second fingerprint is what `EncryptionStatus::KeyChanged`
    -- reports on.
    accepted_at INTEGER,
    PRIMARY KEY (address, fingerprint)
) STRICT, WITHOUT ROWID;

-- ---------------------------------------------------------------------------
-- Per-address overrides
-- ---------------------------------------------------------------------------

-- The user's explicit decision about one correspondent, which outranks both
-- discovery and the global `crypto.auto_encrypt` default.
--
-- Three policies rather than a boolean, because "encrypt to this person" and
-- "never encrypt to this person" are not opposites of the same switch:
-- 'always' is a *requirement* (fail the send rather than fall back to
-- plaintext -- the setting a journalist wants for a source), 'never' is a
-- suppression (a mailing list that cannot read PGP), and 'auto' is the
-- default, meaning "encrypt when a key is known". A boolean would have forced
-- 'always' and 'auto' to share a value and lost the only distinction that
-- changes what happens when discovery fails.
CREATE TABLE pgp_overrides (
    address TEXT PRIMARY KEY,
    policy TEXT NOT NULL CHECK (policy IN ('auto', 'always', 'never')),
    -- A fingerprint the user pinned by hand. When set, discovery's answer is
    -- ignored entirely for this address: this is the escape hatch for someone
    -- who verified a key in person and does not want a keyserver's opinion to
    -- override it.
    pinned_fingerprint TEXT,
    -- A manually imported key, for a correspondent no discovery method can
    -- reach. NULL means "use the discovered key that matches
    -- `pinned_fingerprint`".
    key_data BLOB,
    updated_at INTEGER NOT NULL DEFAULT (unixepoch())
) STRICT;
