//! Rendering a [`Draft`] into the exact RFC 5322 + MIME octets an SMTP
//! submission would transmit.
//!
//! # This is the submission serializer, not a preview
//!
//! [`build`] returns the byte-for-byte `DATA` payload. Task 61's SMTP path
//! hands these octets to `lettre` unchanged — it does not re-serialize, add
//! headers, or fix anything up — so anything wrong here is wrong on the wire.
//! Two consequences that shape the whole module:
//!
//! - **Everything is CRLF.** Every header line, every fold, every body line,
//!   every boundary delimiter. A lone `LF` anywhere is an SMTP protocol
//!   violation, and the encoders below normalize author-supplied text (which
//!   arrives with whatever line endings the client used) before encoding it.
//! - **No line may exceed 998 octets** (RFC 5322 §2.1.1). Every construct
//!   here is bounded by design — addresses by [`super::address`]'s length
//!   limits, headers by folding, encoded-words by RFC 2047's 75-octet cap,
//!   quoted-printable and base64 by their own line wrapping — and [`build`]
//!   re-checks the finished message as a cheap invariant assertion. A
//!   violation there is a bug in this module, never bad input, which is why
//!   it surfaces as [`Error::Internal`].
//!
//! What [`build`] deliberately does **not** do is send, queue, or persist
//! anything, and it emits no `Bcc` header: blind recipients reach the server
//! as `RCPT TO` commands and must not appear in the transmitted message, or
//! they are not blind. [`super::Draft::envelope_recipients`] is what the
//! submission path reads for that.
//!
//! # Transfer encoding
//!
//! A body is only labelled `7bit` when it provably is one — pure ASCII, no
//! over-long lines, no trailing whitespace that a relay would mangle. Anything
//! else is quoted-printable (mostly-ASCII text, which stays readable in the
//! raw source) or base64 (heavily non-ASCII text, where quoted-printable
//! triples the size, and every attachment, which is arbitrary binary).
//!
//! # Header encoding
//!
//! Non-ASCII reaches a header only as an RFC 2047 encoded-word, and only in
//! places RFC 2047 permits one: `Subject` and display names. An addr-spec is
//! **never** encoded — encoded-words are forbidden inside one, and a mail
//! server would have to guess at the real address.

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use chrono::{DateTime, FixedOffset, Local};
use sha2::{Digest, Sha256};

use super::address::Mailbox;
use super::{Draft, DraftAttachment};
use crate::error::Error;

/// The one line terminator this module emits.
const CRLF: &[u8] = b"\r\n";

/// RFC 5322 §2.1.1's hard limit, excluding the CRLF. Enforced as an
/// invariant over the finished message; see the module docs.
const MAX_LINE: usize = 998;

/// The line length header folding aims for. RFC 5322 §2.1.1 "recommends" 78
/// including CRLF; staying under it keeps every produced header comfortably
/// clear of [`MAX_LINE`] with room for a mail server to prepend to a fold.
const SOFT_LINE: usize = 76;

/// RFC 2047 §2 — an encoded-word, including its `=?charset?E?` prologue and
/// `?=` epilogue, may not exceed 75 octets.
const MAX_ENCODED_WORD: usize = 75;

/// Octets an encoded-word's payload may occupy: [`MAX_ENCODED_WORD`] minus
/// `=?utf-8?Q?` and `?=`.
const ENCODED_WORD_PAYLOAD: usize = MAX_ENCODED_WORD - "=?utf-8?Q?".len() - "?=".len();

/// RFC 2045 §6.7 — a quoted-printable line may not exceed 76 octets
/// *including* a trailing soft-break `=`, so content stops one short.
const MAX_QP_LINE: usize = 75;

/// Base64 body lines. RFC 2045 §6.8 caps them at 76; 76 is a multiple of 4,
/// so no line ever splits a quantum.
const MAX_BASE64_LINE: usize = 76;

/// The longest single ASCII run [`fold_unstructured`] will place on its own
/// continuation line rather than falling back to encoded-words.
///
/// A URL or a hash in a subject is legible as-is and illegible once
/// RFC 2047-encoded, so a long unbreakable token is worth a long line. The
/// value leaves ~90 octets of headroom under [`MAX_LINE`] for the header name
/// and the folding whitespace.
const MAX_UNFOLDABLE_TOKEN: usize = 900;

/// A parent `Message-ID` longer than this is dropped from `References` /
/// `In-Reply-To` rather than emitted.
///
/// Folding cannot break inside a single `<...>` token, so an absurd id — which
/// arrives from whoever sent the parent, i.e. from an untrusted source — is
/// the one input that could push a header past [`MAX_LINE`]. Dropping it
/// degrades threading for that one pathological conversation; emitting it
/// would produce a message some relays reject outright.
const MAX_MESSAGE_ID: usize = 250;

