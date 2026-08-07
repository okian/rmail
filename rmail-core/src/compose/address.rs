//! A validated RFC 5322 mailbox: an addr-spec plus an optional display name.
//!
//! # Why validation lives in a type, not in the builder
//!
//! [`Mailbox`] can only be constructed by parsing, so every value of this type
//! is already known to hold an addr-spec that is safe to write into a header
//! and safe to hand to SMTP as a `RCPT TO`. That is what lets
//! [`crate::compose::mime::build`] be infallible about addresses: there is no
//! "what if the address is garbage" branch downstream, because a garbage
//! address never becomes a `Mailbox` in the first place. A caller that wants
//! to report the problem gets [`Error::InvalidArgument`] at the point the
//! string came in — the request boundary — where it can still say *which*
//! field was wrong.
//!
//! # What is deliberately not accepted
//!
//! - **Groups** (`Managers: a@x, b@y;`) — a legal RFC 5322 address form that
//!   no modern client composes and that would make the recipient list a tree
//!   instead of a list. A draft's recipients are a flat list by construction.
//! - **Non-ASCII addr-specs** (SMTPUTF8, RFC 6531). Accepting one here would
//!   produce a message this build cannot submit: the SMTP path (task 61) has
//!   no `SMTPUTF8` negotiation, so the server would reject the envelope at
//!   `MAIL FROM`/`RCPT TO` after the draft had already been accepted. Failing
//!   at compose time, where the user is still looking at the address, is the
//!   honest place to fail. Non-ASCII *display names* are fully supported —
//!   they are encoded per RFC 2047 at render time.
//! - **Address literals** (`user@[192.0.2.1]`) — legal, vanishingly rare, and
//!   the bracket syntax would have to be exempted from the domain character
//!   rules below, weakening them for every ordinary address to serve a case
//!   nobody composes by hand.
//!
//! # Length limits
//!
//! RFC 5321 §4.5.3.1 caps the local part at 64 octets and the domain at 255.
//! Those are enforced here rather than left to the SMTP server because they
//! are also what makes header folding *provably* safe: an addr-spec renders
//! to at most `1 + 64 + 1 + 255 + 1` octets, so the one place the renderer
//! genuinely cannot fold — inside `<...>` — is bounded far below RFC 5322's
//! 998-octet line limit.

use crate::error::Error;

/// RFC 5321 §4.5.3.1 — maximum octets in an addr-spec's local part.
const MAX_LOCAL: usize = 64;

/// RFC 5321 §4.5.3.1 — maximum octets in an addr-spec's domain.
const MAX_DOMAIN: usize = 255;

/// A display name longer than this is refused rather than silently truncated.
///
/// Nothing in the RFCs caps a display name, so the value is derived from what
/// the renderer can provably emit. Two of its three rendering forms fold or
/// chunk and so bound themselves — encoded-words at 75 octets each
/// (`mime::MAX_ENCODED_WORD`), a bare atom run at the name's own length — but
/// the quoted-string form does not: it is one unfoldable token, and every `\`
/// or `"` inside it *doubles*. A name of `N` backslashes renders to `2N + 2`
/// octets on a single line, so 400 is the largest round number that keeps the
/// worst case (802 octets, plus a header name or a fold's tab) comfortably
/// inside RFC 5322's 998-octet limit. Still an order of magnitude past any
/// real name.
const MAX_DISPLAY_NAME: usize = 400;

/// A validated mailbox: an addr-spec, and optionally a display name.
///
/// Constructed only through [`Mailbox::parse`] / [`Mailbox::new`], both of
/// which enforce the rules in the [module docs](self).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Mailbox {
    address: String,
    display_name: Option<String>,
}

impl Mailbox {
    /// Build a mailbox from an already-separated address and display name.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] if `address` is not a valid addr-spec, or
    /// the display name is empty-after-trimming (use `None`), over-long, or
    /// carries a control character.
    pub fn new(address: &str, display_name: Option<&str>) -> Result<Self, Error> {
        let address = validate_addr_spec(address.trim())?;
        let display_name = match display_name.map(str::trim) {
            None | Some("") => None,
            Some(name) => Some(validate_display_name(name)?),
        };
        Ok(Self {
            address,
            display_name,
        })
    }

    /// Parse `Name <addr@example.com>`, `"Quoted, Name" <a@x>`, or a bare
    /// `addr@example.com`.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidArgument`] as [`Mailbox::new`], plus a malformed
    /// `name <addr>` shape (unbalanced or empty angle brackets).
    pub fn parse(input: &str) -> Result<Self, Error> {
        let input = input.trim();
        if input.is_empty() {
            return Err(Error::invalid_argument("address must not be empty"));
        }

        // `rfind`, not `find`: a display name may legally contain a `<` inside
        // a quoted string, and the addr-spec is always the *last* bracketed
        // run. `rfind('<')` therefore lands on the real opening bracket even
        // for `"a <b" <c@x.com>`.
        let Some(open) = input.rfind('<') else {
            return Self::new(input, None);
        };
        let Some(close) = input.rfind('>') else {
            return Err(Error::invalid_argument(format!(
                "address {input:?} opens an angle bracket it never closes"
            )));
        };
        if close < open {
            return Err(Error::invalid_argument(format!(
                "address {input:?} closes an angle bracket before opening one"
            )));
        }
        let addr = &input[open + 1..close];
        let name = unquote(input[..open].trim());
        Self::new(addr, if name.is_empty() { None } else { Some(&name) })
    }

