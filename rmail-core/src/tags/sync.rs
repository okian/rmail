//! Tag ⇄ IMAP keyword/Gmail-label round-trip, and the `auto` downgrade
//! (prd.md III-4, "IMAP/Gmail Interop & Edge Cases").
//!
//! # Grouping: one coalesced `STORE` per mailbox, not one per message
//!
//! [`MailboxGroup`]/[`group_by_mailbox`] turn a batch of message ids into
//! the fewest possible `STORE` calls: every message that shares a
//! `(mailbox, uidvalidity)` is folded into one group, and
//! [`apply_wire`]/[`ImapMutator::store_keyword`] issues exactly one `STORE`
//! per group over a compact UID set. This is what makes `BulkTag` "a single
//! transaction and one coalesced STORE" rather than N round trips — see
//! `super::TagStore::bulk_tag`, which is the one caller that can hand this
//! more than a single message.
//!
//! # `local` / `imap` / `auto`
//!
//! - `local`: [`apply_wire`] returns [`WireOutcome::Skipped`] without
//!   touching IMAP at all.
//! - `imap`: a `STORE` failure propagates as `Err` — nothing local should
//!   change either (see [`super::TagStore`]'s "IMAP first" ordering, the
//!   same rule [`crate::mail::MailStore`] follows).
//! - `auto`: a `STORE` failure is caught and reported as
//!   [`WireOutcome::Downgrade`] rather than propagated. The caller
//!   ([`super::TagStore`]) persists `sync_mode = local` on the tag and
//!   still applies the change locally — prd.md: "`auto` attempts imap then
//!   downgrades to local on `NO`/unsupported" / "persistent failure →
//!   auto-downgrade + warn". This build downgrades on the *first* failure
//!   rather than after a backoff-retry window: `tags` has no durable retry
//!   queue of its own (unlike, say, `crate::ai::queue`), and a tag that
//!   silently stops trying to reach a server that just refused it once is a
//!   safer default than one that keeps hammering a mailbox that will never
//!   accept custom keywords.

use std::collections::BTreeMap;

use crate::config::TagsImap;
use crate::error::Error;
use crate::imap::mutate::ImapMutator;
use crate::repo;
use crate::storage::Database;

use super::model::Tag;

/// One mailbox's worth of UIDs a coalesced `STORE` should target, resolved
/// from a batch of message ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MailboxGroup {
    pub(crate) account_id: i64,
    pub(crate) mailbox_name: String,
    pub(crate) uidvalidity: i64,
    pub(crate) uids: Vec<i64>,
}

/// What attempting a tag's IMAP round-trip produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WireOutcome {
    /// `sync_mode = local` — nothing was attempted.
    Skipped,
    /// Every group's `STORE` succeeded (or there was nothing to store —
    /// `groups` was empty).
    Applied,
    /// `sync_mode = auto` and a `STORE` failed. The caller must persist
    /// `sync_mode = local` on this tag and proceed as if it had been
    /// [`WireOutcome::Skipped`] all along.
    Downgrade,
}

/// Resolve the `(account_id, mailbox_name, uidvalidity)` groups a batch of
/// message ids falls into, coalescing UIDs that share a group. A message id
/// that no longer resolves (deleted between selection and this call) is
/// simply omitted, matching [`crate::repo::get_messages`]'s "absent id"
/// contract rather than failing the whole batch over one race.
///
/// # Errors
/// A mapped storage error.
pub(crate) async fn group_by_mailbox(
    db: &Database,
    message_ids: &[i64],
) -> Result<Vec<MailboxGroup>, Error> {
    let ids = message_ids.to_vec();
    let rows = db
        .read(move |conn| {
            let mut out = Vec::with_capacity(ids.len());
            for id in &ids {
                let Some(message) = repo::get_message(conn, *id)? else {
                    continue;
                };
                let Some(mailbox) = repo::get_mailbox(conn, message.mailbox_id)? else {
                    continue;
                };
                out.push((
                    message.account_id,
                    message.mailbox_id,
                    mailbox.name,
                    message.uidvalidity,
                    message.uid,
                ));
            }
            Ok(out)
        })
        .await?;

    // Keyed by mailbox id (not name): two different mailboxes could in
    // principle share a display name across accounts, and mailbox id is
    // the identity `messages.mailbox_id` already carries.
    let mut groups: BTreeMap<(i64, i64, i64), MailboxGroup> = BTreeMap::new();
    for (account_id, mailbox_id, mailbox_name, uidvalidity, uid) in rows {
        groups
            .entry((account_id, mailbox_id, uidvalidity))
            .or_insert_with(|| MailboxGroup {
                account_id,
                mailbox_name,
                uidvalidity,
                uids: Vec::new(),
            })
            .uids
            .push(uid);
    }
    Ok(groups.into_values().collect())
}

/// Attempt `tag`'s IMAP round-trip over every group in `groups`, add or
/// remove per `add`. See the module docs for the exact `local`/`imap`/`auto`
/// contract.
///
/// # Errors
/// The IMAP mutator's error, when `tag.sync_mode = TagSyncMode::Imap` and a
/// `STORE` fails (an `auto` tag's failure is reported as
/// [`WireOutcome::Downgrade`] instead — see the module docs).
pub(crate) async fn apply_wire(
    imap: &dyn ImapMutator,
    tag: &Tag,
    imap_config: &TagsImap,
    groups: &[MailboxGroup],
    add: bool,
) -> Result<WireOutcome, Error> {
    use crate::config::TagSyncMode;

    if tag.sync_mode == TagSyncMode::Local {
        return Ok(WireOutcome::Skipped);
    }
    let keyword = tag.wire_keyword(&imap_config.keyword_prefix);
    for group in groups {
        let result = imap
            .store_keyword(
                group.account_id,
                &group.mailbox_name,
                group.uidvalidity,
                &group.uids,
                &keyword,
                imap_config.gmail_labels,
                add,
            )
            .await;
        if let Err(error) = result {
            // Only `Imap`/`Auto` can reach here -- `Local` already returned
            // above -- so this is a plain two-way branch, not a third
            // (unreachable) match arm over the full enum.
            if tag.sync_mode == TagSyncMode::Imap {
                return Err(error);
            }
            // `tag_id` only, never the tag's name: a user-authored tag name
            // is mailbox content the same way a filter *value* is (see
            // `retrieve::lexical::operator_kind`'s identical reasoning for
            // why this codebase keeps query/filter values out of traces).
            tracing::warn!(
                tag_id = tag.id,
                mailbox = %group.mailbox_name,
                %error,
                "IMAP keyword STORE refused; downgrading this tag to sync_mode=local"
            );
            return Ok(WireOutcome::Downgrade);
        }
    }
    Ok(WireOutcome::Applied)
}

#[cfg(test)]
mod tests;