/// How many ids `References` may carry.
///
/// RFC 5322 §3.6.4 says to append to the parent's chain and stops there, so a
/// long-running thread's `References` grows without bound — real threads have
/// been observed with hundreds of ids and multi-kilobyte headers. Every
/// threading algorithm in practice (JWZ's, and every MUA that implements it)
/// needs two things from the chain: the **first** id, which names the
/// conversation, and the **most recent** ancestors, which place this message
/// within it. The middle is what gets dropped, so the root and the local
/// neighbourhood both survive — the same truncation strategy RFC 5537 §3.4.4
/// specifies for Netnews, for the identical reason.
const MAX_REFERENCES: usize = 20;

/// Per-send values that are not part of the draft: the `Message-ID` this
/// transmission is identified by, and the `Date` it claims.
///
/// Separate from [`Draft`] because they are minted *per send attempt*, not
/// per edit — task 61 persists the `Message-ID` before SMTP `DATA` so a retry
/// after a crash can recognise an already-delivered message instead of
/// sending a second copy. Constructing one explicitly also makes [`build`]
/// deterministic, which is what lets the tests below assert on exact octets.
#[derive(Debug, Clone)]
pub struct Envelope {
    message_id: String,
    date: DateTime<FixedOffset>,
}

impl Envelope {
    /// Mint an envelope for sending `draft` right now: a fresh `Message-ID`
    /// in the sending account's domain, and the local wall clock with its
    /// UTC offset.
    #[must_use]
    pub fn now(draft: &Draft) -> Self {
        Self {
            message_id: generate_message_id(draft.from.domain()),
            date: Local::now().fixed_offset(),
        }
    }

    /// An envelope with an explicit id and date — for a retry that must reuse
    /// the id it already committed to, and for tests that assert on bytes.
    #[must_use]
    pub fn new(message_id: String, date: DateTime<FixedOffset>) -> Self {
        Self { message_id, date }
    }

    /// The bare `Message-ID` (no angle brackets).
    #[must_use]
    pub fn message_id(&self) -> &str {
        &self.message_id
    }

    /// The `Date` this message claims.
    #[must_use]
    pub fn date(&self) -> DateTime<FixedOffset> {
        self.date
    }
}

/// Mint a globally unique `Message-ID` (without angle brackets) in `domain`.
///
/// Uniqueness comes from three independent sources, because any one alone has
/// a collision mode: the clock (distinct per send, but repeats across a
/// wall-clock rewind), a per-process counter (distinct within a run, but
/// restarts at zero), and the pid (distinct per concurrent process, but
/// recycled by the OS). The domain closes it globally — an id minted here can
/// only collide with another id minted by an rmail sending as the same
/// domain, which the first two components already separate.
///
/// The right half is a truncated SHA-256 rather than the raw values so the id
/// does not leak the sending host's process table or precise clock, both of
/// which travel with the message to every recipient forever.
#[must_use]
pub fn generate_message_id(domain: &str) -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let secs = nanos / 1_000_000_000;
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);

    let mut hasher = Sha256::new();
    hasher.update(nanos.to_le_bytes());
    hasher.update(counter.to_le_bytes());
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(domain.as_bytes());
    let digest = hex(&hasher.finalize()[..10]);

    // A domain that is not usable in an id (empty, or somehow not the
    // validated addr-spec domain a `Mailbox` guarantees) falls back to the
    // RFC 2606 reserved TLD, which can never be a real host — better an id
    // that is obviously local-only than one that impersonates a real domain.
    let domain = if domain.is_empty() || !is_id_safe(domain) {
        "rmail.invalid"
    } else {
        domain
    };
    format!("{secs}.{digest}@{domain}")
}

