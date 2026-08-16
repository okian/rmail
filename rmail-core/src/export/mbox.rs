//! mbox framing, in the **mboxrd** variant.
//!
//! # Why mboxrd and not mboxo
//!
//! mbox separates messages with a line starting `From ` at column zero, which
//! means a body line that happens to start `From ` has to be escaped or the
//! archive silently splits one message into two. The original (`mboxo`)
//! escape prefixes `>` to any line matching `^From ` — and is *not*
//! reversible, because a body line that already read `>From the desk of…`
//! comes back indistinguishable from an escaped `From the desk of…`.
//!
//! mboxrd escapes any line matching `^>*From ` instead. Now the number of
//! leading `>` characters is itself the information: strip exactly one from
//! any line matching `^>+From ` on read and the original bytes come back,
//! whatever they were. That reversibility is the whole reason this module
//! exists rather than a `write_all(raw)`, and `export::tests` proves it by
//! reversing the transform and asserting byte equality.
//!
//! # Exactly one added newline, and how it comes back off
//!
//! [`frame`] writes, per message: the `From_` line, the escaped body, then a
//! single `\n`. That trailing byte is the separator, and it is unconditional
//! — *not* "a newline if the body lacks one, plus a blank line". The
//! unconditional form is what makes the inverse exact: strip one trailing
//! `\n` and you are back to the original body, whether it ended with a
//! newline or not. The conditional form loses that, because `body` and
//! `body\n` would both frame to `body\n\n`.
//!
//! A message that ends with CRLF (every message an IMAP server delivers)
//! therefore produces the customary blank line between entries. Line endings
//! inside the body are never touched: an archive that rewrote CRLF to LF
//! would change every message's bytes, and with them every signature over
//! them.

use chrono::{DateTime, TimeZone, Utc};

use crate::repo;

/// The envelope sender used when a message row carries no `From` address.
///
/// The conventional mbox placeholder; a `From_` line is required to have
/// *something* there, and inventing a plausible address would be worse than
/// naming the one address that is by convention nobody's.
const UNKNOWN_SENDER: &str = "MAILER-DAEMON";

/// Frame one message: `From_` line, mboxrd-escaped raw, trailing separator.
#[must_use]
pub fn frame(message: &repo::Message, raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len() + 96);
    out.extend_from_slice(from_line(message).as_bytes());
    out.push(b'\n');
    escape_into(raw, &mut out);
    out.push(b'\n');
    out
}

/// The `From ` separator line, without its newline.
///
/// `From <envelope-sender> <asctime>`, where the timestamp is the message's
/// own `Date` if it has one, otherwise the server's `INTERNALDATE`, otherwise
/// the Unix epoch. The address is sanitized to a single token: a `From_` line
/// is delimited by spaces, so an address containing one (or a newline) would
/// corrupt the framing of every reader that splits on it.
fn from_line(message: &repo::Message) -> String {
    let sender = message
        .from_addr
        .as_deref()
        .map(sanitize_sender)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| UNKNOWN_SENDER.to_owned());
    let stamp = message
        .date
        .or(message.internaldate)
        .and_then(|secs| Utc.timestamp_opt(secs, 0).single())
        .unwrap_or(DateTime::UNIX_EPOCH);
    // asctime, the format every mbox reader since Version 7 Unix expects:
    // `Thu Jan  1 00:00:00 1970` — day-of-month space-padded, not
    // zero-padded, which is what `%e` gives.
    format!("From {sender} {}", stamp.format("%a %b %e %H:%M:%S %Y"))
}

/// Collapse an address to something that cannot break `From_` framing:
/// no whitespace, no control characters.
fn sanitize_sender(address: &str) -> String {
    address
        .chars()
        .filter(|ch| !ch.is_whitespace() && !ch.is_control())
        .collect()
}

/// Apply mboxrd escaping: prefix `>` to every line matching `^>*From `.
fn escape_into(raw: &[u8], out: &mut Vec<u8>) {
    for line in split_lines(raw) {
        if needs_escape(line) {
            out.push(b'>');
        }
        out.extend_from_slice(line);
    }
}

