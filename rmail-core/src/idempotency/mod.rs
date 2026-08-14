//! The replay fence for mutating RPCs.
//!
//! prd.md: "mutating RPCs carry `idempotency_key` (UUID) — same key+hash
//! replays the cached response, critical for `Send`/`Move`/`Delete`."
//!
//! # The shape is `outbox`'s `smtp_message_id` fence, generalized
//!
//! [`crate::outbox`] already solved the hard half of this for SMTP: it writes
//! the message's `Message-ID` into the row and **commits before** the `DATA`
//! command, so a process that dies mid-send leaves evidence that a copy may
//! already be on the wire. Recording *after* the act cannot distinguish "never
//! started" from "died halfway", and for mail that difference is a second copy
//! in someone's inbox.
//!
//! The same three-way split applies to any mutating RPC, so this module makes
//! the same three moves:
//!
//! - [`IdempotencyStore::claim`] inserts and commits the fence, then the
//!   handler runs.
//! - [`IdempotencyStore::record`] fills in the response once the handler
//!   succeeded. A retry then replays those bytes and never touches the
//!   mutation.
//! - [`IdempotencyStore::release`] drops the fence when the handler *failed*,
//!   so a genuine retry genuinely retries.
//!
//! ## Why a failure releases the fence
//!
//! Caching an error would make one transient `UNAVAILABLE` — an IMAP server
//! that blinked — permanent for the life of that key: every retry would replay
//! the failure instead of reaching the server that has since come back. And
//! releasing is *safe* here specifically because of the ordering
//! [`crate::mail`] documents: every mutation reflects to IMAP **before** any
//! local row changes, so a mutation that returned an error either never
//! reached the server or failed at it. Nothing was applied, and the retry is
//! the first attempt again.
//!
//! The narrow exception is the window [`crate::mail`] also names: IMAP
//! succeeded and the local write then failed. A retry re-issues the IMAP
//! command — against a message that is no longer where the retry says it is,
//! which the server rejects. Bounded and self-correcting, and the alternative
//! (keep the fence) would strand every genuinely-failed mutation behind a key
//! that can never be retried.
//!
//! ## Why an unfinished claim is refused rather than reclaimed
//!
//! A claim with no recorded response means either "running right now" or "the
//! daemon died holding it". Neither can be told apart from the other, and both
//! answer the same question the same way: **do not run it again**. A retry
//! gets [`crate::ErrorReason::Aborted`], distinct from the `ALREADY_EXISTS`
//! that a *differing* payload gets, because one resolves on its own and the
//! other is a client bug.
//!
//! That is the same at-most-once trade `outbox` makes, and it is deliberate:
//! the remedy for a genuinely ambiguous mutation is a caller choosing a fresh
//! key — a decision — rather than a silent second application.
//!
//! ## The two windows are not the same length
//!
//! A *recorded* claim wants a long life: it is what makes a client's retry an
//! hour later replay instead of re-apply. An *unfinished* one wants a short
//! one, because the overwhelmingly common cause of it is not a crashed daemon
//! — it is a client whose deadline elapsed, or whose connection dropped, which
//! makes tonic drop the handler future before it can record or release. That
//! client's very next act is to retry the same key, and locking it out for a
//! day would break the exact workflow the key exists to support.
//!
//! So a claim is fenced for [`IdempotencyStore::in_flight`] until it reports an
//! outcome, and for the full retention afterwards. The short window still has
//! to be comfortably longer than any mutation on this path can take — every
//! one of them is bounded by a small multiple of `imap::IMAP_DEADLINE` (30s) —
//! so "unfinished and older than that" really does mean abandoned.
//!
//! # Keys are globally single-use
//!
//! `idempotency_keys.key` is the primary key, with the method folded into the
//! request hash rather than into the identity. So reusing one key across two
//! different RPCs is a payload conflict (`ALREADY_EXISTS`), never a replay of
//! the wrong method's response. For a value the *client* picks, failing closed
//! is the only safe direction.

use std::time::Duration;

use rusqlite::OptionalExtension;
use sha2::{Digest, Sha256};

use crate::error::Error;
use crate::storage::Database;

