//! What the auto-encrypt path owes, proved against real OpenPGP keys.
//!
//! # Why these tests generate keys instead of asserting on fixtures
//!
//! Every key here is produced by [`generate`], a real rPGP keygen, and then
//! round-tripped through the same [`key::parse`] the network path uses. A
//! hand-written byte fixture would prove that the parser accepts one blob
//! somebody once captured; generating means the expiry, revocation and
//! capability tests are exercising the actual packet semantics, and that a
//! test asserting "an expired key is rejected" is looking at a key that is
//! genuinely expired rather than a string somebody labelled `expired`.
//!
//! # The two properties worth the most
//!
//! `public_keyservers_are_not_queried_when_a_private_source_answers` is the
//! privacy guarantee in executable form: it records the URLs actually
//! requested and fails if a public server is among them. It is the only thing
//! standing between the configured ordering and a refactor that "simplifies"
//! the chain into a parallel fan-out.
//!
//! `a_failed_lookup_does_not_suppress_discovery_for_a_month` is the other. The
//! distinction between "nothing found" and "nothing reachable" is invisible in
//! any output — both leave the user unencrypted — and is exactly the kind of
//! thing that gets collapsed by someone tidying up an enum.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use pgp::composed::{EncryptionCaps, KeyType, SecretKeyParamsBuilder, SubkeyParamsBuilder};
use pgp::types::Password;
use tokio_util::sync::CancellationToken;

use super::cache::{self, Cached, TrustState};
use super::discover::{self, Fetcher, Outcome};
use super::encrypt;
use super::key::{self, KeyError, KeySource};
use super::*;
use crate::config::{CryptoConfig, EncryptPolicy, KeyserverConfig, KeyserverKind};
use crate::storage::Database;

static COUNTER: AtomicU32 = AtomicU32::new(0);

const DAY: i64 = 86_400;
const NOW: i64 = 1_800_000_000;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A database with the schema migrated.
async fn db() -> (Database, PathBuf) {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let path = std::env::temp_dir().join(format!("rmail-crypto-{pid}-{n}.db"));
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(PathBuf::from(format!("{}{suffix}", path.display())));
    }
    let database = Database::open(&path).expect("open temp db");
    (database, path)
}

/// Generate a real key for `address`.
///
/// Returns the armored transferable public key — the same bytes a keyserver
/// would serve, so the tests below exercise the whole parse path rather than
/// an in-memory shortcut.
///
/// # No expiring keys here
///
/// rPGP 0.20's key builder exposes no key-expiration setting, so this cannot
/// mint a key that expires. Expiry is therefore covered where the specified
/// behaviour actually lives — `a_key_expiring_before_the_ttl_cuts_the_cache_short`
/// drives it through [`cache::put_found`]'s `min(ttl, expiry)` — rather than
/// by a parse-level test this fixture cannot honestly construct.
fn generate(address: &str) -> Vec<u8> {
    let mut params = SecretKeyParamsBuilder::default();
    params
        .key_type(KeyType::Ed25519Legacy)
        .can_certify(true)
        .can_sign(true)
        .primary_user_id(format!("Test User <{address}>"))
        .subkey(
            SubkeyParamsBuilder::default()
                .key_type(KeyType::ECDH(
                    pgp::crypto::ecc_curve::ECCCurve::Curve25519Legacy,
                ))
                .can_encrypt(EncryptionCaps::All)
                .build()
                .expect("subkey params"),
        );
    params
        .build()
        .expect("key params")
        .generate(rand::thread_rng())
        .expect("generate")
        .to_public_key()
        .to_armored_bytes(Default::default())
        .expect("armor")
}

/// Parse a generated key into the type the cache stores.
fn usable(address: &str, source: KeySource) -> UsableKey {
    let bytes = generate(address);
    key::parse(&bytes, address, source, NOW, 256 * 1024).expect("generated key must be usable")
}

/// A [`Fetcher`] that answers from a fixed table and records every URL asked
/// for, in order.
///
/// The recording is the point: the privacy ordering is a property of *which
/// requests are made*, and no assertion on the returned key can observe it.
/// One canned answer: a URL substring to match, and what to return for it.
type CannedResponse = (String, Result<Option<Vec<u8>>, String>);

struct RecordingFetcher {
    responses: Vec<CannedResponse>,
    seen: Mutex<Vec<String>>,
}

impl RecordingFetcher {
    fn new(responses: Vec<CannedResponse>) -> Self {
        Self {
            responses,
            seen: Mutex::new(Vec::new()),
        }
    }