/// Render `draft` as the complete RFC 5322 message an SMTP `DATA` would carry.
///
/// # Errors
///
/// [`Error::InvalidArgument`] if the draft names no recipient at all — there
/// is nothing to address such a message to, so it cannot be rendered as one.
/// [`Error::Internal`] if the finished message violates RFC 5322's line-length
/// limit, which is an assertion about this module rather than about the input
/// (see the [module docs](self)).
pub fn build(draft: &Draft, envelope: &Envelope) -> Result<Vec<u8>, Error> {
    if draft.to.is_empty() && draft.cc.is_empty() && draft.bcc.is_empty() {
        return Err(Error::invalid_argument(
            "a draft needs at least one To/Cc/Bcc recipient before it can be rendered",
        ));
    }

    let mut headers = Vec::new();
    headers.push(format!("Date: {}", envelope.date.to_rfc2822()));
    headers.push(fold_addresses("From", std::slice::from_ref(&draft.from)));
    if !draft.to.is_empty() {
        headers.push(fold_addresses("To", &draft.to));
    }
    if !draft.cc.is_empty() {
        headers.push(fold_addresses("Cc", &draft.cc));
    }
    // No `Bcc` — see the module docs.
    if !draft.subject.trim().is_empty() {
        headers.push(fold_unstructured("Subject", &draft.subject));
    }
    headers.push(format!(
        "Message-ID: <{}>",
        sanitize_message_id(envelope.message_id())
    ));
    if let Some(parent) = draft.in_reply_to.as_deref().filter(|id| usable_id(id)) {
        headers.push(fold_ids("In-Reply-To", std::slice::from_ref(&parent)));
    }
    let references = capped_references(&draft.references);
    if !references.is_empty() {
        headers.push(fold_ids("References", &references));
    }
    headers.push("MIME-Version: 1.0".to_owned());

    let entity = body_entity(draft);
    headers.extend(entity.headers);

    let mut out = Vec::new();
    for header in &headers {
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(CRLF);
    }
    out.extend_from_slice(CRLF);
    out.extend_from_slice(&entity.body);
    // SMTP terminates `DATA` with `CRLF.CRLF`, so the payload must itself end
    // on a line boundary. Multipart bodies already do (the closing delimiter);
    // a bare text body need not.
    if !out.ends_with(CRLF) {
        out.extend_from_slice(CRLF);
    }

    check_line_lengths(&out)?;
    Ok(out)
}

/// The `References` chain a reply to a message with these headers must carry,
/// per RFC 5322 §3.6.4.
///
/// The rule the RFC states and every threading implementation depends on:
/// take the parent's own `References`, append the parent's `Message-ID`. The
/// two fallbacks matter more than they look —
///
/// - a parent with **no** `References` (the first message of a thread) still
///   contributes its own id, which is what makes the reply the second link of
///   a chain rather than the start of a new one;
/// - a parent with no `References` but an `In-Reply-To` (common from clients
///   that only ever set one of the two) contributes that id first, so the
///   grandparent is not lost.
///
/// Everything is deduplicated in place: a malformed parent can repeat an id,
/// and a chain with duplicates confuses threaders that count links.
#[must_use]
pub fn reply_references(
    parent_references: &[String],
    parent_in_reply_to: &[String],
    parent_message_id: Option<&str>,
) -> Vec<String> {
    let mut chain: Vec<String> = Vec::new();
    let mut push = |id: &str| {
        let id = sanitize_message_id(id);
        if usable_id(&id) && !chain.contains(&id) {
            chain.push(id);
        }
    };

    let inherited = if parent_references.is_empty() {
        parent_in_reply_to
    } else {
        parent_references
    };
    for id in inherited {
        push(id);
    }
    if let Some(id) = parent_message_id {
        push(id);
    }
    chain
}

// ---------------------------------------------------------------------------
// Body structure
// ---------------------------------------------------------------------------

/// One MIME entity: its own content headers plus its already-encoded body.
struct Entity {
    headers: Vec<String>,
    body: Vec<u8>,
}

impl Entity {
    /// The entity as it appears inside a multipart: headers, blank line, body.
    fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.body.len() + 128);
        for header in &self.headers {
            out.extend_from_slice(header.as_bytes());
            out.extend_from_slice(CRLF);
        }
        out.extend_from_slice(CRLF);
        out.extend_from_slice(&self.body);
        out
    }
}

/// Assemble the draft's content into a single entity, nesting exactly as far
/// as the content requires:
///
/// | text | html | attachments | structure                                    |
/// |------|------|-------------|----------------------------------------------|
/// | yes  | no   | no          | `text/plain`                                 |
/// | yes  | yes  | no          | `multipart/alternative`                      |
/// | yes  | no   | yes         | `multipart/mixed`                            |
/// | yes  | yes  | yes         | `multipart/mixed` wrapping `.../alternative` |
///
/// The nesting order is not a preference: `multipart/alternative` means "the
/// same content, pick one", so an attachment inside it would be an
/// *alternative to the message body* rather than something sent alongside it.
/// Mixed-wrapping-alternative is the only arrangement that says what is meant.
fn body_entity(draft: &Draft) -> Entity {
    let text = text_entity("plain", &draft.body_text);
    let core = match draft.body_html.as_deref() {
        Some(html) if !html.is_empty() => {
            // Least-faithful alternative first, per RFC 2046 §5.1.4: a client
            // renders the last part it understands.
            multipart_entity("alternative", vec![text, text_entity("html", html)])
        }
        _ => text,
    };

    if draft.attachments.is_empty() {
        return core;
    }
    let mut parts = Vec::with_capacity(draft.attachments.len() + 1);
    parts.push(core);
    parts.extend(draft.attachments.iter().map(attachment_entity));
    multipart_entity("mixed", parts)
}

/// A `text/<subtype>; charset="utf-8"` entity with the narrowest transfer
/// encoding that is actually safe for its content.
fn text_entity(subtype: &str, text: &str) -> Entity {
    let normalized = normalize_crlf(text);
    let (encoding, body) = encode_text(&normalized);
    Entity {
        headers: vec![
            format!("Content-Type: text/{subtype}; charset=\"utf-8\""),
            format!("Content-Transfer-Encoding: {encoding}"),
        ],
        body,
    }
}

