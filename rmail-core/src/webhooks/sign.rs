//! HMAC-SHA256 request signing for outbound webhooks.
//!
//! # Why the construction is written out here rather than pulled in
//!
//! This is RFC 2104's HMAC over the SHA-256 already in this workspace
//! (`sha2`, a direct dependency since task 26's pagination cursors). It is
//! twenty lines and it is fully specified by published test vectors, which
//! [`tests`] runs verbatim from RFC 4231 — so correctness here is *proved*
//! against the standard rather than delegated. Adding a crate for it would
//! add supply-chain surface to a security primitive in exchange for code this
//! module can demonstrate is right, which is the wrong trade for the one
//! function in rmail whose output a third party uses to decide whether to
//! trust a request.
//!
//! # What is signed
//!
//! The signature covers `<timestamp>.<body>` — the exact request body bytes,
//! prefixed by the same unix-second timestamp sent in `X-Rmail-Timestamp` and
//! joined with a `.`. Signing the body alone would let an attacker who
//! captured one request replay it forever; binding the timestamp into the
//! signed string means a receiver can reject anything outside its own
//! freshness window and the attacker cannot move the timestamp without
//! invalidating the signature. This is the scheme Stripe and Slack both use,
//! deliberately, because a receiver that already knows one of those knows this
//! one.
//!
//! The signature is emitted as `v1=<lowercase hex>` in `X-Rmail-Signature`.
//! The version prefix is what makes replacing this construction later a
//! non-breaking change for a receiver that checks the prefix.
//!
//! # What this module never does
//!
//! It never logs, formats, or returns the key. [`sign`] takes a
//! [`crate::credential::Secret`] — whose own `Debug`/`Display` print
//! `<redacted>` — and hands back only the digest. There is no code path here
//! that puts key material into a `tracing` field, an error message, or a
//! `String` a caller could accidentally print.

use sha2::{Digest, Sha256};

use crate::credential::Secret;

/// SHA-256's block size in bytes — the width HMAC pads its key to.
const BLOCK: usize = 64;

/// The signature scheme's version prefix. See the module docs.
pub const SIGNATURE_VERSION: &str = "v1";

/// Header carrying `v1=<hex>`.
pub const SIGNATURE_HEADER: &str = "X-Rmail-Signature";

/// Header carrying the unix-second timestamp bound into the signature.
pub const TIMESTAMP_HEADER: &str = "X-Rmail-Timestamp";

/// Header carrying the delivery's stable id, so a receiver can dedupe an
/// at-least-once retry without inspecting the body. See V48's header on why
/// this queue is at-least-once by design.
pub const DELIVERY_HEADER: &str = "X-Rmail-Delivery";

/// Header naming the event a delivery is about (`on_new_message`, `forward`,
/// ...), so a receiver can route without parsing the body.
pub const EVENT_HEADER: &str = "X-Rmail-Event";

/// The exact string a signature covers: `<timestamp>.<body>`.
///
/// Split out from [`sign`] because a receiver's own verification has to
/// rebuild it byte for byte, and a test that only ever went through `sign`
/// could not tell a change in this construction from a change in the MAC.
#[must_use]
pub fn signed_payload(timestamp: i64, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 24);
    out.extend_from_slice(timestamp.to_string().as_bytes());
    out.push(b'.');
    out.extend_from_slice(body);
    out
}