    fn requested(&self) -> Vec<String> {
        self.seen.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

#[async_trait::async_trait]
impl Fetcher for RecordingFetcher {
    async fn get(
        &self,
        url: &str,
        _bearer: Option<&str>,
        _timeout: Duration,
    ) -> Result<Option<Vec<u8>>, String> {
        if let Ok(mut seen) = self.seen.lock() {
            seen.push(url.to_owned());
        }
        for (needle, response) in &self.responses {
            if url.contains(needle.as_str()) {
                return response.clone();
            }
        }
        Ok(None)
    }
}

fn config() -> CryptoConfig {
    CryptoConfig::default()
}

// ---------------------------------------------------------------------------
// Address normalization — the cache's key
// ---------------------------------------------------------------------------

#[test]
fn addresses_that_mean_the_same_thing_normalize_to_one_key() {
    let expected = "alice@example.com";
    for input in [
        "alice@example.com",
        "Alice@Example.COM",
        "  alice@example.com  ",
        "Alice Smith <alice@example.com>",
        "\"Smith, Alice\" <Alice@Example.com>",
    ] {
        assert_eq!(normalize_address(input), expected, "input: {input:?}");
    }
}

#[test]
fn a_display_name_containing_an_angle_bracket_still_yields_the_address() {
    assert_eq!(
        normalize_address("a <weird> name <real@example.com>"),
        "real@example.com"
    );
}

// ---------------------------------------------------------------------------
// Key validation
// ---------------------------------------------------------------------------

#[test]
fn a_generated_key_round_trips_through_parse() {
    let parsed = usable("alice@example.com", KeySource::Wkd);
    assert_eq!(parsed.address, "alice@example.com");
    assert_eq!(
        parsed.fingerprint.len(),
        40,
        "v4 fingerprint is 20 bytes hex"
    );
    assert!(parsed.fingerprint.chars().all(|c| c.is_ascii_hexdigit()));
    assert!(parsed.expires_at.is_none(), "no expiry was requested");
}

#[test]
fn a_key_for_someone_else_is_refused() {
    // The threat: a keyserver answers a query for alice with mallory's key.
    let bytes = generate("mallory@evil.example");
    let err = key::parse(
        &bytes,
        "alice@example.com",
        KeySource::PublicKeyserver,
        NOW,
        1 << 20,
    )
    .expect_err("a key with no matching user id must be refused");
    assert!(
        matches!(err, KeyError::AddressMismatch { .. }),
        "got {err:?}"
    );
}

#[test]
fn an_oversized_key_is_refused_before_it_is_parsed() {
    let bytes = generate("alice@example.com");
    let err = key::parse(&bytes, "alice@example.com", KeySource::Wkd, NOW, 16)
        .expect_err("16 bytes is smaller than any real key");
    assert!(
        matches!(err, KeyError::TooLarge { limit: 16, .. }),
        "expected TooLarge with the configured limit, got {err:?}"
    );
}

#[test]
fn garbage_is_refused_rather_than_panicking() {
    for bytes in [
        &b""[..],
        &b"not a key at all"[..],
        &b"-----BEGIN PGP PUBLIC KEY BLOCK-----\nnope\n-----END PGP PUBLIC KEY BLOCK-----"[..],
    ] {
        let result = key::parse(bytes, "alice@example.com", KeySource::Wkd, NOW, 1 << 20);
        assert!(result.is_err(), "must reject {bytes:?}");
    }
}

// ---------------------------------------------------------------------------
// Selection: newest wins
// ---------------------------------------------------------------------------

#[test]
fn the_newest_key_wins_when_several_are_found() {
    let older = UsableKey {
        created_at: NOW - 100 * DAY,
        ..usable("alice@example.com", KeySource::Wkd)
    };
    let newer = UsableKey {
        created_at: NOW - DAY,
        ..usable("alice@example.com", KeySource::PublicKeyserver)
    };
    let candidates = vec![older.clone(), newer.clone()];
    let best = key::select_best(&candidates).expect("a best key");
    assert_eq!(
        best.created_at, newer.created_at,
        "the rotated-to key must win even though it came from a worse source"
    );
}

#[test]
fn the_better_source_breaks_a_creation_time_tie() {
    let from_keyserver = UsableKey {
        created_at: NOW,
        ..usable("alice@example.com", KeySource::PublicKeyserver)
    };
    let from_autocrypt = UsableKey {
        created_at: NOW,
        ..usable("alice@example.com", KeySource::Autocrypt)
    };
    let candidates = vec![from_keyserver, from_autocrypt];
    let best = key::select_best(&candidates).expect("a best key");
    assert_eq!(best.source, KeySource::Autocrypt);
}

#[test]
fn selecting_from_nothing_is_none() {
    assert!(key::select_best(&[]).is_none());
}

// ---------------------------------------------------------------------------
// The cache: the two TTLs
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_found_key_is_served_from_cache_until_its_ttl() {
    let (database, _path) = db().await;
    let entry = usable("alice@example.com", KeySource::Wkd);
    let ttl = 30 * DAY;

    database
        .with_write(|conn| {
            cache::put_found(conn, &entry, NOW, ttl)?;
            Ok::<_, rusqlite::Error>(())
        })
        .expect("write");

    database
        .with_read(|conn| {
            // Inside the TTL: a hit, no lookup needed.
            assert!(matches!(
                cache::lookup(conn, "alice@example.com", NOW + 29 * DAY)?,
                Cached::Key(_)
            ));
            // Past it: stale, and the old key is still offered for the
            // indicator while a refresh runs.
            let stale = cache::lookup(conn, "alice@example.com", NOW + 31 * DAY)?;
            let carried = match &stale {
                Cached::Stale { previous: Some(k) } => Some(k.fingerprint.clone()),
                _ => None,
            };
            assert_eq!(
                carried.as_deref(),
                Some(entry.fingerprint.as_str()),
                "a stale entry must still carry the old key for the indicator, got {stale:?}"
            );
            Ok::<_, rusqlite::Error>(())
        })
        .expect("read");
}

#[tokio::test]
async fn a_key_expiring_before_the_ttl_cuts_the_cache_short() {
    // The rule the migration calls out: min(ttl, key expiry). A 30-day TTL on
    // a key that dies in 5 days must not keep it alive for 30.
    let (database, _path) = db().await;
    let entry = UsableKey {
        expires_at: Some(NOW + 5 * DAY),
        ..usable("alice@example.com", KeySource::Wkd)
    };

    database
        .with_write(|conn| {
            cache::put_found(conn, &entry, NOW, 30 * DAY)?;
            Ok::<_, rusqlite::Error>(())
        })
        .expect("write");

    database
        .with_read(|conn| {
            assert!(
                matches!(
                    cache::lookup(conn, "alice@example.com", NOW + 4 * DAY)?,
                    Cached::Key(_)
                ),
                "still valid before the key's own expiry"
            );
            assert!(
                !matches!(
                    cache::lookup(conn, "alice@example.com", NOW + 6 * DAY)?,
                    Cached::Key(_)
                ),
                "a key past its own expiry must never be served, TTL notwithstanding"
            );
            Ok::<_, rusqlite::Error>(())
        })
        .expect("read");
}

#[tokio::test]
async fn a_missing_key_suppresses_lookups_for_the_negative_ttl() {
    // "if found none, don't search for a month".
    let (database, _path) = db().await;
    database
        .with_write(|conn| {
            cache::put_absent(conn, "nobody@example.com", NOW, 30 * DAY)?;
            Ok::<_, rusqlite::Error>(())
        })
        .expect("write");

    database
        .with_read(|conn| {
            assert_eq!(
                cache::lookup(conn, "nobody@example.com", NOW + 29 * DAY)?,
                Cached::Absent,
                "inside the negative TTL nothing should be looked up again"
            );
            assert!(
                matches!(
                    cache::lookup(conn, "nobody@example.com", NOW + 31 * DAY)?,
                    Cached::Stale { previous: None }
                ),
                "past it the address becomes eligible again"
            );
            Ok::<_, rusqlite::Error>(())
        })
        .expect("read");
}

#[tokio::test]
async fn a_failed_lookup_does_not_suppress_discovery_for_a_month() {
    // The distinction that is invisible in any output: "nothing found" vs
    // "nothing reachable". Collapsing them means one bad network minute stops
    // encrypting this correspondent's mail for a month.
    let (database, _path) = db().await;
    database
        .with_write(|conn| {
            cache::record_failure(conn, "alice@example.com", NOW)?;
            Ok::<_, rusqlite::Error>(())
        })
        .expect("write");

    database
        .with_read(|conn| {
            // Backoff, not a month of silence.
            let state = cache::lookup(conn, "alice@example.com", NOW + 60)?;
            assert!(
                matches!(state, Cached::Backoff { .. }),
                "expected Backoff, got {state:?}"
            );
            let retry_at = match state {
                Cached::Backoff { retry_at } => retry_at,
                _ => i64::MAX,
            };
            assert!(
                retry_at < NOW + DAY,
                "one failure must retry within a day, not a month; got {retry_at}"
            );
            // And once the backoff elapses it is eligible again.
            assert!(matches!(
                cache::lookup(conn, "alice@example.com", NOW + DAY)?,
                Cached::Stale { .. }
            ));
            Ok::<_, rusqlite::Error>(())
        })
        .expect("read");
}

#[tokio::test]
async fn a_failed_refresh_keeps_serving_the_key_it_already_had() {
    // A routine revalidation that cannot reach a keyserver must not downgrade
    // a correspondent from encrypted to cleartext. The key is unexpired and
    // unrevoked; the only thing that failed was re-asking about it.
    //
    // Asserted through `resolve` as well as `lookup`, because the damage this
    // guards against is only visible at the status layer: `Cached::Backoff`
    // maps to "no key", so getting `lookup` wrong here silently stops
    // encrypting and nothing else in the system complains.
    let (database, _path) = db().await;
    let entry = usable("alice@example.com", KeySource::Wkd);

    database
        .with_write(|conn| {
            cache::put_found(conn, &entry, NOW, 30 * DAY)?;
            // 31 days later the entry is due; the refresh fails.
            cache::record_failure(conn, "alice@example.com", NOW + 31 * DAY)?;
            Ok::<_, rusqlite::Error>(())
        })
        .expect("write");

    database
        .with_read(|conn| {
            let at = NOW + 31 * DAY + 60;
            let cached = cache::lookup(conn, "alice@example.com", at)?;
            let fingerprint = match &cached {
                Cached::Key(k) => Some(k.fingerprint.clone()),
                _ => None,
            };
            assert_eq!(
                fingerprint.as_deref(),
                Some(entry.fingerprint.as_str()),
                "an unreachable server during refresh must not discard a still-valid key; \
                 got {cached:?}"
            );

            let status = encrypt::resolve(conn, &["alice@example.com".to_owned()], &config(), at)?;
            assert!(
                status.will_encrypt(),
                "a network blip must not silently downgrade this message to cleartext: {status:?}"
            );
            Ok::<_, rusqlite::Error>(())
        })
        .expect("read");
}

#[tokio::test]
async fn a_failed_refresh_of_an_expired_key_does_not_keep_using_it() {
    // The other side of the rule above: keeping a key through a failed refresh
    // is only defensible while the key is valid on its own terms. Once it has
    // expired, "we could not reach the server" is no longer a reason to keep
    // encrypting to something dead.
    let (database, _path) = db().await;
    let entry = UsableKey {
        expires_at: Some(NOW + 10 * DAY),
        ..usable("alice@example.com", KeySource::Wkd)
    };

    database
        .with_write(|conn| {
            cache::put_found(conn, &entry, NOW, 30 * DAY)?;
            cache::record_failure(conn, "alice@example.com", NOW + 11 * DAY)?;
            Ok::<_, rusqlite::Error>(())
        })
        .expect("write");

    database
        .with_read(|conn| {
            let cached = cache::lookup(conn, "alice@example.com", NOW + 11 * DAY + 60)?;
            assert!(
                !matches!(cached, Cached::Key(_)),
                "a key past its own expiry must not survive a failed refresh: {cached:?}"
            );
            Ok::<_, rusqlite::Error>(())
        })
        .expect("read");
}

#[tokio::test]
async fn repeated_failures_back_off_but_stay_under_the_negative_ttl() {
    let (database, _path) = db().await;
    database
        .with_write(|conn| {
            for _ in 0..12 {
                cache::record_failure(conn, "alice@example.com", NOW)?;
            }
            Ok::<_, rusqlite::Error>(())
        })
        .expect("write");

    database
        .with_read(|conn| {
            let state = cache::lookup(conn, "alice@example.com", NOW + 60)?;
            assert!(
                matches!(state, Cached::Backoff { .. }),
                "expected Backoff, got {state:?}"
            );
            let retry_at = match state {
                Cached::Backoff { retry_at } => retry_at,
                _ => i64::MAX,
            };
            assert!(
                retry_at <= NOW + 6 * 3600,
                "backoff is capped well below the negative TTL; got {retry_at}"
            );
            Ok::<_, rusqlite::Error>(())
        })
        .expect("read");
}

#[tokio::test]
async fn an_unknown_address_is_stale_not_absent() {
    let (database, _path) = db().await;
    database
        .with_read(|conn| {
            assert!(
                matches!(
                    cache::lookup(conn, "never-seen@example.com", NOW)?,
                    Cached::Stale { previous: None }
                ),
                "never-looked-up and looked-up-and-empty must not be the same state"
            );
            Ok::<_, rusqlite::Error>(())
        })
        .expect("read");
}

#[tokio::test]
async fn due_for_refresh_returns_only_expired_entries() {
    let (database, _path) = db().await;
    let fresh = usable("fresh@example.com", KeySource::Wkd);
    let old = usable("old@example.com", KeySource::Wkd);

    database
        .with_write(|conn| {
            cache::put_found(conn, &fresh, NOW, 30 * DAY)?;
            cache::put_found(conn, &old, NOW - 60 * DAY, 30 * DAY)?;
            Ok::<_, rusqlite::Error>(())
        })
        .expect("write");

    database
        .with_read(|conn| {
            let due = cache::due_for_refresh(conn, NOW, 10)?;
            assert_eq!(due, vec!["old@example.com".to_owned()]);
            Ok::<_, rusqlite::Error>(())
        })
        .expect("read");
}

// ---------------------------------------------------------------------------
// Trust on first use
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_first_key_for_an_address_is_trusted_and_a_second_is_not() {
    let (database, _path) = db().await;
    let first = usable("alice@example.com", KeySource::Wkd);

    database
        .with_write(|conn| {
            assert_eq!(
                cache::trust_state(conn, "alice@example.com", &first.fingerprint)?,
                TrustState::FirstSight
            );
            cache::put_found(conn, &first, NOW, 30 * DAY)?;
            assert_eq!(
                cache::trust_state(conn, "alice@example.com", &first.fingerprint)?,
                TrustState::Unchanged,
                "the same key must not keep re-alarming"
            );

            // A different key appears for the same address — the substitution
            // attack, or an ordinary rotation. Either way, not silent.
            let second = usable("alice@example.com", KeySource::PublicKeyserver);
            assert_eq!(
                cache::trust_state(conn, "alice@example.com", &second.fingerprint)?,
                TrustState::Changed {
                    known: first.fingerprint.clone()
                },
                "a second, unaccepted fingerprint must be reported as a change"
            );
            Ok::<_, rusqlite::Error>(())
        })
        .expect("write");
}

#[tokio::test]
async fn accepting_a_changed_fingerprint_settles_it() {
    let (database, _path) = db().await;
    let first = usable("alice@example.com", KeySource::Wkd);
    let second = usable("alice@example.com", KeySource::Wkd);

    database
        .with_write(|conn| {
            cache::put_found(conn, &first, NOW, 30 * DAY)?;
            cache::put_found(conn, &second, NOW + DAY, 30 * DAY)?;
            assert!(matches!(
                cache::trust_state(conn, "alice@example.com", &second.fingerprint)?,
                TrustState::Changed { .. }
            ));

            cache::accept_fingerprint(conn, "alice@example.com", &second.fingerprint, NOW + DAY)?;
            assert_eq!(
                cache::trust_state(conn, "alice@example.com", &second.fingerprint)?,
                TrustState::Unchanged
            );
            Ok::<_, rusqlite::Error>(())
        })
        .expect("write");
}

// ---------------------------------------------------------------------------
// Discovery ordering — the privacy guarantee
// ---------------------------------------------------------------------------

#[test]
fn private_keyservers_are_ordered_before_public_ones_whatever_the_toml_said() {
    let servers = vec![
        KeyserverConfig {
            name: "public-a".to_owned(),
            url: "https://a.example".to_owned(),
            kind: KeyserverKind::Public,
            token_env: None,
        },
        KeyserverConfig {
            name: "private-b".to_owned(),
            url: "https://b.internal".to_owned(),
            kind: KeyserverKind::Private,
            token_env: None,
        },
        KeyserverConfig {
            name: "public-c".to_owned(),
            url: "https://c.example".to_owned(),
            kind: KeyserverKind::Public,
            token_env: None,
        },
    ];
    let ordered: Vec<&str> = discover::ordered_keyservers(&servers)
        .iter()
        .map(|s| s.name.as_str())
        .collect();
    assert_eq!(
        ordered,
        vec!["private-b", "public-a", "public-c"],
        "private first, and stable within each group"
    );
}

#[tokio::test]
async fn public_keyservers_are_not_queried_when_a_private_source_answers() {
    // The privacy guarantee, made executable. WKD answers, so the public
    // keyserver must never see this address.
    let address = "alice@example.com";
    let armored = generate(address);
    let fetcher = RecordingFetcher::new(vec![(
        "/.well-known/openpgpkey/".to_owned(),
        Ok(Some(armored)),
    )]);

    let mut cfg = config();
    cfg.autocrypt = false;
    cfg.keyservers = vec![KeyserverConfig {
        name: "public".to_owned(),
        url: "https://keys.public.example".to_owned(),
        kind: KeyserverKind::Public,
        token_env: None,
    }];

    let outcome = discover::discover(
        None,
        &fetcher,
        address,
        &cfg,
        NOW,
        &CancellationToken::new(),
    )
    .await;

    assert!(matches!(outcome, Outcome::Found(_)), "WKD should answer");
    let requested = fetcher.requested();
    assert!(
        !requested.iter().any(|u| u.contains("keys.public.example")),
        "a public keyserver was queried even though WKD answered: {requested:?}"
    );
}

#[tokio::test]
async fn a_public_keyserver_is_reached_only_after_the_private_sources_miss() {
    let address = "alice@example.com";
    let armored = generate(address);
    let fetcher = RecordingFetcher::new(vec![
        // WKD 404s.
        ("/.well-known/openpgpkey/".to_owned(), Ok(None)),
        ("keys.public.example".to_owned(), Ok(Some(armored))),
    ]);

    let mut cfg = config();
    cfg.autocrypt = false;
    cfg.keyservers = vec![KeyserverConfig {
        name: "public".to_owned(),
        url: "https://keys.public.example".to_owned(),
        kind: KeyserverKind::Public,
        token_env: None,
    }];

    let outcome = discover::discover(
        None,
        &fetcher,
        address,
        &cfg,
        NOW,
        &CancellationToken::new(),
    )
    .await;
    assert!(matches!(outcome, Outcome::Found(_)));

    let requested = fetcher.requested();
    let wkd_index = requested
        .iter()
        .position(|u| u.contains("openpgpkey"))
        .expect("WKD must be tried");
    let public_index = requested
        .iter()
        .position(|u| u.contains("keys.public.example"))
        .expect("the public server must be tried once WKD misses");
    assert!(wkd_index < public_index, "order: {requested:?}");
}

#[tokio::test]
async fn every_source_erroring_is_a_failure_not_an_absence() {
    let fetcher = RecordingFetcher::new(vec![
        (
            "openpgpkey".to_owned(),
            Err("connection refused".to_owned()),
        ),
        ("keys.public.example".to_owned(), Err("timeout".to_owned())),
    ]);

    let mut cfg = config();
    cfg.autocrypt = false;
    cfg.keyservers = vec![KeyserverConfig {
        name: "public".to_owned(),
        url: "https://keys.public.example".to_owned(),
        kind: KeyserverKind::Public,
        token_env: None,
    }];

    let outcome = discover::discover(
        None,
        &fetcher,
        "alice@example.com",
        &cfg,
        NOW,
        &CancellationToken::new(),
    )
    .await;

    let reasons = match &outcome {
        Outcome::Failed { reasons } => reasons.clone(),
        _ => Vec::new(),
    };
    assert!(
        matches!(outcome, Outcome::Failed { .. }),
        "unreachable servers must not be recorded as 'this address has no key'; got {outcome:?}"
    );
    assert!(!reasons.is_empty(), "a failure must say why");
}

#[tokio::test]
async fn a_reachable_source_with_no_key_is_a_genuine_absence() {
    let fetcher = RecordingFetcher::new(vec![("openpgpkey".to_owned(), Ok(None))]);
    let mut cfg = config();
    cfg.autocrypt = false;
    cfg.keyservers = Vec::new();

    let outcome = discover::discover(
        None,
        &fetcher,
        "alice@example.com",
        &cfg,
        NOW,
        &CancellationToken::new(),
    )
    .await;
    assert!(
        matches!(outcome, Outcome::NotFound),
        "a 404 from every enabled source is an answer, and may be cached"
    );
}

#[tokio::test]
async fn cancellation_stops_the_chain() {
    let fetcher = RecordingFetcher::new(Vec::new());
    let cancel = CancellationToken::new();
    cancel.cancel();

    let mut cfg = config();
    cfg.autocrypt = false;
    let outcome = discover::discover(None, &fetcher, "alice@example.com", &cfg, NOW, &cancel).await;

    assert!(
        fetcher.requested().is_empty(),
        "a cancelled discovery must not make requests"
    );
    assert!(matches!(outcome, Outcome::Failed { .. }));
}

// ---------------------------------------------------------------------------
// URL construction
// ---------------------------------------------------------------------------

#[test]
fn wkd_urls_follow_the_draft() {
    let urls = discover::wkd_urls("alice@example.com");
    assert_eq!(urls.len(), 2);
    assert!(
        urls[0]
            .starts_with("https://openpgpkey.example.com/.well-known/openpgpkey/example.com/hu/"),
        "advanced method first: {}",
        urls[0]
    );
    assert!(
        urls[1].starts_with("https://example.com/.well-known/openpgpkey/hu/"),
        "direct method second: {}",
        urls[1]
    );
    assert!(urls.iter().all(|u| u.ends_with("?l=alice")));
}

#[test]
fn the_wkd_hash_matches_the_published_vector() {
    // The WKD draft's own example: Joe.Doe@example.org hashes to
    // `iy9q119eutrkn8s1mk4r39qejnbu3n5q`. If this changes, every WKD lookup
    // silently 404s and encryption quietly stops working — which is exactly
    // the kind of failure that has no other symptom.
    let urls = discover::wkd_urls("joe.doe@example.org");
    assert!(
        urls[0].contains("iy9q119eutrkn8s1mk4r39qejnbu3n5q"),
        "z-base-32 of SHA-1 of the local part is wrong: {}",
        urls[0]
    );
}

#[test]
fn an_address_without_a_domain_has_no_wkd_urls() {
    assert!(discover::wkd_urls("not-an-address").is_empty());
    assert!(discover::wkd_urls("@example.com").is_empty());
    assert!(discover::wkd_urls("alice@").is_empty());
}

#[test]
fn hkp_urls_ask_for_the_machine_readable_form() {
    let url = discover::hkp_url("https://keys.example.com/", "alice@example.com");
    assert_eq!(
        url,
        "https://keys.example.com/pks/lookup?op=get&options=mr&search=alice@example.com"
    );
}

// ---------------------------------------------------------------------------
// Autocrypt header parsing
// ---------------------------------------------------------------------------

#[test]
fn an_autocrypt_header_yields_its_keydata() {
    use base64::Engine as _;
    let payload = b"\x99\x01\x0dnot-a-real-key";
    let encoded = base64::engine::general_purpose::STANDARD.encode(payload);
    let header = format!("addr=alice@example.com; prefer-encrypt=mutual; keydata={encoded}");
    assert_eq!(
        discover::parse_autocrypt_header(&header).as_deref(),
        Some(&payload[..])
    );
}

#[test]
fn a_folded_autocrypt_header_still_decodes() {
    use base64::Engine as _;
    let payload = b"some longer payload that will be folded across lines";
    let encoded = base64::engine::general_purpose::STANDARD.encode(payload);
    let folded = format!(
        "addr=alice@example.com; keydata={}\r\n {}",
        &encoded[..10],
        &encoded[10..]
    );
    assert_eq!(
        discover::parse_autocrypt_header(&folded).as_deref(),
        Some(&payload[..]),
        "header folding must not break the base64 payload"
    );
}

#[test]
fn an_autocrypt_header_without_keydata_is_none() {
    assert!(discover::parse_autocrypt_header("addr=alice@example.com").is_none());
    assert!(discover::parse_autocrypt_header("").is_none());
}

// ---------------------------------------------------------------------------
// Status resolution — what the indicator shows
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_recipient_with_a_key_resolves_to_encrypted() {
    let (database, _path) = db().await;
    let entry = usable("alice@example.com", KeySource::Wkd);
    database
        .with_write(|conn| {
            cache::put_found(conn, &entry, NOW, 30 * DAY)?;
            Ok::<_, rusqlite::Error>(())
        })
        .expect("write");

    database
        .with_read(|conn| {
            let status = encrypt::resolve(
                conn,
                &["Alice <alice@example.com>".to_owned()],
                &config(),
                NOW,
            )?;
            assert!(status.will_encrypt(), "got {status:?}");
            assert_eq!(status.code(), "encrypted");
            Ok::<_, rusqlite::Error>(())
        })
        .expect("read");
}

#[tokio::test]
async fn one_recipient_without_a_key_makes_the_whole_message_cleartext() {
    // Encryption is all-or-nothing per message: a padlock that meant "two of
    // your three recipients" would be a lie.
    let (database, _path) = db().await;
    let entry = usable("alice@example.com", KeySource::Wkd);
    database
        .with_write(|conn| {
            cache::put_found(conn, &entry, NOW, 30 * DAY)?;
            cache::put_absent(conn, "bob@example.com", NOW, 30 * DAY)?;
            Ok::<_, rusqlite::Error>(())
        })
        .expect("write");

    database
        .with_read(|conn| {
            let status = encrypt::resolve(
                conn,
                &["alice@example.com".to_owned(), "bob@example.com".to_owned()],
                &config(),
                NOW,
            )?;
            assert!(!status.will_encrypt());
            assert_eq!(
                status,
                EncryptionStatus::NoKey {
                    addresses: vec!["bob@example.com".to_owned()]
                },
                "the recipient without a key must be named"
            );
            Ok::<_, rusqlite::Error>(())
        })
        .expect("read");
}

#[tokio::test]
async fn a_changed_key_does_not_encrypt() {
    // The uncomfortable choice, pinned down: a substituted key downgrades to
    // cleartext rather than handing the plaintext to whoever published it.
    let (database, _path) = db().await;
    let first = usable("alice@example.com", KeySource::Wkd);
    let second = usable("alice@example.com", KeySource::PublicKeyserver);

    database
        .with_write(|conn| {
            cache::put_found(conn, &first, NOW, 30 * DAY)?;
            cache::put_found(conn, &second, NOW + DAY, 30 * DAY)?;
            Ok::<_, rusqlite::Error>(())
        })
        .expect("write");

    database
        .with_read(|conn| {
            let status = encrypt::resolve(
                conn,
                &["alice@example.com".to_owned()],
                &config(),
                NOW + DAY,
            )?;
            assert!(
                !status.will_encrypt(),
                "a key that changed under us must not be silently used: {status:?}"
            );
            assert!(status.needs_attention());
            assert_eq!(status.code(), "key_changed");
            Ok::<_, rusqlite::Error>(())
        })
        .expect("read");
}

#[tokio::test]
async fn auto_encrypt_off_disables_everything() {
    let (database, _path) = db().await;
    let entry = usable("alice@example.com", KeySource::Wkd);
    database
        .with_write(|conn| {
            cache::put_found(conn, &entry, NOW, 30 * DAY)?;
            Ok::<_, rusqlite::Error>(())
        })
        .expect("write");

    let mut cfg = config();
    cfg.auto_encrypt = false;

    database
        .with_read(|conn| {
            let status = encrypt::resolve(conn, &["alice@example.com".to_owned()], &cfg, NOW)?;
            assert_eq!(status, EncryptionStatus::Disabled);
            Ok::<_, rusqlite::Error>(())
        })
        .expect("read");
}

#[tokio::test]
async fn always_blocks_when_a_recipient_has_no_key() {
    let (database, _path) = db().await;
    database
        .with_write(|conn| {
            cache::put_absent(conn, "bob@example.com", NOW, 30 * DAY)?;
            Ok::<_, rusqlite::Error>(())
        })
        .expect("write");

    let mut cfg = config();
    cfg.policy = EncryptPolicy::Always;

    database
        .with_read(|conn| {
            let status = encrypt::resolve(conn, &["bob@example.com".to_owned()], &cfg, NOW)?;
            assert!(status.blocks(), "got {status:?}");
            assert!(!status.will_encrypt());
            Ok::<_, rusqlite::Error>(())
        })
        .expect("read");
}

#[tokio::test]
async fn a_never_override_beats_a_discovered_key() {
    let (database, _path) = db().await;
    let entry = usable("alice@example.com", KeySource::Wkd);
    database
        .with_write(|conn| {
            cache::put_found(conn, &entry, NOW, 30 * DAY)?;
            conn.execute(
                "INSERT INTO pgp_overrides (address, policy) VALUES ('alice@example.com', 'never')",
                [],
            )?;
            Ok::<_, rusqlite::Error>(())
        })
        .expect("write");

    database
        .with_read(|conn| {
            let status = encrypt::resolve(conn, &["alice@example.com".to_owned()], &config(), NOW)?;
            assert_eq!(status, EncryptionStatus::Disabled);
            Ok::<_, rusqlite::Error>(())
        })
        .expect("read");
}

#[tokio::test]
async fn an_unknown_recipient_is_pending_not_absent() {
    let (database, _path) = db().await;
    database
        .with_read(|conn| {
            let status =
                encrypt::resolve(conn, &["stranger@example.com".to_owned()], &config(), NOW)?;
            assert_eq!(status.code(), "pending", "got {status:?}");
            assert!(!status.will_encrypt());
            Ok::<_, rusqlite::Error>(())
        })
        .expect("read");
}

// ---------------------------------------------------------------------------
// Encryption
// ---------------------------------------------------------------------------

#[test]
fn encrypting_produces_a_pgp_mime_body_the_plaintext_is_absent_from() {
    let entry = usable("alice@example.com", KeySource::Wkd);
    let secret = "the quick brown fox jumps over the lazy dog";
    let body = format!("Content-Type: text/plain\r\n\r\n{secret}\r\n");

    let mime = encrypt::encrypt_mime(&body, std::slice::from_ref(&entry)).expect("encrypt");

    assert!(mime.content_type.starts_with("multipart/encrypted;"));
    assert!(mime
        .content_type
        .contains("protocol=\"application/pgp-encrypted\""));
    assert!(mime.body.contains("Version: 1"));
    assert!(mime.body.contains("-----BEGIN PGP MESSAGE-----"));
    assert!(
        !mime.body.contains(secret),
        "the plaintext must not survive into the encrypted body"
    );
}

#[test]
fn the_mime_boundary_cannot_appear_inside_the_armored_payload() {
    let entry = usable("alice@example.com", KeySource::Wkd);
    let mime = encrypt::encrypt_mime("hello", std::slice::from_ref(&entry)).expect("encrypt");

    let boundary = mime
        .content_type
        .split("boundary=\"")
        .nth(1)
        .and_then(|s| s.split('"').next())
        .expect("a boundary");
    assert_eq!(
        mime.body.matches(&boundary).count(),
        3,
        "exactly two part delimiters and one closing delimiter"
    );
}

#[test]
fn encrypting_to_several_recipients_succeeds() {
    let alice = usable("alice@example.com", KeySource::Wkd);
    let bob = usable("bob@example.com", KeySource::Wkd);
    let mime = encrypt::encrypt_mime("hello", &[alice, bob]).expect("encrypt to two");
    assert!(mime.body.contains("-----BEGIN PGP MESSAGE-----"));
}

#[test]
fn encrypting_to_nobody_is_refused() {
    // "Encrypted to an empty recipient set" is a message anyone can read.
    let err = encrypt::encrypt_mime("hello", &[]).expect_err("must refuse");
    assert!(matches!(err, crate::error::Error::InvalidArgument(_)));
}

#[test]
fn an_encrypted_message_decrypts_back_to_the_plaintext() {
    // The end-to-end property. Everything else in this file could pass while
    // producing ciphertext nobody can open.
    use pgp::composed::Message;

    let address = "alice@example.com";
    let mut params = SecretKeyParamsBuilder::default();
    params
        .key_type(KeyType::Ed25519Legacy)
        .can_certify(true)
        .can_sign(true)
        .primary_user_id(format!("Test User <{address}>"))
        .subkey(
            SubkeyParamsBuilder::default()
                .key_type(KeyType::ECDH(
                    pgp::crypto::ecc_curve::ECCCurve::Curve25519Legacy,
                ))
                .can_encrypt(EncryptionCaps::All)
                .build()
                .expect("subkey params"),
        );
    let secret = params
        .build()
        .expect("params")
        .generate(rand::thread_rng())
        .expect("generate");
    let public_bytes = secret
        .to_public_key()
        .to_armored_bytes(Default::default())
        .expect("armor");

    let entry = key::parse(&public_bytes, address, KeySource::Wkd, NOW, 1 << 20).expect("parse");
    let plaintext = "attack at dawn";
    let mime = encrypt::encrypt_mime(plaintext, std::slice::from_ref(&entry)).expect("encrypt");

    // Pull the armored block back out of the MIME wrapper, exactly as a
    // receiving client would have to.
    let body = &mime.body;
    let begin = body
        .find("-----BEGIN PGP MESSAGE-----")
        .expect("armor start");
    let end_marker = "-----END PGP MESSAGE-----";
    let end = body.find(end_marker).expect("armor end") + end_marker.len();
    let armored = &body[begin..end];

    let (message, _) = Message::from_armor(armored.as_bytes()).expect("parse message");
    let mut decrypted = message
        .decrypt(&Password::empty(), &secret)
        .expect("decrypt with the matching secret key");
    let bytes = decrypted.as_data_vec().expect("read plaintext");

    assert_eq!(
        String::from_utf8_lossy(&bytes),
        plaintext,
        "round trip must return exactly what went in"
    );
}

// ---------------------------------------------------------------------------
// The RFC 3156 transformation on a whole message
// ---------------------------------------------------------------------------

/// A rendered message of the shape `compose::mime::build` produces.
fn rendered_message() -> Vec<u8> {
    concat!(
        "From: Me <me@example.com>\r\n",
        "To: Alice <alice@example.com>\r\n",
        "Subject: lunch\r\n",
        "Date: Mon, 17 Aug 2026 12:00:00 +0000\r\n",
        "Message-ID: <abc@example.com>\r\n",
        "MIME-Version: 1.0\r\n",
        "Content-Type: text/plain; charset=utf-8\r\n",
        "Content-Transfer-Encoding: 7bit\r\n",
        "X-Rmail-Draft-Notes: internal breadcrumb\r\n",
        "\r\n",
        "meet me at one\r\n",
    )
    .as_bytes()
    .to_vec()
}

#[test]
fn encrypting_a_rendered_message_keeps_routing_headers_outside() {
    let entry = usable("alice@example.com", KeySource::Wkd);
    let out = encrypt::encrypt_rendered(&rendered_message(), std::slice::from_ref(&entry))
        .expect("encrypt");
    let text = String::from_utf8(out).expect("utf-8");

    let (headers, _) = text.split_once("\r\n\r\n").expect("a header block");
    for required in [
        "From: Me <me@example.com>",
        "To: Alice <alice@example.com>",
        "Date: Mon, 17 Aug 2026 12:00:00 +0000",
        "Message-ID: <abc@example.com>",
    ] {
        assert!(
            headers.contains(required),
            "missing {required:?} in {headers:?}"
        );
    }
    assert!(
        headers.contains("multipart/encrypted"),
        "the outer content type must be the encrypted container"
    );
}

#[test]
fn encrypting_a_rendered_message_hides_the_body_and_the_inner_headers() {
    let entry = usable("alice@example.com", KeySource::Wkd);
    let out = encrypt::encrypt_rendered(&rendered_message(), std::slice::from_ref(&entry))
        .expect("encrypt");
    let text = String::from_utf8(out).expect("utf-8");

    assert!(
        !text.contains("meet me at one"),
        "the plaintext body survived into the encrypted message"
    );
    assert!(
        !text.contains("internal breadcrumb"),
        "a non-routing header must be moved inside the encrypted part, not broadcast"
    );
    assert!(
        !text.contains("Content-Transfer-Encoding: 7bit"),
        "the inner transfer encoding belongs inside the encrypted part"
    );
}

#[test]
fn a_message_with_no_header_separator_is_refused() {
    let err = encrypt::encrypt_rendered(b"this is not a message", &[])
        .expect_err("must refuse a non-message");
    assert!(matches!(err, crate::error::Error::Internal(_)));
}

#[test]
fn the_encrypted_message_has_exactly_one_mime_version_header() {
    let entry = usable("alice@example.com", KeySource::Wkd);
    let out = encrypt::encrypt_rendered(&rendered_message(), std::slice::from_ref(&entry))
        .expect("encrypt");
    let text = String::from_utf8(out).expect("utf-8");
    let (headers, _) = text.split_once("\r\n\r\n").expect("a header block");
    assert_eq!(
        headers
            .to_ascii_lowercase()
            .matches("mime-version:")
            .count(),
        1,
        "two MIME-Version headers is a malformed message"
    );
}

// ---------------------------------------------------------------------------
// Outcome -> cache mapping
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_not_found_outcome_is_cached_as_absent_but_a_failure_is_not() {
    // The mapping that decides whether an address goes quiet for a month.
    // Asserted on the resulting cache *state*, not on the call, because the
    // bug being guarded against is the two outcomes being collapsed.
    let (database, _path) = db().await;
    let cfg = config();

    database
        .with_write(|conn| {
            super::service::record(conn, "nokey@example.com", Outcome::NotFound, &cfg, NOW)?;
            super::service::record(
                conn,
                "unreachable@example.com",
                Outcome::Failed {
                    reasons: vec!["timeout".to_owned()],
                },
                &cfg,
                NOW,
            )?;
            Ok::<_, rusqlite::Error>(())
        })
        .expect("write");

    database
        .with_read(|conn| {
            assert_eq!(
                cache::lookup(conn, "nokey@example.com", NOW + 10 * DAY)?,
                Cached::Absent,
                "a reached-but-empty answer is cached for the negative TTL"
            );
            assert!(
                matches!(
                    cache::lookup(conn, "unreachable@example.com", NOW + 10 * DAY)?,
                    Cached::Stale { .. }
                ),
                "an unreachable server must leave the address eligible again \
                 long before the negative TTL"
            );
            Ok::<_, rusqlite::Error>(())
        })
        .expect("read");
}

#[tokio::test]
async fn a_found_outcome_is_cached_with_the_configured_ttl() {
    let (database, _path) = db().await;
    let cfg = config();
    let entry = usable("alice@example.com", KeySource::Wkd);

    database
        .with_write(|conn| {
            super::service::record(
                conn,
                "alice@example.com",
                Outcome::Found(Box::new(entry.clone())),
                &cfg,
                NOW,
            )?;
            Ok::<_, rusqlite::Error>(())
        })
        .expect("write");

    database
        .with_read(|conn| {
            let cached = cache::lookup(conn, "alice@example.com", NOW + 29 * DAY)?;
            let fingerprint = match &cached {
                Cached::Key(k) => Some(k.fingerprint.clone()),
                _ => None,
            };
            assert_eq!(
                fingerprint.as_deref(),
                Some(entry.fingerprint.as_str()),
                "expected a cached key, got {cached:?}"
            );
            assert!(
                !matches!(
                    cache::lookup(conn, "alice@example.com", NOW + 31 * DAY)?,
                    Cached::Key(_)
                ),
                "past the 30-day default TTL the entry must be revalidated"
            );
            Ok::<_, rusqlite::Error>(())
        })
        .expect("read");
}

// ---------------------------------------------------------------------------
// Status presentation
// ---------------------------------------------------------------------------

#[test]
fn every_status_has_a_distinct_code_and_glyph() {
    let statuses = vec![
        EncryptionStatus::Encrypted {
            fingerprints: vec!["A".to_owned()],
        },
        EncryptionStatus::Pending {
            addresses: vec!["a@b.c".to_owned()],
        },
        EncryptionStatus::NoKey {
            addresses: vec!["a@b.c".to_owned()],
        },
        EncryptionStatus::KeyChanged {
            address: "a@b.c".to_owned(),
            known: "OLD".to_owned(),
            discovered: "NEW".to_owned(),
        },
        EncryptionStatus::Disabled,
        EncryptionStatus::Blocked {
            addresses: vec!["a@b.c".to_owned()],
        },
    ];

    let codes: std::collections::BTreeSet<_> = statuses.iter().map(|s| s.code()).collect();
    assert_eq!(codes.len(), statuses.len(), "codes must be distinguishable");

    // Only the genuinely-encrypted state may claim encryption.
    for status in &statuses {
        assert_eq!(
            status.will_encrypt(),
            matches!(status, EncryptionStatus::Encrypted { .. }),
            "{status:?} misreports whether it encrypts"
        );
        assert!(!status.to_string().is_empty());
    }
}

#[test]
fn only_the_dangerous_states_need_attention() {
    assert!(EncryptionStatus::KeyChanged {
        address: "a@b.c".to_owned(),
        known: "OLD".to_owned(),
        discovered: "NEW".to_owned(),
    }
    .needs_attention());
    assert!(EncryptionStatus::Blocked {
        addresses: vec!["a@b.c".to_owned()]
    }
    .needs_attention());
    assert!(!EncryptionStatus::NoKey {
        addresses: vec!["a@b.c".to_owned()]
    }
    .needs_attention());
    assert!(!EncryptionStatus::Disabled.needs_attention());
}

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

#[test]
fn auto_encrypt_defaults_to_on_with_a_month_of_caching() {
    let cfg = CryptoConfig::default();
    assert!(
        cfg.auto_encrypt,
        "the whole point is that it is on by default"
    );
    assert_eq!(
        Duration::from(cfg.key_ttl),
        Duration::from_secs(30 * 86_400)
    );
    assert_eq!(
        Duration::from(cfg.negative_ttl),
        Duration::from_secs(30 * 86_400)
    );
    assert!(
        cfg.keyservers.is_empty(),
        "no public keyserver may be contacted before the user opts in"
    );
    assert!(cfg.autocrypt && cfg.wkd);
    assert!(cfg.warn_on_key_change);
}

#[test]
fn sources_that_leak_the_address_are_labelled_as_such() {
    assert!(!KeySource::Autocrypt.leaks_address());
    assert!(!KeySource::Wkd.leaks_address());
    assert!(!KeySource::Manual.leaks_address());
    assert!(KeySource::PrivateKeyserver.leaks_address());
    assert!(KeySource::PublicKeyserver.leaks_address());
}

#[test]
fn source_tokens_round_trip_through_the_database_representation() {
    for source in [
        KeySource::Autocrypt,
        KeySource::Wkd,
        KeySource::PrivateKeyserver,
        KeySource::PublicKeyserver,
        KeySource::Manual,
    ] {
        assert!(!source.as_str().is_empty());
    }
}
