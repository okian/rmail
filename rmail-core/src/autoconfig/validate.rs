//! The trust boundary between "something on the network said this" and
//! "rmail will connect here".
//!
//! Every value that reaches this module came from somewhere rmail does not
//! control: an autoconfig XML served by the domain being configured, a
//! response from Mozilla's ISPDB, a Microsoft autodiscover document, a DNS
//! SRV record, or a model's guess. None of them is more trusted than any
//! other, and all of them are parsed *before* they are believed:
//!
//! - **Hostnames** must be syntactically valid, ASCII, multi-label public DNS
//!   names. An IP literal is refused, so a document served by `example.com`
//!   cannot write `127.0.0.1` into someone's configuration and have rmail
//!   dial it, and so TLS always has a name to verify against.
//!
//!   Be precise about what that does *not* buy, because the tempting claim is
//!   "so a login can never reach an internal address" and it is false: a name
//!   is not an address. `metadata.google.internal`, or any attacker-owned
//!   name with an A record in `169.254.0.0/16` or RFC 1918, passes this check
//!   unchanged. What actually keeps the password away from such a host is
//!   TLS — [`crate::imap::conn::connect_tls`] verifies the certificate
//!   against the name, so a host that cannot present a valid certificate for
//!   it never sees a `LOGIN`. The TCP connect still happens, which leaves a
//!   port probe an attacker can aim at one address at a time; closing that
//!   would mean resolving the name and refusing non-global addresses before
//!   connecting, which this deliberately does not (yet) do.
//! - **Ports** must be in `1..=65535`. `0` is not "the default"; it is a
//!   value that means something else entirely to a connect call.
//! - **Security** has no plaintext variant at all — see [`Security`]. A
//!   discovery that offers only an unencrypted connection is refused here and
//!   never becomes a settings object, so no later stage has to remember to
//!   check.
//!
//! The refusals are deliberately loud (an error naming the offending value)
//! rather than a silent fallback to a default, because a silent fallback
//! would turn "this domain publishes a hostile autoconfig document" into
//! "autoconfig mysteriously suggested port 993". Every one of them truncates
//! the value it quotes: the text is attacker-controlled and ends up in a
//! `Status` message and a log line.

use std::net::IpAddr;

use crate::error::Error;

/// The longest a DNS name may be, in bytes (RFC 1035 §2.3.4).
const MAX_HOST_LEN: usize = 253;

/// The longest one DNS label may be, in bytes (RFC 1035 §2.3.4).
const MAX_LABEL_LEN: usize = 63;

/// How a client connects to a discovered server.
///
/// There is deliberately **no** plaintext variant. This is the type every
/// discovered server is carried in, so the absence of the variant is what
/// makes "autoconfig downgraded my connection to cleartext" unrepresentable
/// rather than merely unlikely: [`Security::parse`] refuses `plain`, and
/// there is no other way to build one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Security {
    /// STARTTLS: connect in the clear, then upgrade before authenticating.
    ///
    /// Ordered *below* [`Security::Tls`] on purpose — see
    /// [`Security::is_weaker_than`].
    StartTls,
    /// Implicit TLS from the first byte (IMAPS/SMTPS).
    Tls,
}

impl Security {
    /// The wire spelling used in this crate's own types and on the gRPC
    /// surface (not the provider vocabularies [`Security::parse`] accepts).
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Security::Tls => "tls",
            Security::StartTls => "starttls",
        }
    }

    /// Parse a discovered socket type.
    ///
    /// Accepts the vocabularies the probes actually emit: Mozilla autoconfig's
    /// `SSL`/`STARTTLS`, autodiscover's `on`/`off` for its `SSL` element, and
    /// this crate's own `tls`/`starttls`. Case-insensitive, because the
    /// documents in the wild are not consistent about it.
    ///
    /// # Errors
    ///
    /// [`Error::FailedPrecondition`] for a plaintext socket type — a refusal,
    /// not a parse failure, and worth telling apart from one.
    /// [`Error::InvalidArgument`] for anything unrecognized: an unknown
    /// spelling is not evidence of encryption.
    pub fn parse(raw: &str) -> Result<Self, Error> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "ssl" | "tls" | "ssl/tls" | "imaps" | "smtps" | "on" => Ok(Security::Tls),
            "starttls" | "tls-if-available" => Ok(Security::StartTls),
            "plain" | "none" | "off" | "no" | "cleartext" => {
                Err(Error::failed_precondition(format!(
                    "refusing a discovered server offering {}: rmail will not send \
                     credentials over an unencrypted connection, whatever the provider's \
                     configuration says",
                    quoted(raw)
                )))
            }
            _ => Err(Error::invalid_argument(format!(
                "unrecognized socket type {} in a discovered configuration; an \
                 unknown value is not evidence of an encrypted connection",
                quoted(raw)
            ))),
        }
    }

    /// Whether `self` is a weaker guarantee than `other`.
    ///
    /// The one comparison the "never downgrade on a discovery's say-so" rule
    /// needs: STARTTLS begins in the clear and depends on the server actually
    /// offering the upgrade, so it is weaker than implicit TLS.
    #[must_use]
    pub fn is_weaker_than(self, other: Self) -> bool {
        self < other
    }
}

