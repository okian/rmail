//! Resolving the encryption status of a recipient set, and producing the
//! PGP/MIME message when the answer is yes.
//!
//! # Two halves, deliberately separate
//!
//! [`resolve`] is pure, synchronous and offline: it reads the cache and the
//! overrides and returns an [`EncryptionStatus`]. [`encrypt_mime`] does the
//! cryptography. Splitting them is what lets the *indicator* be computed on
//! every keystroke — it is two indexed point lookups — while the expensive
//! half runs once, at send.
//!
//! It also means the thing the user is shown and the thing the send path acts
//! on are the same value, computed by the same function. A UI that decided
//! "encrypted" one way and a sender that decided it another would eventually
//! disagree, and the failure mode of that disagreement is a padlock over a
//! plaintext message.

use pgp::composed::{ArmorOptions, MessageBuilder};
use rusqlite::{Connection, OptionalExtension};

use crate::config::{CryptoConfig, EncryptPolicy};
use crate::error::Error;

use super::cache::{self, Cached, TrustState};
use super::key::UsableKey;
use super::{normalize_address, EncryptionStatus};

/// Decide what would happen to a message addressed to `recipients`.
///
/// Evaluation order is: the master switch, then each recipient's override,
/// then the cache. The first recipient that produces a *worse* answer than
/// "encrypted" determines the result, because encryption is all-or-nothing
/// per message — a message with three recipients and two keys cannot be half
/// encrypted, and sending two copies (one encrypted, one not) is a different
/// feature with its own consent question.
///
/// # Errors
///
/// Propagates any `rusqlite` error, per [`crate::repo`]'s convention that
/// storage helpers hand the raw error to the caller that owns the mapping.
pub fn resolve(
    conn: &Connection,
    recipients: &[String],
    config: &CryptoConfig,
    now: i64,
) -> rusqlite::Result<EncryptionStatus> {
    if !config.auto_encrypt {
        return Ok(EncryptionStatus::Disabled);
    }
    if recipients.is_empty() {
        return Ok(EncryptionStatus::NoKey {
            addresses: Vec::new(),
        });
    }

    let mut fingerprints = Vec::with_capacity(recipients.len());
    let mut pending = Vec::new();
    let mut missing = Vec::new();

    for recipient in recipients {
        let address = normalize_address(recipient);
        let policy = override_policy(conn, &address)?.unwrap_or(config.policy);

        if matches!(policy, EncryptPolicy::Never) {
            return Ok(EncryptionStatus::Disabled);
        }

        // A pinned key ends the question: the user verified this fingerprint
        // by hand and no keyserver's opinion outranks that.
        if let Some(pinned) = pinned_key(conn, &address)? {
            fingerprints.push(pinned);
            continue;
        }

        match cache::lookup(conn, &address, now)? {
            Cached::Key(key) => {
                // TOFU: a fingerprint that changed under us does not silently
                // win. See `EncryptionStatus::KeyChanged`.
                if config.warn_on_key_change {
                    if let TrustState::Changed { known } =
                        cache::trust_state(conn, &address, &key.fingerprint)?
                    {
                        return Ok(EncryptionStatus::KeyChanged {
                            address,
                            known,
                            discovered: key.fingerprint.clone(),
                        });
                    }
                }
                fingerprints.push(key.fingerprint.clone());
            }
            // A stale entry with a previous key keeps showing that key while
            // the refresh runs; a stale entry with nothing is a real pending.
            Cached::Stale { previous } => match previous {
                Some(key) => fingerprints.push(key.fingerprint.clone()),
                None => pending.push(address),
            },
            Cached::Backoff { .. } | Cached::Absent => missing.push(address),
        }
    }

    // Under `Always`, a recipient with *no* key blocks the send. A recipient
    // still being looked up does not: it reports `Pending`, and the caller
    // asks again. Blocking on a lookup in flight would refuse mail that was
    // about to become encryptable a second later, which is a worse failure
    // than making the user wait for the padlock to settle.
    let requires = recipients.iter().any(|r| {
        override_policy(conn, &normalize_address(r))
            .ok()
            .flatten()
            .unwrap_or(config.policy)
            == EncryptPolicy::Always
    });

    if !missing.is_empty() {
        return Ok(if requires {
            EncryptionStatus::Blocked { addresses: missing }
        } else {
            EncryptionStatus::NoKey { addresses: missing }
        });
    }
    if !pending.is_empty() {
        return Ok(EncryptionStatus::Pending { addresses: pending });
    }
    Ok(EncryptionStatus::Encrypted { fingerprints })
}