/// A base64 attachment part.
///
/// Always base64: attachment bytes are arbitrary, and "arbitrary" includes
/// NULs, lone CRs, and megabyte-long runs without a newline — none of which
/// survive a 7bit or quoted-printable claim.
fn attachment_entity(attachment: &DraftAttachment) -> Entity {
    let content_type = sanitize_content_type(&attachment.content_type);
    let mut headers = vec![
        format!("Content-Type: {content_type}"),
        "Content-Transfer-Encoding: base64".to_owned(),
    ];
    headers.push(format!(
        "Content-Disposition: attachment;{}",
        filename_parameter(&attachment.filename)
    ));
    Entity {
        headers,
        body: encode_base64(&attachment.content),
    }
}

/// Wrap `parts` in a `multipart/<subtype>` with a boundary that provably does
/// not occur inside any of them.
fn multipart_entity(subtype: &str, parts: Vec<Entity>) -> Entity {
    let serialized: Vec<Vec<u8>> = parts.iter().map(Entity::to_bytes).collect();
    let boundary = unique_boundary(&serialized);

    let mut body = Vec::new();
    for part in &serialized {
        body.extend_from_slice(b"--");
        body.extend_from_slice(boundary.as_bytes());
        body.extend_from_slice(CRLF);
        body.extend_from_slice(part);
        // RFC 2046 §5.1.1: the CRLF preceding a delimiter belongs to the
        // delimiter, not to the part — so this is not an extra blank line,
        // it is the boundary's own leading break.
        body.extend_from_slice(CRLF);
    }
    body.extend_from_slice(b"--");
    body.extend_from_slice(boundary.as_bytes());
    body.extend_from_slice(b"--");
    body.extend_from_slice(CRLF);

    Entity {
        headers: vec![format!(
            "Content-Type: multipart/{subtype}; boundary=\"{boundary}\""
        )],
        body,
    }
}

/// A boundary that appears in none of `parts`.
///
/// The generated shape (`----=_rmail_<hex>`) cannot occur in base64 output
/// (which has no `-` or `_`) or in quoted-printable output (which would have
/// escaped the `=` as `=3D`), so the first candidate is essentially always
/// accepted. The loop exists anyway because "essentially always" is not a
/// guarantee once a `7bit` part can carry the author's literal text, and a
/// boundary colliding with content silently truncates the message.
fn unique_boundary(parts: &[Vec<u8>]) -> String {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part);
    }
    let seed = hasher.finalize();

    for salt in 0u16..=u16::from(u8::MAX) {
        let mut hasher = Sha256::new();
        hasher.update(seed);
        hasher.update(salt.to_le_bytes());
        let candidate = format!("----=_rmail_{}", hex(&hasher.finalize()[..12]));
        if !parts
            .iter()
            .any(|part| contains(part, candidate.as_bytes()))
        {
            return candidate;
        }
    }
    // Unreachable in practice (256 independent 96-bit values would all have to
    // collide with the content); a time-and-pid suffix is still unique enough
    // to be a correct message rather than a panic.
    format!(
        "----=_rmail_fallback_{}_{}",
        std::process::id(),
        generate_message_id("").replace('@', "_")
    )
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

// ---------------------------------------------------------------------------
// Transfer encodings
// ---------------------------------------------------------------------------

/// Pick a transfer encoding for already-CRLF-normalized text and apply it,
/// returning the `Content-Transfer-Encoding` token and the encoded bytes.
fn encode_text(text: &str) -> (&'static str, Vec<u8>) {
    if is_seven_bit_clean(text) {
        return ("7bit", text.as_bytes().to_vec());
    }
    // Quoted-printable costs three octets per encoded byte, so past roughly a
    // quarter non-ASCII it is larger than base64 *and* unreadable anyway —
    // which is the only thing quoted-printable was buying.
    let non_ascii = text.bytes().filter(|b| !b.is_ascii()).count();
    if non_ascii * 4 <= text.len() {
        ("quoted-printable", encode_quoted_printable(text))
    } else {
        ("base64", encode_base64(text.as_bytes()))
    }
}