#[cfg(test)]
mod tests;

/// Longest accepted key. A UUID is 36 characters; this leaves room for a
/// caller that prefixes its own namespace, and stops an unbounded string from
/// becoming a way to write arbitrary bytes into the database.
pub const MAX_KEY_LEN: usize = 200;

/// `status_code` for a claim with no recorded outcome yet.
///
/// Not a value any gRPC code takes, which is the whole point: `Code::Ok` is
/// `0`, so "unfinished" and "cached success" would otherwise be the same row.
const IN_FLIGHT: i64 = -1;

/// What [`IdempotencyStore::claim`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    /// No prior attempt: this caller owns the mutation. Run it, then call
    /// [`IdempotencyStore::record`] (or [`IdempotencyStore::release`]).
    Fresh,
    /// An identical earlier call already succeeded. Return these bytes and do
    /// not run anything.
    Replay(Vec<u8>),
}

/// The fence table, over the shared database.
///
/// Cheap to clone: a clone shares the database handle.
#[derive(Clone, Debug)]
pub struct IdempotencyStore {
    db: Database,
    retention: Duration,
    in_flight: Duration,
}

impl IdempotencyStore {
    /// A store whose recorded claims lapse `retention` after they are taken,
    /// and whose *unfinished* ones lapse after `in_flight` — see the module
    /// docs on why those are different numbers.
    ///
    /// Both are floored at one second: `expires_at` is second-granular, so a
    /// sub-second value would make a claim expire the instant it was written
    /// and turn the fence into a silent no-op — the one failure mode an
    /// operator setting this could not detect.
    #[must_use]
    pub fn new(db: Database, retention: Duration, in_flight: Duration) -> Self {
        Self {
            db,
            retention: retention.max(Duration::from_secs(1)),
            in_flight: in_flight.max(Duration::from_secs(1)),
        }
    }

    /// Take the fence for `key`, or report what an earlier attempt left.
    ///
    /// `request` is the encoded request; the method is folded in, so the hash
    /// identifies the whole call and not merely its body.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] if the key is empty, too long, or not
    /// printable ASCII. [`Error::AlreadyExists`] if the key was used for a
    /// different request. [`Error::Aborted`] if an identical request holds an
    /// unfinished claim. Otherwise a mapped storage error.
    #[tracing::instrument(skip(self, request), fields(method = method))]
    pub async fn claim(&self, key: &str, method: &str, request: &[u8]) -> Result<Claim, Error> {
        validate_key(key)?;
        let hash = request_hash(method, request);
        let key = key.to_owned();
        let method = method.to_owned();
        // The *short* window: the claim starts unfinished, and `record` extends
        // it to the full retention once there is something to replay.
        let ttl = i64::try_from(self.in_flight.as_secs()).unwrap_or(i64::MAX);

        let existing = self
            .db
            .write(move |conn| {
                let tx = conn.transaction()?;
                // Reap first, in the same transaction: a lapsed claim must be
                // gone *before* the lookup below, or an expired key would
                // report itself as still fenced.
                tx.execute(
                    "DELETE FROM idempotency_keys WHERE expires_at <= unixepoch()",
                    [],
                )?;
                let found = tx
                    .query_row(
                        "SELECT request_hash, response, status_code
                         FROM idempotency_keys WHERE key = ?1",
                        [&key],
                        |row| {
                            Ok((
                                row.get::<_, Vec<u8>>(0)?,
                                row.get::<_, Option<Vec<u8>>>(1)?,
                                row.get::<_, i64>(2)?,
                            ))
                        },
                    )
                    .optional()?;
                if found.is_none() {
                    tx.execute(
                        "INSERT INTO idempotency_keys
                             (key, method, request_hash, response, status_code,
                              created_at, expires_at)
                         VALUES (?1, ?2, ?3, NULL, ?4, unixepoch(), unixepoch() + ?5)",
                        rusqlite::params![key, method, hash.to_vec(), IN_FLIGHT, ttl],
                    )?;
                }
                tx.commit()?;
                Ok(found)
            })
            .await?;

        let Some((stored_hash, response, status_code)) = existing else {
            return Ok(Claim::Fresh);
        };
        if stored_hash != hash {
            return Err(Error::already_exists(
                "this idempotency_key was already used for a different request; \
                 a key identifies one call, so reuse with a changed payload is refused",
            ));
        }
        match response {
            Some(bytes) => {
                tracing::info!(status_code, "replaying an idempotent response");
                Ok(Claim::Replay(bytes))
            }
            None => Err(Error::aborted(
                "an earlier attempt with this idempotency_key has not reported an outcome; \
                 it is not safe to apply the mutation again — retry shortly, or use a fresh \
                 key to apply it deliberately",
            )),
        }
    }

