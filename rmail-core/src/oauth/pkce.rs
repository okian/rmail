//! PKCE (RFC 7636): the verifier/challenge pair that makes a loopback
//! authorization code useless to anyone who intercepts it.

use argon2::password_hash::rand_core::{OsRng, RngCore};
use base64::Engine as _;
use sha2::{Digest, Sha256};

use crate::credential::Secret;

/// Entropy in the verifier, before base64url encoding.
///
/// RFC 7636 §4.1 allows 43–128 characters and recommends 32 octets of
/// randomness, which encodes to exactly 43 characters. Anything less is a
/// verifier an attacker holding the code could search.
const VERIFIER_BYTES: usize = 32;

/// A PKCE verifier and its S256 challenge.
///
/// The verifier is a [`Secret`]: it is the only thing standing between an
/// authorization code observed on the loopback socket and a working grant, so
/// it must not reach a log any more than the code itself may.
pub struct Pkce {
    verifier: Secret,
    challenge: String,
}

impl std::fmt::Debug for Pkce {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The challenge is public (it is in the authorization URL), but
        // printing it next to a redacted verifier invites someone to assume
        // the pair is safe to log wholesale.
        f.debug_struct("Pkce").finish_non_exhaustive()
    }
}

impl Pkce {
    /// Generate a fresh pair from the OS RNG.
    #[must_use]
    pub fn generate() -> Self {
        let mut bytes = [0u8; VERIFIER_BYTES];
        OsRng.fill_bytes(&mut bytes);
        let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
        let challenge = challenge_for(&verifier);
        Self {
            verifier: Secret::new(verifier),
            challenge,
        }
    }

    /// The verifier, posted to the token endpoint with the code.
    #[must_use]
    pub fn verifier(&self) -> &Secret {
        &self.verifier
    }

    /// The S256 challenge, sent in the authorization URL.
    #[must_use]
    pub fn challenge(&self) -> &str {
        &self.challenge
    }
}

/// `BASE64URL-ENCODE(SHA256(ASCII(verifier)))`, unpadded — RFC 7636 §4.2.
///
/// Padding is not merely cosmetic here: a `=` in the challenge is rejected by
/// both providers, since the base64url alphabet in RFC 7636 is the
/// no-padding one.
pub(super) fn challenge_for(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

/// A fresh `state` value: 32 bytes of OS entropy, base64url encoded.
///
/// Its only job is to be unguessable, so that a code delivered to the loopback
/// listener by anything other than the browser this process sent out can be
/// rejected. That makes it a bearer credential for the flow, hence a
/// [`Secret`].
pub(super) fn random_state() -> Secret {
    let mut bytes = [0u8; VERIFIER_BYTES];
    OsRng.fill_bytes(&mut bytes);
    Secret::new(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}
