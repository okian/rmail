//! Delta sync: CONDSTORE / QRESYNC, with a UID-enumeration fallback.
//!
//! The initial walk ([`crate::sync::full`]) answers *"what do I not have?"*.
//! That question is blind to everything a mailbox does after the download: a
//! `\Seen` flag flipped on another device, a message expunged from the phone.
//! Both leave the UID set unchanged, so the walk sees nothing to do and the
//! local copy silently rots.
//!
//! This module answers the other question — *"what changed?"* — and picks the
//! cheapest way the server can answer it:
//!
//! | [`DeltaStrategy`] | requires | round trips |
//! |---|---|---|
//! | [`Qresync`](DeltaStrategy::Qresync) | `QRESYNC` + a stored modseq | one `UID FETCH … (CHANGEDSINCE n VANISHED)` — changes *and* expunges |
//! | [`Condstore`](DeltaStrategy::Condstore) | `CONDSTORE` + a stored modseq | that `FETCH` (no `VANISHED`) plus a `UID SEARCH ALL` to find expunges |
//! | [`UidDiff`](DeltaStrategy::UidDiff) | nothing | `UID SEARCH ALL` plus a flag sweep of the covered range |
//! | [`Full`](DeltaStrategy::Full) | nothing | hands back to the UID-window walk |
//!
//! **The modseq is a checkpoint, not a reading.** `HIGHESTMODSEQ` is only
//! written back once every change the server reported has been applied
//! locally. A run that is cancelled, interrupted, or that could not make sense
//! of an answer leaves the stored modseq where it was, so the next run asks the
//! same question again — re-applying a change is free (flag writes are
//! idempotent, message inserts are keyed on IMAP identity), whereas advancing
//! past an unapplied change loses it forever. The high-water UID mark is held
//! back for exactly the same reason.
//!
//! **Scope.** A delta run asks about the whole UID space (`1:*`) but only
//! downloads bodies the initial walk has already claimed as covered — see
//! [`Coverage`]. Otherwise a flag change on an ancient message would drag its
//! body in ahead of a backlog walk that is still working downward, defeating
//! the newest-first ordering that makes a mailbox useful early.
//!
//! **What cannot be deltaed.** Without a baseline in the same UID space —
//! a first sync, or a server that re-keyed `UIDVALIDITY` — there is no "since"
//! to ask about. Those cases purge what is stale and delegate to the initial
//! walk rather than pretending a delta happened.
//!
//! **Deleting needs more evidence than fetching.** A wrong fetch costs
//! bandwidth; a wrong expunge destroys mail. So the enumeration that drives
//! deletion on the non-QRESYNC paths is cross-checked against the `EXISTS`
//! count the `SELECT` just reported, and an implausible answer is refused
//! rather than acted on.
//!
//! # Known bounds
//!
//! - **Cancellation is observed between round trips**, not inside them, so a
//!   cancelled run can still take up to [`SyncOptions::window_timeout`] to
//!   return. This matches [`crate::sync::full`] and for the same reason:
//!   abandoning a command mid-flight leaves the IMAP session unusable, and the
//!   session outlives the folder.
//! - **The fallback path holds one `(uid, flags)` tuple per stored message in
//!   memory** while the network side stays windowed. Bounded by folder size,
//!   not mailbox size, and only on servers without CONDSTORE.
//! - **`VANISHED` overflow is possible but not silent in effect.** async-imap's
//!   unsolicited channel is bounded (100) and drops on overflow. Every loop
//!   that issues round trips drains it, which is what keeps it from filling; a
//!   notice missed anyway leaves a message locally that the server has deleted
//!   — visible, and corrected by the next run that takes an enumerating
//!   strategy. The reverse (deleting something still on the server) cannot
//!   happen this way.
//! - **A tagged `NO` on `UID FETCH` reaches us as an empty result, not an
//!   error.** async-imap's fetch stream stops at the tagged response without
//!   inspecting its status, so `NO [LIMIT]` and "nothing changed" are
//!   indistinguishable at the API. The QRESYNC probe detects the resulting
//!   self-contradiction (modseq moved, nothing reported) and holds the
//!   checkpoint; the CONDSTORE and fallback paths are covered by their
//!   enumeration. A refused *body* fetch still reads as an empty window — the
//!   UIDs simply stay missing and a later run re-requests them, since the walk
//!   keys on stored UIDs rather than on that response.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::RangeInclusive;
use std::time::Duration;

use async_imap::types::UnsolicitedResponse;
use async_imap::Session;
use futures::StreamExt;
use rusqlite::OptionalExtension;
use tokio_util::sync::CancellationToken;

use crate::error::Error;
use crate::imap::conn::ImapStream;
use crate::imap::ImapCapabilities;
use crate::message::fetch::{fetch_and_persist, flag_to_string};
use crate::repo;
use crate::storage::Database;

use super::full::{self, format_uid_set, SyncOptions};
use super::{Change, ChangeSink};

/// The IMAP attributes a change probe fetches: identity, the flag set, and the
/// modseq that made it change. Deliberately no `BODY[]` — the point of a delta
/// is that a flag flip costs bytes, not megabytes.
const CHANGE_QUERY: &str = "(UID FLAGS MODSEQ)";

/// The same probe for a server with no modseq at all. `MODSEQ` is a CONDSTORE
/// data item: asking a server that never advertised the extension for it earns
/// a tagged `BAD`, not an empty answer.
const SWEEP_QUERY: &str = "(UID FLAGS)";

/// How the server was asked what changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeltaStrategy {
    /// `UID FETCH … (CHANGEDSINCE n VANISHED)` — RFC 7162. Changes and
    /// expunges arrive in one round trip; nothing has to be enumerated.
    Qresync,
    /// `UID FETCH … (CHANGEDSINCE n)` — RFC 7162 without the `VANISHED`
    /// modifier, so expunges still need a `UID SEARCH ALL` to spot.
    Condstore,
    /// Neither extension is usable (unsupported, or no modseq stored yet): the
    /// server's live UID set is enumerated and diffed against the local one.
    UidDiff,
    /// No baseline to delta from — a folder never synced, or one whose
    /// `UIDVALIDITY` the server re-keyed. The initial UID-window walk runs
    /// instead and the folder is delta-syncable from the next run on.
    Full,
}

