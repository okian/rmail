//! The discovery cache: what we know about an address's key, and until when.
//!
//! # The two TTLs are not symmetric, and that is deliberate
//!
//! A *positive* answer expires at `min(fetched_at + key_ttl, key_expires_at)`.
//! Both halves matter, and the minimum is taken on write rather than left to
//! the reader ([`put_found`]) for one reason: a reader that forgets one of the
//! two comparisons fails **open**. It keeps encrypting to a key that expired
//! last week, producing mail the recipient cannot read, and nothing in the
//! system notices. Computing it once, in the only function that writes the
//! column, makes that class of bug unreachable rather than merely unlikely.
//!
//! A *negative* answer expires at `fetched_at + negative_ttl` — the "if found
//! none, don't search for a month" rule. It has no second term because there
//! is no key whose expiry could bound it.
//!
//! # Why a failed lookup is not a negative answer
//!
//! [`record_failure`] exists because "every keyserver timed out" and "every
//! keyserver answered, and none had a key" are different facts that a lesser
//! schema would store identically. Conflating them means one bad network
//! minute suppresses discovery for this address for a *month* — the user's
//! mail silently stops being encrypted and the only visible symptom is its
//! absence. Failures instead back off on a short exponential schedule and
//! leave the address eligible for retry.

use rusqlite::{Connection, OptionalExtension};

use super::key::{KeySource, UsableKey};
use super::normalize_address;

/// The shortest retry delay after a failed lookup, in seconds.
const FAILURE_BACKOFF_BASE_SECS: i64 = 300;

/// The longest retry delay after repeated failures, in seconds (6 hours).
///
/// Capped well below `negative_ttl`: a persistently unreachable keyserver
/// should not end up suppressing discovery for as long as a genuine "this
/// person has no key" answer does.
const FAILURE_BACKOFF_MAX_SECS: i64 = 21_600;

/// What the cache knows about one address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cached {
    /// A usable key, still within its TTL and its own validity period.
    Key(Box<UsableKey>),
    /// Checked, nothing found, and the negative TTL has not run out.
    Absent,
    /// Either never looked up, or the entry is due for revalidation.
    ///
    /// One variant for both because the caller does the same thing with them:
    /// start a discovery. They differ for the *indicator* — a stale row can
    /// still show its old answer while the refresh runs — which is why
    /// [`lookup`] returns the superseded key alongside.
    Stale {
        /// The expired entry's key, if it had one. Lets the UI keep showing a
        /// padlock during a background refresh instead of flickering to
        /// "unknown" and back.
        previous: Option<Box<UsableKey>>,
    },
    /// A lookup failed recently and the backoff has not elapsed.
    ///
    /// Distinct from [`Self::Absent`]: nothing is known, and the caller should
    /// neither encrypt nor start another lookup yet.
    Backoff {
        /// Unix seconds at which a retry becomes allowed.
        retry_at: i64,
    },
}

impl PartialEq for UsableKey {
    fn eq(&self, other: &Self) -> bool {
        self.fingerprint == other.fingerprint && self.address == other.address
    }
}

impl Eq for UsableKey {}

/// Read what is known about `address`.
///
/// # Errors
///
/// Propagates any `rusqlite` error.
pub fn lookup(conn: &Connection, address: &str, now: i64) -> rusqlite::Result<Cached> {
    let address = normalize_address(address);
    let row = conn
        .query_row(
            "SELECT outcome, fingerprint, key_data, key_created_at, key_expires_at,
                    source, revalidate_after, failures
               FROM pgp_keys WHERE address = ?1",
            rusqlite::params![&address],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<Vec<u8>>>(2)?,
                    row.get::<_, Option<i64>>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, i64>(6)?,
                    row.get::<_, i64>(7)?,
                ))
            },
        )
        .optional()?;

    let Some((outcome, fingerprint, data, created, expires, source, revalidate_after, failures)) =
        row
    else {
        return Ok(Cached::Stale { previous: None });
    };

    let key = match (fingerprint, data, created) {
        (Some(fingerprint), Some(data), Some(created_at)) => Some(Box::new(UsableKey {
            fingerprint,
            address: address.clone(),
            created_at,
            expires_at: expires,
            source: source.as_deref().map_or(KeySource::PublicKeyserver, |s| {
                source_from_str(s).unwrap_or(KeySource::PublicKeyserver)
            }),
            data,
        })),
        _ => None,
    };

    if now < revalidate_after {
        return Ok(match outcome.as_str() {
            "found" => key.map_or(Cached::Stale { previous: None }, Cached::Key),
            _ => Cached::Absent,
        });
    }

    // Past `revalidate_after`. A row with recorded failures is in backoff
    // rather than merely stale — see the module docs.
    if failures > 0 {
        let retry_at = revalidate_after.saturating_add(backoff_secs(failures));
        if now < retry_at {
            // ...but a *refresh* that failed must not throw away a key that is
            // still valid on its own terms. `record_failure` leaves the stored
            // key in place precisely so this branch can keep using it: the key
            // has not expired, nothing has revoked it, and the only thing that
            // went wrong is that we could not re-ask a server about it.
            //
            // Returning `Backoff` unconditionally here was a bug, and an
            // expensive one: `encrypt::resolve` maps `Backoff` to "no key", so
            // one unreachable keyserver during a routine revalidation silently
            // downgraded a correspondent from encrypted to cleartext for the
            // length of the backoff. That is exactly the outcome the
            // failure/absence split exists to prevent — it was handled
            // correctly on the write side and thrown away on the read side.
            if let Some(key) = key {
                // Spelled out rather than `is_none_or`, which is newer than
                // this workspace's MSRV.
                let still_valid = match key.expires_at {
                    Some(expiry) => expiry > now,
                    None => true,
                };
                if still_valid {
                    return Ok(Cached::Key(key));
                }
            }
            return Ok(Cached::Backoff { retry_at });
        }
    }

    Ok(Cached::Stale { previous: key })
}

