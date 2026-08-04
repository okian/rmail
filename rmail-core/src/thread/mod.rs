//! Threading: grouping messages into conversations.
//!
//! Two signals decide which conversation a message belongs to, in priority
//! order:
//!
//! 1. **The reference graph** — `Message-ID`, `In-Reply-To` and `References`.
//!    Every id a message mentions is registered in `thread_refs` against the
//!    thread, *including ids whose message has not been fetched* ("phantoms").
//!    Phantom registration is what makes out-of-order arrival stable: a reply
//!    seen before its parent registers the parent's id, so when the parent
//!    lands it joins the existing thread rather than starting a new one.
//!
//! 2. **Normalized subject** — the fallback for mail whose references were
//!    stripped in transit. Its guards are deliberately tight, because a loose
//!    subject rule is how unrelated mail sharing a generic subject ("Invoice",
//!    "Hello") collapses into one endless conversation. It applies only when
//!    the message carries a genuine *reply* prefix (`Re:`, not `Fwd:` — a
//!    forward is a new conversation, often to a new audience) or references
//!    that matched nothing, and only within [`SUBJECT_FALLBACK_WINDOW_SECS`]
//!    of the candidate thread's **first** message. Anchoring on the first
//!    message is what bounds a thread's span: `last_message_at` advances with
//!    every arrival, so a window measured against it never closes.
//!
//! **Thread id stability.** A thread id is stable for its members: joining,
//! re-threading, and re-fetching never move a message to a fresh id. The one
//! id-changing event is a **merge** — when a late message proves two threads
//! were always one conversation, the lower (older) id absorbs the higher and
//! the absorbed id ceases to exist. Merges are reported in
//! [`ThreadAssignment::merged`] so callers holding an id can follow it.
//!
//! Thread aggregates (`message_count`, `last_message_at`, `first_message_at`,
//! `root_message_id`, `subject_norm`, `participants`) are **recomputed from the
//! thread's messages** rather than updated incrementally, so they self-heal
//! after merges, re-threading, and message removal. A thread holds tens of
//! messages, so the recompute is cheap relative to the fetch that triggered it.

use std::collections::BTreeSet;

use rusqlite::{named_params, Connection, OptionalExtension};

use crate::repo::{self, NewThread};

/// How far a message may sit from a candidate thread's **first** message for
/// the subject fallback to join them (30 days). Reference-based linking ignores
/// this window.
pub const SUBJECT_FALLBACK_WINDOW_SECS: i64 = 30 * 24 * 60 * 60;

/// Upper bound on prefix stripping, so a pathological `Re: Re: Re: …` subject
/// cannot spin.
const MAX_PREFIX_STRIPS: usize = 16;

/// Reply prefixes across the common locales. Only these arm the subject
/// fallback: a reply continues a conversation.
const REPLY_PREFIXES: &[&str] = &[
    "re", "aw", "sv", "antw", "antwoord", "odp", "ynt", "rif", "res",
];

/// Forward prefixes. These are stripped for normalization but do **not** arm
/// the subject fallback: a forward starts a new conversation, frequently with a
/// different audience, and joining it to the original would leak an outsider
/// into the thread's participant set.
const FORWARD_PREFIXES: &[&str] = &["fw", "fwd", "wg", "tr", "rv", "vs", "enc", "doorst"];

/// What kind of prefix a subject carried, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubjectPrefix {
    /// No reply or forward prefix — a fresh subject.
    None,
    /// A reply prefix (`Re:`, `Aw:`, …): this message continues a conversation.
    Reply,
    /// A forward prefix (`Fwd:`, `Wg:`, …): a new conversation.
    Forward,
}

/// How a message was joined to its thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadLink {
    /// Matched an existing thread through the reference graph.
    References,
    /// Matched an existing thread by normalized subject inside the window.
    Subject,
    /// Nothing matched; it kept the thread it was already in (a re-thread of a
    /// message that carries no usable ids of its own).
    Existing,
    /// Started a new thread.
    New,
}

/// The outcome of threading one message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadAssignment {
    /// The thread the message now belongs to.
    pub thread_id: i64,
    /// Which signal placed it there.
    pub link: ThreadLink,
    /// Threads that ceased to exist as a result of this assignment: absorbed
    /// because this message proved they were the same conversation, or emptied
    /// when this message left as their last member. Either way the id is gone
    /// and anything holding it should follow it to [`Self::thread_id`].
    pub merged: Vec<i64>,
}