impl DeltaStrategy {
    /// The strategy name, for logs and reports.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Qresync => "qresync",
            Self::Condstore => "condstore",
            Self::UidDiff => "uiddiff",
            Self::Full => "full",
        }
    }
}

/// The outcome of one folder's delta sync.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeltaReport {
    /// The mailbox synced.
    pub mailbox_id: i64,
    /// How the server was asked.
    pub strategy: DeltaStrategy,
    /// The UIDVALIDITY the run was keyed to.
    pub uidvalidity: i64,
    /// The modseq checkpoint after the run — `None` when the server reports no
    /// modseq, or when the run did not finish and so did not advance it.
    pub highestmodseq: Option<i64>,
    /// Messages newly downloaded (new arrivals, and gaps inside the covered
    /// range).
    pub new_messages: u64,
    /// Messages whose stored flag set the server contradicted.
    pub flag_updates: u64,
    /// Messages removed locally because the server no longer has them.
    pub expunged: u64,
    /// Rows dropped because the server re-keyed the UID space.
    pub purged_stale: u64,
    /// Whether the folder was rebuilt from scratch (first sync, or a
    /// `UIDVALIDITY` bump).
    pub resynced: bool,
    /// Whether the server's modseq already matched the checkpoint, so the run
    /// asked for nothing at all.
    pub unchanged: bool,
    /// Whether the run stopped early because it was cancelled. A cancelled run
    /// does not advance the checkpoint.
    pub cancelled: bool,
}

/// One folder's failure inside an account-wide delta sync.
#[derive(Debug)]
pub struct DeltaFailure {
    /// The mailbox that failed.
    pub mailbox_id: i64,
    /// Its name, for logs and messages.
    pub name: String,
    /// Why it failed.
    pub error: Error,
}

/// The outcome of delta-syncing every folder of an account.
#[derive(Debug, Default)]
pub struct AccountDeltaReport {
    /// Folders that synced, in the order they were visited.
    pub reports: Vec<DeltaReport>,
    /// Folders that failed. One bad folder does not stop the others.
    pub failures: Vec<DeltaFailure>,
}

/// The part of the UID space the initial walk has claimed.
///
/// A delta probe returns changes across the whole folder, including messages
/// far below where a partially-finished backlog walk has reached. Downloading
/// those here would jump the queue and undo the newest-first ordering, so a
/// changed UID only earns a body fetch when it is new mail (above the high
/// mark) or a hole inside the range the walk already covered.
#[derive(Debug, Clone, Copy)]
struct Coverage {
    /// Highest UID the walk has covered; everything above it is new mail.
    high_water: i64,
    /// Lowest UID the walk has reached, or `None` if it never started.
    low_water: Option<i64>,
}

impl Coverage {
    fn covers(&self, uid: i64) -> bool {
        uid > self.high_water || self.low_water.is_some_and(|low| uid >= low)
    }
}

/// What a change probe learned.
#[derive(Debug, Default)]
struct Changes {
    /// `(uid, flags)` for every message the server says changed.
    changed: Vec<(i64, Vec<String>)>,
    /// UID ranges reported `VANISHED`. Kept as ranges, never expanded: a server
    /// may legitimately answer `1:4294967295` for a folder it emptied, and
    /// materializing four billion integers to answer "did I have any of these?"
    /// would be a self-inflicted outage.
    vanished: Vec<RangeInclusive<i64>>,
    /// Whether the probe stopped before it had seen everything. A partial
    /// answer may be applied but must never be checkpointed as a complete one.
    cancelled: bool,
    /// Whether the server said something the probe could not use — a `FETCH`
    /// item with no UID, say. Same consequence as [`Self::cancelled`]: apply
    /// what is understood, but do not claim the folder is caught up.
    incomplete: bool,
}

impl Changes {
    fn is_vanished(&self, uid: i64) -> bool {
        self.vanished.iter().any(|range| range.contains(&uid))
    }
}