/// Store a successful discovery.
///
/// `revalidate_after` is `min(now + key_ttl, key_expires_at)` — the only place
/// that minimum is computed. See the module docs for why it lives here.
///
/// # Errors
///
/// Propagates any `rusqlite` error.
pub fn put_found(
    conn: &Connection,
    key: &UsableKey,
    now: i64,
    key_ttl_secs: i64,
) -> rusqlite::Result<()> {
    let ttl_expiry = now.saturating_add(key_ttl_secs);
    let revalidate_after = key.expires_at.map_or(ttl_expiry, |e| ttl_expiry.min(e));

    conn.execute(
        "INSERT INTO pgp_keys
             (address, outcome, fingerprint, key_data, key_created_at,
              key_expires_at, source, fetched_at, revalidate_after, failures)
         VALUES (?1, 'found', ?2, ?3, ?4, ?5, ?6, ?7, ?8, 0)
         ON CONFLICT(address) DO UPDATE SET
             outcome = 'found',
             fingerprint = excluded.fingerprint,
             key_data = excluded.key_data,
             key_created_at = excluded.key_created_at,
             key_expires_at = excluded.key_expires_at,
             source = excluded.source,
             fetched_at = excluded.fetched_at,
             revalidate_after = excluded.revalidate_after,
             failures = 0",
        rusqlite::params![
            &key.address,
            &key.fingerprint,
            &key.data,
            key.created_at,
            key.expires_at,
            key.source.as_str(),
            now,
            revalidate_after,
        ],
    )?;

    record_seen(conn, &key.address, &key.fingerprint, key.source, now)?;
    Ok(())
}

/// Store "checked, and this address has no key", suppressing lookups for
/// `negative_ttl_secs`.
///
/// # Errors
///
/// Propagates any `rusqlite` error.
pub fn put_absent(
    conn: &Connection,
    address: &str,
    now: i64,
    negative_ttl_secs: i64,
) -> rusqlite::Result<()> {
    let address = normalize_address(address);
    conn.execute(
        "INSERT INTO pgp_keys
             (address, outcome, fingerprint, key_data, key_created_at,
              key_expires_at, source, fetched_at, revalidate_after, failures)
         VALUES (?1, 'absent', NULL, NULL, NULL, NULL, NULL, ?2, ?3, 0)
         ON CONFLICT(address) DO UPDATE SET
             outcome = 'absent',
             fingerprint = NULL,
             key_data = NULL,
             key_created_at = NULL,
             key_expires_at = NULL,
             source = NULL,
             fetched_at = excluded.fetched_at,
             revalidate_after = excluded.revalidate_after,
             failures = 0",
        rusqlite::params![&address, now, now.saturating_add(negative_ttl_secs)],
    )?;
    Ok(())
}

/// Record that every source errored for this address.
///
/// Increments the failure count and pushes `revalidate_after` out by the
/// backoff. Deliberately does **not** write an `absent` outcome: see the
/// module docs.
///
/// # Errors
///
/// Propagates any `rusqlite` error.
pub fn record_failure(conn: &Connection, address: &str, now: i64) -> rusqlite::Result<()> {
    let address = normalize_address(address);
    conn.execute(
        "INSERT INTO pgp_keys
             (address, outcome, fetched_at, revalidate_after, failures)
         VALUES (?1, 'absent', ?2, ?2, 1)
         ON CONFLICT(address) DO UPDATE SET
             fetched_at = ?2,
             revalidate_after = ?2,
             failures = pgp_keys.failures + 1",
        rusqlite::params![&address, now],
    )?;
    Ok(())
}