/// Whether `text` can honestly be labelled `7bit`.
///
/// Stricter than "every byte is ASCII", because a `7bit` label also promises
/// the relay chain that lines are short and that nothing will be mangled in
/// transit. Trailing whitespace is stripped by some relays and lines are
/// wrapped by others; both silently corrupt content that claimed it needed no
/// encoding.
fn is_seven_bit_clean(text: &str) -> bool {
    if !text.is_ascii() {
        return false;
    }
    for line in text.split("\r\n") {
        if line.len() > SOFT_LINE {
            return false;
        }
        if line.ends_with(' ') || line.ends_with('\t') {
            return false;
        }
        // A bare CR (a lone `\r` that `normalize_crlf` could not pair) or any
        // other control character has no business in a 7bit body.
        if line
            .bytes()
            .any(|b| b == 0 || (b < 0x20 && b != b'\t') || b == 0x7f)
        {
            return false;
        }
        // `From ` at the start of a line is rewritten by mbox-style storage
        // ("From-mangling"), and a leading `.` is what SMTP's `DATA`
        // terminator is made of. Both are escaped on the quoted-printable
        // path; a `7bit` body has no escaping mechanism at all, so the only
        // way to protect them here is to refuse the label and let
        // quoted-printable take over.
        if line.starts_with("From ") || line.starts_with('.') {
            return false;
        }
    }
    true
}

/// RFC 2045 §6.7 quoted-printable, with soft line breaks at
/// [`MAX_QP_LINE`].
///
/// Beyond the base rules, three things are escaped that a naive encoder would
/// leave alone, each because something downstream rewrites them: whitespace at
/// end of line (relays strip it), `From ` at start of line (mbox mangles it),
/// and `.` at start of line (a belt-and-braces against an SMTP client that
/// forgets to dot-stuff).
fn encode_quoted_printable(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() + text.len() / 4);

    for (index, line) in text.split("\r\n").enumerate() {
        if index > 0 {
            out.extend_from_slice(CRLF);
        }
        let bytes = line.as_bytes();
        let mut column = 0usize;

        for (position, &byte) in bytes.iter().enumerate() {
            let at_line_start = position == 0;
            let is_last = position + 1 == bytes.len();
            let mut literal = match byte {
                b'=' => false,
                b' ' | b'\t' => !is_last,
                b'F' if at_line_start && line.starts_with("From ") => false,
                b'.' if at_line_start => false,
                0x21..=0x7e => true,
                _ => false,
            };

            // RFC 2045 rule #3: whitespace must not appear at the end of an
            // *encoded* line — which includes a line ended by a soft break,
            // not only the end of the source line (`is_last`, above). Whether
            // a space lands there depends on whether the next atom, up to
            // three octets, still fits. Encoding it whenever the line is
            // within four columns of the limit is a strictly-safe
            // approximation: it costs two octets, at most once per line, and
            // it is the difference between a space surviving a relay that
            // strips trailing whitespace and one that does not.
            if literal && matches!(byte, b' ' | b'\t') && column + 4 > MAX_QP_LINE {
                literal = false;
            }

            let width = if literal { 1 } else { 3 };
            if column + width > MAX_QP_LINE {
                out.extend_from_slice(b"=");
                out.extend_from_slice(CRLF);
                column = 0;
            }
            if literal {
                out.push(byte);
            } else {
                out.extend_from_slice(format!("={byte:02X}").as_bytes());
            }
            column += width;
        }
    }
    out
}

/// RFC 2045 §6.8 base64, wrapped at [`MAX_BASE64_LINE`] with CRLF.
fn encode_base64(bytes: &[u8]) -> Vec<u8> {
    let encoded = BASE64.encode(bytes);
    let mut out = Vec::with_capacity(encoded.len() + encoded.len() / MAX_BASE64_LINE * 2 + 2);
    for chunk in encoded.as_bytes().chunks(MAX_BASE64_LINE) {
        out.extend_from_slice(chunk);
        out.extend_from_slice(CRLF);
    }
    out
}

/// Collapse any mix of `CRLF`, bare `LF`, and bare `CR` into `CRLF`.
///
/// Author-supplied text arrives with whatever line endings the client used; a
/// body must be canonical CRLF *before* it is encoded, or a base64 or 7bit
/// part carries lone LFs onto the wire.
fn normalize_crlf(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push_str("\r\n");
            }
            '\n' => out.push_str("\r\n"),
            _ => out.push(ch),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Header construction
// ---------------------------------------------------------------------------

/// `Name: <value>`, folded, with non-ASCII carried as RFC 2047 encoded-words.
///
/// Runs of whitespace in an all-ASCII value collapse to a single space, which
/// is what unfolding an RFC 5322 header does anyway — a receiving client
/// cannot tell an author's double space from a fold. Values that go through
/// encoded-words keep their whitespace exactly, since it is encoded rather
/// than emitted as folding whitespace.
fn fold_unstructured(name: &str, value: &str) -> String {
    let value = sanitize(value);
    let plain = value.is_ascii()
        // A literal `=?…?=` must be re-encoded, not passed through — see
        // `looks_like_encoded_word`.
        && !looks_like_encoded_word(&value)
        && value
            .split_whitespace()
            .all(|t| t.len() <= MAX_UNFOLDABLE_TOKEN);
    let tokens = if plain {
        value.split_whitespace().map(str::to_owned).collect()
    } else {
        encoded_words(&value)
    };
    let groups: Vec<Vec<String>> = tokens.into_iter().map(|token| vec![token]).collect();
    join_folded(name, &groups, "")
}