/// The per-address policy override, if one is set.
fn override_policy(conn: &Connection, address: &str) -> rusqlite::Result<Option<EncryptPolicy>> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT policy FROM pgp_overrides WHERE address = ?1",
            rusqlite::params![address],
            |row| row.get(0),
        )
        .optional()?;
    Ok(raw.and_then(|p| match p.as_str() {
        "auto" => Some(EncryptPolicy::Auto),
        "always" => Some(EncryptPolicy::Always),
        "never" => Some(EncryptPolicy::Never),
        _ => None,
    }))
}

/// A hand-pinned fingerprint for an address.
fn pinned_key(conn: &Connection, address: &str) -> rusqlite::Result<Option<String>> {
    let fingerprint: Option<String> = conn
        .query_row(
            "SELECT pinned_fingerprint FROM pgp_overrides
              WHERE address = ?1 AND pinned_fingerprint IS NOT NULL",
            rusqlite::params![address],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    Ok(fingerprint)
}

/// Collect the keys to encrypt to, in recipient order.
///
/// # Errors
///
/// Returns [`Error::FailedPrecondition`] if any recipient has no cached key —
/// which should be unreachable when [`resolve`] returned
/// [`EncryptionStatus::Encrypted`], and is checked anyway because "unreachable
/// given no bug" is exactly what a plaintext send would be hiding behind.
pub fn keys_for(
    conn: &Connection,
    recipients: &[String],
    now: i64,
) -> Result<Vec<UsableKey>, Error> {
    let mut keys = Vec::with_capacity(recipients.len());
    for recipient in recipients {
        let address = normalize_address(recipient);
        let cached = cache::lookup(conn, &address, now)
            .map_err(|e| Error::Internal(format!("key cache lookup for {address}: {e}")))?;
        match cached {
            Cached::Key(key) => keys.push(*key),
            Cached::Stale {
                previous: Some(key),
            } => keys.push(*key),
            _ => {
                return Err(Error::FailedPrecondition(format!(
                    "no usable key for {address}"
                )))
            }
        }
    }
    Ok(keys)
}

/// The two parts of an RFC 3156 PGP/MIME message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PgpMime {
    /// The `Content-Type` for the multipart/encrypted container, including
    /// its boundary.
    pub content_type: String,
    /// The fully rendered MIME body.
    pub body: String,
}

/// Encrypt an already-rendered MIME body to every key, as RFC 3156
/// `multipart/encrypted`.
///
/// # What is and is not hidden
///
/// The *body* — headers included, since the encrypted part is a complete MIME
/// entity — is protected. The outer envelope is not: `To`, `Cc`, `Subject` and
/// `Date` remain in the clear, because they are what the SMTP path and the
/// recipient's server route and thread on. This is a property of PGP/MIME, not
/// a shortcut taken here, and it is worth stating plainly because an
/// encryption indicator can easily be read as promising more than it delivers.
///
/// # Errors
///
/// Returns [`Error::Internal`] if the OpenPGP layer refuses a key or fails to
/// serialize. Returns [`Error::InvalidArgument`] for an empty key list — a
/// "encrypted to nobody" message is a message anyone can read, and producing
/// one silently is the worst failure this module has.
pub fn encrypt_mime(body: &str, keys: &[UsableKey]) -> Result<PgpMime, Error> {
    encrypt_mime_split(body, keys, &[])
}