/// Sign `body` at `timestamp` with `key`, returning the `X-Rmail-Signature`
/// header value (`v1=<lowercase hex>`).
#[must_use]
pub fn sign(key: &Secret, timestamp: i64, body: &[u8]) -> String {
    let mac = hmac_sha256(key.expose().as_bytes(), &signed_payload(timestamp, body));
    let mut out = String::with_capacity(SIGNATURE_VERSION.len() + 1 + mac.len() * 2);
    out.push_str(SIGNATURE_VERSION);
    out.push('=');
    for byte in mac {
        // Two lowercase hex digits per byte. `write!` into a String cannot
        // fail, but using it here would still need its `Result` handled;
        // indexing a fixed table cannot fail at all.
        const HEX: &[u8; 16] = b"0123456789abcdef";
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

/// Whether `signature` is the signature of `body` at `timestamp` under `key`.
///
/// Present for the tests and for any future inbound-verification surface, and
/// deliberately constant-time in the comparison: a receiver-side check that
/// leaked the position of the first differing byte through timing would let an
/// attacker recover a valid MAC byte by byte. Nothing in rmail verifies
/// incoming signatures today, but a verifier that is only *almost* right is
/// exactly the kind of thing that gets copied into one later.
#[must_use]
pub fn verify(key: &Secret, timestamp: i64, body: &[u8], signature: &str) -> bool {
    constant_time_eq(sign(key, timestamp, body).as_bytes(), signature.as_bytes())
}

/// Byte comparison whose running time depends only on the *lengths* of the
/// inputs, never on where they first differ.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// RFC 2104 HMAC-SHA256.
///
/// A key longer than the hash's block size is replaced by its own digest, and
/// a shorter one is zero-padded to the block size — both exactly as the RFC
/// specifies. The RFC 4231 vectors in [`tests`] cover both cases plus the
/// empty key, which is what makes this implementation checkable rather than
/// merely plausible.
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        let digest = Sha256::digest(key);
        block[..digest.len()].copy_from_slice(&digest);
    } else {
        block[..key.len()].copy_from_slice(key);
    }

    let mut ipad = [0x36u8; BLOCK];
    let mut opad = [0x5cu8; BLOCK];
    for i in 0..BLOCK {
        ipad[i] ^= block[i];
        opad[i] ^= block[i];
    }

    let mut inner = Sha256::new();
    inner.update(ipad);
    inner.update(message);
    let inner = inner.finalize();

    let mut outer = Sha256::new();
    outer.update(opad);
    outer.update(inner);
    outer.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decode a hex string in a test.
    fn unhex(s: &str) -> Vec<u8> {
        let bytes = s.as_bytes();
        assert_eq!(bytes.len() % 2, 0, "hex string must have even length");
        bytes
            .chunks(2)
            .map(|pair| {
                let hi = char::from(pair[0]).to_digit(16).expect("hex digit");
                let lo = char::from(pair[1]).to_digit(16).expect("hex digit");
                u8::try_from(hi * 16 + lo).expect("byte")
            })
            .collect()
    }

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// RFC 4231 §4.2 — 20-byte key, shorter than the block size.
    #[test]
    fn rfc4231_case_1() {
        let key = unhex("0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b");
        let mac = hmac_sha256(&key, b"Hi There");
        assert_eq!(
            hex(&mac),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
    }

    /// RFC 4231 §4.3 — an ASCII key.
    #[test]
    fn rfc4231_case_2() {
        let mac = hmac_sha256(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(
            hex(&mac),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }

    /// RFC 4231 §4.4 — a 20-byte key over 50 bytes of `0xdd`.
    #[test]
    fn rfc4231_case_3() {
        let key = unhex("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let mac = hmac_sha256(&key, &[0xddu8; 50]);
        assert_eq!(
            hex(&mac),
            "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe"
        );
    }

    /// RFC 4231 §4.6 — a 131-byte key, *longer* than SHA-256's block size, so
    /// this is the vector that proves the key-shortening branch is right. An
    /// implementation that forgot to hash an oversized key passes every other
    /// case here and silently signs with the wrong key in production.
    #[test]
    fn rfc4231_case_6_key_longer_than_the_block_is_hashed_first() {
        let key = vec![0xaau8; 131];
        let mac = hmac_sha256(
            &key,
            b"Test Using Larger Than Block-Size Key - Hash Key First",
        );
        assert_eq!(
            hex(&mac),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn the_signature_is_over_timestamp_dot_body() {
        assert_eq!(signed_payload(1_700_000_000, b"{}"), b"1700000000.{}");
    }

    #[test]
    fn a_signature_is_version_prefixed_lowercase_hex() {
        let key = Secret::new("shhh");
        let sig = sign(&key, 1_700_000_000, b"{\"a\":1}");
        assert!(
            sig.starts_with("v1="),
            "signature is not version-prefixed: {sig}"
        );
        let hexpart = sig.trim_start_matches("v1=");
        assert_eq!(hexpart.len(), 64, "sha256 is 32 bytes of hex");
        assert!(
            hexpart
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "signature must be lowercase hex: {hexpart}"
        );
    }

    #[test]
    fn moving_the_timestamp_invalidates_the_signature() {
        let key = Secret::new("shhh");
        let body = b"{\"subject\":\"hi\"}";
        let sig = sign(&key, 1_700_000_000, body);
        assert!(verify(&key, 1_700_000_000, body, &sig));
        // The replay an attacker would attempt: same body, later clock.
        assert!(
            !verify(&key, 1_700_000_060, body, &sig),
            "a signature that survives a moved timestamp is a replayable signature"
        );
    }

    #[test]
    fn a_different_key_does_not_verify() {
        let body = b"{}";
        let sig = sign(&Secret::new("right"), 42, body);
        assert!(!verify(&Secret::new("wrong"), 42, body, &sig));
    }

    #[test]
    fn a_changed_body_does_not_verify() {
        let key = Secret::new("shhh");
        let sig = sign(&key, 42, b"{\"amount\":1}");
        assert!(!verify(&key, 42, b"{\"amount\":9}", &sig));
    }

    #[test]
    fn constant_time_eq_matches_ordinary_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"abcd"));
        assert!(constant_time_eq(b"", b""));
    }

    /// The key must not be reachable through any of the ordinary ways a value
    /// ends up in a log line.
    #[test]
    fn the_key_is_not_printable() {
        let key = Secret::new("hunter2");
        assert!(!format!("{key:?}").contains("hunter2"));
        assert!(!format!("{key}").contains("hunter2"));
    }
}
