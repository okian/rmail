//! Parsing a discovered key, deciding whether it is usable, and picking one
//! when several turn up.
//!
//! # Everything here treats its input as hostile
//!
//! A "discovered key" is bytes an unauthenticated third party handed us
//! because we asked about an email address. Anyone may upload a key claiming
//! any address to a public keyserver; a hostile DNS answer can point WKD at a
//! server of the attacker's choosing. So nothing in this module trusts a key
//! because it was returned for a query — it re-derives every property it
//! reports from the key's own packets, and it rejects far more than it
//! accepts.
//!
//! In particular the address the key was *found under* is never taken as the
//! address the key is *for*: [`parse`] requires a User ID inside the key to
//! match, because a keyserver that returns an unrelated key for a query is
//! either broken or attacking, and both deserve the same answer.

use std::io::Cursor;

use pgp::composed::{Deserializable, SignedPublicKey};
use pgp::types::KeyDetails as _;

use super::normalize_address;

/// Where a key came from. Ordered by how much it is worth believing, which is
/// also the order [`super::discover`] tries them in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum KeySource {
    /// An `Autocrypt:` header on mail already in this mailbox.
    Autocrypt,
    /// Web Key Directory at the address's own domain.
    Wkd,
    /// A keyserver the user or their organization operates.
    PrivateKeyserver,
    /// A public keyserver.
    PublicKeyserver,
    /// Pinned or imported by the user. Outranks everything.
    Manual,
}

impl KeySource {
    /// The token stored in `pgp_keys.source`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Autocrypt => "autocrypt",
            Self::Wkd => "wkd",
            Self::PrivateKeyserver => "private_keyserver",
            Self::PublicKeyserver => "public_keyserver",
            Self::Manual => "manual",
        }
    }

    /// Whether reaching this source discloses the recipient's address to a
    /// third party.
    ///
    /// Read by the discovery chain to decide what may run before what. It is a
    /// method on the source rather than a check at the call site so that
    /// adding a source forces an answer to the question.
    #[must_use]
    pub const fn leaks_address(self) -> bool {
        match self {
            // Local mail; no request leaves the machine.
            Self::Autocrypt | Self::Manual => false,
            // The recipient's own domain — it is about to receive the message
            // anyway, so the lookup tells it nothing it will not learn.
            Self::Wkd => false,
            // Someone else's server learns who the user is emailing.
            Self::PrivateKeyserver | Self::PublicKeyserver => true,
        }
    }
}

/// A key that passed every check in [`parse`] and may be encrypted to.
///
/// The name is the invariant: there is no way to construct one of these from
/// a key that is revoked, expired, or incapable of encryption, so a caller
/// holding one does not need to re-check any of that.
#[derive(Debug, Clone)]
pub struct UsableKey {
    /// Uppercase hex, no spaces.
    pub fingerprint: String,
    /// The address this key was accepted for (normalized).
    pub address: String,
    /// Primary key creation time, unix seconds. The "newest wins" sort key.
    pub created_at: i64,
    /// Expiry in unix seconds, or `None` for a key that does not expire.
    pub expires_at: Option<i64>,
    /// Where it was found.
    pub source: KeySource,
    /// The key as received, for storage and for [`super::encrypt`].
    pub data: Vec<u8>,
}

impl UsableKey {
    /// The parsed key.
    ///
    /// Re-parsed on demand rather than held: `SignedPublicKey` is not cheap to
    /// clone and this struct is stored, cached and compared far more often
    /// than it is used to encrypt.
    ///
    /// # Errors
    ///
    /// Returns [`KeyError::Malformed`] if the stored bytes no longer parse,
    /// which would mean the row was corrupted after it was written.
    pub fn parsed(&self) -> Result<SignedPublicKey, KeyError> {
        let (key, _) = SignedPublicKey::from_reader_single(Cursor::new(&self.data))
            .map_err(|e| KeyError::Malformed(e.to_string()))?;
        Ok(key)
    }
}

/// Why a discovered key was refused.
///
/// Every variant is a *rejection*, not a failure — the network call succeeded
/// and the answer was not good enough. They are distinguished because they
/// mean different things to an operator reading a log: `Malformed` suggests a
/// broken server, `AddressMismatch` suggests a hostile or misconfigured one,
/// and `Expired`/`Revoked` are ordinary facts about a correspondent's key.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum KeyError {
    /// The bytes are not a parseable transferable public key.
    #[error("malformed key: {0}")]
    Malformed(String),
    /// No User ID in the key matches the address it was found under.
    #[error("key contains no user id for {address}")]
    AddressMismatch {
        /// The address that was queried.
        address: String,
    },
    /// The key carries a revocation signature.
    #[error("key {fingerprint} is revoked")]
    Revoked {
        /// The revoked key's fingerprint.
        fingerprint: String,
    },
    /// The key's expiry is in the past.
    #[error("key {fingerprint} expired at {expired_at}")]
    Expired {
        /// The expired key's fingerprint.
        fingerprint: String,
        /// Unix seconds at which it expired.
        expired_at: i64,
    },
    /// Nothing in the key can encrypt.
    #[error("key {fingerprint} has no encryption-capable key")]
    NoEncryptionKey {
        /// The fingerprint of the key that cannot encrypt.
        fingerprint: String,
    },
    /// The key is larger than `crypto.max_key_bytes`.
    #[error("key is {size} bytes, over the {limit}-byte limit")]
    TooLarge {
        /// The rejected key's size.
        size: usize,
        /// The configured ceiling.
        limit: usize,
    },
}