/// As [`encrypt_mime`], but with a second set of recipients whose key IDs are
/// withheld from the message.
///
/// # Why Bcc needs its own arm
///
/// An OpenPGP encrypted message carries one Public-Key Encrypted Session Key
/// packet per recipient, and each names the key it was encrypted to. Encrypt a
/// message to a Bcc'd recipient the ordinary way and their key — which is to
/// say their identity — is sitting in the message every *other* recipient
/// receives. Bcc exists precisely to prevent that, and this module's whole
/// purpose is to make mail more private rather than less, so getting it
/// backwards here would be worse than not encrypting at all.
///
/// `hidden` recipients therefore get `encrypt_to_key_anonymous`, which blanks
/// the recipient field. The cost is real but small and lands on the right
/// party: a hidden recipient's client cannot tell which packet is theirs and
/// must try its keys against each one.
///
/// Visible recipients (To/Cc) are *not* anonymised. They are already named in
/// headers the message carries in the clear, so hiding their key IDs would buy
/// nothing and would make every recipient do the trial decryption.
///
/// # Errors
///
/// As [`encrypt_mime`]. An empty `visible` set is still refused even when
/// `hidden` is non-empty — a message with no To or Cc is not a shape this
/// send path produces, and treating it as encryptable would mean guessing.
pub fn encrypt_mime_split(
    body: &str,
    visible: &[UsableKey],
    hidden: &[UsableKey],
) -> Result<PgpMime, Error> {
    let keys = visible;
    if keys.is_empty() && hidden.is_empty() {
        return Err(Error::InvalidArgument(
            "cannot encrypt to an empty recipient set".to_owned(),
        ));
    }

    let mut rng = rand::thread_rng();
    // `seipd_v1` consumes the builder and returns a different typestate, so
    // the encryption mode is chosen once and cannot be forgotten: there is no
    // `encrypt_to_key` on the unencrypted builder to call by mistake.
    let mut builder = MessageBuilder::from_bytes("", body.as_bytes().to_vec())
        .seipd_v1(&mut rng, pgp::crypto::sym::SymmetricKeyAlgorithm::AES256);

    for key in keys {
        let parsed = key.parsed().map_err(|e| {
            Error::Internal(format!("stored key {} is unusable: {e}", key.fingerprint))
        })?;
        // Encrypt to an encryption-capable subkey when there is one, else to
        // the primary. `UsableKey` guarantees one of them exists.
        let target = parsed
            .public_subkeys
            .iter()
            .find(|sub| {
                sub.signatures
                    .iter()
                    .any(|sig| sig.key_flags().encrypt_comms() || sig.key_flags().encrypt_storage())
            })
            .map(|sub| &sub.key);

        match target {
            Some(subkey) => builder
                .encrypt_to_key(&mut rng, subkey)
                .map_err(|e| Error::Internal(format!("encrypt to {}: {e}", key.fingerprint)))?,
            None => builder
                .encrypt_to_key(&mut rng, &parsed.primary_key)
                .map_err(|e| Error::Internal(format!("encrypt to {}: {e}", key.fingerprint)))?,
        };
    }

    for key in hidden {
        let parsed = key.parsed().map_err(|e| {
            Error::Internal(format!("stored key {} is unusable: {e}", key.fingerprint))
        })?;
        let target = parsed
            .public_subkeys
            .iter()
            .find(|sub| {
                sub.signatures
                    .iter()
                    .any(|sig| sig.key_flags().encrypt_comms() || sig.key_flags().encrypt_storage())
            })
            .map(|sub| &sub.key);
        match target {
            Some(subkey) => builder
                .encrypt_to_key_anonymous(&mut rng, subkey)
                .map_err(|e| Error::Internal(format!("encrypt to {}: {e}", key.fingerprint)))?,
            None => builder
                .encrypt_to_key_anonymous(&mut rng, &parsed.primary_key)
                .map_err(|e| Error::Internal(format!("encrypt to {}: {e}", key.fingerprint)))?,
        };
    }

    let armored = builder
        .to_armored_string(&mut rng, ArmorOptions::default())
        .map_err(|e| Error::Internal(format!("armor: {e}")))?;

    // A boundary that cannot collide with the armored payload: the armor
    // alphabet is base64 plus `-`, `=` and newlines, so a boundary containing
    // `_` is not expressible inside it.
    let boundary = format!("=_rmail_pgp_{:016x}_=", rand_u64(&mut rng));

    let body = format!(
        "--{boundary}\r\n\
         Content-Type: application/pgp-encrypted\r\n\
         Content-Description: PGP/MIME version identification\r\n\
         \r\n\
         Version: 1\r\n\
         \r\n\
         --{boundary}\r\n\
         Content-Type: application/octet-stream; name=\"encrypted.asc\"\r\n\
         Content-Description: OpenPGP encrypted message\r\n\
         Content-Disposition: inline; filename=\"encrypted.asc\"\r\n\
         \r\n\
         {armored}\r\n\
         --{boundary}--\r\n"
    );

    Ok(PgpMime {
        content_type: format!(
            "multipart/encrypted; protocol=\"application/pgp-encrypted\"; boundary=\"{boundary}\""
        ),
        body,
    })
}