/// Split on `\n`, keeping the terminator on each piece so the original bytes
/// are reproduced exactly by concatenation (including a final line with no
/// terminator, and CR bytes belonging to CRLF endings).
fn split_lines(raw: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = raw;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let end = rest
            .iter()
            .position(|&b| b == b'\n')
            .map_or(rest.len(), |idx| idx + 1);
        let (line, tail) = rest.split_at(end);
        rest = tail;
        Some(line)
    })
}

/// Whether a line matches `^>*From ` and must therefore be quoted.
fn needs_escape(line: &[u8]) -> bool {
    let body = line.iter().position(|&b| b != b'>').unwrap_or(line.len());
    line[body..].starts_with(b"From ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The documented inverse of [`escape_into`], written from the mboxrd
    /// rule rather than by inverting this file's code, so a bug in the
    /// escaper cannot cancel out against a matching bug here.
    fn unescape(framed: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(framed.len());
        for line in split_lines(framed) {
            // Strip exactly one `>` from a line whose leading run of `>` is
            // non-empty and is followed by `From `.
            let depth = line.iter().position(|&b| b != b'>').unwrap_or(line.len());
            if depth > 0 && line[depth..].starts_with(b"From ") {
                out.extend_from_slice(&line[1..]);
            } else {
                out.extend_from_slice(line);
            }
        }
        out
    }

    #[test]
    fn escaping_is_reversible_for_every_depth_of_quoting() {
        for body in [
            &b"From the desk of Ada\r\n"[..],
            &b">From the desk of Ada\r\n"[..],
            &b">>>From the desk of Ada\r\n"[..],
            &b"Subject: x\r\n\r\nFrom here on\r\nFrom there on\r\n"[..],
            // Not a separator: no trailing space after `From`.
            &b"Fromage is cheese\r\n"[..],
            // No trailing newline at all.
            &b"From "[..],
        ] {
            let mut escaped = Vec::new();
            escape_into(body, &mut escaped);
            assert_eq!(unescape(&escaped), body, "round trip failed for {body:?}");
        }
    }

    #[test]
    fn an_escaped_body_has_no_bare_from_line() {
        let mut escaped = Vec::new();
        escape_into(b"header\r\n\r\nFrom nowhere\r\n", &mut escaped);
        assert!(
            !escaped.windows(6).any(|w| w == b"\nFrom "),
            "an unescaped `From ` line survived: {escaped:?}"
        );
    }

    #[test]
    fn a_sender_with_spaces_cannot_break_the_separator_line() {
        let message = message_with(Some("ada lovelace@example.com"), Some(0));
        let line = from_line(&message);
        assert_eq!(line.split(' ').next(), Some("From"));
        assert_eq!(line.split(' ').nth(1), Some("adalovelace@example.com"));
    }

    #[test]
    fn a_message_with_no_sender_uses_the_conventional_placeholder() {
        let line = from_line(&message_with(None, Some(0)));
        assert!(line.starts_with("From MAILER-DAEMON "), "{line}");
    }

    #[test]
    fn the_separator_timestamp_is_asctime() {
        let line = from_line(&message_with(Some("a@b.c"), Some(0)));
        assert_eq!(line, "From a@b.c Thu Jan  1 00:00:00 1970");
    }

    fn message_with(from: Option<&str>, date: Option<i64>) -> repo::Message {
        repo::Message {
            id: 1,
            account_id: 1,
            mailbox_id: 1,
            uid: 1,
            uidvalidity: 1,
            message_id: None,
            thread_id: None,
            in_reply_to: None,
            references_hdr: None,
            subject: None,
            from_addr: from.map(str::to_owned),
            from_name: None,
            to_addrs: None,
            cc_addrs: None,
            date,
            internaldate: None,
            size: None,
            raw: None,
            body_text: None,
            body_html: None,
            has_attachments: false,
            created_at: 0,
            updated_at: 0,
        }
    }
}