/// Bring one folder up to date with the server, transferring only what changed.
///
/// Picks the cheapest [`DeltaStrategy`] the server and the stored checkpoint
/// allow, applies flag changes and expunges, downloads new mail, and advances
/// the checkpoint only if every change was applied. `cancel` is observed
/// between round trips; a cancelled run returns its partial report with
/// [`DeltaReport::cancelled`] set rather than an error, and leaves the
/// checkpoint where it was so the next run re-asks.
///
/// `capabilities` states what the run may use on *this session*, not merely
/// what the server advertised. Pass [`ImapCapabilities::without_modseq`] to
/// honor `sync.qresync = false` and force the enumeration diff. Setting
/// `qresync` means QRESYNC has been enabled on the session — call
/// [`enable_qresync`] once after login, or use [`delta_sync_folders`], which
/// does it for you.
///
/// # Errors
///
/// - [`Error::NotFound`] if `mailbox_id` does not exist or the server cannot
///   select the folder.
/// - [`Error::Unavailable`] if the server does not report
///   `UIDVALIDITY`/`UIDNEXT`, or the connection breaks mid-run.
/// - [`Error::DeadlineExceeded`] if a round trip exceeds
///   [`SyncOptions::window_timeout`]. The session is then mid-command and must
///   be dropped.
/// - A mapped storage error if persistence fails.
#[tracing::instrument(
    skip(session, db, capabilities, opts, cancel, sink),
    fields(folder, strategy, uidvalidity)
)]
pub async fn delta_sync<T: ImapStream>(
    session: &mut Session<T>,
    db: &Database,
    mailbox_id: i64,
    capabilities: ImapCapabilities,
    opts: SyncOptions,
    cancel: &CancellationToken,
    sink: &mut impl ChangeSink,
) -> Result<DeltaReport, Error> {
    let mailbox = db
        .read(move |c| repo::get_mailbox(c, mailbox_id))
        .await?
        .ok_or_else(|| Error::not_found(format!("mailbox {mailbox_id} not found")))?;
    let account_id = mailbox.account_id;
    tracing::Span::current().record("folder", tracing::field::display(&mailbox.name));

    let qresync = capabilities.qresync;
    let modseq_capable = qresync || capabilities.condstore;

    let selected = if modseq_capable {
        session.select_condstore(&mailbox.name).await
    } else {
        session.select(&mailbox.name).await
    }
    .map_err(|e| super::select_error(&mailbox.name, e))?;

    // Unsolicited responses are session-scoped, not folder-scoped, and once
    // QRESYNC is enabled a server reports expunges as VANISHED at any moment it
    // likes — including while the previous folder was selected. Anything left
    // in the channel from before this SELECT refers to a different mailbox's
    // UID space, where the same numbers mean different messages, so it is
    // dropped rather than applied.
    let stale = session.unsolicited_responses.clone();
    let discarded = drain_unsolicited(|| stale.try_recv().ok(), &mut Vec::new());
    if discarded > 0 {
        tracing::debug!(
            discarded,
            "dropped unsolicited responses belonging to the previously selected folder"
        );
    }

    let uidvalidity = i64::from(
        selected
            .uid_validity
            .ok_or_else(|| Error::unavailable("server did not report UIDVALIDITY on SELECT"))?,
    );
    let uidnext = i64::from(
        selected
            .uid_next
            .ok_or_else(|| Error::unavailable("server did not report UIDNEXT on SELECT"))?,
    );
    let ceiling = uidnext - 1;
    // A modseq wider than i64 cannot round-trip through SQLite's INTEGER, and a
    // truncated checkpoint would silently re-ask or skip. Treating it as absent
    // downgrades to the enumeration diff, which is correct without it.
    let server_modseq = selected
        .highest_modseq
        .and_then(|modseq| i64::try_from(modseq).ok());
    tracing::Span::current().record("uidvalidity", uidvalidity);

    db.write(move |c| {
        repo::update_mailbox_uid_state(c, mailbox_id, uidvalidity, uidnext)?;
        // Only when the SELECT asked for CONDSTORE. "We did not ask" is not
        // "the server has none", and clearing the mirror on a run that merely
        // had the extension switched off would erase server state this run
        // never enquired about.
        if modseq_capable {
            repo::update_mailbox_highestmodseq(c, mailbox_id, server_modseq)?;
        }
        Ok(())
    })
    .await?;

    let previous = db
        .read(move |c| repo::get_sync_state(c, mailbox_id))
        .await?;
    let stored_uidvalidity = previous.as_ref().and_then(|state| state.uidvalidity);
    let uidvalidity_changed = stored_uidvalidity.is_some_and(|stored| stored != uidvalidity);

    // No baseline in this UID space: there is no "since" to ask about, so hand
    // back to the walk that builds one.
    let baseline = previous.as_ref().filter(|_| !uidvalidity_changed);
    let Some(baseline) = baseline else {
        tracing::Span::current().record("strategy", DeltaStrategy::Full.as_str());
        return resync(
            db,
            session,
            mailbox_id,
            uidvalidity,
            uidvalidity_changed,
            server_modseq,
            opts,
            cancel,
            sink,
        )
        .await;
    };

    let coverage = Coverage {
        high_water: baseline.last_synced_uid.unwrap_or(0),
        low_water: baseline.walked_down_to,
    };
    // RFC 7162's ABNF requires a nonzero mod-sequence for CHANGEDSINCE, and
    // zero is the "no modseq assigned" sentinel besides. A stored zero is not a
    // baseline, it is the absence of one.
    let stored_modseq = baseline.highestmodseq.filter(|modseq| *modseq > 0);
    let server_modseq = server_modseq.filter(|modseq| *modseq > 0);
    let full_sync_done = baseline.full_sync_done;

    let strategy = match (modseq_capable, stored_modseq, server_modseq) {
        (true, Some(_), Some(_)) if qresync => DeltaStrategy::Qresync,
        (true, Some(_), Some(_)) => DeltaStrategy::Condstore,
        _ => DeltaStrategy::UidDiff,
    };
    tracing::Span::current().record("strategy", strategy.as_str());

    // A matching modseq means nothing at all happened in this folder — but only
    // QRESYNC promises that. RFC 7162 §3.1.2.2 requires expunges to bump
    // HIGHESTMODSEQ *for a QRESYNC server*; a CONDSTORE-only server may track
    // mod-sequences for flag changes alone, so an expunge with no other
    // activity would leave the modseq untouched. Taking this shortcut there
    // would hide that expunge on this run and on every run after it.
    if strategy == DeltaStrategy::Qresync && stored_modseq == server_modseq {
        touch_checkpoint(db, mailbox_id).await?;
        tracing::debug!(modseq = ?server_modseq, "folder unchanged since the last delta");
        return Ok(DeltaReport {
            mailbox_id,
            strategy,
            uidvalidity,
            highestmodseq: server_modseq,
            new_messages: 0,
            flag_updates: 0,
            expunged: 0,
            purged_stale: 0,
            resynced: false,
            unchanged: true,
            cancelled: false,
        });
    }

    if cancel.is_cancelled() {
        tracing::info!("delta sync cancelled before probing the server");
        touch_checkpoint(db, mailbox_id).await?;
        return Ok(cancelled_report(
            mailbox_id,
            strategy,
            uidvalidity,
            stored_modseq,
        ));
    }

    // ---- probe -----------------------------------------------------------
    let changes = if strategy == DeltaStrategy::UidDiff {
        // Without a modseq the server cannot say what changed, so the flag set
        // of everything already stored is re-read. It is header-only traffic,
        // and it is the only way a CONDSTORE-less server ever reports a flag
        // flip.
        sweep_flags(session, db, mailbox_id, uidvalidity, opts, cancel).await?
    } else {
        let since = stored_modseq.unwrap_or(0);
        let mut changes = fetch_changed_since(session, since, qresync, opts.window_timeout).await?;

        // A silent empty answer is indistinguishable from a refused one.
        // async-imap's fetch stream stops at the tagged response without
        // inspecting its status (`parse_fetches` in async-imap 0.10), so a
        // server answering `NO [LIMIT]` yields zero items and no error — which
        // would look exactly like "nothing changed" and let the checkpoint
        // skip the very changes the modseq says exist.
        //
        // Under QRESYNC the server has contradicted itself if that happens: it
        // moved HIGHESTMODSEQ, and every reason it could have done so shows up
        // either as a FETCH item or as VANISHED. Hold the checkpoint back. A
        // false positive costs one repeated (cheap) probe; a false negative
        // costs the change. CONDSTORE alone makes no such promise about
        // expunges, so it is left to its enumeration.
        if strategy == DeltaStrategy::Qresync
            && changes.changed.is_empty()
            && changes.vanished.is_empty()
        {
            tracing::warn!(
                stored_modseq,
                server_modseq,
                "server moved HIGHESTMODSEQ but reported no change and no \
                 expunge; treating the probe as incomplete"
            );
            changes.incomplete = true;
        }
        changes
    };

    if changes.cancelled {
        // The probe stopped early, so the run has stopped. Enumerating now
        // would be a fresh round trip on a run that is already over.
        tracing::info!("delta sync cancelled during the change probe");
        touch_checkpoint(db, mailbox_id).await?;
        return Ok(cancelled_report(
            mailbox_id,
            strategy,
            uidvalidity,
            stored_modseq,
        ));
    }

    // ---- expunges --------------------------------------------------------
    let local_uids: BTreeSet<i64> = db
        .read(move |c| repo::list_message_uids(c, mailbox_id, uidvalidity, 1, i64::MAX))
        .await?
        .into_iter()
        .collect();

    // QRESYNC's VANISHED (EARLIER) is exhaustive over the requested set, which
    // was the whole UID space, so it needs no enumeration. CONDSTORE alone says
    // what changed but never what left, and the fallback has neither — both
    // have to ask the server for its live UID set.
    let mut live = if strategy == DeltaStrategy::Qresync {
        None
    } else {
        Some(enumerate_uids(session, opts.window_timeout).await?)
    };

    // An enumeration is trusted to *delete*, so it has to be sane before it is
    // believed. `SELECT` just said how many messages the folder holds; an empty
    // answer to `UID SEARCH ALL` for a folder that is not empty means the
    // search failed or was answered for something else, and acting on it would
    // delete the entire folder and every thread in it. Refuse, and do not
    // checkpoint — the next run asks again.
    let mut enumeration_suspect = false;
    if let Some(found) = &live {
        if found.is_empty() && selected.exists > 0 {
            tracing::warn!(
                exists = selected.exists,
                "UID SEARCH returned nothing for a folder the server says is \
                 not empty; refusing to treat that as an expunge of everything"
            );
            enumeration_suspect = true;
            live = None;
        }
    }

    let gone: Vec<i64> = match (&live, strategy) {
        (Some(live), _) => local_uids
            .iter()
            .copied()
            .filter(|uid| !live.contains(uid))
            .collect(),
        (None, DeltaStrategy::Qresync) => local_uids
            .iter()
            .copied()
            .filter(|uid| changes.is_vanished(*uid))
            .collect(),
        // The enumeration was rejected: nothing else in this run can identify
        // an expunge, so none is applied.
        (None, _) => Vec::new(),
    };
    let expunged = expunge_local(db, mailbox_id, uidvalidity, &gone, sink).await?;
    if expunged > 0 {
        tracing::info!(expunged, "removed messages the server no longer has");
    }

    // ---- flags -----------------------------------------------------------
    let stored: BTreeMap<i64, i64> = load_uid_ids(db, mailbox_id, uidvalidity, &changes).await?;
    let updates: Vec<(i64, i64, Vec<String>)> = changes
        .changed
        .iter()
        .filter_map(|(uid, flags)| {
            stored
                .get(uid)
                .map(|message_id| (*message_id, *uid, flags.clone()))
        })
        .collect();
    let flag_updates = apply_flags(db, updates, sink).await?;
    if flag_updates > 0 {
        tracing::info!(flag_updates, "reconciled flags the server changed");
    }

    // ---- new mail --------------------------------------------------------
    // Two sources, because neither alone is complete. A CHANGEDSINCE probe
    // reports new arrivals (their modseq is above the checkpoint by
    // definition), but the fallback's flag sweep only ever looks at UIDs
    // already stored and so can never discover one. The enumeration, where a
    // strategy has it, sees every live UID including the ones we lack.
    let mut missing: BTreeSet<i64> = changes
        .changed
        .iter()
        .map(|(uid, _)| *uid)
        .filter(|uid| !stored.contains_key(uid))
        .collect();
    match &live {
        Some(live) => {
            missing.extend(live.iter().copied().filter(|uid| !local_uids.contains(uid)));
            // A message the probe reported and the server then expunged would
            // otherwise be downloaded straight back into the folder it just
            // left.
            missing.retain(|uid| live.contains(uid));
        }
        None => missing.retain(|uid| !changes.is_vanished(*uid)),
    }
    // Newest first, so a client watching this folder sees the arrivals before
    // the backfill — the same ordering rule the initial walk follows.
    let missing: Vec<i64> = missing
        .into_iter()
        .filter(|uid| coverage.covers(*uid))
        .collect();

    let mut new_messages = 0u64;
    // A probe that stopped early already disqualifies this run from advancing
    // the checkpoint, whatever the body fetches go on to do.
    let mut cancelled = changes.cancelled;
    let incomplete = enumeration_suspect || changes.incomplete;
    for window in chunks_newest_first(&missing, opts.effective_window()) {
        if cancel.is_cancelled() {
            tracing::info!("delta sync cancelled between fetch windows");
            cancelled = true;
            break;
        }
        let set = format_uid_set(&window);
        let outcomes = tokio::time::timeout(
            opts.window_timeout,
            fetch_and_persist(session, db, account_id, mailbox_id, uidvalidity, &set),
        )
        .await
        .map_err(|_| {
            Error::deadline_exceeded(format!(
                "IMAP fetch of UIDs {set} exceeded {:?}",
                opts.window_timeout
            ))
        })??;
        for outcome in &outcomes {
            if outcome.inserted {
                new_messages += 1;
                sink.changed(Change::Added {
                    message_id: outcome.message_id,
                    uid: outcome.uid,
                });
            }
        }
    }

    // ---- checkpoint ------------------------------------------------------
    // Only a run that applied everything may advance the marks. Anything else
    // must leave the next run asking the same question, because a change that
    // is skipped past is a change lost for good.
    let complete = !cancelled && !incomplete;
    let checkpoint_modseq = if complete {
        server_modseq
    } else {
        stored_modseq
    };
    let high_water = if complete {
        coverage.high_water.max(ceiling)
    } else {
        coverage.high_water
    };
    // Read-modify-write inside the write closure: the walk checkpoints the same
    // row, and a value carried across this run's awaits would clobber whatever
    // it wrote meanwhile. Only the fields this engine owns are overwritten.
    let now = chrono::Utc::now().timestamp();
    let low_water = coverage.low_water;
    db.write(move |c| {
        let current = repo::get_sync_state(c, mailbox_id)?;
        repo::upsert_sync_state(
            c,
            &repo::SyncState {
                mailbox_id,
                uidvalidity: Some(uidvalidity),
                highestmodseq: checkpoint_modseq,
                // Both marks are monotone — the high one only rises, the low one
                // only falls — so merging by min/max is safe under any
                // interleaving with the walk.
                last_synced_uid: Some(
                    current
                        .as_ref()
                        .and_then(|s| s.last_synced_uid)
                        .map_or(high_water, |stored| stored.max(high_water)),
                ),
                walked_down_to: match (current.as_ref().and_then(|s| s.walked_down_to), low_water) {
                    (Some(fresh), Some(seen)) => Some(fresh.min(seen)),
                    (fresh, seen) => fresh.or(seen),
                },
                last_sync_at: Some(now),
                full_sync_done: current.as_ref().is_some_and(|s| s.full_sync_done)
                    || full_sync_done,
            },
        )
    })
    .await?;

    tracing::info!(
        strategy = strategy.as_str(),
        new_messages,
        flag_updates,
        expunged,
        cancelled,
        "delta sync finished"
    );
    Ok(DeltaReport {
        mailbox_id,
        strategy,
        uidvalidity,
        highestmodseq: checkpoint_modseq,
        new_messages,
        flag_updates,
        expunged,
        purged_stale: 0,
        resynced: false,
        unchanged: false,
        cancelled,
    })
}