/// A random u64 for the MIME boundary.
fn rand_u64<R: rand::Rng>(rng: &mut R) -> u64 {
    rng.gen()
}

/// Headers that stay on the *outside* of an encrypted message.
///
/// # This list is the privacy boundary, so it is explicit
///
/// Everything not named here is moved inside the encrypted part. That
/// direction — allowlist the survivors rather than blocklist the secrets — is
/// the only safe one: a header added by a future feature and forgotten by a
/// blocklist would silently leak, whereas one forgotten by an allowlist merely
/// fails to be delivered where some client expected it.
///
/// The entries are the ones delivery genuinely needs. `From`/`To`/`Cc` are the
/// envelope's visible counterpart, `Date` and `Message-ID` are what threading
/// and duplicate suppression run on, and `Subject` — regrettably — is kept
/// because a message with no subject line is treated as spam by a meaningful
/// share of receiving filters. That last one is a real disclosure and it is
/// why [`encrypt_mime`]'s docs say plainly that the subject is not protected.
const OUTER_HEADERS: &[&str] = &[
    "from",
    "to",
    "cc",
    "bcc",
    "subject",
    "date",
    "message-id",
    "in-reply-to",
    "references",
    "mime-version",
    "user-agent",
    "reply-to",
];

/// Transform a fully rendered RFC 5322 message into its PGP/MIME encrypted
/// form (RFC 3156).
///
/// The original headers are split in two: [`OUTER_HEADERS`] stay on the
/// visible message, and everything else — `Content-Type`,
/// `Content-Transfer-Encoding`, and any custom headers — moves *inside* the
/// encrypted part along with the body, so it is protected rather than
/// broadcast.
///
/// # Errors
///
/// Returns [`Error::Internal`] if the rendered message has no header/body
/// separator, which would mean the renderer produced something that is not a
/// message. Otherwise as [`encrypt_mime`].
pub fn encrypt_rendered(rendered: &[u8], keys: &[UsableKey]) -> Result<Vec<u8>, Error> {
    encrypt_rendered_split(rendered, keys, &[])
}

/// As [`encrypt_rendered`], with Bcc recipients encrypted to anonymously.
///
/// # Errors
///
/// As [`encrypt_rendered`].
pub fn encrypt_rendered_split(
    rendered: &[u8],
    visible: &[UsableKey],
    hidden: &[UsableKey],
) -> Result<Vec<u8>, Error> {
    let text = std::str::from_utf8(rendered)
        .map_err(|e| Error::Internal(format!("rendered message is not utf-8: {e}")))?;

    // RFC 5322 separates headers from body with a blank line. Accept a bare
    // LF as well as CRLF: the renderer emits CRLF, but a message that reached
    // here through any other path should still be encrypted rather than
    // rejected into a plaintext fallback.
    let (headers, body) = split_headers(text)
        .ok_or_else(|| Error::Internal("rendered message has no header separator".to_owned()))?;

    let mut outer = String::new();
    let mut inner = String::new();
    for header in unfold(headers) {
        let name = header
            .split_once(':')
            .map(|(n, _)| n.trim().to_ascii_lowercase())
            .unwrap_or_default();
        if OUTER_HEADERS.contains(&name.as_str()) {
            outer.push_str(&header);
            outer.push_str("\r\n");
        } else {
            inner.push_str(&header);
            inner.push_str("\r\n");
        }
    }

    let protected = format!("{inner}\r\n{body}");
    let encrypted = encrypt_mime_split(&protected, visible, hidden)?;

    let mut out = String::with_capacity(outer.len() + encrypted.body.len() + 256);
    out.push_str(&outer);
    // `MIME-Version` may or may not have been in the original; emit it only
    // if the outer set did not already carry one, since two of them is a
    // malformed message.
    if !outer.to_ascii_lowercase().contains("mime-version:") {
        out.push_str("MIME-Version: 1.0\r\n");
    }
    out.push_str("Content-Type: ");
    out.push_str(&encrypted.content_type);
    out.push_str("\r\n\r\n");
    out.push_str(&encrypted.body);

    Ok(out.into_bytes())
}

