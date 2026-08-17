//! Opportunistic OpenPGP: discover recipients' public keys in the background,
//! cache the answer, and encrypt outbound mail when every recipient has one.
//!
//! ```text
//! recipient set ──▶ cache ──hit──▶ EncryptionStatus
//!                     │
//!                    miss
//!                     │
//!                     ▼
//!            discover (background, budgeted)
//!            autocrypt ▶ wkd ▶ private ks ▶ public ks
//!                     │
//!                     ▼
//!            select newest usable ──▶ cache ──▶ EncryptionStatus
//! ```
//!
//! # Three rules, and the reasoning that produced them
//!
//! **1. Discovery never runs on the compose path.** Setting a recipient must
//! stay instant. Every lookup here is spawned onto a background task and its
//! result is read from the cache on the next status query; a compose that
//! blocked on a keyserver would be a compose that hangs when a keyserver is
//! slow, and keyservers are frequently slow. The visible consequence is that
//! [`EncryptionStatus::Pending`] exists and the UI has to render it — a real
//! cost, paid deliberately, because the alternative is a text field that
//! stalls while you type.
//!
//! **2. A missing key is not an error.** Most addresses have no OpenPGP key.
//! [`EncryptPolicy::Auto`] therefore sends in the clear and says so, and only
//! [`EncryptPolicy::Always`] turns absence into a refusal. This is the same
//! shape as [`crate::send::preflight`]'s rule 2: the set of messages this
//! daemon refuses to send should be small, explicit, and never a function of
//! whether a third-party server answered a request today.
//!
//! **3. Encrypting to the wrong key is worse than not encrypting.** This is
//! the rule that shapes the rest of the module. Unauthenticated discovery —
//! which is all of WKD and every keyserver — can be made to hand back a key an
//! attacker holds the private half of. The mail is then unreadable by its
//! recipient *and* readable by the attacker, and a naive implementation shows
//! a padlock the whole time. Nothing here can prevent that. What it can do,
//! and does:
//!
//! - refuse to be quiet about it ([`pgp_key_history`] and
//!   [`EncryptionStatus::KeyChanged`] — a fingerprint that changes is surfaced,
//!   never swapped in silently),
//! - prefer sources that leak less and lie less (Autocrypt keys arrived with
//!   the correspondence; WKD is at least the recipient's own domain),
//! - and let a human end the argument (`pgp_overrides.pinned_fingerprint`).
//!
//! # Why "newest" is the tiebreak, and what it is *not*
//!
//! When discovery turns up several usable keys for one address, this module
//! takes the one with the newest creation time. That matches the common case
//! it exists for — a correspondent rotated their key and the old one is still
//! on a keyserver — and it is what the feature was specified to do.
//!
//! It is worth being honest that "newest" is also exactly what an attacker
//! uploading a fresh key would satisfy. Newest is a tiebreak among keys that
//! have *already* passed [`UsableKey`]'s filters, not a trust decision, and it
//! is why rule 3's machinery is not optional decoration: on an address rmail
//! has corresponded with before, a newly-appeared key does not silently win —
//! it changes the fingerprint, and a changed fingerprint is
//! [`EncryptionStatus::KeyChanged`], which does not encrypt.

use std::fmt;

pub mod cache;
pub mod discover;
pub mod encrypt;
pub mod key;
pub mod service;

#[cfg(test)]
mod tests;

pub use key::{KeySource, UsableKey};

/// Normalize an address into the form every table and lookup in this module
/// keys on: the bare addr-spec, lowercased.
///
/// # Why this is one function and not an inline `to_lowercase`
///
/// The cache is keyed on the result. If a writer and a reader disagree by one
/// character about what "the same address" is, the reader misses on a row that
/// is already there and re-queries the network — the exact leak and latency
/// this cache exists to remove, reintroduced by a display name nobody
/// stripped. Making it a single named function means there is one definition
/// to be wrong, and one place to fix it.
///
/// Only the domain is truly case-insensitive per RFC 5321; the local part is
/// case-*sensitive* in the standard and case-insensitive at essentially every
/// real mail provider. Lowercasing both is the pragmatic choice, and the cost
/// of being wrong is a cache miss, not a misdirected mail — the address in the
/// envelope is never taken from this function's output.
#[must_use]
pub fn normalize_address(input: &str) -> String {
    let trimmed = input.trim();
    // `Display Name <addr@host>` → `addr@host`. Taking the *last* '<' handles
    // a display name that itself contains one.
    let bare = match (trimmed.rfind('<'), trimmed.rfind('>')) {
        (Some(open), Some(close)) if close > open => &trimmed[open + 1..close],
        _ => trimmed,
    };
    bare.trim().to_lowercase()
}