/// The threading-relevant projection of a message row.
struct ThreadingRow {
    id: i64,
    account_id: i64,
    thread_id: Option<i64>,
    message_id: Option<String>,
    in_reply_to: Option<String>,
    references_hdr: Option<String>,
    subject: Option<String>,
    /// `COALESCE(date, internaldate)` — the sort key used everywhere.
    sort_at: Option<i64>,
}

/// A thread member's contribution to the thread's aggregates.
struct MemberRow {
    id: i64,
    subject: Option<String>,
    from_addr: Option<String>,
    to_addrs: Option<String>,
    cc_addrs: Option<String>,
    sort_at: Option<i64>,
}

/// Place a persisted message into a thread, creating, joining, or merging
/// threads as its references require, and refresh the thread's aggregates.
///
/// Idempotent: re-running for the same message resolves to the same thread.
/// Returns `None` if no message with `message_row_id` exists.
///
/// Call this inside the same transaction that inserted the message, so a
/// message is never visible without a thread.
///
/// # Errors
/// Propagates any `rusqlite` error.
#[tracing::instrument(
    skip(conn),
    fields(
        message = message_row_id,
        thread_id = tracing::field::Empty,
        link = tracing::field::Empty,
        merged = tracing::field::Empty,
    )
)]
pub fn assign_thread(
    conn: &Connection,
    message_row_id: i64,
) -> rusqlite::Result<Option<ThreadAssignment>> {
    let Some(row) = load_threading_row(conn, message_row_id)? else {
        return Ok(None);
    };

    let own_id = row
        .message_id
        .as_deref()
        .map(normalize_message_id)
        .filter(|id| !id.is_empty());
    let parent_ids = parent_reference_ids(&row);
    let (subject_norm, prefix) = normalize_subject(row.subject.as_deref());

    // 1. Reference graph — the strong signal, and the only one that may merge
    //    threads, because it is the only one that *proves* a relationship.
    let mut candidates = BTreeSet::new();
    for id in &parent_ids {
        if let Some(thread_id) = lookup_ref(conn, row.account_id, id)? {
            candidates.insert(thread_id);
        }
    }
    // Our own id resolves to one of two very different things: a phantom a
    // child already registered (real evidence — that child's thread is ours),
    // or the thread we are already in (a self-registration, no evidence at
    // all). Treating a self-registration as evidence would merge our current
    // thread into wherever a re-thread moves us, dragging its unrelated
    // members along.
    if let Some(own) = &own_id {
        if let Some(thread_id) = lookup_ref(conn, row.account_id, own)? {
            if Some(thread_id) != row.thread_id {
                candidates.insert(thread_id);
            }
        }
    }
    let mut link = ThreadLink::References;

    // 2. Subject fallback — for mail that presents as a reply, i.e. either a
    //    genuine `Re:`-style prefix or references that led nowhere. Only for a
    //    message that has no thread yet: this is the weakest signal, and
    //    re-running it on an already-threaded message would let a newer
    //    same-subject thread pull it out of the conversation it is in.
    if candidates.is_empty()
        && row.thread_id.is_none()
        && (prefix == SubjectPrefix::Reply || !parent_ids.is_empty())
        && !subject_norm.is_empty()
    {
        if let Some(thread_id) = match_by_subject(conn, row.account_id, &subject_norm, row.sort_at)?
        {
            candidates.insert(thread_id);
            link = ThreadLink::Subject;
        }
    }

    // 3. Last resort: the thread it is already in. Without this, re-threading a
    //    message that carries no ids of its own would mint a duplicate thread
    //    and orphan the old one. It is deliberately *not* a merge candidate —
    //    a message moving threads is no evidence that its old thread's other
    //    members belong to the new one.
    if candidates.is_empty() {
        if let Some(existing) = row.thread_id {
            candidates.insert(existing);
            link = ThreadLink::Existing;
        }
    }

    // 4. Resolve to a single thread. The lowest (oldest) id wins so an id a
    //    client already holds stays valid; the rest are absorbed into it.
    let mut merged = Vec::new();
    let thread_id = match candidates.first().copied() {
        None => {
            link = ThreadLink::New;
            repo::insert_thread(
                conn,
                &NewThread {
                    account_id: row.account_id,
                    subject_norm: (!subject_norm.is_empty()).then(|| subject_norm.clone()),
                    root_message_id: Some(row.id),
                    first_message_at: row.sort_at,
                    last_message_at: row.sort_at,
                },
            )?
        }
        Some(target) => {
            for other in candidates.iter().skip(1).copied() {
                merge_threads(conn, other, target)?;
                merged.push(other);
            }
            target
        }
    };

    // 5. Register the whole reference set — phantoms included — so a later
    //    message mentioning any of these ids lands in this same thread.
    for id in own_id.iter().chain(&parent_ids) {
        register_ref(conn, row.account_id, id, thread_id)?;
    }

    conn.execute(
        "UPDATE messages SET thread_id = ?1, updated_at = unixepoch() WHERE id = ?2",
        rusqlite::params![thread_id, row.id],
    )?;
    recompute_thread(conn, thread_id)?;

    // A re-thread that moved the message may have emptied its old thread. Fold
    // it into the thread that member joined rather than deleting it outright:
    // a plain DELETE would cascade away the phantom refs it still holds, and
    // those belong wherever their registrant went. Report it too — the id
    // ceases to exist, so a follower must be able to chase it.
    if let Some(previous) = row.thread_id {
        if previous != thread_id && !merged.contains(&previous) {
            recompute_thread(conn, previous)?;
            if thread_is_empty(conn, previous)? {
                merge_threads(conn, previous, thread_id)?;
                merged.push(previous);
            }
        }
    }

    let span = tracing::Span::current();
    span.record("thread_id", thread_id);
    span.record("link", tracing::field::debug(link));
    span.record("merged", merged.len());
    tracing::debug!(thread_id, ?link, merged = merged.len(), "threaded message");
    Ok(Some(ThreadAssignment {
        thread_id,
        link,
        merged,
    }))
}

