//! Opaque, scope-bound page tokens for the list RPCs.
//!
//! prd.md's pagination rule is two sentences — "page_size (server caps 500)
//! plus an opaque page_token" — and both halves are load-bearing in a way that
//! is easy to under-build.
//!
//! # Why keyset, not offset
//!
//! A mailbox is not a static array. Mail arrives while a client pages through
//! it, and `LIMIT ? OFFSET ?` against a newest-first order shifts every row
//! down by one for each arrival: page 2 re-shows the tail of page 1, and a
//! deletion silently skips a message instead. A page token here therefore
//! carries a **position in the sort order** — the `(sort_key, id)` pair of the
//! last row of the page just served — and the next page is everything strictly
//! after it. Rows that arrive mid-pagination land *above* the cursor and are
//! simply not in this pass; nothing is duplicated and nothing is skipped.
//!
//! The `id` half is not decoration. Every list here orders by a timestamp, and
//! timestamps tie — a bulk import gives a hundred messages the same
//! `internaldate`. Without a tiebreak in both the `ORDER BY` and the cursor,
//! a page boundary that lands inside a tie group loses or repeats the rest of
//! the group depending on which way SQLite happened to break it.
//!
//! # Why the token is bound to its query
//!
//! A page token is **caller-supplied input**, not a cursor the server can
//! trust because it minted one that looked like it. Treated as a bare
//! position, a token minted for `mailbox_id = 3` and replayed against
//! `mailbox_id = 9` reads mailbox 9 from an offset chosen elsewhere — and,
//! worse, a token minted while filtering to one account resumes into another
//! account's rows if the request's `account_id` changes underneath it.
//!
//! So every token commits to the query it belongs to: [`PageScope`] hashes the
//! method plus **every** request field that selects rows, and [`decode`]
//! recomputes that hash from the request in hand and refuses a token that does
//! not match. Resuming across an account, a mailbox, or a state filter is a
//! rejected token rather than a subtly wrong read.
//!
//! ## What the binding is, and is not
//!
//! It is an *integrity* check on the pairing of token to request, not a
//! secret. There is deliberately no server-side MAC key, because a key would
//! defend against a threat that does not exist here: the digest is recomputed
//! from the current request, so a forged token can only ever resume a query the
//! caller could have issued from scratch — the same method, the same account,
//! the same mailbox, all of which the auth layer has already authorized by the
//! time [`decode`] runs. A MAC would buy nothing and would cost every
//! in-flight token at each daemon restart.
//!
//! What the binding *does* buy is that a token cannot be re-aimed. That is the
//! actual failure mode: a client (or an agent driving one) that keeps a token
//! and reuses it against the next request it builds.
//!
//! # Opacity
//!
//! Clients must not parse or construct these. The encoding is versioned so the
//! shape can change without a proto change, and a token from a future version
//! is rejected rather than misread.

use std::fmt::Display;

use base64::engine::general_purpose::URL_SAFE_NO_PAD as BASE64;
use base64::Engine as _;
use sha2::{Digest, Sha256};

use crate::error::Error;

#[cfg(test)]
mod tests;

/// The hard ceiling on a page, whatever the caller asks for — prd.md's
/// "server caps 500".
///
/// A cap rather than a rejection: a client that asks for 10 000 rows gets 500
/// and a token, which is the behaviour it wanted anyway. Rejecting would turn
/// an over-eager default in some SDK into an outage.
pub const MAX_PAGE_SIZE: i64 = 500;

/// The gRPC response-header key carrying the next page token of a
/// **server-streamed** list.
///
/// `MailService.List` streams `Message` frames, and a stream has no response
/// envelope to hang a `next_page_token` field on. The token rides in the
/// call's initial metadata instead, which works precisely because that handler
/// materializes its (capped) page before the first frame goes out — the token
/// is known at header time. Absent means this was the last page.
///
/// A constant because it is a wire contract: a client branches on it.
pub const NEXT_PAGE_TOKEN_METADATA_KEY: &str = "x-rmail-next-page-token";

/// Token encoding version. Bumped if the byte layout below ever changes, so an
/// old token is rejected rather than decoded as something it is not.
const VERSION: u8 = 1;

/// Bytes of [`PageScope`]'s digest kept in the token.
///
/// Truncated from SHA-256's 32: this is a mismatch check between a token and
/// the request carrying it, not a signature, and 64 bits is far past the point
/// where an accidental collision between two of a client's own live queries is
/// plausible.
const DIGEST_BYTES: usize = 8;

/// `version | digest | sort | id`.
const TOKEN_BYTES: usize = 1 + DIGEST_BYTES + 8 + 8;

/// A position in a list's total order: the sort key of the last row served,
/// plus its `id` to break ties.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    /// The list's ordering column for that row (a unix timestamp, in every
    /// list that exists today).
    pub sort: i64,
    /// That row's primary key.
    pub id: i64,
}

impl Cursor {
    /// A cursor at `(sort, id)`.
    #[must_use]
    pub const fn new(sort: i64, id: i64) -> Self {
        Self { sort, id }
    }
}