/// `Name: <mailbox>, <mailbox>, ...`, folded between addresses **and** inside
/// one, between a display name and its addr-spec.
fn fold_addresses(name: &str, mailboxes: &[Mailbox]) -> String {
    let groups: Vec<Vec<String>> = mailboxes.iter().map(mailbox_tokens).collect();
    join_folded(name, &groups, ",")
}

/// `Name: <id> <id> ...`, folded between ids. Message-id lists are
/// whitespace-separated, not comma-separated (RFC 5322 §3.6.4).
fn fold_ids(name: &str, ids: &[impl AsRef<str>]) -> String {
    let groups: Vec<Vec<String>> = ids
        .iter()
        .map(|id| vec![format!("<{}>", sanitize_message_id(id.as_ref()))])
        .collect();
    join_folded(name, &groups, "")
}

/// Join `groups` of tokens under `name`, folding to a continuation line
/// before a token would push past [`SOFT_LINE`] and appending `separator`
/// after the last token of every group but the last.
///
/// The two levels exist because an address is one *item* of a comma-separated
/// list but several *tokens*: `=?utf-8?Q?Caf=C3=A9?= <a@x>` may legally fold
/// between the display name and the angle-addr (both sit in a phrase, where
/// folding whitespace is allowed), and it must be able to — see
/// [`mailbox_tokens`]. Only the last token of a group carries the comma, so
/// folding inside an address never produces one.
///
/// A token that does not fit on a line of its own still goes on one, which is
/// safe because every caller bounds its token lengths: encoded-words by
/// [`MAX_ENCODED_WORD`], plain subject runs by [`MAX_UNFOLDABLE_TOKEN`],
/// message-ids by [`MAX_MESSAGE_ID`], and display names and addr-specs by
/// [`super::address`]'s own limits. `a_message_at_every_documented_maximum_stays_within_the_line_limit`
/// in this module's tests is what holds that claim to account.
fn join_folded(name: &str, groups: &[Vec<String>], separator: &str) -> String {
    let start = name.len() + 1;
    let mut out = String::with_capacity(start + groups.len() * 48);
    out.push_str(name);
    out.push(':');
    let mut column = start;

    let last_group = groups.len().saturating_sub(1);
    for (group_index, group) in groups.iter().enumerate() {
        let last_token = group.len().saturating_sub(1);
        for (token_index, token) in group.iter().enumerate() {
            let tail = if group_index != last_group && token_index == last_token {
                separator
            } else {
                ""
            };
            let width = 1 + token.len() + tail.len();
            if column > start && column + width > SOFT_LINE {
                // The folding whitespace *is* the separator on a continuation
                // line, so no space is added on top of the tab.
                out.push_str("\r\n\t");
                column = 1;
            } else {
                out.push(' ');
                column += 1;
            }
            out.push_str(token);
            out.push_str(tail);
            column += token.len() + tail.len();
        }
    }
    out
}

/// One address as a sequence of foldable tokens: its display name in
/// whichever of the three legal forms it needs, then the addr-spec verbatim.
///
/// Returned as tokens rather than one string so a long display name and a
/// long addr-spec cannot combine into a single unbreakable run — the two are
/// individually bounded (see [`super::address`]), their concatenation is not,
/// and `join_folded` can only fold *between* tokens.
fn mailbox_tokens(mailbox: &Mailbox) -> Vec<String> {
    let addr = format!("<{}>", mailbox.address());
    let Some(name) = mailbox.display_name() else {
        return vec![addr];
    };
    let mut tokens = if !name.is_ascii() || looks_like_encoded_word(name) {
        // RFC 2047 §5(3): an encoded-word may appear in a phrase, and it must
        // NOT be wrapped in a quoted-string — inside quotes it is literal text,
        // so the recipient would see `=?utf-8?Q?Caf=C3=A9?=` on screen.
        encoded_words(name)
    } else if name.bytes().any(is_atom_special) {
        vec![format!(
            "\"{}\"",
            name.replace('\\', r"\\").replace('"', "\\\"")
        )]
    } else {
        vec![name.to_owned()]
    };
    tokens.push(addr);
    tokens
}

/// Whether `value` contains something a receiving client would decode as an
/// RFC 2047 encoded-word.
///
/// An all-ASCII value normally goes into a header verbatim, which means an
/// author who literally types `=?utf-8?B?QmFuayBvZiBBbWVyaWNh?=` gets a
/// message whose subject *displays as* `Bank of America`. That is a
/// spoofing primitive, and it matters here specifically because prd.md has
/// Claude authoring drafts from untrusted mail content. Re-encoding the
/// value makes the literal text survive as literal text.
fn looks_like_encoded_word(value: &str) -> bool {
    value.contains("=?")
}

