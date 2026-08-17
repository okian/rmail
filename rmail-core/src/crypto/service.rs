//! The background half: turning "a recipient was set" into a cached answer.
//!
//! # Why this is a separate module from [`super::discover`]
//!
//! `discover` knows how to find one key. This knows *when* to look, how many
//! lookups may be in flight, and how not to start the same one twice. Those
//! are scheduling concerns, and folding them into the discovery chain would
//! mean every test of the privacy ordering had to also stand up a task
//! runtime.
//!
//! # Two things this must never do
//!
//! **Block the caller.** [`KeyService::observe_recipients`] returns as soon as
//! it has spawned what it needs to; it is called from the compose path, on
//! every recipient edit, and a version of it that awaited a keyserver would
//! turn a text field into a stall. The answer arrives in the cache and the UI
//! picks it up on its next status poll.
//!
//! **Stampede.** A user typing an address produces one call per keystroke, and
//! a naive implementation would start a fresh lookup for every prefix and then
//! a dozen more for the finished address. [`KeyService`] keeps an in-flight set
//! keyed on the normalized address, so the second through twelfth calls are
//! no-ops rather than twelve identical requests to someone else's keyserver.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use tokio::sync::Semaphore;
use tokio_util::sync::CancellationToken;

use crate::config::CryptoConfig;
use crate::storage::Database;

use super::cache;
use super::discover::{self, Fetcher, Outcome};
use super::normalize_address;

/// How many discoveries may run at once.
///
/// Small on purpose. This is background work nobody asked for, competing with
/// sync and search for the same runtime; the user-visible cost of it being
/// slow is a padlock that appears a second later, and the cost of it being
/// greedy is a mail client that stutters.
const MAX_CONCURRENT: usize = 4;

/// Schedules key discovery and writes the results to the cache.
///
/// Cheap to clone; every clone shares one in-flight set and one semaphore.
#[derive(Clone)]
pub struct KeyService {
    db: Database,
    fetcher: Arc<dyn Fetcher>,
    config: Arc<CryptoConfig>,
    in_flight: Arc<Mutex<HashSet<String>>>,
    permits: Arc<Semaphore>,
    cancel: CancellationToken,
}

impl std::fmt::Debug for KeyService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeyService")
            .field("in_flight", &self.in_flight.lock().map(|g| g.len()).ok())
            .finish_non_exhaustive()
    }
}

impl KeyService {
    /// Build one.
    #[must_use]
    pub fn new(
        db: Database,
        fetcher: Arc<dyn Fetcher>,
        config: Arc<CryptoConfig>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            db,
            fetcher,
            config,
            in_flight: Arc::new(Mutex::new(HashSet::new())),
            permits: Arc::new(Semaphore::new(MAX_CONCURRENT)),
            cancel,
        }
    }

    /// Note that a draft is addressed to `recipients`, and start discovery for
    /// any of them whose cached answer is missing or due for revalidation.
    ///
    /// Returns immediately. The number returned is how many lookups were
    /// actually started — zero on the common path where everything is cached,
    /// which is what makes calling this on every keystroke acceptable.
    ///
    /// # Errors
    ///
    /// Propagates storage errors from the cache read. A failure here means
    /// nothing was scheduled; it does not affect what the indicator shows,
    /// because that reads the cache independently.
    pub async fn observe_recipients(&self, recipients: &[String]) -> Result<usize, crate::Error> {
        if !self.config.auto_encrypt {
            return Ok(0);
        }
        let now = crate::crypto::service::now();
        let mut started = 0;

        for recipient in recipients {
            let address = normalize_address(recipient);
            if address.is_empty() || !address.contains('@') {
                // A half-typed address is not a lookup. Without this every
                // keystroke before the `@` becomes a request for a domain
                // that does not exist.
                continue;
            }

            let cached = {
                let address = address.clone();
                self.db
                    .read(move |conn| cache::lookup(conn, &address, now))
                    .await?
            };
            let needs_lookup = matches!(cached, cache::Cached::Stale { .. });
            if !needs_lookup {
                continue;
            }

            if !self.claim(&address) {
                continue;
            }
            self.spawn(address);
            started += 1;
        }
        Ok(started)
    }

    /// Take the in-flight slot for `address`, or report that someone else has
    /// it.
    fn claim(&self, address: &str) -> bool {
        self.in_flight
            .lock()
            .map(|mut set| set.insert(address.to_owned()))
            .unwrap_or(false)
    }

    fn release(&self, address: &str) {
        if let Ok(mut set) = self.in_flight.lock() {
            set.remove(address);
        }
    }

    /// Run one discovery to completion on a background task.
    fn spawn(&self, address: String) {
        let this = self.clone();
        tokio::spawn(async move {
            // The permit is taken *inside* the task so `observe_recipients`
            // never waits on a busy pool.
            let _permit = match this.permits.clone().acquire_owned().await {
                Ok(permit) => permit,
                Err(_) => {
                    this.release(&address);
                    return;
                }
            };

            // The Autocrypt source needs a connection, and holding one across
            // the network calls would both pin a pool slot for the whole
            // discovery and make this future non-`Send`. So the local source is
            // consulted first, on its own connection, and its answer is handed
            // to the chain as plain bytes.
            let autocrypt = if this.config.autocrypt {
                let address = address.clone();
                this.db
                    .read(move |conn| discover::autocrypt_key(conn, &address))
                    .await
                    .unwrap_or(None)
            } else {
                None
            };

            let outcome = discover::discover(
                autocrypt,
                this.fetcher.as_ref(),
                &address,
                &this.config,
                now(),
                &this.cancel,
            )
            .await;

            let config = Arc::clone(&this.config);
            let recorded = {
                let address = address.clone();
                this.db
                    .write(move |conn| record(conn, &address, outcome, &config, now()))
                    .await
            };
            if let Err(error) = recorded {
                tracing::warn!(%error, %address, "recording a key lookup failed");
            }
            this.release(&address);
        });
    }
}

/// Write a discovery outcome to the cache.
///
/// Split out of the task so the mapping from outcome to cache row — the part
/// that decides whether an address is suppressed for a month — is testable
/// without a runtime.
///
/// # Errors
///
/// Propagates any `rusqlite` error.
pub fn record(
    conn: &rusqlite::Connection,
    address: &str,
    outcome: Outcome,
    config: &CryptoConfig,
    now: i64,
) -> rusqlite::Result<()> {
    match outcome {
        Outcome::Found(key) => {
            let ttl = i64::try_from(std::time::Duration::from(config.key_ttl).as_secs())
                .unwrap_or(i64::MAX);
            cache::put_found(conn, &key, now, ttl)
        }
        Outcome::NotFound => {
            let ttl = i64::try_from(std::time::Duration::from(config.negative_ttl).as_secs())
                .unwrap_or(i64::MAX);
            cache::put_absent(conn, address, now, ttl)
        }
        // Deliberately not `put_absent`: see `crypto::cache`'s module docs.
        // An unreachable keyserver is not evidence that a person has no key.
        Outcome::Failed { reasons } => {
            tracing::debug!(%address, ?reasons, "key discovery failed; backing off");
            cache::record_failure(conn, address, now)
        }
    }
}

/// Wall-clock unix seconds.
fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}