    /// Record the response a successful handler produced, so a retry replays
    /// it, and extend the fence from the in-flight window to the full
    /// retention.
    ///
    /// A claim that lapsed while the handler ran is logged, not raised: the
    /// mutation genuinely succeeded, and failing the call afterwards would be
    /// the worst possible answer — the caller would retry something that had
    /// already been applied.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    #[tracing::instrument(skip(self, response))]
    pub async fn record(&self, key: &str, response: Vec<u8>) -> Result<(), Error> {
        let key = key.to_owned();
        let ttl = i64::try_from(self.retention.as_secs()).unwrap_or(i64::MAX);
        let changed = self
            .db
            .write(move |conn| {
                conn.execute(
                    "UPDATE idempotency_keys
                     SET response = ?2, status_code = 0, expires_at = created_at + ?3
                     WHERE key = ?1 AND response IS NULL",
                    rusqlite::params![key, response, ttl],
                )
            })
            .await?;
        if changed == 0 {
            tracing::warn!(
                "idempotency claim vanished before its response could be recorded; \
                 a retry of this key will re-apply the mutation"
            );
        }
        Ok(())
    }

    /// Drop an unfinished claim so a genuine retry can retry — see the module
    /// docs on why a failed mutation releases rather than caches.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    #[tracing::instrument(skip(self))]
    pub async fn release(&self, key: &str) -> Result<(), Error> {
        let key = key.to_owned();
        self.db
            .write(move |conn| {
                conn.execute(
                    "DELETE FROM idempotency_keys WHERE key = ?1 AND response IS NULL",
                    [key],
                )
            })
            .await?;
        Ok(())
    }

    /// Drop every lapsed claim. [`IdempotencyStore::claim`] already does this
    /// on its own path; this exists for a maintenance sweep over a database
    /// nobody is calling mutations against.
    ///
    /// # Errors
    ///
    /// A mapped storage error.
    pub async fn purge_expired(&self) -> Result<usize, Error> {
        Ok(self
            .db
            .write(|conn| {
                conn.execute(
                    "DELETE FROM idempotency_keys WHERE expires_at <= unixepoch()",
                    [],
                )
            })
            .await?)
    }
}

/// SHA-256 over the method and the encoded request.
///
/// Length-prefixed so a method/body boundary cannot be shifted, which would
/// let two different calls hash alike and replay each other's response.
fn request_hash(method: &str, request: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(b"rmail.idempotency.v1");
    hasher.update((method.len() as u64).to_be_bytes());
    hasher.update(method.as_bytes());
    hasher.update((request.len() as u64).to_be_bytes());
    hasher.update(request);
    hasher.finalize().into()
}

/// Keys are printable ASCII and bounded.
///
/// Not because the database cares, but because the key is echoed into logs and
/// operator tooling; a control character or an unbounded blob there is a
/// problem the storage layer would happily accept.
fn validate_key(key: &str) -> Result<(), Error> {
    if key.is_empty() {
        return Err(Error::invalid_argument("idempotency_key must not be empty"));
    }
    if key.len() > MAX_KEY_LEN {
        return Err(Error::invalid_argument(format!(
            "idempotency_key must be at most {MAX_KEY_LEN} bytes"
        )));
    }
    if !key
        .bytes()
        .all(|b| b.is_ascii_graphic() || b == b' ' || b == b'-')
    {
        return Err(Error::invalid_argument(
            "idempotency_key must be printable ASCII",
        ));
    }
    Ok(())
}