/// Delta-sync every selectable folder of an account over one session, in
/// [`full::prioritize`] order.
///
/// A folder that fails is recorded and the run continues, exactly as
/// [`full::sync_folders`] does: one unselectable mailbox must not stop every
/// other folder from ever catching up.
///
/// This is where [`enable_qresync`] belongs — it runs once, before the first
/// `SELECT`, which is the only state RFC 5161 allows it in.
///
/// # Errors
///
/// Only storage errors reading the folder list; per-folder failures are
/// collected into [`AccountDeltaReport::failures`].
#[tracing::instrument(skip(session, db, capabilities, opts, cancel, sink))]
pub async fn delta_sync_folders<T: ImapStream>(
    session: &mut Session<T>,
    db: &Database,
    account_id: i64,
    capabilities: ImapCapabilities,
    opts: SyncOptions,
    cancel: &CancellationToken,
    sink: &mut impl ChangeSink,
) -> Result<AccountDeltaReport, Error> {
    let mailboxes = db
        .read(move |c| repo::list_mailboxes(c, account_id))
        .await?;
    if cancel.is_cancelled() {
        return Ok(AccountDeltaReport::default());
    }
    let capabilities = enable_qresync(session, capabilities, opts.window_timeout).await;
    let mut out = AccountDeltaReport::default();
    for mailbox in full::prioritize(mailboxes) {
        if cancel.is_cancelled() {
            break;
        }
        match delta_sync(session, db, mailbox.id, capabilities, opts, cancel, sink).await {
            Ok(report) => out.reports.push(report),
            Err(error) => {
                tracing::warn!(folder = %mailbox.name, %error, "folder delta sync failed; continuing");
                out.failures.push(DeltaFailure {
                    mailbox_id: mailbox.id,
                    name: mailbox.name,
                    error,
                });
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Server probes
// ---------------------------------------------------------------------------

/// Enable QRESYNC on a freshly authenticated session, returning `capabilities`
/// adjusted to what the server actually granted.
///
/// **Call this once per session, before any folder is selected.** RFC 5161 §3.1
/// permits `ENABLE` only in the authenticated state with no mailbox selected,
/// so this cannot live inside [`delta_sync`] — the second folder of an
/// account-wide run would issue it from the selected state and a strict server
/// would answer `BAD`, silently downgrading every folder after the first.
/// [`delta_sync_folders`] does this for you.
///
/// A refusal is not an error: it costs one round trip and clears the `qresync`
/// bit, leaving CONDSTORE (or the enumeration diff) to do the work. Only a
/// broken connection is fatal, and the next command reports that anyway.
pub async fn enable_qresync<T: ImapStream>(
    session: &mut Session<T>,
    capabilities: ImapCapabilities,
    timeout: Duration,
) -> ImapCapabilities {
    if !capabilities.qresync {
        return capabilities;
    }
    let downgraded = ImapCapabilities {
        qresync: false,
        ..capabilities
    };
    // Bounded like every other round trip here. A server that completes the TCP
    // handshake and then stops answering would otherwise hang an account-wide
    // run before its first folder, indefinitely.
    match tokio::time::timeout(timeout, session.run_command_and_check_ok("ENABLE QRESYNC")).await {
        Ok(Ok(())) => capabilities,
        Ok(Err(error)) => {
            tracing::warn!(%error, "server advertised QRESYNC but refused ENABLE; using CONDSTORE");
            downgraded
        }
        Err(_) => {
            tracing::warn!(?timeout, "ENABLE QRESYNC timed out; using CONDSTORE");
            downgraded
        }
    }
}

/// Take everything `next` still has queued, appending any `VANISHED` UID ranges
/// to `out`, and report how many responses were consumed.
///
/// Takes a closure rather than the receiver so nothing here has to name
/// async-imap's channel type, which is an implementation detail of that crate
/// and not part of its API surface.
///
/// The channel is bounded and drops on overflow, so this is called from inside
/// the fetch loop as well as after it: a folder that lost more than a channel's
/// worth of messages must not lose the notice too.
fn drain_unsolicited(
    mut next: impl FnMut() -> Option<UnsolicitedResponse>,
    out: &mut Vec<RangeInclusive<i64>>,
) -> usize {
    let mut seen = 0usize;
    while let Some(response) = next() {
        collect_vanished(&response, out);
        seen += 1;
    }
    seen
}

/// Ask the server what changed since `since`, over the whole UID space.
///
/// With `vanished` set this is the QRESYNC form and expunges come back as
/// `VANISHED (EARLIER)` responses interleaved with the fetches.
async fn fetch_changed_since<T: ImapStream>(
    session: &mut Session<T>,
    since: i64,
    vanished: bool,
    timeout: Duration,
) -> Result<Changes, Error> {
    let query = if vanished {
        format!("{CHANGE_QUERY} (CHANGEDSINCE {since} VANISHED)")
    } else {
        format!("{CHANGE_QUERY} (CHANGEDSINCE {since})")
    };

    tokio::time::timeout(timeout, async {
        // VANISHED arrives on the session's unsolicited channel. Holding a
        // second handle lets it be drained *inside* the fetch loop, where the
        // stream itself has the session mutably borrowed.
        let unsolicited = session.unsolicited_responses.clone();
        let mut changes = Changes::default();
        let mut stream = session
            .uid_fetch("1:*", &query)
            .await
            .map_err(|e| super::command_error("UID FETCH", e))?;

        while let Some(item) = stream.next().await {
            let fetch = item.map_err(|e| super::command_error("UID FETCH", e))?;
            match fetch.uid {
                Some(uid) => changes.changed.push((
                    i64::from(uid),
                    fetch.flags().map(|f| flag_to_string(&f)).collect(),
                )),
                None => {
                    // Whatever this item carried is unaddressable, so it cannot
                    // be applied — and a checkpoint written over it would claim
                    // it had been.
                    tracing::warn!("CHANGEDSINCE item without a UID; holding the checkpoint back");
                    changes.incomplete = true;
                }
            }
            drop(fetch);
            drain_unsolicited(|| unsolicited.try_recv().ok(), &mut changes.vanished);
        }
        drop(stream);
        drain_unsolicited(|| unsolicited.try_recv().ok(), &mut changes.vanished);

        tracing::debug!(
            since,
            changed = changes.changed.len(),
            vanished_ranges = changes.vanished.len(),
            "change probe complete"
        );
        Ok(changes)
    })
    .await
    .map_err(|_| {
        Error::deadline_exceeded(format!("IMAP CHANGEDSINCE probe exceeded {timeout:?}"))
    })?
}

/// Re-read the flag set of every message already stored in this folder.
///
/// The CONDSTORE-less path: the server cannot name what changed, so the covered
/// UID range is swept in windows. Headers only — no body is transferred for a
/// message already held.
async fn sweep_flags<T: ImapStream>(
    session: &mut Session<T>,
    db: &Database,
    mailbox_id: i64,
    uidvalidity: i64,
    opts: SyncOptions,
    cancel: &CancellationToken,
) -> Result<Changes, Error> {
    let stored: Vec<i64> = db
        .read(move |c| repo::list_message_uids(c, mailbox_id, uidvalidity, 1, i64::MAX))
        .await?;

    let mut changes = Changes::default();
    for window in chunks_newest_first(&stored, opts.effective_window()) {
        if cancel.is_cancelled() {
            tracing::info!("flag sweep cancelled between windows");
            changes.cancelled = true;
            break;
        }
        let set = format_uid_set(&window);
        let swept = tokio::time::timeout(opts.window_timeout, async {
            let mut stream = session
                .uid_fetch(&set, SWEEP_QUERY)
                .await
                .map_err(|e| super::command_error("UID FETCH", e))?;
            let mut swept: Vec<(i64, Vec<String>)> = Vec::new();
            while let Some(item) = stream.next().await {
                let fetch = item.map_err(|e| super::command_error("UID FETCH", e))?;
                if let Some(uid) = fetch.uid {
                    swept.push((
                        i64::from(uid),
                        fetch.flags().map(|f| flag_to_string(&f)).collect(),
                    ));
                }
            }
            Ok::<_, Error>(swept)
        })
        .await
        .map_err(|_| {
            Error::deadline_exceeded(format!(
                "IMAP flag sweep of UIDs {set} exceeded {:?}",
                opts.window_timeout
            ))
        })??;
        changes.changed.extend(swept);
    }
    tracing::debug!(swept = changes.changed.len(), "flag sweep complete");
    Ok(changes)
}

/// The server's live UID set for the selected folder.
///
/// `UID SEARCH ALL` transfers identifiers only, so enumerating even a very large
/// folder costs one small round trip — cheap enough to be the expunge detector
/// for every server that cannot report `VANISHED`.
async fn enumerate_uids<T: ImapStream>(
    session: &mut Session<T>,
    timeout: Duration,
) -> Result<BTreeSet<i64>, Error> {
    let uids = tokio::time::timeout(timeout, session.uid_search("ALL"))
        .await
        .map_err(|_| Error::deadline_exceeded(format!("IMAP UID SEARCH exceeded {timeout:?}")))?
        .map_err(|e| super::command_error("UID SEARCH", e))?;
    Ok(uids.into_iter().map(i64::from).collect())
}

/// Pull `VANISHED` UID ranges out of an unsolicited response, ignoring
/// everything else the channel carries.
fn collect_vanished(response: &UnsolicitedResponse, out: &mut Vec<RangeInclusive<i64>>) {
    let UnsolicitedResponse::Other(data) = response else {
        return;
    };
    if let async_imap::imap_proto::Response::Vanished { uids, .. } = data.parsed() {
        out.extend(
            uids.iter()
                .map(|range| i64::from(*range.start())..=i64::from(*range.end())),
        );
    }
}

// ---------------------------------------------------------------------------
// Local application
// ---------------------------------------------------------------------------

/// Purge a superseded UID space (if any) and rebuild the folder with the
/// initial walk, then record the modseq so the next run can delta.
#[allow(clippy::too_many_arguments)]
async fn resync<T: ImapStream>(
    db: &Database,
    session: &mut Session<T>,
    mailbox_id: i64,
    uidvalidity: i64,
    uidvalidity_changed: bool,
    server_modseq: Option<i64>,
    opts: SyncOptions,
    cancel: &CancellationToken,
    sink: &mut impl ChangeSink,
) -> Result<DeltaReport, Error> {
    let purged_stale = if uidvalidity_changed {
        tracing::warn!(
            uidvalidity,
            "UIDVALIDITY changed; dropping the stale local copy and resyncing"
        );
        full::purge_other_uidvalidity(db, mailbox_id, uidvalidity, sink).await?
    } else {
        0
    };

    // Point the checkpoint at the new UID space with no marks, so the walk
    // starts from scratch instead of re-detecting the bump it just handled.
    db.write(move |c| {
        repo::upsert_sync_state(
            c,
            &repo::SyncState {
                mailbox_id,
                uidvalidity: Some(uidvalidity),
                ..Default::default()
            },
        )
    })
    .await?;

    let report = full::sync_folder(session, db, mailbox_id, opts, cancel, |_| {}, sink).await?;

    // A walk that did not reach the bottom of the UID space has not seen
    // everything the modseq covers, so it is not a baseline; leaving it unset
    // makes the next run enumerate rather than trust a mark it never earned.
    // `complete` is the field that actually says so — `cancelled` only happens
    // to imply it today.
    let checkpoint_modseq = if report.complete && !report.cancelled {
        server_modseq
    } else {
        None
    };
    if checkpoint_modseq.is_some() {
        db.write(move |c| {
            let mut state = repo::get_sync_state(c, mailbox_id)?.unwrap_or(repo::SyncState {
                mailbox_id,
                ..Default::default()
            });
            state.highestmodseq = checkpoint_modseq;
            repo::upsert_sync_state(c, &state)
        })
        .await?;
    }

    tracing::info!(
        purged_stale,
        fetched = report.fetched,
        "folder resynced from scratch"
    );
    Ok(DeltaReport {
        mailbox_id,
        strategy: DeltaStrategy::Full,
        uidvalidity,
        highestmodseq: checkpoint_modseq,
        new_messages: report.fetched,
        flag_updates: 0,
        expunged: 0,
        purged_stale,
        resynced: true,
        unchanged: false,
        cancelled: report.cancelled,
    })
}

/// Map the changed UIDs to the surrogate ids of the messages already stored.
///
/// Scoped to the changed range rather than the whole folder, and returning ids
/// only — the raw RFC822 blobs stay on disk.
async fn load_uid_ids(
    db: &Database,
    mailbox_id: i64,
    uidvalidity: i64,
    changes: &Changes,
) -> Result<BTreeMap<i64, i64>, Error> {
    let Some(low) = changes.changed.iter().map(|(uid, _)| *uid).min() else {
        return Ok(BTreeMap::new());
    };
    let high = changes
        .changed
        .iter()
        .map(|(uid, _)| *uid)
        .max()
        .unwrap_or(low);
    let pairs = db
        .read(move |c| repo::list_message_uid_ids(c, mailbox_id, uidvalidity, low, high))
        .await?;
    Ok(pairs.into_iter().collect())
}

/// Apply flag replacements in one transaction, returning how many messages
/// actually differed.
async fn apply_flags(
    db: &Database,
    updates: Vec<(i64, i64, Vec<String>)>,
    sink: &mut impl ChangeSink,
) -> Result<u64, Error> {
    if updates.is_empty() {
        return Ok(0);
    }
    // Report only what actually differed. A server re-stating a flag set it
    // already sent is not a change, and an event stream that said otherwise
    // would make every delta pass look like activity.
    let changed = db
        .write(move |conn| {
            let tx = conn.transaction()?;
            let mut changed = Vec::new();
            for (message_id, uid, flags) in updates {
                if repo::replace_flags(&tx, message_id, &flags)? {
                    changed.push((message_id, uid, flags));
                }
            }
            tx.commit()?;
            Ok(changed)
        })
        .await?;
    let count = changed.len() as u64;
    for (message_id, uid, flags) in changed {
        sink.changed(Change::FlagsChanged {
            message_id,
            uid,
            flags,
        });
    }
    Ok(count)
}

/// Delete the local rows for UIDs the server no longer has.
async fn expunge_local(
    db: &Database,
    mailbox_id: i64,
    uidvalidity: i64,
    uids: &[i64],
    sink: &mut impl ChangeSink,
) -> Result<u64, Error> {
    if uids.is_empty() {
        return Ok(0);
    }
    let uids = uids.to_vec();
    // The ids are collected inside the transaction and reported after it
    // commits: they do not survive the delete, and reporting them before would
    // announce a removal that a rollback then undid.
    let removed = db
        .write(move |conn| {
            let tx = conn.transaction()?;
            let pairs: Vec<(i64, i64)> = {
                let mut stmt = tx.prepare(
                    "SELECT id FROM messages
                     WHERE mailbox_id = ?1 AND uidvalidity = ?2 AND uid = ?3",
                )?;
                let mut pairs = Vec::with_capacity(uids.len());
                for uid in &uids {
                    let id: Option<i64> = stmt
                        .query_row(rusqlite::params![mailbox_id, uidvalidity, uid], |row| {
                            row.get(0)
                        })
                        .optional()?;
                    if let Some(id) = id {
                        pairs.push((id, *uid));
                    }
                }
                pairs
            };
            let ids: Vec<i64> = pairs.iter().map(|(id, _)| *id).collect();
            let deleted = super::remove_messages(&tx, &ids)?;
            tx.commit()?;
            // A row that vanished between the lookup and the delete was not
            // removed by this pass — but the ones that *were* still happened,
            // and reporting none of them would both understate `expunged` and
            // leave downstream indexes holding deleted mail.
            Ok((pairs, deleted))
        })
        .await?;
    let (pairs, deleted) = removed;
    for (message_id, uid) in pairs {
        sink.changed(Change::Removed { message_id, uid });
    }
    Ok(deleted as u64)
}

/// Rewrite the checkpoint's timestamp without moving any mark, so "last
/// checked" advances even when there was nothing to do.
async fn touch_checkpoint(db: &Database, mailbox_id: i64) -> Result<(), Error> {
    let now = chrono::Utc::now().timestamp();
    db.write(move |c| {
        // Re-read inside the write: the value this run loaded is older than
        // every await since, and `upsert_sync_state` replaces every column. A
        // "nothing to do" run must not undo something another one did.
        let Some(current) = repo::get_sync_state(c, mailbox_id)? else {
            return Ok(());
        };
        repo::upsert_sync_state(
            c,
            &repo::SyncState {
                last_sync_at: Some(now),
                ..current
            },
        )
    })
    .await?;
    Ok(())
}

/// The report a run that stopped before applying everything returns.
fn cancelled_report(
    mailbox_id: i64,
    strategy: DeltaStrategy,
    uidvalidity: i64,
    stored_modseq: Option<i64>,
) -> DeltaReport {
    DeltaReport {
        mailbox_id,
        strategy,
        uidvalidity,
        // The checkpoint did not move, so this is still what the next run asks
        // from — not `None`, which would read as "this server has no modseq".
        highestmodseq: stored_modseq,
        new_messages: 0,
        flag_updates: 0,
        expunged: 0,
        purged_stale: 0,
        resynced: false,
        unchanged: false,
        cancelled: true,
    }
}

/// Split a sorted UID list into windows, highest window first, each window
/// itself ascending so [`format_uid_set`] can collapse it into ranges.
fn chunks_newest_first(uids: &[i64], window: i64) -> Vec<Vec<i64>> {
    let size = usize::try_from(window.max(1)).unwrap_or(usize::MAX);
    uids.chunks(size).map(<[i64]>::to_vec).rev().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_run_newest_first_but_stay_ascending_inside() {
        // Newest-first ordering is what makes a folder useful early; ascending
        // *within* a window is what lets the UID set collapse to `21:30`.
        let uids: Vec<i64> = (1..=25).collect();
        let windows = chunks_newest_first(&uids, 10);
        assert_eq!(windows.len(), 3);
        assert_eq!(windows[0], (21..=25).collect::<Vec<i64>>());
        assert_eq!(windows[1], (11..=20).collect::<Vec<i64>>());
        assert_eq!(windows[2], (1..=10).collect::<Vec<i64>>());
        assert_eq!(format_uid_set(&windows[0]), "21:25");
    }

    #[test]
    fn an_empty_uid_list_produces_no_windows() {
        assert!(chunks_newest_first(&[], 10).is_empty());
    }

    #[test]
    fn coverage_admits_new_mail_and_holes_but_not_the_backlog() {
        let coverage = Coverage {
            high_water: 100,
            low_water: Some(50),
        };
        assert!(coverage.covers(101), "new mail above the high mark");
        assert!(coverage.covers(70), "a hole inside the covered range");
        assert!(coverage.covers(50), "the low mark itself is covered");
        assert!(
            !coverage.covers(49),
            "below the low mark belongs to the backlog walk, which is still \
             working downward newest-first"
        );

        let unstarted = Coverage {
            high_water: 0,
            low_water: None,
        };
        assert!(
            unstarted.covers(1),
            "everything is new when nothing is stored"
        );
    }

    #[test]
    // The single-element `Vec` is the point of the test (see the comment on
    // `vanished` below) — clippy's suggested `.collect()` rewrite is exactly
    // the four-billion-element allocation this fixture exists to avoid.
    #[allow(clippy::single_range_in_vec_init)]
    fn vanished_ranges_are_tested_not_expanded() {
        let changes = Changes {
            // What a server answers for a folder it emptied. Expanding this
            // would allocate four billion integers.
            vanished: vec![1..=i64::from(u32::MAX)],
            ..Default::default()
        };
        assert!(changes.is_vanished(7));
        assert!(changes.is_vanished(i64::from(u32::MAX)));
        assert!(!changes.is_vanished(0));
    }

    #[tokio::test]
    async fn a_cancelled_sweep_reports_itself_incomplete() {
        // The sweep is the one probe that can be interrupted partway through
        // and still return usable data. If it did not say so, the run would
        // checkpoint a modseq covering flags it never actually read, and those
        // messages would stay wrong until the next UIDVALIDITY bump.
        use crate::imap::mock::MockImap;
        use crate::sync::harness::{connect, mock_config, Fixture};

        let fx = Fixture::open().await;
        let mock = MockImap::start(mock_config(3)).await;
        fx.full_sync(&mock).await;

        let mut session = connect(&mock).await;
        session.select("INBOX").await.unwrap();
        let before = mock.commands().len();
        let cancel = CancellationToken::new();
        cancel.cancel();

        let changes = sweep_flags(
            &mut session,
            &fx.db,
            fx.mailbox_id,
            crate::sync::harness::UIDVALIDITY,
            SyncOptions::default(),
            &cancel,
        )
        .await
        .unwrap();

        assert!(
            changes.cancelled,
            "the partial answer is flagged as partial"
        );
        assert!(changes.changed.is_empty());
        assert_eq!(
            mock.commands().len(),
            before,
            "and it stopped before issuing a single fetch"
        );
        let _ = session.logout().await;
    }
}