/// Thread up to `limit` messages that have no thread yet, oldest first so
/// parents are threaded before their replies. Returns how many were threaded.
///
/// This is the repair path: messages stored before threading existed, or left
/// unthreaded by an interrupted sync, are picked up here rather than waiting
/// for a re-fetch. Safe to call repeatedly — it is a no-op once every message
/// has a thread.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub fn thread_unthreaded_messages(conn: &Connection, limit: i64) -> rusqlite::Result<usize> {
    let ids: Vec<i64> = {
        let mut stmt = conn.prepare(
            "SELECT id FROM messages WHERE thread_id IS NULL
             ORDER BY COALESCE(date, internaldate) ASC, id ASC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit.max(0)], |row| row.get::<_, i64>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    let mut threaded = 0;
    for id in ids {
        if assign_thread(conn, id)?.is_some() {
            threaded += 1;
        }
    }
    if threaded > 0 {
        tracing::info!(threaded, "backfilled threads for unthreaded messages");
    }
    Ok(threaded)
}

/// Recompute a thread's derived columns from its current members:
/// `message_count`, `first_message_at`, `last_message_at`, `root_message_id`,
/// `subject_norm`, and the `participants` set. Safe to call on a thread that
/// has lost all its messages (its aggregates simply zero out).
///
/// Note `message_count` counts message *rows*: an account that stores the same
/// mail in several folders (Gmail's All Mail, say) counts each copy.
///
/// # Errors
/// Propagates any `rusqlite` error.
pub fn recompute_thread(conn: &Connection, thread_id: i64) -> rusqlite::Result<()> {
    let members = load_members(conn, thread_id)?;

    let first_message_at = members.iter().filter_map(|m| m.sort_at).min();
    let last_message_at = members.iter().filter_map(|m| m.sort_at).max();
    // The root is the earliest message; undated mail sorts last, and ties break
    // on the stable id so the root does not flap.
    let root = members
        .iter()
        .min_by_key(|m| (m.sort_at.unwrap_or(i64::MAX), m.id));
    let subject_norm = root.and_then(|m| {
        let (norm, _) = normalize_subject(m.subject.as_deref());
        (!norm.is_empty()).then_some(norm)
    });

    let mut addresses = BTreeSet::new();
    for member in &members {
        for field in [&member.from_addr, &member.to_addrs, &member.cc_addrs] {
            for address in field.as_deref().unwrap_or_default().split(',') {
                let address = address.trim();
                if !address.is_empty() {
                    addresses.insert(address.to_lowercase());
                }
            }
        }
    }
    let participants =
        (!addresses.is_empty()).then(|| addresses.into_iter().collect::<Vec<_>>().join(","));

    conn.execute(
        "UPDATE threads SET
             message_count    = :message_count,
             first_message_at = :first_message_at,
             last_message_at  = :last_message_at,
             root_message_id  = :root_message_id,
             subject_norm     = :subject_norm,
             participants     = :participants,
             updated_at       = unixepoch()
         WHERE id = :id",
        named_params! {
            ":message_count": i64::try_from(members.len()).unwrap_or(i64::MAX),
            ":first_message_at": first_message_at,
            ":last_message_at": last_message_at,
            ":root_message_id": root.map(|m| m.id),
            ":subject_norm": subject_norm,
            ":participants": participants,
            ":id": thread_id,
        },
    )?;
    Ok(())
}