/// Whether a message to a given recipient set would be encrypted, and why.
///
/// This is the type behind the indicator. It is deliberately not a `bool` plus
/// a message: every variant below leads to different UI *and* a different next
/// action, and collapsing them into "encrypted / not encrypted" would put the
/// two dangerous cases ([`Self::KeyChanged`] and [`Self::Blocked`]) in the same
/// bucket as the entirely ordinary [`Self::NoKey`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncryptionStatus {
    /// Every recipient has a usable key; the message will be encrypted.
    Encrypted {
        /// One fingerprint per recipient, in recipient order.
        fingerprints: Vec<String>,
    },
    /// Discovery has not finished for at least one recipient.
    ///
    /// The honest answer while a background lookup is in flight, and the
    /// reason this enum has no `Unknown`: "we do not know yet, ask again"
    /// and "we asked and there is nothing" are different sentences, and a UI
    /// that renders them the same teaches users that the padlock flickers.
    Pending {
        /// Addresses still being looked up.
        addresses: Vec<String>,
    },
    /// At least one recipient has no usable key. The message goes in the
    /// clear under [`EncryptPolicy::Auto`].
    NoKey {
        /// The recipients without a key.
        addresses: Vec<String>,
    },
    /// A recipient presented a fingerprint that is not the one previously
    /// seen for them, and the user has not accepted it.
    ///
    /// **This does not encrypt.** Falling back to plaintext on a key change is
    /// a deliberate and slightly uncomfortable choice: it means an attacker
    /// who can publish a key can *downgrade* the connection to cleartext. The
    /// alternative is worse — encrypting to the new key hands the plaintext to
    /// whoever published it, which converts a downgrade into a disclosure. A
    /// downgrade the user is told about beats a disclosure they are not.
    KeyChanged {
        /// The address whose key changed.
        address: String,
        /// The fingerprint rmail had.
        known: String,
        /// The fingerprint discovery just returned.
        discovered: String,
    },
    /// Encryption is off: [`EncryptPolicy::Never`], or `auto_encrypt = false`.
    Disabled,
    /// [`EncryptPolicy::Always`] is set and some recipient has no usable key,
    /// so the send is refused.
    Blocked {
        /// The recipients that cannot be encrypted to.
        addresses: Vec<String>,
    },
}

impl EncryptionStatus {
    /// Whether the message would actually be encrypted.
    ///
    /// The one predicate a caller should branch on to decide whether to
    /// encrypt. Note that only [`Self::Encrypted`] is true — in particular
    /// [`Self::KeyChanged`] is *not*, which is the point of that variant.
    #[must_use]
    pub const fn will_encrypt(&self) -> bool {
        matches!(self, Self::Encrypted { .. })
    }

    /// Whether this status refuses the send outright.
    #[must_use]
    pub const fn blocks(&self) -> bool {
        matches!(self, Self::Blocked { .. })
    }

    /// Whether the user should be shown something more urgent than a padlock.
    #[must_use]
    pub const fn needs_attention(&self) -> bool {
        matches!(self, Self::KeyChanged { .. } | Self::Blocked { .. })
    }

    /// A short, stable token for logs, the CLI, and the gRPC surface.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Encrypted { .. } => "encrypted",
            Self::Pending { .. } => "pending",
            Self::NoKey { .. } => "no_key",
            Self::KeyChanged { .. } => "key_changed",
            Self::Disabled => "disabled",
            Self::Blocked { .. } => "blocked",
        }
    }

    /// The indicator glyph for a terminal UI.
    ///
    /// Distinct shapes rather than distinct colours alone: the difference
    /// between "encrypted" and "a key changed under you" must survive a
    /// monochrome terminal, a colourblind reader, and a screenshot.
    #[must_use]
    pub const fn glyph(&self) -> &'static str {
        match self {
            Self::Encrypted { .. } => "[LOCKED]",
            Self::Pending { .. } => "[...]",
            Self::NoKey { .. } => "[clear]",
            Self::KeyChanged { .. } => "[!KEY CHANGED]",
            Self::Disabled => "[clear]",
            Self::Blocked { .. } => "[!BLOCKED]",
        }
    }
}

impl fmt::Display for EncryptionStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Encrypted { fingerprints } => {
                write!(f, "encrypted to {} recipient", fingerprints.len())?;
                if fingerprints.len() != 1 {
                    write!(f, "s")?;
                }
                Ok(())
            }
            Self::Pending { addresses } => {
                write!(f, "looking up keys for {}", addresses.join(", "))
            }
            Self::NoKey { addresses } => {
                write!(
                    f,
                    "no key for {}; sending in the clear",
                    addresses.join(", ")
                )
            }
            Self::KeyChanged {
                address,
                known,
                discovered,
            } => write!(
                f,
                "the key for {address} changed ({known} -> {discovered}); \
                 not encrypting until you accept it"
            ),
            Self::Disabled => write!(f, "encryption disabled"),
            Self::Blocked { addresses } => write!(
                f,
                "policy requires encryption but no key is known for {}",
                addresses.join(", ")
            ),
        }
    }
}