/// Everything a page token is allowed to resume against.
///
/// Build one from the method and **every** request field that changes which
/// rows are selected — see the module docs. A field left out is a field a
/// token can be re-aimed across.
#[derive(Debug, Clone)]
pub struct PageScope {
    hasher: Sha256,
}

impl PageScope {
    /// Start a scope for a fully-qualified gRPC method
    /// (`rmail.v1.MailService/List`).
    #[must_use]
    pub fn new(method: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(b"rmail.page.v1");
        let mut scope = Self { hasher };
        scope.write(b"method", method.as_bytes());
        scope
    }

    /// Bind one request field, by name and value.
    ///
    /// Named as well as valued so two fields cannot swap without changing the
    /// digest — `account_id = 3, mailbox_id = 9` and `account_id = 9,
    /// mailbox_id = 3` are different queries and must be different scopes.
    #[must_use]
    pub fn field(mut self, name: &str, value: impl Display) -> Self {
        self.write(name.as_bytes(), value.to_string().as_bytes());
        self
    }

    /// Bind an optional field. `None` is distinct from any present value —
    /// "every account" is a different query from "account 0".
    #[must_use]
    pub fn opt_field(mut self, name: &str, value: Option<impl Display>) -> Self {
        match value {
            Some(value) => self.write(name.as_bytes(), value.to_string().as_bytes()),
            None => self.write(name.as_bytes(), b"\x00none"),
        }
        self
    }

    /// Length-prefixed so `("ab", "c")` and `("a", "bc")` cannot hash alike.
    fn write(&mut self, name: &[u8], value: &[u8]) {
        self.hasher.update((name.len() as u64).to_be_bytes());
        self.hasher.update(name);
        self.hasher.update((value.len() as u64).to_be_bytes());
        self.hasher.update(value);
    }

    fn digest(&self) -> [u8; DIGEST_BYTES] {
        let full = self.hasher.clone().finalize();
        let mut out = [0u8; DIGEST_BYTES];
        out.copy_from_slice(&full[..DIGEST_BYTES]);
        out
    }
}

/// Clamp a caller's `page_size` to `1..=MAX_PAGE_SIZE`, with `0` (and any
/// negative, which the proto's `int32` allows) meaning `default`.
///
/// `default` is itself clamped, so a caller cannot be handed more than
/// [`MAX_PAGE_SIZE`] by a mis-set server default either.
#[must_use]
pub fn clamp_page_size(requested: i64, default: i64) -> i64 {
    let size = if requested <= 0 { default } else { requested };
    size.clamp(1, MAX_PAGE_SIZE)
}

/// Encode a cursor as an opaque token bound to `scope`.
#[must_use]
pub fn encode(scope: &PageScope, cursor: Cursor) -> String {
    let mut bytes = Vec::with_capacity(TOKEN_BYTES);
    bytes.push(VERSION);
    bytes.extend_from_slice(&scope.digest());
    bytes.extend_from_slice(&cursor.sort.to_be_bytes());
    bytes.extend_from_slice(&cursor.id.to_be_bytes());
    BASE64.encode(bytes)
}

/// Decode a caller-supplied token, refusing anything not minted for `scope`.
///
/// An empty token means "start at the beginning" and yields `None`, so a
/// handler can pass `request.page_token` through unconditionally.
///
/// # Errors
///
/// [`Error::InvalidArgument`] if the token is malformed, from another encoding
/// version, or bound to a different query. All three say the same thing to the
/// client — the token is unusable *here* — and deliberately do not say which,
/// since the difference is only ever interesting to someone probing the
/// format.
pub fn decode(token: &str, scope: &PageScope) -> Result<Option<Cursor>, Error> {
    if token.is_empty() {
        return Ok(None);
    }
    let bytes = BASE64.decode(token).map_err(|_| bad_token())?;
    if bytes.len() != TOKEN_BYTES || bytes[0] != VERSION {
        return Err(bad_token());
    }
    if bytes[1..1 + DIGEST_BYTES] != scope.digest() {
        tracing::debug!("rejected a page token minted for a different query");
        return Err(bad_token());
    }

    let mut sort = [0u8; 8];
    sort.copy_from_slice(&bytes[1 + DIGEST_BYTES..1 + DIGEST_BYTES + 8]);
    let mut id = [0u8; 8];
    id.copy_from_slice(&bytes[1 + DIGEST_BYTES + 8..]);
    Ok(Some(Cursor {
        sort: i64::from_be_bytes(sort),
        id: i64::from_be_bytes(id),
    }))
}

/// The token for the page *after* one that returned `rows` items against a
/// requested `page_size`, or `None` when the list is exhausted.
///
/// Callers ask for `page_size + 1` rows and hand the extra one in as
/// `overflow`: a short page is a definitive end-of-list, whereas emitting a
/// token whenever a page came back full costs the client one extra round trip
/// on every list that happens to divide evenly.
#[must_use]
pub fn next_token(scope: &PageScope, last: Option<Cursor>, overflow: bool) -> Option<String> {
    match (overflow, last) {
        (true, Some(cursor)) => Some(encode(scope, cursor)),
        _ => None,
    }
}

fn bad_token() -> Error {
    Error::invalid_argument(
        "page_token is not valid for this request; it is opaque and may only be replayed \
         against the query that produced it",
    )
}