/// Split a message at the first blank line.
fn split_headers(text: &str) -> Option<(&str, &str)> {
    if let Some(index) = text.find("\r\n\r\n") {
        return Some((&text[..index], &text[index + 4..]));
    }
    text.find("\n\n")
        .map(|index| (&text[..index], &text[index + 2..]))
}

/// Unfold RFC 5322 continuation lines into one string per header.
///
/// A folded header split naively on newlines would put its continuation lines
/// through the name check below as if they were headers of their own; they
/// have no colon, so they would land in whichever bucket the fallback chose
/// and be separated from the header they belong to.
fn unfold(headers: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in headers.split("\r\n").flat_map(|l| l.split('\n')) {
        if line.is_empty() {
            continue;
        }
        if line.starts_with(' ') || line.starts_with('\t') {
            if let Some(last) = out.last_mut() {
                last.push_str("\r\n");
                last.push_str(line);
                continue;
            }
        }
        out.push(line.to_owned());
    }
    out
}

/// The one call the send path makes: encrypt a rendered message if policy and
/// discovery allow, or hand back the plaintext and say why not.
///
/// # Why this returns bytes *and* a status
///
/// The caller needs both, and deriving one from the other is where this would
/// go wrong. A caller given only bytes cannot tell whether it is holding
/// ciphertext, so it cannot log honestly or set a flag on the outbox row; a
/// caller given only a status has to re-run the encryption to get the octets.
/// Returning the pair means "what was sent" and "what we say was sent" are
/// produced together and cannot drift.
///
/// # Where this is called from, and why not later
///
/// The outbox freezes `raw_mime` when a send is scheduled and transmits those
/// exact octets on every attempt (see [`crate::outbox`]). Encrypting here —
/// before the freeze — means a retry re-sends byte-identical ciphertext, the
/// at-most-once `Message-ID` fence still matches, and the undo window still
/// shows the user the message they composed. Encrypting at transmit time
/// instead would produce different bytes on every attempt, which is the one
/// thing that path is built not to do.
///
/// `bcc` recipients are encrypted to anonymously; see [`encrypt_mime_split`].
///
/// # Errors
///
/// [`Error::FailedPrecondition`] when the policy is
/// [`EncryptPolicy::Always`] and some recipient has no usable key — the send
/// is refused rather than downgraded. Otherwise as [`encrypt_rendered`].
pub fn seal_for_send(
    conn: &Connection,
    rendered: &[u8],
    visible: &[String],
    bcc: &[String],
    config: &CryptoConfig,
    now: i64,
) -> Result<(Vec<u8>, EncryptionStatus), Error> {
    let mut all: Vec<String> = visible.to_vec();
    all.extend_from_slice(bcc);

    let status = resolve(conn, &all, config, now)
        .map_err(|e| Error::Internal(format!("resolving encryption status: {e}")))?;

    if status.blocks() {
        return Err(Error::FailedPrecondition(format!(
            "encryption is required for this recipient but no key is known: {status}"
        )));
    }
    if !status.will_encrypt() {
        // Every remaining non-encrypting state — no key, still looking, a
        // changed fingerprint, disabled — sends in the clear. The status says
        // which, and the caller is expected to surface it rather than treat
        // "not encrypted" as one undifferentiated outcome.
        return Ok((rendered.to_vec(), status));
    }

    let visible_keys = keys_for(conn, visible, now)?;
    let hidden_keys = keys_for(conn, bcc, now)?;
    let sealed = encrypt_rendered_split(rendered, &visible_keys, &hidden_keys)?;
    Ok((sealed, status))
}