    /// The addr-spec (`local@domain`), verbatim.
    #[must_use]
    pub fn address(&self) -> &str {
        &self.address
    }

    /// The decoded display name, if any.
    #[must_use]
    pub fn display_name(&self) -> Option<&str> {
        self.display_name.as_deref()
    }

    /// The domain half of the addr-spec.
    ///
    /// Infallible: [`validate_addr_spec`] has already established that there
    /// is exactly one `@` with a non-empty domain after it.
    #[must_use]
    pub fn domain(&self) -> &str {
        match self.address.split_once('@') {
            Some((_, domain)) => domain,
            // Unreachable for a constructed `Mailbox`; returning the whole
            // string is the harmless answer, and cheaper than making every
            // caller handle an impossible `None`.
            None => &self.address,
        }
    }
}

/// Strip one layer of RFC 5322 quoted-string quoting from a display name,
/// undoing the `\"` / `\\` escapes inside it.
///
/// A name that is not quoted is returned unchanged, so this is safe to run
/// unconditionally on the pre-`<` text.
fn unquote(raw: &str) -> String {
    let inner = match raw.strip_prefix('"').and_then(|r| r.strip_suffix('"')) {
        Some(inner) => inner,
        None => return raw.to_owned(),
    };
    let mut out = String::with_capacity(inner.len());
    let mut escaped = false;
    for ch in inner.chars() {
        match (escaped, ch) {
            (false, '\\') => escaped = true,
            _ => {
                out.push(ch);
                escaped = false;
            }
        }
    }
    out
}

/// Validate an addr-spec against the rules in the [module docs](self),
/// returning it owned.
fn validate_addr_spec(addr: &str) -> Result<String, Error> {
    if addr.is_empty() {
        return Err(Error::invalid_argument("address must not be empty"));
    }
    if !addr.is_ascii() {
        return Err(Error::invalid_argument(format!(
            "address {addr:?} is not ASCII; internationalized addresses (SMTPUTF8) are not supported"
        )));
    }

    let mut parts = addr.split('@');
    let (Some(local), Some(domain), None) = (parts.next(), parts.next(), parts.next()) else {
        return Err(Error::invalid_argument(format!(
            "address {addr:?} must contain exactly one '@'"
        )));
    };

    if local.is_empty() || domain.is_empty() {
        return Err(Error::invalid_argument(format!(
            "address {addr:?} must have a non-empty local part and domain"
        )));
    }
    if local.len() > MAX_LOCAL {
        return Err(Error::invalid_argument(format!(
            "address local part exceeds {MAX_LOCAL} octets"
        )));
    }
    if domain.len() > MAX_DOMAIN {
        return Err(Error::invalid_argument(format!(
            "address domain exceeds {MAX_DOMAIN} octets"
        )));
    }

    // The local part: printable ASCII minus the specials that would end the
    // token or, worse, terminate the header. A quoted local part
    // (`"odd name"@x.com`) is legal RFC 5322 and rejected here for the same
    // reason groups are — nothing composes one, and allowing it would mean
    // carrying its quoting rules through every renderer and the SMTP
    // envelope.
    for ch in local.chars() {
        if !ch.is_ascii_graphic()
            || matches!(ch, '<' | '>' | '"' | ',' | ';' | ':' | '\\' | '[' | ']')
        {
            return Err(Error::invalid_argument(format!(
                "address {addr:?} has an unsupported character {ch:?} in its local part"
            )));
        }
    }

    // The domain: letters, digits, `-` and `.` only, with no empty label and
    // no label starting or ending in `-`. This is stricter than RFC 5322's
    // grammar and deliberately so — it is the set of domains that actually
    // resolve, and it makes an injected `\r\n` or a stray `>` structurally
    // impossible rather than merely unlikely.
    if domain.starts_with('.') || domain.ends_with('.') {
        return Err(Error::invalid_argument(format!(
            "address {addr:?} has a domain with an empty label"
        )));
    }
    for label in domain.split('.') {
        if label.is_empty() {
            return Err(Error::invalid_argument(format!(
                "address {addr:?} has a domain with an empty label"
            )));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(Error::invalid_argument(format!(
                "address {addr:?} has a domain label that starts or ends with '-'"
            )));
        }
        if !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(Error::invalid_argument(format!(
                "address {addr:?} has an unsupported character in its domain"
            )));
        }
    }

    Ok(addr.to_owned())
}

/// Validate a display name: bounded, and free of the control characters that
/// would let it break out of the header it is rendered into.
fn validate_display_name(name: &str) -> Result<String, Error> {
    if name.len() > MAX_DISPLAY_NAME {
        return Err(Error::invalid_argument(format!(
            "display name exceeds {MAX_DISPLAY_NAME} octets"
        )));
    }
    // Rejected, not stripped. A name containing a bare CR/LF is either a bug
    // in the caller or an attempt at header injection; silently repairing it
    // would hide both. (The renderer strips defensively as well — see
    // `mime::sanitize` — because that is the last line of defence for text
    // that reached a header some other way.)
    if let Some(bad) = name.chars().find(|c| c.is_control()) {
        return Err(Error::invalid_argument(format!(
            "display name contains a control character {bad:?}"
        )));
    }
    Ok(name.to_owned())
}

#[cfg(test)]
mod tests;