/// Normalize a subject for fallback grouping: strip reply/forward prefixes and
/// leading list tags, collapse whitespace, lowercase.
///
/// Returns the normalized text and the kind of the *outermost* prefix — that
/// kind is what distinguishes "a reply whose references were stripped" (join
/// the conversation) from a forward or from two unrelated mails that happen to
/// share a subject (do not).
#[must_use]
pub fn normalize_subject(subject: Option<&str>) -> (String, SubjectPrefix) {
    let mut current = subject.unwrap_or_default().trim();
    let mut prefix = SubjectPrefix::None;

    for _ in 0..MAX_PREFIX_STRIPS {
        if let Some((rest, kind)) = strip_reply_prefix(current) {
            current = rest.trim_start();
            if prefix == SubjectPrefix::None {
                prefix = kind;
            }
        } else if let Some(rest) = strip_list_tag(current) {
            current = rest.trim_start();
        } else {
            break;
        }
    }

    // Trailing "(fwd)" — the other half of the forward convention.
    if let Some(rest) = strip_suffix_ignore_ascii_case(current.trim_end(), "(fwd)") {
        current = rest;
        if prefix == SubjectPrefix::None {
            prefix = SubjectPrefix::Forward;
        }
    }

    let normalized = current.split_whitespace().collect::<Vec<_>>().join(" ");
    (normalized.to_lowercase(), prefix)
}

/// Strip one leading `Re:`/`Fwd:`-style prefix (with an optional `[2]`/`(2)`
/// counter), returning the remainder and which kind it was.
fn strip_reply_prefix(subject: &str) -> Option<(&str, SubjectPrefix)> {
    let s = subject.trim_start();
    let alpha_len = s
        .char_indices()
        .find(|(_, c)| !c.is_alphabetic())
        .map_or(s.len(), |(i, _)| i);
    let alpha = s.get(..alpha_len)?.to_lowercase();
    let kind = if REPLY_PREFIXES.contains(&alpha.as_str()) {
        SubjectPrefix::Reply
    } else if FORWARD_PREFIXES.contains(&alpha.as_str()) {
        SubjectPrefix::Forward
    } else {
        return None;
    };
    let rest = s.get(alpha_len..)?.trim_start();
    let rest = strip_counter(rest).unwrap_or(rest).trim_start();
    rest.strip_prefix(':').map(|rest| (rest, kind))
}

/// Strip a `[2]`/`(2)` reply counter, returning the remainder.
fn strip_counter(s: &str) -> Option<&str> {
    for (open, close) in [('[', ']'), ('(', ')')] {
        let Some(rest) = s.strip_prefix(open) else {
            continue;
        };
        let digits = rest
            .char_indices()
            .find(|(_, c)| !c.is_ascii_digit())
            .map_or(rest.len(), |(i, _)| i);
        if digits > 0 {
            if let Some(after) = rest.get(digits..).and_then(|r| r.strip_prefix(close)) {
                return Some(after);
            }
        }
    }
    None
}

/// Strip one leading `[mailing-list]` tag, returning the remainder.
fn strip_list_tag(subject: &str) -> Option<&str> {
    let rest = subject.trim_start().strip_prefix('[')?;
    let close = rest.find(']')?;
    rest.get(close + 1..)
}

/// `str::strip_suffix`, case-insensitively over an ASCII needle.
fn strip_suffix_ignore_ascii_case<'a>(haystack: &'a str, needle: &str) -> Option<&'a str> {
    let split = haystack.len().checked_sub(needle.len())?;
    let head = haystack.get(..split)?;
    let tail = haystack.get(split..)?;
    tail.eq_ignore_ascii_case(needle).then_some(head)
}

/// The message-ids this message names as ancestors (`In-Reply-To` then
/// `References`), deduplicated in first-seen order. Both headers are stored
/// space-joined by the parser.
fn parent_reference_ids(row: &ThreadingRow) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut ids = Vec::new();
    for source in [row.in_reply_to.as_deref(), row.references_hdr.as_deref()] {
        for raw in source.unwrap_or_default().split_whitespace() {
            let id = normalize_message_id(raw);
            if !id.is_empty() && seen.insert(id.clone()) {
                ids.push(id);
            }
        }
    }
    ids
}