/// Normalize and validate a discovered hostname.
///
/// Returns the lowercased, trailing-dot-stripped name to connect to.
///
/// # Errors
///
/// [`Error::InvalidArgument`] if the name is empty, too long, not ASCII, a
/// single label, an IP literal, or contains anything outside the LDH
/// (letter/digit/hyphen) label alphabet. The rejected value is named, because
/// the interesting case is a domain publishing something hostile and an
/// operator needing to see exactly what.
pub fn host(raw: &str) -> Result<String, Error> {
    let trimmed = raw.trim();
    // One trailing dot is the fully-qualified form and is how every SRV
    // target arrives; more than one is malformed.
    let candidate = trimmed.strip_suffix('.').unwrap_or(trimmed);
    let lowered = candidate.to_ascii_lowercase();

    if lowered.is_empty() {
        return Err(bad_host(raw, "it is empty"));
    }
    if lowered.len() > MAX_HOST_LEN {
        return Err(bad_host(raw, "it is longer than a DNS name may be"));
    }
    if !lowered.is_ascii() {
        // Providers publish punycode; a non-ASCII name here is either a
        // mis-encoded document or a homograph, and neither is worth guessing
        // at with the user's password.
        return Err(bad_host(raw, "it is not ASCII (expected punycode)"));
    }
    if lowered.parse::<IpAddr>().is_ok() {
        return Err(bad_host(
            raw,
            "it is an IP literal; a discovered server must be a published DNS name, \
             so that TLS has a name to verify and a document cannot aim a login at \
             an address inside this network",
        ));
    }
    let labels: Vec<&str> = lowered.split('.').collect();
    if labels.len() < 2 {
        return Err(bad_host(
            raw,
            "it is a single label; a discovered server must be a fully-qualified name",
        ));
    }
    for label in &labels {
        if label.is_empty() {
            return Err(bad_host(raw, "it has an empty label"));
        }
        if label.len() > MAX_LABEL_LEN {
            return Err(bad_host(raw, "it has a label longer than 63 bytes"));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(bad_host(raw, "a label starts or ends with a hyphen"));
        }
        if !label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(bad_host(
                raw,
                "it contains something outside the letter/digit/hyphen label alphabet",
            ));
        }
    }
    // A last label that is all digits means this parsed as a name but reads
    // as an address (`1.2.3.4` already failed above; `1.2.3.4.5` would not).
    if labels
        .last()
        .is_some_and(|tld| tld.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(bad_host(raw, "its last label is numeric"));
    }
    Ok(lowered)
}

/// Validate a discovered port.
///
/// # Errors
///
/// [`Error::InvalidArgument`] outside `1..=65535`. `0` is refused rather than
/// treated as "unset": a discovery that names port 0 has said something, and
/// what it said is not a port.
pub fn port(raw: i64) -> Result<u16, Error> {
    u16::try_from(raw).ok().filter(|p| *p != 0).ok_or_else(|| {
        Error::invalid_argument(format!(
            "refusing a discovered port {raw}: a port must be in 1..=65535"
        ))
    })
}

fn bad_host(raw: &str, why: &str) -> Error {
    Error::invalid_argument(format!(
        "refusing a discovered hostname {}: {why}",
        quoted(raw)
    ))
}

/// A discovered value, quoted and bounded, for an error message.
///
/// Every refusal in this module goes through it. The value is someone else's
/// text on its way into a `Status` message and a log line, and a 256 KiB
/// `<socketType>` element is a perfectly ordinary thing for a hostile
/// document to contain.
fn quoted(raw: &str) -> String {
    const MAX: usize = 80;
    let shown: String = raw.chars().take(MAX).collect();
    if raw.chars().nth(MAX).is_some() {
        format!("{shown:?}…")
    } else {
        format!("{shown:?}")
    }
}
