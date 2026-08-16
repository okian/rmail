//! Maildir naming: which file inside the tree a message becomes, and how its
//! IMAP flags survive the trip.
//!
//! # Everything lands in `cur/`, never `new/`
//!
//! Maildir splits delivered mail three ways: `tmp/` for a delivery in
//! progress, `new/` for mail no client has looked at, `cur/` for everything
//! else. Only `cur/` filenames may carry the `:2,<flags>` info suffix — a
//! `new/` filename with one is malformed, and readers are entitled to ignore
//! or reject it.
//!
//! An export is not a delivery: every message here has already been delivered
//! to an IMAP server, synced, and (usually) read, and it arrives carrying
//! flags that are the whole reason an archive is worth keeping. Writing
//! unseen mail into `new/` would trade those flags for a distinction that
//! `:2,S`'s *absence* already records — so `cur/` it is, with `S` present or
//! absent saying exactly what `new/` would have said, and `R`/`F`/`D`/`T`
//! saying four more things `new/` could not have.
//!
//! `tmp/` is still created by the writer, empty. A Maildir without it is not
//! a Maildir, and the next tool to deliver into this directory needs it.
//!
//! # Filenames are unique by construction, not by hope
//!
//! The classical unique part of a Maildir name is a delivery-time
//! pid/hostname/counter dance whose only job is to avoid a collision between
//! concurrent deliveries. An export has something better: `messages.id` is a
//! primary key, so `<delivery-seconds>.<id>.rmail` cannot collide with
//! another message in the same export, and re-running an export overwrites
//! each message's own file rather than accumulating duplicates of it.

use crate::repo;

/// Maildir info-suffix flag characters, in the ASCII order the spec requires,
/// paired with the IMAP system flag each is written from.
///
/// `P` ("passed", i.e. resent/forwarded) has no IMAP system-flag equivalent
/// and is therefore never emitted — inferring it from anything else would be
/// inventing a fact about the message.
const FLAG_MAP: &[(char, &str)] = &[
    ('D', "\\draft"),
    ('F', "\\flagged"),
    ('R', "\\answered"),
    ('S', "\\seen"),
    ('T', "\\deleted"),
];

/// The path this message takes inside a Maildir export, relative to its root.
#[must_use]
pub fn entry_path(message: &repo::Message, flags: &[String]) -> String {
    format!("cur/{}", entry_name(message, flags))
}

/// The bare filename, without the `cur/` prefix.
#[must_use]
pub fn entry_name(message: &repo::Message, flags: &[String]) -> String {
    let secs = message
        .internaldate
        .or(message.date)
        .unwrap_or(message.created_at)
        .max(0);
    format!("{secs}.{}.rmail:2,{}", message.id, info_flags(flags))
}

/// The `:2,` info suffix's flag characters for a set of IMAP flags.
///
/// Matching is case-insensitive because IMAP system flags are
/// case-insensitive atoms — a server answering `\SEEN` means the same thing
/// as one answering `\Seen`, and an archive that dropped a read receipt over
/// letter case would be wrong in a way nobody would notice until they cared.
#[must_use]
pub fn info_flags(flags: &[String]) -> String {
    let lowered: Vec<String> = flags.iter().map(|flag| flag.to_lowercase()).collect();
    FLAG_MAP
        .iter()
        .filter(|(_, imap)| lowered.iter().any(|flag| flag == imap))
        .map(|(ch, _)| *ch)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(id: i64, internaldate: Option<i64>) -> repo::Message {
        repo::Message {
            id,
            account_id: 1,
            mailbox_id: 1,
            uid: 1,
            uidvalidity: 1,
            message_id: None,
            thread_id: None,
            in_reply_to: None,
            references_hdr: None,
            subject: None,
            from_addr: None,
            from_name: None,
            to_addrs: None,
            cc_addrs: None,
            date: None,
            internaldate,
            size: None,
            raw: None,
            body_text: None,
            body_html: None,
            has_attachments: false,
            created_at: 99,
            updated_at: 0,
        }
    }

    #[test]
    fn flags_are_emitted_in_ascii_order_regardless_of_input_order() {
        let flags = vec![
            "\\Seen".to_owned(),
            "\\Draft".to_owned(),
            "\\Answered".to_owned(),
        ];
        assert_eq!(info_flags(&flags), "DRS");
    }

    #[test]
    fn imap_flag_case_does_not_change_the_suffix() {
        assert_eq!(info_flags(&["\\SEEN".to_owned()]), "S");
        assert_eq!(info_flags(&["\\seen".to_owned()]), "S");
    }

    #[test]
    fn a_keyword_that_is_not_a_system_flag_is_ignored() {
        assert_eq!(info_flags(&["rmail/project".to_owned()]), "");
    }

    #[test]
    fn the_name_carries_the_message_id_so_two_messages_cannot_collide() {
        let a = entry_name(&message(1, Some(10)), &[]);
        let b = entry_name(&message(2, Some(10)), &[]);
        assert_ne!(a, b);
        assert_eq!(a, "10.1.rmail:2,");
    }

    #[test]
    fn a_message_with_no_dates_falls_back_to_its_row_creation_time() {
        assert_eq!(entry_name(&message(7, None), &[]), "99.7.rmail:2,");
    }

    #[test]
    fn the_path_is_relative_and_under_cur() {
        let path = entry_path(&message(1, Some(0)), &["\\Seen".to_owned()]);
        assert_eq!(path, "cur/0.1.rmail:2,S");
    }
}