/// Exponential backoff for `failures` consecutive failures, capped.
fn backoff_secs(failures: i64) -> i64 {
    let shift = u32::try_from(failures.saturating_sub(1))
        .unwrap_or(u32::MAX)
        .min(16);
    FAILURE_BACKOFF_BASE_SECS
        .saturating_mul(1_i64 << shift)
        .min(FAILURE_BACKOFF_MAX_SECS)
}

/// Addresses whose entries are due for revalidation, oldest first.
///
/// Drives the background refresher, which is what makes a rotated key get
/// picked up without anyone composing to that address first.
///
/// # Errors
///
/// Propagates any `rusqlite` error.
pub fn due_for_refresh(conn: &Connection, now: i64, limit: u32) -> rusqlite::Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT address FROM pgp_keys
          WHERE revalidate_after <= ?1
          ORDER BY revalidate_after ASC
          LIMIT ?2",
    )?;
    let rows = stmt.query_map(rusqlite::params![now, limit], |row| row.get::<_, String>(0))?;
    rows.collect()
}

// ---------------------------------------------------------------------------
// Trust on first use
// ---------------------------------------------------------------------------

/// What `pgp_key_history` says about a fingerprint just discovered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrustState {
    /// No fingerprint has been seen for this address before. Trusted on sight.
    FirstSight,
    /// The same fingerprint as before.
    Unchanged,
    /// A different fingerprint, not yet accepted by the user.
    ///
    /// The whole point of the table. See `V55__pgp_keys.sql`.
    Changed {
        /// The fingerprint previously accepted for this address.
        known: String,
    },
}

/// Classify a discovered fingerprint against what has been seen before.
///
/// # Errors
///
/// Propagates any `rusqlite` error.
pub fn trust_state(
    conn: &Connection,
    address: &str,
    fingerprint: &str,
) -> rusqlite::Result<TrustState> {
    let address = normalize_address(address);
    let mut stmt = conn.prepare(
        "SELECT fingerprint FROM pgp_key_history
          WHERE address = ?1 AND accepted_at IS NOT NULL
          ORDER BY first_seen_at ASC",
    )?;
    let accepted: Vec<String> = stmt
        .query_map(rusqlite::params![&address], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;

    if accepted.is_empty() {
        return Ok(TrustState::FirstSight);
    }
    if accepted.iter().any(|f| f == fingerprint) {
        return Ok(TrustState::Unchanged);
    }
    // Report the most recently accepted one as "known": it is the key the
    // user last made a decision about, and therefore the one they will
    // recognize in the warning.
    let known = accepted.last().cloned().unwrap_or_default();
    Ok(TrustState::Changed { known })
}

/// Note that a fingerprint was seen, accepting it if it is the first for the
/// address.
///
/// First-sight acceptance is automatic because there is nothing to compare
/// against and refusing to encrypt until a user ceremonially approves an
/// unfamiliar key would mean opportunistic encryption never starts. Every
/// *subsequent* fingerprint stays unaccepted until someone says so.
///
/// # Errors
///
/// Propagates any `rusqlite` error.
pub fn record_seen(
    conn: &Connection,
    address: &str,
    fingerprint: &str,
    source: KeySource,
    now: i64,
) -> rusqlite::Result<()> {
    let address = normalize_address(address);
    let known: i64 = conn.query_row(
        "SELECT count(*) FROM pgp_key_history WHERE address = ?1",
        rusqlite::params![&address],
        |row| row.get(0),
    )?;
    let accepted_at = if known == 0 { Some(now) } else { None };

    conn.execute(
        "INSERT INTO pgp_key_history
             (address, fingerprint, first_seen_at, last_seen_at, source, accepted_at)
         VALUES (?1, ?2, ?3, ?3, ?4, ?5)
         ON CONFLICT(address, fingerprint) DO UPDATE SET
             last_seen_at = excluded.last_seen_at,
             source = excluded.source",
        rusqlite::params![&address, fingerprint, now, source.as_str(), accepted_at],
    )?;
    Ok(())
}

/// Mark a fingerprint as accepted — the user looked at the warning and kept
/// the new key.
///
/// # Errors
///
/// Propagates any `rusqlite` error.
pub fn accept_fingerprint(
    conn: &Connection,
    address: &str,
    fingerprint: &str,
    now: i64,
) -> rusqlite::Result<usize> {
    let address = normalize_address(address);
    conn.execute(
        "UPDATE pgp_key_history SET accepted_at = ?3
          WHERE address = ?1 AND fingerprint = ?2",
        rusqlite::params![&address, fingerprint, now],
    )
}

/// Parse a `pgp_keys.source` token back into a [`KeySource`].
fn source_from_str(s: &str) -> Option<KeySource> {
    match s {
        "autocrypt" => Some(KeySource::Autocrypt),
        "wkd" => Some(KeySource::Wkd),
        "private_keyserver" => Some(KeySource::PrivateKeyserver),
        "public_keyserver" => Some(KeySource::PublicKeyserver),
        "manual" => Some(KeySource::Manual),
        _ => None,
    }
}