/// Parse and validate a discovered key for `address`.
///
/// The size check runs *first*, before parsing, because parsing is the
/// expensive part and the ceiling exists precisely to stop a hostile upload
/// from making a background task do unbounded work.
///
/// # Errors
///
/// Any [`KeyError`]; see that type for what each rejection means.
pub fn parse(
    bytes: &[u8],
    address: &str,
    source: KeySource,
    now: i64,
    max_bytes: usize,
) -> Result<UsableKey, KeyError> {
    if bytes.len() > max_bytes {
        return Err(KeyError::TooLarge {
            size: bytes.len(),
            limit: max_bytes,
        });
    }

    let (key, _) = SignedPublicKey::from_reader_single(Cursor::new(bytes))
        .map_err(|e| KeyError::Malformed(e.to_string()))?;

    let fingerprint = fingerprint_hex(&key);
    let wanted = normalize_address(address);

    // A key is for whoever its User IDs say it is for, not for whoever we
    // asked about. See the module docs.
    if !user_ids(&key).contains(&wanted) {
        return Err(KeyError::AddressMismatch { address: wanted });
    }

    if !key.details.revocation_signatures.is_empty() {
        return Err(KeyError::Revoked { fingerprint });
    }

    let created_at = timestamp_secs(key.primary_key.created_at());
    let expires_at = expiry_secs(&key, created_at);
    if let Some(expiry) = expires_at {
        if expiry <= now {
            return Err(KeyError::Expired {
                fingerprint,
                expired_at: expiry,
            });
        }
    }

    if !can_encrypt(&key) {
        return Err(KeyError::NoEncryptionKey { fingerprint });
    }

    Ok(UsableKey {
        fingerprint,
        address: wanted,
        created_at,
        expires_at,
        source,
        data: bytes.to_vec(),
    })
}

/// Choose one key from everything discovery turned up.
///
/// **Newest creation time wins**, which is the specified rule and the right
/// one for its motivating case: a correspondent rotated their key and the
/// superseded one is still sitting on a keyserver.
///
/// Source is the tiebreak *within* one instant, not an override — two keys
/// created in the same second are separated by preferring the source that
/// lies less. Sorting by source first was considered and rejected: it would
/// mean a stale Autocrypt header from two years ago outranked a current key
/// from the recipient's own domain, which inverts the rotation case this
/// function exists to get right.
///
/// Returns `None` for an empty slice.
#[must_use]
pub fn select_best(candidates: &[UsableKey]) -> Option<&UsableKey> {
    candidates.iter().max_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then(b.source.cmp(&a.source))
    })
}

/// Uppercase hex fingerprint, no separators.
fn fingerprint_hex(key: &SignedPublicKey) -> String {
    key.fingerprint()
        .as_bytes()
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect()
}

/// Every normalized email address in the key's User IDs.
///
/// A User ID is free text conventionally shaped `Name (comment) <addr@host>`,
/// so the address is extracted the same way [`normalize_address`] does it. An
/// ID with no angle brackets is normalized whole, which lets a bare
/// `addr@host` User ID match.
fn user_ids(key: &SignedPublicKey) -> Vec<String> {
    key.details
        .users
        .iter()
        // `id()` is raw bytes: a User ID is arbitrary octets, not guaranteed
        // UTF-8. Lossy conversion is right here because a malformed ID simply
        // fails to match the address, which is the correct outcome anyway.
        .map(|user| normalize_address(&String::from_utf8_lossy(user.id.id())))
        .collect()
}

/// The key's expiry, derived from whichever self-signature carries one.
///
/// OpenPGP stores expiry as a *duration from key creation* on a signature, not
/// as an absolute time on the key, and it can appear on a direct-key signature
/// or on a User ID binding. Both are checked and the **earliest** wins: a key
/// whose signatures disagree about when it dies should be treated as dying at
/// the first of those moments, because that is the reading under which we stop
/// encrypting to something a recipient may already consider dead.
fn expiry_secs(key: &SignedPublicKey, created_at: i64) -> Option<i64> {
    let direct = key.details.direct_signatures.iter();
    let binding = key.details.users.iter().flat_map(|u| u.signatures.iter());
    direct
        .chain(binding)
        .filter_map(|sig| sig.key_expiration_time())
        .map(|d| i64::from(d.as_secs()))
        .map(|secs| created_at.saturating_add(secs))
        .min()
}

/// Whether anything in the key may be used to encrypt.
///
/// Checks subkeys first because that is where the encryption key lives on
/// essentially every modern key: the primary is a signing/certification key
/// and encryption is delegated to a subkey. A key with neither is legitimate
/// (a signing-only key) and simply cannot be a recipient.
fn can_encrypt(key: &SignedPublicKey) -> bool {
    let subkey_can = key.public_subkeys.iter().any(|sub| {
        sub.signatures
            .iter()
            .any(|sig| sig.key_flags().encrypt_comms() || sig.key_flags().encrypt_storage())
    });
    if subkey_can {
        return true;
    }
    key.details
        .users
        .iter()
        .flat_map(|u| u.signatures.iter())
        .chain(key.details.direct_signatures.iter())
        .any(|sig| sig.key_flags().encrypt_comms() || sig.key_flags().encrypt_storage())
}

/// rPGP timestamps to unix seconds.
///
/// OpenPGP creation times are unsigned 32-bit seconds since the epoch, so the
/// widening is total and needs no fallible path.
fn timestamp_secs(ts: pgp::types::Timestamp) -> i64 {
    i64::from(ts.as_secs())
}