/// Strip the angle brackets around a `Message-ID` header value. Case is
/// preserved: RFC5322 message-ids are case-sensitive.
fn normalize_message_id(raw: &str) -> String {
    raw.trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim()
        .to_owned()
}

fn load_threading_row(conn: &Connection, id: i64) -> rusqlite::Result<Option<ThreadingRow>> {
    conn.query_row(
        "SELECT id, account_id, thread_id, message_id, in_reply_to, references_hdr, subject,
                COALESCE(date, internaldate) AS sort_at
         FROM messages WHERE id = ?1",
        [id],
        |row| {
            Ok(ThreadingRow {
                id: row.get("id")?,
                account_id: row.get("account_id")?,
                thread_id: row.get("thread_id")?,
                message_id: row.get("message_id")?,
                in_reply_to: row.get("in_reply_to")?,
                references_hdr: row.get("references_hdr")?,
                subject: row.get("subject")?,
                sort_at: row.get("sort_at")?,
            })
        },
    )
    .optional()
}

fn load_members(conn: &Connection, thread_id: i64) -> rusqlite::Result<Vec<MemberRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, subject, from_addr, to_addrs, cc_addrs,
                COALESCE(date, internaldate) AS sort_at
         FROM messages WHERE thread_id = ?1",
    )?;
    let rows = stmt.query_map([thread_id], |row| {
        Ok(MemberRow {
            id: row.get("id")?,
            subject: row.get("subject")?,
            from_addr: row.get("from_addr")?,
            to_addrs: row.get("to_addrs")?,
            cc_addrs: row.get("cc_addrs")?,
            sort_at: row.get("sort_at")?,
        })
    })?;
    rows.collect()
}

fn lookup_ref(
    conn: &Connection,
    account_id: i64,
    message_id: &str,
) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT thread_id FROM thread_refs WHERE account_id = ?1 AND message_id = ?2",
        rusqlite::params![account_id, message_id],
        |row| row.get(0),
    )
    .optional()
}

fn register_ref(
    conn: &Connection,
    account_id: i64,
    message_id: &str,
    thread_id: i64,
) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO thread_refs (account_id, message_id, thread_id) VALUES (?1, ?2, ?3)
         ON CONFLICT(account_id, message_id) DO UPDATE SET thread_id = excluded.thread_id",
        rusqlite::params![account_id, message_id, thread_id],
    )?;
    Ok(())
}

/// Find the most recent thread in the account with this normalized subject
/// whose **first** message is within [`SUBJECT_FALLBACK_WINDOW_SECS`] of
/// `sort_at`, so a subject-linked conversation cannot drift indefinitely. Ties
/// break on id so threading is reproducible. A message with no usable timestamp
/// matches on subject alone.
fn match_by_subject(
    conn: &Connection,
    account_id: i64,
    subject_norm: &str,
    sort_at: Option<i64>,
) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT id FROM threads
         WHERE account_id = :account_id
           AND subject_norm = :subject_norm
           AND (:at IS NULL OR first_message_at IS NULL
                OR ABS(:at - first_message_at) <= :window)
         ORDER BY last_message_at DESC, id DESC
         LIMIT 1",
        named_params! {
            ":account_id": account_id,
            ":subject_norm": subject_norm,
            ":at": sort_at,
            ":window": SUBJECT_FALLBACK_WINDOW_SECS,
        },
        |row| row.get(0),
    )
    .optional()
}

/// Whether a thread has no messages left.
fn thread_is_empty(conn: &Connection, thread_id: i64) -> rusqlite::Result<bool> {
    conn.query_row(
        "SELECT NOT EXISTS(SELECT 1 FROM messages WHERE thread_id = ?1)",
        [thread_id],
        |row| row.get(0),
    )
}

/// Absorb thread `from` into thread `into`: repoint its messages and refs, then
/// drop the emptied thread. Aggregates are refreshed by the caller.
fn merge_threads(conn: &Connection, from: i64, into: i64) -> rusqlite::Result<()> {
    if from == into {
        return Ok(());
    }
    conn.execute(
        "UPDATE messages SET thread_id = ?1, updated_at = unixepoch() WHERE thread_id = ?2",
        rusqlite::params![into, from],
    )?;
    // `thread_refs` is keyed on (account_id, message_id), so repointing
    // thread_id can never collide.
    conn.execute(
        "UPDATE thread_refs SET thread_id = ?1 WHERE thread_id = ?2",
        rusqlite::params![into, from],
    )?;
    conn.execute("DELETE FROM threads WHERE id = ?1", [from])?;
    tracing::debug!(from, into, "merged threads");
    Ok(())
}

#[cfg(test)]
mod tests;