/// Whether a byte forces a display name into a quoted-string: RFC 5322 §3.2.3
/// `specials`, plus controls.
///
/// A plain space is deliberately *not* special — a phrase is `1*word`, so
/// `Alice Example <a@x>` needs no quoting, and quoting every two-word name
/// would be noise in the raw source for no gain. Any other whitespace is a
/// control character, which [`super::address`] rejects outright before a
/// display name can get here.
fn is_atom_special(byte: u8) -> bool {
    matches!(
        byte,
        b'(' | b')' | b'<' | b'>' | b'[' | b']' | b':' | b';' | b'@' | b'\\' | b',' | b'.' | b'"'
    ) || byte < b' '
        || byte == 0x7f
}

/// Split `text` into RFC 2047 encoded-words, each within
/// [`MAX_ENCODED_WORD`] octets and each covering a whole number of
/// characters (RFC 2047 §2 — a decoder must be able to decode each word
/// independently, so a UTF-8 sequence may never straddle two).
///
/// `Q` and `B` are both emitted, whichever is shorter for the text at hand:
/// `Q` keeps a mostly-ASCII subject readable in the raw source, while `B`
/// avoids tripling the size of a subject that is mostly non-Latin script.
fn encoded_words(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    let q_cost: usize = text.chars().map(q_width).sum();
    // base64 is 4 octets per 3 source octets, rounded up.
    let b_cost = text.len().div_ceil(3) * 4;
    if q_cost <= b_cost {
        chunk_encoded_words(text, 'Q', |chunk| {
            chunk.chars().map(q_encode_char).collect()
        })
    } else {
        chunk_encoded_words(text, 'B', |chunk| BASE64.encode(chunk))
    }
}

/// Greedily split `text` into encoded-words whose payload stays within
/// [`ENCODED_WORD_PAYLOAD`] octets, encoding each chunk with `encode`.
///
/// The budget is measured by *encoding the candidate*, not by summing a
/// per-character width estimate. Base64 is the reason: it packs three source
/// octets into four, so any per-character estimate has to round up to four
/// octets per character and ends up producing three times as many
/// encoded-words as the limit actually requires. Chunks are bounded by
/// [`ENCODED_WORD_PAYLOAD`], so the repeated encoding is bounded work per
/// character, not quadratic in the whole header.
fn chunk_encoded_words(text: &str, scheme: char, encode: impl Fn(&str) -> String) -> Vec<String> {
    let mut words = Vec::new();
    let mut chunk = String::new();

    for ch in text.chars() {
        // The first character of a chunk always goes in, even in the
        // impossible case that it alone overflows the budget (the widest a
        // single character can encode to is 12 octets), so this never loops
        // without making progress.
        if !chunk.is_empty() {
            let mut candidate = chunk.clone();
            candidate.push(ch);
            if encode(&candidate).len() > ENCODED_WORD_PAYLOAD {
                words.push(format!("=?utf-8?{scheme}?{}?=", encode(&chunk)));
                chunk.clear();
            }
        }
        chunk.push(ch);
    }
    if !chunk.is_empty() {
        words.push(format!("=?utf-8?{scheme}?{}?=", encode(&chunk)));
    }
    words
}

/// Octets `ch` occupies in a `Q`-encoded word — used only to choose between
/// `Q` and `B`, not to size a chunk (see [`chunk_encoded_words`]).
fn q_width(ch: char) -> usize {
    if is_q_literal(ch) || ch == ' ' {
        1
    } else {
        ch.len_utf8() * 3
    }
}

/// `Q`-encode one character (RFC 2047 §4.2).
fn q_encode_char(ch: char) -> String {
    if is_q_literal(ch) {
        return ch.to_string();
    }
    if ch == ' ' {
        return "_".to_owned();
    }
    let mut buf = [0u8; 4];
    ch.encode_utf8(&mut buf)
        .bytes()
        .map(|b| format!("={b:02X}"))
        .collect()
}

/// The conservative `Q` literal set: safe in a phrase (a display name) as well
/// as in unstructured text, so one encoder serves both contexts. RFC 2047 §4.2
/// permits more in unstructured text; nothing is lost by not using it beyond a
/// few octets.
fn is_q_literal(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '!' | '*' | '+' | '-' | '/')
}

// ---------------------------------------------------------------------------
// Parameters, sanitizing, invariants
// ---------------------------------------------------------------------------

