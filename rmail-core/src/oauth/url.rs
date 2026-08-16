//! The two pieces of URL handling this module needs, written out rather than
//! taken as a dependency.
//!
//! A percent encoder for query values and a decoder for the ones the provider
//! hands back on the loopback redirect. This is ~40 lines against a crate that
//! would be linked into the daemon purely to build one URL and read one query
//! string, and getting either half wrong is loud rather than subtle: an
//! unencoded `scope` is rejected by the authorization endpoint, and a
//! non-decoded `code` (Google's contain `/`, which arrives as `%2F`) is
//! rejected by the token endpoint.

/// The RFC 3986 unreserved set — everything else in a query value is escaped.
///
/// Deliberately *not* the looser `application/x-www-form-urlencoded` set:
/// escaping more than strictly necessary is always accepted, while leaving a
/// `+` or a `/` bare in a `redirect_uri` or a `scope` is not.
fn is_unreserved(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~')
}

/// Percent-encode one query-string value.
pub(super) fn encode_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.as_bytes() {
        if is_unreserved(*byte) {
            out.push(char::from(*byte));
        } else {
            out.push('%');
            out.push_str(&format!("{byte:02X}"));
        }
    }
    out
}

/// Build a `k=v&k=v` query string with every value encoded.
pub(super) fn encode_query(params: &[(&str, &str)]) -> String {
    params
        .iter()
        .map(|(k, v)| format!("{}={}", encode_value(k), encode_value(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Percent-decode one query-string value, treating `+` as a space.
///
/// Lossy on invalid UTF-8 rather than failing: the alternative is refusing an
/// authorization because a provider's `error_description` contained a stray
/// byte, and this decoder's output is compared against a known `state` or
/// posted straight back to the provider, both of which fail safely on garbage.
fn decode_value(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                // Sliced out of `bytes`, never out of `value`. Indexing the
                // `&str` here would panic whenever `i + 3` lands inside a
                // multi-byte character — which any local process can arrange,
                // because `read_request_line` builds this string with
                // `from_utf8_lossy` and every invalid byte after a `%` becomes
                // a three-byte U+FFFD. A byte slice has no such hazard.
                match std::str::from_utf8(&bytes[i + 1..i + 3])
                    .ok()
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                {
                    Some(decoded) => {
                        out.push(decoded);
                        i += 3;
                    }
                    // A stray `%` that is not an escape stays a `%`.
                    None => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Split a query string into decoded `(key, value)` pairs.
pub(super) fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| match pair.split_once('=') {
            Some((k, v)) => (decode_value(k), decode_value(v)),
            None => (decode_value(pair), String::new()),
        })
        .collect()
}