/// The `filename` parameter for a `Content-Disposition`, folded onto its own
/// continuation line.
///
/// A non-ASCII filename uses RFC 2231's `filename*=utf-8''<pct>` extended
/// syntax rather than an RFC 2047 encoded-word: encoded-words are not legal
/// inside a parameter value, and while many clients decode one there anyway,
/// RFC 2231 is what the specification actually provides and what
/// `mail_parser` (the parser this workspace already uses, and therefore the
/// one the round-trip tests hold this to) implements.
fn filename_parameter(filename: &str) -> String {
    let filename = sanitize(filename);
    if filename.is_ascii() {
        let quoted = filename.replace('\\', r"\\").replace('"', "\\\"");
        format!("\r\n\tfilename=\"{quoted}\"")
    } else {
        format!("\r\n\tfilename*=utf-8''{}", percent_encode(&filename))
    }
}

/// RFC 2231 §4 percent-encoding: everything outside `attribute-char` escaped.
fn percent_encode(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'!' | b'#'
                        | b'$'
                        | b'&'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
            {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// Reduce a stored content type to a bare `type/subtype` of RFC 2045 token
/// characters, falling back to the universal default.
///
/// Parameters are dropped rather than passed through: the only one that
/// matters for an attachment is `name`, which `Content-Disposition`'s
/// `filename` supersedes, and an unvalidated parameter string is a header
/// injection vector for no benefit.
///
/// The length check is what keeps this function safe *on its own*, the same
/// way [`sanitize`] and [`sanitize_message_id`] are:
/// [`super::MAX_CONTENT_TYPE`] already rejects an over-long type at the
/// request boundary, where the caller can be told why, but a `Content-Type`
/// header is unfoldable (it is one token) so a value that got here some other
/// way would produce a line no relay accepts.
fn sanitize_content_type(raw: &str) -> String {
    let base = raw
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let Some((ctype, subtype)) = base.split_once('/') else {
        return "application/octet-stream".to_owned();
    };
    let ok = |s: &str| {
        !s.is_empty()
            && s.bytes().all(|b| {
                b.is_ascii_alphanumeric()
                    || matches!(
                        b,
                        b'!' | b'#'
                            | b'$'
                            | b'%'
                            | b'&'
                            | b'\''
                            | b'*'
                            | b'+'
                            | b'-'
                            | b'.'
                            | b'^'
                            | b'_'
                            | b'`'
                            | b'|'
                            | b'~'
                    )
            })
    };
    if ok(ctype) && ok(subtype) && base.len() <= super::MAX_CONTENT_TYPE {
        format!("{ctype}/{subtype}")
    } else {
        "application/octet-stream".to_owned()
    }
}

/// Strip the characters that would let a value escape the header it is
/// rendered into.
///
/// Every value reaching here has already been validated at its own boundary
/// ([`super::address`] rejects control characters in a display name;
/// [`super::DraftStore`] rejects them in a subject), so this is the last line
/// of defence rather than the only one — and it strips rather than errors
/// precisely because by this point there is no caller left to report to.
fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect::<String>()
        .trim()
        .to_owned()
}

/// A `Message-ID`'s interior, with anything that could terminate the token
/// removed. Angle brackets are added by the caller.
fn sanitize_message_id(id: &str) -> String {
    id.trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .chars()
        .filter(|c| !c.is_control() && !c.is_whitespace() && *c != '<' && *c != '>')
        .collect()
}

/// Whether a parent id is worth emitting: non-empty once sanitized, and short
/// enough that a header carrying it can still be folded within
/// [`MAX_LINE`] (see [`MAX_MESSAGE_ID`]).
fn usable_id(id: &str) -> bool {
    let clean = sanitize_message_id(id);
    !clean.is_empty() && clean.len() <= MAX_MESSAGE_ID
}

/// Whether a domain can be placed in a `Message-ID` without escaping it.
fn is_id_safe(domain: &str) -> bool {
    domain
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
}

/// `references`, filtered to usable ids and truncated to [`MAX_REFERENCES`]
/// by dropping from the middle — see that constant's docs for why the head
/// and the tail are the parts that matter.
fn capped_references(references: &[String]) -> Vec<String> {
    let usable: Vec<String> = references
        .iter()
        .filter(|id| usable_id(id))
        .map(|id| id.to_owned())
        .collect();
    if usable.len() <= MAX_REFERENCES {
        return usable;
    }
    let mut capped = Vec::with_capacity(MAX_REFERENCES);
    capped.push(usable[0].clone());
    capped.extend_from_slice(&usable[usable.len() - (MAX_REFERENCES - 1)..]);
    capped
}

/// Assert RFC 5322 §2.1.1's line-length limit over a finished message.
fn check_line_lengths(message: &[u8]) -> Result<(), Error> {
    for (index, line) in message.split(|&b| b == b'\n').enumerate() {
        let len = line.strip_suffix(b"\r").unwrap_or(line).len();
        if len > MAX_LINE {
            return Err(Error::internal(format!(
                "rendered line {} is {len} octets, over RFC 5322's {MAX_LINE}-octet limit",
                index + 1
            )));
        }
    }
    Ok(())
}

/// Lowercase hex, for boundaries and message-ids.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests;
